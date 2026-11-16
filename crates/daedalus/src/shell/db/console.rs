use std::collections::VecDeque;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daedalus_db::catalog::Catalog;
use daedalus_db::classify::{self, Statement};
use daedalus_db::claude;
use daedalus_db::export::Format;
use daedalus_db::worker::{Command, Handle, RunMode, SessionId, TxOp, Update};
use daedalus_db::{Engine, Listed};
use daedalus_editor::{EditorEvent, EditorView, Marker, MarkerSeverity};
use daedalus_ui::motion::{self, Busy};
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, ContextMenu, DataGrid, DotState, GridEvent, Space, StatusDot,
};
use gpui::{
    AnyElement, ClickEvent, Context, DismissEvent, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, FontWeight, Pixels, Point, PromptLevel, Render, Subscription, Task,
    Window, div, prelude::*, relative,
};

use super::explorer::{data_color, dot_shape, engine_label, tag};
use super::exporting::{self, Export, Outcome, Spec};
use crate::actions::{RunAll, RunStatement, StopRun};
use crate::shell::lsp::{CONFIRMED, confirm_buttons};
use crate::shell::sidebar::folder_color;
use batch::{Batch, Effect, Piece};
use gate::{Gate, Target};
use history::{HistoryEvent, HistoryPopover};

mod batch;
mod complete;
mod gate;
mod history;
mod text;

pub(super) use batch::grid_columns;

const OUTPUT_CAP: usize = 500;

/// What the console shows of its connection; copied so the view never holds a `Listed`.
#[derive(Clone)]
pub(in crate::shell) struct Info {
    pub name: String,
    pub engine: Engine,
    pub read_only: bool,
    pub production: bool,
    pub color: Option<String>,
}

impl From<&Listed> for Info {
    fn from(l: &Listed) -> Self {
        Self {
            name: l.conn.name.clone(),
            engine: l.conn.engine,
            read_only: l.conn.read_only,
            production: l.conn.production,
            color: l.conn.color.clone(),
        }
    }
}

pub(in crate::shell) enum DbConsoleEvent {
    Connect,
    WantCatalog,
}

enum ConsolePhase {
    Idle,
    Connecting(Busy),
    Connected { server: String },
    Failed(String),
}

