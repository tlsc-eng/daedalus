use std::ops::Range;

use gpui::SharedString;

/// Rows still unloaded below the viewport that trigger a fetch.
pub(crate) const FETCH_AHEAD: usize = 100;
pub(crate) const DISPLAY_BYTES: usize = 4096;
/// Fits "NULL".
pub(crate) const MIN_CHARS: usize = 4;
/// Long values stop counting here so one huge cell cannot stretch a column past its cap.
pub(crate) const MEASURE_CHARS: usize = 120;

/// One result column: its header text, a hover label for the type, and whether values align right.
#[derive(Clone, Debug, PartialEq)]
pub struct GridColumn {
    pub name: SharedString,
    pub type_label: SharedString,
    pub numeric: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellPos {
    pub row: usize,
    pub col: usize,
}

type Row = Vec<Option<SharedString>>;

#[derive(Default)]
pub(crate) struct GridModel {
    pub columns: Vec<GridColumn>,
    pub rows: Vec<Row>,
    pub more: bool,
    pub truncated: bool,
    pub chars: Vec<usize>,
    pub anchor: Option<CellPos>,
    pub active: Option<CellPos>,
    fetch_requested: bool,
}

fn shared(rows: Vec<Vec<Option<String>>>) -> Vec<Row> {
    rows.into_iter()
        .map(|r| r.into_iter().map(|c| c.map(SharedString::from)).collect())
        .collect()
}

impl GridModel {
    pub fn show(&mut self, columns: Vec<GridColumn>, rows: Vec<Vec<Option<String>>>, more: bool) {
        self.rows = shared(rows);
        self.chars = column_chars(&columns, &self.rows);
        self.columns = columns;
        self.more = more;
        self.truncated = false;
        self.anchor = None;
        self.active = None;
        self.fetch_requested = false;
    }

    pub fn append(&mut self, rows: Vec<Vec<Option<String>>>, more: bool) {
        self.rows.extend(shared(rows));
        self.more = more;
        self.fetch_requested = false;
    }

    pub fn finish(&mut self, truncated: bool) {
        self.truncated = truncated;
        self.more = false;
    }

    pub fn select(&mut self, pos: CellPos, extend: bool) {
        if !extend || self.anchor.is_none() {
            self.anchor = Some(pos);
        }
        self.active = Some(pos);
    }

    pub fn select_row(&mut self, row: usize, extend: bool) {
        let Some(last) = self.columns.len().checked_sub(1) else {
            return;
        };
        let anchor_row = match (extend, self.anchor) {
            (true, Some(a)) => a.row,
            _ => row,
        };
        self.anchor = Some(CellPos {
            row: anchor_row,
            col: 0,
        });
        self.active = Some(CellPos { row, col: last });
    }

    pub fn select_all(&mut self) {
        if self.rows.is_empty() || self.columns.is_empty() {
            return;
        }
        self.anchor = Some(CellPos { row: 0, col: 0 });
        self.active = Some(CellPos {
            row: self.rows.len() - 1,
            col: self.columns.len() - 1,
        });
    }

    pub fn step(&mut self, d_row: isize, d_col: isize, extend: bool) -> Option<CellPos> {
        let (last_row, last_col) = (
            self.rows.len().checked_sub(1)?,
            self.columns.len().checked_sub(1)?,
        );
        let next = match self.active {
            None => CellPos { row: 0, col: 0 },
            Some(p) => CellPos {
                row: p.row.saturating_add_signed(d_row).min(last_row),
                col: p.col.saturating_add_signed(d_col).min(last_col),
            },
        };
        self.select(next, extend);
        Some(next)
    }

    pub fn selection(&self) -> Option<(Range<usize>, Range<usize>)> {
        let (a, b) = (self.anchor?, self.active?);
        Some((
            a.row.min(b.row)..a.row.max(b.row) + 1,
            a.col.min(b.col)..a.col.max(b.col) + 1,
        ))
    }

