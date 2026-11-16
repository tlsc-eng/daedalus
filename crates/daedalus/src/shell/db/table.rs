mod pending;

use std::sync::Arc;

use daedalus_db::Engine;
use daedalus_db::catalog::{Catalog, Table};
use daedalus_db::edit::{self, PAGE_ROWS, Plan, Shape};
use daedalus_db::export::Format;
use daedalus_db::value::{Cell, Column};
use daedalus_db::worker::{Command, Handle, RunId, RunMode, SessionId, Update};
use daedalus_ui::motion::{self, Busy};
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, ContextMenu, DataGrid, DotState, GridEvent, InputEvent,
    MenuItem, Space, StatusDot, TextInput,
};
use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, Pixels, Point, PromptLevel, Render, SharedString,
    Subscription, Task, Window, div, prelude::*,
};
use pending::{Pending, Slot, can_edit_value, locate};

use super::console::{Info, grid_columns};
use super::explorer::{data_color, dot_shape, engine_label, tag};
use super::exporting::{self, Export, Outcome, Spec};
use crate::shell::lsp::{CONFIRMED, confirm_buttons};
use crate::shell::sidebar::folder_color;

pub(in crate::shell) enum DbTableEvent {
    Connect,
}

/// What to do once the user agrees to drop the pending edits.
#[derive(Clone, Copy)]
enum Discard {
    Revert,
    Filter,
    Load(usize),
    Connect,
}

struct Preview {
    plan: Plan,
    rows_of: Vec<usize>,
    lines: Vec<SharedString>,
}

struct Submitting {
    busy: Busy,
    count: usize,
    rows_of: Vec<usize>,
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
    applied: (String, String),
    has_next: bool,
    run: Option<RunId>,
    next_run: u64,
    pending: Vec<Column>,
    buffer: Vec<Vec<Cell>>,
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    map: Vec<Option<usize>>,
    edits: Pending,
    preview: Option<Preview>,
    submitting: Option<Submitting>,
    failed: Option<usize>,
    stale: bool,
    note: Option<&'static str>,
    menu: Option<(Entity<ContextMenu>, Subscription)>,
    export: Option<Export>,
    export_note: Option<String>,
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
            applied: Default::default(),
            has_next: false,
            run: None,
            next_run: 0,
            pending: Vec::new(),
            buffer: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            map: Vec::new(),
            edits: Pending::default(),
            preview: None,
            submitting: None,
            failed: None,
            stale: false,
            note: None,
            menu: None,
            export: None,
            export_note: None,
            error: None,
            busy: None,
            spin: None,
            _release: cx.on_release(|this, _| this.disconnect()),
            _subs: Vec::new(),
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

    pub fn ended(&self) -> bool {
        self.session.is_none()
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
        self.export = None;
        cx.notify();
    }

