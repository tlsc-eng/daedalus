use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::catalog::{self, Catalog};
use crate::config::{Connection, Engine};
use crate::edit;
pub use crate::engine::ApplyError;
use crate::engine::{Driver, Item};
use crate::secret::Secret;
use crate::value::{Budget, Cell, Column};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INTROSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const APPLY_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_RESEND: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub u64);

/// What a manual transaction command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOp {
    Begin,
    Commit,
    Rollback,
}

/// How a run is wrapped: autocommit, inside the session's manual transaction, or a read-only transaction rolled back after.
/// ReadOnly is a backstop; callers must only pass `Kind::Read` scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Auto,
    Transaction,
    ReadOnly,
}

pub enum Command {
    Connect {
        session: SessionId,
        conn: Connection,
        password: Option<Secret>,
        root: Option<PathBuf>,
    },
    Disconnect {
        session: SessionId,
    },
    Run {
        session: SessionId,
        run: RunId,
        sql: String,
        page: usize,
        timeout: Option<Duration>,
        mode: RunMode,
        /// False lifts the result cap, for a consumer that writes each page out before asking for more.
        capped: bool,
    },
    Transaction {
        session: SessionId,
        op: TxOp,
    },
    Apply {
        session: SessionId,
        statements: Vec<edit::Stmt>,
    },
    FetchMore {
        session: SessionId,
        run: RunId,
        rows: usize,
    },
    Cancel {
        session: SessionId,
        run: RunId,
    },
    Introspect {
        session: SessionId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Connected {
        session: SessionId,
        server: String,
    },
    /// Built from driver error text only, so it never carries a password or URL.
    Failed {
        session: SessionId,
        error: String,
    },
    Columns {
        session: SessionId,
        run: RunId,
        columns: Vec<Column>,
    },
    Rows {
        session: SessionId,
        run: RunId,
        rows: Vec<Vec<Cell>>,
        more: bool,
    },
    Done {
        session: SessionId,
        run: RunId,
        affected: Option<u64>,
        elapsed: Duration,
        truncated: bool,
    },
    RunError {
        session: SessionId,
        run: RunId,
        message: String,
        position: Option<usize>,
    },
    Catalog {
        session: SessionId,
        catalog: Result<Catalog, String>,
    },
    TxState {
        session: SessionId,
        open: bool,
        error: Option<String>,
    },
    Applied {
        session: SessionId,
        result: Result<u64, ApplyError>,
    },
}

impl Update {
    /// The session an update belongs to.
    pub fn session(&self) -> SessionId {
        match self {
            Update::Connected { session, .. }
            | Update::Failed { session, .. }
            | Update::Columns { session, .. }
            | Update::Rows { session, .. }
            | Update::Done { session, .. }
            | Update::RunError { session, .. }
            | Update::Catalog { session, .. }
            | Update::TxState { session, .. }
            | Update::Applied { session, .. } => *session,
        }
    }
}

#[derive(Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
}

impl Handle {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// Starts the database worker on its own thread.
pub fn start(updates: async_channel::Sender<Update>) -> Handle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = std::thread::Builder::new()
        .name("databases".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let mut sessions: HashMap<SessionId, mpsc::UnboundedSender<Command>> =
                    HashMap::new();
                while let Some(command) = rx.recv().await {
                    dispatch(&mut sessions, &updates, command).await;
                }
            });
        });
    Handle { commands: tx }
}

impl Command {
    fn session(&self) -> SessionId {
        match self {
            Command::Connect { session, .. }
            | Command::Disconnect { session }
            | Command::Run { session, .. }
            | Command::Transaction { session, .. }
            | Command::Apply { session, .. }
            | Command::FetchMore { session, .. }
            | Command::Cancel { session, .. }
            | Command::Introspect { session } => *session,
        }
    }
}

