//! Splits a SQL script into statements and classifies each one for safety prompts.

use std::ops::Range;

use tree_sitter::{Node, Parser};

use crate::config::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    Ddl,
    Transaction,
    Session,
    /// Unparsed or unrecognised; callers must treat it as a write.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Byte range from the first token through the terminating `;`, if any.
    pub range: Range<usize>,
    pub kind: Kind,
    /// An `UPDATE` or `DELETE` with no `WHERE`.
    pub unguarded: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tok {
    Word,
    Semi,
    Punct(u8),
    Literal,
    /// A comment the server executes (MySQL `/*! */`), so the chunk cannot be trusted.
    Hazard,
}

type Toks = [(Tok, Range<usize>)];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Length of a `$tag$` opener at the start of `s`, if it is one.
fn dollar_tag_len(s: &[u8]) -> Option<usize> {
    if s.first() != Some(&b'$') {
        return None;
    }
    let mut i = 1;
    if s.get(1)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        while s.get(i).is_some_and(|b| is_word_byte(*b)) {
            i += 1;
        }
    }
    (s.get(i) == Some(&b'$')).then_some(i + 1)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Tokens outside comments; quoted text and dollar-quoted bodies collapse into one `Literal`.
fn scan(sql: &str, engine: Engine) -> Vec<(Tok, Range<usize>)> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        let next = b.get(i + 1).copied();
        if c.is_ascii_whitespace() {
            i += 1;
        } else if (c == b'-' && next == Some(b'-'))
            && (engine != Engine::Mysql
                || b.get(i + 2)
                    .is_none_or(|x| x.is_ascii_whitespace() || x.is_ascii_control()))
            || (c == b'#' && engine == Engine::Mysql)
        {
            i = b[i..]
                .iter()
                .position(|x| *x == b'\n')
                .map_or(b.len(), |p| i + p);
            // Postgres ends the comment at a bare \r, so the scanner would hide what follows.
            if b[start..i]
                .iter()
                .enumerate()
                .any(|(k, x)| *x == b'\r' && b.get(start + k + 1) != Some(&b'\n'))
            {
                out.push((Tok::Hazard, start..start + 2));
            }
        } else if c == b'/' && next == Some(b'*') {
            i += 2;
            if engine == Engine::Mysql
                && (b.get(i) == Some(&b'!')
                    || (b.get(i) == Some(&b'M') && b.get(i + 1) == Some(&b'!')))
            {
                out.push((Tok::Hazard, start..start + 2));
            }
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if engine == Engine::Postgres && b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if c == b'\'' || c == b'"' || c == b'`' {
            let escapes = match engine {
                Engine::Mysql => c != b'`',
                Engine::Postgres => {
                    c == b'\''
                        && i > 0
                        && b[i - 1] | 0x20 == b'e'
                        && (i == 1 || !(is_word_byte(b[i - 2]) || b[i - 2] == b'$'))
                }
                Engine::Sqlite => false,
            };
            // U&"..\XXXX" decodes to a name the lexer never sees, and its quotes can hide.
            if engine == Engine::Postgres
                && (c == b'"' || c == b'\'')
                && i >= 2
                && b[i - 1] == b'&'
                && b[i - 2] | 0x20 == b'u'
                && (i == 2 || !(is_word_byte(b[i - 3]) || b[i - 3] == b'$'))
            {
                out.push((Tok::Hazard, start..start + 1));
            }
            i += 1;
            while i < b.len() {
                if escapes && b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            // Backslash-quote splits differently under NO_BACKSLASH_ESCAPES, ANSI_QUOTES and
            // standard_conforming_strings, so the rest of the script cannot be trusted.
            let backslashes = match engine {
                Engine::Mysql => c != b'`',
                Engine::Postgres => c == b'\'',
                Engine::Sqlite => false,
            };
            if backslashes
                && b[start + 1..i]
                    .windows(2)
                    .any(|w| w[0] == b'\\' && matches!(w[1], b'\'' | b'"' | b'`'))
            {
                out.push((Tok::Hazard, start..i));
            }
            out.push((Tok::Literal, start..i));
        } else if c == b'[' && engine == Engine::Sqlite {
            i = b[i..]
                .iter()
                .position(|x| *x == b']')
                .map_or(b.len(), |p| i + p + 1);
            out.push((Tok::Literal, start..i));
        } else if let Some(n) = dollar_tag_len(&b[i..])
            .filter(|_| engine == Engine::Postgres && (i == 0 || !is_word_byte(b[i - 1])))
        {
            let tag = &b[i..i + n];
            i = find(&b[i + n..], tag).map_or(b.len(), |p| i + n + p + n);
            out.push((Tok::Literal, start..i));
        } else if is_word_byte(c) {
            // `$` continues an identifier, so `a$$` never opens a dollar quote.
            while i < b.len() && (is_word_byte(b[i]) || b[i] == b'$') {
                i += 1;
            }
            out.push((Tok::Word, start..i));
        } else {
            i += 1;
            out.push((if c == b';' { Tok::Semi } else { Tok::Punct(c) }, start..i));
        }
    }
    out
}

/// Tracks whether a `;` falls inside a trigger, procedure, function or event body.
#[derive(Default)]
struct Body {
    words: usize,
    create: bool,
    header: bool,
    after_end: bool,
    after_begin: bool,
    seen: bool,
    depth: i32,
}

