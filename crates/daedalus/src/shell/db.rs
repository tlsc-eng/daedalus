use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use daedalus_db::worker::{Command, Handle, SessionId, Update};
use daedalus_db::{Engine, Listed, Origin, Secret, Store};
use daedalus_ui::motion::{self, Busy};
use gpui::{Context, Task};

use super::Shell;
use crate::db_secrets;

mod explorer;

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
    Connected(String),
    Failed(String),
}

fn conn_key(origin: Origin, id: &str) -> String {
    match origin {
        Origin::Global => format!("g:{id}"),
        Origin::Project => format!("p:{id}"),
    }
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

    fn connecting(&self) -> bool {
        self.meta
            .values()
            .any(|m| matches!(m.phase, Phase::Connecting(_)))
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
            Update::Connected { session, server } => {
                let key = self.key_of(session).map(str::to_owned);
                match key.and_then(|k| self.meta.get_mut(&k)) {
                    Some(meta) => {
                        meta.phase = Phase::Connected(server);
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
                    cx.notify();
                });
                if kept.is_err() {
                    return;
                }
            }
        }));
    }

    /// Wakes only while a row connects and the Data panel shows, so an idle panel costs no frames.
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
                    let kept = shell.db.connecting();
                    if !kept {
                        shell.db.spin = None;
                    } else if shell.workspace.ui.side_mode == daedalus_workspace::SideMode::Data {
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

    fn db_connect(&mut self, listed: Listed, cx: &mut Context<Self>) {
        let key = conn_key(listed.origin, &listed.conn.id);
        if matches!(
            self.db.phase(&key),
            Phase::Connecting(_) | Phase::Connected(_)
        ) {
            return;
        }
        self.db_start_worker(cx);
        let session = self.db.begin(&key);
        self.db_start_spin(cx);
        cx.notify();
        let root = self.active_root();
        let conn = listed.conn;
        if !conn.save_password || conn.engine == Engine::Sqlite {
            self.db.send(Command::Connect {
                session,
                conn,
                password: None,
                root,
            });
            return;
        }
        let account = daedalus_db::keychain_account(listed.origin, root.as_deref(), &conn);
        cx.spawn(async move |this, cx| {
            let loaded: Result<Option<Secret>, String> = cx
                .background_executor()
                .spawn(async move { db_secrets::load(&account).map_err(|e| e.to_string()) })
                .await;
            this.update(cx, |shell, cx| {
                let current = shell.db.meta.get(&key).and_then(|m| m.session) == Some(session);
                if !current {
                    return;
                }
                match loaded {
                    Ok(Some(secret)) => shell.db.send(Command::Connect {
                        session,
                        conn,
                        password: Some(secret),
                        root,
                    }),
                    Ok(None) => shell.db.fail(
                        &key,
                        "No saved password. Edit the connection to add one.".into(),
                    ),
                    Err(e) => shell.db.fail(&key, e),
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
        assert_eq!(db.phase("g:a"), Phase::Connected("SQLite 3.45".into()));
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
}