enum Shown {
    Nothing,
    Grid,
    Affected(Option<u64>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResultTab {
    Result,
    Output,
}

struct ErrorBanner {
    message: String,
    at: Option<(u32, u32)>,
}

struct OutputLine {
    text: String,
    error: bool,
}

pub(in crate::shell) struct DbConsoleView {
    key: String,
    info: Option<Info>,
    phase: ConsolePhase,
    session: Option<SessionId>,
    manual: bool,
    tx_open: bool,
    catalog: Option<Arc<Catalog>>,
    catalog_asked: bool,
    handle: Option<Handle>,
    setup_error: Option<String>,
    editor: Entity<EditorView>,
    grid: Entity<DataGrid>,
    batch: Batch,
    result: Shown,
    footer: Option<String>,
    error: Option<ErrorBanner>,
    output: VecDeque<OutputLine>,
    tab: ResultTab,
    busy: Option<Busy>,
    spin: Option<Task<()>>,
    export: Option<Export>,
    menu: Option<(Entity<ContextMenu>, Subscription)>,
    history_path: Option<PathBuf>,
    history: Option<(Entity<HistoryPopover>, Subscription)>,
    history_dismissed_by: Option<Point<Pixels>>,
    grid_sub: Option<Subscription>,
    _release: Subscription,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DbConsoleEvent> for DbConsoleView {}

impl Focusable for DbConsoleView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

/// Creates the console file without ever truncating it, in a directory only the user can read.
fn prepare(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map(drop)
}

impl DbConsoleView {
    pub fn new(root: PathBuf, key: String, cx: &mut Context<Self>) -> Self {
        let mut history_path = None;
        let (path, setup_error) = match daedalus_proto::data_dir() {
            Ok(dir) => {
                history_path = Some(daedalus_db::history::path(&dir, &root, &key));
                let path = text::console_path(&dir, &root, &key);
                let error = prepare(&path)
                    .err()
                    .map(|e| format!("Could not create the console file: {e}"));
                (path, error)
            }
            Err(e) => (
                PathBuf::from("/dev/null"),
                Some(format!("Could not create the console file: {e}")),
            ),
        };
        let editor = cx.new(|cx| EditorView::open(path, cx));
        editor.update(cx, |e, cx| {
            e.set_autosave(Some(Duration::from_secs(1)), cx);
            e.set_minimap(false, cx);
            e.set_code_lens(false, cx);
            e.set_inlay_hints(false, cx);
            e.set_completion_triggers(Some(vec![".".into()]));
        });
        let autosave = cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| match event {
            EditorEvent::Edited { .. } => this.editor.update(cx, |e, cx| e.schedule_autosave(cx)),
            EditorEvent::Complete {
                request,
                line,
                character,
                ..
            } => this.complete(*request, (*line, *character), cx),
            _ => {}
        });
        let grid = cx.new(DataGrid::new);
        Self {
            key,
            info: None,
            phase: match &setup_error {
                Some(e) => ConsolePhase::Failed(e.clone()),
                None => ConsolePhase::Idle,
            },
            session: None,
            manual: false,
            tx_open: false,
            catalog: None,
            catalog_asked: false,
            handle: None,
            setup_error,
            editor,
            grid,
            batch: Batch::default(),
            result: Shown::Nothing,
            footer: None,
            error: None,
            output: VecDeque::new(),
            tab: ResultTab::Result,
            busy: None,
            spin: None,
            export: None,
            menu: None,
            history_path,
            history: None,
            history_dismissed_by: None,
            grid_sub: None,
            _release: cx.on_release(|this, cx| {
                this.save_text(cx);
                this.disconnect();
            }),
            _subs: vec![autosave],
        }
    }

    /// Writes the console's text to its file; the shell never treats a console as a dirty editor.
    pub fn save_text(&self, cx: &mut gpui::App) {
        self.editor.update(cx, |e, cx| {
            if e.is_dirty() {
                e.save(cx);
            }
        });
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn label(&self) -> String {
        match &self.info {
            Some(info) => format!("{} console", info.name),
            None => "Console".into(),
        }
    }

    pub fn tx_open(&self) -> bool {
        self.tx_open
    }

    pub fn set_catalog(&mut self, catalog: Arc<Catalog>) {
        self.catalog = Some(catalog);
    }

    /// Asks for the catalog on the console's own session, never inside an open transaction.
    pub fn introspect(&mut self) {
        if self.tx_open {
            return;
        }
        if let (Some(session), Some(handle)) = (self.session, &self.handle) {
            self.catalog_asked = true;
            handle.send(Command::Introspect { session });
        }
    }

    fn complete(&mut self, request: u64, at: (u32, u32), cx: &mut Context<Self>) {
        let connected = matches!(self.phase, ConsolePhase::Connected { .. });
        if connected && self.catalog.is_none() && !self.catalog_asked {
            cx.emit(DbConsoleEvent::WantCatalog);
        }
        let engine = self.info.as_ref().map(|i| i.engine);
        let text = self.editor.read(cx).text();
        let (items, incomplete) = match (engine, text) {
            (Some(engine), Some(text)) => {
                let byte = text::byte_at(&text, at);
                let statement = classify::at(&text, byte, engine)
                    .and_then(|s| text.get(s.range))
                    .unwrap_or("");
                complete::complete(&text[..byte], statement, self.catalog.as_deref())
            }
            _ => (Vec::new(), false),
        };
        self.editor.update(cx, |e, cx| {
            e.show_completions(request, items, incomplete, cx)
        });
    }

    pub fn needs_connect(&self) -> bool {
        matches!(self.phase, ConsolePhase::Idle | ConsolePhase::Failed(_))
    }

    pub fn setup_error(&self) -> Option<String> {
        self.setup_error.clone()
    }

    pub fn connecting(
        &mut self,
        session: SessionId,
        handle: Handle,
        info: Info,
        cx: &mut Context<Self>,
    ) {
        self.session = Some(session);
        self.export = None;
        self.tx_open = false;
        self.catalog = None;
        self.catalog_asked = false;
        self.handle = Some(handle);
        self.info = Some(info);
        self.phase = ConsolePhase::Connecting(Busy::start());
        self.start_spin(cx);
        cx.notify();
    }

    /// Records which connection this is while a password sheet is open, without starting a session.
    pub fn prompting(&mut self, info: Info, cx: &mut Context<Self>) {
        self.info = Some(info);
        cx.notify();
    }

    pub fn idle(&mut self, cx: &mut Context<Self>) {
        self.phase = ConsolePhase::Idle;
        self.session = None;
        self.export = None;
        self.tx_open = false;
        cx.notify();
    }

    pub fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.phase = ConsolePhase::Failed(message.into());
        self.session = None;
        self.export = None;
        self.tx_open = false;
        self.batch.reset();
        self.busy = None;
        cx.notify();
    }

    /// Hangs up the session; never after a `Failed`, which already ended it.
    pub(super) fn disconnect(&mut self) {
        self.tx_open = false;
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
            Update::Connected { server, .. } => {
                self.phase = ConsolePhase::Connected { server };
            }
            Update::Failed { error, .. } => {
                let was_connected = matches!(self.phase, ConsolePhase::Connected { .. });
                if was_connected {
                    self.error = Some(ErrorBanner {
                        message: error.clone(),
                        at: None,
                    });
                    self.log(error.clone(), true);
                }
                self.phase = ConsolePhase::Failed(error);
                self.session = None;
                self.export = None;
                self.tx_open = false;
                self.batch.reset();
                self.busy = None;
                self.grid.update(cx, |g, cx| g.set_stale(false, cx));
            }
            Update::Columns { .. }
            | Update::Rows { .. }
            | Update::Done { .. }
            | Update::RunError { .. } => {
                let effects = self.batch.on(update);
                self.apply(effects, cx);
            }
            Update::Applied { .. } => {}
            Update::TxState { open, error, .. } => {
                self.tx_open = open;
                if let Some(message) = error {
                    self.error = Some(ErrorBanner { message, at: None });
                    self.tab = ResultTab::Result;
                    let text = match open {
                        true => "The transaction is still open.",
                        false => "The transaction ended.",
                    };
                    self.log(text.into(), true);
                }
            }
            Update::Catalog { catalog, .. } => match catalog {
                Ok(c) => self.catalog = Some(Arc::new(c)),
                Err(_) => self.log("Could not read the schema for completion.".into(), true),
            },
        }
        cx.notify();
    }

    fn log(&mut self, text: String, error: bool) {
        if self.output.len() == OUTPUT_CAP {
            self.output.pop_front();
        }
        self.output.push_back(OutputLine { text, error });
    }

    fn apply(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                Effect::Send(command) => {
                    if let Some(handle) = &self.handle {
                        handle.send(*command);
                    }
                }
                Effect::Show {
                    columns,
                    rows,
                    more,
                } => {
                    self.result = Shown::Grid;
                    self.grid
                        .update(cx, |g, cx| g.show(columns, rows, more, cx));
                }
                Effect::Append { rows, more } => {
                    self.grid.update(cx, |g, cx| g.append(rows, more, cx));
                }
                Effect::Finish { truncated } => {
                    self.grid.update(cx, |g, cx| g.finish(truncated, cx));
                }
                Effect::Affected(k) => {
                    self.result = Shown::Affected(k);
                    self.grid.update(cx, |g, cx| g.set_stale(false, cx));
                }
                Effect::Fail { message, at } => {
                    let text = self.editor.read(cx).text().unwrap_or_default();
                    let at = at.map(|b| text::position(&text, b));
                    if let Some(at) = at {
                        let marker = Marker {
                            start: at,
                            end: (at.0, at.1 + 1),
                            severity: MarkerSeverity::Error,
                            message: message.clone(),
                        };
                        self.editor
                            .update(cx, |e, cx| e.set_markers(vec![marker], cx));
                    }
                    let failed_tx = self.manual
                        && self.tx_open
                        && self
                            .info
                            .as_ref()
                            .is_some_and(|i| i.engine == Engine::Postgres);
                    let message = match failed_tx {
                        true => {
                            format!("{message} The transaction has failed; roll back to continue.")
                        }
                        false => message,
                    };
                    self.error = Some(ErrorBanner { message, at });
                    self.grid.update(cx, |g, cx| g.set_stale(false, cx));
                }
                Effect::Log { text, error } => self.log(text, error),
                Effect::Footer(text) => self.footer = Some(text),
                Effect::Idle => {
                    self.busy = None;
                    self.grid.update(cx, |g, cx| g.set_stale(false, cx));
                }
            }
        }
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
                    let kept = this.busy.is_some()
                        || this.export.is_some()
                        || matches!(this.phase, ConsolePhase::Connecting(_));
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

