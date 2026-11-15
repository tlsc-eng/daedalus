use daedalus_db::catalog::Catalog;
use daedalus_editor::Completion;

const CAP: usize = 2_000;

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "ALTER",
    "DROP",
    "TABLE",
    "INDEX",
    "VIEW",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "ON",
    "USING",
    "AS",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "UNION",
    "ALL",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "WITH",
    "RETURNING",
    "EXPLAIN",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
];

const ALIAS_STOPS: &[&str] = &[
    ",",
    "where",
    "on",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "outer",
    "using",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "union",
    "set",
    "select",
    "values",
    "returning",
];

const COLUMN: u8 = 0;
const TABLE: u8 = 1;
const SCHEMA: u8 = 2;
const KEYWORD: u8 = 3;

fn item(label: &str, kind: u32, rank: u8, detail: Option<String>) -> Completion {
    Completion {
        label: label.to_owned(),
        kind: Some(kind),
        detail,
        filter_text: label.to_owned(),
        sort_text: format!("{rank}{label}"),
        text: label.to_owned(),
        range: None,
        select: None,
        stops: vec![],
        additional_edits: vec![],
        preselect: false,
        documentation: vec![],
        resolve: false,
    }
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// The last identifier of `s`, quotes stripped, and what precedes it.
fn last_ident(s: &str) -> Option<(&str, &str)> {
    match s.chars().next_back() {
        Some(q @ ('"' | '`')) => {
            let inner = &s[..s.len() - 1];
            let open = inner.rfind(q)?;
            Some((&inner[open + 1..], &inner[..open]))
        }
        _ => {
            let at = s.trim_end_matches(ident_char).len();
            Some((&s[at..], &s[..at]))
        }
    }
}

/// The optional schema and the name before the `.` that precedes the word being typed.
fn qualifier(before: &str) -> Option<(Option<String>, String)> {
    let word_start = before.trim_end_matches(ident_char).len();
    let head = before[..word_start].strip_suffix('.')?;
    let (name, rest) = last_ident(head)?;
    let schema = rest
        .strip_suffix('.')
        .and_then(last_ident)
        .map(|(s, _)| s.to_owned());
    Some((schema, name.to_owned()))
}

fn tokens(statement: &str) -> Vec<String> {
    statement
        .to_lowercase()
        .replace(',', " , ")
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ';'))
        .filter(|t| !t.is_empty())
        .map(|t| t.replace(['"', '`'], ""))
        .collect()
}

struct Source {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

fn aliases(statement: &str) -> Vec<Source> {
    let toks = tokens(statement);
    let mut out = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        if tok != "from" && tok != "join" {
            continue;
        }
        let mut at = i + 1;
        while let Some(name) = toks.get(at) {
            let mut parts = name.rsplit('.');
            let table = parts.next().unwrap_or_default().to_owned();
            let schema = parts.next().map(str::to_owned);
            at += 1;
            if toks.get(at).is_some_and(|t| t == "as") {
                at += 1;
            }
            let alias = toks
                .get(at)
                .filter(|t| !ALIAS_STOPS.contains(&t.as_str()) && *t != "from")
                .cloned();
            if alias.is_some() {
                at += 1;
            }
            out.push(Source {
                schema,
                table,
                alias,
            });
            if toks.get(at).is_some_and(|t| t == ",") {
                at += 1;
            } else {
                break;
            }
        }
    }
    out
}

/// A subsequence match, as the editor's fuzzy filter also accepts.
fn matches(word: &str, label: &str) -> bool {
    let mut label = label.chars().flat_map(char::to_lowercase);
    word.chars()
        .flat_map(char::to_lowercase)
        .all(|w| label.any(|c| c == w))
}

