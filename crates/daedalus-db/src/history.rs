use std::collections::HashSet;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Entries kept per project and connection.
pub const MAX_ENTRIES: usize = 1000;
/// Compaction only runs this far past the cap so most appends stay cheap.
const SLACK: usize = 100;
/// Statements longer than this are not recorded.
pub const MAX_SQL_BYTES: usize = 16 * 1024;

/// One executed statement; holds SQL, so no Debug.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix time in milliseconds.
    pub at: u64,
    pub sql: String,
}

/// History file for a project and connection key; hashed because ids come from editable JSON.
pub fn path(data_dir: &Path, root: &Path, key: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(root.to_string_lossy().as_bytes());
    hash.update(b"\n");
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    data_dir.join("history").join(format!("{hex}.jsonl"))
}

/// Whether the statement may set or reveal a credential, so it is never recorded.
pub fn has_secret(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    if words(&lower).any(|w| matches!(w, "password" | "passwd") || w.ends_with("_password"))
        || url_with_password(&lower)
    {
        return true;
    }
    // Backslash escaping differs per engine, so a misjudged quote must not hide a keyword.
    [true, false].into_iter().any(|escapes| {
        let stripped = strip_comments(&lower, escapes);
        let tokens: Vec<&str> = words(&stripped).collect();
        follows(&tokens, "identified", &["by", "with"])
            || follows(&tokens, "pragma", &["key", "rekey", "hexkey", "hexrekey"])
    })
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .filter(|w| !w.is_empty())
}

fn follows(tokens: &[&str], first: &str, later: &[&str]) -> bool {
    tokens
        .iter()
        .position(|t| *t == first)
        .is_some_and(|i| tokens[i + 1..].iter().any(|t| later.contains(t)))
}