impl Body {
    fn feed(&mut self, tok: Tok, w: &str) {
        if tok != Tok::Word {
            self.after_end = false;
            self.after_begin = false;
            return;
        }
        let w = w.to_ascii_lowercase();
        self.words += 1;
        if self.words == 1 {
            self.create = w == "create";
        }
        if self.create
            && self.words <= 12
            && matches!(w.as_str(), "trigger" | "procedure" | "function" | "event")
        {
            self.header = true;
        }
        if self.after_begin && w == "atomic" {
            self.seen = true;
        }
        let was_end = self.after_end;
        self.after_end = false;
        self.after_begin = false;
        match w.as_str() {
            "if" | "loop" | "while" | "repeat" if was_end => self.depth += 1,
            "case" if was_end => {}
            "begin" => {
                self.depth += 1;
                self.after_begin = true;
                self.seen |= self.header;
            }
            "case" => self.depth += 1,
            "end" => {
                self.depth -= 1;
                self.after_end = true;
            }
            _ => {}
        }
    }

    fn open(&self) -> bool {
        self.seen && self.depth > 0
    }
}

/// Splits `sql` on top-level semicolons and classifies each statement.
pub fn split(sql: &str, engine: Engine) -> Vec<Statement> {
    let toks = scan(sql, engine);
    let mut parser = Parser::new();
    let ready = parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .is_ok();
    let mut out = Vec::new();
    let mut first = 0;
    let mut body = Body::default();
    let mut tainted = false;
    for (n, (tok, span)) in toks.iter().enumerate() {
        tainted |= *tok == Tok::Hazard;
        body.feed(*tok, &sql[span.clone()]);
        let last = n + 1 == toks.len();
        if (*tok != Tok::Semi || body.open()) && !last {
            continue;
        }
        let chunk = &toks[first..=n];
        first = n + 1;
        let in_body = body.seen;
        body = Body::default();
        if chunk.iter().all(|(t, _)| *t == Tok::Semi) {
            continue;
        }
        let range = chunk[0].1.start..span.end;
        let (kind, unguarded) = if in_body || tainted {
            (Kind::Unknown, false)
        } else {
            classify(sql, chunk, engine, ready.then_some(&mut parser))
        };
        out.push(Statement {
            range,
            kind,
            unguarded,
        });
    }
    out
}

/// The statement containing `offset`, or the nearest one before it.
pub fn at(sql: &str, offset: usize, engine: Engine) -> Option<Statement> {
    let all = split(sql, engine);
    let idx = all
        .iter()
        .rposition(|s| s.range.start <= offset)
        .unwrap_or(0);
    all.into_iter().nth(idx)
}

fn word<'a>(sql: &'a str, tok: &(Tok, Range<usize>)) -> Option<&'a str> {
    (tok.0 == Tok::Word).then(|| &sql[tok.1.clone()])
}

fn words<'a>(sql: &'a str, toks: &'a Toks) -> impl Iterator<Item = String> + 'a {
    toks.iter()
        .filter_map(|t| word(sql, t))
        .map(str::to_ascii_lowercase)
}

fn has_read_write(ws: &[String]) -> bool {
    ws.windows(2).any(|w| w[0] == "read" && w[1] == "write")
}

const PRAGMA_ARG: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

const PRAGMA_BARE: &[&str] = &[
    "database_list",
    "collation_list",
    "function_list",
    "module_list",
    "pragma_list",
    "compile_options",
    "page_count",
    "freelist_count",
    "page_size",
    "user_version",
    "application_id",
    "schema_version",
    "encoding",
    "journal_mode",
    "foreign_keys",
];

fn pragma_kind(sql: &str, toks: &Toks) -> Kind {
    let mut rest = &toks[1..];
    while let Some(((Tok::Semi, _), head)) = rest.split_last() {
        rest = head;
    }
    let name_at = |i: usize| rest.get(i).and_then(|t| word(sql, t));
    let mut at = 0;
    if rest.get(1).is_some_and(|t| t.0 == Tok::Punct(b'.')) && name_at(0).is_some() {
        at = 2;
    }
    let Some(name) = name_at(at).map(str::to_ascii_lowercase) else {
        return Kind::Unknown;
    };
    let args = &rest[at + 1..];
    if args.is_empty()
        && (PRAGMA_ARG.contains(&name.as_str()) || PRAGMA_BARE.contains(&name.as_str()))
    {
        return Kind::Read;
    }
    let call = args.first().is_some_and(|t| t.0 == Tok::Punct(b'('))
        && args.last().is_some_and(|t| t.0 == Tok::Punct(b')'))
        && !args
            .iter()
            .any(|t| t.0 == Tok::Punct(b'=') || t.0 == Tok::Semi);
    if call && PRAGMA_ARG.contains(&name.as_str()) {
        Kind::Read
    } else {
        Kind::Unknown
    }
}

const PG_TXN: &[&str] = &[
    "work",
    "transaction",
    "isolation",
    "level",
    "read",
    "only",
    "committed",
    "uncommitted",
    "repeatable",
    "serializable",
    "deferrable",
];

