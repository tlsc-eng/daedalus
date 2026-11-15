use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::Secret;

/// File name of the global connection list inside the config directory.
pub const FILE: &str = "databases.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
    Sqlite,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Mysql => "mysql",
            Engine::Sqlite => "sqlite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tls {
    Disable,
    Prefer,
    Require,
    RequireUnverified,
}

impl Tls {
    /// Local servers rarely have a certificate, remote ones must prove theirs.
    pub fn default_for(host: &str) -> Tls {
        match host {
            "" | "localhost" | "127.0.0.1" | "::1" | "[::1]" => Tls::Prefer,
            h if h.starts_with('/') => Tls::Prefer,
            _ => Tls::Require,
        }
    }

    fn default_prefer() -> Tls {
        Tls::Prefer
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Database name; for SQLite the file path (relative paths resolve against the project root).
    #[serde(default)]
    pub database: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default = "Tls::default_prefer")]
    pub tls: Tls,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub production: bool,
    /// Lets Claude list and read-only query this connection over MCP; off by default.
    #[serde(default)]
    pub claude: bool,
    /// Project roots this connection is listed for; empty lists it everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<PathBuf>,
    #[serde(default)]
    pub save_password: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    pub version: u32,
    #[serde(deserialize_with = "known_connections")]
    pub connections: Vec<Connection>,
}

impl Store {
    fn empty() -> Self {
        Store {
            version: 1,
            connections: vec![],
        }
    }
}

/// Entries this build cannot read, such as an engine a later release added, are left out rather
/// than failing the whole file.
fn known_connections<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Connection>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Global,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub conn: Connection,
    pub origin: Origin,
}

/// Reads the store; a missing file is an empty store.
pub fn load(path: &Path) -> Result<Store> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::empty()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// Writes via temp file and rename so a crash never leaves a half-written file.
pub fn save(path: &Path, store: &Store) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut out = fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    out.write_all(&serde_json::to_vec_pretty(store)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    // Without this, a power loss after the rename can leave an empty file under the real name.
    out.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Connections that apply to `root`, sorted by name.
pub fn for_project(store: &Store, root: Option<&Path>) -> Vec<Listed> {
    let mut out: Vec<Listed> = store
        .connections
        .iter()
        .filter(|c| {
            c.projects.is_empty() || root.is_some_and(|r| c.projects.iter().any(|p| p == r))
        })
        .map(|c| Listed {
            conn: c.clone(),
            origin: Origin::Global,
        })
        .collect();
    out.sort_by_key(|l| l.conn.name.to_lowercase());
    out
}

/// Keychain account for the connection's password.
pub fn keychain_account(origin: Origin, root: Option<&Path>, conn: &Connection) -> String {
    match origin {
        Origin::Global => conn.id.clone(),
        Origin::Project => {
            let port = conn.port.map(|p| p.to_string()).unwrap_or_default();
            let root = root
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default();
            let parts = [
                root.as_str(),
                conn.id.as_str(),
                conn.engine.name(),
                conn.host.as_str(),
                port.as_str(),
                conn.database.as_str(),
                conn.user.as_str(),
            ];
            let digest = Sha256::digest(parts.join("\0").as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            format!("p-{hex}")
        }
    }
}

/// A fresh 128-bit id as lowercase hex.
pub fn new_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tls_from_param(value: &str) -> Option<Tls> {
    match value.to_ascii_lowercase().as_str() {
        "disable" | "disabled" => Some(Tls::Disable),
        "prefer" | "allow" | "preferred" => Some(Tls::Prefer),
        // libpq's `require` skips certificate checks.
        "require" | "required" => Some(Tls::RequireUnverified),
        "verify-ca" | "verify-full" | "verify_ca" | "verify_identity" => Some(Tls::Require),
        _ => None,
    }
}

/// Parses a connection URL; errors are fixed text because the input may hold a password.
pub fn from_url(input: &str) -> Result<(Connection, Option<Secret>), String> {
    let url = Url::parse(input.trim()).map_err(|_| "not a database URL".to_string())?;
    let engine = match url.scheme() {
        "postgres" | "postgresql" => Engine::Postgres,
        "mysql" | "mariadb" => Engine::Mysql,
        "sqlite" => Engine::Sqlite,
        s => return Err(format!("unsupported scheme {s}")),
    };
    let id = new_id().map_err(|_| "could not create an id".to_string())?;
    let blank = |name: String, host: String, database: String| Connection {
        id: id.clone(),
        name,
        engine,
        host,
        port: None,
        database,
        user: String::new(),
        tls: Tls::Prefer,
        ca_file: None,
        read_only: false,
        production: false,
        claude: false,
        projects: vec![],
        save_password: false,
    };

    if engine == Engine::Sqlite {
        let path = percent_decode(url.path());
        let name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| "missing path".to_string())?;
        return Ok((blank(name, String::new(), path), None));
    }

    let host = url.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return Err("missing host".into());
    }
    let database = percent_decode(url.path().trim_start_matches('/'));
    let mut conn = blank(format!("{database} on {host}"), host, database);
    conn.port = url.port();
    conn.user = percent_decode(url.username());
    conn.tls = Tls::default_for(&conn.host);
    for (key, value) in url.query_pairs() {
        if matches!(key.as_ref(), "sslmode" | "ssl-mode")
            && let Some(tls) = tls_from_param(&value)
        {
            conn.tls = tls;
        }
    }
    let secret = url
        .password()
        .map(percent_decode)
        .filter(|p| !p.is_empty())
        .map(Secret::new);
    Ok((conn, secret))
}

