use std::path::{Path, PathBuf};

use daedalus_db::{Connection, Engine, Listed, Origin, Secret, Tls};
use daedalus_ui::motion::Busy;
use daedalus_ui::{
    ActiveTheme, Button, ButtonKind, InputEvent, SegmentedControl, Space, TextInput, Theme,
};
use daedalus_workspace::FolderColor;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, Focusable, FontWeight, MouseButton,
    Render, Subscription, Task, Window, div, prelude::*, px,
};

use super::tab::DbTabRef;
use crate::actions::{NextField, PrevField};
use crate::shell::sidebar::folder_color;

pub(in crate::shell) enum SheetMode {
    New,
    Edit(Listed),
    Password {
        listed: Listed,
        tab: Option<DbTabRef>,
    },
}

#[derive(Clone)]
pub(in crate::shell) enum PasswordChange {
    Keep,
    Set(Secret),
    Forget,
}

#[derive(Clone)]
pub(in crate::shell) enum DbSheetEvent {
    Save {
        conn: Box<Connection>,
        origin: Origin,
        password: PasswordChange,
    },
    Test {
        conn: Box<Connection>,
        typed: Option<Secret>,
        use_saved: Option<Listed>,
    },
    Delete(Listed),
    Unlock {
        listed: Listed,
        password: Secret,
        save: bool,
        tab: Option<DbTabRef>,
    },
    Close,
}

#[derive(Clone)]
pub(super) struct Fields {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    pub color: Option<String>,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    pub keychain: bool,
    pub tls: Tls,
    pub tls_touched: bool,
    pub read_only: bool,
    pub production: bool,
    pub claude: bool,
    pub only_here: bool,
}

/// Turns the form into a connection; the id, Claude access and CA file come from `base` when editing.
pub(super) fn build(
    f: &Fields,
    base: Option<&Connection>,
    root: Option<&Path>,
) -> Result<Connection, &'static str> {
    let name = f.name.trim();
    if name.is_empty() {
        return Err("Name is required");
    }
    let sqlite = f.engine == Engine::Sqlite;
    let database = f.database.trim().to_owned();
    if sqlite && database.is_empty() {
        return Err("File is required");
    }
    let (host, port, user) = if sqlite {
        (String::new(), None, String::new())
    } else {
        let host = match f.host.trim() {
            "" => "localhost",
            h => h,
        };
        let port = match f.port.trim() {
            "" => None,
            p => Some(
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or("Port must be 1-65535")?,
            ),
        };
        (host.to_owned(), port, f.user.trim().to_owned())
    };
    let tls = match (sqlite, f.tls_touched) {
        (true, _) => base.map_or(Tls::Prefer, |b| b.tls),
        (false, true) => f.tls,
        (false, false) => Tls::default_for(&host),
    };
    let mut projects = base.map(|b| b.projects.clone()).unwrap_or_default();
    if let Some(root) = root {
        projects.retain(|p| p != root);
        if f.only_here {
            projects.push(root.to_path_buf());
        }
    }
    let id = match (base, f.id.is_empty()) {
        (Some(b), _) => b.id.clone(),
        (None, false) => f.id.clone(),
        (None, true) => daedalus_db::new_id().map_err(|_| "Could not create an id")?,
    };
    Ok(Connection {
        id,
        name: name.to_owned(),
        engine: f.engine,
        host,
        port,
        database,
        user,
        tls,
        ca_file: base.and_then(|b| b.ca_file.clone()),
        ssh: base.and_then(|b| b.ssh.clone()),
        read_only: f.read_only,
        production: f.production,
        color: f.color.clone(),
        claude: f.claude,
        projects,
        save_password: f.keychain && !sqlite,
    })
}

/// A URL carries the server and the database, not the name, colour, flags or Keychain choice.
fn url_fields(current: &Fields, url: &Connection) -> Fields {
    let name = match current.name.trim().is_empty() {
        true => url.name.clone(),
        false => current.name.clone(),
    };
    Fields {
        name,
        engine: url.engine,
        host: url.host.clone(),
        port: url.port.map(|p| p.to_string()).unwrap_or_default(),
        database: url.database.clone(),
        user: url.user.clone(),
        tls: url.tls,
        tls_touched: true,
        ..current.clone()
    }
}

