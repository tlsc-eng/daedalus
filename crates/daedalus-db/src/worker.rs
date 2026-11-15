use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::Connection;
use crate::engine::{Driver, Item};
use crate::secret::Secret;
use crate::value::{Budget, Cell, Column};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CANCEL_RESEND: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub u64);

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
}

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
            | Command::FetchMore { session, .. }
            | Command::Cancel { session, .. } => *session,
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
        Command::Connect { conn, password, .. } => {
            let (tx, rx) = mpsc::unbounded_channel();
            sessions.insert(session, tx);
            tokio::spawn(session_task(session, conn, password, rx, updates.clone()));
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
            }
        }
    }
}

async fn session_task(
    session: SessionId,
    conn: Connection,
    password: Option<Secret>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    updates: async_channel::Sender<Update>,
) {
    let mut queued: VecDeque<Command> = VecDeque::new();
    let connected = {
        let connect =
            tokio::time::timeout(CONNECT_TIMEOUT, Driver::connect(&conn, password.as_ref()));
        tokio::pin!(connect);
        loop {
            tokio::select! {
                biased;
                command = rx.recv() => match command {
                    None | Some(Command::Disconnect { .. }) => return,
                    Some(c @ (Command::Run { .. } | Command::Cancel { .. })) => queued.push_back(c),
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
                let job = Job {
                    session,
                    run,
                    sql,
                    page: page.max(1),
                    timeout,
                };
                if !run_job(&driver, &mut rx, &mut queued, &updates, job).await {
                    return;
                }
            }
            Command::Disconnect { .. } => return,
            _ => {}
        }
    }
}

struct Job {
    session: SessionId,
    run: RunId,
    sql: String,
    page: usize,
    timeout: Option<Duration>,
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
        c @ Command::Run { .. } => {
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
                if !budget.admit(&row) {
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
    fn other_engines_are_not_supported_yet() {
        let update = first_update(Command::Connect {
            session: SessionId(1),
            conn: conn(Engine::Sqlite, "", 0),
            password: None,
            root: None,
        });
        assert_eq!(
            update,
            Update::Failed {
                session: SessionId(1),
                error: "engine not supported yet".into()
            }
        );
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
        });
        assert!(
            matches!(update, Update::RunError { ref message, .. } if message == "not connected")
        );
    }
}