    pub fn copy_text(&self) -> Option<String> {
        let (rows, cols) = self.selection()?;
        let cell = |r: usize, c: usize| {
            self.rows
                .get(r)
                .and_then(|row| row.get(c))
                .and_then(|v| v.as_ref())
                .map(|v| v.as_ref())
                .unwrap_or("")
        };
        if rows.len() == 1 && cols.len() == 1 {
            return Some(cell(rows.start, cols.start).to_string());
        }
        let lines: Vec<String> = rows
            .map(|r| {
                cols.clone()
                    .map(|c| tsv_field(cell(r, c)))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        Some(lines.join("\n"))
    }

    pub fn wants_more(&mut self, visible_end: usize) -> bool {
        let ask =
            self.more && !self.fetch_requested && visible_end + FETCH_AHEAD >= self.rows.len();
        if ask {
            self.fetch_requested = true;
        }
        ask
    }
}

fn tsv_field(value: &str) -> String {
    if value.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub(crate) fn column_chars(columns: &[GridColumn], rows: &[Row]) -> Vec<usize> {
    let count = |s: &str| s.chars().take(MEASURE_CHARS).count();
    columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let cells = rows.iter().map(|row| match row.get(c) {
                Some(Some(v)) => count(&display_text(v)),
                _ => MIN_CHARS,
            });
            cells.fold(count(&col.name), usize::max).max(MIN_CHARS)
        })
        .collect()
}

pub(crate) fn column_width(chars: usize, char_w: f32, pad: f32, cap: f32) -> f32 {
    (chars as f32 * char_w + 2. * pad).min(cap)
}

pub(crate) fn gutter_chars(rows: usize) -> usize {
    rows.max(1).to_string().len().max(3)
}

pub(crate) fn display_text(value: &SharedString) -> SharedString {
    let flat = |c: char| matches!(c, '\n' | '\r' | '\t');
    if value.len() <= DISPLAY_BYTES && !value.contains(flat) {
        return value.clone();
    }
    let mut end = value.len().min(DISPLAY_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end]
        .chars()
        .map(|c| if flat(c) { ' ' } else { c })
        .collect::<String>()
        .into()
}

pub(crate) fn visible_columns(lefts: &[f32], scroll_x: f32, width: f32) -> Range<usize> {
    let cols = lefts.len().saturating_sub(1);
    let start = lefts
        .get(1..)
        .unwrap_or(&[])
        .partition_point(|&r| r <= scroll_x);
    let end = lefts[..cols].partition_point(|&l| l < scroll_x + width);
    start..end.max(start)
}

pub(crate) fn reveal_x(scroll_x: f32, left: f32, right: f32, width: f32) -> f32 {
    if left < scroll_x {
        left
    } else if right > scroll_x + width {
        (right - width).min(left)
    } else {
        scroll_x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(n: usize) -> Vec<GridColumn> {
        (0..n)
            .map(|i| GridColumn {
                name: format!("c{i}").into(),
                type_label: "".into(),
                numeric: false,
            })
            .collect()
    }

    fn cell(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    fn grid(rows: usize, columns: usize, more: bool) -> GridModel {
        let mut m = GridModel::default();
        let data = (0..rows)
            .map(|r| (0..columns).map(|c| cell(&format!("{r}-{c}"))).collect())
            .collect();
        m.show(cols(columns), data, more);
        m
    }

    fn pos(row: usize, col: usize) -> CellPos {
        CellPos { row, col }
    }

    #[test]
    fn column_width_caps() {
        assert_eq!(column_width(200, 7., 6., 400.), 400.);
        assert_eq!(column_width(5, 7., 6., 400.), 47.);
    }

    #[test]
    fn column_chars_take_header_and_widest_cell() {
        let named = |n: &str| GridColumn {
            name: n.to_string().into(),
            type_label: "".into(),
            numeric: false,
        };
        let columns = vec![
            named("a_long_header"),
            named("b"),
            named("c"),
            named("d"),
            named("e"),
            named("f"),
        ];
        let long = "x".repeat(5000);
        let row: Vec<Option<String>> = vec![
            cell("ab"),
            cell("ten chars!"),
            None,
            cell("héllo"),
            cell("x"),
            Some(long),
        ];
        let mut m = GridModel::default();
        m.show(columns, vec![row], false);
        assert_eq!(m.chars, vec![13, 10, 4, 5, 4, MEASURE_CHARS]);
    }

    #[test]
    fn copy_selection_as_tsv_escapes_tabs_and_newlines() {
        let mut m = GridModel::default();
        m.show(
            cols(2),
            vec![
                vec![cell("a\tb"), cell("x")],
                vec![cell("line1\nline2"), None],
            ],
            false,
        );
        m.select_all();
        assert_eq!(
            m.copy_text().unwrap(),
            "\"a\tb\"\tx\n\"line1\nline2\"\t".to_string()
        );
        m.show(cols(2), vec![vec![cell("say \"hi\""), cell("y")]], false);
        m.select_all();
        assert_eq!(m.copy_text().unwrap(), "\"say \"\"hi\"\"\"\ty");
    }

    #[test]
    fn copy_single_cell_is_the_raw_value() {
        let mut m = GridModel::default();
        m.show(cols(2), vec![vec![cell("line1\nline2"), None]], false);
        m.select(pos(0, 0), false);
        assert_eq!(m.copy_text().unwrap(), "line1\nline2");
        m.select(pos(0, 1), false);
        assert_eq!(m.copy_text().unwrap(), "");
    }

    #[test]
    fn copy_without_selection_is_none() {
        assert_eq!(grid(2, 2, false).copy_text(), None);
    }

    #[test]
    fn fetch_more_requested_near_end_once() {
        let mut m = grid(500, 1, true);
        assert!(!m.wants_more(300));
        assert!(m.wants_more(400));
        assert!(!m.wants_more(450));
        m.append(vec![vec![cell("x")]; 500], true);
        assert!(!m.wants_more(450));
        assert!(m.wants_more(900));
    }

    #[test]
    fn fetch_more_never_requested_without_more() {
        let mut m = grid(500, 1, true);
        m.finish(false);
        assert!(!m.wants_more(500));
        assert!(!m.wants_more(0));
    }

    #[test]
    fn step_clamps_to_the_grid_edges() {
        let mut m = grid(3, 2, false);
        assert_eq!(m.step(1, 1, false), Some(pos(0, 0)));
        assert_eq!(m.step(-1, 0, false), Some(pos(0, 0)));
        m.step(0, 1, false);
        assert_eq!(m.step(0, 1, false), Some(pos(0, 1)));
        assert_eq!(m.step(5, 0, false), Some(pos(2, 1)));
    }

    #[test]
    fn shift_step_extends_from_the_anchor() {
        let mut m = grid(5, 5, false);
        m.select(pos(2, 2), false);
        m.step(-1, 0, true);
        m.step(-1, 0, true);
        m.step(0, -1, true);
        assert_eq!(m.selection(), Some((0..3, 1..3)));
    }

    #[test]
    fn select_all_covers_every_loaded_cell() {
        let mut m = grid(4, 3, false);
        m.select_all();
        assert_eq!(m.selection(), Some((0..4, 0..3)));
        let mut empty = GridModel::default();
        empty.select_all();
        assert_eq!(empty.selection(), None);
    }

    #[test]
    fn select_row_spans_all_columns() {
        let mut m = grid(4, 3, false);
        m.select_row(2, false);
        assert_eq!(m.selection(), Some((2..3, 0..3)));
        m.select_row(0, true);
        assert_eq!(m.selection(), Some((0..3, 0..3)));
    }

    #[test]
    fn show_resets_selection_and_widths() {
        let mut m = grid(3, 2, true);
        m.select_all();
        m.show(cols(1), vec![vec![cell("wider than header")]], false);
        assert_eq!(m.selection(), None);
        assert_eq!(m.chars, vec![17]);
        assert!(!m.more);
    }

    #[test]
    fn append_keeps_selection_and_widths() {
        let mut m = grid(3, 2, true);
        m.select(pos(1, 1), false);
        let chars = m.chars.clone();
        m.append(vec![vec![cell("a very long appended value"), None]], false);
        assert_eq!(m.selection(), Some((1..2, 1..2)));
        assert_eq!(m.chars, chars);
        assert_eq!(m.rows.len(), 4);
    }

    #[test]
    fn visible_columns_cover_the_viewport_only() {
        let lefts = [0., 100., 200., 300., 400.];
        assert_eq!(visible_columns(&lefts, 150., 100.), 1..3);
        assert_eq!(visible_columns(&lefts, 0., 1000.), 0..4);
        assert_eq!(visible_columns(&[0.], 0., 100.), 0..0);
    }

    #[test]
    fn reveal_scrolls_the_least_to_show_a_column() {
        assert_eq!(reveal_x(100., 150., 250., 200.), 100.);
        assert_eq!(reveal_x(100., 50., 150., 200.), 50.);
        assert_eq!(reveal_x(100., 250., 400., 200.), 200.);
        assert_eq!(reveal_x(100., 250., 600., 200.), 250.);
    }

    #[test]
    fn display_text_flattens_lines_and_caps_bytes() {
        assert_eq!(display_text(&"a\nb\tc".into()), "a b c");
        assert_eq!(display_text(&"x".repeat(5000).into()).len(), 4096);
        let edge = format!("{}é", "x".repeat(4095));
        assert_eq!(display_text(&edge.into()).len(), 4095);
    }

    #[test]
    fn gutter_fits_the_row_count_digits() {
        assert_eq!(gutter_chars(0), 3);
        assert_eq!(gutter_chars(999), 3);
        assert_eq!(gutter_chars(1000), 4);
        assert_eq!(gutter_chars(100_000), 6);
    }
}
