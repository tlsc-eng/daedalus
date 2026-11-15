use std::sync::Arc;

use daedalus_db::Engine;
use daedalus_db::catalog::Table;
use daedalus_db::edit::{self, PAGE_ROWS, Shape};
use daedalus_db::value::{Cell, Column};
use daedalus_db::worker::{Command, Handle, RunId, RunMode, SessionId, Update};
use daedalus_ui::motion::{self, Busy};
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, DataGrid, DotState, InputEvent, Space, StatusDot, TextInput,
};
use gpui::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Render, Subscription, Task, Window, div, prelude::*,
};

use super::console::{Info, grid_columns};
use super::explorer::{data_color, dot_shape, engine_label, tag};
use crate::shell::sidebar::folder_color;

pub(in crate::shell) enum DbTableEvent {
    Connect,
}

enum TablePhase {
    Idle,
    Connecting(Busy),
    Connected,
    Failed(String),
}

pub(in crate::shell) struct DbTableView {
    key: String,
    schema: String,
    table: String,
    info: Option<Info>,
    phase: TablePhase,
    session: Option<SessionId>,
    handle: Option<Handle>,
    spec: Option<Arc<Table>>,
    shape: Option<Result<Shape, &'static str>>,
    filter: Entity<TextInput>,
    order: Entity<TextInput>,
    grid: Entity<DataGrid>,
    offset: usize,
    want: usize,
    has_next: bool,
    run: Option<RunId>,
    next_run: u64,
    pending: Vec<Column>,
    buffer: Vec<Vec<Cell>>,
    #[allow(dead_code)]
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    #[allow(dead_code)]
    map: Vec<Option<usize>>,
    error: Option<String>,
    busy: Option<Busy>,
    spin: Option<Task<()>>,
    _release: Subscription,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DbTableEvent> for DbTableView {}

impl Focusable for DbTableView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }
}

