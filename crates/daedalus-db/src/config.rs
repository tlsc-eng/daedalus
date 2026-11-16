use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
    /// A folder colour name (kebab-case) that marks the connection; unknown names show none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
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
    pub fn empty() -> Self {
        Store {
            version: 1,
            connections: vec![],
        }
    }

    /// Replaces the connection with the same id, else appends it.
    pub fn upsert(&mut self, conn: Connection) {
        match self.connections.iter_mut().find(|c| c.id == conn.id) {
            Some(slot) => *slot = conn,
            None => self.connections.push(conn),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<Connection> {
        let i = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(i))
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
    write_atomic(path, &serde_json::to_vec_pretty(store)?, 0o600)
}

/// Replaces `path` with `bytes` through a temp file in the same directory, created with `mode`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    // A stale temp may be world-readable or held open by another process.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", tmp.display())),
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    out.write_all(bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    // Without this, a power loss after the rename can leave an empty file under the real name.
    out.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Connections that apply to `root`, sorted by name; a personal entry hides a shared one with the same id.
pub fn for_project(store: &Store, shared: &[Connection], root: Option<&Path>) -> Vec<Listed> {
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
    if root.is_some() {
        let hidden: Vec<String> = out.iter().map(|l| l.conn.id.clone()).collect();
        out.extend(
            shared
                .iter()
                .filter(|c| !hidden.contains(&c.id))
                .map(|c| Listed {
                    conn: c.clone(),
                    origin: Origin::Project,
                }),
        );
    }
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
                .map(|r| r.as_os_str().as_encoded_bytes())
                .unwrap_or_default();
            let tls = format!("{:?}", conn.tls);
            let ca = conn
                .ca_file
                .as_deref()
                .map(|p| p.as_os_str().as_encoded_bytes())
                .unwrap_or_default();
            let parts: [&[u8]; 9] = [
                root,
                conn.id.as_bytes(),
                conn.engine.name().as_bytes(),
                conn.host.as_bytes(),
                port.as_bytes(),
                conn.database.as_bytes(),
                conn.user.as_bytes(),
                tls.as_bytes(),
                ca,
            ];
            let mut hasher = Sha256::new();
            for part in parts {
                hasher.update((part.len() as u64).to_le_bytes());
                hasher.update(part);
            }
            let digest = hasher.finalize();
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

fn percent_decode(s: &str) -> Result<String, &'static str> {
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
    String::from_utf8(out).map_err(|_| "credentials are not valid UTF-8")
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
        s @ ("jdbc" | "http" | "https" | "file" | "redis" | "mongodb" | "mongodb+srv"
        | "sqlserver" | "oracle") => return Err(format!("unsupported scheme {s}")),
        _ => return Err("unsupported scheme".to_string()),
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
        color: None,
        claude: false,
        projects: vec![],
        save_password: false,
    };

    if engine == Engine::Sqlite {
        if url.host_str().is_some_and(|h| !h.is_empty()) {
            return Err("use sqlite:relative/path.db or sqlite:///absolute/path.db".into());
        }
        let path = percent_decode(url.path())?.to_string();
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
    let database = percent_decode(url.path().trim_start_matches('/'))?;
    if database.contains('@')
        || url.query().is_some_and(|q| q.contains('@'))
        || url.fragment().is_some_and(|f| f.contains('@'))
    {
        return Err("special characters in the password must be percent-encoded".into());
    }
    let mut conn = blank(format!("{database} on {host}"), host, database);
    conn.port = url.port();
    conn.user = percent_decode(url.username())?;
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
        .transpose()?
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
            color: None,
            claude: true,
            projects: vec!["/work/a".into()],
            save_password: true,
        }
    }

    #[test]
    fn color_round_trips_and_is_omitted_when_none() {
        let mut c = conn("a", "A");
        assert!(!serde_json::to_string(&c).unwrap().contains("color"));
        c.color = Some("sky".into());
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Connection>(&json).unwrap(), c);
    }

    #[test]
    fn upsert_replaces_by_id_and_appends_new() {
        let mut store = Store::empty();
        store.upsert(conn("a", "A"));
        store.upsert(conn("b", "B"));
        store.upsert(conn("a", "A2"));
        assert_eq!(store.connections.len(), 2);
        assert_eq!(store.connections[0].name, "A2");
    }

    #[test]
    fn remove_returns_the_connection() {
        let mut store = Store::empty();
        store.upsert(conn("a", "A"));
        assert_eq!(store.remove("a").unwrap().id, "a");
        assert!(store.remove("a").is_none());
        assert!(store.connections.is_empty());
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
            for_project(&store, &[], root.map(Path::new))
                .into_iter()
                .map(|l| l.conn.id)
                .collect()
        };
        assert_eq!(ids(Some("/work/a")), ["a", "e"]);
        assert_eq!(ids(Some("/work/b")), ["e"]);
        assert_eq!(ids(None), ["e"]);
    }

    #[test]
    fn shared_entries_list_as_project_origin() {
        let store = Store {
            version: 1,
            connections: vec![conn("g", "Mid")],
        };
        let shared = [conn("a", "Alpha"), conn("z", "Zed")];
        let listed = for_project(&store, &shared, Some(Path::new("/work/a")));
        let got: Vec<(&str, Origin)> = listed
            .iter()
            .map(|l| (l.conn.id.as_str(), l.origin))
            .collect();
        assert_eq!(
            got,
            [
                ("a", Origin::Project),
                ("g", Origin::Global),
                ("z", Origin::Project)
            ]
        );
    }

    #[test]
    fn shared_entries_need_a_root() {
        let shared = [conn("a", "Alpha")];
        assert!(for_project(&Store::empty(), &shared, None).is_empty());
    }

    #[test]
    fn personal_entry_hides_shared_with_same_id() {
        let mut mine = conn("x", "Mine");
        mine.projects = vec!["/work/a".into()];
        let store = Store {
            version: 1,
            connections: vec![mine],
        };
        let shared = [conn("x", "Shared")];
        let hit = for_project(&store, &shared, Some(Path::new("/work/a")));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].origin, Origin::Global);
        let other = for_project(&store, &shared, Some(Path::new("/work/b")));
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].origin, Origin::Project);
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
        let mut t = conn("a", "A");
        t.tls = Tls::Disable;
        assert_ne!(before, keychain_account(Origin::Project, Some(root), &t));
        let mut ca = conn("a", "A");
        ca.ca_file = Some("/other/ca.pem".into());
        assert_ne!(before, keychain_account(Origin::Project, Some(root), &ca));
    }

    #[test]
    fn keychain_account_fields_cannot_collide() {
        let root = Path::new("/work/a");
        let mut a = conn("a", "A");
        a.database = "a\0b".into();
        a.user = "c".into();
        let mut b = a.clone();
        b.database = "a".into();
        b.user = "b\0c".into();
        assert_ne!(
            keychain_account(Origin::Project, Some(root), &a),
            keychain_account(Origin::Project, Some(root), &b)
        );
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

    #[test]
    fn unencoded_slash_in_password_is_rejected_not_saved() {
        let err = from_url("postgres://admin:1234/s3cret@h/db").unwrap_err();
        assert_eq!(
            err,
            "special characters in the password must be percent-encoded"
        );
        assert!(!err.contains("s3cret"));
    }

    #[test]
    fn unknown_scheme_error_does_not_echo_it() {
        for input in ["bob:s3cret@db.example.com/app", "s3cret:foo"] {
            let err = from_url(input).unwrap_err();
            assert!(!err.contains("bob") && !err.contains("s3cret"), "{err}");
        }
        assert!(
            from_url("jdbc:postgresql://h/db")
                .unwrap_err()
                .contains("jdbc")
        );
    }

    #[test]
    fn invalid_utf8_password_is_an_error() {
        assert_eq!(
            from_url("postgres://u:%E2%82%AC%FF@h/db").unwrap_err(),
            "credentials are not valid UTF-8"
        );
    }

    #[test]
    fn sqlite_url_with_host_is_rejected() {
        for input in ["sqlite://data/app.db", "sqlite://data.db"] {
            assert_eq!(
                from_url(input).unwrap_err(),
                "use sqlite:relative/path.db or sqlite:///absolute/path.db"
            );
        }
    }

    #[test]
    fn stale_temp_file_is_replaced_not_reused() {
        let dir = temp("stale-temp");
        let path = dir.join("config.json");
        let tmp = path.with_extension("json.tmp");
        let link = dir.join("stale-link");
        fs::write(&tmp, b"stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        fs::hard_link(&tmp, &link).unwrap();
        save(&path, &Store::empty()).unwrap();
        assert_eq!(fs::read(&link).unwrap(), b"stale");
        fs::remove_dir_all(&dir).unwrap();
    }
}
