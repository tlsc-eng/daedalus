use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use daedalus_db::worker::{Command, Handle, SessionId, Update};
use daedalus_db::{Connection, Engine, Listed, Origin, Secret, Store};
use daedalus_ui::motion::{self, Busy, Closing};
use daedalus_ui::{ActiveTheme, Theme};
use gpui::{
    Animation, AnyElement, Context, Entity, MouseButton, PromptLevel, Subscription, Task, Window,
    div, prelude::*, px,
};

use super::Shell;
use crate::db_secrets;
use sheet::{DbSheet, DbSheetEvent, PasswordChange, SheetMode, needs_reconnect};

mod explorer;
mod sheet;

struct OpenSheet {
    entity: Entity<DbSheet>,
    _subscription: Subscription,
}

/// What a finished Keychain read means for the connect that asked for it.
enum Keychain {
    Connect(Box<Command>),
    Prompt,
    Done,
}

#[derive(Default)]
pub(super) struct DbState {
    handle: Option<Handle>,
    _updates: Option<Task<()>>,
    store: Option<Store>,
    load_error: Option<String>,
    loaded: bool,
    load_task: Option<Task<()>>,
    meta: HashMap<String, Meta>,
    next_session: u64,
    selected: Option<String>,
    spin: Option<Task<()>>,
    sheet: Option<OpenSheet>,
    sheet_closing: Option<(Entity<DbSheet>, Closing)>,
    sheet_opened: u64,
    test: Option<(SessionId, Instant)>,
    test_done: Option<(Result<String, String>, u128)>,
}

#[derive(Clone, Default)]
pub(super) struct Meta {
    pub session: Option<SessionId>,
    pub phase: Phase,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum Phase {
    #[default]
    Idle,
    Connecting(Busy),
    Connected {
        server: String,
        busy: Busy,
        at: Instant,
    },
    Failed(String),
}

fn conn_key(origin: Origin, id: &str) -> String {
    match origin {
        Origin::Global => format!("g:{id}"),
        Origin::Project => format!("p:{id}"),
    }
}

/// Whether a spinner that began at `busy` still shows at `now` for a connect that finished at `at`.
fn spinner_lingers(busy: Busy, at: Instant, now: Instant, delay: Duration, hold: Duration) -> bool {
    let waited = at.saturating_duration_since(busy.since());
    if waited < delay {
        return false;
    }
    now.saturating_duration_since(at) < (delay + hold).saturating_sub(waited)
}

fn path() -> Result<PathBuf, String> {
    daedalus_proto::data_dir()
        .map(|dir| dir.join(daedalus_db::FILE))
        .map_err(|e| e.to_string())
}

impl DbState {
    fn phase(&self, key: &str) -> Phase {
        self.meta
            .get(key)
            .map(|m| m.phase.clone())
            .unwrap_or_default()
    }

    /// Whether any row still needs frames: connecting, or holding the spinner after it connected.
    fn animating(&self, now: Instant, delay: Duration, hold: Duration) -> bool {
        self.meta.values().any(|m| match &m.phase {
            Phase::Connecting(_) => true,
            Phase::Connected { busy, at, .. } => spinner_lingers(*busy, *at, now, delay, hold),
            _ => false,
        })
    }

    /// Decides what a finished Keychain read means; the secret moves straight into the command.
    fn keychain_loaded(
        &mut self,
        key: &str,
        session: SessionId,
        loaded: Result<Option<Secret>, String>,
        conn: Connection,
        root: Option<PathBuf>,
    ) -> Keychain {
        if self.meta.get(key).and_then(|m| m.session) != Some(session) {
            return Keychain::Done;
        }
        match loaded {
            Ok(Some(secret)) => Keychain::Connect(Box::new(Command::Connect {
                session,
                conn,
                password: Some(secret),
                root,
            })),
            Ok(None) => {
                self.meta.remove(key);
                Keychain::Prompt
            }
            Err(e) => {
                self.fail(key, e);
                Keychain::Done
            }
        }
    }

