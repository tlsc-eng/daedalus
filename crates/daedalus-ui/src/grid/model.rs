use std::collections::{HashMap, HashSet};
use std::ops::Range;

use gpui::{ScrollStrategy, SharedString};

/// Number of loaded rows left below the visible end at which fetch-more fires.
pub(crate) const FETCH_AHEAD: usize = 100;
pub(crate) const DISPLAY_BYTES: usize = 4096;
/// Fits "NULL".
pub(crate) const MIN_CHARS: usize = 4;
/// Long values stop counting here so one huge cell cannot stretch a column past its cap.
pub(crate) const MEASURE_CHARS: usize = 120;
/// Stops a stray drag from making a column wider than any screen.
pub(crate) const MAX_COL_W: f32 = 2000.;

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

/// How a pending edit changes a whole row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMark {
    Inserted,
    Deleted,
}

/// Pending-edit marks the grid paints; the caller owns what they mean.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct GridMarks {
    pub modified: HashSet<(usize, usize)>,
    pub rows: HashMap<usize, RowMark>,
    pub failed: Option<usize>,
}

impl GridMarks {
    pub(crate) fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.rows.is_empty() && self.failed.is_none()
    }

    pub(crate) fn row_modified(&self, row: usize) -> bool {
        self.modified.iter().any(|&(r, _)| r == row)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Selected,
    Failed,
    Deleted,
    Inserted,
    Modified,
    Plain,
}

pub(crate) fn style_for(marks: &GridMarks, selected: bool, row: usize, col: usize) -> Kind {
    if selected {
        Kind::Selected
    } else if marks.failed == Some(row) {
        Kind::Failed
    } else {
        match marks.rows.get(&row) {
            Some(RowMark::Deleted) => Kind::Deleted,
            Some(RowMark::Inserted) => Kind::Inserted,
            None if marks.modified.contains(&(row, col)) => Kind::Modified,
            None => Kind::Plain,
        }
    }
}

#[derive(Default)]
pub(crate) struct GridModel {
    pub columns: Vec<GridColumn>,
    pub rows: Vec<Row>,
    pub more: bool,
    pub truncated: bool,
    pub chars: Vec<usize>,
    pub widths: Vec<Option<f32>>,
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
        if columns != self.columns {
            self.widths = vec![None; columns.len()];
        }
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

    pub fn set_width(&mut self, col: usize, w: f32) {
        if let Some(slot) = self.widths.get_mut(col) {
            *slot = Some(w);
        }
    }

    /// Widest value of a column across every loaded row, for autofit.
    pub fn fit_chars(&self, col: usize) -> usize {
        let Some(column) = self.columns.get(col) else {
            return MIN_CHARS;
        };
        let count = |s: &str| s.chars().take(MEASURE_CHARS).count();
        let cells = self.rows.iter().map(|row| match row.get(col) {
            Some(Some(v)) => count(&display_text(v)),
            _ => MIN_CHARS,
        });
        cells.fold(count(&column.name), usize::max).max(MIN_CHARS)
    }

    pub fn has_cell(&self, row: usize, col: usize) -> bool {
        row < self.rows.len() && col < self.columns.len()
    }

    pub fn set_cell(&mut self, row: usize, col: usize, value: Option<String>) {
        let Some(slot) = self.rows.get_mut(row).and_then(|r| r.get_mut(col)) else {
            return;
        };
        *slot = value.map(SharedString::from);
        if let (Some(v), Some(chars)) = (&*slot, self.chars.get_mut(col)) {
            *chars = (*chars).max(display_text(v).chars().take(MEASURE_CHARS).count());
        }
    }

