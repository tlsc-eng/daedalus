use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use daedalus_db::worker::{Command, Handle, RunId, SessionId, Update, start};
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
        read_only,
        production: false,
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
        self.handle.send(Command::Run {
            session: S,
            run: RunId(run),
            sql: sql.into(),
            page,
            timeout: None,
        });
    }

    fn cancel(&self, run: u64) {
        self.handle.send(Command::Cancel {
            session: S,
            run: RunId(run),
        });
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
    rusqlite::Connection::open(&main)
        .and_then(|c| c.execute_batch("create table t(x)"))
        .expect("seed");
    let mut rig = connect(conn(main.to_str().expect("utf8"), true), None);
    rig.files.extend([main, other.clone()]);
    assert!(matches!(rig.next(), Update::Connected { .. }));
    rig.run(1, "insert into t values (1)", 100);
    assert!(rig.collect(1).is_err());
    rig.run(
        2,
        &format!("attach '{}' as o; create table o.t(x)", other.display()),
        100,
    );
    assert!(rig.collect(2).is_err());
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