    fn is_test(&self, session: SessionId) -> bool {
        self.test.is_some_and(|(s, _)| s == session)
    }

    fn finish_test(&mut self, result: Result<String, String>) {
        let ms = self
            .test
            .take()
            .map_or(0, |(_, started)| started.elapsed().as_millis());
        self.test_done = Some((result, ms));
    }

    fn send(&self, command: Command) {
        if let Some(handle) = &self.handle {
            handle.send(command);
        }
    }

    fn key_of(&self, session: SessionId) -> Option<&str> {
        self.meta
            .iter()
            .find(|(_, m)| m.session == Some(session))
            .map(|(k, _)| k.as_str())
    }

    fn begin(&mut self, key: &str) -> SessionId {
        let session = SessionId(self.next_session);
        self.next_session += 1;
        self.meta.insert(
            key.to_owned(),
            Meta {
                session: Some(session),
                phase: Phase::Connecting(Busy::start()),
            },
        );
        session
    }

    fn fail(&mut self, key: &str, error: String) {
        self.meta.insert(
            key.to_owned(),
            Meta {
                session: None,
                phase: Phase::Failed(error),
            },
        );
    }

    /// Applies a worker update; the command it returns, if any, must be sent back to the worker.
    pub(super) fn on_update(&mut self, update: Update) -> Option<Command> {
        match update {
            Update::Connected { session, server } if self.is_test(session) => {
                self.finish_test(Ok(server));
                Some(Command::Disconnect { session })
            }
            Update::Failed { session, error } if self.is_test(session) => {
                self.finish_test(Err(error));
                None
            }
            Update::Connected { session, server } => {
                let key = self.key_of(session).map(str::to_owned);
                match key.and_then(|k| self.meta.get_mut(&k)) {
                    Some(meta) => {
                        if let Phase::Connecting(busy) = meta.phase {
                            meta.phase = Phase::Connected {
                                server,
                                busy,
                                at: Instant::now(),
                            };
                        }
                        None
                    }
                    None => Some(Command::Disconnect { session }),
                }
            }
            Update::Failed { session, error } => {
                if let Some(key) = self.key_of(session).map(str::to_owned) {
                    self.fail(&key, error);
                }
                None
            }
            Update::Columns { .. }
            | Update::Rows { .. }
            | Update::Done { .. }
            | Update::RunError { .. }
            | Update::Catalog { .. } => None,
        }
    }
}

impl Shell {
    fn db_start_worker(&mut self, cx: &mut Context<Self>) {
        if self.db.handle.is_some() {
            return;
        }
        let (tx, rx) = async_channel::unbounded::<Update>();
        self.db.handle = Some(daedalus_db::worker::start(tx));
        self.db._updates = Some(cx.spawn(async move |this, cx| {
            while let Ok(update) = rx.recv().await {
                let kept = this.update(cx, |shell, cx| {
                    if let Some(cmd) = shell.db.on_update(update) {
                        shell.db.send(cmd);
                    }
                    shell.db_report_test(cx);
                    cx.notify();
                });
                if kept.is_err() {
                    return;
                }
            }
        }));
    }

    /// Redraws the connecting spinner on a timer while the Data panel is on screen.
    fn data_shown(&self) -> bool {
        self.workspace.active.is_some()
            && self.workspace.ui.tree_visible
            && self.workspace.ui.side_mode == daedalus_workspace::SideMode::Data
    }

    fn db_start_spin(&mut self, cx: &mut Context<Self>) {
        if self.db.spin.is_some() {
            return;
        }
        let start = Instant::now();
        self.db.spin = Some(cx.spawn(async move |this, cx| {
            loop {
                let wait = motion::until_next_spin(start.elapsed());
                cx.background_executor().timer(wait).await;
                let kept = this.update(cx, |shell, cx| {
                    let m = &cx.theme().motion;
                    let (delay, hold) = (m.indicator_delay, m.indicator_hold);
                    let kept = shell.db.animating(Instant::now(), delay, hold);
                    if !kept {
                        shell.db.spin = None;
                    } else if shell.data_shown() {
                        cx.notify();
                    }
                    kept
                });
                if !kept.unwrap_or(false) {
                    break;
                }
            }
        }));
    }

