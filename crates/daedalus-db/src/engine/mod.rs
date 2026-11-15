pub mod mysql;
pub mod pg;
pub mod sqlite;

use std::path::Path;

use std::pin::Pin;

use futures::Stream;

use crate::config::{Connection, Engine};
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
        password: Option<&Secret>,
        root: Option<&Path>,
    ) -> Result<(Driver, String), String> {
        match conn.engine {
            Engine::Postgres => {
                let (pg, server) = pg::Pg::connect(conn, password).await?;
                Ok((Driver::Postgres(pg), server))
            }
            Engine::Sqlite => {
                let (db, server) = sqlite::Sqlite::connect(conn, root).await?;
                Ok((Driver::Sqlite(db), server))
            }
            Engine::Mysql => {
                let (mysql, server) = mysql::Mysql::connect(conn, password).await?;
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
