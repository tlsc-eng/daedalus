mod model;
mod value;

use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnyElement, App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, Entity,
    EventEmitter, FocusHandle, Focusable, Font, FontFeatures, FontStyle, FontWeight, Hsla,
    KeyBinding, ListHorizontalSizingBehavior, ListSizingBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollStrategy, SharedString,
    Subscription, Task, UniformListScrollHandle, Window, actions, div, fill, point, prelude::*, px,
    size, uniform_list,
};

use crate::components::Tooltip;
use crate::motion::{self, Busy, Glide, Opening};
use crate::{ActiveTheme, Ellipsis, InputEvent, Size, Space, TextInput, Theme};

use model::{
    CellPos, GridModel, Kind, MAX_COL_W, bar_offset, cell_at, column_lefts, column_width,
    display_text, drag_width, gutter_chars, page_rows, reveal_x, scroll_strategy, scrollable,
    style_for, thumb, visible_columns, visible_rows,
};
pub use model::{GridColumn, GridMarks, RowMark};
use value::Inspected;

actions!(
    data_grid,
    [
        Up,
        Down,
        Left,
        Right,
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        SelectAll,
        Copy,
        Edit,
        ToggleValue,
        CloseValue,
        JumpUp,
        JumpDown,
        JumpLeft,
        JumpRight,
        SelectJumpUp,
        SelectJumpDown,
        SelectJumpLeft,
        SelectJumpRight,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown
    ]
);

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("DataGrid");
    cx.bind_keys([
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("enter", Edit, ctx),
        KeyBinding::new("shift-enter", ToggleValue, ctx),
        KeyBinding::new("escape", CloseValue, ctx),
        KeyBinding::new("cmd-up", JumpUp, ctx),
        KeyBinding::new("cmd-down", JumpDown, ctx),
        KeyBinding::new("cmd-left", JumpLeft, ctx),
        KeyBinding::new("cmd-right", JumpRight, ctx),
        KeyBinding::new("cmd-shift-up", SelectJumpUp, ctx),
        KeyBinding::new("cmd-shift-down", SelectJumpDown, ctx),
        KeyBinding::new("cmd-shift-left", SelectJumpLeft, ctx),
        KeyBinding::new("cmd-shift-right", SelectJumpRight, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("shift-pageup", SelectPageUp, ctx),
        KeyBinding::new("shift-pagedown", SelectPageDown, ctx),
    ]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GridEvent {
    /// The viewport is near the last loaded row and more rows exist.
    FetchMore,
    /// The user asked to edit a cell; only emitted while the grid is editable.
    EditRequested { row: usize, col: usize },
    /// The in-cell editor closed with text that differs from where it started.
    Edited {
        row: usize,
        col: usize,
        text: String,
    },
    /// Right mouse down on a cell, emitted even when the grid is not editable.
    ContextMenu {
        row: usize,
        col: usize,
        position: Point<Pixels>,
    },
}

struct Editing {
    row: usize,
    col: usize,
    initial: String,
    input: Entity<TextInput>,
    _subs: Vec<Subscription>,
}

struct CellStyle {
    bg: Option<Hsla>,
    fg: Hsla,
    strike: bool,
}

/// A read-only, virtualized table of query results with a sticky header and a pinned row-number gutter.
pub struct DataGrid {
    model: GridModel,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    dim: Glide,
    stale: Option<Busy>,
    stale_task: Option<Task<()>>,
    generation: u64,
    opened: Option<Opening>,
    lefts: Vec<f32>,
    viewport_w: f32,
    row_h: f32,
    gutter_w: f32,
    char_w: f32,
    pad_w: f32,
    drag: Option<Drag>,
    editable: bool,
    marks: GridMarks,
    editing: Option<Editing>,
    // show() has no window, so render restores grid focus if the dropped editor still held it.
    refocus_pending: Option<FocusHandle>,
    value_open: bool,
    value_opened: Option<Opening>,
    value_cache: Option<(CellPos, SharedString, Rc<Inspected>)>,
}

impl EventEmitter<GridEvent> for DataGrid {}

impl Focusable for DataGrid {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn dim_duration(cx: &App) -> Duration {
    let m = &cx.theme().motion;
    if m.reduced { Duration::ZERO } else { m.base }
}

impl DataGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            model: GridModel::default(),
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            dim: Glide::settled(1., dim_duration(cx)),
            stale: None,
            stale_task: None,
            generation: 0,
            opened: None,
            lefts: Vec::new(),
            viewport_w: 0.,
            row_h: 0.,
            gutter_w: 0.,
            char_w: 0.,
            pad_w: 0.,
            drag: None,
            editable: false,
            marks: GridMarks::default(),
            editing: None,
            refocus_pending: None,
            value_open: false,
            value_opened: None,
            value_cache: None,
        }
    }

    /// Replaces the result with a new one and its first batch of rows.
    pub fn show(
        &mut self,
        columns: Vec<GridColumn>,
        rows: Vec<Vec<Option<String>>>,
        more: bool,
        cx: &mut Context<Self>,
    ) {
        self.model.show(columns, rows, more);
        if let Some(editing) = self.editing.take() {
            self.refocus_pending = Some(editing.input.focus_handle(cx));
        }
        self.clear_stale(cx);
        self.generation += 1;
        self.opened = Some(Opening::now());
        let base = self.scroll.0.borrow().base_handle.clone();
        base.set_offset(point(px(0.), base.offset().y));
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Appends the next page; keeps scroll, selection and column widths.
    pub fn append(&mut self, rows: Vec<Vec<Option<String>>>, more: bool, cx: &mut Context<Self>) {
        self.model.append(rows, more);
        cx.notify();
    }

    /// Allows `EditRequested` to be emitted; off by default.
    pub fn set_editable(&mut self, on: bool, cx: &mut Context<Self>) {
        self.editable = on;
        cx.notify();
    }

    /// Replaces the displayed value of one cell; out-of-range positions are ignored.
    pub fn set_cell(
        &mut self,
        row: usize,
        col: usize,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.model.set_cell(row, col, value);
        cx.notify();
    }

    /// Drops every row from `base` on and appends `rows`, keeping the selection inside the result.
    pub fn replace_tail(
        &mut self,
        base: usize,
        rows: Vec<Vec<Option<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.as_ref().is_some_and(|e| e.row >= base) {
            self.cancel_edit(window, cx);
        }
        self.model.replace_tail(base, rows);
        cx.notify();
    }

    pub fn set_marks(&mut self, marks: GridMarks, cx: &mut Context<Self>) {
        self.marks = marks;
        cx.notify();
    }

    pub fn active_cell(&self) -> Option<(usize, usize)> {
        self.model.active.map(|p| (p.row, p.col))
    }

    /// Opens the in-cell editor; `null` shows a NULL placeholder over empty text.
    pub fn begin_edit(
        &mut self,
        row: usize,
        col: usize,
        initial: String,
        null: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.has_cell(row, col) {
            return;
        }
        self.commit_edit(window, cx);
        self.reveal(CellPos { row, col });
        let input = cx.new(|cx| TextInput::new(if null { "NULL" } else { "" }, cx));
        input.update(cx, |i, cx| {
            i.set_text(initial.clone(), cx);
            i.select_all(cx);
        });
        let handle = input.focus_handle(cx);
        let subs = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, _, e: &InputEvent, window, cx| match e {
                    InputEvent::Submit => this.commit_edit(window, cx),
                    InputEvent::Cancel => this.cancel_edit(window, cx),
                    _ => {}
                },
            ),
            cx.on_blur(&handle, window, |this, window, cx| {
                this.commit_edit(window, cx)
            }),
        ];
        self.editing = Some(Editing {
            row,
            col,
            initial,
            input,
            _subs: subs,
        });
        window.focus(&handle);
        cx.notify();
    }

    /// Closes the editor and emits `Edited` if the text changed.
    pub fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let text = editing.input.read(cx).text().to_string();
        if text != editing.initial {
            cx.emit(GridEvent::Edited {
                row: editing.row,
                col: editing.col,
                text,
            });
        }
        if editing.input.focus_handle(cx).is_focused(window) {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    /// Closes the editor without an event.
    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editing) = self.editing.take() {
            if editing.input.focus_handle(cx).is_focused(window) {
                window.focus(&self.focus);
            }
            cx.notify();
        }
    }

    fn request_edit(&mut self, pos: CellPos, cx: &mut Context<Self>) {
        if self.editable && self.editing.is_none() {
            cx.emit(GridEvent::EditRequested {
                row: pos.row,
                col: pos.col,
            });
        } else if !self.editable && self.editing.is_none() {
            self.show_value(cx);
        }
    }

    /// Opens the value panel for the active cell.
    pub fn show_value(&mut self, cx: &mut Context<Self>) {
        if !self.value_open {
            self.value_open = true;
            self.value_opened = Some(Opening::now());
        }
        cx.notify();
    }

    fn close_value(&mut self, cx: &mut Context<Self>) {
        if self.value_open {
            self.value_open = false;
            cx.notify();
        }
    }

    fn inspected(&mut self) -> Option<(CellPos, Rc<Inspected>)> {
        let pos = self.model.active?;
        let raw = self.model.rows.get(pos.row)?.get(pos.col)?;
        let key = raw.clone().unwrap_or_default();
        if let Some((p, k, i)) = &self.value_cache
            && *p == pos
            && *k == key
            && (raw.is_some() == (i.kind != value::ValueKind::Null))
        {
            return Some((pos, i.clone()));
        }
        let inspected = Rc::new(value::inspect(raw.as_ref().map(|v| v.as_ref())));
        self.value_cache = Some((pos, key, inspected.clone()));
        Some((pos, inspected))
    }

    fn value_panel(
        &self,
        shown: Option<(CellPos, Rc<Inspected>)>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let t = cx.theme().clone();
        let name = shown
            .as_ref()
            .and_then(|(p, _)| self.model.columns.get(p.col))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let raw: Option<SharedString> = shown
            .as_ref()
            .and_then(|(p, _)| self.model.rows.get(p.row)?.get(p.col)?.clone());
        let key = shown.as_ref().map_or(0, |(p, _)| p.row * 4096 + p.col);
        let body = match &shown {
            None => div()
                .text_color(t.color.content_muted)
                .child("Select a cell"),
            Some((_, i)) if i.kind == value::ValueKind::Null => div()
                .italic()
                .text_color(t.color.content_muted)
                .child("NULL"),
            Some((_, i)) => div().child(i.body.clone()),
        };
        let summary = shown.as_ref().map(|(_, i)| i.summary.clone());
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex_none()
                .px(t.space(Space::Sm))
                .cursor_pointer()
                .text_color(t.color.content_secondary)
                .hover(|s| s.bg(t.color.surface_hover))
                .child(label)
        };
        div()
            .flex_none()
            .w(t.ui(320.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(t.color.border)
            .bg(t.color.surface)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .h(t.size(Size::Control))
                    .items_center()
                    .px(t.space(Space::Sm))
                    .gap(t.space(Space::Sm))
                    .border_b_1()
                    .border_color(t.color.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .ellipsis()
                            .text_color(t.color.content_muted)
                            .child(match summary {
                                Some(s) => format!("{name} · {s}"),
                                None => name.to_string(),
                            }),
                    )
                    .child(button("grid-value-copy", "Copy").on_click(cx.listener(
                        move |_, _: &gpui::ClickEvent, _, cx| {
                            let text = raw.as_ref().map_or(String::new(), |v| v.to_string());
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        },
                    )))
                    .child(button("grid-value-close", "×").on_click(cx.listener(
                        |this, _: &gpui::ClickEvent, window, cx| {
                            this.close_value(cx);
                            window.focus(&this.focus);
                        },
                    ))),
            )
            .child(
                body.id(("grid-value-body", key))
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .p(t.space(Space::Sm))
                    .font_family(t.typography.mono.clone())
                    .text_color(t.color.content),
            )
    }

    fn right_click(
        &mut self,
        pos: CellPos,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        if !self.in_selection(pos.row, pos.col) {
            self.model.select(pos, false);
        }
        cx.emit(GridEvent::ContextMenu {
            row: pos.row,
            col: pos.col,
            position,
        });
        cx.notify();
    }

    /// Marks the result complete.
    pub fn finish(&mut self, truncated: bool, cx: &mut Context<Self>) {
        self.model.finish(truncated);
        cx.notify();
    }

    /// Dims the rows after `indicator_delay` while a replacement loads; `false` restores them at once.
    pub fn set_stale(&mut self, stale: bool, cx: &mut Context<Self>) {
        if !stale {
            self.clear_stale(cx);
            return;
        }
        if self.stale.is_some() {
            return;
        }
        let busy = Busy::start();
        self.stale = Some(busy);
        let delay = cx.theme().motion.indicator_delay;
        let dur = dim_duration(cx);
        self.stale_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                if this.stale == Some(busy) {
                    this.dim = Glide::settled(1., dur);
                    this.dim.retarget(motion::STALE_OPACITY);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The cells Copy as works on: a multi-cell selection, or every loaded row.
    pub fn export_cells(&self) -> (Vec<GridColumn>, Vec<Vec<Option<String>>>) {
        let (cols, rows) = self.model.export_cells();
        (self.model.columns[cols].to_vec(), rows)
    }

    /// Every column and loaded row, for exporting what the grid holds.
    pub fn loaded_cells(&self) -> (Vec<GridColumn>, Vec<Vec<Option<String>>>) {
        (self.model.columns.clone(), self.model.loaded_cells())
    }

    pub fn row_count(&self) -> usize {
        self.model.rows.len()
    }

    pub fn more(&self) -> bool {
        self.model.more
    }

    pub fn truncated(&self) -> bool {
        self.model.truncated
    }

    fn clear_stale(&mut self, cx: &mut Context<Self>) {
        self.stale = None;
        self.stale_task = None;
        self.dim = Glide::settled(1., dim_duration(cx));
        cx.notify();
    }

    fn reveal(&mut self, pos: CellPos) {
        let base = self.scroll.0.borrow().base_handle.clone();
        let offset = base.offset();
        let scroll_x = -f32::from(offset.x);
        if let Some((&left, &right)) = self.lefts.get(pos.col).zip(self.lefts.get(pos.col + 1)) {
            let x = reveal_x(scroll_x, left, right, self.viewport_w);
            if x != scroll_x {
                base.set_offset(point(px(-x), offset.y));
            }
        }
        self.reveal_row(pos.row);
    }

    fn reveal_row(&mut self, row: usize) {
        let base = self.scroll.0.borrow().base_handle.clone();
        let body_h = f32::from(base.bounds().size.height);
        let scroll_y = -f32::from(base.offset().y);
        if self.row_h > 0. {
            let (first, last) = visible_rows(scroll_y, body_h, self.row_h);
            if let Some(strategy) = scroll_strategy(row, first, last) {
                self.scroll.scroll_to_item(row, strategy);
            }
        }
    }

    fn step(&mut self, d_row: isize, d_col: isize, extend: bool, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return cx.propagate();
        }
        if let Some(pos) = self.model.step(d_row, d_col, extend) {
            self.reveal(pos);
        }
        cx.notify();
    }

    fn page(&mut self, dir: isize, extend: bool, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return cx.propagate();
        }
        let base = self.scroll.0.borrow().base_handle.clone();
        let n = page_rows(f32::from(base.bounds().size.height), self.row_h);
        let y = (-f32::from(base.offset().y) + dir as f32 * n as f32 * self.row_h)
            .clamp(0., f32::from(base.max_offset().height));
        base.set_offset(point(base.offset().x, px(-y)));
        self.step(dir * n as isize, 0, extend, cx);
    }

    fn click(
        &mut self,
        pos: CellPos,
        extend: bool,
        clicks: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        self.model.select(pos, extend);
        self.reveal(pos);
        if clicks >= 2 {
            self.request_edit(pos, cx);
        }
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return cx.propagate();
        }
        if let Some(text) = self.model.copy_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn in_selection(&self, row: usize, col: usize) -> bool {
        self.model
            .selection()
            .is_some_and(|(rows, cols)| rows.contains(&row) && cols.contains(&col))
    }

    fn cell_colors(&self, row: usize, col: usize, t: &Theme) -> CellStyle {
        let c = &t.color;
        let plain = CellStyle {
            bg: None,
            fg: c.content,
            strike: false,
        };
        match style_for(&self.marks, self.in_selection(row, col), row, col) {
            Kind::Selected => CellStyle {
                bg: Some(c.surface_accent),
                ..plain
            },
            Kind::Failed => CellStyle {
                bg: Some(c.danger_surface),
                ..plain
            },
            Kind::Deleted => CellStyle {
                fg: c.content_disabled,
                strike: true,
                ..plain
            },
            Kind::Inserted => CellStyle {
                bg: Some(c.success.opacity(0.12)),
                ..plain
            },
            Kind::Modified => CellStyle {
                bg: Some(c.warning.opacity(0.16)),
                ..plain
            },
            Kind::Plain => plain,
        }
    }
}

