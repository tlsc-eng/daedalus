use std::collections::VecDeque;

use daedalus_db::classify::Kind;
use daedalus_db::value::{Cell, Column};
use daedalus_db::worker::{Command, RunId, RunMode, SessionId, Update};
use daedalus_ui::GridColumn;

use super::gate::excerpt;
use super::text;

pub(super) const PAGE: usize = 500;

pub(super) struct Piece {
    pub sql: String,
    /// Byte offset of `sql` in the document, so an error position maps back to the editor.
    pub start: usize,
    #[allow(dead_code)]
    pub kind: Kind,
}

fn send(command: Command) -> Effect {
    Effect::Send(Box::new(command))
}

pub(super) enum Effect {
    Send(Box<Command>),
    Show {
        columns: Vec<GridColumn>,
        rows: Vec<Vec<Cell>>,
        more: bool,
    },
    Append {
        rows: Vec<Vec<Cell>>,
        more: bool,
    },
    Finish {
        truncated: bool,
    },
    Affected(Option<u64>),
    /// `at` is a byte offset in the document.
    Fail {
        message: String,
        at: Option<usize>,
    },
    Log {
        text: String,
        error: bool,
    },
    Footer(String),
    /// The batch is over or waiting on the user, so busy and stale clear.
    Idle,
}

struct Current {
    run: RunId,
    piece: Piece,
    columns: Option<Vec<Column>>,
    shown: bool,
    paused: bool,
    rows: usize,
}

/// Runs a list of statements one after another and turns worker updates into effects for the view.
#[derive(Default)]
pub(super) struct Batch {
    session: Option<SessionId>,
    mode: Option<RunMode>,
    next_run: u64,
    pending: VecDeque<Piece>,
    current: Option<Current>,
    /// The statement behind the grid on screen, which an export re-runs.
    shown: Option<String>,
}

pub(in crate::shell::db) fn grid_columns(columns: &[Column]) -> Vec<GridColumn> {
    columns
        .iter()
        .map(|c| GridColumn {
            name: c.name.clone().into(),
            type_label: c.type_name.clone().into(),
            numeric: text::numeric(&c.type_name),
        })
        .collect()
}

impl Batch {
    pub fn start(&mut self, session: SessionId, pieces: Vec<Piece>, mode: RunMode) -> Vec<Effect> {
        let mut out = Vec::new();
        if let Some(old) = self.current.take() {
            out.push(send(Command::Cancel {
                session,
                run: old.run,
            }));
        }
        self.session = Some(session);
        self.mode = Some(mode);
        self.pending = pieces.into();
        self.next(&mut out);
        out
    }

    fn next(&mut self, out: &mut Vec<Effect>) {
        let (Some(session), Some(mode), Some(piece)) =
            (self.session, self.mode, self.pending.pop_front())
        else {
            self.current = None;
            out.push(Effect::Idle);
            return;
        };
        let run = RunId(self.next_run);
        self.next_run += 1;
        out.push(send(Command::Run {
            session,
            run,
            sql: piece.sql.clone(),
            page: PAGE,
            timeout: None,
            mode,
            capped: true,
        }));
        self.current = Some(Current {
            run,
            piece,
            columns: None,
            shown: false,
            paused: false,
            rows: 0,
        });
    }

