use std::collections::HashSet;
use std::path::{Path, PathBuf};

use daedalus_db::detect::{self, Source, Suggestion};
use daedalus_db::{Connection, Engine, Listed};
use daedalus_ui::{ActiveTheme, Ellipsis, Space};
use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, MouseButton, MouseDownEvent, Window, div,
    prelude::*, px,
};

use super::explorer::engine_label;
use super::sheet::{Found, SheetMode, caption_field};
use crate::shell::Shell;
use crate::shell::menus::shell_item;
use crate::shell::tree::ROW_HEIGHT;

/// Suggestions read for one project root; holds found passwords, so no Debug.
pub(super) struct Detected {
    pub(super) root: PathBuf,
    pub(super) found: Vec<Suggestion>,
}

/// One explorer row, already sanitised for display.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DetectedRow {
    pub(super) key: String,
    pub(super) name: String,
    pub(super) engine: Engine,
    pub(super) source: String,
    pub(super) tip: String,
}

/// Suggestions whose target no listed row already reaches; SSH rows reach another machine, so they hide nothing.
fn visible<'a>(
    d: &'a Detected,
    listed: &[Listed],
    hidden: &HashSet<(PathBuf, String)>,
) -> Vec<&'a Suggestion> {
    let saved: Vec<String> = listed
        .iter()
        .filter(|l| l.conn.ssh.is_none())
        .map(|l| detect::target_key(&l.conn, &d.root))
        .collect();
    d.found
        .iter()
        .filter(|s| !saved.contains(&s.key))
        .filter(|s| !hidden.contains(&(d.root.clone(), s.key.clone())))
        .collect()
}

fn source_label(s: &Source) -> String {
    match s {
        Source::Env { file, key } => format!("{file} · {}", caption_field(key)),
        Source::Container { name } => format!("docker · {}", caption_field(name)),
    }
}

fn password_note(s: &Source) -> String {
    match s {
        Source::Env { file, key } if key == "DATABASE_URL" || key.ends_with("_URL") => {
            format!("A password was found in {file}")
        }
        _ => "A password was found in this project's .env files".into(),
    }
}

fn row(s: &Suggestion) -> DetectedRow {
    let shown = Connection {
        user: caption_field(&s.conn.user),
        host: caption_field(&s.conn.host),
        database: caption_field(&s.conn.database),
        ..s.conn.clone()
    };
    let source = source_label(&s.source);
    DetectedRow {
        key: s.key.clone(),
        name: caption_field(&s.conn.name),
        engine: s.conn.engine,
        tip: format!("{} · from {source}", daedalus_db::redacted_url(&shown)),
        source,
    }
}

impl Shell {
    pub(super) fn db_ensure_detected(&mut self, cx: &mut Context<Self>) {
        let root = self.active_root();
        if self
            .db
            .detected
            .as_ref()
            .is_some_and(|d| Some(&d.root) != root.as_ref())
        {
            self.db.detected = None;
        }
        let Some(root) = root else {
            return;
        };
        let loading = self
            .db
            .detect_task
            .as_ref()
            .is_some_and(|(r, _)| *r == root);
        if self.db.detected.is_some() || loading {
            return;
        }
        self.db_detect(root, cx);
    }

    pub(super) fn db_detect(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let task_root = root.clone();
        let containers = self.container_infos();
        let task = cx.spawn(async move |this, cx| {
            let read = root.clone();
            let found = cx
                .background_executor()
                .spawn(async move {
                    let env = detect::read_env(&read);
                    detect::detect(&read, &env, &containers, &[])
                })
                .await;
            this.update(cx, |shell, cx| {
                if shell
                    .db
                    .detect_task
                    .as_ref()
                    .is_some_and(|(r, _)| *r == root)
                {
                    shell.db.detect_task = None;
                }
                if shell.active_root().as_ref() == Some(&root) {
                    shell.db.detected = Some(Detected { root, found });
                    cx.notify();
                }
            })
            .ok();
        });
        self.db.detect_task = Some((task_root, task));
    }

    pub(in crate::shell) fn db_env_changed(&mut self, root: &Path, cx: &mut Context<Self>) {
        let known = self.db.detected.as_ref().is_some_and(|d| d.root == root)
            || self.db.detect_task.as_ref().is_some_and(|(r, _)| r == root);
        if known {
            self.db_detect(root.to_path_buf(), cx);
        }
    }

