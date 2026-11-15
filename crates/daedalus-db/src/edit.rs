//! Pure SQL generation for editing a table: quoting, paged SELECT, editability and change plans.

use crate::catalog::{Table, TableKind};
use crate::classify::{self, Kind};
use crate::config::Engine;
use crate::value::Cell;

pub const PAGE_ROWS: usize = 500;

pub fn quote_ident(engine: Engine, name: &str) -> String {
    let q = if engine == Engine::Mysql { '`' } else { '"' };
    let mut out = String::with_capacity(name.len() + 2);
    out.push(q);
    for c in name.chars() {
        if c == q {
            out.push(q);
        }
        out.push(c);
    }
    out.push(q);
    out
}

pub fn qualified(engine: Engine, schema: &str, table: &str) -> String {
    format!(
        "{}.{}",
        quote_ident(engine, schema),
        quote_ident(engine, table)
    )
}

/// Builds one paged SELECT; the fragments sit on their own lines so a trailing `--` cannot hide the clauses after it.
pub fn select_page(
    engine: Engine,
    schema: &str,
    table: &Table,
    filter: &str,
    order: &str,
    offset: usize,
) -> Result<String, String> {
    let filter = filter.trim();
    let order = order.trim();
    if filter.contains(';') {
        return Err("Remove the ; from the filter.".to_string());
    }
    if order.contains(';') {
        return Err("Remove the ; from the order.".to_string());
    }
    let mut sql = format!("SELECT * FROM {}\n", qualified(engine, schema, &table.name));
    if !filter.is_empty() {
        sql.push_str(&format!("WHERE (\n{filter}\n)\n"));
    }
    let default_order = table
        .columns
        .iter()
        .filter(|c| c.pk)
        .map(|c| quote_ident(engine, &c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let order = if order.is_empty() {
        default_order.as_str()
    } else {
        order
    };
    if !order.is_empty() {
        sql.push_str(&format!("ORDER BY\n{order}\n"));
    }
    sql.push_str(&format!("LIMIT {} OFFSET {offset}", PAGE_ROWS + 1));
    let stmts = classify::split(&sql, engine);
    if stmts.len() != 1 || stmts[0].kind != Kind::Read {
        return Err("The filter and order must be expressions, not statements.".to_string());
    }
    Ok(sql)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub pk: Vec<usize>,
    pub editable: Vec<bool>,
}

pub fn shape(engine: Engine, table: &Table) -> Result<Shape, &'static str> {
    if table.kind == TableKind::View {
        return Err("Views are read-only.");
    }
    let pk: Vec<usize> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.pk)
        .map(|(i, _)| i)
        .collect();
    if pk.is_empty() {
        return Err("Read-only: the table has no primary key.");
    }
    if pk
        .iter()
        .any(|&i| binary_type(engine, &table.columns[i].type_name))
    {
        return Err("Read-only: the primary key has a binary column.");
    }
    let editable = table
        .columns
        .iter()
        .map(|c| !binary_type(engine, &c.type_name))
        .collect();
    Ok(Shape { pk, editable })
}

const MYSQL_BINARY: [&str; 17] = [
    "binary",
    "varbinary",
    "tinyblob",
    "blob",
    "mediumblob",
    "longblob",
    "bit",
    "geometry",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
    "geomcollection",
    "vector",
];

pub fn binary_type(engine: Engine, type_name: &str) -> bool {
    let t = type_name.trim().to_ascii_lowercase();
    match engine {
        Engine::Postgres => t == "bytea" || t == "bytea[]",
        Engine::Mysql => {
            let first = t
                .split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or("");
            MYSQL_BINARY.contains(&first)
        }
        Engine::Sqlite => {
            let u = t.to_ascii_uppercase();
            if u.contains("INT") || ["CHAR", "CLOB", "TEXT"].iter().any(|k| u.contains(k)) {
                false
            } else {
                u.contains("BLOB") || u.is_empty()
            }
        }
    }
}