    pub fn on(&mut self, update: Update) -> Vec<Effect> {
        let mut out = Vec::new();
        let Some(session) = self.session else {
            return out;
        };
        let (run, current) = match (&update, self.current.as_mut()) {
            (
                Update::Columns { run, .. }
                | Update::Rows { run, .. }
                | Update::Done { run, .. }
                | Update::RunError { run, .. },
                Some(current),
            ) if *run == current.run => (*run, current),
            _ => return out,
        };
        match update {
            Update::Columns { columns, .. } => current.columns = Some(columns),
            Update::Rows { rows, more, .. } => {
                if !current.shown && rows.is_empty() && more {
                    out.push(send(Command::FetchMore {
                        session,
                        run,
                        rows: PAGE,
                    }));
                    return out;
                }
                current.rows += rows.len();
                match current.shown {
                    true => out.push(Effect::Append { rows, more }),
                    false => {
                        let columns = current.columns.as_deref().unwrap_or_default();
                        self.shown = Some(current.piece.sql.clone());
                        out.push(Effect::Show {
                            columns: grid_columns(columns),
                            rows,
                            more,
                        });
                        current.shown = true;
                    }
                }
                if more && !self.pending.is_empty() {
                    out.push(send(Command::Cancel { session, run }));
                } else if more {
                    current.paused = true;
                    out.push(Effect::Footer(format!("{} rows · more", current.rows)));
                    out.push(Effect::Idle);
                }
            }
            Update::Done {
                affected,
                elapsed,
                truncated,
                ..
            } => {
                let Some(done) = self.current.take() else {
                    return out;
                };
                let ms = elapsed.as_millis();
                let name = excerpt(&done.piece.sql, 80);
                match (&done.columns, done.shown) {
                    (Some(columns), false) => {
                        self.shown = Some(done.piece.sql.clone());
                        out.push(Effect::Show {
                            columns: grid_columns(columns),
                            rows: Vec::new(),
                            more: false,
                        })
                    }
                    (_, true) => out.push(Effect::Finish { truncated }),
                    (None, false) => out.push(Effect::Affected(affected)),
                }
                if done.columns.is_some() {
                    let cut = if truncated { " · truncated" } else { "" };
                    out.push(Effect::Log {
                        text: format!("{name} · {} rows · {ms} ms", done.rows),
                        error: false,
                    });
                    out.push(Effect::Footer(format!("{} rows · {ms} ms{cut}", done.rows)));
                } else {
                    let k = affected.unwrap_or(0);
                    out.push(Effect::Log {
                        text: format!("{name} · {k} rows affected · {ms} ms"),
                        error: false,
                    });
                    out.push(Effect::Footer(format!("{k} rows affected · {ms} ms")));
                }
                self.next(&mut out);
            }
            Update::RunError {
                message, position, ..
            } => {
                let Some(failed) = self.current.take() else {
                    return out;
                };
                let at =
                    position.map(|p| text::error_byte(&failed.piece.sql, failed.piece.start, p));
                out.push(Effect::Log {
                    text: format!("{} · {message}", excerpt(&failed.piece.sql, 80)),
                    error: true,
                });
                out.push(Effect::Fail { message, at });
                self.pending.clear();
                out.push(Effect::Idle);
            }
            _ => {}
        }
        out
    }

    /// Cancels the current run and drops what was queued behind it; the run's Done ends the batch.
    pub fn stop(&mut self) -> Vec<Effect> {
        self.pending.clear();
        let (Some(session), Some(current)) = (self.session, self.current.as_mut()) else {
            return Vec::new();
        };
        current.paused = false;
        vec![send(Command::Cancel {
            session,
            run: current.run,
        })]
    }

    pub fn fetch_more(&mut self) -> Vec<Effect> {
        let (Some(session), Some(current)) = (self.session, self.current.as_mut()) else {
            return Vec::new();
        };
        if !current.paused {
            return Vec::new();
        }
        current.paused = false;
        vec![send(Command::FetchMore {
            session,
            run: current.run,
            rows: PAGE,
        })]
    }

    pub fn running(&self) -> bool {
        self.current.as_ref().is_some_and(|c| !c.paused)
    }

    pub fn paused(&self) -> bool {
        self.current.as_ref().is_some_and(|c| c.paused)
    }

    pub fn idle(&self) -> bool {
        self.current.is_none()
    }

    pub fn shown_sql(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// Hands out a run id from the batch's own counter so an export never collides with a batch run.
    pub fn take_run_id(&mut self) -> RunId {
        let run = RunId(self.next_run);
        self.next_run += 1;
        run
    }

    pub fn reset(&mut self) {
        self.session = None;
        self.pending.clear();
        self.current = None;
        self.shown = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const S: SessionId = SessionId(1);

    fn piece(sql: &str, start: usize) -> Piece {
        Piece {
            sql: sql.into(),
            start,
            kind: Kind::Read,
        }
    }

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.into(),
            type_name: ty.into(),
            table: None,
        }
    }