/// True when saving turns Claude access on for a production connection that did not have both.
pub(in crate::shell) fn asks_for_claude_on_production(
    base: Option<&Connection>,
    new: &Connection,
) -> bool {
    new.production && new.claude && !base.is_some_and(|b| b.production && b.claude)
}

/// Whether an open session no longer matches the edited connection.
pub(in crate::shell) fn needs_reconnect(old: &Connection, new: &Connection) -> bool {
    (&old.engine, &old.host, &old.port, &old.database, &old.user)
        != (&new.engine, &new.host, &new.port, &new.database, &new.user)
        || old.tls != new.tls
        || old.ca_file != new.ca_file
        || old.read_only != new.read_only
}

/// What the sheet does with the saved password when the form is submitted.
fn password_change(base: Option<&Connection>, conn: &Connection, typed: &str) -> PasswordChange {
    let had_saved = base.is_some_and(|b| b.save_password);
    if !conn.save_password {
        return match had_saved {
            true => PasswordChange::Forget,
            false => PasswordChange::Keep,
        };
    }
    match typed.is_empty() {
        true => PasswordChange::Keep,
        false => PasswordChange::Set(Secret::new(typed)),
    }
}

enum Status {
    None,
    Testing(Busy),
    Connected(String, u128),
    Error(String),
}