const MY_START: &[&str] = &["with", "consistent", "snapshot", "read", "only"];

const LITE_BEGIN: &[&str] = &["deferred", "immediate", "exclusive", "transaction"];

/// Transaction only when every word after BEGIN / START TRANSACTION is on the engine's allowlist.
fn txn_start_kind(toks: &Toks, ws: &[String], engine: Engine) -> Kind {
    let is_start = ws[0] == "start";
    let skip = if is_start { 2 } else { 1 };
    if is_start && (engine == Engine::Sqlite || ws.get(1).is_none_or(|w| w != "transaction")) {
        return Kind::Unknown;
    }
    let commas = engine != Engine::Sqlite;
    let plain = toks.iter().enumerate().all(|(n, t)| match t.0 {
        Tok::Word => true,
        Tok::Punct(b',') => commas,
        Tok::Semi => n + 1 == toks.len(),
        _ => false,
    });
    if !plain {
        return Kind::Unknown;
    }
    let rest: Vec<&str> = ws.iter().skip(skip).map(String::as_str).collect();
    let ok = match engine {
        Engine::Postgres => rest.iter().enumerate().all(|(n, w)| {
            PG_TXN.contains(w) || (*w == "not" && rest.get(n + 1) == Some(&"deferrable"))
        }),
        Engine::Mysql if is_start => rest.iter().all(|w| MY_START.contains(w)),
        Engine::Mysql => rest.is_empty() || rest == ["work"],
        Engine::Sqlite => rest.iter().all(|w| LITE_BEGIN.contains(w)),
    };
    if ok { Kind::Transaction } else { Kind::Unknown }
}

const PG_SET: &[&str] = &[
    "search_path",
    "timezone",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "application_name",
    "client_encoding",
    "datestyle",
    "intervalstyle",
    "extra_float_digits",
    "work_mem",
    "client_min_messages",
    "bytea_output",
];

const MY_SET: &[&str] = &[
    "time_zone",
    "max_execution_time",
    "sql_select_limit",
    "lc_time_names",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
];

/// Session only for the allowlisted `SET name = value` shapes; everything else is Unknown.
fn set_kind(sql: &str, toks: &Toks, engine: Engine) -> Kind {
    let mut rest = &toks[1..];
    while let Some(((Tok::Semi, _), head)) = rest.split_last() {
        rest = head;
    }
    let names = match engine {
        Engine::Postgres => PG_SET,
        Engine::Mysql => MY_SET,
        Engine::Sqlite => return Kind::Unknown,
    };
    let lower = |t: &(Tok, Range<usize>)| word(sql, t).map(str::to_ascii_lowercase);
    if rest
        .first()
        .and_then(lower)
        .is_some_and(|w| w == "session" || w == "local")
    {
        rest = &rest[1..];
    }
    let w0 = rest.first().and_then(lower);
    let w1 = rest.get(1).and_then(lower);
    let (value, commas) = match (engine, w0.as_deref(), w1.as_deref()) {
        (Engine::Postgres, Some("time"), Some("zone")) => (&rest[2..], false),
        (_, Some("names"), _) => (&rest[1..], false),
        (Engine::Mysql, Some("character"), Some("set")) => (&rest[2..], false),
        (Engine::Mysql, Some("charset"), _) => (&rest[1..], false),
        (_, Some(name), _)
            if names.contains(&name)
                && (w1.as_deref() == Some("to")
                    || rest.get(1).is_some_and(|t| t.0 == Tok::Punct(b'='))) =>
        {
            (&rest[2..], matches!(name, "search_path" | "datestyle"))
        }
        _ => return Kind::Unknown,
    };
    let plain = !value.is_empty()
        && value.iter().all(|t| match t.0 {
            Tok::Word | Tok::Literal => true,
            Tok::Punct(c) => matches!(c, b'-' | b'+' | b'.') || (c == b',' && commas),
            _ => false,
        });
    if plain { Kind::Session } else { Kind::Unknown }
}

fn classify(sql: &str, toks: &Toks, engine: Engine, parser: Option<&mut Parser>) -> (Kind, bool) {
    if toks.iter().any(|t| t.0 == Tok::Hazard) {
        return (Kind::Unknown, false);
    }
    let mut toks = toks;
    let mut analyzed = false;
    // EXPLAIN ANALYZE runs its statement; plain EXPLAIN only plans it.
    while let Some(w) = toks.first().and_then(|t| word(sql, t)) {
        let desc = engine == Engine::Mysql
            && (w.eq_ignore_ascii_case("desc") || w.eq_ignore_ascii_case("describe"));
        if !(w.eq_ignore_ascii_case("explain") || desc) {
            break;
        }
        match toks.get(1) {
            Some((Tok::Punct(b'('), _)) => return (Kind::Unknown, false),
            Some(t)
                if word(sql, t).is_some_and(|w| {
                    w.eq_ignore_ascii_case("analyze") || w.eq_ignore_ascii_case("analyse")
                }) =>
            {
                toks = &toks[2..];
                analyzed = true;
            }
            _ => return (Kind::Read, false),
        }
    }
    let (kind, unguarded) = classify_one(sql, toks, engine, parser);
    if !analyzed {
        return (kind, unguarded);
    }
    match kind {
        Kind::Read | Kind::Write => (kind, unguarded),
        _ => (Kind::Unknown, false),
    }
}

