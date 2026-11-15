use std::time::{Duration, Instant};

use daedalus_db::worker::{Command, Handle, RunId, RunMode, SessionId, Update, start};
use daedalus_db::{Secret, from_url};

const S: SessionId = SessionId(1);
const WAIT: Duration = Duration::from_secs(20);

struct Rig {
    handle: Handle,
    updates: async_channel::Receiver<Update>,
}

fn test_url() -> String {
    std::env::var("DAEDALUS_TEST_MYSQL").expect("DAEDALUS_TEST_MYSQL")
}

fn rig(read_only: bool) -> Rig {
    let (mut conn, password) = from_url(&test_url()).expect("valid test url");
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
        self.run_mode(run, sql, page, timeout, RunMode::Auto);
    }

    fn run_mode(&self, run: u64, sql: &str, page: usize, timeout: Option<Duration>, mode: RunMode) {
        self.handle.send(Command::Run {
            session: S,
            run: RunId(run),
            sql: sql.into(),
            page,
            timeout,
            mode,
        });
    }
}

fn cancel(rig: &Rig, run: u64) {
    rig.handle.send(Command::Cancel {
        session: S,
        run: RunId(run),
    });
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
                assert!(
                    message.contains("cancel") || message.contains("interrupted"),
                    "{message}"
                );
                return;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn connects_and_reports_version() {
    let (conn, password) = from_url(&test_url()).expect("valid test url");
    let (tx, updates) = async_channel::unbounded();
    let handle = start(tx);
    handle.send(Command::Connect {
        session: S,
        conn,
        password,
        root: None,
    });
    let rig = Rig { handle, updates };
    let Update::Connected { server, .. } = rig.next() else {
        panic!("connected")
    };
    assert!(
        server.starts_with("MySQL ") || server.starts_with("MariaDB "),
        "{server}"
    );
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn reads_rows_in_pages() {
    let rig = rig(false);
    rig.run(
        1,
        "select 1 as g, null as n, '' as e \
         from information_schema.columns a, information_schema.columns b limit 1200",
        500,
        None,
    );
    let Update::Columns { columns, .. } = rig.next() else {
        panic!("columns first")
    };
    assert_eq!(columns.len(), 3);
    let Update::Rows { rows, more, .. } = rig.next() else {
        panic!("page 1")
    };
    assert_eq!((rows.len(), more), (500, true));
    assert_eq!(rows[0][1], None);
    assert_eq!(rows[0][2], Some(String::new()));
    for expected in [(500, true), (200, false)] {
        rig.handle.send(Command::FetchMore {
            session: S,
            run: RunId(1),
            rows: 500,
        });
        let Update::Rows { rows, more, .. } = rig.next() else {
            panic!("next page")
        };
        assert_eq!((rows.len(), more), expected);
    }
    let Update::Done { truncated, .. } = rig.next() else {
        panic!("done")
    };
    assert!(!truncated);
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn null_is_not_empty_string() {
    let rig = rig(false);
    rig.run(1, "select null, ''", 100, None);
    loop {
        if let Update::Rows { rows, .. } = rig.next() {
            assert_eq!(rows[0], vec![None, Some(String::new())]);
            break;
        }
    }
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn binary_is_hex() {
    let rig = rig(false);
    rig.run(1, "select unhex('ff00')", 100, None);
    assert_eq!(scalar(&rig, 1), Some("\\xff00".into()));
    rig.run(2, "select 'héllo'", 100, None);
    assert_eq!(scalar(&rig, 2), Some("héllo".into()));
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn bit_is_hex() {
    let rig = rig(false);
    // A bit literal is typed as a string by the server; only a real BIT column carries the type.
    rig.run(
        1,
        "create temporary table bt (b bit(7), c bit(1)); insert into bt values (b'1000001', b'1'); select b from bt",
        100,
        None,
    );
    assert_eq!(scalar(&rig, 1), Some("\\x41".into()));
    rig.run(2, "select c from bt", 100, None);
    assert_eq!(scalar(&rig, 2), Some("\\x01".into()));
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn multi_statement_script() {
    let rig = rig(false);
    rig.run(
        1,
        "create temporary table zz (x int); insert into zz values (1), (2); select count(*) from zz",
        100,
        None,
    );
    assert_eq!(scalar(&rig, 1), Some("2".into()));
    rig.run(2, "select count(*) from zz", 100, None);
    assert_eq!(scalar(&rig, 2), Some("2".into()));
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn cancel_right_after_run_stops_it() {
    let rig = rig(false);
    let started = Instant::now();
    rig.run(1, "select sleep(3)", 500, None);
    cancel(&rig, 1);
    done_or_cancel_error(&rig, 1);
    assert!(started.elapsed() < Duration::from_millis(2500));
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn cancel_queued_run_never_runs() {
    let rig = rig(false);
    let started = Instant::now();
    rig.run(1, "select sleep(1)", 500, None);
    rig.run(2, "select sleep(2)", 500, None);
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
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn cancel_before_start_does_not_execute() {
    let rig = rig(false);
    rig.run(1, "create temporary table cbs (x int)", 100, None);
    scalar(&rig, 1);
    for i in 0..5u64 {
        let ins = i * 2 + 2;
        rig.run(ins, "insert into cbs select sleep(1)", 100, None);
        cancel(&rig, ins);
        loop {
            match rig.next() {
                Update::Done { run, .. } if run == RunId(ins) => break,
                Update::RunError { run, message, .. } if run == RunId(ins) => {
                    assert!(
                        message.contains("cancel") || message.contains("interrupted"),
                        "{message}"
                    );
                    break;
                }
                _ => {}
            }
        }
        let started = Instant::now();
        rig.run(ins + 1, "select count(*) from cbs", 100, None);
        assert_eq!(scalar(&rig, ins + 1), Some("0".into()));
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}

const SERIES: &str = "select 1 as g \
    from information_schema.columns a, information_schema.columns b limit 1200";

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn cancel_does_not_hit_next_run() {
    let rig = rig(false);
    for i in 0..10u64 {
        let first = i * 2 + 1;
        rig.run(first, SERIES, 100, None);
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
        rig.run(first + 1, "select sleep(0.3)", 100, None);
        // A stray KILL makes sleep return 1 without an error.
        assert_eq!(scalar(&rig, first + 1), Some("0".into()));
    }
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn cancel_mid_result_leaves_conn_usable() {
    let rig = rig(false);
    rig.run(1, SERIES, 100, None);
    loop {
        if let Update::Rows { more: true, .. } = rig.next() {
            break;
        }
    }
    cancel(&rig, 1);
    loop {
        match rig.next() {
            Update::Done { run: RunId(1), .. } => break,
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    rig.run(2, "select 1", 100, None);
    assert_eq!(scalar(&rig, 2), Some("1".into()));
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn read_only_refuses_write() {
    let rig = rig(true);
    rig.run(1, "create table daedalus_ro_probe (x int)", 100, None);
    let (message, _) = rig.next_error();
    assert!(message.to_lowercase().contains("read"), "{message}");
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn wrong_password_error_has_no_secret() {
    let (conn, _) = from_url(&test_url()).expect("valid test url");
    let secret = "wrong-secret-4f9c1e";
    let (tx, updates) = async_channel::unbounded();
    let handle = start(tx);
    handle.send(Command::Connect {
        session: S,
        conn,
        password: Some(Secret::new(secret)),
        root: None,
    });
    let rig = Rig { handle, updates };
    let Update::Failed { error, .. } = rig.next() else {
        panic!("connect should fail")
    };
    assert!(!error.contains(secret), "{error}");
    assert!(!error.contains("Opts"), "{error}");
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn catalog_lists_tables_views_and_pk() {
    use daedalus_db::catalog::TableKind;
    let rig = rig(false);
    let db = format!("dcat_{}", std::process::id());
    let setup = [
        format!("create database {db}"),
        format!("create table {db}.t(id int primary key, note varchar(20))"),
        format!("create view {db}.v as select id from {db}.t"),
    ];
    for (i, sql) in setup.iter().enumerate() {
        rig.run(i as u64 + 1, sql, 10, None);
        scalar(&rig, i as u64 + 1);
    }
    rig.handle.send(Command::Introspect { session: S });
    let catalog = loop {
        if let Update::Catalog { catalog, .. } = rig.next() {
            break catalog;
        }
    };
    rig.run(9, &format!("drop database {db}"), 10, None);
    scalar(&rig, 9);
    let catalog = catalog.expect("catalog");
    let found = catalog
        .schemas
        .iter()
        .find(|s| s.name == db)
        .expect("database listed");
    let names: Vec<_> = found.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["t", "v"]);
    let t = &found.tables[0];
    assert_eq!(t.kind, TableKind::Table);
    assert!(t.columns[0].type_name.starts_with("int"));
    assert!(t.columns[0].pk && !t.columns[0].nullable);
    assert_eq!(t.columns[1].type_name, "varchar(20)");
    assert!(!t.columns[1].pk && t.columns[1].nullable);
    assert_eq!(found.tables[1].kind, TableKind::View);
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn read_only_run_blocks_writes() {
    let rig = rig(false);
    rig.run(1, "DROP TABLE IF EXISTS ro_probe", 500, None);
    rig.run(2, "CREATE TABLE ro_probe(x int)", 500, None);
    let mut done = 0;
    while done < 2 {
        match rig.next() {
            Update::Done { .. } => done += 1,
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    rig.run_mode(
        3,
        "INSERT INTO ro_probe VALUES (1)",
        500,
        None,
        RunMode::ReadOnly,
    );
    rig.next_error();
    rig.run(4, "SELECT COUNT(*) FROM ro_probe", 500, None);
    loop {
        match rig.next() {
            Update::Rows { rows, .. } => {
                assert_eq!(rows[0][0].as_deref(), Some("0"));
                break;
            }
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
}

fn table_exists(rig: &Rig, run: u64) -> bool {
    rig.run(
        run,
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'ro_ddl_probe'",
        500,
        None,
    );
    loop {
        match rig.next() {
            Update::Rows { rows, .. } => return rows[0][0].as_deref() != Some("0"),
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn read_only_run_blocks_ddl() {
    let rig = rig(false);
    rig.run(1, "DROP TABLE IF EXISTS ro_ddl_probe", 500, None);
    assert!(matches!(rig.next(), Update::Done { .. }));
    rig.run_mode(
        2,
        "CREATE TABLE ro_ddl_probe(x int)",
        500,
        None,
        RunMode::ReadOnly,
    );
    rig.next_error();
    assert!(!table_exists(&rig, 3));
}

#[test]
#[ignore = "needs MySQL or MariaDB at DAEDALUS_TEST_MYSQL"]
fn auto_run_after_read_only_can_write() {
    let rig = rig(false);
    rig.run(1, "DROP TABLE IF EXISTS ro_after_probe", 500, None);
    assert!(matches!(rig.next(), Update::Done { .. }));
    rig.run(2, "CREATE TABLE ro_after_probe(x int)", 500, None);
    assert!(matches!(rig.next(), Update::Done { .. }));
    rig.run_mode(3, "SELECT 1", 500, None, RunMode::ReadOnly);
    loop {
        if let Update::Done { .. } = rig.next() {
            break;
        }
    }
    rig.run(4, "INSERT INTO ro_after_probe VALUES (1)", 500, None);
    loop {
        match rig.next() {
            Update::Done { .. } => break,
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
}
