use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, Connection as Db, Error, InterruptHandle, OpenFlags};
use tokio::sync::mpsc;

use super::{ApplyError, Failure, Item, ItemStream};
use crate::config::Connection;
use crate::edit::Stmt;
use crate::value::{Cell, Column};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Lists tables and views; columns come from a per-object pragma so one broken view cannot fail the catalog.
const CATALOG_OBJECTS_SQL: &str = "SELECT name, type FROM sqlite_schema WHERE type IN ('table','view') \
AND substr(name, 1, 7) <> 'sqlite_' AND coalesce(sql,'') NOT LIKE 'CREATE VIRTUAL TABLE%' \
ORDER BY name LIMIT 5001";

pub struct Sqlite {
    conn: Arc<Mutex<Db>>,
    interrupt: Arc<InterruptHandle>,
    current: CurrentRun,
}

#[derive(Clone)]
pub struct SqliteCanceller {
    interrupt: Arc<InterruptHandle>,
    current: CurrentRun,
}

// Each query() installs a fresh flag so a late cancel for run N cannot be cleared by run N+1.
type CurrentRun = Arc<Mutex<Arc<AtomicBool>>>;

enum Work {
    Sql(String),
    Catalog,
}

enum RunState {
    NotStarted {
        conn: Arc<Mutex<Db>>,
        work: Work,
        cancelled: Arc<AtomicBool>,
    },
    Running(mpsc::Receiver<Result<Item, Failure>>),
}

fn resolve(database: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    let path = Path::new(database);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    match root {
        Some(root) => Ok(root.join(path)),
        None => Err("a relative SQLite path needs a project".into()),
    }
}

fn open(path: &Path, read_only: bool) -> Result<Db, Error> {
    let mode = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let db = Db::open_with_flags(path, mode | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(BUSY_TIMEOUT)?;
    if read_only {
        // ATTACH inherits the read-only open flag, so query_only mostly guards temp tables.
        db.pragma_update(None, "query_only", true)?;
        // Blocks ATTACH and VACUUM INTO, which could otherwise write a file even with query_only on.
        db.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    }
    Ok(db)
}

fn cell(value: ValueRef<'_>) -> Cell {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            let mut hex = String::with_capacity(2 + b.len() * 2);
            hex.push_str("\\x");
            for byte in b {
                hex.push_str(&format!("{byte:02x}"));
            }
            Some(hex)
        }
    }
}

const INTERRUPTED: &str = "interrupted";

fn failure(e: &Error, script: &str) -> Failure {
    match e {
        // The error carries the unprepared tail of the script, so the offset is relative to it.
        Error::SqlInputError {
            msg, sql, offset, ..
        } => Failure {
            message: msg.clone(),
            position: script
                .len()
                .checked_sub(sql.len())
                .zip(usize::try_from(*offset).ok())
                .map(|(base, o)| {
                    let abs = base + o;
                    script.char_indices().take_while(|(i, _)| *i < abs).count() + 1
                }),
        },
        _ => Failure {
            message: e.to_string(),
            position: None,
        },
    }
}

fn interrupted() -> Failure {
    Failure {
        message: INTERRUPTED.into(),
        position: None,
    }
}

fn lock_failure() -> Failure {
    Failure {
        message: "the SQLite connection is unavailable".into(),
        position: None,
    }
}

type Tx = mpsc::Sender<Result<Item, Failure>>;

fn send(tx: &Tx, item: Result<Item, Failure>) -> bool {
    tx.blocking_send(item).is_ok()
}

fn apply_blocking(
    conn: &Mutex<Db>,
    statements: &[Stmt],
    stop: &AtomicBool,
) -> Result<u64, ApplyError> {
    let db = conn.lock().map_err(|_| ApplyError {
        index: None,
        message: lock_failure().message,
    })?;
    let rollback = || {
        let _ = db.execute_batch("ROLLBACK");
    };
    if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
        return Err(ApplyError {
            index: None,
            message: failure(&e, "").message,
        });
    }
    for (i, stmt) in statements.iter().enumerate() {
        if stop.load(Ordering::SeqCst) {
            rollback();
            return Err(ApplyError::timed_out());
        }
        match db.execute(&stmt.sql, rusqlite::params_from_iter(stmt.params.iter())) {
            Ok(1) => {}
            Ok(n) => {
                rollback();
                return Err(ApplyError::mismatch(i, n as u64));
            }
            Err(e) => {
                rollback();
                if stop.load(Ordering::SeqCst) {
                    return Err(ApplyError::timed_out());
                }
                return Err(ApplyError {
                    index: Some(i),
                    message: failure(&e, &stmt.sql).message,
                });
            }
        }
    }
    if stop.load(Ordering::SeqCst) {
        rollback();
        return Err(ApplyError::timed_out());
    }
    match db.execute_batch("COMMIT") {
        Ok(()) => Ok(statements.len() as u64),
        Err(e) => {
            rollback();
            Err(ApplyError::commit(&failure(&e, "").message))
        }
    }
}

