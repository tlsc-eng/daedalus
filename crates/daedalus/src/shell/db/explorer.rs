use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use daedalus_db::catalog::TableKind;
use daedalus_db::{Engine, Listed, Origin};
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, DotState, Ellipsis, IconButton, MenuItem, Space, StatusDot,
    empty_state, motion,
};
use daedalus_workspace::FolderColor;
use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Context, FontWeight, MouseButton, MouseDownEvent,
    Window, div, prelude::*, px,
};

use super::sheet::SheetMode;
use super::{Meta, NodeKey, Phase, SharedFile, conn_key};
use crate::shell::Shell;
use crate::shell::claude_sessions::list::chevron;
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
        shared: bool,
        read_only: bool,
        claude: bool,
        claude_busy: bool,
        phase: Phase,
        expanded: bool,
    },
    Error {
        key: String,
        message: String,
    },
    Schema {
        conn: String,
        name: String,
        count: usize,
        expanded: bool,
    },
    Table {
        conn: String,
        schema: String,
        name: String,
        kind: TableKind,
        expanded: bool,
    },
    Column {
        parent: NodeKey,
        name: String,
        type_name: String,
        pk: bool,
        nullable: bool,
    },
    Note {
        parent: NodeKey,
        depth: u8,
        text: String,
        tone: Tone,
        retry: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Tone {
    Muted,
    Danger,
}

/// Each row's opacity: the smallest chevron turn among its ancestors, so a whole subtree fades in together.
fn fades(rows: &[Row], turn: impl Fn(&NodeKey) -> f32) -> Vec<f32> {
    let (mut conn, mut schema, mut table) = (1f32, 1f32, 1f32);
    rows.iter()
        .map(|row| match row {
            Row::Conn { key, .. } => {
                conn = turn(&NodeKey::Conn(key.clone()));
                1.
            }
            Row::Error { .. } => 1.,
            Row::Schema { conn: c, name, .. } => {
                schema = conn.min(turn(&NodeKey::Schema(c.clone(), name.clone())));
                conn
            }
            Row::Table {
                conn: c,
                schema: s,
                name,
                ..
            } => {
                table = schema.min(turn(&NodeKey::Table(c.clone(), s.clone(), name.clone())));
                schema
            }
            Row::Column { .. } => table,
            Row::Note { parent, .. } => match parent {
                NodeKey::Conn(_) => conn,
                NodeKey::Schema(..) => schema,
                NodeKey::Table(..) => table,
            },
        })
        .collect()
}

pub(super) fn data_color(name: Option<&str>) -> Option<FolderColor> {
    serde_json::from_value(serde_json::Value::String(name?.to_owned())).ok()
}

pub(super) fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "PostgreSQL",
        Engine::Mysql => "MySQL",
        Engine::Sqlite => "SQLite",
    }
}

fn rows(
    listed: &[Listed],
    meta: &HashMap<String, Meta>,
    open: &HashSet<NodeKey>,
    delay: Duration,
) -> Vec<Row> {
    let mut out = Vec::new();
    for l in listed {
        let key = conn_key(l.origin, &l.conn.id);
        let phase = meta.get(&key).map(|m| m.phase.clone()).unwrap_or_default();
        let node = NodeKey::Conn(key.clone());
        let expanded = open.contains(&node);
        out.push(Row::Conn {
            key: key.clone(),
            name: l.conn.name.clone(),
            engine: l.conn.engine,
            color: data_color(l.conn.color.as_deref()),
            production: l.conn.production,
            shared: l.origin == Origin::Project,
            read_only: l.conn.read_only,
            claude: l.conn.claude,
            claude_busy: meta.get(&key).is_some_and(|m| m.claude_runs > 0),
            phase: phase.clone(),
            expanded,
        });
        if let Phase::Failed(message) = phase {
            out.push(Row::Error { key, message });
        } else if expanded && let Some(m) = meta.get(&key) {
            push_catalog(&mut out, &key, m, open, delay);
        }
    }
    out
}

