//! The committed `<root>/.daedalus/databases.json`: connections a project shares with its team.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::config::{self, Connection};

/// Location of the shared file relative to the project root.
pub const FILE: &str = ".daedalus/databases.json";
const MAX_BYTES: u64 = 1024 * 1024;
const BROKEN: &str = "fix .daedalus/databases.json first: it is not a valid connection list";
const BAD_ID: &str = "id must be 1-64 letters, digits, '.', '_' or '-'";

// A committed file must never carry these, so they are stripped before serde sees the entry.
const PERSONAL: [(&str, &str); 4] = [
    ("password", "passwords stay in the Keychain"),
    ("claude", "Claude access is set in your own connection list"),
    ("projects", "this file applies to its own project"),
    ("save_password", "saving a password is your own choice"),
];
const KNOWN: [&str; 12] = [
    "id",
    "name",
    "engine",
    "host",
    "port",
    "database",
    "user",
    "tls",
    "ca_file",
    "read_only",
    "production",
    "color",
];

type Parsed = (Vec<Connection>, Vec<String>);

/// Path of the shared file under `root`.
pub fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.chars().count())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Parses the shared file; personal and unknown keys are reported and left out.
pub fn parse(text: &str, root: &Path) -> Result<Parsed, String> {
    if text.trim().is_empty() {
        return Ok((vec![], vec![]));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("databases.json is not valid JSON: {e}"))?;
    let Value::Object(top) = value else {
        return Err("databases.json is not an object".into());
    };
    let mut problems = Vec::new();
    let mut out: Vec<Connection> = Vec::new();
    let mut list = None;
    for (k, v) in top {
        match k.as_str() {
            "version" => {
                if v.as_u64() != Some(1) {
                    problems.push("version must be 1".to_string());
                }
            }
            "connections" => list = Some(v),
            _ => problems.push(format!("has no setting \"{k}\"")),
        }
    }
    let Some(Value::Array(items)) = list else {
        problems.push("\"connections\" must be a list".to_string());
        return Ok((out, problems));
    };
    for (i, item) in items.into_iter().enumerate() {
        let label = match item.get("id").and_then(Value::as_str) {
            Some(id) => format!("connection \"{}\"", id.chars().take(64).collect::<String>()),
            None => format!("connection #{}", i + 1),
        };
        let Value::Object(mut map) = item else {
            problems.push(format!("{label} must be an object"));
            continue;
        };
        for (key, why) in PERSONAL {
            if map.remove(key).is_some() {
                problems.push(format!("{label}: \"{key}\" is ignored here: {why}"));
            }
        }
        for key in map.keys() {
            if !KNOWN.contains(&key.as_str()) {
                problems.push(format!("{label}: has no field \"{key}\""));
            }
        }
        let mut conn = match serde_json::from_value::<Connection>(Value::Object(map)) {
            Ok(c) => c,
            Err(e) => {
                problems.push(format!("{label}: {e}"));
                continue;
            }
        };
        if !valid_id(&conn.id) {
            problems.push(format!("{label}: {BAD_ID}"));
            continue;
        }
        if conn.name.trim().is_empty() {
            problems.push(format!("{label}: name must not be empty"));
            continue;
        }
        if out.iter().any(|c| c.id == conn.id) {
            problems.push(format!("{label}: duplicate id, only the first is used"));
            continue;
        }
        if let Some(ca) = conn.ca_file.as_mut()
            && ca.is_relative()
        {
            *ca = root.join(&*ca);
        }
        conn.claude = false;
        conn.projects = vec![];
        conn.save_password = false;
        out.push(conn);
    }
    Ok((out, problems))
}

/// Reads and parses the file; a missing file is an empty list with no problems.
pub fn load(root: &Path) -> Result<Parsed, String> {
    let file = path(root);
    let meta = match fs::metadata(&file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], vec![])),
        Err(e) => return Err(format!("read {FILE}: {e}")),
    };
    if meta.len() > MAX_BYTES {
        return Err(format!("{FILE} is larger than 1 MiB"));
    }
    let text = fs::read_to_string(&file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            format!("{FILE} is not UTF-8")
        } else {
            format!("read {FILE}: {e}")
        }
    })?;
    parse(&text, root)
}

/// Reads the raw document, or `None` when the file does not exist; a broken file is an error.
fn read_raw(root: &Path) -> anyhow::Result<Option<Value>> {
    let text = match fs::read_to_string(path(root)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {FILE}")),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.get("connections").is_some_and(Value::is_array) => Ok(Some(v)),
        _ => bail!(BROKEN),
    }
}

fn write_raw(root: &Path, value: &Value) -> anyhow::Result<()> {
    fs::create_dir_all(root.join(".daedalus"))?;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    // The file is meant to be committed and holds no secrets.
    config::write_atomic(&path(root), &bytes, 0o644)
}

fn entries(doc: &mut Value) -> &mut Vec<Value> {
    doc["connections"]
        .as_array_mut()
        .expect("checked by read_raw")
}

fn has_id(entry: &Value, id: &str) -> bool {
    entry.get("id").and_then(Value::as_str) == Some(id)
}