impl DataGrid {
    fn gutter_mark(&self, row: usize, t: &Theme) -> Option<(&'static str, Hsla)> {
        match self.marks.rows.get(&row) {
            Some(RowMark::Inserted) => Some(("+", t.color.success)),
            Some(RowMark::Deleted) => Some(("\u{2212}", t.color.content_disabled)),
            None if self.marks.row_modified(row) => Some(("\u{2022}", t.color.warning)),
            None => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Drag {
    Column {
        col: usize,
        start_x: f32,
        start_w: f32,
    },
    Select {
        rows: bool,
    },
    VBar,
    HBar,
}

struct Metrics {
    theme: Theme,
    pad: gpui::Pixels,
    row_h: gpui::Pixels,
    gutter_w: f32,
    scroll_x: f32,
    lefts: Vec<f32>,
    vis: Range<usize>,
}

impl DataGrid {
    fn metrics(&mut self, window: &mut Window, cx: &App) -> Metrics {
        let t = cx.theme().clone();
        let font = Font {
            family: t.typography.mono.clone(),
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let text_system = window.text_system();
        let char_w = text_system
            .advance(text_system.resolve_font(&font), t.typography.caption, 'm')
            .map(|s| f32::from(s.width))
            .unwrap_or(7.);
        let pad = t.space(Space::Sm);
        let cap = f32::from(t.ui(400.));
        let gutter_w = column_width(
            gutter_chars(self.model.rows.len()) + usize::from(!self.marks.is_empty()),
            char_w,
            f32::from(pad),
            f32::MAX,
        );
        let lefts = column_lefts(
            &self.model.chars,
            &self.model.widths,
            char_w,
            f32::from(pad),
            cap,
        );
        let base = self.scroll.0.borrow().base_handle.clone();
        let scroll_x = -f32::from(base.offset().x);
        let bounds_w = f32::from(base.bounds().size.width);
        let list_w = if bounds_w > 0. {
            bounds_w
        } else {
            f32::from(window.viewport_size().width)
        };
        let viewport_w = list_w - gutter_w;
        let vis = visible_columns(
            &lefts,
            scroll_x,
            f32::from(window.viewport_size().width) - gutter_w,
        );
        self.lefts = lefts.clone();
        self.viewport_w = viewport_w;
        let row_h = t.size(Size::Row);
        self.row_h = f32::from(row_h);
        self.gutter_w = gutter_w;
        self.char_w = char_w;
        self.pad_w = f32::from(pad);
        Metrics {
            row_h,
            theme: t,
            pad,
            gutter_w,
            scroll_x,
            lefts,
            vis,
        }
    }

    fn start_resize(&mut self, col: usize, x: f32, clicks: usize, cx: &mut Context<Self>) {
        if clicks >= 2 {
            let w = column_width(
                self.model.fit_chars(col),
                self.char_w,
                self.pad_w,
                MAX_COL_W,
            );
            self.model.set_width(col, w);
            self.drag = None;
            cx.notify();
            return;
        }
        let (Some(left), Some(right)) = (self.lefts.get(col), self.lefts.get(col + 1)) else {
            return;
        };
        self.drag = Some(Drag::Column {
            col,
            start_x: x,
            start_w: right - left,
        });
        cx.notify();
    }

    fn drag_move(
        &mut self,
        pos: Point<Pixels>,
        pressed: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        if pressed != Some(MouseButton::Left) {
            return self.end_drag(cx);
        }
        match self.drag {
            Some(Drag::Column {
                col,
                start_x,
                start_w,
            }) => {
                let min = column_width(1, self.char_w, self.pad_w, MAX_COL_W);
                let w = drag_width(start_w, start_x, f32::from(pos.x), min);
                if self.model.widths.get(col).copied().flatten() != Some(w) {
                    self.model.set_width(col, w);
                    cx.notify();
                }
            }
            Some(Drag::Select { rows }) => {
                let base = self.scroll.0.borrow().base_handle.clone();
                let b = base.bounds();
                let offset = base.offset();
                let x = f32::from(pos.x - b.origin.x - offset.x) - self.gutter_w;
                let y = f32::from(pos.y - b.origin.y - offset.y);
                let Some((row, col)) =
                    cell_at(x, y, &self.lefts, self.row_h, self.model.rows.len())
                else {
                    return;
                };
                if rows {
                    self.model.select_row(row, true);
                } else {
                    self.model.select(CellPos { row, col }, true);
                }
                if rows {
                    self.reveal_row(row);
                } else {
                    self.reveal(CellPos { row, col });
                }
                cx.notify();
            }
            Some(Drag::VBar) => self.bar_to(pos, true, cx),
            Some(Drag::HBar) => self.bar_to(pos, false, cx),
            None => {}
        }
    }

    fn bar_to(&mut self, pos: Point<Pixels>, vertical: bool, cx: &mut Context<Self>) {
        let base = self.scroll.0.borrow().base_handle.clone();
        let b = base.bounds();
        let offset = base.offset();
        let max = base.max_offset();
        let min_len = f32::from(cx.theme().ui(24.));
        let (p, track, view, max, cur) = if vertical {
            (
                f32::from(pos.y - b.origin.y),
                f32::from(b.size.height),
                f32::from(b.size.height),
                f32::from(max.height),
                -f32::from(offset.y),
            )
        } else {
            (
                f32::from(pos.x - b.origin.x) - self.gutter_w,
                f32::from(b.size.width) - self.gutter_w,
                f32::from(b.size.width) - self.gutter_w,
                f32::from(max.width),
                -f32::from(offset.x),
            )
        };
        let Some((_, len)) = thumb(view, max, cur, track, min_len) else {
            return;
        };
        let v = bar_offset(p, track, len, max);
        if vertical {
            base.set_offset(point(offset.x, px(-v)));
        } else {
            base.set_offset(point(px(-v), offset.y));
        }
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        cx.notify();
    }

    /// Mouse listeners are one-frame, so the hook is re-registered on every paint of a drag.
    fn drag_hook(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        self.drag?;
        let grid = cx.entity().downgrade();
        Some(
            gpui::canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let g = grid.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble {
                            g.update(cx, |this, cx| {
                                this.drag_move(e.position, e.pressed_button, cx)
                            })
                            .ok();
                        }
                    });
                    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble {
                            grid.update(cx, |this, cx| this.end_drag(cx)).ok();
                        }
                    });
                },
            )
            .absolute()
            .size_full(),
        )
    }

    fn header(&self, m: &Metrics, cx: &mut Context<Self>) -> AnyElement {
        let t = &m.theme;
        let cells = self
            .model
            .columns
            .iter()
            .enumerate()
            .skip(m.vis.start)
            .take(m.vis.len());
        let cells = cells.map(|(c, col)| {
            let width = m.lefts[c + 1] - m.lefts[c];
            let mut cell = div()
                .id(("grid-col", c))
                .w(px(width))
                .flex_none()
                .h_full()
                .flex()
                .items_center()
                .relative()
                .px(m.pad)
                .border_r_1()
                .border_color(t.color.border)
                .text_color(t.color.content_secondary)
                .font_weight(FontWeight::MEDIUM)
                .child(
                    div()
                        .w_full()
                        .when(col.numeric, |d| d.text_right())
                        .ellipsis()
                        .child(col.name.clone()),
                )
                .child(
                    div()
                        .id(("grid-col-resize", c))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(px(6.))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                                this.start_resize(c, f32::from(e.position.x), e.click_count, cx);
                            }),
                        ),
                );
            if !col.type_label.is_empty() {
                let label = col.type_label.clone();
                cell = cell.tooltip(move |_, cx| Tooltip::view(label.clone(), cx));
            }
            cell
        });
        div()
            .flex()
            .flex_none()
            .h(t.size(Size::Control))
            .border_b_1()
            .border_color(t.color.border)
            .items_center()
            .child(
                div()
                    .w(px(m.gutter_w))
                    .h_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .px(m.pad)
                    .border_r_1()
                    .border_color(t.color.border)
                    .text_color(t.color.content_muted)
                    .child("#"),
            )
            .child(
                div().flex_1().h_full().overflow_hidden().child(
                    div()
                        .relative()
                        .left(px(-m.scroll_x))
                        .h_full()
                        .flex()
                        .items_center()
                        .child(div().flex_none().w(px(m.lefts[m.vis.start])))
                        .children(cells),
                ),
            )
            .into_any_element()
    }

    fn body(&self, m: Metrics, cx: &mut Context<Self>) -> AnyElement {
        let t = m.theme.clone();
        if self.model.rows.is_empty() && !self.model.more {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.color.content_muted)
                .child("No rows")
                .into_any_element();
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let (color, bar_w, inset) = (t.color.border_strong, t.ui(4.), t.ui(2.));
        let min_len = f32::from(t.ui(24.));
        let painted = handle.clone();
        let grid = cx.entity().downgrade();
        let gutter_w = m.gutter_w;
        let (can_v, can_h) = (
            scrollable(f32::from(handle.max_offset().height)),
            scrollable(f32::from(handle.max_offset().width)),
        );
        let list = uniform_list(
            "data-grid-rows",
            self.model.rows.len(),
            cx.processor(move |this, range: Range<usize>, window, cx| {
                if this.model.wants_more(range.end) {
                    cx.emit(GridEvent::FetchMore);
                }
                let focused = this.focus.is_focused(window);
                range
                    .filter(|&r| r < this.model.rows.len())
                    .map(|r| this.row(r, &m, focused, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(0))
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .track_scroll(self.scroll.clone())
        .flex_1();
        let bars = gpui::canvas(
            move |bounds, _, _| {
                let off = handle.offset();
                let max = handle.max_offset();
                let view = handle.bounds().size;
                let track_w = f32::from(bounds.size.width) - gutter_w;
                let v = thumb(
                    f32::from(view.height),
                    f32::from(max.height),
                    -f32::from(off.y),
                    f32::from(bounds.size.height),
                    min_len,
                )
                .map(|(top, len)| {
                    Bounds::new(
                        point(bounds.right() - bar_w - inset, bounds.top() + px(top)),
                        size(bar_w, px(len)),
                    )
                });
                let h = thumb(
                    f32::from(view.width) - gutter_w,
                    f32::from(max.width),
                    -f32::from(off.x),
                    track_w,
                    min_len,
                )
                .map(|(left, len)| {
                    Bounds::new(
                        point(
                            bounds.left() + px(gutter_w + left),
                            bounds.bottom() - bar_w - inset,
                        ),
                        size(px(len), bar_w),
                    )
                });
                [v, h]
            },
            move |_, thumbs, window, cx| {
                let max = painted.max_offset();
                let now = (
                    scrollable(f32::from(max.height)),
                    scrollable(f32::from(max.width)),
                );
                if now != (can_v, can_h) {
                    grid.update(cx, |_, cx| cx.notify()).ok();
                }
                for b in thumbs.into_iter().flatten() {
                    window.paint_quad(fill(b, color).corner_radii(bar_w / 2.));
                }
            },
        )
        .absolute()
        .size_full();
        let strip = |id: &'static str, vertical: bool, cx: &mut Context<Self>| {
            div().id(id).absolute().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus);
                    this.drag = Some(if vertical { Drag::VBar } else { Drag::HBar });
                    this.bar_to(e.position, vertical, cx);
                }),
            )
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .child(bars)
            .when(can_v, |d| {
                d.child(
                    strip("grid-vbar", true, cx)
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(px(10.)),
                )
            })
            .when(can_h, |d| {
                d.child(
                    strip("grid-hbar", false, cx)
                        .left(px(gutter_w))
                        .right_0()
                        .bottom_0()
                        .h(px(10.)),
                )
            })
            .into_any_element()
    }

    fn row(&self, row: usize, m: &Metrics, focused: bool, cx: &mut Context<Self>) -> gpui::Div {
        let t = &m.theme;
        let total = m.lefts.last().copied().unwrap_or(0.);
        let in_rows = self
            .model
            .selection()
            .is_some_and(|(rows, _)| rows.contains(&row));
        let cells = m.vis.clone().map(|c| {
            let width = m.lefts[c + 1] - m.lefts[c];
            let numeric = self.model.columns.get(c).is_some_and(|col| col.numeric);
            let CellStyle { bg, fg, strike } = self.cell_colors(row, c, t);
            let value = self
                .model
                .rows
                .get(row)
                .and_then(|r| r.get(c))
                .and_then(|v| v.as_ref());
            let text = match value {
                Some(v) => div()
                    .child(display_text(v))
                    .text_color(fg)
                    .when(strike, |d| d.line_through()),
                None => div()
                    .child(SharedString::from("NULL"))
                    .text_color(t.color.content_muted)
                    .italic(),
            };
            let active = focused && self.model.active == Some(CellPos { row, col: c });
            div()
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .px(m.pad)
                .flex()
                .items_center()
                .overflow_hidden()
                .border_r_1()
                .border_color(t.color.border)
                .when_some(bg, |d, bg| d.bg(bg))
                .child(
                    match self.editing.as_ref().filter(|e| e.row == row && e.col == c) {
                        Some(e) => div()
                            .w_full()
                            .h_full()
                            .border_1()
                            .border_color(t.color.accent)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    if !matches!(this.drag, Some(Drag::VBar | Drag::HBar)) {
                                        cx.stop_propagation();
                                    }
                                }),
                            )
                            .child(e.input.clone())
                            .into_any_element(),
                        None => text
                            .w_full()
                            .when(numeric, |d| d.text_right())
                            .ellipsis()
                            .into_any_element(),
                    },
                )
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .inset_0()
                            .border_1()
                            .border_color(t.color.focus_ring),
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        if matches!(this.drag, Some(Drag::VBar | Drag::HBar)) {
                            return;
                        }
                        let pos = CellPos { row, col: c };
                        this.click(pos, e.modifiers.shift, e.click_count, window, cx);
                        if e.click_count == 1 && this.editing.is_none() {
                            this.drag = Some(Drag::Select { rows: false });
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        if matches!(this.drag, Some(Drag::VBar | Drag::HBar)) {
                            return;
                        }
                        this.right_click(CellPos { row, col: c }, e.position, window, cx)
                    }),
                )
        });
        let number_color = if in_rows {
            t.color.content
        } else {
            t.color.content_muted
        };
        let gutter = div()
            .absolute()
            .top_0()
            .left(px(m.scroll_x))
            .w(px(m.gutter_w))
            .h_full()
            .flex()
            .items_center()
            .justify_end()
            .px(m.pad)
            .block_mouse_except_scroll()
            .bg(t.color.surface)
            .border_r_1()
            .border_color(t.color.border)
            .text_color(number_color)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    if matches!(this.drag, Some(Drag::VBar | Drag::HBar)) {
                        return;
                    }
                    window.focus(&this.focus);
                    this.model.select_row(row, e.modifiers.shift);
                    if e.click_count == 1 {
                        this.drag = Some(Drag::Select { rows: true });
                    }
                    cx.notify();
                }),
            )
            .when_some(self.gutter_mark(row, t), |d, (glyph, color)| {
                d.child(div().flex_none().mr(px(4.)).text_color(color).child(glyph))
            })
            .child((row + 1).to_string());
        div()
            .relative()
            .flex()
            .w(px(m.gutter_w + total))
            .h(m.row_h)
            .border_b_1()
            .border_color(t.color.border)
            .child(div().flex_none().w(px(m.gutter_w + m.lefts[m.vis.start])))
            .children(cells)
            .child(gutter)
    }
}

