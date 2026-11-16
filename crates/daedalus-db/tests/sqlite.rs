use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use daedalus_db::edit::Stmt;
use daedalus_db::worker::{
    ApplyError, Command, Handle, RunId, RunMode, SessionId, TxOp, Update, start,
};
use daedalus_db::{Connection, Engine, Tls};

const S: SessionId = SessionId(1);
const WAIT: Duration = Duration::from_secs(20);

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "daedalus-sqlite-{}-{n}-{tag}.db",
        std::process::id()
    ))
}

fn make_db(tag: &str) -> PathBuf {
    let path = temp_path(tag);
    rusqlite::Connection::open(&path).expect("create db");
    path
}

struct Rig {
    handle: Handle,
    updates: async_channel::Receiver<Update>,
    files: Vec<PathBuf>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

fn conn(database: &str, read_only: bool) -> Connection {
    Connection {
        id: "t".into(),
        name: "t".into(),
        engine: Engine::Sqlite,
        host: String::new(),
        port: None,
        database: database.into(),
        user: String::new(),
        tls: Tls::Prefer,
        ca_file: None,
        ssh: None,
        read_only,
        production: false,
        color: None,
        claude: false,
        projects: Vec::new(),
        save_password: false,
    }
}

fn connect(c: Connection, root: Option<PathBuf>) -> Rig {
    let (tx, updates) = async_channel::unbounded();
    let handle = start(tx);
    handle.send(Command::Connect {
        session: S,
        conn: c,
        password: None,
        root,
    });
    Rig {
        handle,
        updates,
        files: Vec::new(),
    }
}

fn rig(read_only: bool) -> Rig {
    let path = make_db("main");
    let rig = connect(conn(path.to_str().expect("utf8"), read_only), None);
    let mut rig = rig;
    rig.files.push(path);
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

    fn run(&self, run: u64, sql: &str, page: usize) {
        self.run_mode(run, sql, page, RunMode::Auto);
    }

    fn run_mode(&self, run: u64, sql: &str, page: usize, mode: RunMode) {
        self.run_capped(run, sql, page, mode, true);
    }

    fn run_capped(&self, run: u64, sql: &str, page: usize, mode: RunMode, capped: bool) {
        self.handle.send(Command::Run {
            session: S,
            run: RunId(run),
            sql: sql.into(),
            page,
            timeout: None,
            mode,
            capped,
        });
    }

    fn tx(&self, op: TxOp) {
        self.handle.send(Command::Transaction { session: S, op });
    }

    fn cancel(&self, run: u64) {
        self.handle.send(Command::Cancel {
            session: S,
            run: RunId(run),
        });
    }

    /// Like `collect`, but pages in `fetch` rows and also reports whether the run was truncated.
    fn collect_done(&self, run: u64, fetch: usize) -> (usize, bool) {
        let mut count = 0;
        loop {
            match self.next() {
                Update::Rows { rows, more, .. } => {
                    count += rows.len();
                    if more {
                        self.handle.send(Command::FetchMore {
                            session: S,
                            run: RunId(run),
                            rows: fetch,
                        });
                    }
                }
                Update::Done {
                    run: r, truncated, ..
                } if r == RunId(run) => return (count, truncated),
                Update::RunError { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    }

    /// Collects every row until the run ends; a run error is returned as `Err`.
    fn collect(&self, run: u64) -> Result<Vec<Vec<Option<String>>>, String> {
        let mut all = Vec::new();
        loop {
            match self.next() {
                Update::Rows { rows, more, .. } => {
                    all.extend(rows);
                    if more {
                        self.handle.send(Command::FetchMore {
                            session: S,
                            run: RunId(run),
                            rows: 500,
                        });
                    }
                }
                Update::Done { run: r, .. } if r == RunId(run) => return Ok(all),
                Update::RunError { message, .. } => return Err(message),
                _ => {}
            }
        }
    }
}

#[test]
fn reads_rows_in_pages() {
    let rig = rig(false);
    rig.run(
        1,
        "with recursive c(x) as (select 1 union all select x+1 from c where x < 1200) select x from c",
        500,
    );
    let Update::Columns { .. } = rig.next() else {
        panic!("columns first")
    };
    let Update::Rows { rows, more, .. } = rig.next() else {
        panic!("page 1")
    };
    assert_eq!((rows.len(), more), (500, true));
    for (n, expected, more_after) in [(2, 500, true), (3, 200, false)] {
        rig.handle.send(Command::FetchMore {
            session: S,
            run: RunId(1),
            rows: 500,
        });
        let Update::Rows { rows, more, .. } = rig.next() else {
            panic!("page {n}")
        };
        assert_eq!((rows.len(), more), (expected, more_after));
    }
    let Update::Done { truncated, .. } = rig.next() else {
        panic!("done")
    };
    assert!(!truncated);
}

#[test]
fn interrupt_stops_long_recursive_cte() {
    let rig = rig(false);
    let started = Instant::now();
    rig.run(
        1,
        "select count(*) from (with recursive c(x) as (select 1 union all select x+1 from c where x < 1000000000) select x from c)",
        500,
    );
    std::thread::sleep(Duration::from_millis(200));
    rig.cancel(1);
    loop {
        match rig.next() {
            Update::Done { .. } | Update::RunError { .. } => break,
            _ => {}
        }
    }
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn cancel_does_not_hit_next_run() {
    let rig = rig(false);
    for i in 0..10u64 {
        let run = i * 2 + 1;
        rig.run(run, "select 1", 500);
        rig.cancel(run);
        loop {
            match rig.next() {
                Update::Done { run: r, .. } | Update::RunError { run: r, .. }
                    if r == RunId(run) =>
                {
                    break;
                }
                _ => {}
            }
        }
        rig.run(run + 1, "select 1", 500);
        assert_eq!(rig.collect(run + 1), Ok(vec![vec![Some("1".into())]]));
    }
}

#[test]
fn read_only_refuses_write() {
    let other = make_db("other");
    let main = make_db("ro");
    let target = temp_path("vacuum");
    rusqlite::Connection::open(&main)
        .and_then(|c| c.execute_batch("create table t(x)"))
        .expect("seed");
    let mut rig = connect(conn(main.to_str().expect("utf8"), true), None);
    rig.files.extend([main, other.clone(), target.clone()]);
    assert!(matches!(rig.next(), Update::Connected { .. }));
    rig.run(1, "insert into t values (1)", 100);
    assert!(rig.collect(1).is_err());
    rig.run(
        2,
        &format!("attach '{}' as o; create table o.t(x)", other.display()),
        100,
    );
    assert!(rig.collect(2).is_err());
    for (run, sql) in [
        (3, format!("vacuum into '{}'", target.display())),
        (
            4,
            format!(
                "pragma query_only = off; vacuum into '{}'",
                target.display()
            ),
        ),
    ] {
        rig.run(run, &sql, 100);
        assert!(rig.collect(run).is_err(), "{sql}");
        assert!(!target.exists(), "{sql}");
    }
}

#[test]
fn blob_is_hex() {
    let rig = rig(false);
    rig.run(1, "select x'00ff1a'", 100);
    assert_eq!(rig.collect(1), Ok(vec![vec![Some("\\x00ff1a".into())]]));
}

#[test]
fn null_is_not_empty_string() {
    let rig = rig(false);
    rig.run(1, "select null, ''", 100);
    assert_eq!(rig.collect(1), Ok(vec![vec![None, Some(String::new())]]));
}

#[test]
fn multi_statement_script() {
    let rig = rig(false);
    rig.run(
        1,
        "create table t(x); insert into t values (1),(2); select * from t",
        100,
    );
    assert_eq!(
        rig.collect(1),
        Ok(vec![vec![Some("1".into())], vec![Some("2".into())]])
    );
}

#[test]
fn relative_path_resolves_against_root() {
    let dir = std::env::temp_dir().join(format!("daedalus-sqlite-root-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    rusqlite::Connection::open(dir.join("rel.db")).expect("create");
    let rig = connect(conn("rel.db", false), Some(dir.clone()));
    let connected = matches!(rig.next(), Update::Connected { .. });
    let none = connect(conn("rel.db", false), None);
    let message = match none.next() {
        Update::Failed { error, .. } => error,
        other => panic!("expected failure, got {other:?}"),
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert!(connected);
    assert_eq!(message, "a relative SQLite path needs a project");
}

#[test]
fn missing_file_is_an_error_and_is_not_created() {
    let path = temp_path("missing");
    let rig = connect(conn(path.to_str().expect("utf8"), false), None);
    assert!(matches!(rig.next(), Update::Failed { .. }));
    assert!(!path.exists());
}

const BIG_SCRIPT_ROWS: usize = 20_000;

fn insert_script() -> String {
    "insert into t values (1);".repeat(BIG_SCRIPT_ROWS)
}

fn row_count(rig: &Rig, run: u64) -> usize {
    rig.run(run, "select count(*) from t", 100);
    let rows = rig.collect(run).expect("count");
    rows[0][0]
        .as_ref()
        .expect("count cell")
        .parse()
        .expect("int")
}

#[test]
fn cancel_stops_multi_statement_script() {
    for _ in 0..5 {
        let rig = rig(false);
        rig.run(1, "create table t(x)", 100);
        assert!(rig.collect(1).is_ok());
        rig.run(2, &insert_script(), 100);
        std::thread::sleep(Duration::from_millis(50));
        rig.cancel(2);
        loop {
            match rig.next() {
                Update::Done { truncated, .. } => {
                    assert!(truncated);
                    break;
                }
                Update::RunError { message, .. } => {
                    assert!(message.contains("interrupt"), "{message}");
                    break;
                }
                _ => {}
            }
        }
        assert!(row_count(&rig, 3) < BIG_SCRIPT_ROWS);
    }
}

#[test]
fn timeout_stops_multi_statement_script() {
    let rig = rig(false);
    rig.run(1, "create table t(x)", 100);
    assert!(rig.collect(1).is_ok());
    rig.handle.send(Command::Run {
        session: S,
        run: RunId(2),
        sql: insert_script(),
        page: 100,
        timeout: Some(Duration::from_millis(50)),
        mode: RunMode::Auto,
        capped: true,
    });
    match rig.collect(2) {
        Err(message) => assert!(message.contains("timed out"), "{message}"),
        Ok(_) => panic!("script finished before the timeout"),
    }
    assert!(row_count(&rig, 3) < BIG_SCRIPT_ROWS);
}

#[test]
fn ddl_reports_zero_affected() {
    let rig = rig(false);
    for (run, sql, expected) in [
        (1, "create table a(x)", 0),
        (2, "insert into a values (1),(2)", 2),
        (3, "create table b(x)", 0),
        (4, "insert into a values (3); create table c(x)", 0),
    ] {
        rig.run(run, sql, 100);
        loop {
            if let Update::Done { affected, .. } = rig.next() {
                assert_eq!(affected, Some(expected), "{sql}");
                break;
            }
        }
    }
}

#[test]
fn syntax_error_position_is_absolute() {
    let rig = rig(false);
    rig.run(1, "select 1; selec 2", 100);
    loop {
        match rig.next() {
            Update::RunError {
                message, position, ..
            } => {
                assert_eq!(position, Some(11));
                assert!(!message.contains("selec 2"), "{message}");
                break;
            }
            Update::Done { .. } => panic!("expected an error"),
            _ => {}
        }
    }
}

#[test]
fn cancel_before_start_does_not_execute() {
    let rig = rig(false);
    rig.run(1, "create table t(x)", 100);
    assert!(rig.collect(1).is_ok());
    rig.run(
        2,
        "select count(*) from (with recursive c(x) as (select 1 union all select x+1 from c where x < 1000000000) select x from c)",
        100,
    );
    std::thread::sleep(Duration::from_millis(200));
    let targets: Vec<u64> = (0..5).map(|i| 10 + i).collect();
    for run in &targets {
        rig.run(*run, "insert into t values (1)", 100);
        rig.cancel(*run);
    }
    rig.cancel(2);
    let mut pending: Vec<u64> = targets.iter().copied().chain([2]).collect();
    while !pending.is_empty() {
        match rig.next() {
            Update::Done { run, .. } | Update::RunError { run, .. } => {
                pending.retain(|r| RunId(*r) != run);
            }
            _ => {}
        }
    }
    assert_eq!(row_count(&rig, 30), 0);
}

#[test]
fn catalog_lists_tables_columns_and_pk() {
    use daedalus_db::catalog::TableKind;
    let rig = rig(false);
    let setup = [
        "create table pairs(a integer not null, b text not null, note text, primary key (a, b))",
        "create view pair_notes as select a, note from pairs",
        "create table \"my table\"(id integer primary key)",
    ];
    for (i, sql) in setup.iter().enumerate() {
        rig.run(i as u64 + 1, sql, 10);
        rig.collect(i as u64 + 1).expect("setup");
    }
    rig.handle.send(Command::Introspect { session: S });
    let catalog = loop {
        if let Update::Catalog { catalog, .. } = rig.next() {
            break catalog.expect("catalog");
        }
    };
    assert!(!catalog.truncated);
    assert_eq!(catalog.schemas.len(), 1);
    let schema = &catalog.schemas[0];
    assert_eq!(schema.name, "main");
    let names: Vec<_> = schema.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["my table", "pair_notes", "pairs"]);
    assert_eq!(schema.tables[0].kind, TableKind::Table);
    assert!(schema.tables[0].columns[0].pk);
    assert_eq!(schema.tables[1].kind, TableKind::View);
    let cols = &schema.tables[2].columns;
    let got: Vec<_> = cols
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str(), c.nullable, c.pk))
        .collect();
    assert_eq!(
        got,
        [
            ("a", "INTEGER", false, true),
            ("b", "TEXT", false, true),
            ("note", "TEXT", true, false)
        ]
    );
}

impl Rig {
    fn catalog(&self) -> daedalus_db::catalog::Catalog {
        self.handle.send(Command::Introspect { session: S });
        loop {
            if let Update::Catalog { catalog, .. } = self.next() {
                return catalog.expect("catalog");
            }
        }
    }

    fn exec(&self, run: u64, sql: &str) {
        self.run(run, sql, 10);
        self.collect(run).expect("exec");
    }
}

#[test]
fn catalog_survives_a_broken_view() {
    let rig = rig(false);
    rig.exec(1, "create table gone(x integer)");
    rig.exec(2, "create view orphan as select x from gone");
    rig.exec(3, "create table keep(id integer primary key)");
    rig.exec(4, "drop table gone");
    let catalog = rig.catalog();
    let tables = &catalog.schemas[0].tables;
    let names: Vec<_> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["keep", "orphan"]);
    assert!(tables[0].columns[0].pk);
    assert!(tables[1].columns.is_empty());
}

#[test]
fn sqlite_prefixed_user_tables_are_listed() {
    let rig = rig(false);
    rig.exec(1, "create table sqlite3_data(x)");
    rig.exec(2, "create table SQLiteStats(x)");
    let catalog = rig.catalog();
    let names: Vec<_> = catalog.schemas[0]
        .tables
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["SQLiteStats", "sqlite3_data"]);
}

#[test]
fn introspect_queued_during_connect_is_answered() {
    let path = make_db("early");
    let mut rig = connect(conn(path.to_str().expect("utf8"), false), None);
    rig.files.push(path);
    rig.handle.send(Command::Introspect { session: S });
    assert!(matches!(rig.next(), Update::Connected { .. }));
    assert!(matches!(rig.next(), Update::Catalog { catalog: Ok(_), .. }));
}

#[test]
fn run_sent_during_introspect_runs_after_it_in_order() {
    let rig = rig(false);
    rig.handle.send(Command::Introspect { session: S });
    rig.run(1, "select 1", 10);
    assert!(matches!(rig.next(), Update::Catalog { .. }));
    assert_eq!(rig.collect(1).expect("run"), [[Some("1".to_owned())]]);
}

fn next_tx(rig: &Rig) -> (bool, Option<String>) {
    loop {
        if let Update::TxState { open, error, .. } = rig.next() {
            return (open, error);
        }
    }
}

fn finish_tx_run(rig: &Rig, sql: &str, run: u64) {
    rig.run_mode(run, sql, 100, RunMode::Transaction);
    assert_eq!(next_tx(rig), (true, None));
    assert!(rig.collect(run).is_ok());
}

#[test]
fn manual_transaction_rolls_back() {
    let rig = rig(false);
    rig.run(1, "create table t(x)", 100);
    assert!(rig.collect(1).is_ok());
    finish_tx_run(&rig, "insert into t values (1)", 2);
    rig.tx(TxOp::Rollback);
    assert_eq!(next_tx(&rig), (false, None));
    assert_eq!(row_count(&rig, 3), 0);
}

#[test]
fn manual_transaction_commits() {
    let rig = rig(false);
    rig.run(1, "create table t(x)", 100);
    assert!(rig.collect(1).is_ok());
    finish_tx_run(&rig, "insert into t values (1)", 2);
    rig.tx(TxOp::Commit);
    assert_eq!(next_tx(&rig), (false, None));
    assert_eq!(row_count(&rig, 3), 1);
}

#[test]
fn commit_without_a_transaction_is_a_no_op() {
    let rig = rig(false);
    rig.tx(TxOp::Commit);
    assert_eq!(next_tx(&rig), (false, None));
}

#[test]
fn begin_on_a_read_only_connection_is_refused() {
    let rig = rig(true);
    rig.tx(TxOp::Begin);
    let (open, error) = next_tx(&rig);
    assert!(!open);
    assert!(error.is_some());
}

#[test]
fn read_only_mode_refuses_a_write_on_a_writable_connection() {
    let rig = rig(false);
    rig.run(1, "create table t(x)", 100);
    assert!(rig.collect(1).is_ok());
    rig.run_mode(2, "insert into t values (1)", 100, RunMode::ReadOnly);
    assert!(rig.collect(2).is_err());
    rig.run(3, "insert into t values (1)", 100);
    assert!(rig.collect(3).is_ok());
    assert_eq!(row_count(&rig, 4), 1);
}

#[test]
fn read_only_mode_is_refused_while_a_transaction_is_open() {
    let rig = rig(false);
    rig.tx(TxOp::Begin);
    assert_eq!(next_tx(&rig), (true, None));
    rig.run_mode(1, "select 1", 100, RunMode::ReadOnly);
    assert!(rig.collect(1).is_err());
}

#[test]
fn commit_then_disconnect_commits() {
    for _ in 0..5 {
        let path = make_db("disc");
        {
            let rig = connect(conn(path.to_str().expect("utf8"), false), None);
            assert!(matches!(rig.next(), Update::Connected { .. }));
            rig.run(1, "create table t(x)", 100);
            assert!(rig.collect(1).is_ok());
            finish_tx_run(&rig, "insert into t values (1)", 2);
            rig.tx(TxOp::Commit);
            rig.handle.send(Command::Disconnect { session: S });
            std::thread::sleep(Duration::from_millis(300));
        }
        let rig = connect(conn(path.to_str().expect("utf8"), false), None);
        assert!(matches!(rig.next(), Update::Connected { .. }));
        assert_eq!(row_count(&rig, 3), 1);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn engine_ended_transaction_is_reported() {
    let rig = rig(false);
    rig.run(1, "create table t(x unique)", 100);
    assert!(rig.collect(1).is_ok());
    finish_tx_run(&rig, "insert into t values (1)", 2);
    rig.run_mode(
        3,
        "insert or rollback into t values (1)",
        100,
        RunMode::Transaction,
    );
    assert!(rig.collect(3).is_err());
    let (open, error) = next_tx(&rig);
    assert!(!open);
    assert!(error.is_some());
    finish_tx_run(&rig, "insert into t values (2)", 4);
    rig.tx(TxOp::Rollback);
    assert_eq!(next_tx(&rig), (false, None));
    assert_eq!(row_count(&rig, 5), 0);
}

#[test]
fn cancel_queued_during_connect_never_executes() {
    for _ in 0..20 {
        let path = make_db("idle");
        rusqlite::Connection::open(&path)
            .expect("open")
            .execute("create table t(x)", [])
            .expect("create");
        let (tx, updates) = async_channel::unbounded();
        let handle = start(tx);
        handle.send(Command::Connect {
            session: S,
            conn: conn(path.to_str().expect("utf8"), false),
            password: None,
            root: None,
        });
        handle.send(Command::Run {
            session: S,
            run: RunId(1),
            sql: "insert into t values (1)".into(),
            page: 100,
            timeout: None,
            mode: RunMode::Auto,
            capped: true,
        });
        handle.send(Command::Cancel {
            session: S,
            run: RunId(1),
        });
        let rig = Rig {
            handle,
            updates,
            files: vec![path.clone()],
        };
        assert!(matches!(rig.next(), Update::Connected { .. }));
        assert!(matches!(
            rig.next(),
            Update::Done {
                truncated: true,
                ..
            }
        ));
        assert_eq!(row_count(&rig, 2), 0);
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

fn scalar(rig: &Rig, run: u64, sql: &str) -> Vec<Option<String>> {
    rig.run(run, sql, 100);
    rig.collect(run).expect("query").remove(0)
}

fn table_with_rows(rig: &Rig, sql: &str) {
    rig.run(1, sql, 100);
    assert!(rig.collect(1).is_ok());
}

#[test]
fn apply_commits_every_statement() {
    let rig = rig(false);
    table_with_rows(
        &rig,
        "create table t(id integer primary key, x text); insert into t values (1,'a'),(2,'b')",
    );
    let result = apply(
        &rig,
        vec![
            stmt("update t set x = ? where id = ?", &[Some("A"), Some("1")]),
            stmt("delete from t where id = ?", &[Some("2")]),
            stmt(
                "insert into t (id, x) values (?, ?)",
                &[Some("3"), Some("c")],
            ),
        ],
    );
    assert_eq!(result, Ok(3));
    rig.run(2, "select id, x from t order by id", 100);
    assert_eq!(
        rig.collect(2),
        Ok(vec![
            vec![Some("1".into()), Some("A".into())],
            vec![Some("3".into()), Some("c".into())],
        ])
    );
}

#[test]
fn rowcount_mismatch_rolls_back() {
    let rig = rig(false);
    table_with_rows(
        &rig,
        "create table t(id integer primary key, x text); insert into t values (1,'a')",
    );
    let result = apply(
        &rig,
        vec![
            stmt("update t set x = ? where id = ?", &[Some("A"), Some("1")]),
            stmt("update t set x = ? where id = ?", &[Some("B"), Some("9")]),
        ],
    );
    let err = result.expect_err("mismatch");
    assert_eq!(err.index, Some(1));
    assert_eq!(scalar(&rig, 2, "select x from t"), vec![Some("a".into())]);
}

#[test]
fn apply_rolls_back_when_two_rows_match() {
    let rig = rig(false);
    table_with_rows(
        &rig,
        "create table t(id integer primary key, g text); insert into t values (1,'k'),(2,'k')",
    );
    let err =
        apply(&rig, vec![stmt("delete from t where g = ?", &[Some("k")])]).expect_err("two rows");
    assert_eq!(err.index, Some(0));
    assert_eq!(
        scalar(&rig, 2, "select count(*) from t"),
        vec![Some("2".into())]
    );
}

#[test]
fn null_and_empty_string_differ() {
    let rig = rig(false);
    table_with_rows(&rig, "create table t(id integer, x text)");
    let result = apply(
        &rig,
        vec![
            stmt("insert into t values (?, ?)", &[Some("1"), None]),
            stmt("insert into t values (?, ?)", &[Some("2"), Some("")]),
        ],
    );
    assert_eq!(result, Ok(2));
    rig.run(2, "select x is null, length(x) from t order by id", 100);
    assert_eq!(
        rig.collect(2),
        Ok(vec![
            vec![Some("1".into()), None],
            vec![Some("0".into()), Some("0".into())],
        ])
    );
}

#[test]
fn text_params_fill_integer_columns() {
    let rig = rig(false);
    table_with_rows(&rig, "create table t(n integer)");
    assert_eq!(
        apply(&rig, vec![stmt("insert into t values (?)", &[Some("5")])]),
        Ok(1)
    );
    assert_eq!(
        scalar(&rig, 2, "select typeof(n) from t"),
        vec![Some("integer".into())]
    );
}

#[test]
fn apply_is_refused_on_a_read_only_connection() {
    let rig = rig(true);
    let err = apply(&rig, vec![stmt("select 1", &[])]).expect_err("read-only");
    assert_eq!(err.index, None);
    assert!(err.message.contains("read-only"));
}

#[test]
fn apply_is_refused_while_a_manual_transaction_is_open() {
    let rig = rig(false);
    table_with_rows(&rig, "create table t(x)");
    finish_tx_run(&rig, "insert into t values (1)", 2);
    let err = apply(&rig, vec![stmt("insert into t values (?)", &[Some("2")])])
        .expect_err("open transaction");
    assert_eq!(err.index, None);
    rig.tx(TxOp::Rollback);
    assert_eq!(next_tx(&rig), (false, None));
    assert_eq!(row_count(&rig, 3), 0);
}

#[test]
fn apply_after_disconnect_reports_not_connected() {
    let rig = rig(false);
    rig.handle.send(Command::Disconnect { session: S });
    let err = apply(&rig, vec![stmt("select 1", &[])]).expect_err("gone");
    assert_eq!(err.message, "not connected");
}

#[test]
fn apply_queued_behind_a_run_runs_after_it() {
    let rig = rig(false);
    rig.run(1, "create table t(x)", 100);
    rig.handle.send(Command::Apply {
        session: S,
        statements: vec![stmt("insert into t values (?)", &[Some("1")])],
    });
    let mut done = false;
    loop {
        match rig.next() {
            Update::Done { .. } => done = true,
            Update::Applied { result, .. } => {
                assert!(done, "applied before the run finished");
                assert_eq!(result, Ok(1));
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn apply_rolls_back_on_a_constraint_violation() {
    let rig = rig(false);
    table_with_rows(
        &rig,
        "create table t(id integer primary key, u text unique); insert into t values (1,'a')",
    );
    let err = apply(
        &rig,
        vec![
            stmt("insert into t values (?, ?)", &[Some("2"), Some("b")]),
            stmt("insert into t values (?, ?)", &[Some("3"), Some("a")]),
        ],
    )
    .expect_err("unique");
    assert_eq!(err.index, Some(1));
    assert_eq!(
        scalar(&rig, 2, "select count(*) from t"),
        vec![Some("1".into())]
    );
}

#[test]
fn disconnect_during_apply_changes_nothing() {
    let path = make_db("disc");
    let path_str = path.to_str().expect("utf8").to_owned();
    let mut first = connect(conn(&path_str, false), None);
    first.files.push(path);
    assert!(matches!(first.next(), Update::Connected { .. }));
    table_with_rows(
        &first,
        "create table t(id integer primary key, x integer); insert into t values (1, 0)",
    );
    first.handle.send(Command::Apply {
        session: S,
        statements: vec![stmt(
            "update t set x = (with recursive c(n) as (select 1 union all select n+1 from c where n < 500000000) select max(n) from c) where id = ?",
            &[Some("1")],
        )],
    });
    std::thread::sleep(Duration::from_millis(300));
    first.handle.send(Command::Disconnect { session: S });
    std::thread::sleep(Duration::from_millis(500));
    let second = connect(conn(&path_str, false), None);
    assert!(matches!(second.next(), Update::Connected { .. }));
    assert_eq!(
        scalar(&second, 1, "select x from t"),
        vec![Some("0".into())]
    );
    second.run(2, "update t set x = 7 where id = 1", 100);
    assert!(second.collect(2).is_ok());
    assert_eq!(
        scalar(&second, 3, "select x from t"),
        vec![Some("7".into())]
    );
}

const COUNT_PAST_CAP: &str = "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 100005) SELECT x FROM n";

#[test]
fn uncapped_run_streams_past_the_row_cap() {
    let rig = rig(false);
    rig.run_capped(1, COUNT_PAST_CAP, 5000, RunMode::Auto, false);
    assert_eq!(rig.collect_done(1, 5000), (100_005, false));
}

#[test]
fn capped_run_truncates_at_the_row_cap() {
    let rig = rig(false);
    rig.run_capped(1, COUNT_PAST_CAP, 5000, RunMode::Auto, true);
    assert_eq!(rig.collect_done(1, 5000), (100_000, true));
}