    fn db_ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.db.loaded || self.db.load_task.is_some() {
            return;
        }
        self.db.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let path = path()?;
                    daedalus_db::load(&path).map_err(|e| format!("{e:#}"))
                })
                .await;
            this.update(cx, |shell, cx| {
                match loaded {
                    Ok(store) => shell.db.store = Some(store),
                    Err(e) => shell.db.load_error = Some(e),
                }
                shell.db.loaded = true;
                cx.notify();
            })
            .ok();
        }));
    }

    fn db_reload(&mut self, cx: &mut Context<Self>) {
        self.db.loaded = false;
        self.db.load_error = None;
        self.db.load_task = None;
        cx.notify();
    }

    fn db_connect(&mut self, listed: Listed, window: &mut Window, cx: &mut Context<Self>) {
        let key = conn_key(listed.origin, &listed.conn.id);
        if matches!(
            self.db.phase(&key),
            Phase::Connecting(_) | Phase::Connected { .. }
        ) {
            return;
        }
        if listed.conn.engine != Engine::Sqlite && !listed.conn.save_password {
            self.db_prompt_password(&key, listed, window, cx);
            return;
        }
        self.db_start_worker(cx);
        let session = self.db.begin(&key);
        self.db_start_spin(cx);
        cx.notify();
        let root = self.active_root();
        let conn = listed.conn;
        if conn.engine == Engine::Sqlite {
            self.db.send(Command::Connect {
                session,
                conn,
                password: None,
                root,
            });
            return;
        }
        let account = daedalus_db::keychain_account(listed.origin, root.as_deref(), &conn);
        let origin = listed.origin;
        cx.spawn_in(window, async move |this, cx| {
            let loaded: Result<Option<Secret>, String> = cx
                .background_executor()
                .spawn(async move { db_secrets::load(&account).map_err(|e| e.to_string()) })
                .await;
            this.update_in(cx, |shell, window, cx| {
                let listed = Listed {
                    conn: conn.clone(),
                    origin,
                };
                match shell.db.keychain_loaded(&key, session, loaded, conn, root) {
                    Keychain::Connect(cmd) => shell.db.send(*cmd),
                    Keychain::Prompt => shell.db_prompt_password(&key, listed, window, cx),
                    Keychain::Done => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn db_disconnect(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(meta) = self.db.meta.get_mut(key) {
            if let Some(session) = meta.session.take() {
                self.db.send(Command::Disconnect { session });
            }
            if let Some(meta) = self.db.meta.get_mut(key) {
                meta.phase = Phase::Idle;
            }
        }
        cx.notify();
    }
}

/// Loads databases.json fresh, applies `change` and writes it back; a file that fails to parse is never overwritten.
fn edit_store(change: impl FnOnce(&mut Store)) -> Result<Store, String> {
    let path = path()?;
    let mut store = daedalus_db::load(&path).map_err(|e| format!("{e:#}"))?;
    change(&mut store);
    daedalus_db::save(&path, &store).map_err(|e| format!("{e:#}"))?;
    Ok(store)
}

impl Shell {
    pub(super) fn db_new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_side_mode(daedalus_workspace::SideMode::Data, window, cx);
        self.db_open_sheet(SheetMode::New, window, cx);
    }

    fn db_open_sheet(&mut self, mode: SheetMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.db.sheet.is_some() {
            return;
        }
        let root = self.active_root();
        let entity = cx.new(|cx| DbSheet::new(mode, root, window, cx));
        let subscription = cx.subscribe_in(
            &entity,
            window,
            |this, _, event: &DbSheetEvent, window, cx| {
                this.db_sheet_event(event.clone(), window, cx)
            },
        );
        self.db.sheet_opened = self.next_generation();
        self.db.sheet_closing = None;
        self.db.sheet = Some(OpenSheet {
            entity,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn db_close_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((session, _)) = self.db.test.take() {
            self.db.send(Command::Disconnect { session });
        }
        self.db.test_done = None;
        if let Some(open) = self.db.sheet.take() {
            let generation = self.next_generation();
            self.db.sheet_closing = Some((open.entity, Closing::new(generation)));
            let t = cx.theme();
            let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                this.update(cx, |this, cx| {
                    if this
                        .db
                        .sheet_closing
                        .as_ref()
                        .is_some_and(|(_, c)| c.generation == generation)
                    {
                        this.db.sheet_closing = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        self.focus_active_item(window, cx);
        cx.notify();
    }

    fn db_sheet_event(&mut self, event: DbSheetEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            DbSheetEvent::Save {
                conn,
                origin,
                password,
            } => self.db_save(*conn, origin, password, window, cx),
            DbSheetEvent::Test {
                conn,
                typed,
                use_saved,
            } => self.db_test(*conn, typed, use_saved, cx),
            DbSheetEvent::Delete(listed) => self.db_delete(listed, window, cx),
            DbSheetEvent::Unlock {
                listed,
                password,
                save,
            } => self.db_unlock(listed, password, save, window, cx),
            DbSheetEvent::Close => self.db_close_sheet(window, cx),
        }
    }

    fn db_prompt_password(
        &mut self,
        key: &str,
        listed: Listed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.db.sheet.is_some() {
            self.db.fail(
                key,
                "No saved password. Close the open sheet and connect again.".into(),
            );
            cx.notify();
            return;
        }
        self.db_open_sheet(SheetMode::Password { listed }, window, cx);
    }

    fn db_sheet_is(&self, opened: u64) -> bool {
        self.db.sheet.is_some() && self.db.sheet_opened == opened
    }

    fn db_set_store(&mut self, store: Store) {
        self.db.store = Some(store);
        self.db.load_error = None;
        self.db.loaded = true;
    }

    fn db_sheet_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(open) = &self.db.sheet {
            open.entity.update(cx, |s, cx| s.set_error(message, cx));
        }
    }

    fn db_report_test(&mut self, cx: &mut Context<Self>) {
        let Some((result, ms)) = self.db.test_done.take() else {
            return;
        };
        if let Some(open) = &self.db.sheet {
            open.entity
                .update(cx, |s, cx| s.set_test_result(result, ms, cx));
        }
    }

    fn db_test(
        &mut self,
        conn: Connection,
        typed: Option<Secret>,
        use_saved: Option<Listed>,
        cx: &mut Context<Self>,
    ) {
        if let Some((old, _)) = self.db.test.take() {
            self.db.send(Command::Disconnect { session: old });
        }
        self.db_start_worker(cx);
        let session = SessionId(self.db.next_session);
        self.db.next_session += 1;
        self.db.test = Some((session, Instant::now()));
        let root = self.active_root();
        let saved = use_saved.filter(|l| l.conn.engine != Engine::Sqlite);
        let (Some(listed), None) = (saved, &typed) else {
            self.db.send(Command::Connect {
                session,
                conn,
                password: typed,
                root,
            });
            return;
        };
        let account = daedalus_db::keychain_account(listed.origin, root.as_deref(), &listed.conn);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { db_secrets::load(&account).map_err(|e| e.to_string()) })
                .await;
            this.update(cx, |shell, cx| {
                if !shell.db.is_test(session) {
                    return;
                }
                match loaded {
                    Ok(password) => shell.db.send(Command::Connect {
                        session,
                        conn,
                        password,
                        root,
                    }),
                    Err(e) => {
                        shell.db.finish_test(Err(e));
                        shell.db_report_test(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn db_save(
        &mut self,
        conn: Connection,
        origin: Origin,
        password: PasswordChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = self.active_root();
        let account = daedalus_db::keychain_account(origin, root.as_deref(), &conn);
        let old = self
            .db
            .store
            .as_ref()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn.id))
            .cloned();
        let key = conn_key(origin, &conn.id);
        let new = conn.clone();
        let opened = self.db.sheet_opened;
        cx.spawn_in(window, async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move {
                    match &password {
                        PasswordChange::Set(secret)
                            if conn.save_password && conn.engine != Engine::Sqlite =>
                        {
                            db_secrets::save(&account, secret).map_err(|e| e.to_string())?
                        }
                        PasswordChange::Forget => {
                            db_secrets::delete(&account).map_err(|e| e.to_string())?
                        }
                        _ => {}
                    }
                    edit_store(|store| store.upsert(conn))
                })
                .await;
            this.update_in(cx, |shell, window, cx| match saved {
                Ok(store) => {
                    shell.db_set_store(store);
                    if old.is_some_and(|old| needs_reconnect(&old, &new)) {
                        shell.db_disconnect(&key, cx);
                    }
                    shell.db.selected = Some(key);
                    if shell.db_sheet_is(opened) {
                        shell.db_close_sheet(window, cx);
                    }
                }
                Err(e) if shell.db_sheet_is(opened) => shell.db_sheet_error(e, cx),
                Err(_) => {}
            })
            .ok();
        })
        .detach();
    }

    fn db_unlock(
        &mut self,
        listed: Listed,
        password: Secret,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.db_close_sheet(window, cx);
        let key = conn_key(listed.origin, &listed.conn.id);
        if matches!(
            self.db.phase(&key),
            Phase::Connecting(_) | Phase::Connected { .. }
        ) {
            return;
        }
        self.db_start_worker(cx);
        let session = self.db.begin(&key);
        self.db_start_spin(cx);
        let root = self.active_root();
        let typed = (!password.is_empty()).then(|| password.clone());
        let account = daedalus_db::keychain_account(listed.origin, root.as_deref(), &listed.conn);
        let (id, was_saved) = (listed.conn.id.clone(), listed.conn.save_password);
        self.db.send(Command::Connect {
            session,
            conn: listed.conn,
            password: typed,
            root,
        });
        cx.notify();
        if !save || password.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let stored = cx
                .background_executor()
                .spawn(async move {
                    db_secrets::save(&account, &password).map_err(|e| e.to_string())?;
                    if was_saved {
                        return Ok(None);
                    }
                    edit_store(|store| {
                        if let Some(c) = store.connections.iter_mut().find(|c| c.id == id) {
                            c.save_password = true;
                        }
                    })
                    .map(Some)
                })
                .await;
            this.update(cx, |shell, cx| {
                match stored {
                    Ok(Some(store)) => shell.db_set_store(store),
                    Ok(None) => {}
                    Err(e) => shell.transient_notice("Could not save the password", e, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn db_delete(
        &mut self,
        listed: Listed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete {}?", listed.conn.name),
            Some("Its saved password is removed too."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                this.update_in(cx, |shell, window, cx| {
                    shell.db_delete_confirmed(listed, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn db_delete_confirmed(&mut self, listed: Listed, window: &mut Window, cx: &mut Context<Self>) {
        let key = conn_key(listed.origin, &listed.conn.id);
        self.db_disconnect(&key, cx);
        self.db.meta.remove(&key);
        if self.db.selected.as_deref() == Some(key.as_str()) {
            self.db.selected = None;
        }
        let root = self.active_root();
        let account = daedalus_db::keychain_account(listed.origin, root.as_deref(), &listed.conn);
        let has_secret = listed.conn.save_password && listed.conn.engine != Engine::Sqlite;
        let id = listed.conn.id;
        let opened = self.db.sheet_opened;
        cx.spawn_in(window, async move |this, cx| {
            let (keychain, removed) = cx
                .background_executor()
                .spawn(async move {
                    let keychain = match has_secret {
                        true => db_secrets::delete(&account).err().map(|e| e.to_string()),
                        false => None,
                    };
                    let removed = edit_store(|store| {
                        store.remove(&id);
                    });
                    (keychain, removed)
                })
                .await;
            this.update_in(cx, |shell, window, cx| {
                if let Some(e) = keychain {
                    shell.transient_notice("Could not remove the saved password", e, cx);
                }
                match removed {
                    Ok(store) => {
                        shell.db_set_store(store);
                        if shell.db_sheet_is(opened) {
                            shell.db_close_sheet(window, cx);
                        }
                    }
                    Err(e) => shell.transient_notice("Could not delete the connection", e, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_db_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (entity, closing) = match (&self.db.sheet, &self.db.sheet_closing) {
            (Some(open), _) => (open.entity.clone(), None),
            (None, Some((entity, closing))) => (entity.clone(), Some(*closing)),
            (None, None) => return None,
        };
        let t = cx.theme().clone();
        let panel = div().id("db-sheet-panel").w(t.ui(520.)).child(entity);
        let panel = sheet_motion(panel, closing, self.db.sheet_opened, &t);
        Some(
            div()
                .id("db-sheet-layer")
                .absolute()
                .inset_0()
                .occlude()
                .bg(t.color.overlay)
                .flex()
                .justify_center()
                .items_start()
                .pt(t.ui(96.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(panel)
                .into_any_element(),
        )
    }
}

fn sheet_motion(
    panel: gpui::Stateful<gpui::Div>,
    closing: Option<Closing>,
    opened: u64,
    t: &Theme,
) -> AnyElement {
    match closing {
        Some(closing) => motion::animate_exit(
            t.motion.reduced,
            panel,
            ("db-sheet-exit", closing.generation),
            t.motion.fast,
            |el, d| el.opacity(1. - d).top(px(-4. * d)),
        ),
        None => {
            let spring = motion::ease_spring();
            let fade = motion::ease_enter();
            motion::animate_if(
                t.motion.reduced,
                panel,
                ("db-sheet-enter", opened),
                Animation::new(t.motion.slow),
                move |el, d| {
                    el.opacity(fade((d * 2.5).min(1.)))
                        .top(px(-8. * (1. - spring(d))))
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(key: &str, session: u64) -> DbState {
        let mut db = DbState::default();
        db.meta.insert(
            key.into(),
            Meta {
                session: Some(SessionId(session)),
                phase: Phase::Connecting(Busy::start()),
            },
        );
        db
    }

    #[test]
    fn connected_update_marks_the_row_connected() {
        let mut db = state_with("g:a", 1);
        let cmd = db.on_update(Update::Connected {
            session: SessionId(1),
            server: "SQLite 3.45".into(),
        });
        assert!(cmd.is_none());
        assert!(
            matches!(db.phase("g:a"), Phase::Connected { server, .. } if server == "SQLite 3.45")
        );
    }

    #[test]
    fn failed_update_shows_the_error_and_drops_the_session() {
        let mut db = state_with("g:a", 1);
        let cmd = db.on_update(Update::Failed {
            session: SessionId(1),
            error: "no such file".into(),
        });
        assert!(cmd.is_none());
        assert_eq!(db.phase("g:a"), Phase::Failed("no such file".into()));
        assert_eq!(db.meta["g:a"].session, None);
    }

    #[test]
    fn a_connected_update_for_an_unknown_session_asks_to_disconnect() {
        let mut db = state_with("g:a", 1);
        let cmd = db.on_update(Update::Connected {
            session: SessionId(9),
            server: "x".into(),
        });
        assert!(matches!(
            cmd,
            Some(Command::Disconnect {
                session: SessionId(9)
            })
        ));
        assert!(matches!(db.phase("g:a"), Phase::Connecting(_)));
    }

    fn conn() -> Connection {
        serde_json::from_str(r#"{"id":"a","name":"A","engine":"sqlite"}"#).unwrap()
    }

    #[test]
    fn a_row_disconnected_during_the_keychain_load_sends_nothing() {
        let mut db = state_with("g:a", 1);
        db.meta.get_mut("g:a").unwrap().session = None;
        let cmd = db.keychain_loaded(
            "g:a",
            SessionId(1),
            Ok(Some(Secret::new(String::from("pw")))),
            conn(),
            None,
        );
        assert!(matches!(cmd, Keychain::Done));
    }

    #[test]
    fn a_newer_session_makes_the_old_keychain_result_a_no_op() {
        let mut db = state_with("g:a", 2);
        let cmd = db.keychain_loaded("g:a", SessionId(1), Ok(None), conn(), None);
        assert!(matches!(cmd, Keychain::Done));
        assert!(matches!(db.phase("g:a"), Phase::Connecting(_)));
        let cmd = db.keychain_loaded(
            "g:a",
            SessionId(2),
            Ok(Some(Secret::new(String::from("pw")))),
            conn(),
            None,
        );
        assert!(matches!(cmd, Keychain::Connect(cmd) if matches!(*cmd, Command::Connect { .. })));
    }

    #[test]
    fn no_saved_password_asks_for_one_instead_of_failing() {
        let mut db = state_with("g:a", 1);
        let cmd = db.keychain_loaded("g:a", SessionId(1), Ok(None), conn(), None);
        assert!(matches!(cmd, Keychain::Prompt));
        assert_eq!(db.phase("g:a"), Phase::Idle);
    }

    #[test]
    fn a_failed_test_session_sends_nothing_and_clears_the_test() {
        let mut db = DbState {
            test: Some((SessionId(5), Instant::now())),
            ..DbState::default()
        };
        let cmd = db.on_update(Update::Failed {
            session: SessionId(5),
            error: "refused".into(),
        });
        assert!(cmd.is_none() && db.test.is_none());
        assert!(matches!(db.test_done, Some((Err(ref e), _)) if e == "refused"));
    }

    #[test]
    fn a_late_connected_for_a_replaced_test_session_is_disconnected() {
        let mut db = DbState {
            test: Some((SessionId(6), Instant::now())),
            ..DbState::default()
        };
        let cmd = db.on_update(Update::Connected {
            session: SessionId(5),
            server: "x".into(),
        });
        assert!(matches!(
            cmd,
            Some(Command::Disconnect {
                session: SessionId(5)
            })
        ));
        assert!(db.test.is_some() && db.test_done.is_none());
    }

    #[test]
    fn a_test_session_is_disconnected_after_connected() {
        let mut db = DbState {
            test: Some((SessionId(5), Instant::now())),
            ..DbState::default()
        };
        let cmd = db.on_update(Update::Connected {
            session: SessionId(5),
            server: "SQLite 3.45".into(),
        });
        assert!(matches!(
            cmd,
            Some(Command::Disconnect {
                session: SessionId(5)
            })
        ));
        assert!(db.test.is_none());
        assert!(matches!(db.test_done, Some((Ok(ref s), _)) if s == "SQLite 3.45"));
    }

    #[test]
    fn a_connect_finishing_after_the_delay_keeps_the_spinner_until_the_hold_ends() {
        let (delay, hold) = (Duration::from_millis(250), Duration::from_millis(400));
        let busy = Busy::start();
        let at = busy.since() + Duration::from_millis(300);
        let lingers = |ms| spinner_lingers(busy, at, at + Duration::from_millis(ms), delay, hold);
        assert!(lingers(0));
        assert!(lingers(349));
        assert!(!lingers(350));
        let quick = busy.since() + Duration::from_millis(100);
        assert!(!spinner_lingers(busy, quick, quick, delay, hold));
    }
}
