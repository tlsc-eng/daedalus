use std::future::ready;
use std::sync::Arc;

use futures::{StreamExt, stream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::config::SslMode;
use tokio_postgres::error::DbError;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Catalog query: schema, table, kind, column, type, nullable, pk as text; up to 5001 tables.
pub const CATALOG_SQL: &str = "SELECT t.nspname::text, t.relname::text, \
CASE WHEN t.relkind IN ('v','m') THEN 'view' ELSE 'table' END, \
a.attname::text, format_type(a.atttypid, a.atttypmod), \
CASE WHEN a.attnum IS NULL THEN NULL WHEN a.attnotnull THEN '0' ELSE '1' END, \
CASE WHEN a.attnum IS NULL THEN NULL \
WHEN EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = t.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)) THEN '1' ELSE '0' END \
FROM (SELECT c.oid, n.nspname, c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
WHERE c.relkind IN ('r','p','v','m','f') AND NOT c.relispartition AND n.nspname NOT IN ('pg_catalog','information_schema') \
AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp_%' \
ORDER BY n.nspname, c.relname LIMIT 5001) t \
LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum > 0 AND NOT a.attisdropped \
ORDER BY t.nspname, t.relname, a.attnum";

use super::{Failure, Item, ItemStream};
use crate::config::{Connection, Tls};
use crate::secret::Secret;
use crate::value::{Cell, Column, TableRef};

const DEFAULT_PORT: u16 = 5432;

pub struct Pg {
    client: Arc<Client>,
    tls: MakeRustlsConnect,
}

#[derive(Clone)]
pub struct PgCanceller {
    token: CancelToken,
    tls: MakeRustlsConnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verify {
    Anything,
    Roots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TlsPlan {
    ssl_mode: SslMode,
    verify: Verify,
}

/// Disable never reaches the connector, so it takes the cheap no-roots path.
fn plan(tls: Tls) -> TlsPlan {
    match tls {
        Tls::Disable => TlsPlan {
            ssl_mode: SslMode::Disable,
            verify: Verify::Anything,
        },
        Tls::Prefer => TlsPlan {
            ssl_mode: SslMode::Prefer,
            verify: Verify::Anything,
        },
        Tls::RequireUnverified => TlsPlan {
            ssl_mode: SslMode::Require,
            verify: Verify::Anything,
        },
        Tls::Require => TlsPlan {
            ssl_mode: SslMode::Require,
            verify: Verify::Roots,
        },
    }
}

#[derive(Debug)]
struct AcceptAny(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(
    verify: Verify,
    ca_file: Option<&std::path::Path>,
) -> Result<ClientConfig, String> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = match verify {
        Verify::Anything => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
            .with_no_client_auth(),
        Verify::Roots => {
            let mut roots = RootCertStore::empty();
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            if let Some(path) = ca_file {
                let certs = CertificateDer::pem_file_iter(path)
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| format!("cannot read CA file: {e}"))?;
                roots.add_parsable_certificates(certs);
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    Ok(config)
}

/// tokio-postgres wants the bare address for IPv6, not the URL bracket form.
fn driver_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

fn pg_config(
    conn: &Connection,
    password: Option<&Secret>,
    mode: SslMode,
) -> tokio_postgres::Config {
    let mut config = tokio_postgres::Config::new();
    config
        .host(driver_host(&conn.host))
        .port(conn.port.unwrap_or(DEFAULT_PORT))
        .ssl_mode(mode);
    if !conn.database.is_empty() {
        config.dbname(&conn.database);
    }
    if !conn.user.is_empty() {
        config.user(&conn.user);
    }
    if let Some(password) = password {
        config.password(password.expose());
    }
    config
}

fn to_cell(value: Option<&str>) -> Cell {
    value.map(str::to_owned)
}

/// The error's own `Display` is just "db error", so the server message or the source chain carries the detail.
fn describe(e: &tokio_postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        return db.to_string();
    }
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn failure(e: &tokio_postgres::Error) -> Failure {
    let position = e
        .as_db_error()
        .and_then(DbError::position)
        .and_then(|p| match p {
            tokio_postgres::error::ErrorPosition::Original(n) => Some(*n as usize),
            tokio_postgres::error::ErrorPosition::Internal { .. } => None,
        });
    Failure {
        message: describe(e),
        position,
    }
}

impl Pg {
    pub async fn connect(
        conn: &Connection,
        password: Option<&Secret>,
    ) -> Result<(Pg, String), String> {
        let plan = plan(conn.tls);
        let tls = MakeRustlsConnect::new(client_config(plan.verify, conn.ca_file.as_deref())?);
        let config = pg_config(conn, password, plan.ssl_mode);
        let (client, connection) = config
            .connect(tls.clone())
            .await
            .map_err(|e| describe(&e))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        if conn.read_only {
            client
                .batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                .await
                .map_err(|e| describe(&e))?;
        }
        let version = client
            .simple_query("SHOW server_version")
            .await
            .ok()
            .and_then(|msgs| {
                msgs.into_iter().find_map(|m| match m {
                    SimpleQueryMessage::Row(r) => r.get(0).map(str::to_owned),
                    _ => None,
                })
            });
        let server = match version {
            Some(v) => format!("PostgreSQL {v}"),
            None => "PostgreSQL".to_owned(),
        };
        Ok((
            Pg {
                client: Arc::new(client),
                tls,
            },
            server,
        ))
    }

    pub fn canceller(&self) -> PgCanceller {
        PgCanceller {
            token: self.client.cancel_token(),
            tls: self.tls.clone(),
        }
    }

    /// Prepare only supplies type and table info and is skipped for scripts, where it would abort a transaction.
    pub fn query(&self, sql: String) -> ItemStream {
        let client = self.client.clone();
        let start = async move {
            let typed: Vec<Column> = if may_be_script(&sql) {
                Vec::new()
            } else {
                match client.prepare(&sql).await {
                    Ok(stmt) => stmt
                        .columns()
                        .iter()
                        .map(|c| Column {
                            name: c.name().to_owned(),
                            type_name: c.type_().name().to_owned(),
                            table: c.table_oid().map(|oid| TableRef { oid }),
                        })
                        .collect(),
                    Err(e) => return stream::once(ready(Err(failure(&e)))).boxed(),
                }
            };
            match client.simple_query_raw(&sql).await {
                Ok(rows) => rows
                    .filter_map(move |msg| {
                        ready(match msg {
                            Ok(SimpleQueryMessage::RowDescription(cols)) => {
                                let columns = cols
                                    .iter()
                                    .enumerate()
                                    .map(|(i, c)| match typed.get(i) {
                                        Some(t)
                                            if typed.len() == cols.len() && t.name == c.name() =>
                                        {
                                            t.clone()
                                        }
                                        _ => Column {
                                            name: c.name().to_owned(),
                                            type_name: String::new(),
                                            table: None,
                                        },
                                    })
                                    .collect();
                                Some(Ok(Item::Columns(columns)))
                            }
                            Ok(SimpleQueryMessage::Row(row)) => Some(Ok(Item::Row(
                                (0..row.len()).map(|i| to_cell(row.get(i))).collect(),
                            ))),
                            Ok(SimpleQueryMessage::CommandComplete(n)) => {
                                Some(Ok(Item::Complete(n)))
                            }
                            Ok(_) => None,
                            Err(e) => Some(Err(failure(&e))),
                        })
                    })
                    .boxed(),
                Err(e) => stream::once(ready(Err(failure(&e)))).boxed(),
            }
        };
        stream::once(start).flatten().boxed()
    }
}

/// Conservative: any `;` besides trailing ones may mean several statements; a false positive only costs column types.
fn may_be_script(sql: &str) -> bool {
    sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';')
        .contains(';')
}

impl PgCanceller {
    pub async fn cancel(&self) {
        let _ = self.token.cancel_query(self.tls.clone()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(host: &str, tls: Tls) -> Connection {
        Connection {
            id: "t".into(),
            name: "t".into(),
            engine: crate::config::Engine::Postgres,
            host: host.into(),
            port: Some(1),
            database: "db".into(),
            user: "u".into(),
            tls,
            ca_file: None,
            read_only: false,
            production: false,
            claude: false,
            projects: Vec::new(),
            save_password: false,
        }
    }

    #[test]
    fn cells_keep_null_distinct_from_empty() {
        assert_eq!(to_cell(None), None);
        assert_eq!(to_cell(Some("")), Some(String::new()));
        assert_ne!(to_cell(None), to_cell(Some("")));
    }

    #[test]
    fn tls_mode_maps_to_connector() {
        assert_eq!(plan(Tls::Disable).ssl_mode, SslMode::Disable);
        assert_eq!(
            plan(Tls::Prefer),
            TlsPlan {
                ssl_mode: SslMode::Prefer,
                verify: Verify::Anything
            }
        );
        assert_eq!(
            plan(Tls::RequireUnverified),
            TlsPlan {
                ssl_mode: SslMode::Require,
                verify: Verify::Anything
            }
        );
        assert_eq!(
            plan(Tls::Require),
            TlsPlan {
                ssl_mode: SslMode::Require,
                verify: Verify::Roots
            }
        );
        for tls in [
            Tls::Disable,
            Tls::Prefer,
            Tls::RequireUnverified,
            Tls::Require,
        ] {
            assert!(client_config(plan(tls).verify, None).is_ok());
        }
    }

    #[test]
    fn missing_ca_file_is_an_error() {
        let err = client_config(
            Verify::Roots,
            Some(std::path::Path::new("/nonexistent/ca.pem")),
        );
        assert!(err.is_err());
    }

    #[test]
    fn ipv6_brackets_are_stripped_for_the_driver() {
        assert_eq!(driver_host("[::1]"), "::1");
        assert_eq!(driver_host("localhost"), "localhost");
        let config = pg_config(&conn("[::1]", Tls::Disable), None, SslMode::Disable);
        assert_eq!(
            config.get_hosts(),
            [tokio_postgres::config::Host::Tcp("::1".into())]
        );
    }
}