/// Replaces comments outside quoted text with a space so they cannot split a keyword pair.
fn strip_comments(text: &str, backslash_escapes: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if backslash_escapes && c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            out.push(c);
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 1;
            out.push(' ');
        } else if c == '#' || (c == '-' && chars.get(i + 1) == Some(&'-')) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push('\n');
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn url_with_password(text: &str) -> bool {
    text.match_indices("://").any(|(i, _)| {
        let authority: &str = text[i + 3..]
            .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`'))
            .next()
            .unwrap_or("");
        authority
            .find(':')
            .is_some_and(|colon| authority[colon..].contains('@'))
    })
}

/// Records the statements in order and returns how many were kept.
pub fn append(path: &Path, sqls: &[&str], at: u64) -> Result<usize> {
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let bytes = read_all(path)?;
    let torn = bytes.last().is_some_and(|b| *b != b'\n');
    let mut entries = parse(&bytes);
    let mut new = Vec::new();
    for sql in sqls {
        let sql = sql.trim();
        if sql.is_empty()
            || sql.len() > MAX_SQL_BYTES
            || has_secret(sql)
            || entries.last().is_some_and(|last| last.sql == sql)
        {
            continue;
        }
        let entry = Entry {
            at,
            sql: sql.to_string(),
        };
        entries.push(entry.clone());
        new.push(entry);
    }
    if new.is_empty() {
        return Ok(0);
    }
    if entries.len() > MAX_ENTRIES + SLACK {
        let keep = &entries[entries.len() - MAX_ENTRIES..];
        rewrite(path, keep)?;
    } else {
        let mut buf = String::new();
        if torn {
            buf.push('\n');
        }
        buf.push_str(&lines(&new)?);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
        file.write_all(buf.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(new.len())
}

/// Entries oldest first; a missing file is empty and unreadable lines are skipped.
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    Ok(parse(&read_all(path)?))
}

fn read_all(path: &Path) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn parse(bytes: &[u8]) -> Vec<Entry> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Entry>(line).ok())
        .collect()
}

/// Newest-first matches, each SQL text once, at most `limit`.
pub fn search<'a>(entries: &'a [Entry], query: &str, limit: usize) -> Vec<&'a Entry> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut found = Vec::new();
    for entry in entries.iter().rev() {
        if found.len() >= limit {
            break;
        }
        let lower = entry.sql.to_lowercase();
        if terms.iter().all(|t| lower.contains(t.as_str())) && seen.insert(entry.sql.as_str()) {
            found.push(entry);
        }
    }
    found
}

fn lines(entries: &[Entry]) -> Result<String> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serde_json::to_string(entry)?);
        out.push('\n');
    }
    Ok(out)
}

fn rewrite(path: &Path, entries: &[Entry]) -> Result<()> {
    let body = lines(entries)?;
    let tmp = path.with_extension("jsonl.tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(body.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("daedalus-hist-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(at: u64, sql: &str) -> Entry {
        Entry {
            at,
            sql: sql.into(),
        }
    }

    fn sqls(entries: &[&Entry]) -> Vec<String> {
        entries.iter().map(|e| e.sql.clone()).collect()
    }

    #[test]
    fn path_is_hex_jsonl_and_differs_per_project_and_key() {
        let data = Path::new("/data");
        let a = path(data, Path::new("/p1"), "g:a");
        assert_eq!(a.parent().unwrap(), data.join("history"));
        let name = a.file_name().unwrap().to_str().unwrap();
        let stem = name.strip_suffix(".jsonl").unwrap();
        assert_eq!(stem.len(), 16);
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, path(data, Path::new("/p2"), "g:a"));
        assert_ne!(a, path(data, Path::new("/p1"), "g:b"));
        let evil = path(data, Path::new("/p1"), "g:../x");
        assert_eq!(evil.parent().unwrap(), data.join("history"));
    }

    #[test]
    fn append_then_load_round_trips_in_order() {
        let file = temp("round").join("h").join("a.jsonl");
        assert_eq!(append(&file, &["select 1", "select 2"], 5).unwrap(), 2);
        assert_eq!(append(&file, &["select 3"], 9).unwrap(), 1);
        let loaded = load(&file).unwrap();
        assert!(
            loaded
                == vec![
                    entry(5, "select 1"),
                    entry(5, "select 2"),
                    entry(9, "select 3")
                ]
        );
    }

    #[test]
    fn blank_oversize_and_repeated_statements_are_skipped() {
        let file = temp("skip").join("a.jsonl");
        let big = "x".repeat(MAX_SQL_BYTES + 1);
        let kept = append(
            &file,
            &["  ", &big, " select 1 ", "select 1", "select 2", "select 1"],
            1,
        )
        .unwrap();
        assert_eq!(kept, 3);
        let loaded = load(&file).unwrap();
        let texts: Vec<&str> = loaded.iter().map(|e| e.sql.as_str()).collect();
        assert_eq!(texts, ["select 1", "select 2", "select 1"]);
        assert_eq!(append(&file, &["select 1"], 2).unwrap(), 0);
    }

    #[test]
    fn secret_statements_are_never_written() {
        let secrets = [
            "ALTER USER bob WITH PASSWORD 'x'",
            "CREATE ROLE r LOGIN ENCRYPTED PASSWORD 'x'",
            "CREATE USER 'u'@'%' IDENTIFIED BY 'x'",
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY 'x'",
            "SET PASSWORD FOR u = 'x'",
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD='x'",
            "PRAGMA key = 'x'",
            "SELECT dblink_connect('postgres://u:x@h/db')",
            "SELECT dblink_connect('postgres://u:hun/ter2@h/db')",
            "create user u identified by 'x'",
            "CREATE USER u\nIDENTIFIED\nBY 'x'",
            "CREATE USER u IDENTIFIED /* c */ BY 'x'",
            "ALTER USER u IDENTIFIED -- c\n BY 'x'",
            "ALTER USER u IDENTIFIED # c\n BY 'x'",
            "select 1; ALTER ROLE r PASSWORD 'x'",
            "PRAGMA main.key = 'x'",
        ];
        let file = temp("secret").join("a.jsonl");
        for sql in secrets {
            assert!(has_secret(sql));
            assert_eq!(append(&file, &[sql], 1).unwrap(), 0);
        }
        assert!(!file.exists());
        append(&file, &["select 1"], 1).unwrap();
        append(&file, &secrets, 2).unwrap();
        let bytes = fs::read(&file).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!secrets.iter().any(|s| text.contains(s)));
        assert!(!text.contains("PASSWORD"));
    }

    #[test]
    fn ordinary_statements_are_kept() {
        let file = temp("ordinary").join("a.jsonl");
        let ok = [
            "SELECT password_hash FROM users",
            "SELECT 'identified' AS by_name",
            "select * from users where id = 1",
            "select 'http://example.com/a/b'",
        ];
        for sql in ok {
            assert!(!has_secret(sql));
        }
        assert_eq!(append(&file, &ok, 1).unwrap(), ok.len());
    }

    #[test]
    fn file_and_dir_are_owner_only() {
        let dir = temp("perm");
        let file = dir.join("history").join("a.jsonl");
        append(&file, &["select 1"], 1).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);

        let wide = dir.join("wide.jsonl");
        fs::write(&wide, "").unwrap();
        fs::set_permissions(&wide, fs::Permissions::from_mode(0o644)).unwrap();
        append(&wide, &["select 1"], 1).unwrap();
        assert_eq!(mode(&wide), 0o600);
    }

    #[test]
    fn compacts_to_the_newest_max_entries() {
        let file = temp("compact").join("a.jsonl");
        let texts: Vec<String> = (0..=MAX_ENTRIES + SLACK)
            .map(|i| format!("select {i}"))
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        append(&file, &refs, 1).unwrap();
        let loaded = load(&file).unwrap();
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded.last().unwrap().sql, texts.last().unwrap().as_str());
        assert_eq!(loaded[0].sql, texts[texts.len() - MAX_ENTRIES].as_str());
        assert!(!file.with_extension("jsonl.tmp").exists());
    }

    #[test]
    fn torn_last_line_is_skipped_and_not_glued() {
        let file = temp("torn").join("a.jsonl");
        let good = serde_json::to_string(&entry(1, "select 1")).unwrap();
        fs::write(&file, format!("{good}\n{{\"at\":2,\"sql\":\"sel")).unwrap();
        append(&file, &["select 3"], 3).unwrap();
        assert!(load(&file).unwrap() == vec![entry(1, "select 1"), entry(3, "select 3")]);
    }

    #[test]
    fn missing_file_loads_empty() {
        assert!(
            load(&temp("missing").join("none.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn garbage_lines_are_skipped() {
        let file = temp("garbage").join("a.jsonl");
        let good = serde_json::to_string(&entry(1, "select 1")).unwrap();
        let mut bytes = b"not json\n\n\xff\xfe\n".to_vec();
        bytes.extend_from_slice(format!("{good}\n{{}}\n").as_bytes());
        fs::write(&file, bytes).unwrap();
        assert!(load(&file).unwrap() == vec![entry(1, "select 1")]);
    }

    #[test]
    fn search_is_newest_first_case_insensitive_and_term_and() {
        let entries = vec![
            entry(1, "SELECT * FROM Users"),
            entry(2, "select 1"),
            entry(3, "select name from users where id = 1"),
        ];
        assert_eq!(
            sqls(&search(&entries, "USERS select", 10)),
            ["select name from users where id = 1", "SELECT * FROM Users"]
        );
        assert!(search(&entries, "users orders", 10).is_empty());
    }

    #[test]
    fn search_collapses_repeats_and_honours_limit() {
        let entries = vec![
            entry(1, "select 1"),
            entry(2, "select 2"),
            entry(3, "select 1"),
            entry(4, "select 3"),
        ];
        assert_eq!(
            sqls(&search(&entries, "select", 10)),
            ["select 3", "select 1", "select 2"]
        );
        assert_eq!(
            sqls(&search(&entries, "select", 2)),
            ["select 3", "select 1"]
        );
    }

    #[test]
    fn empty_query_lists_newest() {
        let entries = vec![entry(1, "a"), entry(2, "b")];
        assert_eq!(sqls(&search(&entries, "  ", 10)), ["b", "a"]);
    }
}
