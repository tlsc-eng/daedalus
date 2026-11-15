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

fn qualifier(before: &str) -> Option<String> {
    let word_start = before.trim_end_matches(ident_char).len();
    let head = before[..word_start].strip_suffix('.')?;
    let name = match head.chars().next_back() {
        Some(q @ ('"' | '`')) => {
            let inner = &head[..head.len() - 1];
            let open = inner.rfind(q)?;
            &inner[open + 1..]
        }
        _ => &head[head.trim_end_matches(ident_char).len()..],
    };
    Some(name.to_owned())
}

fn tokens(statement: &str) -> Vec<String> {
    statement
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';'))
        .filter(|t| !t.is_empty())
        .map(|t| t.replace(['"', '`'], ""))
        .collect()
}

/// Each FROM/JOIN table with its alias, if any.
fn aliases(statement: &str) -> Vec<(String, Option<String>)> {
    let toks = tokens(statement);
    let mut out = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        if tok != "from" && tok != "join" {
            continue;
        }
        let Some(table) = toks.get(i + 1) else {
            continue;
        };
        let table = table.rsplit('.').next().unwrap_or(table).to_owned();
        let mut next = i + 2;
        if toks.get(next).is_some_and(|t| t == "as") {
            next += 1;
        }
        let alias = toks
            .get(next)
            .filter(|t| !ALIAS_STOPS.contains(&t.as_str()) && *t != "from")
            .cloned();
        out.push((table, alias));
    }
    out
}

fn columns(catalog: &Catalog, table: &str) -> Vec<Completion> {
    catalog
        .schemas
        .iter()
        .flat_map(|s| &s.tables)
        .filter(|t| t.name.eq_ignore_ascii_case(table))
        .flat_map(|t| &t.columns)
        .map(|c| item(&c.name, 5, COLUMN, Some(c.type_name.clone())))
        .collect()
}

/// Suggestions for the word before the cursor: `before` is the text up to it, `statement` the one it sits in.
pub(super) fn complete(
    before: &str,
    statement: &str,
    catalog: Option<&Catalog>,
) -> Vec<Completion> {
    let mut out = match qualifier(before) {
        Some(q) => {
            let Some(catalog) = catalog else {
                return vec![];
            };
            if let Some(schema) = catalog
                .schemas
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&q))
            {
                schema
                    .tables
                    .iter()
                    .map(|t| item(&t.name, 7, TABLE, Some(schema.name.clone())))
                    .collect()
            } else {
                let aliased = aliases(statement)
                    .into_iter()
                    .find(|(_, alias)| alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&q)))
                    .map(|(table, _)| table);
                columns(catalog, aliased.as_deref().unwrap_or(&q))
            }
        }
        None => {
            let mut out = Vec::new();
            if let Some(catalog) = catalog {
                for schema in &catalog.schemas {
                    out.extend(
                        schema
                            .tables
                            .iter()
                            .map(|t| item(&t.name, 7, TABLE, Some(schema.name.clone()))),
                    );
                }
                out.extend(
                    catalog
                        .schemas
                        .iter()
                        .map(|s| item(&s.name, 9, SCHEMA, None)),
                );
            }
            out.extend(KEYWORDS.iter().map(|k| item(k, 14, KEYWORD, None)));
            out
        }
    };
    out.truncate(CAP);
    out
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

    fn labels(items: &[Completion]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn keywords_without_catalog() {
        let items = complete("sel", "sel", None);
        assert!(labels(&items).contains(&"SELECT"));
        assert!(items.iter().all(|i| i.kind == Some(14)));
    }

    #[test]
    fn tables_after_schema_dot() {
        let c = catalog();
        let items = complete("select * from audit.", "select * from audit.", Some(&c));
        assert_eq!(labels(&items), ["log"]);
        assert_eq!(items[0].detail.as_deref(), Some("audit"));
        assert_eq!(items[0].kind, Some(7));
    }

    #[test]
    fn columns_after_table_dot() {
        let c = catalog();
        let items = complete("select users.", "select users.", Some(&c));
        assert_eq!(labels(&items), ["id", "email"]);
        assert_eq!(items[0].detail.as_deref(), Some("int4"));
        assert_eq!(items[0].kind, Some(5));
    }

    #[test]
    fn columns_after_alias_dot() {
        let c = catalog();
        let sql = "select u.e from public.users as u join orders o on o.id = u.id";
        let items = complete("select u.e", sql, Some(&c));
        assert_eq!(labels(&items), ["id", "email"]);
        let items = complete("select o.", sql, Some(&c));
        assert_eq!(labels(&items), ["total"]);
    }

    #[test]
    fn quoted_qualifier_is_stripped() {
        let c = catalog();
        let items = complete("select \"users\".", "select \"users\".", Some(&c));
        assert_eq!(labels(&items), ["id", "email"]);
        let items = complete("select `PUBLIC`.", "select `PUBLIC`.", Some(&c));
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
        assert_eq!(complete("", "", Some(&c)).len(), CAP);
    }
}
