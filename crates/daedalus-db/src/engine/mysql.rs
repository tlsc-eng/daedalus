use std::fmt::Write as _;
use std::sync::Arc;

use futures::{StreamExt, stream};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, Opts, OptsBuilder, SslOpts, Value};
use tokio::sync::{Mutex, mpsc};

use super::{Failure, Item, ItemStream};
use crate::config::{Connection, Tls};
use crate::secret::Secret;
use crate::value::{Cell, Column};

const DEFAULT_PORT: u16 = 3306;

pub struct Mysql {
    conn: Arc<Mutex<Conn>>,
    opts: Arc<Opts>,
    id: u32,
}

#[derive(Clone)]
pub struct MysqlCanceller {
    opts: Arc<Opts>,
    id: u32,
}

/// mysql_async wants the bare address for IPv6, not the URL bracket form.
fn driver_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

fn ssl_opts(tls: Tls, ca_file: Option<&std::path::Path>) -> Option<SslOpts> {
    match tls {
        Tls::Disable => None,
        Tls::Prefer | Tls::RequireUnverified => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        Tls::Require => {
            let ssl = SslOpts::default();
            Some(match ca_file {
                Some(path) => ssl.with_root_certs(vec![path.to_path_buf().into()]),
                None => ssl,
            })
        }
    }
}

/// Built field by field because the Debug output of `Opts` prints the password.
fn build_opts(conn: &Connection, password: Option<&Secret>, tls: Option<SslOpts>) -> Opts {
    OptsBuilder::default()
        .ip_or_hostname(driver_host(&conn.host))
        .tcp_port(conn.port.unwrap_or(DEFAULT_PORT))
        .user((!conn.user.is_empty()).then_some(conn.user.as_str()))
        .db_name((!conn.database.is_empty()).then_some(conn.database.as_str()))
        .pass(password.map(|p| p.expose().to_owned()))
        .ssl_opts(tls)
        .into()
}

fn describe(e: &mysql_async::Error) -> String {
    e.to_string()
}

fn failure(e: &mysql_async::Error) -> Failure {
    Failure {
        message: describe(e),
        position: None,
    }
}

fn is_no_tls(e: &mysql_async::Error) -> bool {
    matches!(
        e,
        mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer)
    )
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("\\x");
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn to_cell(value: &Value) -> Cell {
    match value {
        Value::NULL => None,
        Value::Bytes(b) => Some(match std::str::from_utf8(b) {
            Ok(s) => s.to_owned(),
            Err(_) => hex(b),
        }),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        Value::Date(y, mo, d, h, mi, s, us) => {
            let mut text = format!("{y:04}-{mo:02}-{d:02}");
            if (*h, *mi, *s, *us) != (0, 0, 0, 0) {
                let _ = write!(text, " {h:02}:{mi:02}:{s:02}");
                if *us != 0 {
                    let _ = write!(text, ".{us:06}");
                }
            }
            Some(text)
        }
        Value::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(*days) * 24 + u64::from(*h);
            let mut text = format!("{}{hours:02}:{mi:02}:{s:02}", if *neg { "-" } else { "" });
            if *us != 0 {
                let _ = write!(text, ".{us:06}");
            }
            Some(text)
        }
    }
}

fn column(c: &mysql_async::Column) -> Column {
    let ty = format!("{:?}", c.column_type());
    Column {
        name: c.name_str().into_owned(),
        type_name: ty
            .strip_prefix("MYSQL_TYPE_")
            .unwrap_or(&ty)
            .to_ascii_lowercase(),
        table: None,
    }
}

async fn open(opts: &Opts) -> Result<Conn, mysql_async::Error> {
    Conn::new(opts.clone()).await
}

async fn open_with_fallback(
    conn: &Connection,
    password: Option<&Secret>,
) -> Result<(Conn, Opts), mysql_async::Error> {
    let opts = build_opts(conn, password, ssl_opts(conn.tls, conn.ca_file.as_deref()));
    match open(&opts).await {
        Err(e) if conn.tls == Tls::Prefer && is_no_tls(&e) => {
            let plain = build_opts(conn, password, None);
            open(&plain).await.map(|c| (c, plain))
        }
        other => other.map(|c| (c, opts)),
    }
}