fn classify_one(
    sql: &str,
    toks: &Toks,
    engine: Engine,
    parser: Option<&mut Parser>,
) -> (Kind, bool) {
    let (Some(first), Some(last)) = (toks.first(), toks.last()) else {
        return (Kind::Unknown, false);
    };
    let text = &sql[first.1.start..last.1.end];
    let ws: Vec<String> = words(sql, toks).collect();
    let head = toks
        .first()
        .and_then(|t| word(sql, t))
        .map(str::to_ascii_lowercase);
    let unknown = (Kind::Unknown, false);
    match head.as_deref() {
        Some("show" | "describe" | "desc") => return (Kind::Read, false),
        Some("pragma") => return (pragma_kind(sql, toks), false),
        Some("begin" | "start") => return (txn_start_kind(toks, &ws, engine), false),
        Some("commit" | "end" | "rollback" | "savepoint" | "release") => {
            return if has_read_write(&ws) {
                unknown
            } else {
                (Kind::Transaction, false)
            };
        }
        Some("truncate") => return (Kind::Ddl, false),
        Some("reset") => return unknown,
        Some("set") => return (set_kind(sql, toks, engine), false),
        _ => {}
    }
    if engine == Engine::Mysql
        && ws
            .windows(2)
            .any(|w| w[0] == "into" && (w[1] == "outfile" || w[1] == "dumpfile"))
    {
        return (Kind::Write, false);
    }
    let Some(tree) = parser.and_then(|p| p.parse(text, None)) else {
        return unknown;
    };
    let root = tree.root_node();
    if root.has_error() {
        return unknown;
    }
    let kind = kind_of(root, text, engine);
    (kind, kind == Kind::Write && has_unguarded(root))
}

// A SELECT calling these changes state or runs arbitrary SQL, so it must not pass as a plain read.
const PG_SIDE_EFFECT_FNS: &[&str] = &[
    "set_config",
    "nextval",
    "setval",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_switch_wal",
    "pg_notify",
    "pg_promote",
    "pg_import_system_collations",
    "pg_create_restore_point",
    "query_to_xml",
    "query_to_xmlschema",
    "query_to_xml_and_xmlschema",
    "cursor_to_xml",
    "pg_sleep",
    "pg_sleep_for",
    "pg_sleep_until",
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_stat_file",
];

// Locks and sleeps are not data changes but are not harmless reads for an automated caller.
const MY_SIDE_EFFECT_FNS: &[&str] = &[
    "get_lock",
    "release_lock",
    "release_all_locks",
    "sleep",
    "benchmark",
];

// These load native code or touch the filesystem.
const LITE_SIDE_EFFECT_FNS: &[&str] =
    &["load_extension", "readfile", "writefile", "fts3_tokenizer"];

fn is_side_effect_fn(name: &str, engine: Engine) -> bool {
    // Anything beyond plain identifier bytes may be an escape the name check cannot decode.
    if name
        .bytes()
        .any(|c| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$'))
    {
        return true;
    }
    let n = name.to_ascii_lowercase();
    match engine {
        Engine::Postgres => {
            PG_SIDE_EFFECT_FNS.contains(&n.as_str())
                || n.starts_with("dblink")
                || n.starts_with("lo_")
                || n.starts_with("pg_advisory")
                || n.starts_with("pg_try_advisory")
                || n.starts_with("pg_logical_emit")
                || n.starts_with("pg_file_")
                || n.starts_with("pg_wal_replay_")
                || n.starts_with("pg_backup_")
                || n.starts_with("pg_ls_")
                || n.starts_with("pg_stat_reset")
                || n.starts_with("pg_log_")
                || n.starts_with("pg_replication_origin_")
                || (n.starts_with("pg_") && n.contains("replication_slot"))
        }
        Engine::Mysql => MY_SIDE_EFFECT_FNS.contains(&n.as_str()),
        Engine::Sqlite => LITE_SIDE_EFFECT_FNS.contains(&n.as_str()),
    }
}

fn calls_side_effect_fn(root: Node, src: &str, engine: Engine) -> bool {
    any_node(root, |n| {
        if n.kind() != "invocation" {
            return false;
        }
        let name = n
            .named_child(0)
            .and_then(|r| r.child_by_field_name("name"))
            .and_then(|id| src.get(id.byte_range()));
        // An unreadable name cannot be vouched for.
        name.is_none_or(|nm| {
            is_side_effect_fn(
                nm.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']')),
                engine,
            )
        })
    })
}

fn kind_of(root: Node, src: &str, engine: Engine) -> Kind {
    match statement_kind(root) {
        // INTO may sit anywhere in a set operation, and a call can hide in any subtree.
        Kind::Read if any_node(root, |n| n.kind() == "keyword_into") => Kind::Write,
        Kind::Read if calls_side_effect_fn(root, src, engine) => Kind::Unknown,
        k => k,
    }
}

