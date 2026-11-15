mod model;

use std::ops::Range;
use std::time::Duration;

use gpui::{
    Animation, AnyElement, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Font, FontFeatures, FontStyle, FontWeight, Hsla, KeyBinding,
    ListHorizontalSizingBehavior, ListSizingBehavior, MouseButton, MouseDownEvent, Pixels, Point,
    Render, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, Window,
    actions, div, point, prelude::*, px, uniform_list,
};

use crate::components::Tooltip;
use crate::motion::{self, Busy, Glide, Opening};
use crate::{ActiveTheme, Ellipsis, InputEvent, Size, Space, TextInput, Theme};

use model::{
    CellPos, GridModel, Kind, column_width, display_text, gutter_chars, reveal_x, scroll_strategy,
    style_for, visible_columns, visible_rows,
};
pub use model::{GridColumn, GridMarks, RowMark};

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
        Edit
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
    editable: bool,
    marks: GridMarks,
    editing: Option<Editing>,
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
            editable: false,
            marks: GridMarks::default(),
            editing: None,
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
            self.refocus_if_focused(editing.input.focus_handle(cx), cx);
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

    /// `show` has no window, so the check and the focus move run in a deferred window update.
    fn refocus_if_focused(&self, editor: FocusHandle, cx: &mut Context<Self>) {
        let (Some(window), grid) = (cx.active_window(), self.focus.clone()) else {
            return;
        };
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, _| {
                    if editor.is_focused(window) {
                        window.focus(&grid);
                    }
                })
                .ok();
        });
    }

    fn request_edit(&mut self, pos: CellPos, cx: &mut Context<Self>) {
        if self.editable && self.editing.is_none() {
            cx.emit(GridEvent::EditRequested {
                row: pos.row,
                col: pos.col,
            });
        }
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
        let body_h = f32::from(base.bounds().size.height);
        let scroll_y = -f32::from(offset.y);
        if self.row_h > 0. {
            let (first, last) = visible_rows(scroll_y, body_h, self.row_h);
            if let Some(strategy) = scroll_strategy(pos.row, first, last) {
                self.scroll.scroll_to_item(pos.row, strategy);
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
        let mut lefts = vec![0.];
        for &chars in &self.model.chars {
            let w = column_width(chars, char_w, f32::from(pad), cap);
            lefts.push(lefts.last().copied().unwrap_or(0.) + w);
        }
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

    fn header(&self, m: &Metrics) -> AnyElement {
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
        uniform_list(
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
        .flex_1()
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
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                        let pos = CellPos { row, col: c };
                        this.click(pos, e.modifiers.shift, e.click_count, window, cx)
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
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
                    window.focus(&this.focus);
                    this.model.select_row(row, e.modifiers.shift);
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
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(-1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &Left, _, cx| this.step(0, -1, false, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.step(0, 1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step(-1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step(1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.step(0, -1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.step(0, 1, true, cx)))
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
            }));
        if self.model.columns.is_empty() {
            return root;
        }
        let fresh = self.opened.is_some_and(|o| o.running(t.motion.fast));
        let header = self.header(&m);
        let body = self.body(m, cx);
        root.child(motion::animate_enter(
            t.motion.reduced,
            fresh,
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(header)
                .child(body),
            ("data-grid-in", self.generation),
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        ))
    }
}
