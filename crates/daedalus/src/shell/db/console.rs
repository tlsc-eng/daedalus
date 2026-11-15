use std::collections::VecDeque;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use daedalus_db::classify::{self, Statement};
use daedalus_db::worker::{Command, Handle, RunMode, SessionId, TxOp, Update};
use daedalus_db::{Engine, Listed};
use daedalus_editor::{EditorEvent, EditorView, Marker, MarkerSeverity};
use daedalus_ui::motion::{self, Busy};
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, DataGrid, DotState, GridEvent, Space, StatusDot,
};
use gpui::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    PromptLevel, Render, Subscription, Task, Window, div, prelude::*, relative,
};

use super::explorer::{data_color, dot_shape, engine_label, tag};
use crate::actions::{RunAll, RunStatement, StopRun};
use crate::shell::lsp::{CONFIRMED, confirm_buttons};
use crate::shell::sidebar::folder_color;
use batch::{Batch, Effect, Piece};
use gate::{Gate, Target};

mod batch;
mod gate;
mod text;

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
        let (path, setup_error) = match daedalus_proto::data_dir() {
            Ok(dir) => {
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
        });
        let autosave = cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Edited { .. }) {
                this.editor.update(cx, |e, cx| e.schedule_autosave(cx));
            }
        });
        let grid = cx.new(DataGrid::new);
        let fetch = cx.subscribe(&grid, |this, _, _: &GridEvent, cx| this.fetch_more(cx));
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
            _release: cx.on_release(|this, cx| {
                this.save_text(cx);
                this.disconnect();
            }),
            _subs: vec![fetch, autosave],
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
        self.tx_open = false;
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
        self.tx_open = false;
        cx.notify();
    }

    pub fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.phase = ConsolePhase::Failed(message.into());
        self.session = None;
        self.tx_open = false;
        self.batch.reset();
        self.busy = None;
        cx.notify();
    }

    /// Hangs up the session; never after a `Failed`, which already ended it.
    fn disconnect(&mut self) {
        self.tx_open = false;
        if let (Some(session), Some(handle)) = (self.session.take(), &self.handle) {
            handle.send(Command::Disconnect { session });
        }
    }

    pub fn on_update(&mut self, update: Update, cx: &mut Context<Self>) {
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
            Update::Catalog { .. } => {}
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
                    let kept =
                        this.busy.is_some() || matches!(this.phase, ConsolePhase::Connecting(_));
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

    fn begin(&mut self, pieces: Vec<Piece>, cx: &mut Context<Self>) {
        let (Some(session), Some(info)) = (self.session, self.info.clone()) else {
            return;
        };
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let toolbar = self.render_toolbar(cx);
        let results = self.render_results(cx);
        div()
            .size_full()
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
    }
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
        let refusal = match (self.tx_open, self.batch.idle()) {
            (true, _) => Some("Commit or roll back the open transaction first."),
            (false, false) => Some("Wait for the run to finish or stop it first."),
            (false, true) => None,
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
        let tx_ready = self.tx_open && !running;
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
            let end = |id: &'static str, label: &'static str, op: TxOp| match tx_ready {
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
                    el.child(end("db-console-commit", "Commit", TxOp::Commit))
                        .child(end("db-console-rollback", "Roll Back", TxOp::Rollback))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_mode_prefers_read_only() {
        assert_eq!(run_mode(true, true), RunMode::ReadOnly);
        assert_eq!(run_mode(true, false), RunMode::ReadOnly);
        assert_eq!(run_mode(false, true), RunMode::Transaction);
        assert_eq!(run_mode(false, false), RunMode::Auto);
    }
}
