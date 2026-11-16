use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use daedalus_db::catalog::Catalog;
use daedalus_db::claude::{self, Collector, MAX_ROWS, QUERY_TIMEOUT, Step};
use daedalus_db::worker::{Command, RunId, RunMode, SessionId, Update};
use daedalus_db::{Listed, Store};
use daedalus_proto::{AppMsg, AppReply, PaneId, Private};
use gpui::Context;

use super::{Meta, Phase, Shell, conn_key, path};

const MAX_RUNS_PER_SESSION: usize = 4;
/// Counts from the send, so a run still queued behind other work can expire before it starts.
const DEADLINE: Duration = Duration::from_secs(QUERY_TIMEOUT.as_secs() + 5);
const NO_CALLER: &str = "Database tools work only from Claude in a Daedalus terminal.";
const UNFINISHED: &str = "The query did not finish.";

fn clamp_rows(asked: u32) -> u32 {
    asked.clamp(1, MAX_ROWS)
}

fn failure(message: impl Into<String>) -> AppReply {
    AppReply::Error(message.into())
}

fn json(text: String) -> AppReply {
    AppReply::DbJson(Private(text))
}

/// What a catalog request asked for.
#[derive(Clone)]
enum Wanted {
    Tables(Option<String>),
    Describe(Option<String>, String),
}

impl Wanted {
    fn answer(&self, catalog: &Catalog) -> AppReply {
        let found = match self {
            Wanted::Tables(schema) => claude::tables_json(catalog, schema.as_deref()),
            Wanted::Describe(schema, table) => {
                claude::table_json(catalog, schema.as_deref(), table)
            }
        };
        found.map_or_else(failure, json)
    }
}

struct ClaudeRun {
    reply: SyncSender<AppReply>,
    collector: Collector,
    answered: bool,
    cancelled: bool,
}

struct Waiter {
    id: u64,
    reply: SyncSender<AppReply>,
    wanted: Wanted,
}

/// What the shell must do after one update reached a Claude run.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Fed {
    pub cancel: bool,
    pub ended: bool,
}

/// Claude's queries and catalog requests in flight, so each is answered once and none outlives its session.
#[derive(Default)]
pub(super) struct ClaudeDb {
    runs: HashMap<(SessionId, RunId), ClaudeRun>,
    waiters: HashMap<SessionId, Vec<Waiter>>,
    next: u64,
    told: HashSet<String>,
}