impl DbTableView {
    pub fn new(
        key: String,
        schema: String,
        table: String,
        spec: Option<Arc<Table>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| TextInput::new("WHERE", cx));
        let order = cx.new(|cx| TextInput::new("ORDER BY", cx));
        let grid = cx.new(DataGrid::new);
        let subs = [&filter, &order]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Submit => this.load(0, cx),
                    InputEvent::Cancel => this.stop(cx),
                    _ => {}
                })
            })
            .collect();
        Self {
            key,
            schema,
            table,
            info: None,
            phase: TablePhase::Idle,
            session: None,
            handle: None,
            spec,
            shape: None,
            filter,
            order,
            grid,
            offset: 0,
            want: 0,
            has_next: false,
            run: None,
            next_run: 0,
            pending: Vec::new(),
            buffer: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            map: Vec::new(),
            error: None,
            busy: None,
            spin: None,
            _release: cx.on_release(|this, _| this.disconnect()),
            _subs: subs,
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn is(&self, key: &str, schema: &str, table: &str) -> bool {
        self.key == key && self.schema == schema && self.table == table
    }

    pub fn label(&self) -> String {
        self.table.clone()
    }

    pub fn needs_connect(&self) -> bool {
        matches!(self.phase, TablePhase::Idle | TablePhase::Failed(_))
    }

    pub fn setup_error(&self) -> Option<String> {
        None
    }

    pub fn connecting(
        &mut self,
        session: SessionId,
        handle: Handle,
        info: Info,
        cx: &mut Context<Self>,
    ) {
        self.session = Some(session);
        self.handle = Some(handle);
        self.info = Some(info);
        self.run = None;
        self.busy = None;
        self.phase = TablePhase::Connecting(Busy::start());
        self.start_spin(cx);
        cx.notify();
    }

    /// Records which connection this is while a password sheet is open, without starting a session.
    pub fn prompting(&mut self, info: Info, cx: &mut Context<Self>) {
        self.info = Some(info);
        cx.notify();
    }

    pub fn idle(&mut self, cx: &mut Context<Self>) {
        self.phase = TablePhase::Idle;
        self.session = None;
        cx.notify();
    }

    pub fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.phase = TablePhase::Failed(message.into());
        self.session = None;
        self.run = None;
        self.busy = None;
        self.grid.update(cx, |g, cx| g.set_stale(false, cx));
        cx.notify();
    }

    /// Hangs up the session; never after a `Failed`, which already ended it.
    fn disconnect(&mut self) {
        if let (Some(session), Some(handle)) = (self.session.take(), &self.handle) {
            handle.send(Command::Disconnect { session });
        }
    }

    pub fn on_update(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Connected { .. } => {
                self.phase = TablePhase::Connected;
                match self.spec {
                    Some(_) => self.load(0, cx),
                    None => {
                        if let (Some(session), Some(handle)) = (self.session, &self.handle) {
                            handle.send(Command::Introspect { session });
                        }
                    }
                }
            }
            Update::Failed { error, .. } => {
                if matches!(self.phase, TablePhase::Connected) {
                    self.error = Some(error.clone());
                }
                self.phase = TablePhase::Failed(error);
                self.session = None;
                self.run = None;
                self.busy = None;
                self.grid.update(cx, |g, cx| g.set_stale(false, cx));
            }
            Update::Catalog { catalog, .. } => {
                if self.spec.is_some() {
                    return;
                }
                match catalog {
                    Ok(c) => {
                        let found = c
                            .schemas
                            .iter()
                            .find(|s| s.name == self.schema)
                            .and_then(|s| s.tables.iter().find(|t| t.name == self.table));
                        match found {
                            Some(t) => {
                                self.spec = Some(Arc::new(t.clone()));
                                self.load(0, cx);
                            }
                            None => self.fail("This table no longer exists.", cx),
                        }
                    }
                    Err(e) => self.fail(e, cx),
                }
            }
            Update::Columns { run, columns, .. } if self.run == Some(run) => {
                self.pending = columns;
            }
            Update::Rows { run, rows, .. } if self.run == Some(run) => {
                self.buffer.extend(rows);
            }
            Update::Done { run, truncated, .. } if self.run == Some(run) => {
                self.finish(truncated, cx);
            }
            Update::RunError { run, message, .. } if self.run == Some(run) => {
                self.run = None;
                self.busy = None;
                self.error = Some(message);
                self.grid.update(cx, |g, cx| g.set_stale(false, cx));
            }
            _ => {}
        }
        cx.notify();
    }

    fn finish(&mut self, truncated: bool, cx: &mut Context<Self>) {
        self.run = None;
        self.busy = None;
        let mut rows = std::mem::take(&mut self.buffer);
        self.has_next = rows.len() > PAGE_ROWS;
        rows.truncate(PAGE_ROWS);
        let columns = std::mem::take(&mut self.pending);
        self.offset = self.want;
        self.map = match &self.spec {
            Some(spec) => map_columns_by_name(&columns, spec),
            None => Vec::new(),
        };
        self.shape = match (&self.info, &self.spec) {
            (Some(info), Some(spec)) => {
                Some(editability(info.engine, info.read_only, spec, &self.map))
            }
            _ => None,
        };
        self.grid.update(cx, |g, cx| {
            g.show(grid_columns(&columns), rows.clone(), false, cx);
            g.finish(truncated, cx);
            g.set_stale(false, cx);
        });
        self.columns = columns;
        self.rows = rows;
    }

    fn load(&mut self, offset: usize, cx: &mut Context<Self>) {
        let (Some(session), Some(handle), Some(info), Some(spec)) = (
            self.session,
            self.handle.clone(),
            self.info.clone(),
            self.spec.clone(),
        ) else {
            return;
        };
        if !matches!(self.phase, TablePhase::Connected) {
            return;
        }
        let sql = edit::select_page(
            info.engine,
            &self.schema,
            &spec,
            self.filter.read(cx).text(),
            self.order.read(cx).text(),
            offset,
        );
        let sql = match sql {
            Ok(sql) => sql,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        if let Some(old) = self.run {
            handle.send(Command::Cancel { session, run: old });
        }
        let run = RunId(self.next_run);
        self.next_run += 1;
        self.run = Some(run);
        self.want = offset;
        self.error = None;
        self.pending.clear();
        self.buffer.clear();
        handle.send(Command::Run {
            session,
            run,
            sql,
            page: PAGE_ROWS + 1,
            timeout: None,
            mode: run_mode(info.read_only),
        });
        self.busy = Some(Busy::start());
        self.start_spin(cx);
        self.grid.update(cx, |g, cx| g.set_stale(true, cx));
        cx.notify();
    }

    /// Drops the run so its late updates are ignored, and keeps the page already shown.
    fn stop(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(handle), Some(run)) =
            (self.session, &self.handle, self.run.take())
        else {
            return;
        };
        handle.send(Command::Cancel { session, run });
        self.busy = None;
        self.grid.update(cx, |g, cx| g.set_stale(false, cx));
        cx.notify();
    }

    fn start_spin(&mut self, cx: &mut Context<Self>) {
        if self.spin.is_some() {
            return;
        }
        let start = std::time::Instant::now();
        self.spin = Some(cx.spawn(async move |this, cx| {
            loop {
                let wait = motion::until_next_spin(start.elapsed());
                cx.background_executor().timer(wait).await;
                let kept = this.update(cx, |this, cx| {
                    let kept =
                        this.busy.is_some() || matches!(this.phase, TablePhase::Connecting(_));
                    match kept {
                        true => cx.notify(),
                        false => this.spin = None,
                    }
                    kept
                });
                if !kept.unwrap_or(false) {
                    break;
                }
            }
        }));
    }
}