/// The connection as a URL, without the password.
pub fn redacted_url(conn: &Connection) -> String {
    if conn.engine == Engine::Sqlite {
        return format!("sqlite:{}", conn.database);
    }
    let user = if conn.user.is_empty() {
        String::new()
    } else {
        format!("{}@", conn.user)
    };
    let port = conn.port.map(|p| format!(":{p}")).unwrap_or_default();
    format!(
        "{}://{user}{}{port}/{}",
        conn.engine.name(),
        conn.host,
        conn.database
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("daedalus-db-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn conn(id: &str, name: &str) -> Connection {
        Connection {
            id: id.into(),
            name: name.into(),
            engine: Engine::Postgres,
            host: "db.example.com".into(),
            port: Some(5433),
            database: "app".into(),
            user: "me".into(),
            tls: Tls::Require,
            ca_file: Some("/etc/ca.pem".into()),
            read_only: true,
            production: true,
            claude: true,
            projects: vec!["/work/a".into()],
            save_password: true,
        }
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let dir = temp("missing");
        let store = load(&dir.join(FILE)).unwrap();
        assert_eq!(store, Store::empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp("roundtrip");
        let path = dir.join(FILE);
        let store = Store {
            version: 1,
            connections: vec![conn("a", "A")],
        };
        save(&path, &store).unwrap();
        assert_eq!(load(&path).unwrap(), store);
    }

    #[test]
    fn saved_file_is_owner_only() {
        let dir = temp("mode");
        let path = dir.join(FILE);
        save(&path, &Store::empty()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn unknown_engine_entry_is_skipped_not_fatal() {
        let dir = temp("unknown");
        let path = dir.join(FILE);
        let good = serde_json::to_value(conn("a", "A")).unwrap();
        let json = serde_json::json!({
            "version": 1,
            "connections": [good, {"id": "b", "name": "B", "engine": "oracle"}],
        });
        fs::write(&path, json.to_string()).unwrap();
        assert_eq!(load(&path).unwrap().connections.len(), 1);
    }

    #[test]
    fn claude_access_defaults_off() {
        let dir = temp("claude");
        let path = dir.join(FILE);
        let json = r#"{"version":1,"connections":[{"id":"a","name":"A","engine":"sqlite"}]}"#;
        fs::write(&path, json).unwrap();
        let store = load(&path).unwrap();
        assert_eq!(store.connections.len(), 1);
        assert!(!store.connections[0].claude);
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = temp("corrupt");
        let path = dir.join(FILE);
        fs::write(&path, "{ not json").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().starts_with("parse "));
    }

    #[test]
    fn for_project_lists_global_and_bound_connections_only() {
        let mut everywhere = conn("e", "everywhere");
        everywhere.projects.clear();
        let mut a = conn("a", "Alpha");
        a.projects = vec!["/work/a".into()];
        let store = Store {
            version: 1,
            connections: vec![a, everywhere],
        };
        let ids = |root: Option<&str>| -> Vec<String> {
            for_project(&store, root.map(Path::new))
                .into_iter()
                .map(|l| l.conn.id)
                .collect()
        };
        assert_eq!(ids(Some("/work/a")), ["a", "e"]);
        assert_eq!(ids(Some("/work/b")), ["e"]);
        assert_eq!(ids(None), ["e"]);
    }

    #[test]
    fn keychain_account_changes_when_host_changes() {
        let root = Path::new("/work/a");
        let mut c = conn("a", "A");
        let before = keychain_account(Origin::Project, Some(root), &c);
        c.host = "evil.example.com".into();
        let after = keychain_account(Origin::Project, Some(root), &c);
        assert!(before.starts_with("p-"));
        assert_ne!(before, after);
    }

    #[test]
    fn global_account_is_the_id() {
        assert_eq!(
            keychain_account(Origin::Global, None, &conn("abc", "A")),
            "abc"
        );
    }

    #[test]
    fn postgres_url_parses_user_db_port_and_password() {
        let (c, secret) = from_url("postgres://bob:pw@db.example.com:6543/shop").unwrap();
        assert_eq!(c.engine, Engine::Postgres);
        assert_eq!(c.user, "bob");
        assert_eq!(c.host, "db.example.com");
        assert_eq!(c.port, Some(6543));
        assert_eq!(c.database, "shop");
        assert_eq!(c.name, "shop on db.example.com");
        assert_eq!(secret.unwrap().expose(), "pw");
        let (c, secret) = from_url("postgresql://bob@localhost/shop").unwrap();
        assert_eq!(c.port, None);
        assert!(secret.is_none());
    }

    #[test]
    fn percent_encoded_password_is_decoded() {
        let (c, secret) = from_url("postgres://b%40b:p%2Fw%3A%25@h/d%20b").unwrap();
        assert_eq!(c.user, "b@b");
        assert_eq!(c.database, "d b");
        assert_eq!(secret.unwrap().expose(), "p/w:%");
    }

    #[test]
    fn sslmode_maps_to_tls() {
        let tls = |q: &str| from_url(&format!("postgres://u@h/d?{q}")).unwrap().0.tls;
        assert_eq!(tls("sslmode=disable"), Tls::Disable);
        assert_eq!(tls("sslmode=allow"), Tls::Prefer);
        assert_eq!(tls("sslmode=prefer"), Tls::Prefer);
        assert_eq!(tls("sslmode=require"), Tls::RequireUnverified);
        assert_eq!(tls("sslmode=verify-ca"), Tls::Require);
        assert_eq!(tls("sslmode=verify-full"), Tls::Require);
        let my = |q: &str| from_url(&format!("mysql://u@h/d?{q}")).unwrap().0.tls;
        assert_eq!(my("ssl-mode=DISABLED"), Tls::Disable);
        assert_eq!(my("ssl-mode=PREFERRED"), Tls::Prefer);
        assert_eq!(my("ssl-mode=REQUIRED"), Tls::RequireUnverified);
        assert_eq!(my("ssl-mode=VERIFY_CA"), Tls::Require);
        assert_eq!(my("ssl-mode=VERIFY_IDENTITY"), Tls::Require);
    }

    #[test]
    fn mysql_and_mariadb_schemes() {
        assert_eq!(from_url("mysql://u@h/d").unwrap().0.engine, Engine::Mysql);
        assert_eq!(from_url("mariadb://u@h/d").unwrap().0.engine, Engine::Mysql);
    }

    #[test]
    fn sqlite_relative_and_absolute_paths() {
        let (c, s) = from_url("sqlite:data/app.db").unwrap();
        assert_eq!(
            (c.engine, c.database.as_str(), c.name.as_str()),
            (Engine::Sqlite, "data/app.db", "app.db")
        );
        assert!(s.is_none());
        assert_eq!(
            from_url("sqlite:///abs/app.db").unwrap().0.database,
            "/abs/app.db"
        );
    }

    #[test]
    fn default_tls_is_prefer_for_localhost_and_require_for_remote() {
        for h in ["", "localhost", "127.0.0.1", "::1", "/var/run/postgresql"] {
            assert_eq!(Tls::default_for(h), Tls::Prefer, "{h}");
        }
        assert_eq!(Tls::default_for("db.example.com"), Tls::Require);
        assert_eq!(
            from_url("postgres://u@db.example.com/d").unwrap().0.tls,
            Tls::Require
        );
    }

    #[test]
    fn bad_url_error_never_contains_input() {
        for input in [
            "postgres://u:s3cret@",
            "ftp://u:s3cret@h/db",
            "not a url s3cret",
        ] {
            let err = from_url(input).unwrap_err();
            assert!(!err.contains("s3cret"), "{err}");
        }
    }

    #[test]
    fn redacted_url_has_no_password() {
        let (c, _) = from_url("postgres://bob:s3cret@h:5432/shop").unwrap();
        let shown = redacted_url(&c);
        assert_eq!(shown, "postgres://bob@h:5432/shop");
        assert!(!shown.contains("s3cret"));
        assert_eq!(
            redacted_url(&from_url("sqlite:a.db").unwrap().0),
            "sqlite:a.db"
        );
    }
}