impl ClaudeDb {
    fn bump(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn runs_in(&self, session: SessionId) -> usize {
        self.runs.keys().filter(|(s, _)| *s == session).count()
    }

    /// Starts tracking a run; refuses with an answer to the caller when the session is full.
    fn begin_run(
        &mut self,
        session: SessionId,
        reply: &SyncSender<AppReply>,
        rows: u32,
    ) -> Option<RunId> {
        if self.runs_in(session) >= MAX_RUNS_PER_SESSION {
            let _ = reply.send(failure(
                "Too many queries are running; wait for one to finish.",
            ));
            return None;
        }
        let run = RunId(self.bump());
        self.runs.insert(
            (session, run),
            ClaudeRun {
                reply: reply.clone(),
                collector: Collector::new(rows),
                answered: false,
                cancelled: false,
            },
        );
        Some(run)
    }

    fn owns(&self, session: SessionId, run: RunId) -> bool {
        self.runs.contains_key(&(session, run))
    }

    /// Folds an update into its run; None when the run is not Claude's.
    fn feed(&mut self, session: SessionId, run: RunId, update: Update) -> Option<Fed> {
        let entry = self.runs.get_mut(&(session, run))?;
        let step = entry.collector.feed(update);
        let answer = |entry: &mut ClaudeRun, reply: AppReply| {
            if !entry.answered {
                entry.answered = true;
                let _ = entry.reply.send(reply);
            }
        };
        let mut fed = Fed {
            cancel: false,
            ended: false,
        };
        match step {
            Step::Wait => {}
            Step::Cancel => fed.cancel = !entry.cancelled,
            Step::Reject(message) => {
                answer(entry, failure(message));
                fed.cancel = !entry.cancelled;
            }
            Step::Done(text) => {
                answer(entry, json(text));
                fed.ended = true;
            }
            Step::Failed(message) => {
                answer(entry, failure(message));
                fed.ended = true;
            }
        }
        if fed.cancel {
            entry.cancelled = true;
        }
        if fed.ended {
            self.runs.remove(&(session, run));
        }
        Some(fed)
    }

    fn add_waiter(
        &mut self,
        session: SessionId,
        reply: SyncSender<AppReply>,
        wanted: Wanted,
    ) -> u64 {
        let id = self.bump();
        self.waiters
            .entry(session)
            .or_default()
            .push(Waiter { id, reply, wanted });
        id
    }

    fn answer_waiters(&mut self, session: SessionId, catalog: &Result<Catalog, String>) {
        for w in self.waiters.remove(&session).unwrap_or_default() {
            let _ = w.reply.send(match catalog {
                Ok(c) => w.wanted.answer(c),
                Err(e) => failure(e.clone()),
            });
        }
    }

    /// Fails every run and waiter of a session; returns how many runs it dropped.
    fn fail_session(&mut self, session: SessionId, message: &str) -> usize {
        let keys: Vec<_> = self
            .runs
            .keys()
            .filter(|(s, _)| *s == session)
            .copied()
            .collect();
        for key in &keys {
            if let Some(run) = self.runs.remove(key)
                && !run.answered
            {
                let _ = run.reply.send(failure(message));
            }
        }
        for w in self.waiters.remove(&session).unwrap_or_default() {
            let _ = w.reply.send(failure(message));
        }
        keys.len()
    }

    /// Ends a run whose deadline passed; the bool says whether a Cancel still has to go out.
    fn expire_run(&mut self, session: SessionId, run: RunId) -> Option<bool> {
        let entry = self.runs.remove(&(session, run))?;
        if !entry.answered {
            let _ = entry.reply.send(failure(UNFINISHED));
        }
        Some(!entry.cancelled)
    }

    fn expire_waiter(&mut self, session: SessionId, id: u64) -> bool {
        let Some(list) = self.waiters.get_mut(&session) else {
            return false;
        };
        let Some(at) = list.iter().position(|w| w.id == id) else {
            return false;
        };
        let w = list.remove(at);
        let _ = w.reply.send(failure(UNFINISHED));
        true
    }
}

fn is_db_tool(msg: &AppMsg) -> bool {
    matches!(
        msg,
        AppMsg::DbConnections
            | AppMsg::DbTables { .. }
            | AppMsg::DbDescribe { .. }
            | AppMsg::DbQuery { .. }
    )
}

fn run_of(update: &Update) -> Option<RunId> {
    match update {
        Update::Columns { run, .. }
        | Update::Rows { run, .. }
        | Update::Done { run, .. }
        | Update::RunError { run, .. } => Some(*run),
        _ => None,
    }
}

impl Shell {
    pub(in crate::shell) fn db_answer_claude(
        &mut self,
        msg: &AppMsg,
        caller: Option<PaneId>,
        reply: &SyncSender<AppReply>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !is_db_tool(msg) {
            return false;
        }
        let Some(pane) = caller else {
            let _ = reply.send(failure(NO_CALLER));
            return true;
        };
        let root = self.caller_root(Some(pane));
        let (msg, reply) = (msg.clone(), reply.clone());
        cx.spawn(async move |this, cx| {
            let store = cx
                .background_executor()
                .spawn(async move {
                    let path = path()?;
                    daedalus_db::load(&path).map_err(|e| format!("{e:#}"))
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                shell.db_claude_ask(msg, root, store, reply, cx)
            });
        })
        .detach();
        true
    }