fn push_catalog(
    out: &mut Vec<Row>,
    key: &str,
    meta: &Meta,
    open: &HashSet<NodeKey>,
    delay: Duration,
) {
    let node = NodeKey::Conn(key.to_owned());
    let note = |parent: &NodeKey, depth, text: String, tone, retry| Row::Note {
        parent: parent.clone(),
        depth,
        text,
        tone,
        retry,
    };
    if meta.reading.is_some_and(|b| b.shows(delay)) {
        out.push(note(&node, 1, "Reading tables…".into(), Tone::Muted, None));
    }
    if let Some(e) = &meta.catalog_error {
        let text = format!("Could not read tables: {e}");
        out.push(note(&node, 1, text, Tone::Danger, Some(key.to_owned())));
    }
    let Some(catalog) = &meta.catalog else {
        return;
    };
    for schema in &catalog.schemas {
        let snode = NodeKey::Schema(key.to_owned(), schema.name.clone());
        let expanded = open.contains(&snode);
        out.push(Row::Schema {
            conn: key.to_owned(),
            name: schema.name.clone(),
            count: schema.tables.len(),
            expanded,
        });
        if !expanded {
            continue;
        }
        if schema.tables.is_empty() {
            out.push(note(&snode, 2, "No tables".into(), Tone::Muted, None));
        }
        for table in &schema.tables {
            let tnode = NodeKey::Table(key.to_owned(), schema.name.clone(), table.name.clone());
            let expanded = open.contains(&tnode);
            out.push(Row::Table {
                conn: key.to_owned(),
                schema: schema.name.clone(),
                name: table.name.clone(),
                kind: table.kind,
                expanded,
            });
            if expanded {
                out.extend(table.columns.iter().map(|c| Row::Column {
                    parent: tnode.clone(),
                    name: c.name.clone(),
                    type_name: c.type_name.clone(),
                    pk: c.pk,
                    nullable: c.nullable,
                }));
            }
        }
    }
    if catalog.truncated {
        let text = "Only the first 5,000 tables are listed".to_owned();
        out.push(note(&node, 1, text, Tone::Muted, None));
    }
}

