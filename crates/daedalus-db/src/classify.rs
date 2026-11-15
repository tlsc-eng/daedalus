//! Splits a SQL script into statements and classifies each one for safety prompts.

use std::ops::Range;

use tree_sitter::{Node, Parser};

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
}

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
fn scan(sql: &str) -> Vec<(Tok, Range<usize>)> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            i = b[i..]
                .iter()
                .position(|x| *x == b'\n')
                .map_or(b.len(), |p| i + p);
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
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
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push((Tok::Literal, start..i));
        } else if let Some(n) =
            dollar_tag_len(&b[i..]).filter(|_| i == 0 || !is_word_byte(b[i - 1]))
        {
            let tag = &b[i..i + n];
            i = find(&b[i + n..], tag).map_or(b.len(), |p| i + n + p + n);
            out.push((Tok::Literal, start..i));
        } else if is_word_byte(c) {
            while i < b.len() && is_word_byte(b[i]) {
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

/// Splits `sql` on top-level semicolons and classifies each statement.
pub fn split(sql: &str) -> Vec<Statement> {
    let toks = scan(sql);
    let mut parser = Parser::new();
    let ready = parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .is_ok();
    let mut out = Vec::new();
    let mut first = 0;
    for (n, (tok, span)) in toks.iter().enumerate() {
        let last = n + 1 == toks.len();
        if *tok != Tok::Semi && !last {
            continue;
        }
        let chunk = &toks[first..=n];
        first = n + 1;
        if chunk.iter().all(|(t, _)| *t == Tok::Semi) {
            continue;
        }
        let range = chunk[0].1.start..span.end;
        let (kind, unguarded) = classify(
            &sql[range.clone()],
            chunk,
            sql,
            ready.then_some(&mut parser),
        );
        out.push(Statement {
            range,
            kind,
            unguarded,
        });
    }
    out
}

/// The statement containing `offset`, or the nearest one before it.
pub fn at(sql: &str, offset: usize) -> Option<Statement> {
    let all = split(sql);
    let idx = all
        .iter()
        .rposition(|s| s.range.start <= offset)
        .unwrap_or(0);
    all.into_iter().nth(idx)
}

fn word<'a>(sql: &'a str, tok: &(Tok, Range<usize>)) -> Option<&'a str> {
    (tok.0 == Tok::Word).then(|| &sql[tok.1.clone()])
}

fn is_keyword_read(sql: &str, toks: &[(Tok, Range<usize>)]) -> bool {
    let Some(head) = toks.first().and_then(|t| word(sql, t)) else {
        return false;
    };
    let words = || toks.iter().filter_map(|t| word(sql, t));
    match head.to_ascii_lowercase().as_str() {
        "show" | "describe" | "desc" => true,
        "explain" => {
            !words().any(|w| w.eq_ignore_ascii_case("analyze") || w.eq_ignore_ascii_case("analyse"))
        }
        "pragma" => !toks.iter().any(|t| t.0 == Tok::Punct(b'=')),
        _ => false,
    }
}

fn classify(
    text: &str,
    toks: &[(Tok, Range<usize>)],
    full: &str,
    parser: Option<&mut Parser>,
) -> (Kind, bool) {
    if is_keyword_read(full, toks) {
        return (Kind::Read, false);
    }
    match toks
        .first()
        .and_then(|t| word(full, t))
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("begin" | "start" | "commit" | "end" | "rollback" | "savepoint" | "release") => {
            return (Kind::Transaction, false);
        }
        Some("truncate") => return (Kind::Ddl, false),
        _ => {}
    }
    let Some(tree) = parser.and_then(|p| p.parse(text, None)) else {
        return (Kind::Unknown, false);
    };
    let root = tree.root_node();
    if root.has_error() {
        return (Kind::Unknown, false);
    }
    let kind = kind_of(root);
    (kind, kind == Kind::Write && has_unguarded(root))
}

fn kind_of(root: Node) -> Kind {
    let mut cur = root.walk();
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
                "insert" | "update" | "delete" => write = true,
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

fn contains_write(node: Node) -> bool {
    if matches!(node.kind(), "insert" | "update" | "delete") {
        return true;
    }
    let mut c = node.walk();
    node.named_children(&mut c).any(contains_write)
}

fn has_unguarded(node: Node) -> bool {
    if matches!(node.kind(), "update" | "delete") {
        let mut c = node.walk();
        let mut guarded = node.named_children(&mut c).any(|k| k.kind() == "where");
        // The grammar nests a DELETE's WHERE inside the sibling FROM node.
        if let Some(from) = node.next_named_sibling().filter(|n| n.kind() == "from") {
            let mut c = from.walk();
            guarded |= from.named_children(&mut c).any(|k| k.kind() == "where");
        }
        if !guarded {
            return true;
        }
    }
    let mut c = node.walk();
    node.named_children(&mut c).any(has_unguarded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str) -> Statement {
        let all = split(sql);
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
        let all = split(sql);
        assert_eq!(all.len(), 4, "{all:?}");
        assert_eq!(&sql[all[0].range.clone()], "select 'a;b';");
        assert_eq!(&sql[all[1].range.clone()], "select \"x;y\" from t;");
        assert!(sql[all[2].range.clone()].starts_with("select 1 -- c; d"));
        assert_eq!(&sql[all[3].range.clone()], "select 2;");
    }

    #[test]
    fn doubled_quotes_stay_inside_the_string() {
        assert_eq!(split("select 'it''s; fine'; select 2").len(), 2);
    }

    #[test]
    fn dollar_quoted_bodies_do_not_split() {
        let a =
            "create function f() returns int as $$ select 1; select 2; $$ language sql; select 3";
        assert_eq!(split(a).len(), 2);
        let b = "do $fn$ begin perform 1; end $fn$; select 2";
        let all = split(b);
        assert_eq!(all.len(), 2);
        assert_eq!(
            &b[all[0].range.clone()],
            "do $fn$ begin perform 1; end $fn$;"
        );
        assert_eq!(split("select $1; select 2").len(), 2);
    }

    #[test]
    fn comment_only_chunks_are_skipped() {
        assert!(split("-- nothing\n/* x */ ;;").is_empty());
        assert!(split("").is_empty());
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
        assert_eq!(at(sql, sql.len()).unwrap().range, 10..19);
        assert_eq!(at(sql, 3).unwrap().range, 0..9);
        assert_eq!(at(sql, 9).unwrap().range, 0..9);
        assert_eq!(at(sql, 10).unwrap().range, 10..19);
        assert_eq!(at("-- x", 2), None);
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
            for s in split(&sql) {
                assert!(sql.is_char_boundary(s.range.start) && sql.is_char_boundary(s.range.end));
            }
            let _ = at(&sql, sql.len() / 2);
        }
    }
}