    pub(in crate::shell) fn db_containers_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.active_root() {
            let known = self.db.detected.as_ref().is_some_and(|d| d.root == root)
                || self
                    .db
                    .detect_task
                    .as_ref()
                    .is_some_and(|(r, _)| *r == root);
            if known {
                self.db_detect(root, cx);
            }
        }
    }

    pub(super) fn db_detected_rows(&self) -> Vec<DetectedRow> {
        let ready = self.db.loaded && self.db.load_error.is_none() && self.db_shared_ready();
        match (&self.db.detected, self.active_root()) {
            (Some(d), Some(root)) if ready && d.root == root => {
                visible(d, &self.db_listed(), &self.db.hidden_detected)
                    .into_iter()
                    .map(row)
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn db_open_detected(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.db.sheet.is_some() {
            return;
        }
        let root = self.active_root();
        let Some(s) = self
            .db
            .detected
            .as_ref()
            .filter(|d| Some(&d.root) == root.as_ref())
            .and_then(|d| d.found.iter().find(|s| s.key == key))
        else {
            return;
        };
        let found = Found {
            conn: s.conn.clone(),
            password: s.password.clone(),
            note: format!(
                "Found in {}. Check the host before you test or save.",
                source_label(&s.source)
            ),
            password_note: password_note(&s.source),
        };
        self.db_open_sheet(SheetMode::New, window, cx);
        if let Some(open) = &self.db.sheet {
            open.entity.update(cx, |sheet, cx| sheet.prefill(found, cx));
        }
    }

    pub(super) fn db_can_look_in_docker(&self) -> bool {
        self.active_root().is_some() && !self.containers_started()
    }

    pub(super) fn render_look_in_docker(
        &self,
        centered: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        div()
            .id("db-look-docker")
            .when(!centered, |el| el.w_full())
            .when(centered, |el| el.justify_center())
            .h(t.ui(ROW_HEIGHT))
            .pl(px(t.ui_scale * 12.))
            .pr(t.space(Space::Md))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_size(t.typography.caption)
            .text_color(t.color.content_muted)
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.containers_start(cx)))
            .child("Look in Docker")
            .into_any_element()
    }

    fn open_detected_menu(
        &mut self,
        key: String,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let items = vec![
            shell_item("Add Connection…", cx, {
                let key = key.clone();
                move |this, window, cx| this.db_open_detected(&key, window, cx)
            }),
            shell_item("Hide", cx, move |this, _, cx| {
                this.db.hidden_detected.insert((root.clone(), key.clone()));
                cx.notify();
            }),
        ];
        self.open_context_menu(event.position, items, window, cx);
    }

    pub(super) fn render_detected(
        &self,
        rows: Vec<DetectedRow>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let header = div()
            .flex()
            .items_center()
            .px(t.space(Space::Lg))
            .h(t.ui(ROW_HEIGHT))
            .text_size(t.typography.caption)
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.color.content_secondary)
            .child("Detected");
        let header = (!rows.is_empty()).then_some(header);
        let items = rows
            .into_iter()
            .map(|row| {
                let key = row.key.clone();
                let tip = row.tip.clone();
                div()
                    .id(gpui::ElementId::Name(format!("db-detected:{key}").into()))
                    .w_full()
                    .h(t.ui(ROW_HEIGHT))
                    .pl(px(t.ui_scale * 12.))
                    .pr(t.space(Space::Md))
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .cursor_pointer()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .hover(|s| s.bg(t.color.surface_hover))
                    .tooltip(move |_, cx| daedalus_ui::Tooltip::view(tip.clone(), cx))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener({
                            let key = key.clone();
                            move |this, event: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.open_detected_menu(key.clone(), event, window, cx);
                            }
                        }),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.db_open_detected(&key, window, cx);
                    }))
                    .child(div().min_w_0().flex_1().ellipsis().child(row.name))
                    .child(div().flex_none().child(engine_label(row.engine)))
                    .child(
                        div()
                            .flex_none()
                            .max_w(gpui::relative(0.45))
                            .ellipsis()
                            .child(row.source),
                    )
            })
            .collect::<Vec<_>>();
        let look = self
            .db_can_look_in_docker()
            .then(|| self.render_look_in_docker(false, cx));
        div()
            .children(header)
            .children(items)
            .children(look)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use daedalus_db::Origin;
    use daedalus_db::detect::EnvFile;

    use super::*;

    const ROOT: &str = "/proj";

    fn found(text: &str) -> Detected {
        let env = [EnvFile {
            name: ".env",
            text: text.into(),
        }];
        Detected {
            root: ROOT.into(),
            found: detect::detect(Path::new(ROOT), &env, &[], &[]),
        }
    }

    fn listed(url: &str, origin: Origin) -> Listed {
        let (mut conn, _) = daedalus_db::from_url(url).unwrap();
        conn.id = "saved".into();
        Listed { conn, origin }
    }

    #[test]
    fn saved_target_hides_a_suggestion() {
        let d = found("DATABASE_URL=postgres://me@127.0.0.1/app\n");
        assert_eq!(visible(&d, &[], &HashSet::new()).len(), 1);
        let saved = listed("postgres://me@localhost:5432/app", Origin::Global);
        assert!(visible(&d, &[saved], &HashSet::new()).is_empty());
    }

    #[test]
    fn shared_row_hides_a_suggestion() {
        let d = found("DATABASE_URL=postgres://me@127.0.0.1/app\n");
        let saved = listed("postgres://me@localhost/app", Origin::Project);
        assert!(visible(&d, &[saved], &HashSet::new()).is_empty());
    }

    #[test]
    fn ssh_row_does_not_hide_a_local_suggestion() {
        let d = found("DATABASE_URL=postgres://me@127.0.0.1/app\n");
        let mut saved = listed("postgres://me@localhost/app", Origin::Global);
        saved.conn.ssh = Some(daedalus_db::SshTunnel {
            host: "jump.example".into(),
            port: None,
            user: String::new(),
            identity_file: None,
        });
        assert_eq!(visible(&d, &[saved], &HashSet::new()).len(), 1);
    }

    #[test]
    fn other_user_is_still_suggested() {
        let d = found("DATABASE_URL=postgres://me@127.0.0.1/app\n");
        let saved = listed("postgres://admin@localhost/app", Origin::Global);
        assert_eq!(visible(&d, &[saved], &HashSet::new()).len(), 1);
    }

    #[test]
    fn hidden_key_is_per_root() {
        let d = found("DATABASE_URL=postgres://me@127.0.0.1/app\n");
        let key = d.found[0].key.clone();
        let mut hidden = HashSet::new();
        hidden.insert((PathBuf::from("/other"), key.clone()));
        assert_eq!(visible(&d, &[], &hidden).len(), 1);
        hidden.insert((PathBuf::from(ROOT), key));
        assert!(visible(&d, &[], &hidden).is_empty());
    }

    #[test]
    fn source_label_names_file_and_key_or_container() {
        let env = Source::Env {
            file: ".env.local",
            key: "DATABASE_URL".into(),
        };
        assert_eq!(source_label(&env), ".env.local · DATABASE_URL");
        let container = Source::Container {
            name: "db-1".into(),
        };
        assert_eq!(source_label(&container), "docker · db-1");
    }

    fn unsafe_char(c: char) -> bool {
        c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}')
    }

    #[test]
    fn labels_are_sanitised() {
        let d = found("DATABASE_URL=postgres://me@127.0.0.1/app\n");
        let mut s = d.found[0].clone();
        s.conn.name = format!("\x1b[31m{}", "n".repeat(100));
        let shown = row(&s);
        assert!(!shown.name.chars().any(unsafe_char));
        assert!(shown.name.ends_with('…'));
        let container = Source::Container {
            name: "db\u{202E}gnp".into(),
        };
        let label = source_label(&container);
        assert!(!label.chars().any(unsafe_char));
        assert!(label.contains('\u{FFFD}'));
        let long = Source::Container {
            name: "c".repeat(100),
        };
        assert!(source_label(&long).ends_with('…'));
    }

    #[test]
    fn password_note_names_the_file_only_for_url_keys() {
        let url = Source::Env {
            file: ".env.local",
            key: "DATABASE_URL".into(),
        };
        assert_eq!(password_note(&url), "A password was found in .env.local");
        let group = Source::Env {
            file: ".env",
            key: "POSTGRES_PASSWORD".into(),
        };
        assert!(password_note(&group).contains("these") || password_note(&group).contains("files"));
        assert!(!password_note(&group).contains(".env.local"));
    }

    #[test]
    fn tip_never_contains_the_password() {
        let d = found("DATABASE_URL=postgres://me:hunter2@127.0.0.1/app\n");
        let shown = row(&d.found[0]);
        assert!(d.found[0].password.is_some());
        assert!(!shown.tip.contains("hunter2"));
    }
}