async fn dispatch(
    sessions: &mut HashMap<SessionId, mpsc::UnboundedSender<Command>>,
    updates: &async_channel::Sender<Update>,
    command: Command,
) {
    let session = command.session();
    match command {
        Command::Connect {
            conn,
            password,
            root,
            ..
        } => {
            let (tx, rx) = mpsc::unbounded_channel();
            sessions.insert(session, tx);
            tokio::spawn(session_task(
                session,
                conn,
                password,
                root,
                rx,
                updates.clone(),
            ));
        }
        Command::Disconnect { .. } => {
            if let Some(tx) = sessions.remove(&session) {
                let _ = tx.send(command);
            }
        }
        _ => {
            let run = match &command {
                Command::Run { run, .. } => Some(*run),
                _ => None,
            };
            let introspect = matches!(command, Command::Introspect { .. });
            let transaction = matches!(command, Command::Transaction { .. });
            let apply = matches!(command, Command::Apply { .. });
            let delivered = sessions
                .get(&session)
                .is_some_and(|tx| tx.send(command).is_ok());
            if !delivered {
                sessions.remove(&session);
                if let Some(run) = run {
                    let _ = updates
                        .send(Update::RunError {
                            session,
                            run,
                            message: "not connected".into(),
                            position: None,
                        })
                        .await;
                }
                if transaction {
                    let error = Some("not connected".into());
                    let _ = updates
                        .send(Update::TxState {
                            session,
                            open: false,
                            error,
                        })
                        .await;
                }
                if apply {
                    let result = Err(ApplyError {
                        index: None,
                        message: "not connected".into(),
                    });
                    let _ = updates.send(Update::Applied { session, result }).await;
                }
                if introspect {
                    let catalog = Err("not connected".into());
                    let _ = updates.send(Update::Catalog { session, catalog }).await;
                }
            }
        }
    }
}

