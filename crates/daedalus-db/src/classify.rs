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
                        && (i == 1 || !is_word_byte(b[i - 2]))
                }
                Engine::Sqlite => false,
            };
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
    for (n, (tok, span)) in toks.iter().enumerate() {
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
        let (kind, unguarded) = if in_body {
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

fn classify(sql: &str, toks: &Toks, engine: Engine, parser: Option<&mut Parser>) -> (Kind, bool) {
    if toks.iter().any(|t| t.0 == Tok::Hazard) {
        return (Kind::Unknown, false);
    }
    let mut toks = toks;
    let mut analyzed = false;
    // EXPLAIN ANALYZE runs its statement; plain EXPLAIN only plans it.
    while let Some(w) = toks.first().and_then(|t| word(sql, t)) {
        if !w.eq_ignore_ascii_case("explain") {
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
        Some("begin" | "commit" | "end" | "rollback" | "savepoint" | "release") => {
            return if has_read_write(&ws) {
                unknown
            } else {
                (Kind::Transaction, false)
            };
        }
        Some("start") => {
            return if ws.get(1).is_some_and(|w| w == "transaction") && !has_read_write(&ws) {
                (Kind::Transaction, false)
            } else {
                unknown
            };
        }
        Some("truncate") => return (Kind::Ddl, false),
        Some("reset") => return unknown,
        Some("set") => {
            let risky = has_read_write(&ws)
                || ws.iter().any(|w| w.contains("read_only"))
                || ws
                    .iter()
                    .take(3)
                    .any(|w| w == "role" || w == "authorization");
            if risky {
                return unknown;
            }
        }
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
    let kind = kind_of(root);
    (kind, kind == Kind::Write && has_unguarded(root))
}

fn kind_of(root: Node) -> Kind {
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
        assert_eq!(kind("set x = 1"), Kind::Session);
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
        assert_eq!(k[1], Kind::Write);
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