fn statement_kind(root: Node) -> Kind {
    let mut cur = root.walk();
    if root
        .named_children(&mut cur)
        .filter(|n| n.kind() == "statement")
        .count()
        > 1
    {
        return Kind::Unknown;
    }
    let Some(mut node) = root.named_children(&mut cur).next() else {
        return Kind::Unknown;
    };
    if node.kind() == "statement" {
        let mut c = node.walk();
        let mut kids = node.named_children(&mut c);
        let mut found = None;
        let mut write = false;
        for k in kids.by_ref() {
            match k.kind() {
                "insert" | "update" | "delete" | "keyword_into" => write = true,
                "cte" if contains_write(k) => write = true,
                _ if found.is_none() && !k.kind().starts_with("keyword_") => found = Some(k),
                _ => {}
            }
        }
        if write {
            return Kind::Write;
        }
        match found {
            Some(k) => node = k,
            None => return Kind::Unknown,
        }
    }
    let k = node.kind();
    match k {
        "select" | "cte" | "set_operation" => Kind::Read,
        "transaction" => Kind::Transaction,
        "set_statement" => Kind::Session,
        _ if k.starts_with("create_") || k.starts_with("alter_") || k.starts_with("drop_") => {
            Kind::Ddl
        }
        _ => Kind::Unknown,
    }
}

/// Pre-order walk of `root`'s subtree without recursion, so deep nesting cannot overflow the stack.
fn any_node(root: Node, mut pred: impl FnMut(Node) -> bool) -> bool {
    let mut c = root.walk();
    loop {
        if pred(c.node()) {
            return true;
        }
        if c.goto_first_child() {
            continue;
        }
        while !c.goto_next_sibling() {
            if !c.goto_parent() {
                return false;
            }
        }
    }
}

fn contains_write(node: Node) -> bool {
    any_node(node, |n| matches!(n.kind(), "insert" | "update" | "delete"))
}

