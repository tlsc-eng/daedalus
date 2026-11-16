//! Row-at-a-time writers that turn a result into CSV, TSV, JSON or SQL INSERT text.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Engine;
use crate::edit::quote_ident;
use crate::value::{Cell, Column};

/// Output format of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    Insert,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::Insert => "sql",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::Insert => "SQL INSERT",
        }
    }
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
            "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary"
            | "bit" => Kind::Binary,
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
    json_keys: Vec<String>,
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
            json_keys: unique_keys(&names),
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

    /// Writes one row; cells beyond the row's length count as NULL.
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
                        json_string(&self.json_keys[i]),
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

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Streams an export into a hidden temp file that replaces the destination only on commit.
pub struct FileExport {
    exporter: Option<Exporter<BufWriter<File>>>,
    temp: PathBuf,
    dest: PathBuf,
    rows: u64,
    committed: bool,
}

impl FileExport {
    pub fn create(
        dest: &Path,
        format: Format,
        engine: Engine,
        table: Option<String>,
        columns: &[Column],
    ) -> io::Result<Self> {
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned());
        let name = name.unwrap_or_else(|| "export".to_string());
        // Beside the destination, because rename is only atomic within one filesystem.
        // The sequence keeps two exports to one destination from deleting each other's temp.
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = dest.with_file_name(format!(".{name}.{}.{seq}.part", std::process::id()));
        let _ = fs::remove_file(&temp);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&temp)?;
        let writer = BufWriter::with_capacity(1 << 16, file);
        let exporter = match Exporter::new(format, engine, table, columns, writer) {
            Ok(exporter) => exporter,
            Err(e) => {
                let _ = fs::remove_file(&temp);
                return Err(e);
            }
        };
        Ok(Self {
            exporter: Some(exporter),
            temp,
            dest: dest.to_path_buf(),
            rows: 0,
            committed: false,
        })
    }

    pub fn write(&mut self, rows: &[Vec<Cell>]) -> io::Result<()> {
        let Some(exporter) = self.exporter.as_mut() else {
            return Ok(());
        };
        for row in rows {
            exporter.row(row)?;
            self.rows += 1;
        }
        Ok(())
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Finishes the file and moves it over the destination, returning the row count.
    pub fn commit(mut self) -> io::Result<u64> {
        let Some(exporter) = self.exporter.take() else {
            return Ok(self.rows);
        };
        let file = exporter
            .finish()?
            .into_inner()
            .map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.temp, &self.dest)?;
        self.committed = true;
        Ok(self.rows)
    }
}

impl Drop for FileExport {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temp);
        }
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