impl Shell {
    pub(in crate::shell) fn render_data_explorer(
        &mut self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.db_ensure_loaded(cx);
        self.db_ensure_shared(cx);
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
                div()
                    .flex()
                    .items_center()
                    .child(
                        IconButton::new("db-new")
                            .label("New")
                            .tooltip("New connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.db_open_sheet(SheetMode::New, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("db-reload")
                            .label("Reload")
                            .tooltip("Reload databases.json")
                            .on_click(cx.listener(|this, _, _, cx| this.db_reload(cx))),
                    ),
            );
        let body = if let Some(error) = self.db.load_error.clone() {
            let reload = Button::new("db-reload-empty", "Reload", ButtonKind::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.db_reload(cx)));
            empty_state("Could not read databases.json", error, Some(reload), cx)
                .p(t.space(Space::Lg))
                .into_any_element()
        } else {
            let root = self.active_root();
            let listed = self.db_listed();
            let notice = self
                .db
                .shared
                .as_ref()
                .filter(|s| Some(&s.root) == root.as_ref())
                .and_then(|file| {
                    let (line, error) = shared_notice(file)?;
                    Some(self.render_shared_notice(
                        root.clone()?,
                        line,
                        error,
                        notice_tip(file),
                        cx,
                    ))
                });
            let list = if self.db.loaded && listed.is_empty() {
                let new = Button::new("db-new-empty", "New connection", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.db_open_sheet(SheetMode::New, window, cx)
                    }));
                empty_state(
                    "No databases",
                    "Add a connection here, or edit databases.json.",
                    Some(new),
                    cx,
                )
                .p(t.space(Space::Lg))
                .into_any_element()
            } else {
                let delay = t.motion.indicator_delay;
                let rows = rows(&listed, &self.db.meta, &self.db.expanded, delay);
                let fade = fades(&rows, |n| self.db.turn_of(n, t.motion.fast));
                div()
                    .id("data-explorer-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows.into_iter().zip(fade).map(|(row, fade)| {
                        let el = match row {
                            Row::Conn { .. } => self.render_conn_row(row, &listed, cx),
                            Row::Error { key, message } => self.render_error_row(key, message, cx),
                            row => self.render_node_row(row, cx),
                        };
                        match fade < 1. {
                            true => div().opacity(fade).child(el).into_any_element(),
                            false => el,
                        }
                    }))
                    .into_any_element()
            };
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .children(notice)
                .child(list)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_shared_notice(
        &self,
        root: PathBuf,
        line: String,
        error: bool,
        tip: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let tint = if error {
            t.color.danger
        } else {
            t.color.warning
        };
        div()
            .id("db-shared-notice")
            .w_full()
            .h(t.ui(ROW_HEIGHT))
            .flex_none()
            .pl(t.space(Space::Lg))
            .pr(t.space(Space::Md))
            .flex()
            .items_center()
            .text_size(t.typography.caption)
            .text_color(tint)
            .tooltip(move |_, cx| daedalus_ui::Tooltip::view(tip.clone(), cx))
            .when(!error, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let path = daedalus_db::shared::path(&root);
                        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                            this.open_file(path, window, cx);
                        }
                    }))
            })
            .child(div().flex_1().min_w_0().ellipsis().child(line))
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

    fn render_node_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let fast = t.motion.fast;
        let base = |id: String, depth: u8| {
            div()
                .id(gpui::ElementId::Name(id.into()))
                .w_full()
                .h(t.ui(ROW_HEIGHT))
                .pl(px(t.ui_scale * (12. + INDENT * f32::from(depth))))
                .pr(t.space(Space::Md))
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .text_size(t.typography.caption)
                .text_color(t.color.content_secondary)
        };
        let slot = || div().w(t.ui(10.)).flex_none();
        match row {
            Row::Schema {
                conn, name, count, ..
            } => {
                let node = NodeKey::Schema(conn.clone(), name.clone());
                let turn = self.db.turn_of(&node, fast);
                base(format!("db-schema:{conn}:{name}"), 1)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.db_toggle(node.clone(), None, window, cx)
                    }))
                    .child(slot().child(chevron(turn, &t)))
                    .child(div().min_w_0().ellipsis().child(name))
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.color.content_muted)
                            .child(count.to_string()),
                    )
                    .into_any_element()
            }
            Row::Table {
                conn,
                schema,
                name,
                kind,
                ..
            } => {
                let node = NodeKey::Table(conn.clone(), schema.clone(), name.clone());
                let turn = self.db.turn_of(&node, fast);
                base(format!("db-table:{conn}:{schema}:{name}"), 2)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                    .on_click(cx.listener({
                        let (conn, schema, name) = (conn.clone(), schema.clone(), name.clone());
                        move |this, event: &ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.db_open_table(
                                    conn.clone(),
                                    schema.clone(),
                                    name.clone(),
                                    window,
                                    cx,
                                );
                            } else {
                                this.db_toggle(node.clone(), None, window, cx)
                            }
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener({
                            let (conn, schema, name) = (conn.clone(), schema.clone(), name.clone());
                            move |this, event: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                let open = shell_item("Open Table", cx, {
                                    let (conn, schema, name) =
                                        (conn.clone(), schema.clone(), name.clone());
                                    move |this, window, cx| {
                                        this.db_open_table(
                                            conn.clone(),
                                            schema.clone(),
                                            name.clone(),
                                            window,
                                            cx,
                                        )
                                    }
                                });
                                this.open_context_menu(event.position, vec![open], window, cx);
                            }
                        }),
                    )
                    .child(slot().child(chevron(turn, &t)))
                    .child(div().min_w_0().ellipsis().child(name))
                    .when(kind == TableKind::View, |el| {
                        el.child(tag("view", t.color.content_muted))
                    })
                    .into_any_element()
            }
            Row::Column {
                parent,
                name,
                type_name,
                pk,
                nullable,
            } => base(format!("db-column:{parent:?}:{name}"), 3)
                .child(slot())
                .child(div().min_w_0().ellipsis().child(name))
                .child(
                    div()
                        .flex_none()
                        .font_family(t.typography.mono.clone())
                        .text_color(t.color.content_muted)
                        .child(type_name),
                )
                .when(pk, |el| el.child(tag("PK", t.color.accent)))
                .when(nullable, |el| el.child(tag("NULL", t.color.content_muted)))
                .into_any_element(),
            Row::Note {
                parent,
                depth,
                text,
                tone,
                retry,
            } => {
                let color = match tone {
                    Tone::Muted => t.color.content_muted,
                    Tone::Danger => t.color.danger,
                };
                let tip = text.clone();
                base(format!("db-note:{parent:?}:{text}"), depth)
                    .text_color(color)
                    .tooltip(move |_, cx| daedalus_ui::Tooltip::view(tip.clone(), cx))
                    .when_some(retry, |el, key| {
                        el.cursor_pointer()
                            .hover(|s| s.bg(t.color.surface_hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.db_introspect(&key, cx)
                            }))
                    })
                    .child(slot())
                    .child(div().flex_1().min_w_0().ellipsis().child(text))
                    .into_any_element()
            }
            Row::Conn { .. } | Row::Error { .. } => div().into_any_element(),
        }
    }

    fn render_conn_row(&self, row: Row, listed: &[Listed], cx: &mut Context<Self>) -> AnyElement {
        let Row::Conn {
            key,
            name,
            engine,
            color,
            production,
            shared,
            read_only,
            claude,
            claude_busy,
            phase,
            expanded: _,
        } = row
        else {
            return div().into_any_element();
        };
        let t = cx.theme().clone();
        let selected = self.db.selected.as_deref() == Some(key.as_str());
        let tint = color.map_or(t.color.content_muted, |c| folder_color(&t, c));
        let (delay, hold) = (t.motion.indicator_delay, t.motion.indicator_hold);
        let spinning = match &phase {
            Phase::Connecting(busy) => Some(busy.shows(delay)).filter(|s| *s).map(|_| *busy),
            Phase::Connected { busy, at, .. }
                if super::spinner_lingers(*busy, *at, Instant::now(), delay, hold) =>
            {
                Some(*busy)
            }
            _ => None,
        };
        let dot = match (spinning, &phase) {
            (Some(busy), _) => {
                let angle = match t.motion.reduced {
                    true => 0.,
                    false => motion::spin_turn(busy.since().elapsed()),
                };
                StatusDot::new(DotState::Running)
                    .rotation(angle)
                    .into_any_element()
            }
            (None, Phase::Connected { .. }) => dot_shape(true, tint, &t),
            _ => dot_shape(false, tint, &t),
        };
        let server = match &phase {
            Phase::Connected { server, .. } => Some(server.clone()),
            _ => None,
        };
        let node = NodeKey::Conn(key.clone());
        let turn = self.db.turn_of(&node, t.motion.fast);
        let listed_conn = listed
            .iter()
            .find(|l| conn_key(l.origin, &l.conn.id) == key)
            .cloned();
        let toggle = {
            let (key, listed_conn) = (key.clone(), listed_conn.clone());
            move |this: &mut Shell, window: &mut Window, cx: &mut Context<Shell>| {
                this.db_toggle(NodeKey::Conn(key.clone()), listed_conn.clone(), window, cx);
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
                move |this, event: &ClickEvent, window, cx| {
                    this.db.selected = Some(key.clone());
                    if event.click_count() >= 2 {
                        click_toggle(this, window, cx);
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        marker_toggle(this, window, cx);
                    }))
                    .child(chevron(turn, &t)),
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
            .when(shared, |el| el.child(tag("project", t.color.content_muted)))
            .when(production, |el| el.child(tag("prod", t.color.danger)))
            .when(read_only, |el| {
                el.child(tag("read-only", t.color.content_muted))
            })
            .when(claude, |el| {
                let tint = if claude_busy {
                    t.color.accent
                } else {
                    t.color.content_muted
                };
                el.child(tag("claude", tint))
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
            Phase::Connecting(_) | Phase::Connected { .. }
        );
        let toggle = match connected {
            true => shell_item("Disconnect", cx, {
                let key = key.clone();
                move |this, _, cx| this.db_disconnect(&key, cx)
            }),
            false => shell_item("Connect", cx, {
                let listed = listed.clone();
                move |this, window, cx| this.db_connect(listed.clone(), window, cx)
            }),
        };
        let refresh = matches!(self.db.phase(&key), Phase::Connected { .. }).then(|| {
            shell_item("Refresh", cx, {
                let key = key.clone();
                move |this, _, cx| this.db_introspect(&key, cx)
            })
        });
        let url = daedalus_db::redacted_url(&listed.conn);
        let mut items = vec![
            shell_item("New Console", cx, {
                let listed = listed.clone();
                move |this, window, cx| this.db_open_console(listed.clone(), window, cx)
            }),
            MenuItem::separator(),
            toggle,
        ];
        if listed.origin == Origin::Global {
            items.push(shell_item("Edit…", cx, {
                let listed = listed.clone();
                move |this, window, cx| {
                    this.db_open_sheet(SheetMode::Edit(listed.clone()), window, cx)
                }
            }));
        }
        items.push(MenuItem::separator());
        items.push(MenuItem::new("Copy URL", move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
        }));
        if listed.origin == Origin::Global {
            items.push(MenuItem::separator());
            items.push(shell_item("Delete…", cx, {
                let listed = listed.clone();
                move |this, window, cx| this.db_delete(listed.clone(), window, cx)
            }));
        }
        if let Some(refresh) = refresh {
            items.insert(1, refresh);
        }
        self.open_context_menu(event.position, items, window, cx);
    }
}

