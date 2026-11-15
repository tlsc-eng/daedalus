use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, Connection as Db, Error, InterruptHandle, OpenFlags};
use tokio::sync::mpsc;

use super::{Failure, Item, ItemStream};
use crate::config::Connection;
use crate::value::{Cell, Column};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Sqlite {
    conn: Arc<Mutex<Db>>,
    interrupt: Arc<InterruptHandle>,
}

#[derive(Clone)]
pub struct SqliteCanceller {
    interrupt: Arc<InterruptHandle>,
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
        // ATTACHed files are opened read-write regardless of the main connection's flags.
        db.pragma_update(None, "query_only", true)?;
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

/// Offsets are only exact for the first statement, because a script is prepared one tail at a time.
fn failure(e: &Error, sql: &str, first: bool) -> Failure {
    let position = match e {
        Error::SqlInputError { offset, .. } if first => usize::try_from(*offset)
            .ok()
            .map(|o| sql.char_indices().take_while(|(i, _)| *i < o).count() + 1),
        _ => None,
    };
    Failure {
        message: e.to_string(),
        position,
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

fn run_script(db: &Db, sql: &str, tx: &Tx) {
    let mut batch = Batch::new(db, sql);
    let mut first = true;
    loop {
        let mut stmt = match batch.next() {
            Ok(Some(stmt)) => stmt,
            Ok(None) => return,
            Err(e) => {
                send(tx, Err(failure(&e, sql, first)));
                return;
            }
        };
        let width = stmt.column_count();
        if width == 0 {
            match stmt.execute([]) {
                Ok(changes) => {
                    if !send(tx, Ok(Item::Complete(changes as u64))) {
                        return;
                    }
                }
                Err(e) => {
                    send(tx, Err(failure(&e, sql, first)));
                    return;
                }
            }
            first = false;
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
                send(tx, Err(failure(&e, sql, first)));
                return;
            }
        };
        let mut count = 0u64;
        loop {
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
                    send(tx, Err(failure(&e, sql, first)));
                    return;
                }
            }
        }
        if !send(tx, Ok(Item::Complete(count))) {
            return;
        }
        first = false;
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
            },
            format!("SQLite {}", rusqlite::version()),
        ))
    }

    pub fn canceller(&self) -> SqliteCanceller {
        SqliteCanceller {
            interrupt: self.interrupt.clone(),
        }
    }

    pub fn query(&self, sql: String) -> ItemStream {
        let conn = self.conn.clone();
        let (tx, rx) = mpsc::channel(1);
        tokio::task::spawn_blocking(move || match conn.lock() {
            Ok(db) => run_script(&db, &sql, &tx),
            Err(_) => {
                send(&tx, Err(lock_failure()));
            }
        });
        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }
}

impl SqliteCanceller {
    pub async fn cancel(&self) {
        self.interrupt.interrupt();
    }
}