fn has_unguarded(root: Node) -> bool {
    any_node(root, |node| {
        if !matches!(node.kind(), "update" | "delete") {
            return false;
        }
        let has_where = |n: Node| {
            let mut c = n.walk();
            n.named_children(&mut c).any(|k| k.kind() == "where")
        };
        // The grammar nests WHERE in a FROM node: a sibling for DELETE, a child for UPDATE.
        let mut c = node.walk();
        let mut guarded = has_where(node)
            || node
                .named_children(&mut c)
                .any(|k| k.kind() == "from" && has_where(k));
        if let Some(from) = node.next_named_sibling().filter(|n| n.kind() == "from") {
            guarded |= has_where(from);
        }
        !guarded
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Engine = Engine::Postgres;
    const MY: Engine = Engine::Mysql;
    const LITE: Engine = Engine::Sqlite;

    fn one(sql: &str) -> Statement {
        let all = split(sql, PG);
        assert_eq!(all.len(), 1, "{sql}: {all:?}");
        all.into_iter().next().unwrap()
    }

    fn kind(sql: &str) -> Kind {
        one(sql).kind
    }

    #[test]
    fn select_is_read() {
        assert_eq!(kind("select 1"), Kind::Read);
        assert_eq!(kind("with x as (select 1) select * from x;"), Kind::Read);
        assert_eq!(kind("select 1 union select 2"), Kind::Read);
    }

    #[test]
    fn writes_ddl_session_transaction() {
        assert_eq!(kind("insert into t values (1)"), Kind::Write);
        assert_eq!(kind("update t set a = 1 where b = 2"), Kind::Write);
        assert_eq!(kind("create table a(b int)"), Kind::Ddl);
        assert_eq!(kind("alter table a add c int"), Kind::Ddl);
        assert_eq!(kind("drop table a"), Kind::Ddl);
        assert_eq!(kind("truncate t"), Kind::Ddl);
        assert_eq!(kind("set search_path = public"), Kind::Session);
        for s in ["begin", "commit;", "ROLLBACK", "start transaction"] {
            assert_eq!(kind(s), Kind::Transaction, "{s}");
        }
    }

    #[test]
    fn cte_with_delete_is_write() {
        let s = one("with x as (delete from t where a = 1 returning *) select * from x");
        assert_eq!(s.kind, Kind::Write);
        assert!(!s.unguarded);
    }

    #[test]
    fn delete_without_where_is_unguarded() {
        assert!(one("delete from t").unguarded);
        assert!(one("update t set a = 1;").unguarded);
        assert!(one("with x as (delete from t returning *) select * from x").unguarded);
    }

    #[test]
    fn update_with_where_is_guarded() {
        assert!(!one("update t set a = 1 where b = 2").unguarded);
        assert!(!one("delete from t where a in (select 1)").unguarded);
        assert!(!one("select 1").unguarded);
    }

    #[test]
    fn script_splits_on_semicolons_not_in_strings() {
        let sql = "select 'a;b'; select \"x;y\" from t; select 1 -- c; d\n; /* e; f */ select 2;";
        let all = split(sql, PG);
        assert_eq!(all.len(), 4, "{all:?}");
        assert_eq!(&sql[all[0].range.clone()], "select 'a;b';");
        assert_eq!(&sql[all[1].range.clone()], "select \"x;y\" from t;");
        assert!(sql[all[2].range.clone()].starts_with("select 1 -- c; d"));
        assert_eq!(&sql[all[3].range.clone()], "select 2;");
    }

    #[test]
    fn doubled_quotes_stay_inside_the_string() {
        assert_eq!(split("select 'it''s; fine'; select 2", PG).len(), 2);
    }

    #[test]
    fn dollar_quoted_bodies_do_not_split() {
        let a =
            "create function f() returns int as $$ select 1; select 2; $$ language sql; select 3";
        assert_eq!(split(a, PG).len(), 2);
        let b = "do $fn$ begin perform 1; end $fn$; select 2";
        let all = split(b, PG);
        assert_eq!(all.len(), 2);
        assert_eq!(
            &b[all[0].range.clone()],
            "do $fn$ begin perform 1; end $fn$;"
        );
        assert_eq!(split("select $1; select 2", PG).len(), 2);
    }

    #[test]
    fn comment_only_chunks_are_skipped() {
        assert!(split("-- nothing\n/* x */ ;;", PG).is_empty());
        assert!(split("", PG).is_empty());
    }

    #[test]
    fn parse_error_is_unknown() {
        assert_eq!(kind("select * from t where ("), Kind::Unknown);
        assert_eq!(kind("vacuum"), Kind::Unknown);
        assert_eq!(kind("grant all on t to u"), Kind::Unknown);
        assert_eq!(kind("explain analyze delete from t"), Kind::Write);
        assert_eq!(kind("pragma journal_mode = wal"), Kind::Unknown);
    }

    #[test]
    fn keyword_fallback_is_read() {
        for s in [
            "show tables",
            "SHOW search_path;",
            "explain select 1",
            "explain delete from t",
            "describe users",
            "desc users",
            "pragma table_info(t)",
        ] {
            assert_eq!(kind(s), Kind::Read, "{s}");
        }
        assert_eq!(kind("explain (analyze) select 1"), Kind::Unknown);
    }

    #[test]
    fn cursor_after_last_semicolon_picks_previous_statement() {
        let sql = "select 1;\nselect 2;\n";
        assert_eq!(at(sql, sql.len(), PG).unwrap().range, 10..19);
        assert_eq!(at(sql, 3, PG).unwrap().range, 0..9);
        assert_eq!(at(sql, 9, PG).unwrap().range, 0..9);
        assert_eq!(at(sql, 10, PG).unwrap().range, 10..19);
        assert_eq!(at("-- x", 2, PG), None);
    }

    #[test]
    fn random_input_never_panics() {
        let alphabet: Vec<&str> = vec![
            "select", "from", "'", "\"", "`", "$$", "$a$", ";", "--", "/*", "*/", "\n", " ", "(",
            ")", "=", "é", "$1", "delete", "explain", "pragma",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..300 {
            let mut sql = String::new();
            for _ in 0..24 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                sql.push_str(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            for s in split(&sql, PG) {
                assert!(sql.is_char_boundary(s.range.start) && sql.is_char_boundary(s.range.end));
            }
            let _ = at(&sql, sql.len() / 2, PG);
        }
    }

    fn kinds(sql: &str, engine: Engine) -> Vec<Kind> {
        split(sql, engine).iter().map(|s| s.kind).collect()
    }

    fn not_read(k: Kind) -> bool {
        k != Kind::Read
    }

    #[test]
    fn postgres_scanner_hazards_do_not_hide_statements() {
        let k = kinds("explain select 1 as a$$$; drop table t; select $$x$$", PG);
        assert_eq!(k.len(), 3);
        assert_eq!(k[0], Kind::Read);
        assert!(not_read(k[1]));
        let k = kinds("explain select E'\\'' ; drop table t; --'", PG);
        assert_eq!(k.len(), 2);
        assert!(not_read(k[1]));
        let k = kinds("select E'\\'' ; delete from t; select 'a'", PG);
        assert_eq!(k.len(), 3);
        assert!(not_read(k[1]));
        let k = kinds("select 1 /* /* */ ; delete from t; */ ; drop table u", PG);
        assert_eq!(k.len(), 2);
    }

    #[test]
    fn mysql_scanner_hazards_do_not_hide_statements() {
        let k = kinds("show tables like 'a\\'' ; drop table t; -- '", MY);
        assert_eq!(k.len(), 2);
        assert!(not_read(k[1]));
        let k = kinds("show tables /* /* */ ; drop table t; /* */", MY);
        assert_eq!(k.len(), 2);
        assert!(not_read(k[1]));
        let k = kinds("show tables # '\n; drop table t; -- '", MY);
        assert_eq!(k.len(), 2);
        assert!(not_read(k[1]));
        assert_eq!(
            kind_with("select * /*!50000 into outfile '/tmp/x' */ from t", MY),
            Kind::Unknown
        );
        assert_eq!(
            kind_with("select * into outfile '/tmp/x' from t", MY),
            Kind::Write
        );
    }

    #[test]
    fn bare_cr_in_line_comment_is_unknown() {
        let pg = "select 1 where 1=1 -- x\r or nextval('s') > 0";
        let my = "select 1 where 1=1 -- x\r or sleep(1) > 0";
        let hash = "select 1 where 1=1 # x\r or sleep(1) > 0";
        assert_eq!(kind_with(pg, PG), Kind::Unknown);
        assert_eq!(kind_with(pg, LITE), Kind::Unknown);
        assert_eq!(kind_with(my, MY), Kind::Unknown);
        assert_eq!(kind_with(hash, MY), Kind::Unknown);
    }

    #[test]
    fn crlf_line_comment_still_classifies() {
        for e in [PG, MY, LITE] {
            assert_eq!(kind_with("-- note\r\nselect 1", e), Kind::Read);
        }
    }

    #[test]
    fn sqlite_block_comments_do_not_nest() {
        let k = kinds("select 1 /* /* */ ; delete from t; /* */", LITE);
        assert_eq!(k.len(), 2);
        assert_eq!(k[1], Kind::Write);
    }

    fn kind_with(sql: &str, engine: Engine) -> Kind {
        let all = split(sql, engine);
        assert_eq!(all.len(), 1, "{sql}: {all:?}");
        all[0].kind
    }

    #[test]
    fn select_into_is_write() {
        assert_eq!(kind("select a into new_table from t"), Kind::Write);
    }

    #[test]
    fn into_inside_set_operations_is_write() {
        for sql in [
            "select 1 as a into t2 union select 2",
            "select 1 into t2 union all select 2",
            "(select 1 into t2) union select 2",
            "select 1 into t2 intersect select 1",
        ] {
            assert_eq!(kind_with(sql, PG), Kind::Write, "{sql}");
        }
        for sql in [
            "select 1 union select 2 into @a",
            "select 1 into @a union select 2",
        ] {
            assert_eq!(kind_with(sql, MY), Kind::Write, "{sql}");
        }
    }

    #[test]
    fn side_effect_functions_are_not_read() {
        let pg = [
            "select pg_catalog.set_config('a','b',false)",
            "select NEXTVAL('s')",
            "select 1 from t where x = setval('s', 1)",
            "with c as (select pg_terminate_backend(1)) select * from c",
            "select * from t where id in (select pg_advisory_lock(1))",
            "select lo_import('/etc/passwd')",
            "select dblink_exec('x','y')",
            "select pg_drop_replication_slot('s')",
            "select pg_stat_reset()",
            "select pg_log_backend_memory_contexts(1)",
            "select query_to_xml('delete from t', true, true, '')",
            "select coalesce(1, \"pg_notify\"('a','b'))",
            "select U&\"set\\005fconfig\"('a','b',false)",
            "select pg_catalog.U&\"set\\005fconfig\"(1)",
            "select u&'x' from t",
            "select pg_try_advisory_lock(1)",
            "select pg_try_advisory_lock_shared(1)",
            "select pg_try_advisory_xact_lock(1)",
            "select pg_logical_emit_message(true,'a','b')",
            "select pg_file_write('a','b',false)",
            "select pg_wal_replay_pause()",
            "select pg_backup_start('l')",
            "select pg_sleep(1)",
            "select pg_sleep_for('1s')",
            "select pg_sleep_until(now())",
            "select pg_read_file('x')",
            "select pg_read_binary_file('x')",
            "select pg_ls_dir('x')",
            "select pg_stat_file('x')",
            "select pg_ls_logdir()",
        ];
        for sql in pg {
            assert_eq!(kind_with(sql, PG), Kind::Unknown, "{sql}");
        }
        for sql in [
            "select get_lock('a',1)",
            "select 1 where sleep(1)",
            "select BENCHMARK(1,1)",
        ] {
            assert_eq!(kind_with(sql, MY), Kind::Unknown, "{sql}");
        }
        for sql in ["select load_extension('x')", "select writefile('a','b')"] {
            assert_eq!(kind_with(sql, LITE), Kind::Unknown, "{sql}");
        }
    }

    #[test]
    fn plain_functions_and_lookalike_columns_stay_read() {
        for sql in [
            "select count(*) from t",
            "select lower(name) from t",
            "select now()",
            "select nextval_count from t",
            "select nextval_count(1)",
        ] {
            assert_eq!(kind_with(sql, PG), Kind::Read, "{sql}");
        }
    }

    #[test]
    fn dollar_in_identifier_does_not_open_a_dollar_quote() {
        assert_eq!(kinds("select x$e'\\' ; delete from t; -- '", PG).len(), 2);
    }

    #[test]
    fn explain_options_and_analyze() {
        assert_eq!(kind("explain (\"analyze\") delete from t"), Kind::Unknown);
        assert_eq!(kind("explain (analyze true) delete from t"), Kind::Unknown);
        assert_eq!(kind("explain analyze delete from t"), Kind::Write);
        assert_eq!(kind("explain select 1"), Kind::Read);
    }

    #[test]
    fn pragma_allowlist() {
        for s in [
            "pragma user_version(42)",
            "pragma journal_mode(delete)",
            "pragma writable_schema(1)",
            "pragma incremental_vacuum(100)",
            "pragma wal_checkpoint(truncate)",
            "pragma optimize",
        ] {
            assert_eq!(kind_with(s, LITE), Kind::Unknown, "{s}");
        }
        for s in [
            "pragma table_info(t)",
            "pragma main.table_info('t')",
            "pragma user_version",
            "PRAGMA Foreign_Keys;",
        ] {
            assert_eq!(kind_with(s, LITE), Kind::Read, "{s}");
        }
    }

    #[test]
    fn session_and_transaction_guards() {
        for s in [
            "set default_transaction_read_only = off",
            "begin read write",
            "start transaction read write",
            "set transaction read write",
            "set role postgres",
            "set session authorization postgres",
            "start replica",
            "reset all",
            "reset role",
        ] {
            assert_eq!(kind(s), Kind::Unknown, "{s}");
        }
        assert_eq!(kind("begin"), Kind::Transaction);
        assert_eq!(kind("start transaction"), Kind::Transaction);
    }

    fn none_read(sql: &str, engine: Engine) {
        let k = kinds(sql, engine);
        assert!(
            !k.is_empty() && k.iter().all(|k| not_read(*k)),
            "{sql}: {k:?}"
        );
    }

    #[test]
    fn mysql_desc_behaves_like_explain() {
        for s in [
            "desc analyze delete from t",
            "describe analyze delete t from t join u on 1",
        ] {
            assert!(not_read(kind_with(s, MY)), "{s}");
        }
        for s in [
            "describe t",
            "desc t",
            "describe t col",
            "desc analyze select 1",
        ] {
            assert_eq!(kind_with(s, MY), Kind::Read, "{s}");
        }
    }

    #[test]
    fn backslash_before_quote_taints_the_script() {
        for s in [
            "show tables like 'a\\' ; drop table t; -- '",
            "explain select 'a\\' ; drop table t; -- '",
            "select \"a\\\" ; drop table t; -- \"",
        ] {
            none_read(s, MY);
        }
        none_read("select 'a\\' ; select ' ; delete from t; -- '", PG);
        none_read("explain select x$e'\\' ; delete from t; -- '", PG);
        assert_eq!(
            kinds("select 'a\\'; select 1", LITE),
            [Kind::Read, Kind::Read]
        );
    }

    #[test]
    fn set_allowlist() {
        for (s, e) in [
            ("set \"default_transaction_read_only\" = off", PG),
            ("set session `transaction_read_only` = 0", MY),
            ("set local session authorization postgres", PG),
            ("set session_authorization = 'postgres'", PG),
            ("set \"role\" = postgres", PG),
            ("set password = 'x'", MY),
            ("set standard_conforming_strings = off", PG),
            ("set sql_mode = 'NO_BACKSLASH_ESCAPES'", MY),
            ("set @a = 1", MY),
            ("set time_zone = '+00:00', sql_mode = ''", MY),
            ("set time_zone = 'a' b = 1", MY),
            ("set search_path = public", LITE),
        ] {
            assert_eq!(kind_with(s, e), Kind::Unknown, "{s}");
        }
        for (s, e) in [
            ("set search_path = public, app", PG),
            ("set statement_timeout to 5000", PG),
            ("set time zone 'UTC'", PG),
            ("set names utf8mb4", MY),
            ("set names utf8mb4 collate utf8mb4_bin", MY),
            ("set character set utf8mb4", MY),
            ("set session time_zone = '+00:00'", MY),
        ] {
            assert_eq!(kind_with(s, e), Kind::Session, "{s}");
        }
    }

    #[test]
    fn begin_allowlist() {
        let k = kinds("begin not atomic delete from t; end", MY);
        assert_eq!(k[0], Kind::Unknown, "{k:?}");
        assert_eq!(
            kind_with("begin isolation level serializable, read only", PG),
            Kind::Transaction
        );
        assert_eq!(kind_with("begin not deferrable", PG), Kind::Transaction);
        assert_eq!(kind_with("begin immediate", LITE), Kind::Transaction);
        assert_eq!(kind_with("begin work", MY), Kind::Transaction);
        assert_eq!(
            kind_with("start transaction with consistent snapshot", MY),
            Kind::Transaction
        );
        for (s, e) in [
            ("begin foo", PG),
            ("begin not", PG),
            ("begin foo", MY),
            ("begin immediate", PG),
            ("begin read write", PG),
            ("start transaction", LITE),
        ] {
            assert_eq!(kind_with(s, e), Kind::Unknown, "{s}");
        }
    }

    #[test]
    fn adjacent_selects_are_unknown() {
        assert_eq!(kind("select 1 select 2"), Kind::Unknown);
    }

    #[test]
    fn update_from_with_where_is_guarded() {
        assert!(!one("update t set a = 1 from u where t.a = u.a").unguarded);
        assert!(one("update t set a = 1 from u").unguarded);
    }

    #[test]
    fn routine_bodies_do_not_split() {
        let sql = "create trigger tr after insert on t begin delete from u; end; select 1";
        assert_eq!(kinds(sql, LITE), vec![Kind::Unknown, Kind::Read]);
        let sql = "create trigger tr after insert on t begin update u set a = case when 1 then 2 end; delete from u; end; select 1";
        assert_eq!(kinds(sql, LITE), vec![Kind::Unknown, Kind::Read]);
        let sql =
            "create procedure p() begin if 1 then delete from u; end if; select 1; end; select 2";
        assert_eq!(kinds(sql, MY), vec![Kind::Unknown, Kind::Read]);
        assert_eq!(kinds("begin; select 'begin'; commit", PG).len(), 3);
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let parens = |n: usize| format!("{}1{}", "(".repeat(n), ")".repeat(n));
        let a = format!("with x as (select {}) select 1", parens(20000));
        let b = format!("update t set a = 1 where {}", parens(20000));
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let _ = split(&a, PG);
                let _ = split(&b, PG);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
