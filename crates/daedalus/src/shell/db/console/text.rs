use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Byte offset of a 0-based line and UTF-16 column, clamped to the line end and the text end.
pub(super) fn byte_at(text: &str, (line, col): (u32, u32)) -> usize {
    let mut start = 0;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(n) => start += n + 1,
            None => return text.len(),
        }
    }
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if c == '\n' || units >= col {
            return start + i;
        }
        units += c.len_utf16() as u32;
    }
    text.len()
}

/// 0-based line and UTF-16 column of a byte offset, clamped to the text and to a char boundary.
pub(super) fn position(text: &str, byte: usize) -> (u32, u32) {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    let before = &text[..byte];
    let line = before.matches('\n').count() as u32;
    let from = before.rfind('\n').map_or(0, |n| n + 1);
    let col = before[from..].chars().map(|c| c.len_utf16() as u32).sum();
    (line, col)
}

/// Document byte of a server error given as a 1-based char position inside the piece that was sent.
pub(super) fn error_byte(sent: &str, start: usize, position: usize) -> usize {
    let at = sent
        .char_indices()
        .nth(position.saturating_sub(1))
        .map_or(sent.len(), |(i, _)| i);
    start + at
}

/// The console file for a project and connection; named by a hash because ids come from editable JSON.
pub(super) fn console_path(data_dir: &Path, root: &Path, key: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(root.to_string_lossy().as_bytes());
    hash.update(b"\n");
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    data_dir.join("consoles").join(format!("{hex}.sql"))
}

/// Whether values of this column type align right.
pub(super) fn numeric(type_name: &str) -> bool {
    let name = type_name.to_ascii_lowercase();
    [
        "int",
        "smallint",
        "bigint",
        "tinyint",
        "mediumint",
        "integer",
        "numeric",
        "decimal",
        "float",
        "double",
        "real",
        "money",
        "oid",
        "serial",
    ]
    .iter()
    .any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_at_counts_utf16_columns() {
        let text = "a😀b\nsecond";
        // the emoji is two UTF-16 units and four bytes
        assert_eq!(byte_at(text, (0, 3)), 5);
        assert_eq!(byte_at(text, (1, 2)), "a😀b\nse".len());
        assert_eq!(byte_at(text, (0, 99)), "a😀b".len());
        assert_eq!(byte_at(text, (9, 0)), text.len());
    }

    #[test]
    fn position_round_trips_byte_at() {
        let text = "a😀b\nsecond\n";
        for byte in [0, 1, 5, 6, 7, 11, 12] {
            assert_eq!(byte_at(text, position(text, byte)), byte);
        }
        assert_eq!(position(text, 999), (2, 0));
        assert_eq!(position(text, 3), (0, 1));
    }

    #[test]
    fn error_byte_counts_chars_not_bytes() {
        assert_eq!(error_byte("é = x", 10, 3), 10 + "é ".len());
        assert_eq!(error_byte("select", 4, 1), 4);
    }

    #[test]
    fn error_byte_past_the_end_clamps() {
        assert_eq!(error_byte("abc", 7, 99), 10);
        assert_eq!(error_byte("abc", 7, 0), 7);
    }

    #[test]
    fn console_path_is_hex_and_differs_per_project() {
        let dir = Path::new("/d");
        let a = console_path(dir, Path::new("/p"), "g:../x");
        let b = console_path(dir, Path::new("/q"), "g:../x");
        assert_ne!(a, b);
        let name = a.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 16);
        assert!(
            name.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(a.parent(), Some(Path::new("/d/consoles")));
    }

    #[test]
    fn numeric_types_align_right() {
        for t in [
            "int4",
            "BIGINT",
            "numeric(10,2)",
            "serial",
            "double precision",
            "money",
        ] {
            assert!(numeric(t), "{t}");
        }
        for t in ["text", "varchar", "timestamp", "uuid", "bool"] {
            assert!(!numeric(t), "{t}");
        }
    }
}
