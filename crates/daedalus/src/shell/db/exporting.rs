use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use daedalus_db::Engine;
use daedalus_db::export::{self, FileExport, Format};
use daedalus_db::value::{Cell, Column};
use daedalus_db::worker::{Command, Handle, RunId, RunMode, SessionId, Update};
use daedalus_ui::motion::Busy;
use daedalus_ui::{DataGrid, GridColumn, MenuItem};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, ClipboardItem, Context, Entity, Task, WeakEntity, Window};

pub(in crate::shell::db) const FORMATS: [Format; 4] =
    [Format::Csv, Format::Tsv, Format::Json, Format::Insert];

const EXPORT_PAGE: usize = 2000;

pub(in crate::shell::db) struct Spec {
    pub dest: PathBuf,
    pub format: Format,
    pub engine: Engine,
    /// The already-quoted INSERT target; `None` means `result`.
    pub table: Option<String>,
}

pub(in crate::shell::db) enum Outcome {
    Done { rows: u64, name: String },
    Stopped,
    Failed(String),
}

/// A running export; dropping it cancels the run and deletes the unfinished temp file.
pub(in crate::shell::db) struct Export {
    run: RunId,
    session: SessionId,
    handle: Handle,
    rows: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    busy: Busy,
    tx: UnboundedSender<Update>,
    _task: Task<()>,
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into())
}

/// Cancelling makes the database fail the statement, which is the user's stop and not a failure.
fn run_error(message: String, stopping: bool) -> Outcome {
    match stopping {
        true => Outcome::Stopped,
        false => Outcome::Failed(message),
    }
}

fn to_columns(columns: &[GridColumn]) -> Vec<Column> {
    columns
        .iter()
        .map(|c| Column {
            name: c.name.to_string(),
            type_name: c.type_label.to_string(),
            table: None,
        })
        .collect()
}

/// Replaces anything outside `[A-Za-z0-9._-]` so a table name is a safe default file name.
pub(in crate::shell::db) fn file_stem(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                true => c,
                false => '_',
            },
        )
        .collect();
    match safe.trim_start_matches('.') {
        "" => "result".into(),
        stem => stem.into(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::shell::db) fn start<V: 'static>(
    cx: &mut Context<V>,
    handle: Handle,
    session: SessionId,
    run: RunId,
    sql: String,
    spec: Spec,
    on_end: fn(&mut V, Outcome, &mut Context<V>),
) -> Export {
    // The only place that lifts the result cap; each page is written out before the next is asked for.
    handle.send(Command::Run {
        session,
        run,
        sql,
        page: EXPORT_PAGE,
        timeout: None,
        mode: RunMode::ReadOnly,
        capped: false,
    });
    let (tx, mut rx) = unbounded::<Update>();
    let rows = Arc::new(AtomicU64::new(0));
    let counter = rows.clone();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop_seen = stopping.clone();
    let worker = handle.clone();
    let task = cx.spawn(async move |this, cx| {
        let Spec {
            dest,
            format,
            engine,
            table,
        } = spec;
        let name = file_name(&dest);
        let cancel = || worker.send(Command::Cancel { session, run });
        let failed = |e: std::io::Error| {
            cancel();
            Outcome::Failed(format!("Could not write {name}: {e}"))
        };
        let mut file: Option<FileExport> = None;
        let outcome = loop {
            let Some(update) = rx.next().await else {
                break Outcome::Stopped;
            };
            match update {
                Update::Columns { columns, .. } => {
                    let (dest, table) = (dest.clone(), table.clone());
                    let made =
                        cx.background_executor()
                            .spawn(async move {
                                FileExport::create(&dest, format, engine, table, &columns)
                            })
                            .await;
                    match made {
                        Ok(f) => file = Some(f),
                        Err(e) => break failed(e),
                    }
                }
                Update::Rows {
                    rows: page, more, ..
                } => {
                    let Some(mut f) = file.take() else {
                        continue;
                    };
                    let (f, written) = cx
                        .background_executor()
                        .spawn(async move {
                            let written = f.write(&page);
                            (f, written)
                        })
                        .await;
                    if let Err(e) = written {
                        break failed(e);
                    }
                    counter.store(f.rows(), Ordering::Relaxed);
                    file = Some(f);
                    if more {
                        worker.send(Command::FetchMore {
                            session,
                            run,
                            rows: EXPORT_PAGE,
                        });
                    }
                    this.update(cx, |_, cx| cx.notify()).ok();
                }
                Update::Done {
                    truncated: true, ..
                } => break Outcome::Stopped,
                Update::Done { .. } => {
                    let Some(f) = file.take() else {
                        break Outcome::Failed("The query returned no result to export.".into());
                    };
                    let committed = cx.background_executor().spawn(async move { f.commit() });
                    break match committed.await {
                        Ok(rows) => Outcome::Done {
                            rows,
                            name: name.clone(),
                        },
                        Err(e) => failed(e),
                    };
                }
                Update::RunError { message, .. } => {
                    break run_error(message, stop_seen.load(Ordering::Relaxed));
                }
                _ => {}
            }
        };
        drop(file);
        this.update(cx, |v, cx| on_end(v, outcome, cx)).ok();
    });
    Export {
        run,
        session,
        handle,
        rows,
        stopping,
        busy: Busy::start(),
        tx,
        _task: task,
    }
}