fn unique_keys(names: &[String]) -> Vec<String> {
    let mut taken: std::collections::HashSet<String> = names.iter().cloned().collect();
    let mut seen = std::collections::HashSet::new();
    names
        .iter()
        .map(|name| {
            if seen.insert(name.clone()) {
                return name.clone();
            }
            let key = (2..)
                .map(|n| format!("{name}_{n}"))
                .find(|k| !taken.contains(k))
                .unwrap_or_default();
            taken.insert(key.clone());
            key
        })
        .collect()
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
    let escaped = engine == Engine::Postgres && s.contains('\\');
    let mut out = String::with_capacity(s.len() + 3);
    if escaped {
        out.push('E');
    }
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if escaped || engine == Engine::Mysql => out.push_str("\\\\"),
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
            Some(body) if engine == Engine::Postgres => format!("decode('{body}', 'hex')"),
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

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("daedalus-export-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn file_export_commit_replaces_destination() {
        let dir = scratch("commit");
        let dest = dir.join("out.csv");
        fs::write(&dest, "old").unwrap();
        let mut f = FileExport::create(
            &dest,
            Format::Csv,
            Engine::Sqlite,
            None,
            &[col("a", "text")],
        )
        .unwrap();
        f.write(&[vec![s("x")], vec![s("y")]]).unwrap();
        assert_eq!(f.rows(), 2);
        assert_eq!(f.commit().unwrap(), 2);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "a\r\nx\r\ny\r\n");
        assert_eq!(leftovers(&dir), ["out.csv"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_export_drop_without_commit_keeps_old_file() {
        let dir = scratch("drop");
        let dest = dir.join("out.csv");
        fs::write(&dest, "old").unwrap();
        let mut f = FileExport::create(
            &dest,
            Format::Csv,
            Engine::Sqlite,
            None,
            &[col("a", "text")],
        )
        .unwrap();
        f.write(&[vec![s("x")]]).unwrap();
        drop(f);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
        assert_eq!(leftovers(&dir), ["out.csv"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_exports_to_one_destination_do_not_share_a_temp() {
        let dir = scratch("concurrent");
        let dest = dir.join("out.csv");
        let cols = [col("a", "text")];
        let mut a = FileExport::create(&dest, Format::Csv, Engine::Sqlite, None, &cols).unwrap();
        let mut b = FileExport::create(&dest, Format::Csv, Engine::Sqlite, None, &cols).unwrap();
        a.write(&[vec![s("from-a")]]).unwrap();
        b.write(&[vec![s("from-b")]]).unwrap();
        a.commit().unwrap();
        drop(b);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "a\r\nfrom-a\r\n");
        assert_eq!(leftovers(&dir), ["out.csv"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_export_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let dest = dir.join("out.csv");
        let f = FileExport::create(
            &dest,
            Format::Csv,
            Engine::Sqlite,
            None,
            &[col("a", "text")],
        )
        .unwrap();
        f.commit().unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_and_label_cover_every_format() {
        let all = [
            (Format::Csv, "csv", "CSV"),
            (Format::Tsv, "tsv", "TSV"),
            (Format::Json, "json", "JSON"),
            (Format::Insert, "sql", "SQL INSERT"),
        ];
        for (format, ext, label) in all {
            assert_eq!((format.extension(), format.label()), (ext, label));
        }
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
            "[\n  {\"a\": 1, \"a_2\": \"x\"},\n  {\"a\": \"01\", \"a_2\": \"y\\\"\"}\n]\n"
        );
    }

    #[test]
    fn json_duplicate_keys_skip_taken_suffixes() {
        let cols = [
            col("id", "text"),
            col("id", "text"),
            col("id_2", "text"),
            col("id", "text"),
        ];
        let rows = vec![vec![s("a"), s("b"), s("c"), s("d")]];
        assert_eq!(
            run(Format::Json, Engine::Postgres, &cols, &rows),
            "[\n  {\"id\": \"a\", \"id_3\": \"b\", \"id_2\": \"c\", \"id_4\": \"d\"}\n]\n"
        );
    }

    #[test]
    fn postgres_backslash_uses_escape_string() {
        let cols = [col("t", "text"), col("b", "bytea")];
        let rows = vec![vec![s("a\\b'c"), s("\\xdead")]];
        assert_eq!(
            run(Format::Insert, Engine::Postgres, &cols, &rows),
            "INSERT INTO \"result\" (\"t\", \"b\") VALUES (E'a\\\\b''c', decode('dead', 'hex'));\n"
        );
    }

    #[test]
    fn mysql_text_bit_and_varbinary() {
        let cols = [col("t", "text"), col("v", "varbinary"), col("b", "bit")];
        let rows = vec![vec![s("\\x41"), s("\\xff00"), s("\\x01")]];
        assert_eq!(
            run(Format::Insert, Engine::Mysql, &cols, &rows),
            "INSERT INTO `result` (`t`, `v`, `b`) VALUES ('\\\\x41', X'ff00', X'01');\n"
        );
    }

    #[test]
    fn insert_per_engine() {
        let cols = [col("id", "int4"), col("n\"x", "text")];
        let rows = vec![vec![s("1"), s("o'k\\")]];
        assert_eq!(
            run(Format::Insert, Engine::Postgres, &cols, &rows),
            "INSERT INTO \"result\" (\"id\", \"n\"\"x\") VALUES (1, E'o''k\\\\');\n"
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
        let pg = q("\"b\", \"x\", \"n\"") + "(TRUE, decode('dead', 'hex'), '1; DROP TABLE t');\n";
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

    #[test]
    fn sqlite_storage_classes_type_json_and_insert() {
        let cols = [
            col("n", "integer"),
            col("r", "real"),
            col("b", "blob"),
            col("t", "text"),
            col("z", ""),
        ];
        let rows = vec![vec![s("2"), s("0.5"), s("\\x0a"), s("3"), s("4")]];
        assert_eq!(
            run(Format::Json, Engine::Sqlite, &cols, &rows),
            "[\n  {\"n\": 2, \"r\": 0.5, \"b\": \"\\\\x0a\", \"t\": \"3\", \"z\": \"4\"}\n]\n"
        );
        assert_eq!(
            run(Format::Insert, Engine::Sqlite, &cols, &rows),
            "INSERT INTO \"result\" (\"n\", \"r\", \"b\", \"t\", \"z\") VALUES (2, 0.5, X'0a', '3', '4');\n"
        );
    }
}