pub(in crate::shell) struct DbSheet {
    mode: SheetMode,
    base: Option<Connection>,
    root: Option<PathBuf>,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    database: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    engine: Engine,
    color: Option<String>,
    tls: Tls,
    tls_touched: bool,
    keychain: bool,
    read_only: bool,
    production: bool,
    claude: bool,
    only_here: bool,
    status: Status,
    status_timer: Option<Task<()>>,
    new_id: String,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DbSheetEvent> for DbSheet {}

impl DbSheet {
    pub(in crate::shell) fn new(
        mode: SheetMode,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(placeholder, cx))
        };
        let name = input("Name", cx);
        let host = input("localhost", cx);
        let port = input("Default port", cx);
        let database = input("Database", cx);
        let user = input("User", cx);
        let password = cx.new(|cx| TextInput::new("Password", cx).masked());
        let mut subscriptions = Vec::new();
        for field in [&name, &host, &port, &database, &user, &password] {
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Changed => cx.notify(),
                    InputEvent::Submit => this.submit(cx),
                    InputEvent::Cancel => cx.emit(DbSheetEvent::Close),
                    _ => {}
                },
            ));
        }
        let base = match &mode {
            SheetMode::Edit(listed) => Some(listed.conn.clone()),
            _ => None,
        };
        let mut sheet = Self {
            mode,
            base: base.clone(),
            root,
            name,
            host,
            port,
            database,
            user,
            password,
            engine: Engine::Postgres,
            color: None,
            tls: Tls::Prefer,
            tls_touched: false,
            keychain: true,
            read_only: false,
            production: false,
            claude: false,
            only_here: false,
            status: Status::None,
            status_timer: None,
            new_id: daedalus_db::new_id().unwrap_or_default(),
            _subscriptions: subscriptions,
        };
        if let Some(conn) = &base {
            sheet.fill(conn, cx);
        }
        let first = match sheet.mode {
            SheetMode::Password { .. } => sheet.password.clone(),
            _ => sheet.name.clone(),
        };
        window.focus(&first.focus_handle(cx));
        sheet
    }

    fn fill(&mut self, conn: &Connection, cx: &mut Context<Self>) {
        let set = |input: &Entity<TextInput>, text: &str, cx: &mut Context<Self>| {
            input.update(cx, |i, cx| i.set_text(text, cx));
        };
        set(&self.name, &conn.name, cx);
        set(&self.host, &conn.host, cx);
        set(
            &self.port,
            &conn.port.map(|p| p.to_string()).unwrap_or_default(),
            cx,
        );
        set(&self.database, &conn.database, cx);
        set(&self.user, &conn.user, cx);
        self.engine = conn.engine;
        self.color = conn.color.clone();
        self.tls = conn.tls;
        self.tls_touched = true;
        self.keychain = conn.save_password;
        self.read_only = conn.read_only;
        self.production = conn.production;
        self.claude = conn.claude;
        self.only_here = self
            .root
            .as_ref()
            .is_some_and(|r| conn.projects.contains(r));
    }

    fn fill_from_url(&mut self, conn: &Connection, cx: &mut Context<Self>) {
        let next = url_fields(&self.fields(cx), conn);
        let set = |input: &Entity<TextInput>, text: &str, cx: &mut Context<Self>| {
            input.update(cx, |i, cx| i.set_text(text, cx));
        };
        set(&self.name, &next.name, cx);
        set(&self.host, &next.host, cx);
        set(&self.port, &next.port, cx);
        set(&self.database, &next.database, cx);
        set(&self.user, &next.user, cx);
        self.engine = next.engine;
        self.tls = next.tls;
        self.tls_touched = next.tls_touched;
    }

    fn fields(&self, cx: &App) -> Fields {
        let text = |i: &Entity<TextInput>| i.read(cx).text().to_owned();
        Fields {
            id: self.new_id.clone(),
            name: text(&self.name),
            engine: self.engine,
            color: self.color.clone(),
            host: text(&self.host),
            port: text(&self.port),
            database: text(&self.database),
            user: text(&self.user),
            keychain: self.keychain,
            tls: self.tls,
            tls_touched: self.tls_touched,
            read_only: self.read_only,
            production: self.production,
            claude: self.claude,
            only_here: self.only_here,
        }
    }

    fn built(&self, cx: &App) -> Result<Connection, &'static str> {
        build(&self.fields(cx), self.base.as_ref(), self.root.as_deref())
    }

    fn typed_password(&self, cx: &App) -> String {
        self.password.read(cx).text().to_owned()
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        match &self.mode {
            SheetMode::Password { .. } => self.unlock(cx),
            _ => self.save(cx),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let conn = match self.built(cx) {
            Ok(conn) => conn,
            Err(e) => {
                self.status = Status::Error(e.into());
                cx.notify();
                return;
            }
        };
        let origin = match &self.mode {
            SheetMode::Edit(listed) => listed.origin,
            _ => Origin::Global,
        };
        let password = password_change(self.base.as_ref(), &conn, &self.typed_password(cx));
        cx.emit(DbSheetEvent::Save {
            conn: Box::new(conn),
            origin,
            password,
        });
    }

    fn unlock(&mut self, cx: &mut Context<Self>) {
        let SheetMode::Password { listed, tab } = &self.mode else {
            return;
        };
        let (listed, tab) = (listed.clone(), tab.clone());
        let password = Secret::new(self.typed_password(cx));
        cx.emit(DbSheetEvent::Unlock {
            listed,
            password,
            save: self.keychain,
            tab,
        });
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        let conn = match self.built(cx) {
            Ok(conn) => conn,
            Err(e) => {
                self.status = Status::Error(e.into());
                cx.notify();
                return;
            }
        };
        let typed = self.typed_password(cx);
        let typed = (!typed.is_empty()).then(|| Secret::new(typed));
        let use_saved = match (&self.mode, &typed) {
            (SheetMode::Edit(listed), None) if listed.conn.save_password => Some(listed.clone()),
            _ => None,
        };
        self.status = Status::Testing(Busy::start());
        let delay = cx.theme().motion.indicator_delay;
        self.status_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        cx.emit(DbSheetEvent::Test {
            conn: Box::new(conn),
            typed,
            use_saved,
        });
        cx.notify();
    }

    pub(in crate::shell) fn set_test_result(
        &mut self,
        result: Result<String, String>,
        ms: u128,
        cx: &mut Context<Self>,
    ) {
        self.status_timer = None;
        self.status = match result {
            Ok(server) => Status::Connected(server, ms),
            Err(e) => Status::Error(e),
        };
        cx.notify();
    }

    pub(in crate::shell) fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.status = Status::Error(message);
        cx.notify();
    }

    fn paste_url(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text());
        match daedalus_db::from_url(text.as_deref().unwrap_or_default()) {
            Ok((conn, secret)) => {
                self.fill_from_url(&conn, cx);
                if let Some(secret) = secret {
                    self.password
                        .update(cx, |i, cx| i.set_text(secret.expose(), cx));
                }
                self.status = Status::None;
            }
            Err(e) => self.status = Status::Error(e),
        }
        cx.notify();
    }

    fn cycle(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let mut order = vec![self.name.clone()];
        if self.engine == Engine::Sqlite {
            order.push(self.database.clone());
        } else {
            order.extend([
                self.host.clone(),
                self.port.clone(),
                self.database.clone(),
                self.user.clone(),
                self.password.clone(),
            ]);
        }
        if matches!(self.mode, SheetMode::Password { .. }) {
            order = vec![self.password.clone()];
        }
        let at = order
            .iter()
            .position(|i| i.focus_handle(cx).is_focused(window));
        let next = match at {
            Some(i) => (i as isize + step).rem_euclid(order.len() as isize) as usize,
            None => 0,
        };
        window.focus(&order[next].focus_handle(cx));
    }
}