    fn columns(run: u64) -> Update {
        Update::Columns {
            session: S,
            run: RunId(run),
            columns: vec![col("n", "int4")],
        }
    }

    fn rows(run: u64, n: usize, more: bool) -> Update {
        Update::Rows {
            session: S,
            run: RunId(run),
            rows: vec![vec![Some("1".into())]; n],
            more,
        }
    }

    fn done(run: u64, affected: Option<u64>, truncated: bool) -> Update {
        Update::Done {
            session: S,
            run: RunId(run),
            affected,
            elapsed: Duration::from_millis(7),
            truncated,
        }
    }

    fn sent(out: &[Effect]) -> Vec<&Command> {
        out.iter()
            .filter_map(|e| match e {
                Effect::Send(c) => Some(&**c),
                _ => None,
            })
            .collect()
    }

    fn started(pieces: Vec<Piece>) -> Batch {
        let mut b = Batch::default();
        let out = b.start(S, pieces, RunMode::Auto);
        assert!(matches!(sent(&out)[..], [Command::Run { .. }]));
        b
    }

    #[test]
    fn rows_wait_for_columns_then_show() {
        let mut b = started(vec![piece("select 1", 0)]);
        assert!(b.on(columns(0)).is_empty());
        let out = b.on(rows(0, 1, false));
        match &out[..] {
            [
                Effect::Show {
                    columns,
                    rows,
                    more,
                },
            ] => {
                assert_eq!(columns[0].name.as_ref(), "n");
                assert!(columns[0].numeric);
                assert_eq!(rows.len(), 1);
                assert!(!more);
            }
            _ => panic!("expected one Show"),
        }
    }

    #[test]
    fn empty_first_page_with_more_asks_for_more_instead_of_showing() {
        let mut b = started(vec![piece("select 1", 0)]);
        b.on(columns(0));
        let out = b.on(rows(0, 0, true));
        assert!(matches!(
            sent(&out)[..],
            [Command::FetchMore { rows: PAGE, .. }]
        ));
        assert_eq!(out.len(), 1);
        assert!(b.running());
        assert!(matches!(b.on(rows(0, 1, false))[..], [Effect::Show { .. }]));
    }

    #[test]
    fn a_paused_result_mid_batch_is_cancelled_and_the_batch_moves_on() {
        let mut b = started(vec![piece("select 1", 0), piece("select 2", 10)]);
        b.on(columns(0));
        let out = b.on(rows(0, 1, true));
        assert!(matches!(out[0], Effect::Show { .. }));
        assert!(matches!(sent(&out)[..], [Command::Cancel { .. }]));
        assert_eq!(out.len(), 2);
        assert!(b.running());
        let out = b.on(done(0, None, true));
        assert!(
            out.iter()
                .any(|e| matches!(e, Effect::Finish { truncated: true }))
        );
        assert!(matches!(
            sent(&out)[..],
            [Command::Run { run: RunId(1), .. }]
        ));
    }

    #[test]
    fn start_cancels_a_paused_result_before_the_new_run() {
        let mut b = started(vec![piece("select 1", 0)]);
        b.on(columns(0));
        b.on(rows(0, 1, true));
        assert!(!b.running());
        let out = b.start(S, vec![piece("select 2", 0)], RunMode::Auto);
        assert!(matches!(
            sent(&out)[..],
            [
                Command::Cancel { run: RunId(0), .. },
                Command::Run { run: RunId(1), .. }
            ]
        ));
    }

    #[test]
    fn an_error_stops_the_batch_and_maps_the_position() {
        let mut b = started(vec![piece("select é from x", 10), piece("select 2", 30)]);
        let out = b.on(Update::RunError {
            session: S,
            run: RunId(0),
            message: "boom".into(),
            position: Some(8),
        });
        let at = out.iter().find_map(|e| match e {
            Effect::Fail { at, .. } => *at,
            _ => None,
        });
        assert_eq!(at, Some(10 + "select ".len()));
        assert!(matches!(out.last(), Some(Effect::Idle)));
        assert!(!out.iter().any(|e| matches!(e, Effect::Send(_))));
        assert!(!b.running());
    }