    pub fn replace_tail(&mut self, base: usize, rows: Vec<Vec<Option<String>>>) {
        self.rows.truncate(base);
        self.rows.extend(shared(rows));
        let tail = column_chars(&self.columns, &self.rows[self.rows.len().min(base)..]);
        for (have, new) in self.chars.iter_mut().zip(tail) {
            *have = (*have).max(new);
        }
        let last = self.rows.len().checked_sub(1);
        let clamp = |p: CellPos| {
            last.map(|l| CellPos {
                row: p.row.min(l),
                col: p.col,
            })
        };
        self.anchor = self.anchor.and_then(clamp);
        self.active = self.active.and_then(clamp);
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

    pub fn loaded_cells(&self) -> Vec<Vec<Option<String>>> {
        self.rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| c.as_ref().map(|v| v.to_string()))
                    .collect()
            })
            .collect()
    }

    pub fn export_cells(&self) -> (Range<usize>, Vec<Vec<Option<String>>>) {
        match self.selection() {
            Some((rows, cols)) if rows.len() > 1 || cols.len() > 1 => {
                let end = rows.end.min(self.rows.len());
                let start = rows.start.min(end);
                let picked = self.rows[start..end]
                    .iter()
                    .map(|r| {
                        cols.clone()
                            .map(|c| r.get(c).and_then(|v| v.as_ref()).map(|v| v.to_string()))
                            .collect()
                    })
                    .collect();
                (cols, picked)
            }
            _ => (0..self.columns.len(), self.loaded_cells()),
        }
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

pub(crate) fn column_lefts(
    chars: &[usize],
    widths: &[Option<f32>],
    char_w: f32,
    pad: f32,
    cap: f32,
) -> Vec<f32> {
    let mut lefts = Vec::with_capacity(chars.len() + 1);
    lefts.push(0.);
    for (c, &n) in chars.iter().enumerate() {
        let w = widths
            .get(c)
            .copied()
            .flatten()
            .unwrap_or_else(|| column_width(n, char_w, pad, cap));
        lefts.push(lefts.last().copied().unwrap_or(0.) + w);
    }
    lefts
}

pub(crate) fn drag_width(start_w: f32, start_x: f32, x: f32, min: f32) -> f32 {
    (start_w + x - start_x).clamp(min, MAX_COL_W.max(min))
}

const AUTOSCROLL_GAIN: f32 = 10.;
const AUTOSCROLL_MIN_UNITS: f32 = 2.;
const AUTOSCROLL_MAX_UNITS: f32 = 40.;
const AUTOSCROLL_MAX_DT: f32 = 0.05;

/// How far `p` is past `lo..=hi`: negative before it, positive after it, zero inside.
pub(crate) fn edge_overshoot(p: f32, lo: f32, hi: f32) -> f32 {
    if p < lo {
        p - lo
    } else if p > hi {
        p - hi
    } else {
        0.
    }
}

/// Autoscroll speed in px/s for an overshoot in px; `unit` is one row height.
pub(crate) fn autoscroll_speed(over: f32, unit: f32) -> f32 {
    if over == 0. || unit <= 0. || !over.is_finite() || !unit.is_finite() {
        return 0.;
    }
    over.signum()
        * (over.abs() * AUTOSCROLL_GAIN)
            .clamp(AUTOSCROLL_MIN_UNITS * unit, AUTOSCROLL_MAX_UNITS * unit)
}

/// Pixels to scroll this tick; a stalled frame never jumps more than `AUTOSCROLL_MAX_DT` of travel.
pub(crate) fn autoscroll_step(speed: f32, dt: f32) -> f32 {
    speed * dt.clamp(0., AUTOSCROLL_MAX_DT)
}

/// Rows moved by one page key: a screen minus one row of overlap, never fewer than one.
pub(crate) fn page_rows(body_h: f32, row_h: f32) -> usize {
    if row_h <= 0. || !body_h.is_finite() || !row_h.is_finite() {
        return 1;
    }
    ((body_h / row_h).floor() as usize).saturating_sub(1).max(1)
}

/// Cell under a point in content coordinates (gutter and scroll already removed), clamped to the grid.
pub(crate) fn cell_at(
    x: f32,
    y: f32,
    lefts: &[f32],
    row_h: f32,
    rows: usize,
) -> Option<(usize, usize)> {
    let cols = lefts.len().checked_sub(1).filter(|&c| c > 0)?;
    let last = rows.checked_sub(1)?;
    if row_h <= 0. || !x.is_finite() || !y.is_finite() {
        return None;
    }
    let row = ((y.max(0.) / row_h).floor() as usize).min(last);
    let col = lefts[1..].partition_point(|&r| r <= x).min(cols - 1);
    Some((row, col))
}

/// Whether an axis has enough overflow to show a scrollbar; render and paint must agree on it.
pub(crate) fn scrollable(max_offset: f32) -> bool {
    max_offset > 0.5
}

/// Top and length in px of a scrollbar thumb inside a track, or `None` when the content fits.
// Mirrors the palette's scroll_thumb; daedalus-ui cannot import the app crate.
pub(crate) fn thumb(
    view: f32,
    max_offset: f32,
    offset: f32,
    track: f32,
    min_len: f32,
) -> Option<(f32, f32)> {
    if view <= 0. || !scrollable(max_offset) || track <= 0. {
        return None;
    }
    let len = (track * view / (view + max_offset)).max(min_len).min(track);
    let top = (offset / max_offset).clamp(0., 1.) * (track - len);
    Some((top, len))
}

/// Scroll offset that centres the thumb on a pointer position along the track.
pub(crate) fn bar_offset(pos: f32, track: f32, thumb_len: f32, max_offset: f32) -> f32 {
    if track <= thumb_len || max_offset <= 0. {
        return 0.;
    }
    ((pos - thumb_len / 2.) / (track - thumb_len)).clamp(0., 1.) * max_offset
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

/// Fully visible (first, last) rows; (1, 0) when none fits, so the range stays empty.
pub(crate) fn visible_rows(scroll_y: f32, body_h: f32, row_h: f32) -> (usize, usize) {
    if row_h <= 0. || !row_h.is_finite() || !scroll_y.is_finite() || !body_h.is_finite() {
        return (0, 0);
    }
    let first = (scroll_y / row_h).ceil() as usize;
    let end = ((scroll_y + body_h) / row_h).floor() as usize;
    if end == 0 {
        return (1, 0);
    }
    (first, end - 1)
}

pub(crate) fn scroll_strategy(
    target: usize,
    first_visible: usize,
    last_visible: usize,
) -> Option<ScrollStrategy> {
    if target < first_visible {
        Some(ScrollStrategy::Top)
    } else if target > last_visible {
        Some(ScrollStrategy::Bottom)
    } else {
        None
    }
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
    #[test]
    fn edge_overshoot_is_signed_and_zero_inside() {
        assert_eq!(edge_overshoot(5., 10., 100.), -5.);
        assert_eq!(edge_overshoot(50., 10., 100.), 0.);
        assert_eq!(edge_overshoot(130., 10., 100.), 30.);
        assert_eq!(edge_overshoot(10., 10., 100.), 0.);
        assert_eq!(edge_overshoot(100., 10., 100.), 0.);
    }

    #[test]
    fn autoscroll_speed_grows_with_distance_and_is_clamped() {
        assert_eq!(autoscroll_speed(1., 20.), 40.);
        assert_eq!(autoscroll_speed(30., 20.), 300.);
        assert_eq!(autoscroll_speed(1000., 20.), 800.);
        assert_eq!(autoscroll_speed(-30., 20.), -300.);
        assert_eq!(autoscroll_speed(0., 20.), 0.);
        assert_eq!(autoscroll_speed(30., 0.), 0.);
        assert_eq!(autoscroll_speed(f32::NAN, 20.), 0.);
    }

    #[test]
    fn autoscroll_step_caps_a_long_frame() {
        assert!((autoscroll_step(300., 0.016) - 4.8).abs() < 1e-4);
        assert!((autoscroll_step(300., 1.0) - 15.).abs() < 1e-4);
        assert_eq!(autoscroll_step(300., -1.0), 0.);
    }

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
    fn export_cells_uses_a_multi_cell_selection() {
        let mut m = grid(4, 3, false);
        m.select(pos(1, 1), false);
        m.select(pos(2, 2), true);
        let (cols, rows) = m.export_cells();
        assert_eq!(cols, 1..3);
        assert_eq!(
            rows,
            vec![
                vec![cell("1-1"), cell("1-2")],
                vec![cell("2-1"), cell("2-2")]
            ]
        );
    }

    #[test]
    fn export_cells_single_cell_means_all_rows() {
        let mut m = grid(3, 2, false);
        m.select(pos(1, 1), false);
        let (cols, rows) = m.export_cells();
        assert_eq!((cols, rows.len()), (0..2, 3));
    }

    #[test]
    fn export_cells_without_selection_is_all_rows() {
        let m = grid(3, 2, false);
        let (cols, rows) = m.export_cells();
        assert_eq!((cols, rows), (0..2, m.loaded_cells()));
    }

    #[test]
    fn loaded_cells_ignores_the_selection() {
        let mut m = grid(3, 2, false);
        m.select(pos(0, 0), false);
        m.select(pos(1, 1), true);
        assert_eq!(m.loaded_cells().len(), 3);
        assert_eq!(m.loaded_cells()[2], vec![cell("2-0"), cell("2-1")]);
    }

    #[test]
    fn set_cell_replaces_display_value() {
        let mut m = grid(2, 2, false);
        m.set_cell(1, 0, cell("edited"));
        m.set_cell(0, 1, None);
        m.set_cell(9, 9, cell("ignored"));
        assert_eq!(m.rows[1][0].as_ref().map(|v| v.as_ref()), Some("edited"));
        assert_eq!(m.rows[0][1], None);
        assert_eq!(m.rows.len(), 2);
    }

    #[test]
    fn replace_tail_truncates_and_clamps_selection() {
        let mut m = grid(5, 2, false);
        m.select(pos(4, 1), false);
        m.select(pos(4, 1), true);
        m.replace_tail(2, vec![vec![cell("n0"), cell("n1")]]);
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows[2][0].as_ref().map(|v| v.as_ref()), Some("n0"));
        assert_eq!(m.active, Some(pos(2, 1)));
        assert_eq!(m.anchor, Some(pos(2, 1)));
        m.replace_tail(0, vec![]);
        assert_eq!((m.anchor, m.active), (None, None));
    }

    #[test]
    fn replace_tail_widens_columns_from_the_new_rows_only() {
        let mut m = grid(2, 1, false);
        let before = m.chars[0];
        m.replace_tail(1, vec![vec![cell("a much longer value")]]);
        assert_eq!(m.chars[0], "a much longer value".len());
        m.replace_tail(1, vec![vec![cell("x")]]);
        assert_eq!(m.chars[0], "a much longer value".len().max(before));
    }

    #[test]
    fn has_cell_checks_both_bounds() {
        let m = grid(2, 3, false);
        assert!(m.has_cell(1, 2));
        assert!(!m.has_cell(2, 0));
        assert!(!m.has_cell(0, 3));
    }

    #[test]
    fn replace_tail_with_base_beyond_len_appends() {
        let mut m = grid(2, 2, false);
        m.replace_tail(10, vec![vec![cell("a"), cell("b")]]);
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows[2][1].as_ref().map(|v| v.as_ref()), Some("b"));
    }

    #[test]
    fn selection_wins_over_marks() {
        let mut marks = GridMarks::default();
        marks.modified.insert((0, 0));
        marks.rows.insert(0, RowMark::Deleted);
        marks.failed = Some(0);
        assert_eq!(style_for(&marks, true, 0, 0), Kind::Selected);
        assert_eq!(style_for(&marks, false, 1, 0), Kind::Plain);
    }

    #[test]
    fn failed_wins_over_modified() {
        let mut marks = GridMarks::default();
        marks.modified.insert((0, 0));
        marks.failed = Some(0);
        assert_eq!(style_for(&marks, false, 0, 0), Kind::Failed);
        marks.failed = None;
        assert_eq!(style_for(&marks, false, 0, 0), Kind::Modified);
        assert_eq!(style_for(&marks, false, 0, 1), Kind::Plain);
    }

    #[test]
    fn deleted_strikes() {
        let mut marks = GridMarks::default();
        marks.rows.insert(3, RowMark::Deleted);
        marks.rows.insert(4, RowMark::Inserted);
        marks.modified.insert((3, 0));
        assert_eq!(style_for(&marks, false, 3, 0), Kind::Deleted);
        assert_eq!(style_for(&marks, false, 4, 0), Kind::Inserted);
    }

    #[test]
    fn column_width_caps() {
        assert_eq!(column_width(200, 7., 6., 400.), 400.);
        assert_eq!(column_width(5, 7., 6., 400.), 47.);
    }

    #[test]
    fn column_lefts_mixes_overrides_and_auto() {
        let lefts = column_lefts(&[5, 200, 3], &[None, Some(50.), None], 7., 6., 400.);
        assert_eq!(lefts, vec![0., 47., 97., 130.]);
    }

    #[test]
    fn column_lefts_ignores_missing_widths() {
        let auto = column_lefts(&[5, 200, 3], &[None, None, None], 7., 6., 400.);
        assert_eq!(column_lefts(&[5, 200, 3], &[], 7., 6., 400.), auto);
    }

    #[test]
    fn drag_width_clamps() {
        assert_eq!(drag_width(100., 10., 60., 20.), 150.);
        assert_eq!(drag_width(100., 10., -500., 20.), 20.);
        assert_eq!(drag_width(100., 0., 9999., 20.), MAX_COL_W);
    }

    #[test]
    fn show_keeps_widths_for_same_columns() {
        let mut m = GridModel::default();
        m.show(cols(2), vec![vec![cell("a"), cell("b")]], false);
        m.set_width(1, 80.);
        m.show(cols(2), vec![vec![cell("x"), cell("y")]], false);
        assert_eq!(m.widths, vec![None, Some(80.)]);
        let mut other = cols(2);
        other[1].name = "other".into();
        m.show(other, vec![], false);
        assert_eq!(m.widths, vec![None, None]);
    }

    #[test]
    fn append_keeps_widths() {
        let mut m = grid(2, 2, true);
        m.set_width(0, 60.);
        m.append(vec![vec![cell("a"), cell("b")]], false);
        assert_eq!(m.widths, vec![Some(60.), None]);
    }

    #[test]
    fn fit_chars_spans_all_loaded_rows() {
        let mut m = GridModel::default();
        m.show(cols(1), vec![vec![cell("ab")]], true);
        m.append(
            vec![vec![Some(std::iter::repeat_n('x', 30).collect())]],
            false,
        );
        assert_eq!(m.fit_chars(0), 30);
        let mut nulls = GridModel::default();
        let id = GridColumn {
            name: "id".into(),
            type_label: "".into(),
            numeric: false,
        };
        nulls.show(vec![id], vec![vec![None]], false);
        assert_eq!(nulls.fit_chars(0), MIN_CHARS);
        assert_eq!(nulls.fit_chars(9), MIN_CHARS);
    }

    #[test]
    fn fit_chars_caps_at_measure_chars() {
        let mut m = GridModel::default();
        let long: String = std::iter::repeat_n('x', 5000).collect();
        m.show(cols(1), vec![vec![Some(long)]], false);
        assert_eq!(m.fit_chars(0), MEASURE_CHARS);
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
    fn scroll_strategy_moves_only_when_target_is_off_screen() {
        assert_eq!(scroll_strategy(5, 3, 10), None);
        assert_eq!(scroll_strategy(3, 3, 10), None);
        assert_eq!(scroll_strategy(10, 3, 10), None);
        assert!(matches!(
            scroll_strategy(2, 3, 10),
            Some(ScrollStrategy::Top)
        ));
        assert!(matches!(
            scroll_strategy(11, 3, 10),
            Some(ScrollStrategy::Bottom)
        ));
    }

    #[test]
    fn visible_rows_counts_only_fully_visible_rows() {
        assert_eq!(visible_rows(0., 100., 20.), (0, 4));
        assert_eq!(visible_rows(0., 110., 20.), (0, 4));
        assert_eq!(visible_rows(10., 100., 20.), (1, 4));
        assert_eq!(visible_rows(40., 100., 20.), (2, 6));
        assert_eq!(visible_rows(0., 100., 0.), (0, 0));
        assert_eq!(visible_rows(f32::NAN, 100., 20.), (0, 0));
    }

    #[test]
    fn empty_visible_range_always_scrolls() {
        let (first, last) = visible_rows(0., 0., 20.);
        assert!(scroll_strategy(0, first, last).is_some());
        assert!(scroll_strategy(1, first, last).is_some());
    }

    #[test]
    fn down_from_last_visible_row_scrolls_to_bottom() {
        let (first, last) = visible_rows(0., 100., 20.);
        assert!(matches!(
            scroll_strategy(5, first, last),
            Some(ScrollStrategy::Bottom)
        ));
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

    #[test]
    fn page_rows_cases() {
        assert_eq!(page_rows(220., 22.), 9);
        assert_eq!(page_rows(30., 22.), 1);
        assert_eq!(page_rows(0., 22.), 1);
        assert_eq!(page_rows(100., 0.), 1);
        assert_eq!(page_rows(f32::NAN, 22.), 1);
    }

    #[test]
    fn cell_at_maps_content_coords() {
        let lefts = [0., 100., 200., 300.];
        assert_eq!(cell_at(150., 45., &lefts, 22., 10), Some((2, 1)));
        assert_eq!(cell_at(-5., -5., &lefts, 22., 10), Some((0, 0)));
        assert_eq!(cell_at(999., 9999., &lefts, 22., 10), Some((9, 2)));
        assert_eq!(cell_at(100., 22., &lefts, 22., 10), Some((1, 1)));
    }

    #[test]
    fn cell_at_follows_resized_columns() {
        let lefts = [0., 40., 300.];
        assert_eq!(cell_at(39., 0., &lefts, 22., 3), Some((0, 0)));
        assert_eq!(cell_at(41., 0., &lefts, 22., 3), Some((0, 1)));
    }

    #[test]
    fn cell_at_empty() {
        assert_eq!(cell_at(10., 10., &[0.], 22., 10), None);
        assert_eq!(cell_at(10., 10., &[], 22., 10), None);
        assert_eq!(cell_at(10., 10., &[0., 100.], 22., 0), None);
        assert_eq!(cell_at(f32::NAN, 10., &[0., 100.], 22., 5), None);
    }

    #[test]
    fn bar_offset_centres_thumb() {
        assert_eq!(bar_offset(0., 100., 20., 500.), 0.);
        assert_eq!(bar_offset(50., 100., 20., 500.), 250.);
        assert_eq!(bar_offset(100., 100., 20., 500.), 500.);
        assert_eq!(bar_offset(30., 100., 20., 500.), 125.);
        assert_eq!(bar_offset(30., 20., 20., 500.), 0.);
    }

    #[test]
    fn thumb_matches_palette() {
        assert_eq!(thumb(100., 300., 150., 100., 24.), Some((37.5, 25.)));
        assert_eq!(thumb(100., 0., 0., 100., 24.), None);
    }

    #[test]
    fn step_jump_saturates() {
        let mut m = grid(4, 3, false);
        m.select(pos(1, 1), false);
        assert_eq!(m.step(isize::MAX, isize::MAX, false), Some(pos(3, 2)));
        assert_eq!(m.step(-isize::MAX, 0, false), Some(pos(0, 2)));
    }

    #[test]
    fn scrollable_ignores_view_and_track() {
        assert!(scrollable(0.6));
        assert!(!scrollable(0.5));
        assert!(!scrollable(0.));
        assert!(thumb(0., 100., 0., 50., 24.).is_none());
        assert!(scrollable(100.));
    }

    #[test]
    fn thumb_centre_round_trips_through_bar_offset() {
        let (view, max, cur, track, min_len) = (100., 900., 300., 100., 30.);
        let (top, len) = thumb(view, max, cur, track, min_len).unwrap();
        let back = bar_offset(top + len / 2., track, len, max);
        assert!((back - cur).abs() < 0.01);
    }
}
