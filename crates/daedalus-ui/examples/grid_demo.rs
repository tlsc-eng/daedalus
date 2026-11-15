//! Opens the result grid on synthetic data: cargo run -p daedalus-ui --example grid_demo

use std::time::Duration;

use daedalus_ui::{ActiveTheme, Button, ButtonKind, DataGrid, GridColumn, GridEvent};
use gpui::{
    AnyView, App, Application, Bounds, Context, Entity, IntoElement, ParentElement, Render,
    StyleRefinement, Styled, Subscription, Task, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};

const TOTAL: usize = 20_000;
const PAGE: usize = 500;
const COLUMNS: usize = 36;

fn columns() -> Vec<GridColumn> {
    let col = |name: &str, label: &str, numeric: bool| GridColumn {
        name: name.to_string().into(),
        type_label: label.to_string().into(),
        numeric,
    };
    let mut cols = vec![
        col("id", "integer", true),
        col("total", "numeric(12,2)", true),
        col("email", "text", false),
        col("note", "text", false),
        col("body", "text", false),
    ];
    cols.extend((5..COLUMNS).map(|i| col(&format!("c{i:02}"), "text", false)));
    cols
}

fn row(seed: u64, i: usize) -> Vec<Option<String>> {
    let n = i as u64 + seed;
    let note = if i.is_multiple_of(7) {
        None
    } else if i.is_multiple_of(5) {
        Some(String::new())
    } else if i.is_multiple_of(11) {
        Some("tab\there".to_string())
    } else if i.is_multiple_of(13) {
        Some("line one\nline two".to_string())
    } else {
        Some(format!("note {n}"))
    };
    let mut cells = vec![
        Some((i + 1).to_string()),
        Some(format!("{}.{:02}", n * 37 % 10_000, n % 100)),
        Some(format!("user{n}@example.com")),
        note,
        Some(["0123456789"; 30].concat()),
    ];
    cells.extend((5..COLUMNS).map(|c| Some(format!("v{n}-{c}"))));
    cells
}

fn page(seed: u64, from: usize) -> Vec<Vec<Option<String>>> {
    (from..(from + PAGE).min(TOTAL))
        .map(|i| row(seed, i))
        .collect()
}

struct Demo {
    grid: Entity<DataGrid>,
    served: usize,
    seed: u64,
    fetch: Option<Task<()>>,
    rerun: Option<Task<()>>,
    _sub: Subscription,
}

impl Demo {
    fn new(cx: &mut Context<Self>) -> Self {
        let grid = cx.new(DataGrid::new);
        let first = page(0, 0);
        let served = first.len();
        grid.update(cx, |g, cx| g.show(columns(), first, served < TOTAL, cx));
        let sub = cx.subscribe(&grid, |this, _, event: &GridEvent, cx| {
            if *event == GridEvent::FetchMore && this.rerun.is_none() {
                this.fetch_more(cx);
            }
        });
        Self {
            grid,
            served,
            seed: 0,
            fetch: None,
            rerun: None,
            _sub: sub,
        }
    }

    fn fetch_more(&mut self, cx: &mut Context<Self>) {
        self.fetch = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            this.update(cx, |this, cx| {
                let rows = page(this.seed, this.served);
                this.served += rows.len();
                let more = this.served < TOTAL;
                let grid = this.grid.clone();
                grid.update(cx, |g, cx| {
                    g.append(rows, more, cx);
                    if !more {
                        g.finish(false, cx);
                    }
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn rerun(&mut self, millis: u64, cx: &mut Context<Self>) {
        self.fetch = None;
        self.grid.update(cx, |g, cx| g.set_stale(true, cx));
        self.rerun = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(millis))
                .await;
            this.update(cx, |this, cx| {
                this.seed += 1000;
                let first = page(this.seed, 0);
                this.served = first.len();
                let more = this.served < TOTAL;
                this.grid
                    .update(cx, |g, cx| g.show(columns(), first, more, cx));
                this.rerun = None;
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for Demo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let this = cx.weak_entity();
        let slow = this.clone();
        let caption = format!(
            "{} rows{}",
            self.served,
            if self.served < TOTAL { " · more" } else { "" }
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .text_color(t.color.content)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .child(
                        Button::new("rerun-slow", "Re-run (slow)", ButtonKind::Secondary).on_click(
                            move |_, _, cx| {
                                slow.update(cx, |d, cx| d.rerun(1500, cx)).ok();
                            },
                        ),
                    )
                    .child(
                        Button::new("rerun-fast", "Re-run (fast)", ButtonKind::Secondary).on_click(
                            move |_, _, cx| {
                                this.update(cx, |d, cx| d.rerun(100, cx)).ok();
                            },
                        ),
                    )
                    .child(div().text_color(t.color.content_muted).child(caption)),
            )
            .child(div().flex_1().min_h_0().child(
                AnyView::from(self.grid.clone()).cached(StyleRefinement::default().size_full()),
            ))
    }
}

fn main() {
    Application::new()
        .with_assets(daedalus_ui::Assets)
        .run(|cx: &mut App| {
            daedalus_ui::init(cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1100.), px(700.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                |_, cx| cx.new(Demo::new),
            )
            .ok();
            cx.activate(true);
            cx.on_window_closed(|cx| cx.quit()).detach();
        });
}
