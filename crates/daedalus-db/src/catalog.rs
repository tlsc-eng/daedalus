use crate::value::Cell;

/// Most tables one catalog holds; the rest are dropped and `truncated` is set.
pub const MAX_TABLES: usize = 5_000;

/// Schemas, tables and columns of one database, in server order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub schemas: Vec<Schema>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<CatalogColumn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumn {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub pk: bool,
}

const BAD_ROW: &str = "unexpected catalog row";

fn flag(cell: &Cell) -> Result<bool, String> {
    match cell.as_deref() {
        Some("1") => Ok(true),
        Some("0") => Ok(false),
        _ => Err(BAD_ROW.into()),
    }
}

/// Builds a catalog from the 7-column rows (schema, table, kind, column, type, nullable, pk) of the engine query.
///
/// Consecutive rows with the same schema and table form one table; server order is kept.
pub fn from_rows(rows: Vec<Vec<Cell>>) -> Result<Catalog, String> {
    let mut schemas: Vec<Schema> = Vec::new();
    let mut tables = 0usize;
    let mut truncated = false;
    let mut last: Option<(String, String)> = None;
    for row in rows {
        let [schema, table, kind, name, type_name, nullable, pk] =
            <[Cell; 7]>::try_from(row).map_err(|_| String::from(BAD_ROW))?;
        let (Some(schema), Some(table)) = (schema, table) else {
            return Err(BAD_ROW.into());
        };
        let kind = match kind.as_deref() {
            Some("table") => TableKind::Table,
            Some("view") => TableKind::View,
            _ => return Err(BAD_ROW.into()),
        };
        let column = match name {
            Some(name) => Some(CatalogColumn {
                name,
                type_name: type_name.ok_or_else(|| String::from(BAD_ROW))?,
                nullable: flag(&nullable)?,
                pk: flag(&pk)?,
            }),
            None => None,
        };
        let same = last
            .as_ref()
            .is_some_and(|(s, t)| *s == schema && *t == table);
        if !same {
            if tables >= MAX_TABLES {
                truncated = true;
                break;
            }
            tables += 1;
            if schemas.last().is_none_or(|s| s.name != schema) {
                schemas.push(Schema {
                    name: schema.clone(),
                    tables: Vec::new(),
                });
            }
            if let Some(s) = schemas.last_mut() {
                s.tables.push(Table {
                    name: table.clone(),
                    kind,
                    columns: Vec::new(),
                });
            }
            last = Some((schema, table));
        }
        if let (Some(column), Some(table)) =
            (column, schemas.last_mut().and_then(|s| s.tables.last_mut()))
        {
            table.columns.push(column);
        }
    }
    Ok(Catalog { schemas, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Cell {
        Some(v.to_owned())
    }

    fn row(
        schema: &str,
        table: &str,
        kind: &str,
        col: Option<(&str, &str, &str, &str)>,
    ) -> Vec<Cell> {
        let mut r = vec![s(schema), s(table), s(kind)];
        match col {
            Some((n, t, nul, pk)) => r.extend([s(n), s(t), s(nul), s(pk)]),
            None => r.extend([None, None, None, None]),
        }
        r
    }

    #[test]
    fn groups_rows_into_schemas_and_tables() {
        let rows = vec![
            row("a", "t1", "table", Some(("id", "int", "0", "1"))),
            row("a", "t1", "table", Some(("name", "text", "1", "0"))),
            row("a", "t2", "table", Some(("x", "int", "1", "0"))),
            row("b", "t0", "table", Some(("y", "int", "1", "0"))),
        ];
        let c = from_rows(rows).unwrap();
        assert!(!c.truncated);
        let names: Vec<_> = c.schemas.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(c.schemas[0].tables.len(), 2);
        assert_eq!(c.schemas[0].tables[0].columns.len(), 2);
        assert_eq!(c.schemas[0].tables[1].name, "t2");
        assert_eq!(c.schemas[1].tables[0].name, "t0");
    }

    #[test]
    fn view_kind_and_flags_parse() {
        let rows = vec![
            row("a", "v", "view", Some(("id", "int", "0", "1"))),
            row("a", "v", "view", Some(("n", "text", "1", "0"))),
        ];
        let c = from_rows(rows).unwrap();
        let t = &c.schemas[0].tables[0];
        assert_eq!(t.kind, TableKind::View);
        assert_eq!(
            t.columns[0],
            CatalogColumn {
                name: "id".into(),
                type_name: "int".into(),
                nullable: false,
                pk: true
            }
        );
        assert!(t.columns[1].nullable && !t.columns[1].pk);
    }

    #[test]
    fn zero_column_table_has_no_columns() {
        let c = from_rows(vec![row("a", "empty", "table", None)]).unwrap();
        assert_eq!(c.schemas[0].tables[0].name, "empty");
        assert!(c.schemas[0].tables[0].columns.is_empty());
    }

    fn many(n: usize) -> Vec<Vec<Cell>> {
        (0..n)
            .map(|i| row("a", &format!("t{i}"), "table", Some(("c", "int", "1", "0"))))
            .collect()
    }

    #[test]
    fn more_than_max_tables_truncates() {
        let c = from_rows(many(MAX_TABLES + 1)).unwrap();
        assert_eq!(c.schemas[0].tables.len(), MAX_TABLES);
        assert!(c.truncated);
    }

    #[test]
    fn exactly_max_tables_is_not_truncated() {
        let c = from_rows(many(MAX_TABLES)).unwrap();
        assert_eq!(c.schemas[0].tables.len(), MAX_TABLES);
        assert!(!c.truncated);
    }

    #[test]
    fn bad_row_is_an_error_without_contents() {
        let short = vec![s("secret-schema"), s("t")];
        let kind = row("secret-schema", "t", "sequence", None);
        for bad in [short, kind] {
            let e = from_rows(vec![bad]).unwrap_err();
            assert_eq!(e, "unexpected catalog row");
            assert!(!e.contains("secret"));
        }
    }
}
