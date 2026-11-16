//! Suggests connections from a project's `.env*` files and its running compose containers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::Secret;
use crate::config::{Connection, Engine, Listed, Tls, from_url};

/// Env files read for suggestions, highest precedence first.
pub const ENV_FILES: [&str; 4] = [
    ".env.development.local",
    ".env.local",
    ".env.development",
    ".env",
];
const MAX_ENV_BYTES: u64 = 256 * 1024;

/// One env file's text; holds secrets, so no Debug.
pub struct EnvFile {
    pub name: &'static str,
    pub text: String,
}

/// What detection needs from a running container.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerInfo {
    pub name: String,
    pub image: String,
    pub state: String,
    pub ports: Vec<String>,
    pub project: Option<String>,
    pub service: Option<String>,
    pub working_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Env { file: &'static str, key: String },
    Container { name: String },
}

/// A connection found in the project; nothing is saved until the user confirms.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// The id is empty; the caller assigns one on save.
    pub conn: Connection,
    pub password: Option<Secret>,
    pub source: Source,
    pub key: String,
}

/// Reads the known env files that exist, are small and are valid UTF-8.
pub fn read_env(root: &Path) -> Vec<EnvFile> {
    ENV_FILES
        .iter()
        .filter_map(|&name| {
            let path = root.join(name);
            // A symlink pointing outside the project must not leak other files into suggestions.
            let real = path.canonicalize().ok()?;
            if !real.starts_with(root.canonicalize().ok()?) {
                return None;
            }
            let meta = fs::metadata(&real).ok()?;
            if !meta.is_file() || meta.len() > MAX_ENV_BYTES {
                return None;
            }
            let text = fs::read_to_string(&real).ok()?;
            Some(EnvFile { name, text })
        })
        .collect()
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Parses a dotenv subset into `(key, value)` pairs in file order; nothing is expanded.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line_no = i;
        i += 1;
        let line = lines[line_no].trim_start_matches(is_blank);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = match line.strip_prefix("export") {
            Some(rest) if rest.starts_with(is_blank) => rest.trim_start_matches(is_blank),
            _ => line,
        };
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim_end_matches(is_blank);
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            continue;
        }
        let rest = rest.trim_start_matches(is_blank);
        match rest.chars().next() {
            Some(q @ ('"' | '\'' | '`')) => {
                let mut value = String::new();
                let mut pending = Some(&rest[1..]);
                let mut closed = false;
                let mut next = i;
                while let Some(seg) = pending.take() {
                    let mut chars = seg.chars();
                    while let Some(c) = chars.next() {
                        if c == q {
                            closed = true;
                            break;
                        }
                        if c == '\\' && q == '"' {
                            match chars.next() {
                                Some('n') => value.push('\n'),
                                Some('r') => value.push('\r'),
                                Some('t') => value.push('\t'),
                                Some('"') => value.push('"'),
                                Some('\\') => value.push('\\'),
                                Some(o) => {
                                    value.push('\\');
                                    value.push(o);
                                }
                                None => value.push('\\'),
                            }
                        } else {
                            value.push(c);
                        }
                    }
                    if closed {
                        break;
                    }
                    if next < lines.len() {
                        value.push('\n');
                        pending = Some(lines[next]);
                        next += 1;
                    }
                }
                if closed {
                    out.push((key.to_string(), value));
                    i = next;
                }
            }
            _ => {
                let mut end = rest.len();
                let mut prev_blank = true;
                for (idx, c) in rest.char_indices() {
                    if c == '#' && prev_blank {
                        end = idx;
                        break;
                    }
                    prev_blank = is_blank(c);
                }
                let value = rest[..end].trim_matches(is_blank);
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}

/// Compose's project-name normalization: lowercase `[a-z0-9_-]`, no leading `_` or `-`.
pub fn normalize_project(name: &str) -> String {
    let kept: String = name
        .to_lowercase()
        .chars()
        .filter(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
        .collect();
    kept.trim_start_matches(['_', '-']).to_string()
}

/// Identifies the database a connection points at, ignoring name, TLS and flags.
pub fn target_key(conn: &Connection, root: &Path) -> String {
    let (engine, default_port) = match conn.engine {
        Engine::Sqlite => {
            let p = Path::new(&conn.database);
            let path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            };
            return format!("sqlite\0{}", path.display());
        }
        Engine::Postgres => ("postgres", 5432),
        Engine::Mysql => ("mysql", 3306),
    };
    let host = conn.host.to_lowercase();
    let host = match host.as_str() {
        "" | "127.0.0.1" | "::1" | "[::1]" => "localhost",
        h => h,
    };
    let port = conn.port.unwrap_or(default_port);
    format!("{engine}\0{host}\0{port}\0{}\0{}", conn.database, conn.user)
}

type Merged = HashMap<String, (String, &'static str)>;

fn merge(env: &[EnvFile]) -> Merged {
    let mut map = Merged::new();
    for file in env.iter().rev() {
        for (k, v) in parse_env(&file.text) {
            if v.is_empty() || v.contains("${") {
                map.remove(&k);
            } else {
                map.insert(k, (v, file.name));
            }
        }
    }
    map
}

struct Target {
    name: String,
    engine: Engine,
    host: String,
    port: Option<u16>,
    database: String,
    user: String,
    password: Option<String>,
}

fn build(root: &Path, t: Target) -> (Connection, Option<Secret>) {
    let tls = Tls::default_for(&t.host);
    let conn = Connection {
        id: String::new(),
        name: t.name,
        engine: t.engine,
        host: t.host,
        port: t.port,
        database: t.database,
        user: t.user,
        tls,
        ca_file: None,
        ssh: None,
        read_only: false,
        production: false,
        color: None,
        claude: false,
        projects: vec![root.to_path_buf()],
        save_password: false,
    };
    let secret = t.password.filter(|p| !p.is_empty()).map(Secret::new);
    (conn, secret)
}

fn engine_of_image(image: &str) -> Option<(Engine, u16)> {
    let last = image.rsplit('/').next().unwrap_or(image);
    let last = last.split('@').next().unwrap_or(last);
    let last = last.split(':').next().unwrap_or(last).to_lowercase();
    if ["postgres", "postgis", "timescale"]
        .iter()
        .any(|s| last.contains(s))
    {
        Some((Engine::Postgres, 5432))
    } else if ["mysql", "mariadb", "percona"]
        .iter()
        .any(|s| last.contains(s))
    {
        Some((Engine::Mysql, 3306))
    } else {
        None
    }
}

fn published_port(ports: &[String], private: u16) -> Option<u16> {
    ports.iter().find_map(|entry| {
        let entry = entry.split('/').next().unwrap_or(entry);
        let (public, priv_) = match entry.split_once("->") {
            Some((left, right)) => (left.rsplit(':').next().unwrap_or(left), right),
            None => entry.split_once(':')?,
        };
        let public: u16 = public.parse().ok()?;
        let priv_: u16 = priv_.parse().ok()?;
        (priv_ == private).then_some(public)
    })
}

fn compose_name(root: &Path, env: &[EnvFile]) -> String {
    let from_env = env
        .iter()
        .find(|f| f.name == ".env")
        .and_then(|f| {
            parse_env(&f.text)
                .into_iter()
                .rev()
                .find(|(k, _)| k == "COMPOSE_PROJECT_NAME")
        })
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty() && !v.contains("${"));
    match from_env {
        Some(v) => normalize_project(&v),
        None => normalize_project(
            &root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    }
}

fn suggestion(
    root: &Path,
    conn: Connection,
    password: Option<Secret>,
    source: Source,
) -> Suggestion {
    Suggestion {
        key: target_key(&conn, root),
        conn,
        password,
        source,
    }
}

fn url_candidates(root: &Path, merged: &Merged, out: &mut Vec<Suggestion>) {
    let mut keys: Vec<&String> = merged
        .keys()
        .filter(|k| *k == "DATABASE_URL" || k.ends_with("_URL"))
        .collect();
    keys.sort_by(|a, b| (*a != "DATABASE_URL", a).cmp(&(*b != "DATABASE_URL", b)));
    for key in keys {
        let (value, file) = &merged[key];
        let Some((scheme, rest)) = value.trim().split_once(':') else {
            continue;
        };
        let scheme = scheme.to_lowercase();
        let scheme = scheme.split('+').next().unwrap_or(&scheme);
        if !matches!(
            scheme,
            "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite"
        ) {
            continue;
        }
        let Ok((mut conn, password)) = from_url(&format!("{scheme}:{rest}")) else {
            continue;
        };
        conn.id = String::new();
        conn.projects = vec![root.to_path_buf()];
        let source = Source::Env {
            file,
            key: key.clone(),
        };
        out.push(suggestion(root, conn, password, source));
    }
}

fn container_candidates(
    root: &Path,
    env: &[EnvFile],
    merged: &Merged,
    containers: &[ContainerInfo],
    out: &mut Vec<Suggestion>,
) {
    let get = |k: &str| merged.get(k).map(|(v, _)| v.clone());
    let compose = compose_name(root, env);
    for c in containers {
        if c.state != "running" {
            continue;
        }
        let Some((engine, private)) = engine_of_image(&c.image) else {
            continue;
        };
        let matches = c.working_dir.as_deref() == Some(root)
            || c.project.as_deref() == Some(compose.as_str());
        if !matches {
            continue;
        }
        let Some(public) = published_port(&c.ports, private) else {
            continue;
        };
        let name = format!("{} (docker)", c.service.as_deref().unwrap_or(&c.name));
        let (user, database, password) = if engine == Engine::Postgres {
            let user = get("POSTGRES_USER").unwrap_or_else(|| "postgres".into());
            let db = get("POSTGRES_DB").unwrap_or_else(|| user.clone());
            (user, db, get("POSTGRES_PASSWORD"))
        } else {
            let user = get("MYSQL_USER");
            let password = if user.is_some() {
                get("MYSQL_PASSWORD")
            } else {
                get("MYSQL_ROOT_PASSWORD")
            };
            (
                user.unwrap_or_else(|| "root".into()),
                get("MYSQL_DATABASE").unwrap_or_default(),
                password,
            )
        };
        let (conn, password) = build(
            root,
            Target {
                name,
                engine,
                host: "localhost".into(),
                port: Some(public),
                database,
                user,
                password,
            },
        );
        let source = Source::Container {
            name: c.name.clone(),
        };
        out.push(suggestion(root, conn, password, source));
    }
}

struct Group<'a> {
    keys: &'a [&'a str],
    engine: Engine,
    host: Option<String>,
    port: Option<String>,
    user: String,
    database: Option<String>,
    password: Option<String>,
}