    #[test]
    fn updates_for_other_runs_are_ignored() {
        let mut b = started(vec![piece("select 1", 0)]);
        assert!(b.on(rows(9, 1, false)).is_empty());
        assert!(b.on(done(9, None, false)).is_empty());
        assert!(b.running());
    }

    #[test]
    fn done_without_rows_shows_headers() {
        let mut b = started(vec![piece("select 1 where false", 0)]);
        b.on(columns(0));
        let out = b.on(done(0, None, false));
        assert!(matches!(
            &out[0],
            Effect::Show { rows, more: false, .. } if rows.is_empty()
        ));
        assert!(matches!(out.last(), Some(Effect::Idle)));
    }

    #[test]
    fn dml_reports_affected_rows() {
        let mut b = started(vec![piece("update t set a = 1", 0)]);
        let out = b.on(done(0, Some(3), false));
        assert!(matches!(out[0], Effect::Affected(Some(3))));
        assert!(out.iter().any(|e| matches!(
            e,
            Effect::Footer(f) if f == "3 rows affected · 7 ms"
        )));
    }

    #[test]
    fn stop_cancels_the_current_run_and_drops_pending() {
        let mut b = started(vec![piece("select 1", 0), piece("select 2", 10)]);
        assert!(matches!(
            sent(&b.stop())[..],
            [Command::Cancel { run: RunId(0), .. }]
        ));
        let out = b.on(done(0, None, true));
        assert!(!out.iter().any(|e| matches!(e, Effect::Send(_))));
        assert!(matches!(out.last(), Some(Effect::Idle)));
        assert!(!b.running());
    }

    #[test]
    fn fetch_more_only_when_paused() {
        let mut b = started(vec![piece("select 1", 0)]);
        b.on(columns(0));
        assert!(b.fetch_more().is_empty());
        let out = b.on(rows(0, 1, true));
        assert!(matches!(out.last(), Some(Effect::Idle)));
        assert!(!b.running());
        assert!(matches!(
            sent(&b.fetch_more())[..],
            [Command::FetchMore { run: RunId(0), .. }]
        ));
        assert!(b.running());
        assert!(b.fetch_more().is_empty());
    }

    #[test]
    fn stop_on_a_paused_batch_cancels_the_current_run() {
        let mut b = started(vec![piece("select 1", 0)]);
        b.on(columns(0));
        b.on(rows(0, 1, true));
        assert!(b.paused());
        assert!(matches!(
            sent(&b.stop())[..],
            [Command::Cancel { run: RunId(0), .. }]
        ));
    }

    #[test]
    fn a_paused_batch_is_not_idle() {
        let mut b = started(vec![piece("select 1", 0)]);
        assert!(!b.idle());
        b.on(columns(0));
        b.on(rows(0, 1, true));
        assert!(b.paused());
        assert!(!b.running());
        assert!(!b.idle());
    }

    #[test]
    fn shown_sql_is_the_piece_that_showed_rows() {
        let mut b = Batch::default();
        b.start(
            S,
            vec![piece("select 1", 0), piece("select 2", 9)],
            RunMode::Auto,
        );
        assert_eq!(b.shown_sql(), None);
        b.on(columns(0));
        b.on(rows(0, 1, false));
        assert_eq!(b.shown_sql(), Some("select 1"));
        b.on(done(0, None, false));
        b.on(columns(1));
        b.on(done(1, None, false));
        assert_eq!(b.shown_sql(), Some("select 2"));
    }

    #[test]
    fn take_run_id_does_not_collide_with_batch_runs() {
        let mut b = Batch::default();
        let first = b.take_run_id();
        b.start(S, vec![piece("select 1", 0)], RunMode::Auto);
        let batch_run = b.current.as_ref().map(|c| c.run);
        assert_ne!(Some(first), batch_run);
        assert_ne!(b.take_run_id(), first);
        assert_ne!(Some(b.take_run_id()), batch_run);
    }

    #[test]
    fn reset_clears_shown_sql() {
        let mut b = Batch::default();
        b.start(S, vec![piece("select 1", 0)], RunMode::Auto);
        b.on(columns(0));
        b.on(rows(0, 1, false));
        b.reset();
        assert_eq!(b.shown_sql(), None);
    }
}
