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