fn group_candidate(root: &Path, merged: &Merged, g: Group) -> Option<Suggestion> {
    let (file, key) = g
        .keys
        .iter()
        .find_map(|k| merged.get(*k).map(|(_, f)| (*f, (*k).to_string())))?;
    let port = match g.port {
        Some(p) => Some(p.trim().parse::<u16>().ok()?),
        None => None,
    };
    let host = g
        .host
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into());
    let database = g.database.unwrap_or_else(|| g.user.clone());
    let (conn, password) = build(
        root,
        Target {
            name: format!("{database} on {host}"),
            engine: g.engine,
            host,
            port,
            database,
            user: g.user,
            password: g.password,
        },
    );
    Some(suggestion(root, conn, password, Source::Env { file, key }))
}

fn group_candidates(root: &Path, merged: &Merged, out: &mut Vec<Suggestion>) {
    let get = |k: &str| merged.get(k).map(|(v, _)| v.clone());
    let mysql_user = get("MYSQL_USER");
    let mysql_password = if mysql_user.is_some() {
        get("MYSQL_PASSWORD").or_else(|| get("MYSQL_PWD"))
    } else {
        get("MYSQL_ROOT_PASSWORD").or_else(|| get("MYSQL_PWD"))
    };
    let groups = [
        Group {
            keys: &["PGHOST", "PGDATABASE", "PGUSER"],
            engine: Engine::Postgres,
            host: get("PGHOST"),
            port: get("PGPORT"),
            user: get("PGUSER").unwrap_or_default(),
            database: get("PGDATABASE"),
            password: get("PGPASSWORD"),
        },
        Group {
            keys: &["POSTGRES_DB", "POSTGRES_USER", "POSTGRES_PASSWORD"],
            engine: Engine::Postgres,
            host: get("POSTGRES_HOST"),
            port: get("POSTGRES_PORT"),
            user: get("POSTGRES_USER").unwrap_or_else(|| "postgres".into()),
            database: get("POSTGRES_DB"),
            password: get("POSTGRES_PASSWORD"),
        },
        Group {
            keys: &[
                "MYSQL_HOST",
                "MYSQL_DATABASE",
                "MYSQL_USER",
                "MYSQL_ROOT_PASSWORD",
            ],
            engine: Engine::Mysql,
            host: get("MYSQL_HOST"),
            port: get("MYSQL_TCP_PORT").or_else(|| get("MYSQL_PORT")),
            user: mysql_user.unwrap_or_else(|| "root".into()),
            database: Some(get("MYSQL_DATABASE").unwrap_or_default()),
            password: mysql_password,
        },
    ];
    out.extend(
        groups
            .into_iter()
            .filter_map(|g| group_candidate(root, merged, g)),
    );
}