    fn fetch_more(&mut self, cx: &mut Context<Self>) {
        let effects = self.batch.fetch_more();
        if effects.is_empty() {
            return;
        }
        self.busy = Some(Busy::start());
        self.start_spin(cx);
        self.apply(effects, cx);
    }

    fn run_statement(&mut self, _: &RunStatement, window: &mut Window, cx: &mut Context<Self>) {
        self.run(false, window, cx);
    }

    fn run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        self.run(true, window, cx);
    }

    fn stop_run(&mut self, _: &StopRun, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(export) = &self.export {
            export.stop();
            return;
        }
        let effects = self.batch.stop();
        self.apply(effects, cx);
    }

    fn run(&mut self, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.info.clone() else {
            return;
        };
        if !matches!(self.phase, ConsolePhase::Connected { .. }) || self.batch.running() {
            return;
        }
        if self.export.is_some() {
            self.error = Some(ErrorBanner {
                message: "An export is running; stop it first.".into(),
                at: None,
            });
            self.tab = ResultTab::Result;
            cx.notify();
            return;
        }
        self.editor.update(cx, |e, cx| {
            e.save(cx);
        });
        let Some(text) = self.editor.read(cx).text() else {
            return;
        };
        let selection = self.editor.read(cx).selection_utf16();
        let engine = info.engine;
        let (source, base, statements): (String, usize, Vec<Statement>) = match selection {
            Some((from, to, sel)) if !all && from != to => {
                let statements = classify::split(&sel, engine);
                (sel, text::byte_at(&text, from), statements)
            }
            _ if all => {
                let statements = classify::split(&text, engine);
                (text.clone(), 0, statements)
            }
            _ => {
                let at = self.editor.read(cx).cursor_utf16().unwrap_or((0, 0));
                let one = classify::at(&text, text::byte_at(&text, at), engine);
                (text.clone(), 0, one.into_iter().collect())
            }
        };
        let target = Target {
            name: &info.name,
            read_only: info.read_only,
            production: info.production,
            manual: self.manual,
            mysql: info.engine == Engine::Mysql,
        };
        let verdict = gate::gate(&source, &statements, &target);
        let pieces = statements
            .iter()
            .map(|s| Piece {
                sql: source[s.range.clone()].to_owned(),
                start: base + s.range.start,
                kind: s.kind,
            })
            .collect::<Vec<_>>();
        match verdict {
            Gate::Run => self.begin(pieces, cx),
            Gate::Refuse(message) => {
                if !message.is_empty() {
                    self.error = Some(ErrorBanner { message, at: None });
                    self.tab = ResultTab::Result;
                    cx.notify();
                }
            }
            Gate::Confirm { title, detail } => {
                let session = self.session;
                let version = self.editor.read(cx).version();
                cx.defer_in(window, move |_, window, cx| {
                    let answer = window.prompt(
                        PromptLevel::Warning,
                        &title,
                        Some(&detail),
                        &confirm_buttons("Cancel", "Run"),
                        cx,
                    );
                    cx.spawn_in(window, async move |this, cx| {
                        let answer = answer.await;
                        this.update(cx, |this, cx| {
                            let same = this.session == session
                                && matches!(this.phase, ConsolePhase::Connected { .. })
                                && this.editor.read(cx).version() == version;
                            if answer == Ok(CONFIRMED) && same && !this.batch.running() {
                                this.begin(pieces, cx);
                            }
                        })
                        .ok();
                    })
                    .detach();
                });
            }
        }
    }

    fn record_history(&self, pieces: &[Piece], cx: &mut Context<Self>) {
        let Some(path) = self.history_path.clone() else {
            return;
        };
        let sqls = history_sqls(pieces);
        let at = history::now_ms();
        cx.background_executor()
            .spawn(async move {
                let refs: Vec<&str> = sqls.iter().map(String::as_str).collect();
                let _ = daedalus_db::history::append(&path, &refs, at);
            })
            .detach();
    }

    fn toggle_history(&mut self, click: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if crate::shell::notices::is_dismissing_click(click, &mut self.history_dismissed_by) {
            return;
        }
        if self.history.take().is_some() {
            cx.notify();
            return;
        }
        let popover = cx.new(|cx| HistoryPopover::new(self.history_path.clone(), window, cx));
        let sub = cx.subscribe_in(
            &popover,
            window,
            |this, _, event: &HistoryEvent, window, cx| {
                this.history = None;
                this.history_dismissed_by = None;
                match event {
                    HistoryEvent::Insert(sql) => {
                        let text = format!("{sql}\n");
                        this.editor
                            .update(cx, |e, cx| e.replace_text_in_range(None, &text, window, cx));
                    }
                    HistoryEvent::Close { at } => this.history_dismissed_by = *at,
                }
                window.focus(&this.editor.focus_handle(cx));
                cx.notify();
            },
        );
        self.history = Some((popover, sub));
        cx.notify();
    }

    fn begin(&mut self, pieces: Vec<Piece>, cx: &mut Context<Self>) {
        let (Some(session), Some(info)) = (self.session, self.info.clone()) else {
            return;
        };
        self.record_history(&pieces, cx);
        self.error = None;
        self.editor
            .update(cx, |e, cx| e.set_markers(Vec::new(), cx));
        self.grid.update(cx, |g, cx| g.set_stale(true, cx));
        self.busy = Some(Busy::start());
        self.start_spin(cx);
        let mode = run_mode(info.read_only, self.manual);
        let effects = self.batch.start(session, pieces, mode);
        self.apply(effects, cx);
    }
}