/// The explorer's line about the shared file, and whether it is an error.
fn shared_notice(file: &SharedFile) -> Option<(String, bool)> {
    if file.error.is_some() {
        return Some(("Could not read .daedalus/databases.json".into(), true));
    }
    let n = file.problems.len();
    if n == 0 {
        return None;
    }
    let count = if n == 1 {
        "1 problem".to_owned()
    } else {
        format!("{n} problems")
    };
    Some((format!("{count} in .daedalus/databases.json"), false))
}

/// File text can echo teammate-written strings, so each line is cut before it reaches a tooltip.
fn notice_tip(file: &SharedFile) -> String {
    const SHOWN: usize = 8;
    let cut = |s: &String| s.chars().take(200).collect::<String>();
    if let Some(e) = &file.error {
        return cut(e);
    }
    let mut lines: Vec<String> = file.problems.iter().take(SHOWN).map(cut).collect();
    if file.problems.len() > SHOWN {
        lines.push(format!("…and {} more", file.problems.len() - SHOWN));
    }
    lines.join("\n")
}

pub(super) fn dot_shape(filled: bool, tint: gpui::Hsla, t: &daedalus_ui::Theme) -> AnyElement {
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

pub(super) fn tag(label: &'static str, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_none()
        .text_color(color)
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use daedalus_db::catalog::{Catalog, CatalogColumn, Schema, Table};
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
            ssh: None,
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
        let listed =
            daedalus_db::for_project(&store(vec![conn("a", "A"), conn("b", "B")]), &[], None);
        let mut meta = HashMap::new();
        meta.insert(
            conn_key(Origin::Global, "a"),
            Meta {
                session: None,
                phase: Phase::Failed("no such file".into()),
                ..Meta::default()
            },
        );
        let out = rows(&listed, &meta, &HashSet::new(), Duration::ZERO);
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
            &[],
            None,
        );
        let names: Vec<_> = rows(&listed, &HashMap::new(), &HashSet::new(), Duration::ZERO)
            .into_iter()
            .filter_map(|r| match r {
                Row::Conn { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"]);
    }

    fn column(name: &str, pk: bool, nullable: bool) -> CatalogColumn {
        CatalogColumn {
            name: name.into(),
            type_name: "INTEGER".into(),
            nullable,
            pk,
        }
    }

    fn table(name: &str, kind: TableKind, columns: Vec<CatalogColumn>) -> Table {
        Table {
            name: name.into(),
            kind,
            columns,
        }
    }

    fn schema(name: &str, tables: Vec<Table>) -> Schema {
        Schema {
            name: name.into(),
            tables,
        }
    }

    const KEY: &str = "g:a";

    fn setup(
        schemas: Vec<Schema>,
        truncated: bool,
        open: &[NodeKey],
    ) -> (Vec<Listed>, HashMap<String, Meta>, HashSet<NodeKey>) {
        let listed = daedalus_db::for_project(&store(vec![conn("a", "A")]), &[], None);
        let mut meta = HashMap::new();
        meta.insert(
            KEY.to_owned(),
            Meta {
                session: Some(daedalus_db::worker::SessionId(1)),
                phase: Phase::Connecting(daedalus_ui::motion::Busy::start()),
                catalog: Some(Catalog { schemas, truncated }),
                ..Meta::default()
            },
        );
        let mut open: HashSet<_> = open.iter().cloned().collect();
        open.insert(NodeKey::Conn(KEY.into()));
        (listed, meta, open)
    }

    fn schema_key(name: &str) -> NodeKey {
        NodeKey::Schema(KEY.into(), name.into())
    }

    fn table_key(schema: &str, name: &str) -> NodeKey {
        NodeKey::Table(KEY.into(), schema.into(), name.into())
    }

    #[test]
    fn rows_nest_schema_table_column_in_server_order() {
        let (listed, meta, open) = setup(
            vec![
                schema(
                    "z",
                    vec![
                        table("t2", TableKind::Table, vec![column("id", true, false)]),
                        table("t1", TableKind::Table, vec![]),
                    ],
                ),
                schema("a", vec![]),
            ],
            false,
            &[schema_key("z"), table_key("z", "t2")],
        );
        let out = rows(&listed, &meta, &open, Duration::ZERO);
        assert!(matches!(&out[0], Row::Conn { expanded: true, .. }));
        assert!(
            matches!(&out[1], Row::Schema { name, count: 2, expanded: true, .. } if name == "z")
        );
        assert!(matches!(&out[2], Row::Table { name, expanded: true, .. } if name == "t2"));
        assert!(matches!(&out[3], Row::Column { name, .. } if name == "id"));
        assert!(matches!(&out[4], Row::Table { name, expanded: false, .. } if name == "t1"));
        assert!(matches!(&out[5], Row::Schema { name, expanded: false, .. } if name == "a"));
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn views_carry_a_view_tag_and_columns_carry_pk_and_null() {
        let (listed, meta, open) = setup(
            vec![schema(
                "main",
                vec![table(
                    "v",
                    TableKind::View,
                    vec![column("id", true, false), column("note", false, true)],
                )],
            )],
            false,
            &[schema_key("main"), table_key("main", "v")],
        );
        let out = rows(&listed, &meta, &open, Duration::ZERO);
        assert!(matches!(
            &out[2],
            Row::Table {
                kind: TableKind::View,
                ..
            }
        ));
        assert!(matches!(
            &out[3],
            Row::Column {
                pk: true,
                nullable: false,
                ..
            }
        ));
        assert!(matches!(
            &out[4],
            Row::Column {
                pk: false,
                nullable: true,
                ..
            }
        ));
    }

    #[test]
    fn an_empty_schema_shows_no_tables() {
        let (listed, meta, open) =
            setup(vec![schema("main", vec![])], false, &[schema_key("main")]);
        let out = rows(&listed, &meta, &open, Duration::ZERO);
        assert!(matches!(&out[2], Row::Note { text, depth: 2, .. } if text == "No tables"));
    }

    #[test]
    fn a_truncated_catalog_ends_with_the_limit_note() {
        let (listed, meta, open) = setup(vec![schema("main", vec![])], true, &[]);
        let out = rows(&listed, &meta, &open, Duration::ZERO);
        assert!(matches!(
            out.last(),
            Some(Row::Note { text, tone: Tone::Muted, .. }) if text == "Only the first 5,000 tables are listed"
        ));
    }

    #[test]
    fn a_row_fades_with_the_smallest_turn_among_its_ancestors() {
        let (listed, meta, open) = setup(
            vec![schema(
                "main",
                vec![table(
                    "t",
                    TableKind::Table,
                    vec![column("id", true, false)],
                )],
            )],
            false,
            &[schema_key("main"), table_key("main", "t")],
        );
        let out = rows(&listed, &meta, &open, Duration::ZERO);
        let turn = |n: &NodeKey| match n {
            NodeKey::Conn(_) => 0.25,
            _ => 1.,
        };
        assert_eq!(fades(&out, turn), [1., 0.25, 0.25, 0.25]);
    }

    #[test]
    fn a_collapsed_connection_hides_its_tree() {
        let (listed, meta, _) = setup(vec![schema("main", vec![])], true, &[]);
        let out = rows(&listed, &meta, &HashSet::new(), Duration::ZERO);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn reading_and_error_notes_come_first_and_the_error_can_retry() {
        let (listed, mut meta, open) = setup(vec![schema("main", vec![])], false, &[]);
        let m = meta.get_mut(KEY).unwrap();
        m.reading = Some(daedalus_ui::motion::Busy::start());
        m.catalog_error = Some("boom".into());
        let out = rows(&listed, &meta, &open, Duration::ZERO);
        assert!(matches!(&out[1], Row::Note { text, .. } if text == "Reading tables…"));
        assert!(matches!(
            &out[2],
            Row::Note { text, tone: Tone::Danger, retry: Some(_), .. } if text == "Could not read tables: boom"
        ));
        assert!(matches!(&out[3], Row::Schema { .. }));
        let out = rows(&listed, &meta, &open, Duration::from_secs(60));
        assert!(matches!(
            &out[1],
            Row::Note {
                tone: Tone::Danger,
                ..
            }
        ));
    }

    #[test]
    fn data_color_reads_kebab_names_and_ignores_unknown() {
        assert_eq!(data_color(Some("magenta")), Some(FolderColor::Magenta));
        assert_eq!(data_color(Some("accent")), Some(FolderColor::Accent));
        assert_eq!(data_color(Some("chartreuse")), None);
        assert_eq!(data_color(None), None);
    }

    fn shared_file(problems: Vec<String>, error: Option<String>) -> SharedFile {
        SharedFile {
            root: "/a".into(),
            conns: vec![],
            problems,
            error,
        }
    }

    #[test]
    fn project_rows_are_marked_shared() {
        let listed = vec![
            Listed {
                conn: conn("a", "A"),
                origin: Origin::Global,
            },
            Listed {
                conn: conn("b", "B"),
                origin: Origin::Project,
            },
        ];
        let out = rows(&listed, &HashMap::new(), &HashSet::new(), Duration::ZERO);
        assert!(matches!(&out[0], Row::Conn { shared: false, .. }));
        assert!(matches!(&out[1], Row::Conn { shared: true, .. }));
    }

    #[test]
    fn shared_notice_prefers_the_error_and_counts_problems() {
        assert_eq!(shared_notice(&shared_file(vec![], None)), None);
        let one = shared_file(vec!["x".into()], None);
        assert_eq!(
            shared_notice(&one),
            Some(("1 problem in .daedalus/databases.json".into(), false))
        );
        let two = shared_file(vec!["x".into(), "y".into()], None);
        assert_eq!(
            shared_notice(&two),
            Some(("2 problems in .daedalus/databases.json".into(), false))
        );
        let broken = shared_file(vec!["x".into()], Some("bad".into()));
        assert_eq!(
            shared_notice(&broken),
            Some(("Could not read .daedalus/databases.json".into(), true))
        );
    }

    #[test]
    fn notice_tip_cuts_long_lines_and_counts_the_rest() {
        let mut problems: Vec<String> = (0..10).map(|i| format!("p{i}")).collect();
        problems[0] = "x".repeat(500);
        let tip = notice_tip(&shared_file(problems, None));
        let lines: Vec<&str> = tip.lines().collect();
        assert_eq!(lines.len(), 9);
        assert_eq!(lines[0].chars().count(), 200);
        assert_eq!(lines[8], "…and 2 more");
    }
}
