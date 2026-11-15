use daedalus_db::worker::{Handle, SessionId, Update};
use gpui::{App, Entity, WeakEntity};

use super::console::{DbConsoleView, Info};
use super::table::DbTableView;

/// A tab that owns one worker session; consoles and tables share the session registry.
#[derive(Clone)]
pub(in crate::shell) enum DbTab {
    Console(Entity<DbConsoleView>),
    Table(Entity<DbTableView>),
}

#[derive(Clone)]
pub(in crate::shell) enum DbTabRef {
    Console(WeakEntity<DbConsoleView>),
    Table(WeakEntity<DbTableView>),
}

impl DbTab {
    pub fn key(&self, cx: &App) -> String {
        match self {
            Self::Console(v) => v.read(cx).key().to_owned(),
            Self::Table(v) => v.read(cx).key().to_owned(),
        }
    }

    pub fn needs_connect(&self, cx: &App) -> bool {
        match self {
            Self::Console(v) => v.read(cx).needs_connect(),
            Self::Table(v) => v.read(cx).needs_connect(),
        }
    }

    pub fn setup_error(&self, cx: &App) -> Option<String> {
        match self {
            Self::Console(v) => v.read(cx).setup_error(),
            Self::Table(v) => v.read(cx).setup_error(),
        }
    }

    pub fn connecting(&self, session: SessionId, handle: Handle, info: Info, cx: &mut App) {
        match self {
            Self::Console(v) => v.update(cx, |c, cx| c.connecting(session, handle, info, cx)),
            Self::Table(v) => v.update(cx, |c, cx| c.connecting(session, handle, info, cx)),
        }
    }

    pub fn prompting(&self, info: Info, cx: &mut App) {
        match self {
            Self::Console(v) => v.update(cx, |c, cx| c.prompting(info, cx)),
            Self::Table(v) => v.update(cx, |c, cx| c.prompting(info, cx)),
        }
    }

    pub fn idle(&self, cx: &mut App) {
        match self {
            Self::Console(v) => v.update(cx, |c, cx| c.idle(cx)),
            Self::Table(v) => v.update(cx, |c, cx| c.idle(cx)),
        }
    }

    pub fn fail(&self, message: impl Into<String>, cx: &mut App) {
        let message = message.into();
        match self {
            Self::Console(v) => v.update(cx, |c, cx| c.fail(message, cx)),
            Self::Table(v) => v.update(cx, |c, cx| c.fail(message, cx)),
        }
    }

    pub fn on_update(&self, update: Update, cx: &mut App) {
        match self {
            Self::Console(v) => v.update(cx, |c, cx| c.on_update(update, cx)),
            Self::Table(v) => v.update(cx, |c, cx| c.on_update(update, cx)),
        }
    }

    pub fn downgrade(&self) -> DbTabRef {
        match self {
            Self::Console(v) => DbTabRef::Console(v.downgrade()),
            Self::Table(v) => DbTabRef::Table(v.downgrade()),
        }
    }
}

impl DbTabRef {
    pub fn upgrade(&self) -> Option<DbTab> {
        match self {
            Self::Console(w) => w.upgrade().map(DbTab::Console),
            Self::Table(w) => w.upgrade().map(DbTab::Table),
        }
    }
}
