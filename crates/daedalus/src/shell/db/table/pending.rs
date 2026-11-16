use std::collections::{BTreeMap, BTreeSet};

use daedalus_db::edit::{Change, Shape};
use daedalus_db::value::Cell;
use daedalus_ui::{GridMarks, RowMark};

const MAX_EDIT_BYTES: usize = 4096;
const MAX_PANEL_EDIT_BYTES: usize = 64 * 1024;

/// Unsubmitted edits keyed by grid row; page rows are `0..page_len`, inserted rows follow.
#[derive(Default)]
pub(super) struct Pending {
    cells: BTreeMap<(usize, usize), Cell>,
    deleted: BTreeSet<usize>,
    inserted: Vec<BTreeMap<usize, Cell>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    Page(usize),
    Inserted(usize),
}

pub(super) fn locate(row: usize, page_len: usize) -> Slot {
    match row < page_len {
        true => Slot::Page(row),
        false => Slot::Inserted(row - page_len),
    }
}

/// Multi-line and long values stay read-only so the single-line cell editor never mangles them.
pub(super) fn can_edit_value(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.len() <= MAX_EDIT_BYTES && !v.contains(['\n', '\r']))
}

/// Values the value panel shows in full can be edited there, line breaks included.
pub(super) fn can_edit_in_panel(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.len() <= MAX_PANEL_EDIT_BYTES)
}

impl Pending {
    pub fn set(&mut self, row: usize, col: usize, value: Cell, original: &Cell) {
        match value == *original {
            true => self.cells.remove(&(row, col)),
            false => self.cells.insert((row, col), value),
        };
    }

    pub fn set_inserted(&mut self, i: usize, col: usize, value: Cell) {
        let Some(row) = self.inserted.get_mut(i) else {
            return;
        };
        match value {
            Some(_) => row.insert(col, value),
            None => row.remove(&col),
        };
    }

    pub fn revert_cell(&mut self, row: usize, col: usize) {
        self.cells.remove(&(row, col));
    }

    /// Drops a page row's cell edits and its delete mark.
    pub fn revert_row(&mut self, row: usize) {
        self.cells.retain(|&(r, _), _| r != row);
        self.deleted.remove(&row);
    }

    /// Marks a page row deleted and forgets its cell edits.
    pub fn delete_row(&mut self, row: usize) {
        self.cells.retain(|&(r, _), _| r != row);
        self.deleted.insert(row);
    }

    pub fn delete_inserted(&mut self, i: usize) {
        if i < self.inserted.len() {
            self.inserted.remove(i);
        }
    }

    pub fn add_row(&mut self) -> usize {
        self.inserted.push(BTreeMap::new());
        self.inserted.len() - 1
    }

