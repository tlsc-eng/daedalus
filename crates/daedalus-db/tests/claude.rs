use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use daedalus_db::claude::{Collector, Step, gate};
use daedalus_db::worker::{Command, Handle, RunId, RunMode, SessionId, Update, start};
use daedalus_db::{Connection, Engine, Tls};
use serde_json::Value;

const S: SessionId = SessionId(1);
const WAIT: Duration = Duration::from_secs(20);

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Rig {
    handle: Handle,
    updates: async_channel::Receiver<Update>,
    file: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

fn conn(database: &str) -> Connection {
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
        read_only: false,
        production: false,
        color: None,
        claude: true,
        projects: Vec::new(),
        save_password: false,
    }
}

fn rig() -> Rig {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("daedalus-claude-{}-{n}.db", std::process::id()));
    rusqlite::Connection::open(&file).expect("create db");
    let (tx, updates) = async_channel::unbounded();
    let handle = start(tx);
    handle.send(Command::Connect {
        session: S,
        conn: conn(file.to_str().expect("utf8")),
        password: None,
        root: None,
    });
    let rig = Rig {
        handle,
        updates,
        file,
    };
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

    fn run(&self, run: u64, sql: &str, page: usize, timeout: Option<Duration>, mode: RunMode) {
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

    /// Gates `sql`, runs it read-only and feeds the collector, sending Cancel when asked.
    fn ask(&self, run: u64, sql: &str, max_rows: u32, timeout: Option<Duration>) -> Step {
        let sql = gate(sql, Engine::Sqlite).expect("gate");
        self.run(run, &sql, max_rows as usize, timeout, RunMode::ReadOnly);
        self.collect(run, max_rows)
    }

    fn collect(&self, run: u64, max_rows: u32) -> Step {
        let mut collector = Collector::new(max_rows);
        loop {
            match collector.feed(self.next()) {
                Step::Wait => {}
                Step::Cancel => self.handle.send(Command::Cancel {
                    session: S,
                    run: RunId(run),
                }),
                Step::Reject(message) => {
                    self.handle.send(Command::Cancel {
                        session: S,
                        run: RunId(run),
                    });
                    return Step::Failed(message);
                }
                end => return end,
            }
        }
    }

    fn exec(&self, run: u64, sql: &str) {
        self.run(run, sql, 100, None, RunMode::Auto);
        loop {
            match self.next() {
                Update::Done { .. } => return,
                Update::RunError { message, .. } => panic!("write failed: {message}"),
                _ => {}
            }
        }
    }
}

fn json(step: Step) -> Value {
    match step {
        Step::Done(s) => serde_json::from_str(&s).expect("json"),
        Step::Failed(_) => panic!("run failed"),
        _ => panic!("not finished"),
    }
}

const COUNT_300: &str =
    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) SELECT i FROM n";

#[test]
fn select_returns_rows_as_json() {
    let rig = rig();
    rig.exec(1, "CREATE TABLE t (a INTEGER, b TEXT)");
    rig.exec(2, "INSERT INTO t VALUES (1, 'x'), (2, NULL)");
    let v = json(rig.ask(3, "SELECT a, b FROM t ORDER BY a", 100, None));
    assert_eq!(v["row_count"], 2);
    assert_eq!(v["columns"][0]["name"], "a");
    assert_eq!(v["rows"][0][1], "x");
    assert!(v["rows"][1][1].is_null());
    assert_eq!(v["truncated"], false);
    assert!(v["truncated_by"].is_null());
}

#[test]
fn row_cap_cancels_and_session_stays_usable() {
    let rig = rig();
    let v = json(rig.ask(1, COUNT_300, 50, None));
    assert_eq!(v["row_count"], 50);
    assert_eq!(v["truncated_by"], "max_rows");

    let v = json(rig.ask(2, "SELECT 1", 50, None));
    assert_eq!(v["row_count"], 1);

    // A write only succeeds if query_only was restored after the cancelled run.
    rig.exec(3, "CREATE TABLE after_cancel (a INTEGER)");
}

#[test]
fn read_only_backstop_blocks_a_write() {
    let rig = rig();
    rig.exec(1, "CREATE TABLE t (a INTEGER)");
    rig.run(2, "INSERT INTO t VALUES (1)", 100, None, RunMode::ReadOnly);
    assert!(matches!(rig.collect(2, 100), Step::Failed(_)));
    let v = json(rig.ask(3, "SELECT a FROM t", 100, None));
    assert_eq!(v["row_count"], 0);
}

#[test]
fn timeout_is_reported() {
    let rig = rig();
    let sql =
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";
    match rig.ask(1, sql, 100, Some(Duration::from_millis(200))) {
        Step::Failed(m) => assert!(m.contains("timed out"), "{m}"),
        _ => panic!("expected a timeout"),
    }
}