impl Render for DbConsoleView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_subs(window, cx);
        let t = cx.theme().clone();
        let toolbar = self.render_toolbar(cx);
        let results = self.render_results(cx);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .key_context("DbConsole")
            .on_action(cx.listener(Self::run_statement))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::stop_run))
            .child(toolbar)
            .child(
                div()
                    .flex_basis(relative(0.4))
                    .min_h_0()
                    .overflow_hidden()
                    .child(div().size_full().child(self.editor.clone())),
            )
            .child(results)
            .children(self.menu.as_ref().map(|(menu, _)| menu.clone()))
            .children(self.history.as_ref().map(|(popover, _)| popover.clone()))
    }
}

fn history_sqls(pieces: &[Piece]) -> Vec<String> {
    pieces.iter().map(|p| p.sql.clone()).collect()
}

fn run_mode(read_only: bool, manual: bool) -> RunMode {
    match (read_only, manual) {
        (true, _) => RunMode::ReadOnly,
        (false, true) => RunMode::Transaction,
        (false, false) => RunMode::Auto,
    }
}

impl DbConsoleView {
    fn set_manual(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.manual == manual {
            return;
        }
        let refusal = match (self.export.is_some(), self.tx_open, self.batch.idle()) {
            (true, _, _) => Some("Wait for the export to finish or stop it first."),
            (_, true, _) => Some("Commit or roll back the open transaction first."),
            (_, false, false) => Some("Wait for the run to finish or stop it first."),
            (_, false, true) => None,
        };
        if let Some(message) = refusal {
            self.error = Some(ErrorBanner {
                message: message.into(),
                at: None,
            });
            self.tab = ResultTab::Result;
        } else {
            self.manual = manual;
        }
        cx.notify();
    }