async fn session_task(
    session: SessionId,
    conn: Connection,
    password: Option<Secret>,
    root: Option<PathBuf>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    updates: async_channel::Sender<Update>,
) {
    let mut queued: VecDeque<Command> = VecDeque::new();
    let connected = {
        let connect = tokio::time::timeout(
            CONNECT_TIMEOUT,
            Driver::connect(&conn, password.as_ref(), root.as_deref()),
        );
        tokio::pin!(connect);
        loop {
            tokio::select! {
                biased;
                command = rx.recv() => match command {
                    None | Some(Command::Disconnect { .. }) => return,
                    Some(c @ (Command::Run { .. } | Command::Transaction { .. } | Command::Apply { .. } | Command::Cancel { .. } | Command::Introspect { .. })) => queued.push_back(c),
                    Some(_) => {}
                },
                result = &mut connect => break result,
            }
        }
    };
    drop(password);
    let driver = match connected {
        Ok(Ok((driver, server))) => {
            if updates
                .send(Update::Connected { session, server })
                .await
                .is_err()
            {
                return;
            }
            driver
        }
        Ok(Err(error)) => {
            let _ = updates.send(Update::Failed { session, error }).await;
            return;
        }
        Err(_) => {
            let error = "connection timed out after 10s".to_owned();
            let _ = updates.send(Update::Failed { session, error }).await;
            return;
        }
    };
    let mut tx_open = false;
    loop {
        let command = match queued.pop_front() {
            Some(c) => c,
            None => match rx.recv().await {
                Some(c) => c,
                None => return,
            },
        };
        match command {
            Command::Run {
                run,
                sql,
                page,
                timeout,
                mode,
                capped,
                ..
            } => {
                let early = queued
                    .iter()
                    .position(|c| matches!(c, Command::Cancel { run: r, .. } if *r == run));
                if let Some(i) = early {
                    queued.remove(i);
                    if updates.send(cancelled_done(session, run)).await.is_err() {
                        return;
                    }
                    continue;
                }
                let mut closing: &[&str] = &[];
                let mut begun = false;
                match mode {
                    RunMode::Auto => {}
                    RunMode::Transaction => {
                        if conn.read_only {
                            if !send_run_error(&updates, session, run, READ_ONLY_NO_TX.to_owned())
                                .await
                            {
                                return;
                            }
                            continue;
                        }
                        if !tx_open {
                            match begin_tx(&driver, conn.engine, &mut rx, &mut queued).await {
                                Began::Done => {
                                    tx_open = true;
                                    begun = true;
                                    if !send_tx(&updates, session, true, None).await {
                                        return;
                                    }
                                }
                                Began::Lost => {
                                    let _ = updates.send(lost_autocommit(session)).await;
                                    return;
                                }
                                Began::Failed(message) => {
                                    if !send_run_error(&updates, session, run, message).await {
                                        return;
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                    RunMode::ReadOnly => {
                        let (begin, end) = read_only_sql(conn.engine, conn.read_only);
                        let refusal = if tx_open {
                            Some("Commit or roll back the open transaction first.".to_owned())
                        } else if let Err(e) = exec_all(&driver, begin, &mut rx, &mut queued).await
                        {
                            // A half-applied begin must not leave the session read-only.
                            let _ = exec_all(&driver, end, &mut rx, &mut queued).await;
                            Some(e)
                        } else {
                            None
                        };
                        if let Some(message) = refusal {
                            if !send_run_error(&updates, session, run, message).await {
                                return;
                            }
                            continue;
                        }
                        closing = end;
                        begun = true;
                    }
                }
                if begun {
                    let cancelled = queued
                        .iter()
                        .position(|c| matches!(c, Command::Cancel { run: r, .. } if *r == run));
                    let disconnect = queued
                        .iter()
                        .any(|c| matches!(c, Command::Disconnect { .. }));
                    if cancelled.is_some() || disconnect {
                        if let Some(i) = cancelled {
                            queued.remove(i);
                        }
                        let closed = exec_all(&driver, closing, &mut rx, &mut queued).await;
                        if disconnect {
                            return;
                        }
                        if closed.is_err() {
                            let _ = updates.send(lost_read_only(session)).await;
                            return;
                        }
                        if updates.send(cancelled_done(session, run)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                }
                let job = Job {
                    session,
                    run,
                    sql,
                    page: page.max(1),
                    timeout,
                    capped,
                };
                if !run_job(&driver, &mut rx, &mut queued, &updates, job).await {
                    return;
                }
                if exec_all(&driver, closing, &mut rx, &mut queued)
                    .await
                    .is_err()
                {
                    let _ = updates.send(lost_read_only(session)).await;
                    return;
                }
                if mode == RunMode::Transaction
                    && tx_open
                    && driver.in_transaction().await == Some(false)
                {
                    tx_open = false;
                    let error = Some(
                        "The database ended the transaction, so changes since it began were rolled back."
                            .to_owned(),
                    );
                    if !send_tx(&updates, session, false, error).await {
                        return;
                    }
                }
            }
            Command::Transaction { op, .. } => {
                let (open, error) = match op {
                    TxOp::Begin if tx_open => (true, None),
                    TxOp::Begin if conn.read_only => (false, Some(READ_ONLY_NO_TX.to_owned())),
                    TxOp::Begin => {
                        match begin_tx(&driver, conn.engine, &mut rx, &mut queued).await {
                            Began::Done => {
                                tx_open = true;
                                (true, None)
                            }
                            Began::Lost => {
                                let _ = updates.send(lost_autocommit(session)).await;
                                return;
                            }
                            Began::Failed(e) => (false, Some(e)),
                        }
                    }
                    TxOp::Rollback => {
                        let mut error = None;
                        if tx_open {
                            match end_tx(&driver, conn.engine, op, &mut rx, &mut queued).await {
                                Ended::Done => {}
                                Ended::Failed(e) => error = Some(e),
                                Ended::Lost => {
                                    let _ = updates.send(lost_autocommit(session)).await;
                                    return;
                                }
                            }
                        }
                        tx_open = false;
                        (false, error)
                    }
                    TxOp::Commit if !tx_open => (false, None),
                    TxOp::Commit => {
                        let postgres = conn.engine == Engine::Postgres;
                        let probe_failed = postgres
                            && exec(&driver, "SELECT 1", &mut rx, &mut queued)
                                .await
                                .is_err();
                        if probe_failed {
                            let _ = exec(&driver, "ROLLBACK", &mut rx, &mut queued).await;
                            tx_open = false;
                            let msg = "The transaction had failed, so it was rolled back instead of committed.";
                            (false, Some(msg.to_owned()))
                        } else {
                            match end_tx(&driver, conn.engine, op, &mut rx, &mut queued).await {
                                Ended::Done => {
                                    tx_open = false;
                                    (false, None)
                                }
                                Ended::Failed(e) if postgres => {
                                    tx_open = false;
                                    (
                                        false,
                                        Some(format!(
                                            "Commit failed and the transaction was rolled back: {e}"
                                        )),
                                    )
                                }
                                Ended::Failed(e) => (true, Some(e)),
                                Ended::Lost => {
                                    let _ = updates.send(lost_autocommit(session)).await;
                                    return;
                                }
                            }
                        }
                    }
                };
                if !send_tx(&updates, session, open, error).await {
                    return;
                }
            }
            Command::Introspect { .. } => {
                if !introspect(&driver, &mut rx, &mut queued, &updates, session).await {
                    return;
                }
            }
            Command::Apply { statements, .. } => {
                let refusal = if conn.read_only {
                    Some("This connection is read-only.")
                } else if tx_open {
                    Some("Finish the open transaction first.")
                } else {
                    None
                };
                if let Some(message) = refusal {
                    let result = Err(ApplyError {
                        index: None,
                        message: message.into(),
                    });
                    if updates
                        .send(Update::Applied { session, result })
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if !apply(&driver, statements, &mut rx, &mut queued, &updates, session).await
                {
                    return;
                }
            }
            Command::Disconnect { .. } => return,
            _ => {}
        }
    }
}

const READ_ONLY_NO_TX: &str = "This connection is read-only, so it has no manual transactions.";

async fn send_tx(
    updates: &async_channel::Sender<Update>,
    session: SessionId,
    open: bool,
    error: Option<String>,
) -> bool {
    updates
        .send(Update::TxState {
            session,
            open,
            error,
        })
        .await
        .is_ok()
}

async fn send_run_error(
    updates: &async_channel::Sender<Update>,
    session: SessionId,
    run: RunId,
    message: String,
) -> bool {
    updates
        .send(Update::RunError {
            session,
            run,
            message,
            position: None,
        })
        .await
        .is_ok()
}

const MYSQL_AUTOCOMMIT_ON: &str = "SET autocommit = 1";

/// MySQL runs manual transactions with autocommit off so a statement after an engine-side rollback is not committed on its own.
fn tx_sql(engine: Engine, op: TxOp) -> &'static [&'static str] {
    match (op, engine) {
        (TxOp::Begin, Engine::Mysql) => &["SET autocommit = 0", "START TRANSACTION"],
        (TxOp::Begin, _) => &["BEGIN"],
        (TxOp::Commit, Engine::Mysql) => &["COMMIT", MYSQL_AUTOCOMMIT_ON],
        (TxOp::Commit, _) => &["COMMIT"],
        (TxOp::Rollback, Engine::Mysql) => &["ROLLBACK", MYSQL_AUTOCOMMIT_ON],
        (TxOp::Rollback, _) => &["ROLLBACK"],
    }
}

/// MySQL also flips the session setting because DDL implicitly commits and would end a read-only transaction.
/// A read-only connection already has its own session setting, which the close must not undo.
fn read_only_sql(
    engine: Engine,
    connection_read_only: bool,
) -> (&'static [&'static str], &'static [&'static str]) {
    match (engine, connection_read_only) {
        (Engine::Postgres, _) => (&["BEGIN READ ONLY"], &["ROLLBACK"]),
        (Engine::Mysql, false) => (
            &[
                "SET SESSION TRANSACTION READ ONLY",
                "START TRANSACTION READ ONLY",
            ],
            &["ROLLBACK", "SET SESSION TRANSACTION READ WRITE"],
        ),
        (Engine::Mysql, true) => (&["START TRANSACTION READ ONLY"], &["ROLLBACK"]),
        (Engine::Sqlite, false) => (&["PRAGMA query_only = ON"], &["PRAGMA query_only = OFF"]),
        (Engine::Sqlite, true) => (&[], &[]),
    }
}

fn lost_read_only(session: SessionId) -> Update {
    Update::Failed {
        session,
        error: "Lost the read-only transaction; connect again.".into(),
    }
}

fn lost_autocommit(session: SessionId) -> Update {
    Update::Failed {
        session,
        error: "Could not restore autocommit; connect again.".into(),
    }
}

/// Runs a short control statement to the end; commands that arrive meanwhile wait in `queued`.
async fn exec(
    driver: &Driver,
    sql: &str,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
) -> Result<(), String> {
    let mut stream = driver.query(sql.to_owned());
    let mut failed: Option<String> = None;
    // A closed channel follows a Disconnect; finish this statement and let the main loop end the session.
    let mut listening = true;
    loop {
        tokio::select! {
            biased;
            command = rx.recv(), if listening => match command {
                None => listening = false,
                Some(c) => queued.push_back(c),
            },
            item = stream.next() => match item {
                Some(Err(failure)) => {
                    failed.get_or_insert(failure.message);
                }
                Some(Ok(_)) => {}
                None => break,
            },
        }
    }
    failed.map_or(Ok(()), Err)
}

async fn exec_all(
    driver: &Driver,
    sqls: &[&str],
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
) -> Result<(), String> {
    for sql in sqls {
        exec(driver, sql, rx, queued).await?;
    }
    Ok(())
}

enum Began {
    Done,
    Failed(String),
    Lost,
}

async fn begin_tx(
    driver: &Driver,
    engine: Engine,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
) -> Began {
    match exec_all(driver, tx_sql(engine, TxOp::Begin), rx, queued).await {
        Ok(()) => Began::Done,
        Err(e) if engine == Engine::Mysql => {
            // A half-applied begin must not leave autocommit off.
            match exec(driver, MYSQL_AUTOCOMMIT_ON, rx, queued).await {
                Ok(()) => Began::Failed(e),
                Err(_) => Began::Lost,
            }
        }
        Err(e) => Began::Failed(e),
    }
}

enum Ended {
    Done,
    Failed(String),
    Lost,
}

/// Restoring autocommit commits an open MySQL transaction, so it only runs after a successful COMMIT or ROLLBACK; a failed MySQL ROLLBACK loses the session instead.
async fn end_tx(
    driver: &Driver,
    engine: Engine,
    op: TxOp,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
) -> Ended {
    let statements = tx_sql(engine, op);
    let first = exec(driver, statements[0], rx, queued).await;
    if let Err(e) = &first {
        return if op == TxOp::Rollback && statements.len() > 1 {
            Ended::Lost
        } else {
            Ended::Failed(e.clone())
        };
    }
    for sql in &statements[1..] {
        if exec(driver, sql, rx, queued).await.is_err() {
            return Ended::Lost;
        }
    }
    Ended::Done
}

/// Returns false when the session should end.
async fn introspect(
    driver: &Driver,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
    updates: &async_channel::Sender<Update>,
    session: SessionId,
) -> bool {
    let canceller = driver.canceller();
    // Bounded so a dead network cannot wedge the session; an idle backend just ignores the signal.
    let cancel = || async {
        let _ = tokio::time::timeout(CONNECT_TIMEOUT, canceller.cancel()).await;
    };
    let mut stream = driver.catalog();
    let deadline = Instant::now() + INTROSPECT_TIMEOUT;
    let mut timed_out = false;
    let mut rows = Vec::new();
    let mut failed: Option<String> = None;
    loop {
        let sleep = async {
            if timed_out {
                std::future::pending().await
            } else {
                tokio::time::sleep_until(deadline).await
            }
        };
        tokio::select! {
            biased;
            command = rx.recv() => match command {
                None | Some(Command::Disconnect { .. }) => {
                    drop(stream);
                    let canceller = canceller.clone();
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(CONNECT_TIMEOUT, canceller.cancel()).await;
                    });
                    return false;
                }
                Some(c) => queued.push_back(c),
            },
            () = sleep => {
                cancel().await;
                timed_out = true;
            }
            item = stream.next() => match item {
                Some(Ok(Item::Row(row))) => rows.push(row),
                Some(Ok(_)) => {}
                Some(Err(failure)) => {
                    failed.get_or_insert(failure.message);
                }
                None => break,
            },
        }
    }
    let rows = if timed_out {
        Err("catalog timed out after 30s".into())
    } else {
        failed.map_or(Ok(rows), Err)
    };
    let catalog = rows.and_then(catalog::from_rows);
    updates
        .send(Update::Catalog { session, catalog })
        .await
        .is_ok()
}

/// Returns false when the session should end.
async fn apply(
    driver: &Driver,
    statements: Vec<edit::Stmt>,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
    updates: &async_channel::Sender<Update>,
    session: SessionId,
) -> bool {
    let canceller = driver.canceller();
    let stop = Arc::new(AtomicBool::new(false));
    let work = driver.apply(statements, stop.clone());
    tokio::pin!(work);
    let deadline = Instant::now() + APPLY_TIMEOUT;
    let mut timed_out = false;
    let result = loop {
        let sleep = async {
            if timed_out {
                std::future::pending().await
            } else {
                tokio::time::sleep_until(deadline).await
            }
        };
        tokio::select! {
            biased;
            command = rx.recv() => match command {
                None | Some(Command::Disconnect { .. }) => {
                    stop.store(true, Ordering::SeqCst);
                    let _ = tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        futures::future::join(canceller.cancel(), &mut work),
                    )
                    .await;
                    return false;
                }
                Some(c) => queued.push_back(c),
            },
            () = sleep => {
                timed_out = true;
                stop.store(true, Ordering::SeqCst);
                let _ = tokio::time::timeout(CONNECT_TIMEOUT, canceller.cancel()).await;
            }
            result = &mut work => break result,
        }
    };
    updates
        .send(Update::Applied { session, result })
        .await
        .is_ok()
}

struct Job {
    session: SessionId,
    run: RunId,
    sql: String,
    page: usize,
    timeout: Option<Duration>,
    capped: bool,
}

fn timeout_message(timeout: Duration) -> String {
    let secs = timeout.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("timed out after {secs:.0}s")
    } else {
        format!("timed out after {secs:.1}s")
    }
}

fn cancelled_done(session: SessionId, run: RunId) -> Update {
    Update::Done {
        session,
        run,
        affected: None,
        elapsed: Duration::ZERO,
        truncated: true,
    }
}

enum Route {
    Cancel,
    Disconnect,
    Cancelled(RunId),
    Nothing,
}

/// Sorts a command that arrived mid-run; a Cancel for a queued run removes it so it never starts.
fn route(command: Command, run: RunId, queued: &mut VecDeque<Command>) -> Route {
    match command {
        Command::Cancel { run: r, .. } if r == run => Route::Cancel,
        Command::Cancel { run: r, .. } => {
            let at = queued
                .iter()
                .position(|c| matches!(c, Command::Run { run: q, .. } if *q == r));
            match at {
                Some(i) => {
                    queued.remove(i);
                    Route::Cancelled(r)
                }
                None => Route::Nothing,
            }
        }
        Command::Disconnect { .. } => Route::Disconnect,
        c @ (Command::Run { .. }
        | Command::Transaction { .. }
        | Command::Apply { .. }
        | Command::Introspect { .. }) => {
            queued.push_back(c);
            Route::Nothing
        }
        _ => Route::Nothing,
    }
}

#[derive(PartialEq)]
enum Stop {
    User,
    Timeout,
}

enum Resume {
    More(usize),
    Stop,
    Gone,
}

/// Waits for the consumer to ask for the next page; other commands are kept for later.
async fn wait_for_resume(
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
    updates: &async_channel::Sender<Update>,
    session: SessionId,
    run: RunId,
) -> Resume {
    loop {
        let Some(command) = rx.recv().await else {
            return Resume::Gone;
        };
        if let Command::FetchMore { run: r, rows, .. } = command {
            if r == run {
                return Resume::More(rows.max(1));
            }
            continue;
        }
        match route(command, run, queued) {
            Route::Cancel => return Resume::Stop,
            Route::Disconnect => return Resume::Gone,
            Route::Cancelled(r) => {
                if updates.send(cancelled_done(session, r)).await.is_err() {
                    return Resume::Gone;
                }
            }
            Route::Nothing => {}
        }
    }
}

/// Returns false when the session should end.
async fn run_job(
    driver: &Driver,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
    updates: &async_channel::Sender<Update>,
    job: Job,
) -> bool {
    let Job {
        session,
        run,
        sql,
        page,
        timeout,
        capped,
    } = job;
    let started = Instant::now();
    let canceller = driver.canceller();
    // Bounded so a dead network cannot wedge the session; an idle backend just ignores the signal.
    let cancel = || async {
        let _ = tokio::time::timeout(CONNECT_TIMEOUT, canceller.cancel()).await;
    };
    let cancel_detached = || {
        let canceller = canceller.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(CONNECT_TIMEOUT, canceller.cancel()).await;
        });
    };
    let mut deadline = timeout.map(|t| started + t);
    let mut stop: Option<Stop> = None;
    let mut first_item = true;
    let mut resend = tokio::time::interval(CANCEL_RESEND);
    resend.reset();
    let mut stream = driver.query(sql);
    let mut buffer: Vec<Vec<Cell>> = Vec::new();
    let mut limit = page;
    let mut budget = Budget::default();
    let mut has_columns = false;
    let mut statement_rows = false;
    let mut affected = None;
    let mut truncated = false;

    macro_rules! send {
        ($u:expr) => {
            if updates.send($u).await.is_err() {
                return false;
            }
        };
    }

    loop {
        match rx.try_recv() {
            Ok(command) => match route(command, run, queued) {
                Route::Cancel => {
                    send!(cancelled_done(session, run));
                    return true;
                }
                Route::Disconnect => return false,
                Route::Cancelled(r) => send!(cancelled_done(session, r)),
                Route::Nothing => {}
            },
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }

    loop {
        let sleep = async {
            match deadline {
                Some(d) if stop.is_none() => tokio::time::sleep_until(d).await,
                _ => std::future::pending().await,
            }
        };
        let waiting_on_cancel = first_item && stop == Some(Stop::User);
        let resend_tick = async {
            if waiting_on_cancel {
                resend.tick().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let item = tokio::select! {
            biased;
            command = rx.recv() => {
                let Some(command) = command else { return false };
                match route(command, run, queued) {
                    Route::Cancel => {
                        cancel().await;
                        stop.get_or_insert(Stop::User);
                    }
                    Route::Disconnect => {
                        cancel_detached();
                        return false;
                    }
                    Route::Cancelled(r) => send!(cancelled_done(session, r)),
                    Route::Nothing => {}
                }
                continue;
            }
            () = sleep => {
                cancel().await;
                stop = Some(Stop::Timeout);
                continue;
            }
            () = resend_tick => {
                // Statements without a RowDescription give no early item, so a cancel sent before the backend was busy is lost.
                cancel().await;
                continue;
            }
            item = stream.next() => item,
        };
        if std::mem::take(&mut first_item) && stop == Some(Stop::User) {
            // The cancel may have raced the query send and been discarded by the backend.
            cancel().await;
        }
        match item {
            None => break,
            Some(Err(failure)) => {
                if !buffer.is_empty() {
                    send!(Update::Rows {
                        session,
                        run,
                        rows: std::mem::take(&mut buffer),
                        more: false
                    });
                }
                let message = match (&stop, timeout) {
                    (Some(Stop::Timeout), Some(t)) => timeout_message(t),
                    _ => failure.message,
                };
                let position = if stop == Some(Stop::Timeout) {
                    None
                } else {
                    failure.position
                };
                send!(Update::RunError {
                    session,
                    run,
                    message,
                    position
                });
                return true;
            }
            Some(Ok(Item::Columns(columns))) => {
                if !buffer.is_empty() {
                    send!(Update::Rows {
                        session,
                        run,
                        rows: std::mem::take(&mut buffer),
                        more: false
                    });
                }
                has_columns = true;
                statement_rows = true;
                send!(Update::Columns {
                    session,
                    run,
                    columns
                });
            }
            Some(Ok(Item::Row(row))) => {
                if capped && !budget.admit(&row) {
                    truncated = true;
                    cancel().await;
                    break;
                }
                if buffer.len() >= limit {
                    if stop.is_some() {
                        truncated = true;
                        break;
                    }
                    send!(Update::Rows {
                        session,
                        run,
                        rows: std::mem::take(&mut buffer),
                        more: true
                    });
                    let paused = Instant::now();
                    match wait_for_resume(rx, queued, updates, session, run).await {
                        Resume::More(rows) => limit = rows,
                        Resume::Stop => {
                            cancel().await;
                            truncated = true;
                            break;
                        }
                        Resume::Gone => {
                            cancel_detached();
                            return false;
                        }
                    }
                    deadline = deadline.map(|d| d + paused.elapsed());
                }
                buffer.push(row);
            }
            Some(Ok(Item::Complete(n))) => {
                affected = if statement_rows { None } else { Some(n) };
                statement_rows = false;
            }
        }
    }
    drop(stream);
    if !buffer.is_empty() || has_columns {
        send!(Update::Rows {
            session,
            run,
            rows: buffer,
            more: false
        });
    }
    send!(Update::Done {
        session,
        run,
        affected,
        elapsed: started.elapsed(),
        truncated
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Engine, Tls};

    fn conn(engine: Engine, host: &str, port: u16) -> Connection {
        Connection {
            id: "t".into(),
            name: "t".into(),
            engine,
            host: host.into(),
            port: Some(port),
            database: "db".into(),
            user: "u".into(),
            tls: Tls::Disable,
            ca_file: None,
            read_only: false,
            production: false,
            color: None,
            claude: false,
            projects: Vec::new(),
            save_password: false,
        }
    }

    fn first_update(command: Command) -> Update {
        let (tx, rx) = async_channel::unbounded();
        let handle = start(tx);
        handle.send(command);
        rx.recv_blocking().unwrap()
    }

    #[test]
    fn failed_connect_never_echoes_the_password() {
        for host in ["127.0.0.1", "[::1]"] {
            let update = first_update(Command::Connect {
                session: SessionId(1),
                conn: conn(Engine::Postgres, host, 1),
                password: Some(Secret::new("s3cret")),
                root: None,
            });
            let Update::Failed { error, .. } = update else {
                panic!("expected a failure");
            };
            assert!(!error.is_empty());
            assert!(!error.contains("s3cret"));
        }
    }

    #[test]
    fn run_without_a_session_reports_not_connected() {
        let update = first_update(Command::Run {
            session: SessionId(9),
            run: RunId(1),
            sql: "select 1".into(),
            page: 500,
            timeout: None,
            mode: RunMode::Auto,
            capped: true,
        });
        assert!(
            matches!(update, Update::RunError { ref message, .. } if message == "not connected")
        );
    }

    #[test]
    fn transaction_without_a_session_reports_not_connected() {
        let update = first_update(Command::Transaction {
            session: SessionId(9),
            op: TxOp::Begin,
        });
        assert_eq!(
            update,
            Update::TxState {
                session: SessionId(9),
                open: false,
                error: Some("not connected".into()),
            }
        );
    }

    #[test]
    fn tx_sql_matches_each_engine() {
        assert_eq!(tx_sql(Engine::Postgres, TxOp::Begin), &["BEGIN"]);
        assert_eq!(tx_sql(Engine::Sqlite, TxOp::Begin), &["BEGIN"]);
        assert_eq!(
            tx_sql(Engine::Mysql, TxOp::Begin),
            &["SET autocommit = 0", "START TRANSACTION"]
        );
        for engine in [Engine::Postgres, Engine::Sqlite] {
            assert_eq!(tx_sql(engine, TxOp::Commit), &["COMMIT"]);
            assert_eq!(tx_sql(engine, TxOp::Rollback), &["ROLLBACK"]);
        }
        assert_eq!(
            tx_sql(Engine::Mysql, TxOp::Commit),
            &["COMMIT", "SET autocommit = 1"]
        );
        assert_eq!(
            tx_sql(Engine::Mysql, TxOp::Rollback),
            &["ROLLBACK", "SET autocommit = 1"]
        );
    }

    #[test]
    fn read_only_sql_matches_each_engine() {
        assert_eq!(
            read_only_sql(Engine::Postgres, false),
            (&["BEGIN READ ONLY"][..], &["ROLLBACK"][..])
        );
        assert_eq!(
            read_only_sql(Engine::Mysql, false),
            (
                &[
                    "SET SESSION TRANSACTION READ ONLY",
                    "START TRANSACTION READ ONLY"
                ][..],
                &["ROLLBACK", "SET SESSION TRANSACTION READ WRITE"][..]
            )
        );
        assert_eq!(
            read_only_sql(Engine::Sqlite, false),
            (
                &["PRAGMA query_only = ON"][..],
                &["PRAGMA query_only = OFF"][..]
            )
        );
    }

    #[test]
    fn read_only_sql_never_undoes_a_read_only_connection() {
        assert_eq!(
            read_only_sql(Engine::Mysql, true),
            (&["START TRANSACTION READ ONLY"][..], &["ROLLBACK"][..])
        );
        assert_eq!(read_only_sql(Engine::Sqlite, true), (&[][..], &[][..]));
    }

    #[test]
    fn introspect_without_a_session_reports_not_connected() {
        let update = first_update(Command::Introspect {
            session: SessionId(9),
        });
        assert!(
            matches!(update, Update::Catalog { catalog: Err(ref m), .. } if m == "not connected")
        );
    }
}
