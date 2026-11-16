//! Row-at-a-time writers that turn a result into CSV, TSV, JSON or SQL INSERT text.

use std::io::{self, Write};

use crate::config::Engine;
use crate::edit::quote_ident;
use crate::value::{Cell, Column};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    Insert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Float,
    Decimal,
    Bool,
    Binary,
    Text,
}

impl Kind {
    fn of(engine: Engine, type_name: &str) -> Kind {
        let lower = type_name.to_ascii_lowercase();
        let base = lower.split('(').next().unwrap_or("");
        let base = base.replace("unsigned", "");
        let base = base.trim();
        if base.starts_with('_') || lower.ends_with("[]") {
            return Kind::Text;
        }
        match base {
            "smallint" | "integer" | "bigint" | "int2" | "int4" | "int8" | "int" | "tinyint"
            | "mediumint" | "serial" | "smallserial" | "bigserial" | "tiny" | "short" | "long"
            | "longlong" | "int24" => Kind::Int,
            "real" | "float4" | "float8" | "double" | "double precision" | "float" => Kind::Float,
            "numeric" | "decimal" | "newdecimal" => Kind::Decimal,
            "bool" | "boolean" => Kind::Bool,
            "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" => {
                Kind::Binary
            }
            _ if engine == Engine::Sqlite => {
                if base.contains("int") {
                    Kind::Int
                } else if base.contains("real") || base.contains("floa") || base.contains("doub") {
                    Kind::Float
                } else if base.contains("blob") {
                    Kind::Binary
                } else {
                    Kind::Text
                }
            }
            _ => Kind::Text,
        }
    }
}

/// Streams rows into `out` in one format; only the current row is ever held in memory.
pub struct Exporter<W: Write> {
    format: Format,
    engine: Engine,
    names: Vec<String>,
    kinds: Vec<Kind>,
    insert_head: String,
    rows: usize,
    out: W,
}

impl<W: Write> Exporter<W> {
    /// `table` is the already-quoted, qualified INSERT target; `None` uses the placeholder `result`.
    pub fn new(
        format: Format,
        engine: Engine,
        table: Option<String>,
        columns: &[Column],
        out: W,
    ) -> io::Result<Self> {
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let kinds = columns
            .iter()
            .map(|c| Kind::of(engine, &c.type_name))
            .collect();
        let insert_head = if format == Format::Insert {
            let target = table.unwrap_or_else(|| quote_ident(engine, "result"));
            let cols: Vec<String> = names.iter().map(|n| quote_ident(engine, n)).collect();
            format!("INSERT INTO {target} ({}) VALUES (", cols.join(", "))
        } else {
            String::new()
        };
        let mut exporter = Exporter {
            format,
            engine,
            names,
            kinds,
            insert_head,
            rows: 0,
            out,
        };
        match format {
            Format::Csv => {
                let line: Vec<String> = exporter.names.iter().map(|n| csv_field(n)).collect();
                write!(exporter.out, "{}\r\n", line.join(","))?;
            }
            Format::Tsv => {
                let line: Vec<String> = exporter.names.iter().map(|n| tsv_field(n)).collect();
                writeln!(exporter.out, "{}", line.join("\t"))?;
            }
            Format::Json | Format::Insert => {}
        }
        Ok(exporter)
    }