    fn db_claude_ask(
        &mut self,
        msg: AppMsg,
        root: Option<PathBuf>,
        store: Result<Store, String>,
        reply: SyncSender<AppReply>,
        cx: &mut Context<Self>,
    ) {
        let Ok(store) = store else {
            let _ = reply.send(failure("Daedalus could not read its database list."));
            return;
        };
        let visible = claude::visible(&store, root.as_deref());
        let connection = match &msg {
            AppMsg::DbConnections => {
                let items: Vec<_> = visible
                    .into_iter()
                    .map(|l| {
                        let server = match self.db.phase(&conn_key(l.origin, &l.conn.id)) {
                            Phase::Connected { server, .. } => Some(server),
                            _ => None,
                        };
                        (l, server)
                    })
                    .collect();
                let _ = reply.send(json(claude::connections_json(&items)));
                return;
            }
            AppMsg::DbTables { connection, .. }
            | AppMsg::DbDescribe { connection, .. }
            | AppMsg::DbQuery { connection, .. } => connection,
            _ => return,
        };
        let listed = match claude::find(&visible, connection) {
            Ok(l) => l.clone(),
            Err(e) => {
                let _ = reply.send(failure(e));
                return;
            }
        };
        let key = conn_key(listed.origin, &listed.conn.id);
        let session = match self.db.meta.get(&key) {
            Some(Meta {
                session: Some(session),
                phase: Phase::Connected { .. },
                ..
            }) => *session,
            _ => {
                let _ = reply.send(failure(format!(
                    "{} is not connected. Ask the user to connect it in the Data panel.",
                    listed.conn.name
                )));
                return;
            }
        };
        match msg {
            AppMsg::DbTables { schema, .. } => {
                self.db_claude_catalog(&key, session, Wanted::Tables(schema), reply, cx)
            }
            AppMsg::DbDescribe { schema, table, .. } => {
                self.db_claude_catalog(&key, session, Wanted::Describe(schema, table), reply, cx)
            }
            AppMsg::DbQuery { sql, max_rows, .. } => {
                self.db_claude_query(&listed, &key, session, sql, max_rows, reply, root, cx)
            }
            _ => {}
        }
    }