impl Export {
    /// Takes the updates of this export's run and hands the rest back.
    pub fn take(&self, update: Update) -> Option<Update> {
        let mine = match &update {
            Update::Columns { run, .. }
            | Update::Rows { run, .. }
            | Update::Done { run, .. }
            | Update::RunError { run, .. } => *run == self.run,
            _ => false,
        };
        if !mine {
            return Some(update);
        }
        let _ = self.tx.unbounded_send(update);
        None
    }

    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    pub fn busy(&self) -> Busy {
        self.busy
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.cancel();
    }

    fn cancel(&self) {
        self.handle.send(Command::Cancel {
            session: self.session,
            run: self.run,
        });
    }
}

impl Drop for Export {
    /// The worker ignores a run it no longer knows, so this is harmless after the run ended.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Writes rows already in memory, for when re-running the query is not safe.
pub(in crate::shell::db) fn write_loaded<V: 'static>(
    cx: &mut Context<V>,
    spec: Spec,
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    on_end: fn(&mut V, Outcome, &mut Context<V>),
) {
    cx.spawn(async move |this, cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move {
                let name = file_name(&spec.dest);
                let write = || -> std::io::Result<u64> {
                    let mut file = FileExport::create(
                        &spec.dest,
                        spec.format,
                        spec.engine,
                        spec.table.clone(),
                        &columns,
                    )?;
                    file.write(&rows)?;
                    file.commit()
                };
                match write() {
                    Ok(rows) => Outcome::Done { rows, name },
                    Err(e) => Outcome::Failed(format!("Could not write {name}: {e}")),
                }
            })
            .await;
        this.update(cx, |v, cx| on_end(v, outcome, cx)).ok();
    })
    .detach();
}

pub(in crate::shell::db) fn copy_as(
    grid: &Entity<DataGrid>,
    format: Format,
    engine: Engine,
    table: Option<String>,
    cx: &mut App,
) {
    let (columns, rows) = grid.read(cx).export_cells();
    let text = export::to_string(format, engine, table, &to_columns(&columns), &rows);
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

pub(in crate::shell::db) fn loaded(grid: &DataGrid) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let (columns, rows) = grid.loaded_cells();
    (to_columns(&columns), rows)
}

pub(in crate::shell::db) fn ask_path<V: 'static>(
    cx: &mut Context<V>,
    window: &mut Window,
    suggested: String,
    then: impl FnOnce(&mut V, PathBuf, &mut Window, &mut Context<V>) + 'static,
) {
    let home = std::env::home_dir().unwrap_or_default();
    let downloads = home.join("Downloads");
    let dir = match downloads.is_dir() {
        true => downloads,
        false => home,
    };
    let picked = cx.prompt_for_new_path(&dir, Some(&suggested));
    cx.spawn_in(window, async move |this, cx| {
        let Ok(Ok(Some(path))) = picked.await else {
            return;
        };
        let _ = this.update_in(cx, |v, window, cx| then(v, path, window, cx));
    })
    .detach();
}

type Pick<V> = fn(&mut V, Format, &mut Window, &mut Context<V>);

pub(in crate::shell::db) fn menu_block<V: 'static>(
    this: WeakEntity<V>,
    copy: Pick<V>,
    export: Pick<V>,
    export_enabled: bool,
    note: Option<&'static str>,
) -> Vec<MenuItem> {
    let item = |label: String, enabled: bool, run: Pick<V>, format: Format| {
        let this = this.clone();
        MenuItem::new(label, move |window, cx| {
            this.update(cx, |v, cx| run(v, format, window, cx)).ok();
        })
        .disabled(!enabled)
    };
    let mut items: Vec<MenuItem> = FORMATS
        .into_iter()
        .map(|f| item(format!("Copy as {}", f.label()), true, copy, f))
        .collect();
    items.push(MenuItem::separator());
    items.extend(
        FORMATS
            .into_iter()
            .map(|f| item(format!("Export {}…", f.label()), export_enabled, export, f)),
    );
    items.extend(note.map(|n| MenuItem::new(n, |_, _| {}).disabled(true)));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::db::console::grid_columns;

    #[test]
    fn file_stem_replaces_unsafe_chars() {
        assert_eq!(file_stem("a/b c"), "a_b_c");
        assert_eq!(file_stem("..x"), "x");
        assert_eq!(file_stem(""), "result");
        assert_eq!(file_stem("..."), "result");
        assert_eq!(file_stem("users-1.v2"), "users-1.v2");
    }

    #[test]
    fn a_run_error_after_stop_is_a_stop() {
        let stopped = run_error("canceling statement due to user request".into(), true);
        assert!(matches!(stopped, Outcome::Stopped));
        let failed = run_error("boom".into(), false);
        assert!(matches!(failed, Outcome::Failed(m) if m == "boom"));
    }

    #[test]
    fn grid_columns_round_trip_to_columns() {
        let columns = vec![Column {
            name: "id".into(),
            type_name: "int4".into(),
            table: None,
        }];
        let back = to_columns(&grid_columns(&columns));
        assert_eq!(back[0].name, "id");
        assert_eq!(back[0].type_name, "int4");
        assert!(back[0].table.is_none());
    }
}