    fn end_transaction(&mut self, op: TxOp, cx: &mut Context<Self>) {
        let (Some(session), Some(handle)) = (self.session, self.handle.clone()) else {
            return;
        };
        if self.batch.running() || !self.tx_open {
            return;
        }
        if self.batch.paused() && op == TxOp::Commit {
            self.error = Some(ErrorBanner {
                message: "Load the rest of the result or stop it before committing.".into(),
                at: None,
            });
            self.tab = ResultTab::Result;
            return;
        }
        if self.batch.paused() {
            let effects = self.batch.stop();
            self.apply(effects, cx);
        }
        handle.send(Command::Transaction { session, op });
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let (delay, reduced) = (t.motion.indicator_delay, t.motion.reduced);
        let spinner = |busy: Busy| {
            let angle = match reduced {
                true => 0.,
                false => motion::spin_turn(busy.since().elapsed()),
            };
            StatusDot::new(DotState::Running)
                .rotation(angle)
                .into_any_element()
        };
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
        let running = self.batch.running();
        let run = match running {
            true => Button::new("db-console-stop", "Stop", ButtonKind::Danger).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.stop_run(&StopRun, window, cx)),
            ),
            false => Button::new("db-console-run", "Run", ButtonKind::Primary).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.run(false, window, cx)),
            ),
        };
        let manual = self.manual;
        let rollback_ready = self.tx_open && !running;
        let commit_ready = self.tx_open && self.batch.idle();
        let mode = self.info.as_ref().is_some_and(|i| !i.read_only).then(|| {
            let pick = |id: &'static str, label: &'static str, on: bool| {
                let kind = match on {
                    true => ButtonKind::Secondary,
                    false => ButtonKind::Ghost,
                };
                Button::new(id, label, kind).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| this.set_manual(label == "Manual", cx),
                ))
            };
            let end = |id: &'static str, label: &'static str, op: TxOp, ready: bool| match ready {
                true => Button::new(id, label, ButtonKind::Secondary)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.end_transaction(op, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
                false => div()
                    .text_color(t.color.content_muted)
                    .child(label)
                    .into_any_element(),
            };
            div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .child(pick("db-console-auto", "Auto", !manual))
                .child(pick("db-console-manual", "Manual", manual))
                .when(manual || self.tx_open, |el| {
                    el.child(end(
                        "db-console-commit",
                        "Commit",
                        TxOp::Commit,
                        commit_ready,
                    ))
                    .child(end(
                        "db-console-rollback",
                        "Roll Back",
                        TxOp::Rollback,
                        rollback_ready,
                    ))
                })
                .when(self.tx_open, |el| {
                    el.child(tag("Transaction open", t.color.warning))
                })
        });
        let status = match &self.phase {
            ConsolePhase::Connecting(busy) => div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .text_color(t.color.content_muted)
                .when(busy.shows(delay), |el| el.child(spinner(*busy)))
                .child("Connecting…")
                .into_any_element(),
            ConsolePhase::Connected { server } => div()
                .min_w_0()
                .text_color(t.color.content_muted)
                .child(server.clone())
                .into_any_element(),
            ConsolePhase::Idle | ConsolePhase::Failed(_) => {
                let message = match &self.phase {
                    ConsolePhase::Failed(m) => Some(m.clone()),
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
                        Button::new("db-console-connect", "Connect", ButtonKind::Secondary)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(DbConsoleEvent::Connect)
                            })),
                    )
                    .into_any_element()
            }
        };
        div()
            .h(t.ui(32.))
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
            .child(run)
            .child(
                Button::new("db-console-run-all", "Run All", ButtonKind::Secondary).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.run(true, window, cx)),
                ),
            )
            .child(
                Button::new("db-console-history", "History", ButtonKind::Ghost).on_click(
                    cx.listener(|this, click: &ClickEvent, window, cx| {
                        this.toggle_history(click, window, cx)
                    }),
                ),
            )
            .children(mode)
            .child(div().flex_1())
            .child(status)
            .into_any_element()
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let toggle =
            |id: &'static str, label: &'static str, tab: ResultTab, cx: &mut Context<Self>| {
                let on = self.tab == tab;
                div()
                    .id(id)
                    .px(t.space(Space::Sm))
                    .cursor_pointer()
                    .text_color(match on {
                        true => t.color.accent,
                        false => t.color.content_muted,
                    })
                    .when(on, |el| el.font_weight(FontWeight::MEDIUM))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
                    .child(label)
            };
        let exporting = self.export.as_ref().map(|export| {
            let shown = export.busy().shows(Duration::from_millis(250));
            div()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .text_color(t.color.content_muted)
                .when(shown, |el| {
                    el.child(format!("Exporting {} rows…", export.rows()))
                })
                .child(
                    Button::new("db-console-export-stop", "Stop", ButtonKind::Danger).on_click(
                        cx.listener(|this, _: &ClickEvent, _, _| {
                            if let Some(export) = &this.export {
                                export.stop();
                            }
                        }),
                    ),
                )
        });
        let strip = div()
            .h(t.ui(28.))
            .flex_none()
            .px(t.space(Space::Md))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption)
            .child(toggle(
                "db-console-tab-result",
                "Result",
                ResultTab::Result,
                cx,
            ))
            .child(toggle(
                "db-console-tab-output",
                "Output",
                ResultTab::Output,
                cx,
            ))
            .child(div().flex_1())
            .children(exporting)
            .child(
                div()
                    .text_color(t.color.content_muted)
                    .children(self.footer.clone()),
            );
        let banner = self.error.as_ref().map(|e| {
            let at = e.at;
            div()
                .flex_none()
                .px(t.space(Space::Md))
                .py(t.space(Space::Sm))
                .flex()
                .gap(t.space(Space::Md))
                .bg(t.color.danger_surface)
                .text_size(t.typography.caption)
                .text_color(t.color.danger)
                .child(div().flex_1().min_w_0().child(e.message.clone()))
                .when_some(at, |el, (line, col)| {
                    el.child(
                        div()
                            .id("db-console-error-at")
                            .flex_none()
                            .cursor_pointer()
                            .underline()
                            .child(format!("line {}, col {}", line + 1, col + 1))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.editor
                                    .update(cx, |e, cx| e.go_to_position(line, col, cx));
                                window.focus(&this.editor.focus_handle(cx));
                            })),
                    )
                })
        });
        let body = match (self.tab, &self.result) {
            (ResultTab::Result, Shown::Grid) => div()
                .flex_1()
                .min_h_0()
                .child(self.grid.clone())
                .into_any_element(),
            (ResultTab::Result, Shown::Affected(k)) => div()
                .p(t.space(Space::Md))
                .text_size(t.typography.caption)
                .text_color(t.color.content_secondary)
                .child(match k {
                    Some(k) => format!("{k} rows affected"),
                    None => "Done".into(),
                })
                .into_any_element(),
            (ResultTab::Result, Shown::Nothing) => div().into_any_element(),
            (ResultTab::Output, _) => div()
                .id("db-console-output")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(t.space(Space::Md))
                .text_size(t.typography.caption)
                .children(self.output.iter().map(|line| {
                    div()
                        .text_color(match line.error {
                            true => t.color.danger,
                            false => t.color.content_secondary,
                        })
                        .child(line.text.clone())
                }))
                .into_any_element(),
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(strip)
            .children(banner)
            .child(body)
            .into_any_element()
    }
}