    fn db_claude_catalog(
        &mut self,
        key: &str,
        session: SessionId,
        wanted: Wanted,
        reply: SyncSender<AppReply>,
        cx: &mut Context<Self>,
    ) {
        if let Some(catalog) = self.db.meta.get(key).and_then(|m| m.catalog.as_ref()) {
            let _ = reply.send(wanted.answer(catalog));
            return;
        }
        let id = self.db.claude.add_waiter(session, reply, wanted);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEADLINE).await;
            let _ = this.update(cx, |shell, _| {
                shell.db.claude.expire_waiter(session, id);
            });
        })
        .detach();
        if self.db.meta.get(key).is_some_and(|m| m.reading.is_none())
            && let Some(cmd) = self.db.introspect(key)
        {
            self.db.send(cmd);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn db_claude_query(
        &mut self,
        listed: &Listed,
        key: &str,
        session: SessionId,
        sql: Private,
        max_rows: u32,
        reply: SyncSender<AppReply>,
        root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let gated = match claude::gate(&sql.0, listed.conn.engine) {
            Ok(sql) => sql,
            Err(e) => {
                let _ = reply.send(failure(e));
                return;
            }
        };
        let rows = clamp_rows(max_rows);
        let Some(run) = self.db.claude.begin_run(session, &reply, rows) else {
            return;
        };
        if let Some(meta) = self.db.meta.get_mut(key) {
            meta.claude_runs += 1;
        }
        tracing::debug!(connection = %listed.conn.id, run = run.0, rows, "claude query started");
        self.db.send(claude_run(session, run, gated, rows));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEADLINE).await;
            let _ = this.update(cx, |shell, cx| shell.db_claude_expire(session, run, cx));
        })
        .detach();
        if self.db.claude.told.insert(key.to_owned()) {
            let project = root.as_deref().and_then(|r| r.file_name()).map_or_else(
                || "this window".into(),
                |n| n.to_string_lossy().into_owned(),
            );
            self.transient_notice(
                format!("Claude is reading {}", listed.conn.name),
                format!("Read-only queries from Claude in {project}."),
                cx,
            );
        }
        cx.notify();
    }

    fn db_claude_expire(&mut self, session: SessionId, run: RunId, cx: &mut Context<Self>) {
        let Some(cancel) = self.db.claude.expire_run(session, run) else {
            return;
        };
        if cancel {
            self.db.send(Command::Cancel { session, run });
        }
        self.db_claude_ended(session, 1);
        cx.notify();
    }

    fn db_claude_ended(&mut self, session: SessionId, runs: usize) {
        let key = self.db.key_of(session).map(str::to_owned);
        if let Some(meta) = key.and_then(|k| self.db.meta.get_mut(&k)) {
            meta.claude_runs = meta.claude_runs.saturating_sub(runs);
        }
    }

    pub(super) fn db_fail_claude(&mut self, session: SessionId, message: &str) {
        let dropped = self.db.claude.fail_session(session, message);
        self.db_claude_ended(session, dropped);
    }

    pub(super) fn db_name_of(&self, key: &str) -> String {
        let id = key.split_once(':').map_or(key, |(_, id)| id);
        self.db
            .store
            .as_ref()
            .and_then(|s| s.connections.iter().find(|c| c.id == id))
            .map_or_else(|| "the database".into(), |c| c.name.clone())
    }

    /// Takes Claude's updates out of the stream; everything else goes on to the explorer.
    pub(super) fn db_route_claude(&mut self, update: Update) -> Option<Update> {
        let session = update.session();
        if let Some(run) = run_of(&update) {
            if !self.db.claude.owns(session, run) {
                return Some(update);
            }
            if let Some(fed) = self.db.claude.feed(session, run, update) {
                if fed.cancel {
                    self.db.send(Command::Cancel { session, run });
                }
                if fed.ended {
                    self.db_claude_ended(session, 1);
                }
            }
            return None;
        }
        match &update {
            Update::Catalog { catalog, .. } => self.db.claude.answer_waiters(session, catalog),
            Update::Failed { error, .. } => self.db_fail_claude(session, error),
            _ => {}
        }
        Some(update)
    }
}

