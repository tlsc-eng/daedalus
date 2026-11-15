mod model;

use std::ops::Range;
use std::time::Duration;

use gpui::{
    Animation, AnyElement, App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Font,
    FontFeatures, FontStyle, FontWeight, Hsla, KeyBinding, ListHorizontalSizingBehavior,
    ListSizingBehavior, MouseButton, MouseDownEvent, Render, ScrollStrategy, SharedString, Task,
    UniformListScrollHandle, Window, actions, div, point, prelude::*, px, uniform_list,
};

use crate::components::Tooltip;
use crate::motion::{self, Busy, Glide, Opening};
use crate::{ActiveTheme, Ellipsis, Size, Space, Theme};

pub use model::GridColumn;
use model::{
    CellPos, GridModel, column_width, display_text, gutter_chars, reveal_x, scroll_strategy,
    visible_columns,
};

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
        Copy
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
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridEvent {
    /// The viewport is near the last loaded row and more rows exist.
    FetchMore,
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
            let first = (scroll_y / self.row_h).ceil() as usize;
            let last = ((scroll_y + body_h) / self.row_h).floor() as usize;
            if let Some(strategy) = scroll_strategy(pos.row, first, last) {
                self.scroll.scroll_to_item(pos.row, strategy);
            }
        }
    }

    fn step(&mut self, d_row: isize, d_col: isize, extend: bool, cx: &mut Context<Self>) {
        if let Some(pos) = self.model.step(d_row, d_col, extend) {
            self.reveal(pos);
        }
        cx.notify();
    }

    fn click(&mut self, pos: CellPos, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.model.select(pos, extend);
        self.reveal(pos);
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.model.copy_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn in_selection(&self, row: usize, col: usize) -> bool {
        self.model
            .selection()
            .is_some_and(|(rows, cols)| rows.contains(&row) && cols.contains(&col))
    }

    /// The single seam for cell tints, so editing can add pending states later.
    fn cell_colors(&self, row: usize, col: usize, t: &Theme) -> (Option<Hsla>, Hsla) {
        let bg = self
            .in_selection(row, col)
            .then_some(t.color.surface_accent);
        (bg, t.color.content)
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
            gutter_chars(self.model.rows.len()),
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
        self.row_h = f32::from(t.size(Size::Row));
        Metrics {
            row_h: t.size(Size::Row),
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
            let (bg, fg) = self.cell_colors(row, c, t);
            let value = self
                .model
                .rows
                .get(row)
                .and_then(|r| r.get(c))
                .and_then(|v| v.as_ref());
            let text = match value {
                Some(v) => div().child(display_text(v)).text_color(fg),
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
                .child(text.w_full().when(numeric, |d| d.text_right()).ellipsis())
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
                        this.click(CellPos { row, col: c }, e.modifiers.shift, window, cx)
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
                this.model.select_all();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)));
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