fn run_catalog(db: &Db, tx: &Tx, cancelled: &AtomicBool) {
    let objects: Result<Vec<(String, String)>, Error> =
        db.prepare(CATALOG_OBJECTS_SQL).and_then(|mut stmt| {
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        });
    let objects = match objects {
        Ok(o) => o,
        Err(e) => {
            send(tx, Err(failure(&e, CATALOG_OBJECTS_SQL)));
            return;
        }
    };
    for (name, kind) in objects {
        if cancelled.load(Ordering::SeqCst) {
            send(tx, Err(interrupted()));
            return;
        }
        let columns: Result<Vec<Vec<Cell>>, Error> = db
            .prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1) ORDER BY cid")
            .and_then(|mut stmt| {
                stmt.query_map([&name], |r| {
                    let notnull: i64 = r.get(2)?;
                    let pk: i64 = r.get(3)?;
                    Ok(vec![
                        Some(r.get::<_, String>(0)?),
                        Some(r.get::<_, String>(1)?),
                        Some(if notnull != 0 { "0" } else { "1" }.to_owned()),
                        Some(if pk > 0 { "1" } else { "0" }.to_owned()),
                    ])
                })?
                .collect()
            });
        if cancelled.load(Ordering::SeqCst) {
            send(tx, Err(interrupted()));
            return;
        }
        let head = [Some("main".to_owned()), Some(name), Some(kind)];
        let rows = match columns {
            Ok(rows) if !rows.is_empty() => rows,
            _ => vec![vec![None; 4]],
        };
        for tail in rows {
            let row = head.iter().cloned().chain(tail).collect();
            if !send(tx, Ok(Item::Row(row))) {
                return;
            }
        }
    }
}

fn run_script(db: &Db, sql: &str, tx: &Tx, cancelled: &AtomicBool) {
    let mut batch = Batch::new(db, sql);
    loop {
        // SQLite drops an interrupt that lands between statements, so the flag is the source of truth.
        if cancelled.load(Ordering::SeqCst) {
            send(tx, Err(interrupted()));
            return;
        }
        let mut stmt = match batch.next() {
            Ok(Some(stmt)) => stmt,
            Ok(None) => return,
            Err(e) => {
                send(tx, Err(failure(&e, sql)));
                return;
            }
        };
        let width = stmt.column_count();
        if width == 0 {
            let before = db.total_changes();
            match stmt.execute([]) {
                Ok(changes) => {
                    // changes() keeps the previous statement's count after DDL.
                    let affected = if db.total_changes() != before {
                        changes as u64
                    } else {
                        0
                    };
                    if !send(tx, Ok(Item::Complete(affected))) {
                        return;
                    }
                }
                Err(e) => {
                    send(tx, Err(failure(&e, sql)));
                    return;
                }
            }
            continue;
        }
        let columns: Vec<Column> = stmt
            .columns()
            .map(|c| Column {
                name: c.name().to_owned(),
                type_name: c.decl_type().map(str::to_lowercase).unwrap_or_default(),
                table: None,
            })
            .collect();
        if !send(tx, Ok(Item::Columns(columns))) {
            return;
        }
        let mut rows = match stmt.query([]) {
            Ok(rows) => rows,
            Err(e) => {
                send(tx, Err(failure(&e, sql)));
                return;
            }
        };
        let mut count = 0u64;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                send(tx, Err(interrupted()));
                return;
            }
            match rows.next() {
                Ok(Some(row)) => {
                    let cells = (0..width)
                        .map(|i| row.get_ref(i).map(cell).unwrap_or(None))
                        .collect();
                    if !send(tx, Ok(Item::Row(cells))) {
                        return;
                    }
                    count += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    send(tx, Err(failure(&e, sql)));
                    return;
                }
            }
        }
        if !send(tx, Ok(Item::Complete(count))) {
            return;
        }
    }
}