pub enum Change {
    Update {
        pk: Vec<Cell>,
        set: Vec<(usize, Cell)>,
    },
    Delete {
        pk: Vec<Cell>,
    },
    Insert {
        values: Vec<(usize, Cell)>,
    },
}

pub struct Stmt {
    pub sql: String,
    pub params: Vec<Cell>,
}

pub struct Plan {
    pub statements: Vec<Stmt>,
    pub preview: String,
}

/// Writes the executable SQL and its display twin together, so placeholders are never found by searching text.
struct Builder {
    engine: Engine,
    sql: String,
    shown: String,
    params: Vec<Cell>,
}

impl Builder {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            sql: String::new(),
            shown: String::new(),
            params: Vec::new(),
        }
    }

    fn text(&mut self, s: &str) {
        self.sql.push_str(s);
        self.shown.push_str(s);
    }

    fn ident(&mut self, name: &str) {
        let q = quote_ident(self.engine, name);
        self.text(&q);
    }

    fn value(&mut self, cell: &Cell) {
        if self.engine == Engine::Postgres {
            self.sql.push_str(&format!("${}", self.params.len() + 1));
        } else {
            self.sql.push('?');
        }
        self.shown.push_str(&self.literal(cell));
        self.params.push(cell.clone());
    }

    fn literal(&self, cell: &Cell) -> String {
        match cell {
            None => "NULL".to_string(),
            Some(s) => {
                let s = if self.engine == Engine::Mysql {
                    s.replace('\\', "\\\\")
                } else {
                    s.clone()
                };
                format!("'{}'", s.replace('\'', "''"))
            }
        }
    }

    fn where_pk(&mut self, table: &Table, shape: &Shape, pk: &[Cell]) {
        self.text(" WHERE ");
        for (n, (&col, val)) in shape.pk.iter().zip(pk).enumerate() {
            if n > 0 {
                self.text(" AND ");
            }
            self.ident(&table.columns[col].name);
            self.text(" = ");
            self.value(val);
        }
    }

    fn finish(self) -> (Stmt, String) {
        (
            Stmt {
                sql: self.sql,
                params: self.params,
            },
            self.shown,
        )
    }
}

