use std::collections::HashMap;

use daedalus_db::{Engine, Listed};
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, DotState, Ellipsis, IconButton, MenuItem, Space, StatusDot,
    empty_state, motion,
};
use daedalus_workspace::FolderColor;
use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Context, FontWeight, MouseButton, MouseDownEvent,
    Window, div, prelude::*, px,
};

use super::{Meta, Phase, conn_key};
use crate::shell::Shell;
use crate::shell::menus::shell_item;
use crate::shell::sidebar::folder_color;
use crate::shell::tree::{INDENT, ROW_HEIGHT};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Row {
    Conn {
        key: String,
        name: String,
        engine: Engine,
        color: Option<FolderColor>,
        production: bool,
        read_only: bool,
        phase: Phase,
    },
    Error {
        key: String,
        message: String,
    },
}

fn data_color(name: Option<&str>) -> Option<FolderColor> {
    serde_json::from_value(serde_json::Value::String(name?.to_owned())).ok()
}

fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "PostgreSQL",
        Engine::Mysql => "MySQL",
        Engine::Sqlite => "SQLite",
    }
}

fn rows(listed: &[Listed], meta: &HashMap<String, Meta>) -> Vec<Row> {
    let mut out = Vec::new();
    for l in listed {
        let key = conn_key(l.origin, &l.conn.id);
        let phase = meta.get(&key).map(|m| m.phase.clone()).unwrap_or_default();
        out.push(Row::Conn {
            key: key.clone(),
            name: l.conn.name.clone(),
            engine: l.conn.engine,
            color: data_color(l.conn.color.as_deref()),
            production: l.conn.production,
            read_only: l.conn.read_only,
            phase: phase.clone(),
        });
        if let Phase::Failed(message) = phase {
            out.push(Row::Error { key, message });
        }
    }
    out
}

impl Shell {
    pub(in crate::shell) fn render_data_explorer(
        &mut self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.db_ensure_loaded(cx);
        let t = cx.theme().clone();
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .px(t.space(Space::Lg))
            .h(t.ui(ROW_HEIGHT))
            .child(
                div()
                    .text_size(t.typography.caption)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content_secondary)
                    .child("Databases"),
            )
            .child(
                IconButton::new("db-reload")
                    .label("Reload")
                    .tooltip("Reload databases.json")
                    .on_click(cx.listener(|this, _, _, cx| this.db_reload(cx))),
            );
        let body = if let Some(error) = self.db.load_error.clone() {
            let reload = Button::new("db-reload-empty", "Reload", ButtonKind::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.db_reload(cx)));
            empty_state("Could not read databases.json", error, Some(reload), cx)
                .p(t.space(Space::Lg))
                .into_any_element()
        } else {
            let root = self.active_root();
            let listed = self
                .db
                .store
                .as_ref()
                .map(|s| daedalus_db::for_project(s, root.as_deref()))
                .unwrap_or_default();
            if self.db.loaded && listed.is_empty() {
                empty_state(
                    "No databases",
                    "Add connections in databases.json, or use New connection once it ships.",
                    None,
                    cx,
                )
                .p(t.space(Space::Lg))
                .into_any_element()
            } else {
                let rows = rows(&listed, &self.db.meta);
                div()
                    .id("data-explorer-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows.into_iter().map(|row| match row {
                        Row::Conn { .. } => self.render_conn_row(row, &listed, cx),
                        Row::Error { key, message } => self.render_error_row(key, message, cx),
                    }))
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_error_row(&self, key: String, message: String, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let tip = message.clone();
        div()
            .id(gpui::ElementId::Name(format!("db-error:{key}").into()))
            .w_full()
            .h(t.ui(ROW_HEIGHT))
            .pl(px(t.ui_scale * (12. + INDENT)))
            .pr(t.space(Space::Md))
            .flex()
            .items_center()
            .text_size(t.typography.caption)
            .text_color(t.color.danger)
            .tooltip(move |_, cx| daedalus_ui::Tooltip::view(tip.clone(), cx))
            .child(div().flex_1().min_w_0().ellipsis().child(message))
            .into_any_element()
    }

    fn render_conn_row(&self, row: Row, listed: &[Listed], cx: &mut Context<Self>) -> AnyElement {
        let Row::Conn {
            key,
            name,
            engine,
            color,
            production,
            read_only,
            phase,
        } = row
        else {
            return div().into_any_element();
        };
        let t = cx.theme().clone();
        let selected = self.db.selected.as_deref() == Some(key.as_str());
        let tint = color.map_or(t.color.content_muted, |c| folder_color(&t, c));
        let dot = match &phase {
            Phase::Connecting(busy) if busy.shows(t.motion.indicator_delay) => {
                let angle = match t.motion.reduced {
                    true => 0.,
                    false => motion::spin_turn(busy.since().elapsed()),
                };
                StatusDot::new(DotState::Running)
                    .rotation(angle)
                    .into_any_element()
            }
            Phase::Connected(_) => dot_shape(true, tint, &t),
            _ => dot_shape(false, tint, &t),
        };
        let server = match &phase {
            Phase::Connected(server) => Some(server.clone()),
            _ => None,
        };
        let connected = matches!(phase, Phase::Connecting(_) | Phase::Connected(_));
        let listed_conn = listed
            .iter()
            .find(|l| conn_key(l.origin, &l.conn.id) == key)
            .cloned();
        let toggle = {
            let (key, listed_conn) = (key.clone(), listed_conn.clone());
            move |this: &mut Shell, cx: &mut Context<Shell>| {
                if connected {
                    this.db_disconnect(&key, cx);
                } else if let Some(l) = listed_conn.clone() {
                    this.db_connect(l, cx);
                }
            }
        };
        let (marker_toggle, click_toggle) = (toggle.clone(), toggle);
        div()
            .id(gpui::ElementId::Name(format!("db-row:{key}").into()))
            .w_full()
            .h(t.ui(ROW_HEIGHT))
            .pl(px(t.ui_scale * 12.))
            .pr(t.space(Space::Md))
            .flex()
            .items_center()
            .gap(t.space(Space::Sm))
            .cursor_pointer()
            .text_size(t.typography.caption)
            .text_color(if selected {
                t.color.accent
            } else {
                t.color.content_secondary
            })
            .when(selected, |el| {
                el.font_weight(FontWeight::MEDIUM)
                    .bg(t.color.surface_accent)
            })
            .when(!selected, |el| {
                el.hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let (key, listed_conn) = (key.clone(), listed_conn.clone());
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_db_menu(key.clone(), listed_conn.clone(), event, window, cx);
                    }
                }),
            )
            .on_click(cx.listener({
                let key = key.clone();
                move |this, event: &ClickEvent, _, cx| {
                    this.db.selected = Some(key.clone());
                    if event.click_count() >= 2 {
                        click_toggle(this, cx);
                    }
                    cx.notify();
                }
            }))
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("db-chevron:{key}").into()))
                    .w(t.ui(10.))
                    .flex_none()
                    .text_color(t.color.content_muted)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        marker_toggle(this, cx);
                    }))
                    .child(if connected { "▾" } else { "▸" }),
            )
            .child(dot)
            .child(
                div()
                    .min_w_0()
                    .ellipsis()
                    .when(production, |el| el.text_color(t.color.danger))
                    .child(name),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(t.color.content_muted)
                    .child(engine_label(engine)),
            )
            .when(production, |el| el.child(tag("prod", t.color.danger)))
            .when(read_only, |el| {
                el.child(tag("read-only", t.color.content_muted))
            })
            .when_some(server, |el, server| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .ellipsis()
                        .text_right()
                        .text_color(t.color.content_muted)
                        .child(server),
                )
            })
            .into_any_element()
    }

    fn open_db_menu(
        &mut self,
        key: String,
        listed: Option<Listed>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(listed) = listed else {
            return;
        };
        let connected = matches!(
            self.db.phase(&key),
            Phase::Connecting(_) | Phase::Connected(_)
        );
        let toggle = match connected {
            true => shell_item("Disconnect", cx, {
                let key = key.clone();
                move |this, _, cx| this.db_disconnect(&key, cx)
            }),
            false => shell_item("Connect", cx, {
                let listed = listed.clone();
                move |this, _, cx| this.db_connect(listed.clone(), cx)
            }),
        };
        let url = daedalus_db::redacted_url(&listed.conn);
        let items = vec![
            toggle,
            MenuItem::separator(),
            MenuItem::new("Copy URL", move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
            }),
        ];
        self.open_context_menu(event.position, items, window, cx);
    }
}