    pub fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.disconnect();
        self.phase = TablePhase::Failed(message.into());
        self.export = None;
        self.run = None;
        self.busy = None;
        self.submitting = None;
        self.preview = None;
        self.grid.update(cx, |g, cx| g.set_stale(false, cx));
        self.sync_editable(cx);
        cx.notify();
    }

    /// Hangs up the session; never after a `Failed`, which already ended it.
    pub(super) fn disconnect(&mut self) {
        if let (Some(session), Some(handle)) = (self.session.take(), &self.handle) {
            handle.send(Command::Disconnect { session });
        }
    }

    pub fn on_update(&mut self, mut update: Update, cx: &mut Context<Self>) {
        if let Some(export) = &self.export {
            match export.take(update) {
                None => return,
                Some(rest) => update = rest,
            }
        }
        match update {
            Update::Connected { .. } => {
                self.phase = TablePhase::Connected;
                match self.spec {
                    Some(_) => self.apply(cx),
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
                self.export = None;
                self.run = None;
                self.busy = None;
                self.submitting = None;
                self.preview = None;
                self.grid.update(cx, |g, cx| g.set_stale(false, cx));
                self.sync_editable(cx);
            }
            Update::Catalog { catalog, .. } => {
                if self.spec.is_some() {
                    return;
                }
                match catalog {
                    Ok(c) => match find_table(&c, &self.schema, &self.table) {
                        Ok(t) => {
                            self.spec = Some(t);
                            self.apply(cx);
                        }
                        Err(message) => self.fail(message, cx),
                    },
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
                self.sync_editable(cx);
            }
            Update::Applied { result, .. } => {
                if let Some(sent) = self.submitting.take() {
                    match result {
                        Ok(_) => {
                            self.edits.clear();
                            self.failed = None;
                            self.stale = true;
                            self.sync_marks(cx);
                            self.load(self.offset, cx);
                        }
                        Err(e) => {
                            self.error = Some(e.message);
                            self.failed = e.index.and_then(|i| sent.rows_of.get(i).copied());
                            self.sync_marks(cx);
                        }
                    }
                    self.sync_editable(cx);
                }
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
        self.edits.clear();
        self.failed = None;
        self.preview = None;
        self.stale = false;
        self.note = None;
        self.sync_marks(cx);
        self.sync_editable(cx);
    }

    /// Takes the typed WHERE and ORDER BY as the query that paging and Reload keep running.
    fn apply(&mut self, cx: &mut Context<Self>) {
        self.applied = (
            self.filter.read(cx).text().to_owned(),
            self.order.read(cx).text().to_owned(),
        );
        self.load(0, cx);
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
        if !matches!(self.phase, TablePhase::Connected) || self.export.is_some() {
            return;
        }
        let sql = edit::select_page(
            info.engine,
            &self.schema,
            &spec,
            &self.applied.0,
            &self.applied.1,
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
        self.export_note = None;
        self.pending.clear();
        self.buffer.clear();
        handle.send(Command::Run {
            session,
            run,
            sql,
            page: PAGE_ROWS + 1,
            timeout: None,
            mode: run_mode(info.read_only),
            capped: true,
        });
        self.busy = Some(Busy::start());
        self.start_spin(cx);
        self.grid.update(cx, |g, cx| g.set_stale(true, cx));
        self.sync_editable(cx);
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
        self.sync_editable(cx);
        cx.notify();
    }

    pub fn pending_count(&self) -> usize {
        self.edits.count()
    }

    fn ensure_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self._subs.is_empty() {
            return;
        }
        let mut subs: Vec<Subscription> =
            [&self.filter, &self.order]
                .into_iter()
                .map(|input| {
                    cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Submit => this.guard(Discard::Filter, window, cx),
                            InputEvent::Cancel => this.stop(cx),
                            _ => {}
                        }
                    })
                })
                .collect();
        subs.push(cx.subscribe_in(
            &self.grid,
            window,
            |this, _, event: &GridEvent, window, cx| this.on_grid(event, window, cx),
        ));
        self._subs = subs;
    }

    fn shape_ok(&self) -> Option<&Shape> {
        self.shape.as_ref()?.as_ref().ok()
    }

    fn can_edit(&self) -> bool {
        edit_gate(
            self.shape_ok().is_some(),
            matches!(self.phase, TablePhase::Connected),
            self.run.is_none(),
            self.submitting.is_none() && self.preview.is_none() && self.export.is_none(),
            self.stale,
        )
    }

    fn sync_editable(&self, cx: &mut Context<Self>) {
        let on = self.can_edit();
        self.grid.update(cx, |g, cx| g.set_editable(on, cx));
    }

    fn sync_marks(&self, cx: &mut Context<Self>) {
        let mut marks = self.edits.marks(self.rows.len());
        marks.failed = self.failed;
        self.grid.update(cx, |g, cx| g.set_marks(marks, cx));
        cx.notify();
    }

    fn refresh_marks(&mut self, cx: &mut Context<Self>) {
        self.failed = None;
        self.sync_marks(cx);
    }

    /// The catalog column behind a result column, when this table lets it be edited.
    fn cell_target(&self, col: usize) -> Option<usize> {
        let c = (*self.map.get(col)?)?;
        let editable = self.shape_ok()?.editable.get(c).copied().unwrap_or(false);
        editable.then_some(c)
    }

    fn original(&self, row: usize, col: usize) -> Cell {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
            .flatten()
    }

    fn current(&self, row: usize, col: usize) -> Cell {
        match locate(row, self.rows.len()) {
            Slot::Page(r) => match self.edits.display(r, col) {
                Some(value) => value.clone(),
                None => self.original(r, col),
            },
            Slot::Inserted(i) => self.edits.inserted_value(i, col),
        }
    }

    fn on_grid(&mut self, event: &GridEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            GridEvent::EditRequested { row, col } => self.request_edit(*row, *col, window, cx),
            GridEvent::Edited { row, col, text } => {
                self.set_value(*row, *col, Some(text.clone()), cx)
            }
            GridEvent::ContextMenu { row, col, position } => {
                self.open_menu(*row, *col, *position, window, cx)
            }
            GridEvent::FetchMore => {}
        }
    }

    fn request_edit(
        &mut self,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = self.current(row, col);
        let deleted =
            matches!(locate(row, self.rows.len()), Slot::Page(r) if self.edits.is_deleted(r));
        let too_big = !can_edit_value(value.as_deref());
        if deleted || self.cell_target(col).is_none() || too_big {
            self.note = Some("This cell can't be edited here");
            if too_big && !deleted && self.cell_target(col).is_some() {
                self.grid.update(cx, |g, cx| g.show_value(cx));
            }
            cx.notify();
            return;
        }
        self.note = None;
        let null = value.is_none();
        let initial = value.unwrap_or_default();
        self.grid.update(cx, |g, cx| {
            g.begin_edit(row, col, initial, null, window, cx)
        });
        cx.notify();
    }

    fn set_value(&mut self, row: usize, col: usize, value: Cell, cx: &mut Context<Self>) {
        let shown = match locate(row, self.rows.len()) {
            Slot::Page(r) => {
                let original = self.original(r, col);
                self.edits.set(r, col, value, &original);
                self.edits.display(r, col).cloned().unwrap_or(original)
            }
            Slot::Inserted(i) => {
                self.edits.set_inserted(i, col, value.clone());
                value
            }
        };
        self.grid
            .update(cx, |g, cx| g.set_cell(row, col, shown, cx));
        self.refresh_marks(cx);
    }

    fn restore_row(&mut self, row: usize, cx: &mut Context<Self>) {
        for col in 0..self.columns.len() {
            let original = self.original(row, col);
            self.grid
                .update(cx, |g, cx| g.set_cell(row, col, original, cx));
        }
    }

    fn rebuild_tail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let base = self.rows.len();
        let rows = self.edits.inserted_rows(self.columns.len());
        self.grid
            .update(cx, |g, cx| g.replace_tail(base, rows, window, cx));
        self.refresh_marks(cx);
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        self.edits.add_row();
        self.rebuild_tail(window, cx);
    }

    fn toggle_delete(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        match locate(row, self.rows.len()) {
            Slot::Page(r) => {
                match self.edits.is_deleted(r) {
                    true => self.edits.revert_row(r),
                    false => {
                        self.edits.delete_row(r);
                        self.restore_row(r, cx);
                    }
                }
                self.refresh_marks(cx);
            }
            Slot::Inserted(i) => {
                self.edits.delete_inserted(i);
                self.rebuild_tail(window, cx);
            }
        }
    }

    fn revert_cell(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        match locate(row, self.rows.len()) {
            Slot::Page(r) => {
                self.edits.revert_cell(r, col);
                let original = self.original(r, col);
                self.grid
                    .update(cx, |g, cx| g.set_cell(row, col, original, cx));
                self.refresh_marks(cx);
            }
            Slot::Inserted(_) => self.set_value(row, col, None, cx),
        }
    }

    fn revert_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        match locate(row, self.rows.len()) {
            Slot::Page(r) => {
                self.edits.revert_row(r);
                self.restore_row(r, cx);
                self.refresh_marks(cx);
            }
            Slot::Inserted(i) => {
                self.edits.delete_inserted(i);
                self.rebuild_tail(window, cx);
            }
        }
    }

    fn drop_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (row, col) in self.edits.edited_cells() {
            let original = self.original(row, col);
            self.grid
                .update(cx, |g, cx| g.set_cell(row, col, original, cx));
        }
        self.edits.clear();
        self.rebuild_tail(window, cx);
    }

    /// Runs `then` now, or after the user agrees to drop the pending edits.
    fn guard(&mut self, then: Discard, window: &mut Window, cx: &mut Context<Self>) {
        if self.submitting.is_some() || self.preview.is_some() || self.export.is_some() {
            return;
        }
        self.grid.update(cx, |g, cx| g.cancel_edit(window, cx));
        let n = self.edits.count();
        if n == 0 {
            return self.run_discarded(then, cx);
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Discard {n} pending changes?"),
            Some("Edits that were not submitted are lost."),
            &confirm_buttons("Cancel", "Discard"),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(CONFIRMED) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                if this.export.is_some() {
                    return;
                }
                this.drop_edits(window, cx);
                this.run_discarded(then, cx);
            })
            .ok();
        })
        .detach();
    }

    fn run_discarded(&mut self, then: Discard, cx: &mut Context<Self>) {
        match then {
            Discard::Revert => {}
            Discard::Filter => self.apply(cx),
            Discard::Load(offset) => self.load(offset, cx),
            Discard::Connect => cx.emit(DbTableEvent::Connect),
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.edits.is_empty() || !self.can_edit() {
            return;
        }
        let built = match (&self.info, &self.spec, &self.shape) {
            (Some(info), Some(spec), Some(Ok(shape))) => {
                let (changes, rows_of) = self.edits.changes(&self.rows, &self.map, shape);
                let plan = edit::plan(info.engine, &self.schema, spec, shape, &changes);
                Some((plan, rows_of))
            }
            _ => None,
        };
        let Some((plan, rows_of)) = built else {
            return;
        };
        let lines = plan
            .preview
            .lines()
            .map(|l| SharedString::from(l.to_owned()))
            .collect();
        self.preview = Some(Preview {
            plan,
            rows_of,
            lines,
        });
        self.sync_editable(cx);
        cx.notify();
    }

    fn cancel_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = None;
        self.sync_editable(cx);
        cx.notify();
    }

    fn apply_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(count) = self.preview.as_ref().map(|p| p.rows_of.len()) else {
            return;
        };
        let production = self
            .info
            .as_ref()
            .filter(|i| i.production)
            .map(|i| i.name.clone());
        let Some(name) = production else {
            return self.send_apply(cx);
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Apply {count} changes to {name}?"),
            Some("This is a production connection."),
            &confirm_buttons("Cancel", "Apply"),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(CONFIRMED) {
                this.update(cx, |this, cx| this.send_apply(cx)).ok();
            }
        })
        .detach();
    }

    fn send_apply(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(handle)) = (self.session, self.handle.clone()) else {
            return;
        };
        let Some(Preview { plan, rows_of, .. }) = self.preview.take() else {
            return;
        };
        self.error = None;
        self.failed = None;
        self.submitting = Some(Submitting {
            busy: Busy::start(),
            count: rows_of.len(),
            rows_of,
        });
        handle.send(Command::Apply {
            session,
            statements: plan.statements,
        });
        self.start_spin(cx);
        self.sync_editable(cx);
        self.sync_marks(cx);
    }

    fn open_menu(
        &mut self,
        row: usize,
        col: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.menu_items(row, col, cx);
        if items.is_empty() {
            return;
        }
        let menu = ContextMenu::build(position, items, window, cx);
        let sub = cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, _, cx| {
            if this.menu.as_ref().is_some_and(|(m, _)| m == menu) {
                this.menu = None;
                cx.notify();
            }
        });
        self.menu = Some((menu, sub));
        cx.notify();
    }

    fn menu_items(&self, row: usize, col: usize, cx: &mut Context<Self>) -> Vec<MenuItem> {
        let mut items = self.edit_items(row, col, cx);
        if self.columns.is_empty() {
            return items;
        }
        let enabled = matches!(self.phase, TablePhase::Connected)
            && self.spec.is_some()
            && self.run.is_none()
            && self.submitting.is_none()
            && self.export.is_none();
        if !items.is_empty() {
            items.push(MenuItem::separator());
        }
        items.extend(exporting::menu_block(
            cx.weak_entity(),
            Self::copy_result,
            Self::export_result,
            enabled,
            None,
        ));
        items
    }

    fn insert_target(&self) -> Option<String> {
        let engine = self.info.as_ref()?.engine;
        Some(edit::qualified(
            engine,
            &self.schema,
            &self.spec.as_ref()?.name,
        ))
    }

    fn copy_result(&mut self, format: Format, _: &mut Window, cx: &mut Context<Self>) {
        let Some(engine) = self.info.as_ref().map(|i| i.engine) else {
            return;
        };
        exporting::copy_as(&self.grid, format, engine, self.insert_target(), cx);
    }

    fn export_result(&mut self, format: Format, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session;
        let suggested = format!(
            "{}.{}",
            exporting::file_stem(&self.table),
            format.extension()
        );
        exporting::ask_path(cx, window, suggested, move |this, dest, _, cx| {
            this.start_export(session, dest, format, cx)
        });
    }

    fn start_export(
        &mut self,
        asked: Option<SessionId>,
        dest: std::path::PathBuf,
        format: Format,
        cx: &mut Context<Self>,
    ) {
        let (Some(session), Some(handle), Some(info), Some(spec)) = (
            self.session,
            self.handle.clone(),
            self.info.clone(),
            self.spec.clone(),
        ) else {
            return;
        };
        if asked != Some(session)
            || !matches!(self.phase, TablePhase::Connected)
            || self.run.is_some()
            || self.submitting.is_some()
            || self.export.is_some()
        {
            return;
        }
        let sql = edit::select_all(
            info.engine,
            &self.schema,
            &spec,
            &self.applied.0,
            &self.applied.1,
        );
        let sql = match sql {
            Ok(sql) => sql,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let run = RunId(self.next_run);
        self.next_run += 1;
        let spec = Spec {
            dest,
            format,
            engine: info.engine,
            table: self.insert_target(),
        };
        self.export = Some(exporting::start(
            cx,
            handle,
            session,
            run,
            sql,
            spec,
            Self::on_export_end,
        ));
        self.export_note = None;
        self.error = None;
        self.start_spin(cx);
        self.sync_editable(cx);
        cx.notify();
    }

    fn on_export_end(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        self.export = None;
        match outcome {
            Outcome::Done { rows, name } => {
                self.export_note = Some(format!("Exported {rows} rows to {name}"))
            }
            Outcome::Stopped => self.export_note = Some("Export stopped".into()),
            Outcome::Failed(message) => self.error = Some(message),
        }
        self.sync_editable(cx);
        cx.notify();
    }

    fn edit_items(&self, row: usize, col: usize, cx: &mut Context<Self>) -> Vec<MenuItem> {
        match &self.shape {
            Some(Err(reason)) => return vec![MenuItem::new(*reason, |_, _| {}).disabled(true)],
            Some(Ok(_)) if self.can_edit() => {}
            _ => return Vec::new(),
        }
        let this = cx.weak_entity();
        let item =
            |label: &'static str,
             enabled: bool,
             run: fn(&mut Self, usize, usize, &mut Window, &mut Context<Self>)| {
                let this = this.clone();
                MenuItem::new(label, move |window, cx| {
                    this.update(cx, |t, cx| run(t, row, col, window, cx)).ok();
                })
                .disabled(!enabled)
            };
        let slot = locate(row, self.rows.len());
        let target = self.cell_target(col);
        let deleted = matches!(slot, Slot::Page(r) if self.edits.is_deleted(r));
        let nullable = target
            .and_then(|c| self.spec.as_ref()?.columns.get(c))
            .is_some_and(|c| c.nullable);
        let can_set = target.is_some() && !deleted;
        let (cell_pending, row_pending) = match slot {
            Slot::Page(r) => (
                self.edits.is_cell_pending(r, col),
                self.edits.is_row_pending(r),
            ),
            Slot::Inserted(i) => (self.edits.inserted_value(i, col).is_some(), true),
        };
        let delete_label = match deleted {
            true => "Restore Row",
            false => "Delete Row",
        };
        vec![
            item("Set NULL", can_set && nullable, |t, r, c, _, cx| {
                t.set_value(r, c, None, cx)
            }),
            item("Set Empty String", can_set, |t, r, c, _, cx| {
                t.set_value(r, c, Some(String::new()), cx)
            }),
            item("Revert Cell", cell_pending, |t, r, c, _, cx| {
                t.revert_cell(r, c, cx)
            }),
            item("Revert Row", row_pending, |t, r, _, w, cx| {
                t.revert_row(r, w, cx)
            }),
            MenuItem::separator(),
            item("Add Row", true, |t, _, _, w, cx| t.add_row(w, cx)),
            item(delete_label, true, |t, r, _, w, cx| {
                t.toggle_delete(r, w, cx)
            }),
        ]
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
                    let kept = this.busy.is_some()
                        || this.submitting.is_some()
                        || this.export.is_some()
                        || matches!(this.phase, TablePhase::Connecting(_));
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

fn find_table(catalog: &Catalog, schema: &str, table: &str) -> Result<Arc<Table>, &'static str> {
    let found = catalog
        .schemas
        .iter()
        .find(|s| s.name == schema)
        .and_then(|s| s.tables.iter().find(|t| t.name == table));
    match (found, catalog.truncated) {
        (Some(t), _) => Ok(Arc::new(t.clone())),
        (None, true) => {
            Err("Daedalus couldn't find this table in the first 5,000 tables of the catalog.")
        }
        (None, false) => Err("This table no longer exists."),
    }
}

/// A page that still shows applied values must be reloaded before it can be edited again.
fn edit_gate(shape: bool, connected: bool, idle: bool, free: bool, stale: bool) -> bool {
    shape && connected && idle && free && !stale
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_subs(window, cx);
        let t = cx.theme().clone();
        let overlay = self.render_preview(cx);
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
            .relative()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .key_context("DbTable")
            .child(toolbar)
            .children(banner)
            .child(div().flex_1().min_h_0().child(self.grid.clone()))
            .child(footer)
            .children(overlay)
            .children(self.menu.as_ref().map(|(menu, _)| menu.clone()))
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
                let ready = self.run.is_none() && self.export.is_none();
                Button::new("db-table-reload", "Reload", ButtonKind::Secondary)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if ready {
                            this.guard(Discard::Load(this.offset), window, cx)
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
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.guard(Discard::Connect, window, cx)
                            }),
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
        let idle = self.run.is_none()
            && self.export.is_none()
            && matches!(self.phase, TablePhase::Connected);
        let step = |id: &'static str,
                    label: &'static str,
                    on: bool,
                    forward: bool,
                    cx: &mut Context<Self>| match on {
            true => Button::new(id, label, ButtonKind::Ghost)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let to = match forward {
                        true => this.offset + PAGE_ROWS,
                        false => this.offset.saturating_sub(PAGE_ROWS),
                    };
                    this.guard(Discard::Load(to), window, cx)
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
        let reload_hint =
            (self.stale && idle && self.shape_ok().is_some()).then_some("Reload to edit again.");
        let note = self.note.or(reload_hint).or(match &self.shape {
            Some(Err(reason)) => Some(*reason),
            _ => None,
        });
        let editing = self.render_edit_controls(cx);
        let submitting = self.submitting.as_ref().map(|sent| {
            let (delay, reduced) = (t.motion.indicator_delay, t.motion.reduced);
            let angle = match reduced {
                true => 0.,
                false => motion::spin_turn(sent.busy.since().elapsed()),
            };
            div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .when(sent.busy.shows(delay), |el| {
                    el.child(StatusDot::new(DotState::Running).rotation(angle))
                })
                .child(format!("Submitting {} changes", sent.count))
        });
        let stop = self.run.is_some().then(|| {
            Button::new("db-table-stop", "Stop", ButtonKind::Danger)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop(cx)))
        });
        let exporting = self.export.as_ref().map(|export| {
            let shown = export.busy().shows(std::time::Duration::from_millis(250));
            div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .when(shown, |el| {
                    el.child(format!("Exporting {} rows…", export.rows()))
                })
                .child(
                    Button::new("db-table-export-stop", "Stop", ButtonKind::Danger).on_click(
                        cx.listener(|this, _: &ClickEvent, _, _| {
                            if let Some(export) = &this.export {
                                export.stop();
                            }
                        }),
                    ),
                )
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
            .children(self.export_note.clone())
            .children(submitting)
            .children(exporting)
            .children(editing)
            .children(stop)
            .into_any_element()
    }

    fn render_edit_controls(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.shape_ok()?;
        let t = cx.theme().clone();
        let n = self.edits.count();
        let can = self.can_edit();
        let action = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      run: fn(&mut Self, &mut Window, &mut Context<Self>),
                      cx: &mut Context<Self>| match on {
            true => Button::new(id, label, ButtonKind::Ghost)
                .on_click(
                    cx.listener(move |this, _: &ClickEvent, window, cx| run(this, window, cx)),
                )
                .into_any_element(),
            false => div()
                .px(t.ui(12.))
                .text_color(t.color.content_muted)
                .child(label)
                .into_any_element(),
        };
        let add = action(
            "db-table-add-row",
            "＋ Row",
            can,
            |this, window, cx| {
                this.grid.update(cx, |g, cx| g.commit_edit(window, cx));
                cx.defer_in(window, |this, window, cx| this.add_row(window, cx));
            },
            cx,
        );
        let remove = action(
            "db-table-delete-row",
            "− Row",
            can && self.grid.read(cx).active_cell().is_some(),
            |this, window, cx| {
                this.grid.update(cx, |g, cx| g.commit_edit(window, cx));
                cx.defer_in(window, |this, window, cx| {
                    if let Some((row, _)) = this.grid.read(cx).active_cell() {
                        this.toggle_delete(row, window, cx);
                    }
                });
            },
            cx,
        );
        let revert = action(
            "db-table-revert-all",
            "Revert All",
            can && n > 0,
            |this, window, cx| this.guard(Discard::Revert, window, cx),
            cx,
        );
        let submit = action(
            "db-table-submit",
            "Submit…",
            can && n > 0,
            |this, window, cx| {
                this.grid.update(cx, |g, cx| g.commit_edit(window, cx));
                cx.defer_in(window, |this, _, cx| this.submit(cx));
            },
            cx,
        );
        Some(
            div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .child(add)
                .child(remove)
                .child(format!("{n} pending"))
                .child(revert)
                .child(submit)
                .into_any_element(),
        )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.preview.as_ref()?;
        let t = cx.theme().clone();
        let production = self.info.as_ref().is_some_and(|i| i.production);
        let text = preview.plan.preview.clone();
        let card = div()
            .w(t.ui(640.))
            .max_w_full()
            .max_h(t.ui(480.))
            .p(t.space(Space::Lg))
            .flex()
            .flex_col()
            .gap(t.space(Space::Md))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .text_color(t.color.content_secondary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.color.content)
                            .child("Review changes"),
                    )
                    .when(production, |el| {
                        el.child(tag("production", t.color.warning))
                    }),
            )
            .child(
                div()
                    .id("db-table-preview-sql")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(t.space(Space::Sm))
                    .bg(t.color.surface_sunken)
                    .rounded(t.shape.radius_control)
                    .font_family(t.typography.mono.clone())
                    .children(preview.lines.iter().cloned().map(|l| div().child(l))),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(t.space(Space::Sm))
                    .child(
                        Button::new("db-table-preview-copy", "Copy", ButtonKind::Secondary)
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                            })),
                    )
                    .child(
                        Button::new("db-table-preview-cancel", "Cancel", ButtonKind::Secondary)
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_preview(cx)),
                            ),
                    )
                    .child(
                        Button::new("db-table-preview-apply", "Apply", ButtonKind::Primary)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.apply_edits(window, cx)
                            })),
                    ),
            );
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.color.overlay)
                .child(card)
                .into_any_element(),
        )
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
    fn a_missing_table_is_told_apart_from_a_capped_catalog() {
        let mut catalog = Catalog {
            schemas: vec![daedalus_db::catalog::Schema {
                name: "public".into(),
                tables: vec![spec()],
            }],
            truncated: false,
        };
        assert!(find_table(&catalog, "public", "t").is_ok());
        assert_eq!(
            find_table(&catalog, "public", "x").err(),
            Some("This table no longer exists.")
        );
        catalog.truncated = true;
        assert!(
            find_table(&catalog, "public", "x")
                .unwrap_err()
                .contains("5,000")
        );
        assert!(find_table(&catalog, "public", "t").is_ok());
    }

    #[test]
    fn a_stale_page_cannot_be_edited_until_a_load_finishes() {
        assert!(edit_gate(true, true, true, true, false));
        assert!(!edit_gate(true, true, true, true, true));
        assert!(!edit_gate(true, true, false, true, false));
        assert!(!edit_gate(false, true, true, true, false));
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
