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
}

/// Asks the server to stop whatever its connection is running.
#[derive(Clone)]
pub enum Canceller {
    Postgres(pg::PgCanceller),
    Sqlite(sqlite::SqliteCanceller),
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
            Engine::Mysql => Err("engine not supported yet".into()),
        }
    }

    pub fn query(&self, sql: String) -> ItemStream {
        match self {
            Driver::Postgres(pg) => pg.query(sql),
            Driver::Sqlite(db) => db.query(sql),
        }
    }

    pub fn canceller(&self) -> Canceller {
        match self {
            Driver::Postgres(pg) => Canceller::Postgres(pg.canceller()),
            Driver::Sqlite(db) => Canceller::Sqlite(db.canceller()),
        }
    }
}

impl Canceller {
    pub async fn cancel(&self) {
        match self {
            Canceller::Postgres(c) => c.cancel().await,
            Canceller::Sqlite(c) => c.cancel().await,
        }
    }
}