/// Inserts or replaces the entry with `conn.id`, keeping every other entry and key as written.
pub fn upsert(root: &Path, conn: &Connection) -> anyhow::Result<()> {
    if !valid_id(&conn.id) {
        bail!(BAD_ID);
    }
    let mut doc = read_raw(root)?.unwrap_or_else(|| json!({"version": 1, "connections": []}));
    let mut conn = conn.clone();
    if let Some(rel) = conn
        .ca_file
        .as_ref()
        .and_then(|p| p.strip_prefix(root).ok())
    {
        conn.ca_file = Some(rel.to_path_buf());
    }
    let mut entry = serde_json::to_value(&conn)?;
    if let Some(map) = entry.as_object_mut() {
        for key in ["claude", "projects", "save_password"] {
            map.remove(key);
        }
    }
    let list = entries(&mut doc);
    match list.iter_mut().find(|e| has_id(e, &conn.id)) {
        Some(slot) => *slot = entry,
        None => list.push(entry),
    }
    write_raw(root, &doc)
}

/// Removes the entry with `id`; false when there was none (the file is then not rewritten).
pub fn remove(root: &Path, id: &str) -> anyhow::Result<bool> {
    let Some(mut doc) = read_raw(root)? else {
        return Ok(false);
    };
    let list = entries(&mut doc);
    let before = list.len();
    list.retain(|e| !has_id(e, id));
    if list.len() == before {
        return Ok(false);
    }
    write_raw(root, &doc)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::config::{Engine, Tls};

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("daedalus-shared-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn doc(entries: &str) -> String {
        format!(r#"{{"version":1,"connections":[{entries}]}}"#)
    }

    fn conn(id: &str) -> Connection {
        Connection {
            id: id.into(),
            name: id.into(),
            engine: Engine::Postgres,
            host: "db.example.com".into(),
            port: Some(5432),
            database: "app".into(),
            user: "me".into(),
            tls: Tls::Require,
            ca_file: None,
            read_only: false,
            production: false,
            color: None,
            claude: false,
            projects: vec![],
            save_password: false,
        }
    }

    fn write(root: &Path, text: &str) {
        fs::create_dir_all(root.join(".daedalus")).unwrap();
        fs::write(path(root), text).unwrap();
    }

    const PG: &str = r#"{"id":"pg","name":"Main","engine":"postgres","host":"h","port":5432,"user":"u","database":"d"}"#;

    #[test]
    fn parse_reads_entries_as_connections() {
        let sqlite = r#"{"id":"lite","name":"Lite","engine":"sqlite","database":"data/app.db"}"#;
        let (conns, problems) = parse(&doc(&format!("{PG},{sqlite}")), Path::new("/r")).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].host, "h");
        assert_eq!(conns[0].port, Some(5432));
        assert_eq!(conns[1].database, "data/app.db");
    }

    #[test]
    fn password_key_is_reported_and_dropped() {
        let e = r#"{"id":"pg","name":"Main","engine":"postgres","password":"s3cret"}"#;
        let result = parse(&doc(e), Path::new("/r"));
        assert!(!format!("{result:?}").contains("s3cret"));
        let (conns, problems) = result.unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("\"password\""));
    }

    #[test]
    fn shared_file_cannot_turn_on_claude() {
        let e = r#"{"id":"pg","name":"Main","engine":"postgres","claude":true}"#;
        let (conns, problems) = parse(&doc(e), Path::new("/r")).unwrap();
        assert!(!conns[0].claude);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn projects_and_save_password_are_ignored() {
        let e = r#"{"id":"pg","name":"Main","engine":"postgres","projects":["/x"],"save_password":true}"#;
        let (conns, problems) = parse(&doc(e), Path::new("/r")).unwrap();
        assert!(conns[0].projects.is_empty());
        assert!(!conns[0].save_password);
        assert_eq!(problems.len(), 2);
    }

    #[test]
    fn unknown_field_is_reported_but_entry_kept() {
        let e = r#"{"id":"pg","name":"Main","engine":"postgres","flavour":"x"}"#;
        let (conns, problems) = parse(&doc(e), Path::new("/r")).unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("\"flavour\""));
    }

    #[test]
    fn unknown_engine_entry_is_reported_and_skipped() {
        let e = r#"{"id":"o","name":"Ora","engine":"oracle"}"#;
        let (conns, problems) = parse(&doc(e), Path::new("/r")).unwrap();
        assert!(conns.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("connection \"o\""));
    }

    #[test]
    fn bad_ids_and_empty_names_are_skipped() {
        let long = "a".repeat(65);
        let entries = [
            r#"{"id":"","name":"A","engine":"sqlite"}"#.to_string(),
            r#"{"id":"a/b","name":"A","engine":"sqlite"}"#.to_string(),
            r#"{"id":"../x","name":"A","engine":"sqlite"}"#.to_string(),
            format!(r#"{{"id":"{long}","name":"A","engine":"sqlite"}}"#),
            r#"{"id":"ok","name":" ","engine":"sqlite"}"#.to_string(),
        ];
        let (conns, problems) = parse(&doc(&entries.join(",")), Path::new("/r")).unwrap();
        assert!(conns.is_empty());
        assert_eq!(problems.len(), 5);
    }

    #[test]
    fn duplicate_id_keeps_the_first() {
        let b = PG.replace("Main", "Other");
        let (conns, problems) = parse(&doc(&format!("{PG},{b}")), Path::new("/r")).unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].name, "Main");
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn relative_ca_file_resolves_against_root() {
        let e = r#"{"id":"pg","name":"M","engine":"postgres","ca_file":"certs/ca.pem"}"#;
        let (conns, _) = parse(&doc(e), Path::new("/r")).unwrap();
        assert_eq!(
            conns[0].ca_file.as_deref(),
            Some(Path::new("/r/certs/ca.pem"))
        );
    }

    #[test]
    fn not_json_and_not_object_are_errors() {
        assert!(
            parse("{ nope", Path::new("/r"))
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert_eq!(
            parse("[]", Path::new("/r")).unwrap_err(),
            "databases.json is not an object"
        );
    }

    #[test]
    fn empty_text_is_empty() {
        assert_eq!(parse("  \n", Path::new("/r")).unwrap(), (vec![], vec![]));
    }

    #[test]
    fn wrong_version_is_a_problem() {
        let (conns, problems) = parse(
            &format!(r#"{{"version":2,"connections":[{PG}]}}"#),
            Path::new("/r"),
        )
        .unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(problems, ["version must be 1"]);
    }

    #[test]
    fn missing_file_loads_empty() {
        let root = temp("missing");
        assert_eq!(load(&root).unwrap(), (vec![], vec![]));
    }

    #[test]
    fn oversize_file_is_an_error() {
        let root = temp("big");
        write(&root, &" ".repeat(MAX_BYTES as usize + 1));
        assert!(load(&root).unwrap_err().contains("larger than 1 MiB"));
    }

    #[test]
    fn upsert_appends_then_replaces_by_id() {
        let root = temp("upsert");
        let mut c = conn("a");
        c.claude = true;
        c.save_password = true;
        c.projects = vec!["/x".into()];
        upsert(&root, &c).unwrap();
        let text = fs::read_to_string(path(&root)).unwrap();
        for key in ["claude", "projects", "save_password"] {
            assert!(!text.contains(key), "{key} was written");
        }
        c.host = "other".into();
        upsert(&root, &c).unwrap();
        upsert(&root, &conn("b")).unwrap();
        let (conns, problems) = load(&root).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].host, "other");
        assert_eq!(conns[1].id, "b");
    }

    #[test]
    fn upsert_keeps_unknown_entries_verbatim() {
        let root = temp("verbatim");
        let ora = r#"{"id":"o","name":"Ora","engine":"oracle","zeta":1,"alpha":{"k":[1,2]}}"#;
        write(&root, &doc(ora));
        upsert(&root, &conn("a")).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(path(&root)).unwrap()).unwrap();
        let want: Value = serde_json::from_str(ora).unwrap();
        assert_eq!(after["connections"][0], want);
        assert_eq!(after["connections"][1]["id"], "a");
    }

    #[test]
    fn upsert_refuses_a_broken_file() {
        let root = temp("broken");
        write(&root, "{ not json");
        assert!(upsert(&root, &conn("a")).is_err());
        assert!(remove(&root, "a").is_err());
        assert_eq!(fs::read_to_string(path(&root)).unwrap(), "{ not json");
        let not_list = r#"{"version":1,"connections":{}}"#;
        write(&root, not_list);
        assert!(upsert(&root, &conn("a")).is_err());
        assert_eq!(fs::read_to_string(path(&root)).unwrap(), not_list);
    }

    #[test]
    fn upsert_refuses_a_bad_id() {
        let root = temp("badid");
        assert!(upsert(&root, &conn("a/b")).is_err());
        assert!(!path(&root).exists());
    }

    #[test]
    fn remove_reports_whether_it_removed() {
        let root = temp("remove");
        assert!(!remove(&root, "a").unwrap());
        assert!(!path(&root).exists());
        upsert(&root, &conn("a")).unwrap();
        assert!(!remove(&root, "zzz").unwrap());
        assert!(remove(&root, "a").unwrap());
        assert!(load(&root).unwrap().0.is_empty());
    }

    #[test]
    fn written_file_is_0644_and_ends_with_newline() {
        let root = temp("mode");
        upsert(&root, &conn("a")).unwrap();
        let mode = fs::metadata(path(&root)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert!(fs::read_to_string(path(&root)).unwrap().ends_with("}\n"));
    }

    #[test]
    fn ca_file_under_root_is_written_relative() {
        let root = temp("ca");
        let mut c = conn("a");
        c.ca_file = Some(root.join("certs/ca.pem"));
        upsert(&root, &c).unwrap();
        let raw: Value = serde_json::from_str(&fs::read_to_string(path(&root)).unwrap()).unwrap();
        assert_eq!(raw["connections"][0]["ca_file"], "certs/ca.pem");
        assert_eq!(
            load(&root).unwrap().0[0].ca_file,
            Some(root.join("certs/ca.pem"))
        );
    }
}