fn run_mode(read_only: bool) -> RunMode {
    match read_only {
        true => RunMode::ReadOnly,
        false => RunMode::Auto,
    }
}

fn map_columns_by_name(result: &[Column], spec: &Table) -> Vec<Option<usize>> {
    result
        .iter()
        .map(|c| spec.columns.iter().position(|s| s.name == c.name))
        .collect()
}

fn editability(
    engine: Engine,
    read_only: bool,
    spec: &Table,
    map: &[Option<usize>],
) -> Result<Shape, &'static str> {
    if read_only {
        return Err("Read-only connection.");
    }
    let shape = edit::shape(engine, spec)?;
    if shape.pk.iter().any(|i| !map.contains(&Some(*i))) {
        return Err("Refresh the Data panel: the table changed.");
    }
    Ok(shape)
}

fn page_label(offset: usize, shown: usize) -> String {
    match shown {
        0 => "No rows".to_owned(),
        n => format!("Rows {}–{}", offset + 1, offset + n),
    }
}

impl Render for DbTableView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let toolbar = self.render_toolbar(cx);
        let footer = self.render_footer(cx);
        let banner = self.error.as_ref().map(|message| {
            div()
                .flex_none()
                .px(t.space(Space::Md))
                .py(t.space(Space::Sm))
                .bg(t.color.danger_surface)
                .text_size(t.typography.caption)
                .text_color(t.color.danger)
                .child(message.clone())
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .key_context("DbTable")
            .child(toolbar)
            .children(banner)
            .child(div().flex_1().min_h_0().child(self.grid.clone()))
            .child(footer)
    }
}