/// Columns of one table: the named schema's, else the first schema in catalog order that holds it.
fn columns(catalog: &Catalog, schema: Option<&str>, table: &str) -> Vec<Completion> {
    catalog
        .schemas
        .iter()
        .filter(|s| schema.is_none_or(|n| s.name.eq_ignore_ascii_case(n)))
        .find_map(|s| s.tables.iter().find(|t| t.name.eq_ignore_ascii_case(table)))
        .map(|t| {
            t.columns
                .iter()
                .map(|c| item(&c.name, 5, COLUMN, Some(c.type_name.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn tables_of(schema: &daedalus_db::catalog::Schema) -> Vec<Completion> {
    schema
        .tables
        .iter()
        .map(|t| item(&t.name, 7, TABLE, Some(schema.name.clone())))
        .collect()
}

/// Suggestions matching the word before the cursor, and whether the cap dropped any.
///
/// `before` is the text up to the cursor, `statement` the one it sits in.
pub(super) fn complete(
    before: &str,
    statement: &str,
    catalog: Option<&Catalog>,
) -> (Vec<Completion>, bool) {
    let word = &before[before.trim_end_matches(ident_char).len()..];
    // Filled in this order so a cap drops tables before keywords.
    let mut columns_out = Vec::new();
    let mut keywords = Vec::new();
    let mut schemas = Vec::new();
    let mut tables = Vec::new();
    match qualifier(before) {
        Some((schema, q)) => {
            let Some(catalog) = catalog else {
                return (vec![], false);
            };
            let as_schema = schema.is_none().then(|| {
                catalog
                    .schemas
                    .iter()
                    .find(|s| s.name.eq_ignore_ascii_case(&q))
            });
            match as_schema.flatten() {
                Some(s) => tables = tables_of(s),
                None => {
                    let source = schema.is_none().then(|| {
                        aliases(statement).into_iter().find(|a| {
                            a.alias
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(&q))
                        })
                    });
                    columns_out = match source.flatten() {
                        Some(a) => columns(catalog, a.schema.as_deref(), &a.table),
                        None => columns(catalog, schema.as_deref(), &q),
                    };
                }
            }
        }
        None => {
            keywords = KEYWORDS
                .iter()
                .map(|k| item(k, 14, KEYWORD, None))
                .collect();
            if let Some(catalog) = catalog {
                schemas = catalog
                    .schemas
                    .iter()
                    .map(|s| item(&s.name, 9, SCHEMA, None))
                    .collect();
                tables = catalog.schemas.iter().flat_map(tables_of).collect();
            }
        }
    }
    let mut out: Vec<Completion> = columns_out
        .into_iter()
        .chain(keywords)
        .chain(schemas)
        .chain(tables)
        .filter(|i| matches(word, &i.label))
        .collect();
    let dropped = out.len() > CAP;
    out.truncate(CAP);
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use daedalus_db::catalog::{CatalogColumn, Schema, Table, TableKind};

    fn catalog() -> Catalog {
        let col = |name: &str, ty: &str| CatalogColumn {
            name: name.into(),
            type_name: ty.into(),
            nullable: true,
            pk: false,
        };
        let table = |name: &str, cols: Vec<CatalogColumn>| Table {
            name: name.into(),
            kind: TableKind::Table,
            columns: cols,
        };
        Catalog {
            schemas: vec![
                Schema {
                    name: "public".into(),
                    tables: vec![
                        table("users", vec![col("id", "int4"), col("email", "text")]),
                        table("orders", vec![col("total", "numeric")]),
                    ],
                },
                Schema {
                    name: "audit".into(),
                    tables: vec![table("log", vec![])],
                },
            ],
            truncated: false,
        }
    }

    fn go(before: &str, statement: &str, catalog: Option<&Catalog>) -> Vec<Completion> {
        complete(before, statement, catalog).0
    }

    fn labels(items: &[Completion]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn keywords_without_catalog() {
        let items = go("sel", "sel", None);
        assert!(labels(&items).contains(&"SELECT"));
        assert!(items.iter().all(|i| i.kind == Some(14)));
    }

    #[test]
    fn tables_after_schema_dot() {
        let c = catalog();
        let items = go("select * from audit.", "select * from audit.", Some(&c));
        assert_eq!(labels(&items), ["log"]);
        assert_eq!(items[0].detail.as_deref(), Some("audit"));
        assert_eq!(items[0].kind, Some(7));
    }

    #[test]
    fn columns_after_table_dot() {
        let c = catalog();
        let items = go("select users.", "select users.", Some(&c));
        assert_eq!(labels(&items), ["id", "email"]);
        assert_eq!(items[0].detail.as_deref(), Some("int4"));
        assert_eq!(items[0].kind, Some(5));
    }

    #[test]
    fn columns_after_alias_dot() {
        let c = catalog();
        let sql = "select u.e from public.users as u join orders o on o.id = u.id";
        let items = go("select u.e", sql, Some(&c));
        assert_eq!(labels(&items), ["email"]);
        let items = go("select o.", sql, Some(&c));
        assert_eq!(labels(&items), ["total"]);
    }

    #[test]
    fn quoted_qualifier_is_stripped() {
        let c = catalog();
        let items = go("select \"users\".", "select \"users\".", Some(&c));
        assert_eq!(labels(&items), ["id", "email"]);
        let items = go("select `PUBLIC`.", "select `PUBLIC`.", Some(&c));
        assert_eq!(labels(&items), ["users", "orders"]);
    }

    #[test]
    fn results_are_capped() {
        let tables = (0..CAP + 50)
            .map(|i| Table {
                name: format!("t{i}"),
                kind: TableKind::Table,
                columns: vec![],
            })
            .collect();
        let c = Catalog {
            schemas: vec![Schema {
                name: "s".into(),
                tables,
            }],
            truncated: false,
        };
        let (items, dropped) = complete("", "", Some(&c));
        assert_eq!(items.len(), CAP);
        assert!(dropped);
        let (items, _) = complete("SEL", "", Some(&c));
        assert!(labels(&items).contains(&"SELECT"));
        let last = format!("t{}", CAP + 49);
        let (items, dropped) = complete("t20", "", Some(&c));
        assert!(labels(&items).contains(&last.as_str()));
        assert!(!dropped);
    }

    #[test]
    fn columns_follow_the_qualified_schema() {
        let mut c = catalog();
        c.schemas[1].tables.push(Table {
            name: "users".into(),
            kind: TableKind::Table,
            columns: vec![CatalogColumn {
                name: "actor".into(),
                type_name: "text".into(),
                nullable: true,
                pk: false,
            }],
        });
        let items = go("select audit.users.", "select audit.users.", Some(&c));
        assert_eq!(labels(&items), ["actor"]);
        let sql = "select u. from audit.users u";
        assert_eq!(labels(&go("select u.", sql, Some(&c))), ["actor"]);
        let items = go("select users.", "select users.", Some(&c));
        assert_eq!(labels(&items), ["id", "email"]);
    }

    #[test]
    fn comma_separated_tables_are_not_aliases() {
        let found = aliases("select * from users, orders o, audit.log where 1");
        let got: Vec<_> = found
            .iter()
            .map(|a| (a.table.as_str(), a.alias.as_deref()))
            .collect();
        assert_eq!(got, [("users", None), ("orders", Some("o")), ("log", None)]);
    }
}