/// Claude's results are never allowed past the row cap, whatever else changes about export runs.
fn claude_run(session: SessionId, run: RunId, sql: String, rows: u32) -> Command {
    Command::Run {
        session,
        run,
        sql,
        page: rows as usize,
        timeout: Some(QUERY_TIMEOUT),
        mode: RunMode::ReadOnly,
        capped: true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, sync_channel};

    use super::*;

    const S: SessionId = SessionId(1);

    fn chan() -> (SyncSender<AppReply>, Receiver<AppReply>) {
        sync_channel(8)
    }

    fn wide_columns(run: RunId) -> Update {
        let columns = (0..2000)
            .map(|i| daedalus_db::value::Column {
                name: format!("column_name_{i:0>200}"),
                type_name: "text".into(),
                table: None,
            })
            .collect();
        Update::Columns {
            session: S,
            run,
            columns,
        }
    }

    #[test]
    fn claude_runs_keep_the_result_cap() {
        let cmd = claude_run(S, RunId(1), "select 1".into(), 10);
        assert!(matches!(
            cmd,
            Command::Run {
                capped: true,
                mode: RunMode::ReadOnly,
                ..
            }
        ));
    }

    #[test]
    fn only_the_four_db_messages_are_db_tools() {
        let tables = AppMsg::DbTables {
            connection: "c".into(),
            schema: None,
        };
        let describe = AppMsg::DbDescribe {
            connection: "c".into(),
            schema: None,
            table: "t".into(),
        };
        let query = AppMsg::DbQuery {
            connection: "c".into(),
            sql: Private("select 1".into()),
            max_rows: 1,
        };
        for msg in [AppMsg::DbConnections, tables, describe, query] {
            assert!(is_db_tool(&msg), "{msg:?}");
        }
        assert!(!is_db_tool(&AppMsg::TestResults));
    }

    #[test]
    fn clamp_rows_bounds() {
        assert_eq!(clamp_rows(0), 1);
        assert_eq!(clamp_rows(1), 1);
        assert_eq!(clamp_rows(50), 50);
        assert_eq!(clamp_rows(MAX_ROWS), MAX_ROWS);
        assert_eq!(clamp_rows(u32::MAX), MAX_ROWS);
    }

    #[test]
    fn fail_session_drains_runs_and_waiters() {
        let mut db = ClaudeDb::default();
        let (tx, rx) = chan();
        let other = SessionId(2);
        db.begin_run(S, &tx, 10).unwrap();
        let kept = db.begin_run(other, &tx, 10).unwrap();
        db.add_waiter(S, tx.clone(), Wanted::Tables(None));
        assert_eq!(db.fail_session(S, "gone"), 1);
        assert_eq!(rx.try_recv().unwrap(), AppReply::Error("gone".into()));
        assert_eq!(rx.try_recv().unwrap(), AppReply::Error("gone".into()));
        assert!(rx.try_recv().is_err());
        assert!(db.owns(other, kept));
        assert!(db.waiters.is_empty());
    }

    #[test]
    fn fifth_run_is_refused() {
        let mut db = ClaudeDb::default();
        let (tx, rx) = chan();
        for _ in 0..MAX_RUNS_PER_SESSION {
            assert!(db.begin_run(S, &tx, 10).is_some());
        }
        assert!(db.begin_run(S, &tx, 10).is_none());
        assert!(matches!(rx.try_recv().unwrap(), AppReply::Error(m) if m.contains("Too many")));
        assert!(db.begin_run(SessionId(2), &tx, 10).is_some());
    }

    #[test]
    fn reject_answers_once_and_waits_for_the_end() {
        let mut db = ClaudeDb::default();
        let (tx, rx) = chan();
        let run = db.begin_run(S, &tx, 10).unwrap();
        let fed = db.feed(S, run, wide_columns(run)).unwrap();
        assert_eq!(
            fed,
            Fed {
                cancel: true,
                ended: false
            }
        );
        assert!(matches!(rx.try_recv().unwrap(), AppReply::Error(_)));
        assert!(db.owns(S, run));
        let more = Update::Rows {
            session: S,
            run,
            rows: vec![],
            more: true,
        };
        assert_eq!(
            db.feed(S, run, more).unwrap(),
            Fed {
                cancel: false,
                ended: false
            }
        );
        let done = Update::RunError {
            session: S,
            run,
            message: "cancelled".into(),
            position: None,
        };
        assert_eq!(
            db.feed(S, run, done).unwrap(),
            Fed {
                cancel: false,
                ended: true
            }
        );
        assert!(rx.try_recv().is_err());
        assert!(!db.owns(S, run));
    }

    #[test]
    fn deadline_expires_a_stuck_run() {
        let mut db = ClaudeDb::default();
        let (tx, rx) = chan();
        let run = db.begin_run(S, &tx, 10).unwrap();
        assert_eq!(db.expire_run(S, run), Some(true));
        assert_eq!(rx.try_recv().unwrap(), AppReply::Error(UNFINISHED.into()));
        assert_eq!(db.expire_run(S, run), None);
        let id = db.add_waiter(S, tx.clone(), Wanted::Tables(None));
        assert!(db.expire_waiter(S, id));
        assert!(!db.expire_waiter(S, id));
        assert_eq!(rx.try_recv().unwrap(), AppReply::Error(UNFINISHED.into()));
        let rejected = db.begin_run(S, &tx, 10).unwrap();
        db.feed(S, rejected, wide_columns(rejected));
        let _ = rx.try_recv();
        assert_eq!(db.expire_run(S, rejected), Some(false));
        assert!(rx.try_recv().is_err());
    }
}