/// Suggests connections not already saved, one per target, in the order URL, container, variable group.
pub fn detect(
    root: &Path,
    env: &[EnvFile],
    containers: &[ContainerInfo],
    saved: &[Listed],
) -> Vec<Suggestion> {
    let merged = merge(env);
    let mut all = Vec::new();
    url_candidates(root, &merged, &mut all);
    container_candidates(root, env, &merged, containers, &mut all);
    group_candidates(root, &merged, &mut all);
    drop(merged);

    let mut seen: Vec<String> = saved.iter().map(|l| target_key(&l.conn, root)).collect();
    all.retain(|s| {
        if seen.contains(&s.key) {
            false
        } else {
            seen.push(s.key.clone());
            true
        }
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Origin;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("daedalus-detect-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ef(name: &'static str, text: &str) -> EnvFile {
        EnvFile {
            name,
            text: text.into(),
        }
    }

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse_env(text)
    }

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    fn root() -> PathBuf {
        PathBuf::from("/work/My App.v2")
    }

    fn run(env: &[EnvFile]) -> Vec<Suggestion> {
        detect(&root(), env, &[], &[])
    }

    fn container(image: &str, ports: &[&str]) -> ContainerInfo {
        ContainerInfo {
            name: "myappv2-db-1".into(),
            image: image.into(),
            state: "running".into(),
            ports: ports.iter().map(|s| s.to_string()).collect(),
            project: Some("myappv2".into()),
            service: Some("db".into()),
            working_dir: None,
        }
    }

    #[test]
    fn export_prefix_is_stripped() {
        assert_eq!(
            pairs("export A=1\nexportB=2\nexport\tC=3"),
            vec![kv("A", "1"), kv("exportB", "2"), kv("C", "3")]
        );
    }

    #[test]
    fn double_quotes_unescape() {
        assert_eq!(
            pairs(r#"A="x\ny\t\"q\" \\ \z" # c"#),
            vec![kv("A", "x\ny\t\"q\" \\ \\z")]
        );
    }

    #[test]
    fn single_and_backtick_quotes_are_literal() {
        assert_eq!(
            pairs("A='a\\n#b'\nB=`c\\td`"),
            vec![kv("A", "a\\n#b"), kv("B", "c\\td")]
        );
    }

    #[test]
    fn hash_starts_a_comment_only_after_space() {
        assert_eq!(
            pairs("a=b#c\nd=e #f\ng=#h"),
            vec![kv("a", "b#c"), kv("d", "e"), kv("g", "")]
        );
    }

    #[test]
    fn multiline_quoted_value() {
        assert_eq!(
            pairs("A=\"l1\nl2\"\nB=2"),
            vec![kv("A", "l1\nl2"), kv("B", "2")]
        );
    }

    #[test]
    fn unterminated_quote_skips_only_that_entry() {
        assert_eq!(pairs("A=\"oops\nB=2"), vec![kv("B", "2")]);
    }

    #[test]
    fn crlf_and_bom() {
        assert_eq!(
            pairs("\u{feff}A=1\r\nB=\"2\"\r\n"),
            vec![kv("A", "1"), kv("B", "2")]
        );
    }

    #[test]
    fn invalid_keys_and_lines_without_equals_are_skipped() {
        assert_eq!(
            pairs("=x\nno equals\nA B=1\n# c=1\nOK.k-1=v"),
            vec![kv("OK.k-1", "v")]
        );
    }

    #[test]
    fn env_url_parsed_without_echoing_password() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgres://me:s3cret@db.example.com:5433/app\n",
        )]);
        assert_eq!(s.len(), 1);
        assert!(s[0].password.is_some());
        assert_eq!(s[0].conn.id, "");
        assert_eq!(s[0].conn.projects, vec![root()]);
        assert!(!format!("{s:?}").contains("s3cret"));
    }

    #[test]
    fn interpolated_values_skipped() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgres://${USER}@h/db\nPOSTGRES_USER=me\nPOSTGRES_PASSWORD=${PW}\n",
        )]);
        assert_eq!(s.len(), 1);
        assert!(matches!(&s[0].source, Source::Env { key, .. } if key == "POSTGRES_USER"));
        assert!(s[0].password.is_none());
    }

    #[test]
    fn local_file_overrides_base_file() {
        let env = [
            ef(".env.local", "DATABASE_URL=postgres://u@local/db\n"),
            ef(".env", "DATABASE_URL=postgres://u@base/db\n"),
        ];
        let s = run(&env);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].conn.host, "local");
        assert!(matches!(
            &s[0].source,
            Source::Env {
                file: ".env.local",
                ..
            }
        ));

        let env = [
            ef(
                ".env.development.local",
                "DATABASE_URL=postgres://u@devlocal/db\n",
            ),
            ef(".env.local", "DATABASE_URL=postgres://u@local/db\n"),
            ef(".env", "DATABASE_URL=postgres://u@base/db\n"),
        ];
        assert_eq!(run(&env)[0].conn.host, "devlocal");
    }

    #[test]
    fn driver_suffix_in_scheme_is_dropped() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgresql+asyncpg://u:p@h/db\nX_URL=mysql+pymysql://u@m/d\n",
        )]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].conn.engine, Engine::Postgres);
        assert_eq!(s[1].conn.engine, Engine::Mysql);
    }

    #[test]
    fn non_database_urls_are_ignored() {
        let s = run(&[ef(
            ".env",
            "REDIS_URL=redis://localhost:6379\nAPI_URL=https://example.com\n",
        )]);
        assert!(s.is_empty());
    }

    #[test]
    fn bad_url_is_skipped_silently() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgres://u:p@h:notaport/db\nOTHER_URL=postgres://\n",
        )]);
        assert!(s.is_empty());
    }

    #[test]
    fn compose_container_matches_project_folder() {
        assert_eq!(normalize_project("My App.v2"), "myappv2");
        assert_eq!(normalize_project("_-Foo_bar-"), "foo_bar-");
        let s = detect(
            &root(),
            &[],
            &[container("postgres:16", &["15432:5432"])],
            &[],
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].conn.port, Some(15432));
        assert_eq!(s[0].conn.name, "db (docker)");
        assert!(matches!(&s[0].source, Source::Container { name } if name == "myappv2-db-1"));
    }

    #[test]
    fn compose_project_name_from_env_wins() {
        let mut c = container("postgres:16", &["15432:5432"]);
        c.project = Some("custom".into());
        let hit = detect(
            &root(),
            &[ef(".env", "COMPOSE_PROJECT_NAME=Custom\n")],
            std::slice::from_ref(&c),
            &[],
        );
        assert_eq!(hit.len(), 1);
        assert!(detect(&root(), &[], std::slice::from_ref(&c), &[]).is_empty());
        let other = detect(
            &root(),
            &[ef(".env.local", "COMPOSE_PROJECT_NAME=custom\n")],
            &[c],
            &[],
        );
        assert!(other.is_empty());
    }

    #[test]
    fn working_dir_match_beats_name() {
        let mut c = container("postgres:16", &["15432:5432"]);
        c.project = Some("elsewhere".into());
        c.working_dir = Some(root());
        assert_eq!(detect(&root(), &[], &[c], &[]).len(), 1);
    }

    #[test]
    fn stopped_or_unrelated_containers_are_skipped() {
        let mut stopped = container("postgres:16", &["15432:5432"]);
        stopped.state = "exited".into();
        let list = [
            stopped,
            container("prom/postgres-exporter:v0.15", &["9187:9187"]),
            container("redis:7", &["6379:6379"]),
            container("postgres:16", &[]),
        ];
        assert!(detect(&root(), &[], &list, &[]).is_empty());
    }

    #[test]
    fn published_port_both_formats() {
        assert_eq!(published_port(&["15432:5432".into()], 5432), Some(15432));
        assert_eq!(
            published_port(&["0.0.0.0:15432->5432/tcp".into()], 5432),
            Some(15432)
        );
        assert_eq!(
            published_port(&["8080:80".into(), "13306:3306".into()], 3306),
            Some(13306)
        );
        assert_eq!(published_port(&["8080:80".into()], 5432), None);
    }

    #[test]
    fn container_uses_postgres_env_credentials() {
        let env = [ef(
            ".env",
            "POSTGRES_USER=app\nPOSTGRES_PASSWORD=pw\nPOSTGRES_DB=appdb\n",
        )];
        let s = detect(
            &root(),
            &env,
            &[container("postgis/postgis:16", &["5433:5432"])],
            &[],
        );
        assert_eq!(s[0].conn.user, "app");
        assert_eq!(s[0].conn.database, "appdb");
        assert!(s[0].password.is_some());
        let plain = detect(
            &root(),
            &[],
            &[container("postgres:16", &["5433:5432"])],
            &[],
        );
        assert_eq!(
            (plain[0].conn.user.as_str(), plain[0].conn.database.as_str()),
            ("postgres", "postgres")
        );
    }

    #[test]
    fn pg_vars_group() {
        let s = run(&[ef(
            ".env",
            "PGHOST=db.example.com\nPGPORT=5433\nPGUSER=me\nPGDATABASE=app\nPGPASSWORD=pw\n",
        )]);
        assert_eq!(s.len(), 1);
        let c = &s[0].conn;
        assert_eq!(
            (
                c.host.as_str(),
                c.port,
                c.user.as_str(),
                c.database.as_str()
            ),
            ("db.example.com", Some(5433), "me", "app")
        );
        assert_eq!(c.tls, Tls::Require);
        assert!(s[0].password.is_some());
        assert!(matches!(&s[0].source, Source::Env { key, .. } if key == "PGHOST"));
    }

    #[test]
    fn postgres_docker_vars_default_user() {
        let s = run(&[ef(".env", "POSTGRES_DB=shop\n")]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].conn.user, "postgres");
        assert_eq!(s[0].conn.database, "shop");
        assert_eq!(s[0].conn.host, "localhost");
        assert_eq!(s[0].conn.name, "shop on localhost");
    }

    #[test]
    fn mysql_root_password_when_no_user() {
        let s = run(&[ef(
            ".env",
            "MYSQL_ROOT_PASSWORD=rootpw\nMYSQL_PASSWORD=other\nMYSQL_DATABASE=d\n",
        )]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].conn.engine, Engine::Mysql);
        assert_eq!(s[0].conn.user, "root");
        assert!(
            s[0].password
                .as_ref()
                .is_some_and(|p| p.expose() == "rootpw")
        );
    }

    #[test]
    fn bad_port_skips_group() {
        assert!(run(&[ef(".env", "PGHOST=h\nPGPORT=abc\n")]).is_empty());
    }

    #[test]
    fn saved_connection_hides_suggestion() {
        let env = [ef(".env", "DATABASE_URL=postgres://me@127.0.0.1/app\n")];
        let mut conn = run(&env)[0].conn.clone();
        conn.host = "localhost".into();
        conn.port = Some(5432);
        let saved = [Listed {
            conn,
            origin: Origin::Global,
        }];
        assert!(detect(&root(), &env, &[], &saved).is_empty());
    }

    #[test]
    fn same_target_from_two_sources_listed_once() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgres://me@localhost:5432/app\nPOSTGRES_USER=me\nPOSTGRES_DB=app\n",
        )]);
        assert_eq!(s.len(), 1);
        assert!(matches!(&s[0].source, Source::Env { key, .. } if key == "DATABASE_URL"));
    }

    #[test]
    fn different_user_is_not_a_duplicate() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgres://a@localhost/app\nOTHER_URL=postgres://b@localhost/app\n",
        )]);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn read_env_reads_only_known_files_in_order() {
        let dir = temp("read");
        for n in [".env", ".env.local", ".env.example"] {
            fs::write(dir.join(n), n).unwrap();
        }
        let names: Vec<_> = read_env(&dir).iter().map(|f| f.name).collect();
        assert_eq!(names, vec![".env.local", ".env"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversize_env_file_is_skipped() {
        let dir = temp("big");
        fs::write(dir.join(".env"), vec![b'a'; MAX_ENV_BYTES as usize + 1]).unwrap();
        assert!(read_env(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interpolated_local_value_hides_lower_file() {
        let s = run(&[
            ef(".env.local", "DATABASE_URL=postgres://${USER}@new/db\n"),
            ef(".env", "DATABASE_URL=postgres://u@stale/db\n"),
        ]);
        assert!(s.is_empty());
    }

    #[test]
    fn interpolated_value_hides_earlier_line_in_same_file() {
        let s = run(&[ef(
            ".env",
            "DATABASE_URL=postgres://u@stale/db\nDATABASE_URL=${X}\n",
        )]);
        assert!(s.is_empty());
    }

    #[test]
    fn blank_postgres_user_uses_defaults() {
        let s = run(&[ef(
            ".env",
            "POSTGRES_DB=shop\nPOSTGRES_USER=\nPOSTGRES_PASSWORD=\n",
        )]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].conn.user, "postgres");
        assert!(s[0].password.is_none());
    }

    #[test]
    fn blank_pgport_uses_default_port() {
        let s = run(&[ef(".env", "PGHOST=h\nPGUSER=me\nPGPORT=\n")]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].conn.port, None);
    }

    #[test]
    fn blank_compose_project_name_falls_back_to_folder() {
        let mut c = container("postgres:16", &["15432:5432"]);
        c.project = Some(normalize_project("My App.v2"));
        let hit = detect(&root(), &[ef(".env", "COMPOSE_PROJECT_NAME=\n")], &[c], &[]);
        assert_eq!(hit.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn env_symlink_must_stay_inside_project() {
        let dir = temp("symlink");
        let outside = temp("symlink-out");
        fs::write(dir.join("real.txt"), "A=1").unwrap();
        fs::write(outside.join("secret"), "B=2").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join(".env")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join(".env.local")).unwrap();
        let names: Vec<_> = read_env(&dir).iter().map(|f| f.name).collect();
        assert_eq!(names, vec![".env"]);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }
}