fn boxed(input: &Entity<TextInput>, t: &Theme) -> AnyElement {
    div()
        .h(t.ui(28.))
        .px(t.ui(8.))
        .flex()
        .items_center()
        .rounded(t.shape.radius_control)
        .border_1()
        .border_color(t.color.border_strong)
        .bg(t.color.surface_sunken)
        .text_size(t.typography.caption)
        .child(div().flex_1().min_w_0().child(input.clone()))
        .into_any_element()
}

fn labelled(label: &'static str, body: impl IntoElement, t: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(t.space(Space::Xs))
        .child(
            div()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(label),
        )
        .child(body)
        .into_any_element()
}

fn checkbox(
    id: &'static str,
    label: String,
    checked: bool,
    t: &Theme,
    on_toggle: impl Fn(&mut DbSheet, &mut Context<DbSheet>) + 'static,
    cx: &mut Context<DbSheet>,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(t.space(Space::Sm))
        .cursor_pointer()
        .text_size(t.typography.caption)
        .text_color(t.color.content_secondary)
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            on_toggle(this, cx);
            cx.notify();
        }))
        .child(
            div()
                .size(t.ui(14.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(t.color.border_strong)
                .when(checked, |el| {
                    el.bg(t.color.accent)
                        .border_color(t.color.accent)
                        .text_color(t.color.content_on_accent)
                        .child("✓")
                }),
        )
        .child(label)
        .into_any_element()
}

fn engine_index(engine: Engine) -> usize {
    match engine {
        Engine::Postgres => 0,
        Engine::Mysql => 1,
        Engine::Sqlite => 2,
    }
}

fn tls_index(tls: Tls) -> usize {
    match tls {
        Tls::Disable => 0,
        Tls::Prefer => 1,
        Tls::Require => 2,
        Tls::RequireUnverified => 3,
    }
}

fn color_name(color: FolderColor) -> Option<String> {
    serde_json::to_value(color)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
}

impl DbSheet {
    fn swatches(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let none = div()
            .id("db-color-none")
            .size(t.ui(16.))
            .rounded_full()
            .border_1()
            .border_color(t.color.border_strong)
            .cursor_pointer()
            .when(self.color.is_none(), |el| {
                el.border_2().border_color(t.color.accent)
            })
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.color = None;
                cx.notify();
            }));
        let mut row = div()
            .flex()
            .items_center()
            .gap(t.space(Space::Sm))
            .child(none);
        for (i, color) in FolderColor::ALL.into_iter().take(6).enumerate() {
            let name = color_name(color);
            let selected = self.color == name;
            row = row.child(
                div()
                    .id(("db-color", i))
                    .size(t.ui(16.))
                    .rounded_full()
                    .bg(folder_color(t, color))
                    .cursor_pointer()
                    .border_2()
                    .border_color(if selected {
                        t.color.content
                    } else {
                        folder_color(t, color)
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.color = name.clone();
                        cx.notify();
                    })),
            );
        }
        labelled("Colour", row, t)
    }

    fn status_line(&self, t: &Theme) -> Option<AnyElement> {
        let (text, color) = match &self.status {
            Status::None => return None,
            Status::Testing(busy) if busy.shows(t.motion.indicator_delay) => {
                ("Testing…".to_owned(), t.color.content_muted)
            }
            Status::Testing(_) => return None,
            Status::Connected(server, ms) => {
                (format!("Connected · {server} · {ms} ms"), t.color.success)
            }
            Status::Error(e) => (e.clone(), t.color.danger),
        };
        Some(
            div()
                .text_size(t.typography.caption)
                .text_color(color)
                .child(text)
                .into_any_element(),
        )
    }

    fn render_password(
        &mut self,
        listed: &Listed,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let keychain = self.keychain;
        div()
            .flex()
            .flex_col()
            .gap(t.space(Space::Md))
            .p(t.space(Space::Lg))
            .child(
                div()
                    .text_size(t.typography.body)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content)
                    .child(format!("Password for {}", listed.conn.name)),
            )
            .child(boxed(&self.password, t))
            .child(checkbox(
                "db-keychain",
                "Save in Keychain".into(),
                keychain,
                t,
                |this, _| this.keychain = !this.keychain,
                cx,
            ))
            .children(self.status_line(t))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(t.space(Space::Sm))
                    .child(
                        Button::new("db-pw-cancel", "Cancel", ButtonKind::Ghost)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DbSheetEvent::Close))),
                    )
                    .child(
                        Button::new("db-pw-connect", "Connect", ButtonKind::Primary)
                            .on_click(cx.listener(|this, _, _, cx| this.unlock(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_form(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let sqlite = self.engine == Engine::Sqlite;
        let built = self.built(cx);
        let editing = matches!(self.mode, SheetMode::Edit(_));
        let title = match editing {
            true => "Edit connection",
            false => "New connection",
        };
        let project_name = self.root.as_ref().map(|r| {
            r.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| r.display().to_string())
        });
        let mut form = div()
            .flex()
            .flex_col()
            .gap(t.space(Space::Md))
            .child(
                div()
                    .text_size(t.typography.body)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content)
                    .child(title),
            )
            .child(labelled("Name", boxed(&self.name, t), t))
            .child(labelled(
                "Driver",
                SegmentedControl::new(
                    "db-engine",
                    ["PostgreSQL", "MySQL", "SQLite"],
                    engine_index(self.engine),
                )
                .on_select({
                    let sheet = cx.entity();
                    move |i, _, cx| {
                        let engine = match i {
                            0 => Engine::Postgres,
                            1 => Engine::Mysql,
                            _ => Engine::Sqlite,
                        };
                        sheet.update(cx, |this, cx| {
                            this.engine = engine;
                            cx.notify();
                        });
                    }
                }),
                t,
            ))
            .child(self.swatches(t, cx));
        if sqlite {
            form = form.child(labelled("File", boxed(&self.database, t), t));
        } else {
            form = form
                .child(
                    div()
                        .flex()
                        .gap(t.space(Space::Md))
                        .child(
                            div()
                                .flex_1()
                                .child(labelled("Host", boxed(&self.host, t), t)),
                        )
                        .child(div().w(t.ui(120.)).child(labelled(
                            "Port",
                            boxed(&self.port, t),
                            t,
                        ))),
                )
                .child(labelled("Database", boxed(&self.database, t), t))
                .child(labelled("User", boxed(&self.user, t), t))
                .child(labelled("Password", boxed(&self.password, t), t))
                .child(checkbox(
                    "db-keychain",
                    "Save in Keychain".into(),
                    self.keychain,
                    t,
                    |this, _| this.keychain = !this.keychain,
                    cx,
                ))
                .child(labelled(
                    "TLS",
                    SegmentedControl::new(
                        "db-tls",
                        ["Off", "Prefer", "Verify", "Unverified"],
                        match built.as_ref() {
                            Ok(conn) => tls_index(conn.tls),
                            Err(_) => tls_index(self.tls),
                        },
                    )
                    .on_select({
                        let sheet = cx.entity();
                        move |i, _, cx| {
                            let tls = [
                                Tls::Disable,
                                Tls::Prefer,
                                Tls::Require,
                                Tls::RequireUnverified,
                            ][i.min(3)];
                            sheet.update(cx, |this, cx| {
                                this.tls = tls;
                                this.tls_touched = true;
                                cx.notify();
                            });
                        }
                    }),
                    t,
                ));
        }
        form = form.child(
            div()
                .flex()
                .flex_wrap()
                .gap_x(t.space(Space::Lg))
                .gap_y(t.space(Space::Sm))
                .child(checkbox(
                    "db-read-only",
                    "Read-only".into(),
                    self.read_only,
                    t,
                    |this, _| this.read_only = !this.read_only,
                    cx,
                ))
                .child(checkbox(
                    "db-production",
                    "Production".into(),
                    self.production,
                    t,
                    |this, _| this.production = !this.production,
                    cx,
                ))
                .child(checkbox(
                    "db-claude",
                    "Claude can read".into(),
                    self.claude,
                    t,
                    |this, _| this.claude = !this.claude,
                    cx,
                ))
                .children(project_name.map(|name| {
                    checkbox(
                        "db-only-here",
                        format!("Only in {name}"),
                        self.only_here,
                        t,
                        |this, _| this.only_here = !this.only_here,
                        cx,
                    )
                })),
        );
        if self.claude {
            form = form.child(
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("Claude can list tables and run read-only queries. Use a read-only database user."),
            );
        }
        if let Ok(conn) = &built {
            form = form.child(
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(daedalus_db::redacted_url(conn)),
            );
        }
        form = form.children(self.status_line(t));
        let saveable = built.is_ok();
        let delete = match &self.mode {
            SheetMode::Edit(listed) => {
                let listed = listed.clone();
                Some(
                    Button::new("db-delete", "Delete", ButtonKind::Danger).on_click(cx.listener(
                        move |_, _, _, cx| cx.emit(DbSheetEvent::Delete(listed.clone())),
                    )),
                )
            }
            _ => None,
        };
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().children(delete))
            .child(
                div()
                    .flex()
                    .gap(t.space(Space::Sm))
                    .child(
                        Button::new("db-paste", "Paste URL", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| this.paste_url(cx))),
                    )
                    .child(
                        Button::new("db-test", "Test", ButtonKind::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                    )
                    .child(
                        Button::new("db-cancel", "Cancel", ButtonKind::Ghost)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DbSheetEvent::Close))),
                    )
                    .child(div().when(!saveable, |el| el.opacity(0.4)).child(
                        Button::new("db-save", "Save", ButtonKind::Primary).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if saveable {
                                    this.save(cx);
                                }
                            },
                        )),
                    )),
            );
        div()
            .flex()
            .flex_col()
            .gap(t.space(Space::Md))
            .p(t.space(Space::Lg))
            .child(form)
            .child(footer)
            .into_any_element()
    }
}

