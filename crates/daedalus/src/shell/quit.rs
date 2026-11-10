use std::path::{Path, PathBuf};

use daedalus_editor::EditorView;
use daedalus_workspace::ItemId;
use gpui::{App, Context, Entity, PromptLevel, Window};

use super::Shell;
use super::item::ItemView;

impl Shell {
    /// Editors with unsaved changes, in every project or only the one at `root`.
    pub(super) fn dirty_editors(
        &self,
        root: Option<&Path>,
        cx: &Context<Self>,
    ) -> Vec<Entity<EditorView>> {
        // Tabs on the same file share one buffer; list and save it once.
        let mut paths = std::collections::HashSet::new();
        self.items
            .iter()
            .filter(|((r, _), _)| root.is_none_or(|root| r == root))
            .filter_map(|(_, v)| match v {
                ItemView::Editor(e)
                    if e.read(cx).is_dirty()
                        && paths.insert(super::lsp::document_key(e.read(cx).path())) =>
                {
                    Some(e.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Quits, first asking each window what to do with its unsaved files. Shells keep running in
    /// the daemon.
    pub(super) fn quit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.defer(super::windows::quit);
    }

    /// Settles this window's unsaved files for quitting, then runs `then` unless cancelled.
    pub(super) fn settle_for_quit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut gpui::App) + 'static,
    ) {
        let dirty = self.dirty_editors(None, cx);
        if !dirty.is_empty() {
            window.activate_window();
        }
        self.settle_unsaved(dirty, "quitting", window, cx, |this, cx| {
            this.save_now(cx);
            cx.defer(then);
        });
    }

    /// A quit that skipped the Save question (Dock, logout, `osascript`) saves what auto save would
    /// and keeps recovery copies of the rest, so nothing unsaved is dropped.
    pub(super) fn flush_unsaved(&mut self, cx: &mut Context<Self>) {
        if self.quit_settled {
            return;
        }
        if self.autosave_delay().is_some() {
            for editor in self.dirty_editors(None, cx) {
                editor.update(cx, |e, cx| e.save(cx));
            }
        }
        let Ok(dir) = daedalus_proto::recovery_dir() else {
            return;
        };
        match daedalus_editor::recovery::write_dirty(&dir) {
            Ok(0) => {}
            Ok(n) => tracing::info!("kept copies of {n} unsaved files in {}", dir.display()),
            Err(err) => tracing::error!("could not keep copies of unsaved files: {err}"),
        }
    }

    /// Closes the active project once its unsaved files are settled: auto save saves them quietly,
    /// as closing a tab does; otherwise the user is asked, as on quit.
    pub(super) fn close_active_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let ids: Vec<ItemId> = self
            .workspace
            .active
            .and_then(|i| self.workspace.projects.get(i))
            .map(|p| p.items().map(|i| i.id).collect())
            .unwrap_or_default();
        let programs = self.running_programs(&root, ids, cx);
        self.confirm_end_programs(programs, window, cx, move |this, window, cx| {
            this.close_project_at(root, window, cx)
        });
    }

    /// Terminals of the project at `root` whose shell runs a program, by display name.
    pub(super) fn running_programs(
        &self,
        root: &Path,
        items: impl IntoIterator<Item = ItemId>,
        cx: &App,
    ) -> Vec<String> {
        items
            .into_iter()
            .filter_map(|id| match self.items.get(&(root.to_path_buf(), id)) {
                Some(ItemView::Terminal(t)) => t.read(cx).running_program(),
                _ => None,
            })
            .collect()
    }

    /// Runs `then` at once when no program runs, else after the user confirms ending them.
    pub(super) fn confirm_end_programs(
        &mut self,
        programs: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if programs.is_empty() {
            return then(self, window, cx);
        }
        let (message, detail) = end_programs_text(&programs);
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &super::lsp::confirm_buttons("Cancel", "Close"),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(super::lsp::CONFIRMED) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| then(this, window, cx));
        })
        .detach();
    }

    fn close_project_at(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let mut dirty = self.dirty_editors(Some(&root), cx);
        if self.autosave_delay().is_some() {
            dirty.retain(|e| !e.update(cx, |e, cx| e.save(cx)));
        }
        self.settle_unsaved(dirty, "closing the project", window, cx, move |this, cx| {
            this.remove_project(&root, cx)
        });
    }

    /// Runs `then` at once if nothing is unsaved, else after Save (when every save worked) or
    /// Don't Save; `before` ends the question "Save changes to … before …?".
    pub(super) fn settle_unsaved(
        &mut self,
        dirty: Vec<Entity<EditorView>>,
        before: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if dirty.is_empty() {
            return then(self, cx);
        }
        let names: Vec<String> = dirty
            .iter()
            .filter_map(|e| {
                e.read(cx)
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        let message = match names.as_slice() {
            [one] => format!("Save changes to {one} before {before}?"),
            _ => format!("Save changes to {} files before {before}?", names.len()),
        };
        let detail = names.join(", ");
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = this.update(cx, |this, cx| {
                let go = match choice {
                    0 => dirty.iter().all(|e| e.update(cx, |e, cx| e.save(cx))),
                    1 => true,
                    _ => false,
                };
                if go {
                    then(this, cx);
                }
            });
        })
        .detach();
    }

    /// Drops the project at `root`, which may no longer be the active one once a prompt is answered.
    pub(super) fn remove_project(&mut self, root: &Path, cx: &mut Context<Self>) {
        let Some(index) = self.workspace.projects.iter().position(|p| p.root == root) else {
            return;
        };
        let was_active = self.workspace.active == Some(index);
        self.drop_project_items(root, cx);
        self.history.forget_root(root);
        self.lsp_project_closed(root);
        self.git_project_closed(root);
        self.debug_project_closed(root, cx);
        self.workspace.close_project(index);
        self.rail_from = self.workspace.active.unwrap_or(0);
        if was_active {
            self.zoomed = None;
            self.focus_pending = true;
            self.switch_count += 1;
        }
        self.schedule_save(cx);
        cx.notify();
    }
}

fn end_programs_text(programs: &[String]) -> (String, String) {
    match programs {
        [one] => (
            format!("Close the terminal running {one}?"),
            format!("Closing it ends {one}."),
        ),
        _ => (
            format!("Close {} terminals with programs running?", programs.len()),
            format!("Closing them ends {}.", programs.join(", ")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_program_names_it() {
        let (message, detail) = end_programs_text(&["vim".into()]);
        assert_eq!(message, "Close the terminal running vim?");
        assert_eq!(detail, "Closing it ends vim.");
    }

    #[test]
    fn several_programs_count_and_list_them() {
        let (message, detail) = end_programs_text(&["vim".into(), "Claude".into()]);
        assert_eq!(message, "Close 2 terminals with programs running?");
        assert_eq!(detail, "Closing them ends vim, Claude.");
    }
}
