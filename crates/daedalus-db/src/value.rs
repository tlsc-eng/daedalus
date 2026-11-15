/// A result cell as text; `None` is SQL NULL and stays distinct from the empty string.
pub type Cell = Option<String>;

pub const MAX_ROWS: usize = 100_000;
pub const MAX_BYTES: usize = 128 * 1024 * 1024;

/// Engine-specific id of the table a column comes from, when the driver reports one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableRef {
    pub oid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub table: Option<TableRef>,
}

/// Tracks how much of a result has been kept so a runaway query can be cut off.
#[derive(Debug, Default)]
pub struct Budget {
    rows: usize,
    bytes: usize,
}

impl Budget {
    /// Counts the row and returns false when it would pass a cap, in which case it is not counted.
    pub fn admit(&mut self, row: &[Cell]) -> bool {
        let size: usize = row.iter().flatten().map(String::len).sum();
        if self.rows >= MAX_ROWS || self.bytes + size > MAX_BYTES {
            return false;
        }
        self.rows += 1;
        self.bytes += size;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_cap_truncates() {
        let mut budget = Budget::default();
        let big = vec![Some(String::from_utf8(vec![b'x'; MAX_BYTES / 2]).unwrap())];
        assert!(budget.admit(&big));
        assert!(budget.admit(&big));
        assert!(!budget.admit(&[Some("y".into())]));
    }

    #[test]
    fn row_cap_truncates() {
        let mut budget = Budget::default();
        let row = [None];
        for _ in 0..MAX_ROWS {
            assert!(budget.admit(&row));
        }
        assert!(!budget.admit(&row));
    }
}