impl Render for DbSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let body = match &self.mode {
            SheetMode::Password { listed, .. } => {
                let listed = listed.clone();
                self.render_password(&listed, &t, cx)
            }
            _ => self.render_form(&t, cx),
        };
        div()
            .id("db-sheet")
            .key_context("DbSheet")
            .on_action(cx.listener(|this, _: &NextField, window, cx| this.cycle(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevField, window, cx| this.cycle(-1, window, cx)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w_full()
            .max_h(t.ui(640.))
            .overflow_y_scroll()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Fields {
        Fields {
            id: String::new(),
            name: "Local".into(),
            engine: Engine::Postgres,
            color: None,
            host: String::new(),
            port: String::new(),
            database: "app".into(),
            user: "me".into(),
            keychain: true,
            tls: Tls::Disable,
            tls_touched: false,
            read_only: false,
            production: false,
            claude: false,
            only_here: false,
        }
    }

    fn base() -> Connection {
        let mut c = build(&fields(), None, None).unwrap();
        c.id = "fixed".into();
        c.claude = true;
        c.ca_file = Some("/tmp/ca.pem".into());
        c
    }

    #[test]
    fn build_requires_a_name() {
        let f = Fields {
            name: "  ".into(),
            ..fields()
        };
        assert_eq!(build(&f, None, None).unwrap_err(), "Name is required");
    }

    #[test]
    fn build_requires_a_file_for_sqlite() {
        let f = Fields {
            engine: Engine::Sqlite,
            database: " ".into(),
            ..fields()
        };
        assert_eq!(build(&f, None, None).unwrap_err(), "File is required");
    }

    #[test]
    fn build_rejects_port_zero_and_text() {
        for port in ["0", "abc", "70000"] {
            let f = Fields {
                port: port.into(),
                ..fields()
            };
            assert_eq!(
                build(&f, None, None).unwrap_err(),
                "Port must be 1-65535",
                "{port}"
            );
        }
        let f = Fields {
            port: "5433".into(),
            ..fields()
        };
        assert_eq!(build(&f, None, None).unwrap().port, Some(5433));
    }

    #[test]
    fn build_keeps_id_from_the_base() {
        let b = base();
        let built = build(&fields(), Some(&b), None).unwrap();
        assert_eq!(built.id, "fixed");
        assert_eq!(built.ca_file, b.ca_file);
        assert_eq!(built.host, "localhost");
    }

    #[test]
    fn build_takes_claude_from_the_form() {
        let b = base();
        assert!(b.claude);
        assert!(!build(&fields(), Some(&b), None).unwrap().claude);
        let on = Fields {
            claude: true,
            ..fields()
        };
        let mut off = base();
        off.claude = false;
        assert!(build(&on, Some(&off), None).unwrap().claude);
    }

    #[test]
    fn asks_for_claude_on_production_only_when_newly_both() {
        let mut new = base();
        new.production = true;
        let mut prod_only = new.clone();
        prod_only.claude = false;
        let mut claude_only = new.clone();
        claude_only.production = false;
        assert!(asks_for_claude_on_production(None, &new));
        assert!(asks_for_claude_on_production(Some(&prod_only), &new));
        assert!(asks_for_claude_on_production(Some(&claude_only), &new));
        assert!(!asks_for_claude_on_production(Some(&new), &new));
        assert!(!asks_for_claude_on_production(None, &prod_only));
    }

    #[test]
    fn build_never_saves_password_for_sqlite() {
        let f = Fields {
            engine: Engine::Sqlite,
            database: "/tmp/x.db".into(),
            host: "ignored".into(),
            ..fields()
        };
        let built = build(&f, None, None).unwrap();
        assert!(!built.save_password);
        assert!(built.host.is_empty() && built.user.is_empty());
    }

    #[test]
    fn untouched_tls_follows_the_host_default() {
        let remote = Fields {
            host: "db.example.com".into(),
            ..fields()
        };
        assert_eq!(build(&remote, None, None).unwrap().tls, Tls::Require);
        let local = Fields {
            host: "localhost".into(),
            ..fields()
        };
        assert_eq!(build(&local, None, None).unwrap().tls, Tls::Prefer);
        let touched = Fields {
            tls_touched: true,
            ..remote
        };
        assert_eq!(build(&touched, None, None).unwrap().tls, Tls::Disable);
    }

    #[test]
    fn one_new_sheet_builds_the_same_id_twice() {
        let f = Fields {
            id: "once".into(),
            ..fields()
        };
        let a = build(&f, None, None).unwrap();
        let b = build(&f, None, None).unwrap();
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn paste_url_keeps_the_keychain_tick_and_production() {
        let (url_conn, secret) = daedalus_db::from_url("postgres://u:pw@db.example.com/x").unwrap();
        let current = Fields {
            color: Some("red".into()),
            read_only: true,
            production: true,
            only_here: true,
            ..fields()
        };
        let next = url_fields(&current, &url_conn);
        assert_eq!(next.host, "db.example.com");
        assert_eq!(next.database, "x");
        assert!(next.keychain && next.production && next.read_only && next.only_here);
        assert_eq!(next.color.as_deref(), Some("red"));
        assert_eq!(next.name, "Local");
        let blank = Fields {
            name: " ".into(),
            ..current
        };
        assert_eq!(url_fields(&blank, &url_conn).name, url_conn.name);
        let edited = base();
        let built = build(&next, Some(&edited), None).unwrap();
        assert!(built.save_password && built.production);
        let typed = secret.as_ref().map_or("", |s| s.expose());
        assert!(matches!(
            password_change(Some(&edited), &built, typed),
            PasswordChange::Set(_)
        ));
        assert!(matches!(
            password_change(Some(&edited), &built, ""),
            PasswordChange::Keep
        ));
    }

    #[test]
    fn needs_reconnect_ignores_cosmetic_fields() {
        let old = base();
        let mut new = old.clone();
        new.name = "Other".into();
        new.color = Some("red".into());
        new.production = true;
        new.claude = false;
        new.projects = vec!["/p".into()];
        new.save_password = !old.save_password;
        assert!(!needs_reconnect(&old, &new));
    }

    #[test]
    fn needs_reconnect_sees_host_and_tls_changes() {
        let old = base();
        let mut host = old.clone();
        host.host = "elsewhere".into();
        assert!(needs_reconnect(&old, &host));
        let mut tls = old.clone();
        tls.tls = Tls::Require;
        assert!(needs_reconnect(&old, &tls));
        let mut ro = old.clone();
        ro.read_only = true;
        assert!(needs_reconnect(&old, &ro));
    }

    #[test]
    fn an_empty_password_in_edit_keeps_the_saved_one() {
        let b = base();
        assert!(matches!(
            password_change(Some(&b), &b, ""),
            PasswordChange::Keep
        ));
        assert!(matches!(
            password_change(Some(&b), &b, "new"),
            PasswordChange::Set(_)
        ));
        let mut off = b.clone();
        off.save_password = false;
        assert!(matches!(
            password_change(Some(&b), &off, "new"),
            PasswordChange::Forget
        ));
        assert!(matches!(
            password_change(None, &off, "new"),
            PasswordChange::Keep
        ));
    }
}