    pub fn row(&mut self, row: &[Cell]) -> io::Result<()> {
        let cell = |i: usize| row.get(i).and_then(|c| c.as_deref());
        let count = self.names.len();
        match self.format {
            Format::Csv => {
                let line: Vec<String> = (0..count)
                    .map(|i| cell(i).map(csv_field_value).unwrap_or_default())
                    .collect();
                write!(self.out, "{}\r\n", line.join(","))?;
            }
            Format::Tsv => {
                let line: Vec<String> = (0..count)
                    .map(|i| cell(i).map(tsv_field).unwrap_or_default())
                    .collect();
                writeln!(self.out, "{}", line.join("\t"))?;
            }
            Format::Json => {
                self.out
                    .write_all(if self.rows == 0 { b"[\n  {" } else { b",\n  {" })?;
                for i in 0..count {
                    if i > 0 {
                        self.out.write_all(b", ")?;
                    }
                    write!(
                        self.out,
                        "{}: {}",
                        json_string(&self.names[i]),
                        json_value(self.kinds[i], cell(i))
                    )?;
                }
                self.out.write_all(b"}")?;
            }
            Format::Insert => {
                let vals: Vec<String> = (0..count)
                    .map(|i| sql_value(self.engine, self.kinds[i], cell(i)))
                    .collect();
                writeln!(self.out, "{}{});", self.insert_head, vals.join(", "))?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Writes any closing text, flushes and gives the writer back.
    pub fn finish(mut self) -> io::Result<W> {
        if self.format == Format::Json {
            self.out
                .write_all(if self.rows == 0 { b"[]\n" } else { b"\n]\n" })?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Renders a whole in-memory result in one go, for Copy as….
pub fn to_string(
    format: Format,
    engine: Engine,
    table: Option<String>,
    columns: &[Column],
    rows: &[Vec<Cell>],
) -> String {
    let run = || -> io::Result<Vec<u8>> {
        let mut exporter = Exporter::new(format, engine, table, columns, Vec::new())?;
        for row in rows {
            exporter.row(row)?;
        }
        exporter.finish()
    };
    String::from_utf8(run().unwrap_or_default()).unwrap_or_default()
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn csv_field_value(value: &str) -> String {
    if value.is_empty() {
        "\"\"".to_string()
    } else {
        csv_field(value)
    }
}

fn tsv_field(value: &str) -> String {
    if value.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Optional sign, digits, optional `.digits`, optional exponent; nothing else.
fn strict_number(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    digits(int)
        && frac.is_none_or(digits)
        && exp.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)))
}

fn json_number(s: &str) -> bool {
    let unsigned = s.strip_prefix('-').unwrap_or(s);
    let int = unsigned.split(['.', 'e', 'E']).next().unwrap_or("");
    let leading_zero = int.len() > 1 && int.starts_with('0');
    strict_number(s)
        && !s.starts_with('+')
        && !leading_zero
        && s.parse::<f64>().is_ok_and(f64::is_finite)
}

fn bool_of(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "t" | "true" | "1" => Some(true),
        "f" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn hex_body(s: &str) -> Option<&str> {
    let body = s.strip_prefix("\\x")?;
    (body.len() % 2 == 0 && body.bytes().all(|b| b.is_ascii_hexdigit())).then_some(body)
}

fn json_value(kind: Kind, cell: Option<&str>) -> String {
    let Some(text) = cell else {
        return "null".into();
    };
    match kind {
        Kind::Int | Kind::Float if json_number(text) => text.to_string(),
        Kind::Bool => match bool_of(text) {
            Some(b) => b.to_string(),
            None => json_string(text),
        },
        _ => json_string(text),
    }
}

fn sql_string(engine: Engine, s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if engine == Engine::Mysql => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn sql_value(engine: Engine, kind: Kind, cell: Option<&str>) -> String {
    let Some(text) = cell else {
        return "NULL".into();
    };
    match kind {
        Kind::Int | Kind::Float | Kind::Decimal if strict_number(text) => text.to_string(),
        Kind::Bool => match (bool_of(text), engine) {
            (Some(true), Engine::Sqlite) => "1".into(),
            (Some(false), Engine::Sqlite) => "0".into(),
            (Some(true), _) => "TRUE".into(),
            (Some(false), _) => "FALSE".into(),
            (None, _) => sql_string(engine, text),
        },
        Kind::Binary => match hex_body(text) {
            Some(body) if engine == Engine::Postgres => format!("'\\x{body}'::bytea"),
            Some(body) => format!("X'{body}'"),
            None => sql_string(engine, text),
        },
        _ => sql_string(engine, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.into(),
            type_name: ty.into(),
            table: None,
        }
    }

    fn s(v: &str) -> Cell {
        Some(v.into())
    }

    fn run(format: Format, engine: Engine, cols: &[Column], rows: &[Vec<Cell>]) -> String {
        to_string(format, engine, None, cols, rows)
    }

    #[test]
    fn csv_quotes_and_null() {
        let cols = [col("a", "text"), col("b,x", "text")];
        let rows = vec![
            vec![s("x,y"), s("say \"hi\"")],
            vec![s("l1\nl2"), None],
            vec![s(""), s("plain")],
        ];
        assert_eq!(
            run(Format::Csv, Engine::Postgres, &cols, &rows),
            "a,\"b,x\"\r\n\"x,y\",\"say \"\"hi\"\"\"\r\n\"l1\nl2\",\r\n\"\",plain\r\n"
        );
    }

    #[test]
    fn tsv_follows_grid_rules() {
        let cols = [col("a", "text"), col("b", "text")];
        let rows = vec![vec![s("a\tb"), None], vec![s("q\"q"), s("")]];
        assert_eq!(
            run(Format::Tsv, Engine::Postgres, &cols, &rows),
            "a\tb\n\"a\tb\"\t\n\"q\"\"q\"\t\n"
        );
    }

    #[test]
    fn json_typed_values() {
        let cols = [
            col("i", "int4"),
            col("f", "float8"),
            col("g", "float8"),
            col("b", "bool"),
            col("c", "bool"),
            col("d", "numeric"),
            col("n", "text"),
        ];
        let rows = vec![vec![
            s("42"),
            s("1.5"),
            s("NaN"),
            s("t"),
            s("f"),
            s("1.10"),
            None,
        ]];
        assert_eq!(
            run(Format::Json, Engine::Postgres, &cols, &rows),
            "[\n  {\"i\": 42, \"f\": 1.5, \"g\": \"NaN\", \"b\": true, \"c\": false, \"d\": \"1.10\", \"n\": null}\n]\n"
        );
    }

    #[test]
    fn json_empty_and_multiple_rows() {
        let cols = [col("a", "int4"), col("a", "text")];
        assert_eq!(run(Format::Json, Engine::Postgres, &cols, &[]), "[]\n");
        let rows = vec![vec![s("1"), s("x")], vec![s("01"), s("y\"")]];
        assert_eq!(
            run(Format::Json, Engine::Postgres, &cols, &rows),
            "[\n  {\"a\": 1, \"a\": \"x\"},\n  {\"a\": \"01\", \"a\": \"y\\\"\"}\n]\n"
        );
    }

    #[test]
    fn insert_per_engine() {
        let cols = [col("id", "int4"), col("n\"x", "text")];
        let rows = vec![vec![s("1"), s("o'k\\")]];
        assert_eq!(
            run(Format::Insert, Engine::Postgres, &cols, &rows),
            "INSERT INTO \"result\" (\"id\", \"n\"\"x\") VALUES (1, 'o''k\\');\n"
        );
        assert_eq!(
            run(Format::Insert, Engine::Sqlite, &cols, &rows),
            "INSERT INTO \"result\" (\"id\", \"n\"\"x\") VALUES (1, 'o''k\\');\n"
        );
        let cols = [col("id", "long"), col("n`x", "var_string")];
        assert_eq!(
            run(Format::Insert, Engine::Mysql, &cols, &rows),
            "INSERT INTO `result` (`id`, `n``x`) VALUES (1, 'o''k\\\\');\n"
        );
    }

    #[test]
    fn insert_bool_binary_and_injection() {
        let cols = [col("b", "bool"), col("x", "bytea"), col("n", "int4")];
        let rows = vec![vec![s("t"), s("\\xdead"), s("1; DROP TABLE t")]];
        let mut out = String::new();
        for engine in [Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
            out.push_str(&to_string(
                Format::Insert,
                engine,
                Some("t".into()),
                &cols,
                &rows,
            ));
        }
        let q = |c: &str| format!("INSERT INTO t ({c}) VALUES ");
        let pg = q("\"b\", \"x\", \"n\"") + "(TRUE, '\\xdead'::bytea, '1; DROP TABLE t');\n";
        let my = q("`b`, `x`, `n`") + "(TRUE, X'dead', '1; DROP TABLE t');\n";
        let lite = q("\"b\", \"x\", \"n\"") + "(1, X'dead', '1; DROP TABLE t');\n";
        assert_eq!(out, format!("{pg}{my}{lite}"));
    }

    #[test]
    fn insert_null_and_bad_hex() {
        let cols = [col("x", "blob"), col("y", "int")];
        let rows = vec![vec![s("not hex"), None]];
        assert_eq!(
            run(Format::Insert, Engine::Sqlite, &cols, &rows),
            "INSERT INTO \"result\" (\"x\", \"y\") VALUES ('not hex', NULL);\n"
        );
    }

    #[test]
    fn streaming_matches_to_string() {
        let cols = [col("a", "int4"), col("b", "text")];
        let rows = vec![vec![s("1"), s("x")], vec![None, s("y")]];
        for format in [Format::Csv, Format::Tsv, Format::Json, Format::Insert] {
            let mut e = Exporter::new(format, Engine::Postgres, None, &cols, Vec::new()).unwrap();
            e.row(&rows[0]).unwrap();
            e.row(&rows[1]).unwrap();
            let bytes = e.finish().unwrap();
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                run(format, Engine::Postgres, &cols, &rows)
            );
        }
    }
}