impl Mysql {
    pub async fn connect(
        conn: &Connection,
        password: Option<&Secret>,
    ) -> Result<(Mysql, String), String> {
        let (mut live, opts) = open_with_fallback(conn, password)
            .await
            .map_err(|e| describe(&e))?;
        if conn.read_only {
            live.query_drop("SET SESSION TRANSACTION READ ONLY")
                .await
                .map_err(|e| describe(&e))?;
        }
        let version: Option<String> = live.query_first("SELECT VERSION()").await.ok().flatten();
        let server = match version {
            Some(v) if v.contains("MariaDB") => format!("MariaDB {v}"),
            Some(v) => format!("MySQL {v}"),
            None => "MySQL".to_owned(),
        };
        let id = live.id();
        Ok((
            Mysql {
                conn: Arc::new(Mutex::new(live)),
                opts: Arc::new(opts),
                id,
            },
            server,
        ))
    }

    pub fn canceller(&self) -> MysqlCanceller {
        MysqlCanceller {
            opts: self.opts.clone(),
            id: self.id,
        }
    }

    pub fn query(&self, sql: String) -> ItemStream {
        let (tx, rx) = mpsc::channel(1);
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let mut guard = conn.lock_owned().await;
            if let Err(e) = run(&mut guard, &sql, &tx).await {
                let _ = tx.send(Err(failure(&e))).await;
            }
        });
        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) }).boxed()
    }
}

/// Returns early once the receiver is gone, after draining the rest so the connection stays usable.
async fn run(
    conn: &mut Conn,
    sql: &str,
    tx: &mpsc::Sender<Result<Item, Failure>>,
) -> Result<(), mysql_async::Error> {
    let mut result = conn.query_iter(sql).await?;
    while let Some(columns) = result.columns() {
        let affected = result.affected_rows();
        let has_rows = !columns.is_empty();
        if has_rows {
            let cols = columns.iter().map(column).collect();
            if tx.send(Ok(Item::Columns(cols))).await.is_err() {
                return result.drop_result().await;
            }
        }
        let mut count = 0u64;
        while let Some(row) = result.next().await? {
            let cells = (0..row.len())
                .map(|i| row.as_ref(i).and_then(to_cell))
                .collect();
            count += 1;
            if tx.send(Ok(Item::Row(cells))).await.is_err() {
                return result.drop_result().await;
            }
        }
        let done = if has_rows { count } else { affected };
        if tx.send(Ok(Item::Complete(done))).await.is_err() {
            return result.drop_result().await;
        }
    }
    Ok(())
}

impl MysqlCanceller {
    /// Every failure is ignored because the worker re-sends the cancel until the query answers.
    pub async fn cancel(&self) {
        let Ok(mut side) = open(&self.opts).await else {
            return;
        };
        let _ = side.query_drop(format!("KILL QUERY {}", self.id)).await;
        let _ = side.disconnect().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_keep_null_distinct_from_empty() {
        assert_eq!(to_cell(&Value::NULL), None);
        assert_eq!(to_cell(&Value::Bytes(Vec::new())), Some(String::new()));
    }

    #[test]
    fn invalid_utf8_becomes_hex() {
        assert_eq!(
            to_cell(&Value::Bytes(vec![0xff, 0x00])),
            Some("\\xff00".into())
        );
        assert_eq!(to_cell(&Value::Bytes("é".into())), Some("é".into()));
    }

    #[test]
    fn dates_and_times_print_plainly() {
        assert_eq!(
            to_cell(&Value::Date(2024, 1, 2, 3, 4, 5, 0)),
            Some("2024-01-02 03:04:05".into())
        );
        assert_eq!(
            to_cell(&Value::Date(2024, 1, 2, 0, 0, 0, 0)),
            Some("2024-01-02".into())
        );
        assert_eq!(
            to_cell(&Value::Time(true, 1, 2, 3, 4, 500)),
            Some("-26:03:04.000500".into())
        );
    }

    #[test]
    fn ipv6_brackets_are_stripped_for_the_driver() {
        assert_eq!(driver_host("[::1]"), "::1");
        assert_eq!(driver_host("localhost"), "localhost");
    }

    #[test]
    fn tls_modes_map_to_ssl_opts() {
        assert!(ssl_opts(Tls::Disable, None).is_none());
        for tls in [Tls::Prefer, Tls::RequireUnverified] {
            let ssl = ssl_opts(tls, None).expect("ssl");
            assert!(ssl.accept_invalid_certs() && ssl.skip_domain_validation());
        }
        let ssl = ssl_opts(Tls::Require, None).expect("ssl");
        assert!(!ssl.accept_invalid_certs() && !ssl.skip_domain_validation());
    }
}
