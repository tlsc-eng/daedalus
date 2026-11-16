use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use daedalus_db::history::{self as store, Entry};
use daedalus_ui::{ActiveTheme, InputEvent, Space, TextInput};
use gpui::{
    ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    MouseDownEvent, Pixels, Point, Render, ScrollHandle, Window, div, prelude::*,
};

const LIMIT: usize = 200;

pub(super) enum HistoryEvent {
    Insert(String),
    Close { at: Option<Point<Pixels>> },
}

enum Loaded {
    Pending,
    Failed,
    Ready(Vec<Entry>),
}

pub(super) struct HistoryPopover {
    input: Entity<TextInput>,
    loaded: Loaded,
    shown: Vec<Entry>,
    selected: usize,
    scroll: ScrollHandle,
    _sub: gpui::Subscription,
}

impl EventEmitter<HistoryEvent> for HistoryPopover {}

impl Focusable for HistoryPopover {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Compact age for the muted column: minutes, hours or days.
fn ago(now_ms: u64, at_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(at_ms) / 60_000;
    match minutes {
        0 => "now".into(),
        1..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

fn clamp_selection(selected: usize, len: usize) -> usize {
    selected.min(len.saturating_sub(1))
}

impl HistoryPopover {
    pub(super) fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search history…", cx));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Changed => {
                    this.refilter(cx);
                    this.selected = 0;
                    this.scroll.scroll_to_item(0);
                }
                InputEvent::Up => this.step(-1, cx),
                InputEvent::Down => this.step(1, cx),
                InputEvent::Submit => this.insert(this.selected, cx),
                InputEvent::Cancel => cx.emit(HistoryEvent::Close { at: None }),
                InputEvent::SubmitBeside => {}
            },
        );
        window.focus(&input.focus_handle(cx));
        let loaded = match path {
            Some(path) => {
                cx.spawn(async move |this, cx| {
                    let read = cx
                        .background_executor()
                        .spawn(async move { store::load(&path) })
                        .await;
                    this.update(cx, |this, cx| {
                        this.loaded = read.map_or(Loaded::Failed, Loaded::Ready);
                        this.refilter(cx);
                    })
                    .ok();
                })
                .detach();
                Loaded::Pending
            }
            None => Loaded::Ready(Vec::new()),
        };
        Self {
            input,
            loaded,
            shown: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            _sub: sub,
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let Loaded::Ready(entries) = &self.loaded else {
            return;
        };
        let query = self.input.read(cx).text().to_string();
        self.shown = store::search(entries, &query, LIMIT)
            .into_iter()
            .cloned()
            .collect();
        self.selected = clamp_selection(self.selected, self.shown.len());
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let last = self.shown.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    fn insert(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.shown.get(row) {
            cx.emit(HistoryEvent::Insert(entry.sql.clone()));
        }
    }
}

impl Render for HistoryPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let now = now_ms();
        let note = match (&self.loaded, self.shown.is_empty()) {
            (Loaded::Pending, _) => None,
            (Loaded::Failed, _) => Some("Couldn't read history"),
            (Loaded::Ready(all), true) if all.is_empty() => Some("No queries yet"),
            (Loaded::Ready(_), true) => Some("No matches"),
            _ => None,
        };
        let rows: Vec<_> = self
            .shown
            .iter()
            .enumerate()
            .map(|(row, entry)| {
                let selected = row == self.selected;
                let first = entry.sql.lines().next().unwrap_or_default().to_string();
                div()
                    .id(("db-history-row", row))
                    .h(t.ui(28.))
                    .px(t.space(Space::Md))
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Md))
                    .cursor_pointer()
                    .text_color(if selected {
                        t.color.accent
                    } else {
                        t.color.content
                    })
                    .when(!selected, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            this.insert(row, cx);
                        } else {
                            this.selected = row;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(t.typography.mono.clone())
                            .child(first),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.color.content_muted)
                            .child(ago(now, entry.at)),
                    )
            })
            .collect();
        div()
            .absolute()
            .top(t.ui(36.))
            .right(t.space(Space::Md))
            .w(t.ui(420.))
            .flex()
            .flex_col()
            .occlude()
            .on_mouse_down_out(cx.listener(|_, event: &MouseDownEvent, _, cx| {
                cx.emit(HistoryEvent::Close {
                    at: Some(event.position),
                })
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .child(
                div()
                    .p(t.space(Space::Sm))
                    .border_b_1()
                    .border_color(t.color.border)
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .id("db-history-list")
                    .max_h(t.ui(320.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(note.map(|n| {
                        div()
                            .p(t.space(Space::Md))
                            .text_color(t.color.content_muted)
                            .child(n)
                    }))
                    .children(rows),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_picks_the_largest_unit() {
        let now = 10 * 86_400_000;
        assert_eq!(ago(now, now), "now");
        assert_eq!(ago(now, now - 2 * 60_000), "2m");
        assert_eq!(ago(now, now - 3 * 3_600_000), "3h");
        assert_eq!(ago(now, now - 5 * 86_400_000), "5d");
        assert_eq!(ago(now, now + 1), "now");
    }

    #[test]
    fn selection_stays_inside_a_shrinking_list() {
        assert_eq!(clamp_selection(5, 3), 2);
        assert_eq!(clamp_selection(1, 3), 1);
        assert_eq!(clamp_selection(4, 0), 0);
    }
}
