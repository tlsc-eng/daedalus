use std::time::{Duration, Instant};

use daedalus_db::worker::{Command, Handle, RunId, SessionId, Update, start};
use daedalus_db::{Secret, from_url};

const S: SessionId = SessionId(1);
const WAIT: Duration = Duration::from_secs(20);

struct Rig {
    handle: Handle,
    updates: async_channel::Receiver<Update>,
}

fn rig(read_only: bool) -> Rig {
    let url = std::env::var("DAEDALUS_TEST_PG").expect("DAEDALUS_TEST_PG");
    let (mut conn, password) = from_url(&url).expect("valid test url");
    conn.read_only = read_only;
    let (tx, updates) = async_channel::unbounded();
    let handle = start(tx);
    handle.send(Command::Connect {
        session: S,
        conn,
        password: password.or_else(|| Some(Secret::new(""))),
        root: None,
    });
    let rig = Rig { handle, updates };
    assert!(matches!(rig.next(), Update::Connected { .. }));
    rig
}

impl Rig {
    fn next(&self) -> Update {
        let end = Instant::now() + WAIT;
        loop {
            match self.updates.try_recv() {
                Ok(u) => return u,
                Err(_) if Instant::now() > end => panic!("no update in time"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn next_error(&self) -> (String, Option<usize>) {
        loop {
            match self.next() {
                Update::Columns { .. } => {}
                Update::RunError {
                    message, position, ..
                } => return (message, position),
                other => panic!("expected a run error, got {other:?}"),
            }
        }
    }

    fn run(&self, run: u64, sql: &str, page: usize, timeout: Option<Duration>) {
        self.handle.send(Command::Run {
            session: S,
            run: RunId(run),
            sql: sql.into(),
            page,
            timeout,
        });
    }
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn streams_in_pages() {
    let rig = rig(false);
    rig.run(
        1,
        "select g, null as n, '' as e from generate_series(1, 1200) g",
        500,
        None,
    );
    let Update::Columns { columns, .. } = rig.next() else {
        panic!("columns first")
    };
    assert_eq!(columns[0].type_name, "int4");
    let Update::Rows { rows, more, .. } = rig.next() else {
        panic!("page 1")
    };
    assert_eq!((rows.len(), more), (500, true));
    assert_eq!(rows[0][1], None);
    assert_eq!(rows[0][2], Some(String::new()));
    rig.handle.send(Command::FetchMore {
        session: S,
        run: RunId(1),
        rows: 500,
    });
    let Update::Rows { rows, more, .. } = rig.next() else {
        panic!("page 2")
    };
    assert_eq!((rows.len(), more), (500, true));
    rig.handle.send(Command::FetchMore {
        session: S,
        run: RunId(1),
        rows: 500,
    });
    let Update::Rows { rows, more, .. } = rig.next() else {
        panic!("page 3")
    };
    assert_eq!((rows.len(), more), (200, false));
    let Update::Done { truncated, .. } = rig.next() else {
        panic!("done")
    };
    assert!(!truncated);
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn cancel_stops_pg_sleep() {
    let rig = rig(false);
    let started = Instant::now();
    rig.run(1, "select pg_sleep(30)", 500, None);
    std::thread::sleep(Duration::from_millis(300));
    rig.handle.send(Command::Cancel {
        session: S,
        run: RunId(1),
    });
    let (message, _) = rig.next_error();
    assert!(message.contains("cancel"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn read_only_refuses_insert() {
    let rig = rig(true);
    rig.run(1, "create table daedalus_ro_probe (x int)", 500, None);
    let (message, _) = rig.next_error();
    assert!(message.contains("read-only"), "{message}");
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn timeout_cancels() {
    let rig = rig(false);
    rig.run(
        1,
        "select pg_sleep(30)",
        500,
        Some(Duration::from_millis(500)),
    );
    let (message, _) = rig.next_error();
    assert_eq!(message, "timed out after 0.5s");
}

fn cancel(rig: &Rig, run: u64) {
    rig.handle.send(Command::Cancel {
        session: S,
        run: RunId(run),
    });
}

fn done_or_cancel_error(rig: &Rig, run: u64) {
    loop {
        match rig.next() {
            Update::Columns { .. } | Update::Rows { .. } => {}
            Update::Done {
                run: r, truncated, ..
            } if r == RunId(run) => {
                assert!(truncated);
                return;
            }
            Update::RunError {
                run: r, message, ..
            } if r == RunId(run) => {
                assert!(message.contains("cancel"), "{message}");
                return;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn cancel_right_after_run_stops_it() {
    let rig = rig(false);
    let started = Instant::now();
    rig.run(1, "select pg_sleep(3)", 500, None);
    cancel(&rig, 1);
    done_or_cancel_error(&rig, 1);
    assert!(started.elapsed() < Duration::from_millis(2500));
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn cancel_right_after_run_stops_a_write() {
    let rig = rig(false);
    rig.run(1, "create temp table cancel_t as select 1 as a", 500, None);
    loop {
        match rig.next() {
            Update::Done { run: RunId(1), .. } => break,
            Update::Columns { .. } | Update::Rows { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    for i in 0..5 {
        let run = 2 + i;
        let started = Instant::now();
        rig.run(
            run,
            "update cancel_t set a = length(pg_sleep(3)::text)",
            500,
            None,
        );
        cancel(&rig, run);
        done_or_cancel_error(&rig, run);
        assert!(started.elapsed() < Duration::from_millis(2500));
    }
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn cancel_queued_run_never_runs() {
    let rig = rig(false);
    let started = Instant::now();
    rig.run(1, "select pg_sleep(1)", 500, None);
    rig.run(2, "select pg_sleep(2)", 500, None);
    cancel(&rig, 2);
    let (mut a_done, mut b_cancelled) = (false, false);
    while !(a_done && b_cancelled) {
        match rig.next() {
            Update::Columns { .. } | Update::Rows { .. } => {}
            Update::Done {
                run: RunId(1),
                truncated,
                ..
            } => {
                assert!(!truncated);
                a_done = true;
            }
            Update::Done {
                run: RunId(2),
                truncated,
                ..
            } => {
                assert!(truncated);
                b_cancelled = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(started.elapsed() < Duration::from_millis(1900));
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn cancel_while_paused_does_not_hit_next_run() {
    let rig = rig(false);
    for i in 0..10u64 {
        let first = i * 2 + 1;
        rig.run(first, "select g from generate_series(1, 600) g", 100, None);
        loop {
            if let Update::Rows { more: true, .. } = rig.next() {
                break;
            }
        }
        cancel(&rig, first);
        loop {
            match rig.next() {
                Update::Done { run, .. } if run == RunId(first) => break,
                Update::RunError { run, message, .. } if run == RunId(first) => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
        rig.run(first + 1, "select pg_sleep(0.3)", 100, None);
        loop {
            match rig.next() {
                Update::Done { run, .. } if run == RunId(first + 1) => break,
                Update::RunError { message, .. } => panic!("next run hit: {message}"),
                _ => {}
            }
        }
    }
}

fn scalar(rig: &Rig, run: u64) -> Option<String> {
    let mut value = None;
    loop {
        match rig.next() {
            Update::Rows { rows, .. } => {
                if let Some(row) = rows.first() {
                    value = row[0].clone();
                }
            }
            Update::Done { run: r, .. } if r == RunId(run) => return value,
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn multi_statement_script_keeps_transaction_usable() {
    let rig = rig(false);
    rig.run(1, "begin", 100, None);
    scalar(&rig, 1);
    rig.run(
        2,
        "create temp table zz(x int); insert into zz values (1)",
        100,
        None,
    );
    scalar(&rig, 2);
    rig.run(3, "select count(*) from zz", 100, None);
    assert_eq!(scalar(&rig, 3), Some("1".into()));
    rig.run(4, "rollback", 100, None);
    scalar(&rig, 4);
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn bad_statement_in_transaction_reports_real_error() {
    let rig = rig(false);
    rig.run(1, "begin", 100, None);
    scalar(&rig, 1);
    rig.run(2, "select * from nope_table", 100, None);
    let (message, _) = rig.next_error();
    assert!(message.contains("nope_table"), "{message}");
    assert!(!message.contains("aborted"), "{message}");
}