fn dot_shape(filled: bool, tint: gpui::Hsla, t: &daedalus_ui::Theme) -> AnyElement {
    let size = t.ui(8.);
    div()
        .size(size)
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(tint)
        .when(filled, |el| el.bg(tint))
        .into_any_element()
}

fn tag(label: &'static str, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_none()
        .text_color(color)
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use daedalus_db::{Origin, Store, Tls};

    fn conn(id: &str, name: &str) -> daedalus_db::Connection {
        daedalus_db::Connection {
            id: id.into(),
            name: name.into(),
            engine: Engine::Sqlite,
            host: String::new(),
            port: None,
            database: "/tmp/x.db".into(),
            user: String::new(),
            tls: Tls::Prefer,
            ca_file: None,
            read_only: false,
            production: false,
            color: None,
            claude: false,
            projects: vec![],
            save_password: false,
        }
    }

    fn store(conns: Vec<daedalus_db::Connection>) -> Store {
        let mut s = Store::empty();
        for c in conns {
            s.upsert(c);
        }
        s
    }

    #[test]
    fn rows_put_the_error_under_its_connection() {
        let listed = daedalus_db::for_project(&store(vec![conn("a", "A"), conn("b", "B")]), None);
        let mut meta = HashMap::new();
        meta.insert(
            conn_key(Origin::Global, "a"),
            Meta {
                session: None,
                phase: Phase::Failed("no such file".into()),
            },
        );
        let out = rows(&listed, &meta);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Row::Conn { name, .. } if name == "A"));
        assert!(matches!(&out[1], Row::Error { message, .. } if message == "no such file"));
        assert!(matches!(&out[2], Row::Conn { name, .. } if name == "B"));
    }

    #[test]
    fn rows_follow_for_project_order() {
        let listed = daedalus_db::for_project(
            &store(vec![
                conn("1", "beta"),
                conn("2", "Alpha"),
                conn("3", "gamma"),
            ]),
            None,
        );
        let names: Vec<_> = rows(&listed, &HashMap::new())
            .into_iter()
            .filter_map(|r| match r {
                Row::Conn { name, .. } => Some(name),
                Row::Error { .. } => None,
            })
            .collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"]);
    }

    #[test]
    fn data_color_reads_kebab_names_and_ignores_unknown() {
        assert_eq!(data_color(Some("magenta")), Some(FolderColor::Magenta));
        assert_eq!(data_color(Some("accent")), Some(FolderColor::Accent));
        assert_eq!(data_color(Some("chartreuse")), None);
        assert_eq!(data_color(None), None);
    }
}