impl DbConsoleView {
    fn ensure_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid_sub.is_some() {
            return;
        }
        self.grid_sub = Some(cx.subscribe_in(
            &self.grid,
            window,
            |this, _, event: &GridEvent, window, cx| match event {
                GridEvent::FetchMore => this.fetch_more(cx),
                GridEvent::ContextMenu { position, .. } => this.open_menu(*position, window, cx),
                _ => {}
            },
        ));
    }

    fn connected(&self) -> bool {
        matches!(self.phase, ConsolePhase::Connected { .. })
    }

    /// The statement to re-run for an export, only when it is safe to run again as is.
    fn rerun_sql(&self) -> Option<String> {
        let engine = self.info.as_ref()?.engine;
        if !self.connected() || self.tx_open || self.batch.running() {
            return None;
        }
        claude::gate(self.batch.shown_sql()?, engine).ok()
    }

    fn open_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.result, Shown::Grid) {
            return;
        }
        let enabled = self.connected() && self.export.is_none() && !self.batch.running();
        let note = self
            .rerun_sql()
            .is_none()
            .then_some("Export: loaded rows only");
        let items = exporting::menu_block(
            cx.weak_entity(),
            Self::copy_result,
            Self::export_result,
            enabled,
            note,
        );
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

    fn copy_result(&mut self, format: Format, _: &mut Window, cx: &mut Context<Self>) {
        let Some(engine) = self.info.as_ref().map(|i| i.engine) else {
            return;
        };
        exporting::copy_as(&self.grid, format, engine, None, cx);
    }

    fn export_result(&mut self, format: Format, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session;
        let suggested = format!("result.{}", format.extension());
        exporting::ask_path(cx, window, suggested, move |this, dest, _, cx| {
            this.start_export(session, dest, format, cx)
        });
    }

    fn start_export(
        &mut self,
        asked: Option<SessionId>,
        dest: PathBuf,
        format: Format,
        cx: &mut Context<Self>,
    ) {
        let (Some(session), Some(handle), Some(info)) =
            (self.session, self.handle.clone(), self.info.clone())
        else {
            return;
        };
        if asked != Some(session)
            || !self.connected()
            || self.export.is_some()
            || self.batch.running()
        {
            return;
        }
        let spec = Spec {
            dest,
            format,
            engine: info.engine,
            table: None,
        };
        let sql = self.rerun_sql();
        if self.batch.paused() {
            let effects = self.batch.stop();
            self.apply(effects, cx);
        }
        match sql {
            Some(sql) => {
                let run = self.batch.take_run_id();
                self.export = Some(exporting::start(
                    cx,
                    handle,
                    session,
                    run,
                    sql,
                    spec,
                    Self::on_export_end,
                ));
            }
            None => {
                let (columns, rows) = exporting::loaded(self.grid.read(cx));
                exporting::write_loaded(cx, spec, columns, rows, Self::on_loaded_end);
            }
        }
        self.footer = None;
        self.error = None;
        self.start_spin(cx);
        cx.notify();
    }

    fn on_export_end(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        self.export = None;
        self.on_loaded_end(outcome, cx);
    }

    /// Reports an export's end without touching `export`, which a loaded-rows write never owns.
    fn on_loaded_end(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        match outcome {
            Outcome::Done { rows, name } => {
                let text = format!("Exported {rows} rows to {name}");
                self.log(text.clone(), false);
                self.footer = Some(text);
            }
            Outcome::Stopped => self.footer = Some("Export stopped".into()),
            Outcome::Failed(message) => {
                self.log(message.clone(), true);
                self.error = Some(ErrorBanner { message, at: None });
                self.tab = ResultTab::Result;
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_records_each_piece_sql() {
        let piece = |sql: &str| Piece {
            sql: sql.into(),
            start: 0,
            kind: classify::Kind::Read,
        };
        let pieces = [piece("select 1"), piece("select 2")];
        assert_eq!(history_sqls(&pieces), ["select 1", "select 2"]);
    }

    #[test]
    fn run_mode_prefers_read_only() {
        assert_eq!(run_mode(true, true), RunMode::ReadOnly);
        assert_eq!(run_mode(true, false), RunMode::ReadOnly);
        assert_eq!(run_mode(false, true), RunMode::Transaction);
        assert_eq!(run_mode(false, false), RunMode::Auto);
    }
}