impl Render for DataGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(editor) = self.refocus_pending.take()
            && editor.is_focused(window)
        {
            window.focus(&self.focus);
        }
        let shown = self.value_open.then(|| self.inspected()).flatten();
        let m = self.metrics(window, cx);
        let t = m.theme.clone();
        if self.dim.running() {
            window.request_animation_frame();
        }
        let root = div()
            .id("data-grid")
            .key_context("DataGrid")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(t.color.surface)
            .font_family(t.typography.mono.clone())
            .text_size(t.typography.caption)
            .opacity(self.dim.value())
            .relative()
            .when(matches!(self.drag, Some(Drag::Column { .. })), |d| {
                d.cursor(CursorStyle::ResizeLeftRight)
            })
            .children(self.drag_hook(cx))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(-1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &Left, _, cx| this.step(0, -1, false, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.step(0, 1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step(-1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step(1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.step(0, -1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.step(0, 1, true, cx)))
            .on_action(cx.listener(|this, _: &JumpUp, _, cx| this.step(-isize::MAX, 0, false, cx)))
            .on_action(cx.listener(|this, _: &JumpDown, _, cx| this.step(isize::MAX, 0, false, cx)))
            .on_action(
                cx.listener(|this, _: &JumpLeft, _, cx| this.step(0, -isize::MAX, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &JumpRight, _, cx| this.step(0, isize::MAX, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectJumpUp, _, cx| this.step(-isize::MAX, 0, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectJumpDown, _, cx| this.step(isize::MAX, 0, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectJumpLeft, _, cx| this.step(0, -isize::MAX, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectJumpRight, _, cx| this.step(0, isize::MAX, true, cx)),
            )
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.page(-1, false, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.page(1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.page(-1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.page(1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                if this.editing.is_some() {
                    return cx.propagate();
                }
                this.model.select_all();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Edit, _, cx| {
                if let Some(pos) = this.model.active {
                    this.request_edit(pos, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleValue, _, cx| {
                if this.editing.is_some() {
                    return cx.propagate();
                }
                if this.value_open {
                    this.close_value(cx);
                } else {
                    this.show_value(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseValue, window, cx| {
                if this.editing.is_some() || !this.value_open {
                    return cx.propagate();
                }
                this.close_value(cx);
                window.focus(&this.focus);
            }));
        if self.model.columns.is_empty() {
            return root;
        }
        let fresh = self.opened.is_some_and(|o| o.running(t.motion.fast));
        let header = self.header(&m, cx);
        let body = self.body(m, cx);
        let grid = motion::animate_enter(
            t.motion.reduced,
            fresh,
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(header)
                .child(body),
            ("data-grid-in", self.generation),
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        let panel = self.value_open.then(|| {
            let fresh = self.value_opened.is_some_and(|o| o.running(t.motion.base));
            motion::animate_enter(
                t.motion.reduced,
                fresh,
                self.value_panel(shown, cx).relative(),
                ("grid-value-in", self.generation),
                Animation::new(t.motion.base).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d).left(px((1. - d) * 16.)),
            )
        });
        root.child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h_0()
                .child(grid)
                .children(panel),
        )
    }
}