impl DbTableView {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let (delay, reduced) = (t.motion.indicator_delay, t.motion.reduced);
        let chip = self.info.as_ref().map(|info| {
            let tint = data_color(info.color.as_deref())
                .map_or(t.color.content_muted, |c| folder_color(&t, c));
            div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .child(dot_shape(true, tint, &t))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(info.name.clone()),
                )
                .child(
                    div()
                        .text_color(t.color.content_muted)
                        .child(engine_label(info.engine)),
                )
                .when(info.read_only, |el| {
                    el.child(tag("read-only", t.color.content_muted))
                })
                .when(info.production, |el| {
                    el.child(tag("production", t.color.warning))
                })
        });
        let status = match &self.phase {
            TablePhase::Connecting(busy) => {
                let angle = match reduced {
                    true => 0.,
                    false => motion::spin_turn(busy.since().elapsed()),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .text_color(t.color.content_muted)
                    .when(busy.shows(delay), |el| {
                        el.child(StatusDot::new(DotState::Running).rotation(angle))
                    })
                    .child("Connecting…")
                    .into_any_element()
            }
            TablePhase::Connected => {
                let ready = self.run.is_none();
                Button::new("db-table-reload", "Reload", ButtonKind::Secondary)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if ready {
                            this.load(this.offset, cx)
                        }
                    }))
                    .into_any_element()
            }
            TablePhase::Idle | TablePhase::Failed(_) => {
                let message = match &self.phase {
                    TablePhase::Failed(m) => Some(m.clone()),
                    _ => None,
                };
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .min_w_0()
                    .when_some(message, |el, m| {
                        el.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(t.color.danger)
                                .child(m),
                        )
                    })
                    .child(
                        Button::new("db-table-connect", "Connect", ButtonKind::Secondary).on_click(
                            cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(DbTableEvent::Connect)),
                        ),
                    )
                    .into_any_element()
            }
        };
        div()
            .h(t.ui(36.))
            .flex_none()
            .px(t.space(Space::Md))
            .flex()
            .items_center()
            .gap(t.space(Space::Md))
            .border_b_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption)
            .text_color(t.color.content_secondary)
            .children(chip)
            .child(div().flex_1().min_w_0().child(self.filter.clone()))
            .child(div().flex_1().min_w_0().child(self.order.clone()))
            .child(status)
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let idle = self.run.is_none() && matches!(self.phase, TablePhase::Connected);
        let step = |id: &'static str,
                    label: &'static str,
                    on: bool,
                    forward: bool,
                    cx: &mut Context<Self>| match on {
            true => Button::new(id, label, ButtonKind::Ghost)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let to = match forward {
                        true => this.offset + PAGE_ROWS,
                        false => this.offset.saturating_sub(PAGE_ROWS),
                    };
                    this.load(to, cx)
                }))
                .into_any_element(),
            false => div()
                .px(t.ui(12.))
                .text_color(t.color.content_muted)
                .child(label)
                .into_any_element(),
        };
        let prev = step("db-table-prev", "‹", idle && self.offset > 0, false, cx);
        let next = step("db-table-next", "›", idle && self.has_next, true, cx);
        let note = match &self.shape {
            Some(Err(reason)) => Some(*reason),
            _ => None,
        };
        let stop = self.run.is_some().then(|| {
            Button::new("db-table-stop", "Stop", ButtonKind::Danger)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop(cx)))
        });
        div()
            .h(t.ui(28.))
            .flex_none()
            .px(t.space(Space::Md))
            .flex()
            .items_center()
            .gap(t.space(Space::Md))
            .border_t_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption)
            .text_color(t.color.content_muted)
            .child(prev)
            .child(page_label(self.offset, self.rows.len()))
            .child(next)
            .child(div().flex_1())
            .children(note)
            .children(stop)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daedalus_db::catalog::{CatalogColumn, TableKind};

    fn spec() -> Table {
        let column = |name: &str, pk: bool| CatalogColumn {
            name: name.into(),
            type_name: "int".into(),
            nullable: !pk,
            pk,
        };
        Table {
            name: "t".into(),
            kind: TableKind::Table,
            columns: vec![column("id", true), column("name", false)],
        }
    }

    fn result(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|n| Column {
                name: (*n).into(),
                type_name: "int".into(),
                table: None,
            })
            .collect()
    }

    #[test]
    fn columns_map_by_name_and_a_missing_pk_is_not_editable() {
        let spec = spec();
        let map = map_columns_by_name(&result(&["name", "id", "extra"]), &spec);
        assert_eq!(map, [Some(1), Some(0), None]);
        assert!(editability(Engine::Postgres, false, &spec, &map).is_ok());
        let map = map_columns_by_name(&result(&["name"]), &spec);
        assert_eq!(
            editability(Engine::Postgres, false, &spec, &map),
            Err("Refresh the Data panel: the table changed.")
        );
    }

    #[test]
    fn page_label_counts_from_the_offset() {
        assert_eq!(page_label(500, 500), "Rows 501–1000");
        assert_eq!(page_label(0, 3), "Rows 1–3");
        assert_eq!(page_label(0, 0), "No rows");
    }

    #[test]
    fn run_mode_is_read_only_on_read_only_connections() {
        assert_eq!(run_mode(true), RunMode::ReadOnly);
        assert_eq!(run_mode(false), RunMode::Auto);
        let spec = spec();
        assert_eq!(
            editability(Engine::Postgres, true, &spec, &[Some(0), Some(1)]),
            Err("Read-only connection.")
        );
    }
}