impl Sqlite {
    pub async fn connect(
        conn: &Connection,
        root: Option<&Path>,
    ) -> Result<(Sqlite, String), String> {
        let path = resolve(&conn.database, root)?;
        let read_only = conn.read_only;
        let shown = path.display().to_string();
        let db = tokio::task::spawn_blocking(move || open(&path, read_only))
            .await
            .map_err(|_| "opening the SQLite file failed".to_string())?
            .map_err(|e| format!("{shown}: {e}"))?;
        let interrupt = Arc::new(db.get_interrupt_handle());
        Ok((
            Sqlite {
                conn: Arc::new(Mutex::new(db)),
                interrupt,
                current: Arc::new(Mutex::new(Arc::new(AtomicBool::new(false)))),
            },
            format!("SQLite {}", rusqlite::version()),
        ))
    }

    pub fn canceller(&self) -> SqliteCanceller {
        SqliteCanceller {
            interrupt: self.interrupt.clone(),
            current: self.current.clone(),
        }
    }

    pub async fn in_transaction(&self) -> Option<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || conn.lock().ok().map(|db| !db.is_autocommit()))
            .await
            .ok()
            .flatten()
    }

    pub fn query(&self, sql: String) -> ItemStream {
        self.start(Work::Sql(sql))
    }

    /// Streams the catalog in the engine's 7-column row contract.
    pub fn catalog(&self) -> ItemStream {
        self.start(Work::Catalog)
    }

    pub async fn apply(
        &self,
        statements: Vec<Stmt>,
        stop: Arc<AtomicBool>,
    ) -> Result<u64, ApplyError> {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = stop.clone();
        let conn = self.conn.clone();
        let job = tokio::task::spawn_blocking(move || apply_blocking(&conn, &statements, &stop));
        job.await.unwrap_or_else(|_| {
            Err(ApplyError {
                index: None,
                message: "the SQLite connection is unavailable".into(),
            })
        })
    }

    fn start(&self, work: Work) -> ItemStream {
        let cancelled = Arc::new(AtomicBool::new(false));
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = cancelled.clone();
        let start = RunState::NotStarted {
            conn: self.conn.clone(),
            work,
            cancelled,
        };
        // Spawning on first poll means a stream dropped unpolled never runs a statement.
        futures::stream::unfold(start, |state| async move {
            let mut rx = match state {
                RunState::NotStarted {
                    conn,
                    work,
                    cancelled,
                } => {
                    let (tx, rx) = mpsc::channel(1);
                    tokio::task::spawn_blocking(move || match conn.lock() {
                        Ok(db) => match work {
                            Work::Sql(sql) => run_script(&db, &sql, &tx, &cancelled),
                            Work::Catalog => run_catalog(&db, &tx, &cancelled),
                        },
                        Err(_) => {
                            send(&tx, Err(lock_failure()));
                        }
                    });
                    rx
                }
                RunState::Running(rx) => rx,
            };
            rx.recv().await.map(|item| (item, RunState::Running(rx)))
        })
        .boxed()
    }
}

impl SqliteCanceller {
    pub async fn cancel(&self) {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_cancel_does_not_hit_next_query() {
        let path =
            std::env::temp_dir().join(format!("daedalus-sqlite-unit-{}.db", std::process::id()));
        Db::open(&path).expect("create");
        let conn = Connection {
            id: "t".into(),
            name: "t".into(),
            engine: crate::config::Engine::Sqlite,
            host: String::new(),
            port: None,
            database: path.to_str().expect("utf8").into(),
            user: String::new(),
            tls: crate::config::Tls::Prefer,
            ca_file: None,
            read_only: false,
            production: false,
            color: None,
            claude: false,
            projects: Vec::new(),
            save_password: false,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let items = rt.block_on(async {
            let (db, _) = Sqlite::connect(&conn, None).await.expect("connect");
            db.canceller().cancel().await;
            db.query("select 1".into()).collect::<Vec<_>>().await
        });
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            items.as_slice(),
            [
                Ok(Item::Columns(_)),
                Ok(Item::Row(_)),
                Ok(Item::Complete(1))
            ]
        ));
    }
}