/// Statement i is change i; the caller orders deletes, then updates, then inserts.
pub fn plan(
    engine: Engine,
    schema: &str,
    table: &Table,
    shape: &Shape,
    changes: &[Change],
) -> Plan {
    let q = qualified(engine, schema, &table.name);
    let begin = if engine == Engine::Mysql {
        "START TRANSACTION;"
    } else {
        "BEGIN;"
    };
    let mut statements = Vec::with_capacity(changes.len());
    let mut lines = vec![begin.to_string()];
    for change in changes {
        let mut b = Builder::new(engine);
        match change {
            Change::Update { pk, set } => {
                b.text(&format!("UPDATE {q} SET "));
                for (n, (col, val)) in set.iter().enumerate() {
                    if n > 0 {
                        b.text(", ");
                    }
                    b.ident(&table.columns[*col].name);
                    b.text(" = ");
                    b.value(val);
                }
                b.where_pk(table, shape, pk);
            }
            Change::Delete { pk } => {
                b.text(&format!("DELETE FROM {q}"));
                b.where_pk(table, shape, pk);
            }
            Change::Insert { values } if values.is_empty() => {
                if engine == Engine::Mysql {
                    b.text(&format!("INSERT INTO {q} () VALUES ()"));
                } else {
                    b.text(&format!("INSERT INTO {q} DEFAULT VALUES"));
                }
            }
            Change::Insert { values } => {
                b.text(&format!("INSERT INTO {q} ("));
                for (n, (col, _)) in values.iter().enumerate() {
                    if n > 0 {
                        b.text(", ");
                    }
                    b.ident(&table.columns[*col].name);
                }
                b.text(") VALUES (");
                for (n, (_, val)) in values.iter().enumerate() {
                    if n > 0 {
                        b.text(", ");
                    }
                    b.value(val);
                }
                b.text(")");
            }
        }
        let (stmt, mut shown) = b.finish();
        shown.push(';');
        lines.push(shown);
        statements.push(stmt);
    }
    lines.push("COMMIT;".to_string());
    Plan {
        statements,
        preview: lines.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogColumn;

    fn col(name: &str, ty: &str, pk: bool) -> CatalogColumn {
        CatalogColumn {
            name: name.into(),
            type_name: ty.into(),
            nullable: true,
            pk,
        }
    }

    fn table(cols: Vec<CatalogColumn>) -> Table {
        Table {
            name: "t".into(),
            kind: TableKind::Table,
            columns: cols,
        }
    }

    fn users() -> Table {
        table(vec![col("id", "int", true), col("name", "text", false)])
    }

    fn s(v: &str) -> Cell {
        Some(v.to_string())
    }

    const ALL: [Engine; 3] = [Engine::Postgres, Engine::Mysql, Engine::Sqlite];

    #[test]
    fn identifier_with_quote_is_escaped() {
        assert_eq!(quote_ident(Engine::Postgres, "a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident(Engine::Sqlite, "a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn mysql_quotes_backticks() {
        assert_eq!(quote_ident(Engine::Mysql, "a`b"), "`a``b`");
        assert_eq!(qualified(Engine::Mysql, "d", "t"), "`d`.`t`");
    }

    #[test]
    fn select_orders_by_primary_key_by_default() {
        let t = table(vec![col("a", "int", true), col("b", "int", true)]);
        let sql = select_page(Engine::Postgres, "public", &t, "", "", 0).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"public\".\"t\"\nORDER BY\n\"a\", \"b\"\nLIMIT 501 OFFSET 0"
        );
    }

    #[test]
    fn select_without_pk_has_no_order() {
        let t = table(vec![col("a", "int", false)]);
        let sql = select_page(Engine::Sqlite, "main", &t, "", "", 1000).unwrap();
        assert!(!sql.contains("ORDER BY"));
        assert!(sql.ends_with("LIMIT 501 OFFSET 1000"));
    }

    #[test]
    fn select_wraps_filter_on_its_own_lines() {
        let sql = select_page(Engine::Mysql, "d", &users(), " id > 1 ", " name DESC ", 0).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM `d`.`t`\nWHERE (\nid > 1\n)\nORDER BY\nname DESC\nLIMIT 501 OFFSET 0"
        );
    }

    #[test]
    fn filter_with_semicolon_is_refused() {
        let r = select_page(Engine::Postgres, "p", &users(), "1=1; DROP TABLE t", "", 0);
        assert_eq!(r, Err("Remove the ; from the filter.".to_string()));
    }

    #[test]
    fn filter_with_union_still_classifies_read() {
        for e in ALL {
            let r = select_page(
                e,
                "p",
                &users(),
                "1=1) UNION ALL SELECT * FROM t WHERE (1=1",
                "",
                0,
            );
            assert!(r.is_ok(), "{r:?}");
        }
    }

    #[test]
    fn parenthesised_union_arm_fails_closed() {
        let r = select_page(
            Engine::Postgres,
            "p",
            &users(),
            "1=1) UNION ALL (SELECT * FROM t",
            "",
            0,
        );
        assert!(r.is_err());
    }

    #[test]
    fn order_with_semicolon_is_refused() {
        let r = select_page(Engine::Postgres, "p", &users(), "", "id; DELETE FROM t", 0);
        assert_eq!(r, Err("Remove the ; from the order.".to_string()));
    }

    #[test]
    fn trailing_comment_cannot_drop_limit() {
        let sql = select_page(Engine::Postgres, "p", &users(), "id > 1 -- x", "", 0).unwrap();
        let last = sql.lines().last().unwrap();
        assert_eq!(last, "LIMIT 501 OFFSET 0");
        assert!(sql.contains("\n-- x\n") || sql.contains("id > 1 -- x\n)"));
    }

    #[test]
    fn view_is_read_only() {
        let mut t = users();
        t.kind = TableKind::View;
        assert_eq!(shape(Engine::Postgres, &t), Err("Views are read-only."));
    }

    #[test]
    fn table_without_pk_is_read_only() {
        let t = table(vec![col("a", "int", false)]);
        assert_eq!(
            shape(Engine::Postgres, &t),
            Err("Read-only: the table has no primary key.")
        );
    }

    #[test]
    fn binary_pk_is_read_only() {
        let t = table(vec![col("k", "bytea", true)]);
        assert_eq!(
            shape(Engine::Postgres, &t),
            Err("Read-only: the primary key has a binary column.")
        );
    }

    #[test]
    fn shape_marks_binary_columns_not_editable() {
        let t = table(vec![
            col("id", "int", true),
            col("b", "bytea", false),
            col("n", "text", false),
        ]);
        let sh = shape(Engine::Postgres, &t).unwrap();
        assert_eq!(sh.pk, vec![0]);
        assert_eq!(sh.editable, vec![true, false, true]);
    }

    #[test]
    fn sqlite_affinity_marks_blob_columns() {
        let e = Engine::Sqlite;
        assert!(binary_type(e, "BLOB"));
        assert!(binary_type(e, ""));
        assert!(binary_type(e, "  "));
        assert!(!binary_type(e, "INTEGER"));
        assert!(!binary_type(e, "VARCHAR(10)"));
        assert!(!binary_type(e, "TEXT"));
        assert!(!binary_type(e, "REAL"));
        assert!(!binary_type(e, "NUMERIC"));
        assert!(!binary_type(e, "POINTINT"));
    }

    #[test]
    fn mysql_bit_and_blob_are_binary_but_bigint_is_not() {
        let e = Engine::Mysql;
        for t in ["bit(1)", "varbinary(16)", "LONGBLOB", "geometry", "point"] {
            assert!(binary_type(e, t), "{t}");
        }
        for t in ["bigint", "bigint unsigned", "varchar(5)", "text"] {
            assert!(!binary_type(e, t), "{t}");
        }
        assert!(binary_type(Engine::Postgres, "BYTEA[]"));
        assert!(!binary_type(Engine::Postgres, "text"));
    }

    #[test]
    fn pg_update_numbers_params_per_statement() {
        let t = users();
        let sh = shape(Engine::Postgres, &t).unwrap();
        let ch = [
            Change::Update {
                pk: vec![s("1")],
                set: vec![(1, s("x"))],
            },
            Change::Update {
                pk: vec![s("2")],
                set: vec![(1, s("y"))],
            },
        ];
        let p = plan(Engine::Postgres, "public", &t, &sh, &ch);
        let want = "UPDATE \"public\".\"t\" SET \"name\" = $1 WHERE \"id\" = $2";
        assert_eq!(p.statements[0].sql, want);
        assert_eq!(p.statements[1].sql, want);
        assert_eq!(p.statements[1].params, vec![s("y"), s("2")]);
    }

    #[test]
    fn composite_pk_where_uses_and() {
        let t = table(vec![col("a", "int", true), col("b", "int", true)]);
        let sh = shape(Engine::Mysql, &t).unwrap();
        let ch = [Change::Delete {
            pk: vec![s("1"), s("2")],
        }];
        let p = plan(Engine::Mysql, "d", &t, &sh, &ch);
        assert_eq!(
            p.statements[0].sql,
            "DELETE FROM `d`.`t` WHERE `a` = ? AND `b` = ?"
        );
        assert_eq!(p.statements[0].params, vec![s("1"), s("2")]);
    }

    #[test]
    fn insert_without_values_uses_default_values() {
        let t = users();
        for (e, want) in [
            (Engine::Postgres, "INSERT INTO \"s\".\"t\" DEFAULT VALUES"),
            (Engine::Sqlite, "INSERT INTO \"s\".\"t\" DEFAULT VALUES"),
            (Engine::Mysql, "INSERT INTO `s`.`t` () VALUES ()"),
        ] {
            let sh = shape(e, &t).unwrap();
            let p = plan(e, "s", &t, &sh, &[Change::Insert { values: vec![] }]);
            assert_eq!(p.statements[0].sql, want);
            assert!(p.statements[0].params.is_empty());
        }
    }

    #[test]
    fn insert_with_values_lists_columns() {
        let t = users();
        let sh = shape(Engine::Postgres, &t).unwrap();
        let ch = [Change::Insert {
            values: vec![(0, s("1")), (1, None)],
        }];
        let p = plan(Engine::Postgres, "s", &t, &sh, &ch);
        assert_eq!(
            p.statements[0].sql,
            "INSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES ($1, $2)"
        );
        assert_eq!(p.statements[0].params, vec![s("1"), None]);
    }

    #[test]
    fn null_and_empty_string_differ() {
        let t = users();
        let sh = shape(Engine::Sqlite, &t).unwrap();
        let ch = [
            Change::Update {
                pk: vec![s("1")],
                set: vec![(1, None)],
            },
            Change::Update {
                pk: vec![s("1")],
                set: vec![(1, s(""))],
            },
        ];
        let p = plan(Engine::Sqlite, "main", &t, &sh, &ch);
        assert_eq!(p.statements[0].params[0], None);
        assert_eq!(p.statements[1].params[0], s(""));
        assert!(p.preview.contains("\"name\" = NULL WHERE"));
        assert!(p.preview.contains("\"name\" = '' WHERE"));
    }

    #[test]
    fn preview_sql_matches_params() {
        let t = users();
        let sh = shape(Engine::Postgres, &t).unwrap();
        let ch = [
            Change::Delete { pk: vec![s("9")] },
            Change::Update {
                pk: vec![s("1")],
                set: vec![(1, s("it's"))],
            },
        ];
        let p = plan(Engine::Postgres, "public", &t, &sh, &ch);
        assert_eq!(p.statements.len(), ch.len());
        let lines: Vec<&str> = p.preview.lines().collect();
        assert_eq!(lines.first(), Some(&"BEGIN;"));
        assert_eq!(lines.last(), Some(&"COMMIT;"));
        assert_eq!(lines[1], "DELETE FROM \"public\".\"t\" WHERE \"id\" = '9';");
        assert_eq!(
            lines[2],
            "UPDATE \"public\".\"t\" SET \"name\" = 'it''s' WHERE \"id\" = '1';"
        );

        let m = shape(Engine::Mysql, &t).unwrap();
        let ch = [Change::Update {
            pk: vec![s("1")],
            set: vec![(1, s("a\\b"))],
        }];
        let p = plan(Engine::Mysql, "d", &t, &m, &ch);
        assert!(p.preview.starts_with("START TRANSACTION;\n"));
        assert!(p.preview.contains("'a\\\\b'"));
        assert_eq!(p.statements[0].params[0], s("a\\b"));
    }

    #[test]
    fn placeholder_in_identifier_is_not_replaced() {
        let t = table(vec![col("id", "int", true), col("?x", "text", false)]);
        let sh = shape(Engine::Sqlite, &t).unwrap();
        let ch = [Change::Update {
            pk: vec![s("1")],
            set: vec![(1, s("v"))],
        }];
        let p = plan(Engine::Sqlite, "main", &t, &sh, &ch);
        assert!(p.preview.contains("\"?x\" = 'v' WHERE \"id\" = '1';"));

        let t = table(vec![col("id", "int", true), col("$1", "text", false)]);
        let sh = shape(Engine::Postgres, &t).unwrap();
        let p = plan(Engine::Postgres, "s", &t, &sh, &ch);
        assert!(p.preview.contains("\"$1\" = 'v' WHERE \"id\" = '1';"));
        assert_eq!(p.statements[0].params, vec![s("v"), s("1")]);
    }
}
