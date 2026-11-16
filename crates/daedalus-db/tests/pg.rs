use std::time::{Duration, Instant};

use daedalus_db::edit::Stmt;
use daedalus_db::worker::{
    ApplyError, Command, Handle, RunId, RunMode, SessionId, TxOp, Update, start,
};
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
            capped: true,
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

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn catalog_lists_tables_views_and_pk() {
    use daedalus_db::catalog::TableKind;
    let rig = rig(false);
    let schema = format!("dcat_{}", std::process::id());
    let setup = [
        format!("create schema {schema}"),
        format!("create table {schema}.t(id int primary key, note text)"),
        format!("create view {schema}.v as select id from {schema}.t"),
        format!("create table {schema}.empty()"),
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
    rig.run(9, &format!("drop schema {schema} cascade"), 10, None);
    scalar(&rig, 9);
    let catalog = catalog.expect("catalog");
    let found = catalog
        .schemas
        .iter()
        .find(|s| s.name == schema)
        .expect("schema listed");
    let names: Vec<_> = found.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["empty", "t", "v"]);
    assert!(found.tables[0].columns.is_empty());
    let t = &found.tables[1];
    assert_eq!(t.kind, TableKind::Table);
    assert_eq!(t.columns[0].type_name, "integer");
    assert!(t.columns[0].pk && !t.columns[0].nullable);
    assert!(!t.columns[1].pk && t.columns[1].nullable);
    assert_eq!(found.tables[2].kind, TableKind::View);
    assert!(
        !catalog
            .schemas
            .iter()
            .any(|s| s.name == "pg_catalog" || s.name == "information_schema")
    );
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn read_only_run_blocks_writes_after_session_set_read_write() {
    let rig = rig(false);
    rig.run(
        1,
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        500,
        None,
    );
    loop {
        if let Update::Done { .. } = rig.next() {
            break;
        }
    }
    rig.run(3, "DROP TABLE IF EXISTS ro_probe", 500, None);
    loop {
        if let Update::Done { .. } = rig.next() {
            break;
        }
    }
    rig.run_mode(
        2,
        "CREATE TABLE ro_probe(x int)",
        500,
        None,
        RunMode::ReadOnly,
    );
    let (message, _) = rig.next_error();
    let message = message.to_lowercase();
    assert!(
        message.contains("read-only") || message.contains("read only"),
        "{message}"
    );
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn commit_of_a_failed_transaction_rolls_back() {
    let rig = rig(false);
    rig.run_mode(1, "SELECT 1/0", 500, None, RunMode::Transaction);
    loop {
        match rig.next() {
            Update::RunError { .. } => break,
            Update::TxState { .. } | Update::Columns { .. } => {}
            other => panic!("expected a run error, got {other:?}"),
        }
    }
    rig.handle.send(Command::Transaction {
        session: S,
        op: TxOp::Commit,
    });
    match rig.next() {
        Update::TxState { open, error, .. } => {
            assert!(!open);
            let message = error.expect("a rollback message");
            assert!(message.contains("rolled back"), "{message}");
        }
        other => panic!("expected a transaction state, got {other:?}"),
    }
}

fn stmt(sql: &str, params: &[Option<&str>]) -> Stmt {
    Stmt {
        sql: sql.into(),
        params: params.iter().map(|p| p.map(str::to_owned)).collect(),
    }
}

fn apply(rig: &Rig, statements: Vec<Stmt>) -> Result<u64, ApplyError> {
    rig.handle.send(Command::Apply {
        session: S,
        statements,
    });
    loop {
        if let Update::Applied { result, .. } = rig.next() {
            return result;
        }
    }
}

fn exec(rig: &Rig, run: u64, sql: &str) {
    rig.run(run, sql, 100, None);
    scalar(rig, run);
}

fn row(rig: &Rig, run: u64, sql: &str) -> Vec<Option<String>> {
    rig.run(run, sql, 100, None);
    loop {
        match rig.next() {
            Update::Rows { mut rows, .. } if !rows.is_empty() => {
                let first = rows.remove(0);
                scalar(rig, run);
                return first;
            }
            Update::Done { run: r, .. } if r == RunId(run) => panic!("no row"),
            Update::RunError { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn pg_text_params_fill_typed_columns() {
    let rig = rig(false);
    exec(
        &rig,
        1,
        "create temp table t(i int, n numeric, b bool, ts timestamptz, j jsonb, a int[], u uuid)",
    );
    let result = apply(
        &rig,
        vec![stmt(
            "insert into t values ($1, $2, $3, $4, $5, $6, $7)",
            &[
                Some("5"),
                Some("1.50"),
                Some("t"),
                Some("2024-01-02 03:04:05+00"),
                Some("{\"a\":1}"),
                Some("{1,2}"),
                Some("123e4567-e89b-12d3-a456-426614174000"),
            ],
        )],
    );
    assert_eq!(result, Ok(1));
    let got = row(
        &rig,
        2,
        "select i::text, n::text, b::text, (ts = '2024-01-02 03:04:05+00'::timestamptz)::text, j::text, a::text, u::text from t",
    );
    let want = [
        "5",
        "1.50",
        "true",
        "true",
        "{\"a\": 1}",
        "{1,2}",
        "123e4567-e89b-12d3-a456-426614174000",
    ];
    assert_eq!(got, want.map(|w| Some(w.to_owned())));
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn pg_varchar_overflow_fails_and_rolls_back() {
    let rig = rig(false);
    exec(&rig, 1, "create temp table t(id int, v varchar(3))");
    let err = apply(
        &rig,
        vec![
            stmt("insert into t values ($1, $2)", &[Some("1"), Some("abc")]),
            stmt("insert into t values ($1, $2)", &[Some("2"), Some("abcd")]),
        ],
    )
    .expect_err("overflow");
    assert_eq!(err.index, Some(1));
    assert_eq!(
        row(&rig, 2, "select count(*)::text from t"),
        vec![Some("0".into())]
    );
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn pg_rowcount_mismatch_rolls_back() {
    let rig = rig(false);
    exec(&rig, 1, "create temp table t(id int primary key, x text)");
    exec(&rig, 2, "insert into t values (1, 'a')");
    let err = apply(
        &rig,
        vec![
            stmt("update t set x = $1 where id = $2", &[Some("A"), Some("1")]),
            stmt("update t set x = $1 where id = $2", &[Some("B"), Some("9")]),
        ],
    )
    .expect_err("mismatch");
    assert_eq!(err.index, Some(1));
    assert_eq!(row(&rig, 3, "select x from t"), vec![Some("a".into())]);
}

#[test]
#[ignore = "needs Postgres at DAEDALUS_TEST_PG"]
fn pg_null_and_empty_string_differ() {
    let rig = rig(false);
    exec(&rig, 1, "create temp table t(id int, x text)");
    let result = apply(
        &rig,
        vec![
            stmt("insert into t values ($1, $2)", &[Some("1"), None]),
            stmt("insert into t values ($1, $2)", &[Some("2"), Some("")]),
        ],
    );
    assert_eq!(result, Ok(2));
    assert_eq!(
        row(&rig, 2, "select (x is null)::text from t where id = 1"),
        vec![Some("true".into())]
    );
    assert_eq!(
        row(&rig, 3, "select (x = '')::text from t where id = 2"),
        vec![Some("true".into())]
    );
}