    pub fn count(&self) -> usize {
        let edited: BTreeSet<usize> = self.cells.keys().map(|&(r, _)| r).collect();
        edited.len() + self.deleted.len() + self.inserted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn display(&self, row: usize, col: usize) -> Option<&Cell> {
        self.cells.get(&(row, col))
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.deleted.contains(&row)
    }

    pub fn is_cell_pending(&self, row: usize, col: usize) -> bool {
        self.cells.contains_key(&(row, col))
    }

    pub fn is_row_pending(&self, row: usize) -> bool {
        self.deleted.contains(&row) || self.cells.keys().any(|&(r, _)| r == row)
    }

    pub fn inserted_value(&self, i: usize, col: usize) -> Cell {
        self.inserted
            .get(i)
            .and_then(|r| r.get(&col))
            .cloned()
            .flatten()
    }

    pub fn inserted_rows(&self, width: usize) -> Vec<Vec<Cell>> {
        (0..self.inserted.len())
            .map(|i| (0..width).map(|c| self.inserted_value(i, c)).collect())
            .collect()
    }

    pub fn edited_cells(&self) -> Vec<(usize, usize)> {
        self.cells.keys().copied().collect()
    }

    /// Deletes, then updates, then inserts; the second vec is each change's grid row.
    pub fn changes(
        &self,
        rows: &[Vec<Cell>],
        map: &[Option<usize>],
        shape: &Shape,
    ) -> (Vec<Change>, Vec<usize>) {
        let pk_of = |row: usize| -> Vec<Cell> {
            shape
                .pk
                .iter()
                .map(|c| {
                    map.iter()
                        .position(|m| *m == Some(*c))
                        .and_then(|rc| rows.get(row)?.get(rc).cloned())
                        .unwrap_or(None)
                })
                .collect()
        };
        let mut changes = Vec::new();
        let mut at = Vec::new();
        for &row in &self.deleted {
            changes.push(Change::Delete { pk: pk_of(row) });
            at.push(row);
        }
        let mut updates: BTreeMap<usize, Vec<(usize, Cell)>> = BTreeMap::new();
        for (&(row, col), value) in &self.cells {
            if let Some(Some(c)) = map.get(col) {
                updates.entry(row).or_default().push((*c, value.clone()));
            }
        }
        for (row, set) in updates {
            changes.push(Change::Update {
                pk: pk_of(row),
                set,
            });
            at.push(row);
        }
        for (i, row) in self.inserted.iter().enumerate() {
            let values = row
                .iter()
                .filter(|(_, v)| v.is_some())
                .filter_map(|(&col, v)| Some((map.get(col).copied().flatten()?, v.clone())))
                .collect();
            changes.push(Change::Insert { values });
            at.push(rows.len() + i);
        }
        (changes, at)
    }

    pub fn marks(&self, page_len: usize) -> GridMarks {
        let mut marks = GridMarks {
            modified: self.cells.keys().copied().collect(),
            ..GridMarks::default()
        };
        for &row in &self.deleted {
            marks.rows.insert(row, RowMark::Deleted);
        }
        for i in 0..self.inserted.len() {
            marks.rows.insert(page_len + i, RowMark::Inserted);
        }
        marks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Cell {
        Some(s.to_owned())
    }

    fn shape() -> Shape {
        Shape {
            pk: vec![0],
            editable: vec![true, true, true],
        }
    }

    fn rows() -> Vec<Vec<Cell>> {
        vec![
            vec![text("1"), text("a"), text("x")],
            vec![text("2"), text("b"), text("y")],
            vec![text("3"), text("c"), text("z")],
        ]
    }

    const IDENTITY: [Option<usize>; 3] = [Some(0), Some(1), Some(2)];

    #[test]
    fn edit_back_to_original_removes_it() {
        let mut p = Pending::default();
        p.set(0, 1, text("new"), &text("a"));
        assert_eq!(p.count(), 1);
        p.set(0, 1, text("a"), &text("a"));
        assert!(p.is_empty());
    }

    #[test]
    fn edits_on_one_row_make_one_update() {
        let mut p = Pending::default();
        p.set(1, 1, text("n"), &text("b"));
        p.set(1, 2, text("m"), &text("y"));
        let (changes, _) = p.changes(&rows(), &IDENTITY, &shape());
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Update { pk, set } => {
                assert_eq!(pk, &[text("2")]);
                assert_eq!(set, &[(1, text("n")), (2, text("m"))]);
            }
            _ => panic!("expected an update"),
        }
    }

    #[test]
    fn changes_order_deletes_updates_inserts() {
        let mut p = Pending::default();
        let i = p.add_row();
        p.set_inserted(i, 1, text("q"));
        p.set(1, 1, text("n"), &text("b"));
        p.delete_row(2);
        let (changes, _) = p.changes(&rows(), &IDENTITY, &shape());
        let kinds: Vec<_> = changes
            .iter()
            .map(|c| match c {
                Change::Delete { .. } => 'd',
                Change::Update { .. } => 'u',
                Change::Insert { .. } => 'i',
            })
            .collect();
        assert_eq!(kinds, ['d', 'u', 'i']);
    }

    #[test]
    fn deleting_a_row_drops_its_cell_edits() {
        let mut p = Pending::default();
        p.set(1, 1, text("n"), &text("b"));
        p.delete_row(1);
        assert_eq!(p.count(), 1);
        assert!(p.display(1, 1).is_none());
        assert!(p.is_deleted(1));
        p.revert_row(1);
        assert!(p.is_empty());
    }

    #[test]
    fn deleting_an_inserted_row_removes_it() {
        let mut p = Pending::default();
        let i = p.add_row();
        p.set_inserted(i, 1, text("q"));
        p.delete_inserted(i);
        assert!(p.is_empty());
        assert!(p.marks(3).rows.is_empty());
    }

    #[test]
    fn set_null_and_set_empty_differ() {
        let mut p = Pending::default();
        p.set(0, 1, None, &text("a"));
        assert_eq!(p.display(0, 1), Some(&None));
        p.set(0, 1, text(""), &text("a"));
        assert_eq!(p.display(0, 1), Some(&text("")));
        p.set(0, 1, text(""), &None);
        assert_eq!(p.display(0, 1), Some(&text("")));
        p.set(0, 1, None, &None);
        assert!(p.is_empty());
    }

    #[test]
    fn pk_values_are_the_originals_even_when_pk_is_edited() {
        let mut p = Pending::default();
        p.set(0, 0, text("99"), &text("1"));
        let (changes, _) = p.changes(&rows(), &IDENTITY, &shape());
        match &changes[0] {
            Change::Update { pk, set } => {
                assert_eq!(pk, &[text("1")]);
                assert_eq!(set, &[(0, text("99"))]);
            }
            _ => panic!("expected an update"),
        }
    }

    #[test]
    fn rows_of_maps_each_change_to_its_grid_row() {
        let mut p = Pending::default();
        let i = p.add_row();
        p.set_inserted(i, 1, text("q"));
        p.set(0, 1, text("n"), &text("a"));
        p.delete_row(2);
        let (_, at) = p.changes(&rows(), &IDENTITY, &shape());
        assert_eq!(at, [2, 0, 3]);
    }

    #[test]
    fn count_counts_rows_not_cells() {
        let mut p = Pending::default();
        p.set(0, 1, text("n"), &text("a"));
        p.set(0, 2, text("m"), &text("x"));
        p.set(1, 1, text("o"), &text("b"));
        p.delete_row(2);
        p.add_row();
        assert_eq!(p.count(), 4);
    }

    #[test]
    fn marks_match_pending_state() {
        let mut p = Pending::default();
        p.set(0, 1, text("n"), &text("a"));
        p.delete_row(1);
        p.add_row();
        let marks = p.marks(3);
        assert!(marks.modified.contains(&(0, 1)));
        assert_eq!(marks.modified.len(), 1);
        assert_eq!(marks.rows.get(&1), Some(&RowMark::Deleted));
        assert_eq!(marks.rows.get(&3), Some(&RowMark::Inserted));
        assert_eq!(marks.failed, None);
    }

    #[test]
    fn inserts_leave_null_cells_out_and_map_to_catalog_columns() {
        let mut p = Pending::default();
        let i = p.add_row();
        p.set_inserted(i, 0, text("7"));
        p.set_inserted(i, 1, None);
        let map = [Some(2), Some(1), Some(0)];
        let (changes, _) = p.changes(&rows(), &map, &shape());
        match &changes[0] {
            Change::Insert { values } => assert_eq!(values, &[(2, text("7"))]),
            _ => panic!("expected an insert"),
        }
    }

    #[test]
    fn edit_requested_refuses_multiline_and_long_values() {
        assert!(can_edit_value(None));
        assert!(can_edit_value(Some("")));
        assert!(can_edit_value(Some("plain")));
        assert!(!can_edit_value(Some("a\nb")));
        assert!(!can_edit_value(Some("a\rb")));
        let long = String::from_utf8(vec![b'x'; 4097]).unwrap_or_default();
        assert!(!can_edit_value(Some(&long)));
        let exact = String::from_utf8(vec![b'x'; 4096]).unwrap_or_default();
        assert!(can_edit_value(Some(&exact)));
    }

    #[test]
    fn panel_edit_takes_line_breaks_and_long_values_up_to_the_cap() {
        assert!(can_edit_in_panel(None));
        assert!(can_edit_in_panel(Some("a\nb")));
        assert!(can_edit_in_panel(Some("a\r\nb")));
        let bytes = |n: usize| String::from_utf8(vec![b'x'; n]).unwrap_or_default();
        assert!(can_edit_in_panel(Some(&bytes(4097))));
        assert!(can_edit_in_panel(Some(&bytes(65536))));
        assert!(!can_edit_in_panel(Some(&bytes(65537))));
        assert!(!can_edit_value(Some("a\nb")));
    }
}
