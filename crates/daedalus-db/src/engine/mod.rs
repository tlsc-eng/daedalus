pub mod mysql;
pub mod pg;
pub mod sqlite;

use std::net::SocketAddr;
use std::path::Path;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::Stream;

use crate::config::{Connection, Engine};
use crate::edit::Stmt;
use crate::secret::Secret;
use crate::value::{Cell, Column};

/// One step of a running statement, in the order the server sends them.
pub enum Item {
    Columns(Vec<Column>),
    Row(Vec<Cell>),
    Complete(u64),
}

/// A driver error reduced to what may be shown; built from the error's `Display` only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub position: Option<usize>,
}

/// Why an apply was rolled back; `index` is the failing statement, when one is to blame.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyError {
    pub index: Option<usize>,
    pub message: String,
}

impl ApplyError {
    pub(crate) fn mismatch(index: usize, n: u64) -> Self {
        ApplyError {
            index: Some(index),
            message: format!(
                "Row {}: expected 1 row, the statement matched {n}; nothing was saved.",
                index + 1
            ),
        }
    }

    pub(crate) fn timed_out() -> Self {
        ApplyError {
            index: None,
            message: "Apply timed out; nothing was saved.".into(),
        }
    }

    pub(crate) fn commit(e: &str) -> Self {
        ApplyError {
            index: None,
            message: format!("Commit failed: {e}. Reload to check what was saved."),
        }
    }
}

pub type ItemStream = Pin<Box<dyn Stream<Item = Result<Item, Failure>> + Send>>;

/// A live connection to one database; each engine adds a variant.
pub enum Driver {
    Postgres(pg::Pg),
    Sqlite(sqlite::Sqlite),
    Mysql(mysql::Mysql),
}

/// Asks the server to stop whatever its connection is running.
#[derive(Clone)]
pub enum Canceller {
    Postgres(pg::PgCanceller),
    Sqlite(sqlite::SqliteCanceller),
    Mysql(mysql::MysqlCanceller),
}

impl Driver {
    /// Returns the driver and a server description; the error is safe to show.
    pub async fn connect(
        conn: &Connection,
        via: Option<SocketAddr>,
        password: Option<&Secret>,
        root: Option<&Path>,
    ) -> Result<(Driver, String), String> {
        match conn.engine {
            Engine::Postgres => {
                let (pg, server) = pg::Pg::connect(conn, via, password).await?;
                Ok((Driver::Postgres(pg), server))
            }
            Engine::Sqlite => {
                let (db, server) = sqlite::Sqlite::connect(conn, root).await?;
                Ok((Driver::Sqlite(db), server))
            }
            Engine::Mysql => {
                let (mysql, server) = mysql::Mysql::connect(conn, via, password).await?;
                Ok((Driver::Mysql(mysql), server))
            }
        }
    }

    pub fn query(&self, sql: String) -> ItemStream {
        match self {
            Driver::Postgres(pg) => pg.query(sql),
            Driver::Sqlite(db) => db.query(sql),
            Driver::Mysql(m) => m.query(sql),
        }
    }

    /// Runs the statements in one transaction and commits only if each one touched exactly one row.
    pub async fn apply(
        &self,
        statements: Vec<Stmt>,
        stop: Arc<AtomicBool>,
    ) -> Result<u64, ApplyError> {
        match self {
            Driver::Postgres(pg) => pg.apply(statements, &stop).await,
            Driver::Sqlite(db) => db.apply(statements, stop).await,
            Driver::Mysql(m) => m.apply(statements, &stop).await,
        }
    }

    /// Streams the catalog rows in the 7-column contract of `catalog::from_rows`.
    pub fn catalog(&self) -> ItemStream {
        match self {
            Driver::Postgres(pg) => pg.query(pg::CATALOG_SQL.to_owned()),
            Driver::Sqlite(db) => db.catalog(),
            Driver::Mysql(m) => m.query(mysql::CATALOG_SQL.to_owned()),
        }
    }

    /// Whether the connection has an open transaction, for engines that can say; others return None.
    pub async fn in_transaction(&self) -> Option<bool> {
        match self {
            Driver::Sqlite(db) => db.in_transaction().await,
            Driver::Postgres(_) | Driver::Mysql(_) => None,
        }
    }

    pub fn canceller(&self) -> Canceller {
        match self {
            Driver::Postgres(pg) => Canceller::Postgres(pg.canceller()),
            Driver::Sqlite(db) => Canceller::Sqlite(db.canceller()),
            Driver::Mysql(m) => Canceller::Mysql(m.canceller()),
        }
    }
}

impl Canceller {
    pub async fn cancel(&self) {
        match self {
            Canceller::Postgres(c) => c.cancel().await,
            Canceller::Sqlite(c) => c.cancel().await,
            Canceller::Mysql(c) => c.cancel().await,
        }
    }
}
