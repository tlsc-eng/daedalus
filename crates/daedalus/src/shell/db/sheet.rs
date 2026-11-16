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
        root: Option<PathBuf>,
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

/// Whether two connections reach the same server as the same user, so a saved password still belongs to the target.
pub(in crate::shell) fn same_target(a: &Connection, b: &Connection) -> bool {
    (&a.engine, &a.host, &a.port, &a.database, &a.user)
        == (&b.engine, &b.host, &b.port, &b.database, &b.user)
        && a.tls == b.tls
        && a.ca_file == b.ca_file
        && a.ssh == b.ssh
}

/// True when an edit keeps the saved password but points the connection at a different target.
pub(in crate::shell) fn needs_typed_password(
    old: Option<&Connection>,
    new: &Connection,
    password: &PasswordChange,
) -> bool {
    matches!(password, PasswordChange::Keep)
        && new.save_password
        && old.is_some_and(|o| o.save_password && !same_target(o, new))
}

/// The saved row a Test may borrow a password from; project rows use the form's own values.
pub(super) fn saved_for_test(
    editing: Option<&Listed>,
    built: &Connection,
    typed: bool,
) -> Option<Listed> {
    let l = editing.filter(|_| !typed)?;
    if l.origin == Origin::Project {
        return Some(Listed {
            conn: built.clone(),
            origin: Origin::Project,
        });
    }
    (l.conn.save_password && same_target(&l.conn, built)).then(|| l.clone())
}

/// Claude access and the personal-only scope make no sense in a file the team commits.
fn target_fields(f: Fields, target: Origin) -> Fields {
    match target {
        Origin::Global => f,
        Origin::Project => Fields {
            claude: false,
            only_here: false,
            ..f
        },
    }
}

/// Whether an open session no longer matches the edited connection.
pub(in crate::shell) fn needs_reconnect(old: &Connection, new: &Connection) -> bool {
    (&old.engine, &old.host, &old.port, &old.database, &old.user)
        != (&new.engine, &new.host, &new.port, &new.database, &new.user)
        || old.tls != new.tls
        || old.ca_file != new.ca_file
        || old.read_only != new.read_only
        || old.ssh != new.ssh
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

/// A connection found in project files, offered to a New sheet.
pub(in crate::shell) struct Found {
    pub conn: Connection,
    pub password: Option<Secret>,
    pub note: String,
    pub password_note: String,
}

/// The password a Save or Test uses: what was typed, else a found one the user chose for its own target.
fn effective_password(
    typed: &str,
    found: Option<&Found>,
    use_found: bool,
    built: &Connection,
) -> String {
    if !typed.is_empty() {
        return typed.to_owned();
    }
    match found {
        Some(f) if use_found && same_target(&f.conn, built) => f
            .password
            .as_ref()
            .map(|p| p.expose().to_owned())
            .unwrap_or_default(),
        _ => String::new(),
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
    target: Origin,
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
    found: Option<Found>,
    use_found: bool,
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
        let target = match &mode {
            SheetMode::Edit(listed) => listed.origin,
            _ => Origin::Global,
        };
        let mut sheet = Self {
            mode,
            base: base.clone(),
            root,
            target,
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
            found: None,
            use_found: false,
            status: Status::None,
            status_timer: None,
            new_id: daedalus_db::new_id().unwrap_or_default(),
            _subscriptions: subscriptions,
        };
        if let Some(conn) = &base {
            sheet.fill(conn, cx);
            if target == Origin::Project {
                sheet.keychain = true;
            }
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

    pub(in crate::shell) fn prefill(&mut self, found: Found, cx: &mut Context<Self>) {
        self.fill(&found.conn, cx);
        let name = caption_field(&found.conn.name);
        self.name.update(cx, |i, cx| i.set_text(&name, cx));
        self.keychain = true;
        self.found = Some(found);
        self.use_found = false;
        cx.notify();
    }

    fn password_for(&self, built: &Connection, cx: &App) -> String {
        effective_password(
            &self.typed_password(cx),
            self.found.as_ref(),
            self.use_found,
            built,
        )
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
        target_fields(
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
            },
            self.target,
        )
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
        let password = password_change(self.base.as_ref(), &conn, &self.password_for(&conn, cx));
        cx.emit(DbSheetEvent::Save {
            conn: Box::new(conn),
            origin: self.target,
            password,
            root: self.root.clone(),
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
        let typed = self.password_for(&conn, cx);
        let typed = (!typed.is_empty()).then(|| Secret::new(typed));
        let editing = match &self.mode {
            SheetMode::Edit(listed) => Some(listed),
            _ => None,
        };
        let use_saved = saved_for_test(editing, &conn, typed.is_some());
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

    pub(in crate::shell) fn password_target(&self) -> Option<&Listed> {
        match &self.mode {
            SheetMode::Password { listed, .. } => Some(listed),
            _ => None,
        }
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

fn tls_label(tls: Tls) -> &'static str {
    ["Off", "Prefer", "Verify", "Unverified"][tls_index(tls)]
}

/// Where a project row's password will go, shown before the user types it.
pub(super) fn prompt_detail(listed: &Listed) -> Option<String> {
    if listed.origin == Origin::Global {
        return None;
    }
    let conn = &listed.conn;
    let shown = Connection {
        user: caption_field(&conn.user),
        host: caption_field(&conn.host),
        database: caption_field(&conn.database),
        ..conn.clone()
    };
    let mut out = format!(
        "{} · TLS {}",
        daedalus_db::redacted_url(&shown),
        tls_label(conn.tls)
    );
    if let Some(ca) = &conn.ca_file {
        out.push_str(&format!(
            " · CA {}",
            caption_field(&ca.display().to_string())
        ));
    }
    if let Some(ssh) = &conn.ssh {
        out.push_str(" · via ssh ");
        if !ssh.user.is_empty() {
            out.push_str(&format!("{}@", caption_field(&ssh.user)));
        }
        out.push_str(&caption_field(&ssh.host));
        if let Some(port) = ssh.port {
            out.push_str(&format!(":{port}"));
        }
    }
    Some(out)
}

/// Makes a value from a repo file safe to show: no control, bidi or invisible characters, bounded length.
pub(super) fn caption_field(s: &str) -> String {
    const MAX: usize = 64;
    let mut out: String = s
        .chars()
        .take(MAX)
        .map(|c| {
            let hidden = matches!(
                c,
                '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
            );
            if c.is_control() || hidden { '\u{FFFD}' } else { c }
        })
        .collect();
    if s.chars().nth(MAX).is_some() {
        out.push('…');
    }
    out
}

/// Whether the host is this machine or a file path, where an unencrypted password stays local.
fn is_local_host(conn: &Connection) -> bool {
    conn.engine == Engine::Sqlite
        || conn.host.is_empty()
        || conn.host.starts_with('/')
        || matches!(conn.host.as_str(), "localhost" | "127.0.0.1" | "::1")
}

/// Whether the password can cross a network without TLS to a non-local server.
fn tls_warning(conn: &Connection) -> Option<&'static str> {
    if conn.ssh.is_some() {
        return None;
    }
    match conn.tls {
        Tls::Disable => Some("TLS is off, so the password may cross the network unencrypted."),
        Tls::Prefer if !is_local_host(conn) => {
            Some("TLS is optional, so the password may be sent unencrypted.")
        }
        _ => None,
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
        let detail = prompt_detail(listed);
        let caption = |text: String, color| {
            div()
                .text_size(t.typography.caption)
                .text_color(color)
                .child(text)
        };
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
            .when_some(detail, |el, detail| {
                el.child(caption(detail, t.color.content_secondary))
                    .child(caption(
                        "From this project's .daedalus/databases.json. Check the server before you type a password.".into(),
                        t.color.content_muted,
                    ))
                    .when_some(tls_warning(&listed.conn), |el, warning| {
                        el.child(caption(warning.into(), t.color.warning))
                    })
            })
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
            .children(self.found.as_ref().map(|f| {
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(f.note.clone())
            }))
            .children((!editing && self.root.is_some()).then(|| {
                labelled(
                    "Save to",
                    SegmentedControl::new(
                        "db-target",
                        ["My connections", "This project (shared)"],
                        usize::from(self.target == Origin::Project),
                    )
                    .on_select({
                        let sheet = cx.entity();
                        move |i, _, cx| {
                            sheet.update(cx, |this, cx| {
                                this.target = match i {
                                    0 => Origin::Global,
                                    _ => Origin::Project,
                                };
                                cx.notify();
                            });
                        }
                    }),
                    t,
                )
            }))
            .children((editing && self.target == Origin::Project).then(|| {
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("In this project's .daedalus/databases.json")
            }))
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
                .children(
                    self.found
                        .as_ref()
                        .filter(|f| f.password.is_some())
                        .map(|f| {
                            div()
                                .text_size(t.typography.caption)
                                .text_color(t.color.content_muted)
                                .child(f.password_note.clone())
                        }),
                )
                .children(
                    self.found
                        .as_ref()
                        .filter(|f| f.password.is_some())
                        .map(|_| {
                            checkbox(
                                "db-use-found",
                                "Use it".into(),
                                self.use_found,
                                t,
                                |this, _| this.use_found = !this.use_found,
                                cx,
                            )
                        }),
                )
                .children(
                    self.found
                        .as_ref()
                        .filter(|f| self.use_found && f.password.is_some())
                        .zip(built.as_ref().ok())
                        .filter(|(f, conn)| !same_target(&f.conn, conn))
                        .map(|_| {
                            div()
                                .text_size(t.typography.caption)
                                .text_color(t.color.warning)
                                .child("The target changed, so the found password is not used.")
                        }),
                )
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
                .children((self.target == Origin::Global).then(|| {
                    checkbox(
                        "db-claude",
                        "Claude can read".into(),
                        self.claude,
                        t,
                        |this, _| this.claude = !this.claude,
                        cx,
                    )
                }))
                .children(
                    project_name
                        .filter(|_| self.target == Origin::Global)
                        .map(|name| {
                            checkbox(
                                "db-only-here",
                                format!("Only in {name}"),
                                self.only_here,
                                t,
                                |this, _| this.only_here = !this.only_here,
                                cx,
                            )
                        }),
                ),
        );
        if self.claude && self.target == Origin::Global {
            form = form.child(
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("Claude can list tables and run read-only queries. Use a read-only database user."),
            );
        }
        if self.target == Origin::Project {
            form = form.child(
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("Saved in .daedalus/databases.json, which your team can commit. Your password stays in your Keychain."),
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
        let mut ssh = old.clone();
        ssh.ssh = Some(daedalus_db::SshTunnel {
            host: "bastion".into(),
            port: None,
            user: String::new(),
            identity_file: None,
        });
        assert!(needs_reconnect(&old, &ssh));
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

    #[test]
    fn prompt_detail_names_host_tls_and_ca_for_project_rows() {
        let listed = Listed {
            conn: base(),
            origin: Origin::Project,
        };
        let detail = prompt_detail(&listed).unwrap();
        assert!(detail.contains(&daedalus_db::redacted_url(&listed.conn)));
        assert!(detail.contains("TLS "));
        assert!(detail.ends_with(" · CA /tmp/ca.pem"));
    }

    #[test]
    fn prompt_detail_shows_the_ssh_hop() {
        let mut conn = base();
        conn.host = "localhost".into();
        conn.ssh = Some(daedalus_db::SshTunnel {
            host: "attacker.example".into(),
            port: Some(2222),
            user: "root".into(),
            identity_file: None,
        });
        let detail = prompt_detail(&Listed {
            conn,
            origin: Origin::Project,
        })
        .unwrap();
        assert!(detail.contains("via ssh root@attacker.example:2222"));
    }

    #[test]
    fn caption_field_strips_controls_and_cuts_long_values() {
        assert_eq!(caption_field("db.example"), "db.example");
        assert_eq!(caption_field("a\nb"), "a\u{FFFD}b");
        let cut = caption_field(&"x".repeat(100));
        assert_eq!(cut.chars().count(), 65);
        assert!(cut.ends_with('…'));
        assert_eq!(caption_field(&"y".repeat(64)).chars().count(), 64);
        assert_eq!(caption_field("a\u{202E}b\u{200B}c"), "a\u{FFFD}b\u{FFFD}c");
    }

    fn found_with(password: &str) -> Found {
        Found {
            conn: base(),
            password: Some(Secret::new(password)),
            note: String::new(),
            password_note: String::new(),
        }
    }

    #[test]
    fn typed_password_wins_over_a_found_one() {
        let found = found_with("from-env");
        let out = effective_password("typed", Some(&found), true, &base());
        assert_eq!(out, "typed");
    }

    #[test]
    fn found_password_needs_the_tick() {
        let found = found_with("from-env");
        assert_eq!(effective_password("", Some(&found), false, &base()), "");
    }

    #[test]
    fn found_password_is_dropped_when_the_target_changes() {
        let found = found_with("from-env");
        let mut other_host = base();
        other_host.host = "elsewhere.example".into();
        assert_eq!(effective_password("", Some(&found), true, &other_host), "");
        let mut other_tls = base();
        other_tls.tls = Tls::Require;
        assert_eq!(effective_password("", Some(&found), true, &other_tls), "");
    }

    #[test]
    fn found_password_is_used_for_its_own_target() {
        let found = found_with("from-env");
        assert_eq!(
            effective_password("", Some(&found), true, &base()),
            "from-env"
        );
    }

    #[test]
    fn tls_warning_covers_prefer_on_remote_hosts_only() {
        let mut conn = base();
        conn.tls = Tls::Prefer;
        conn.host = "db.example".into();
        assert!(tls_warning(&conn).is_some());
        conn.host = "localhost".into();
        assert!(tls_warning(&conn).is_none());
        conn.host = "db.example".into();
        conn.ssh = Some(daedalus_db::SshTunnel {
            host: "bastion".into(),
            port: None,
            user: String::new(),
            identity_file: None,
        });
        assert!(tls_warning(&conn).is_none());
        conn.ssh = None;
        conn.tls = Tls::Disable;
        assert!(tls_warning(&conn).is_some());
    }

    #[test]
    fn prompt_detail_is_none_for_personal_rows() {
        let listed = Listed {
            conn: base(),
            origin: Origin::Global,
        };
        assert_eq!(prompt_detail(&listed), None);
    }

    fn listed(conn: Connection, origin: Origin) -> Listed {
        Listed { conn, origin }
    }

    #[test]
    fn project_target_drops_claude_and_only_here() {
        let f = Fields {
            claude: true,
            only_here: true,
            ..fields()
        };
        let kept = target_fields(f.clone(), Origin::Global);
        assert!(kept.claude && kept.only_here);
        let out = target_fields(f, Origin::Project);
        let conn = build(&out, None, Some(Path::new("/a"))).unwrap();
        assert!(!conn.claude && conn.projects.is_empty());
    }

    #[test]
    fn saved_for_test_uses_the_form_values_for_project_rows() {
        let saved = listed(base(), Origin::Project);
        let mut edited = base();
        edited.host = "other.example".into();
        let out = saved_for_test(Some(&saved), &edited, false).unwrap();
        assert_eq!(out.origin, Origin::Project);
        assert_ne!(
            daedalus_db::keychain_account(out.origin, Some(Path::new("/a")), &out.conn),
            daedalus_db::keychain_account(saved.origin, Some(Path::new("/a")), &saved.conn)
        );
    }

    #[test]
    fn saved_for_test_keeps_personal_rows_as_before() {
        let mut conn = base();
        conn.save_password = true;
        let saved = listed(conn.clone(), Origin::Global);
        let out = saved_for_test(Some(&saved), &conn, false).unwrap();
        assert_eq!(out.conn.id, conn.id);
        conn.save_password = false;
        assert!(
            saved_for_test(Some(&listed(conn.clone(), Origin::Global)), &conn, false).is_none()
        );
    }

    #[test]
    fn saved_for_test_never_borrows_when_a_password_is_typed() {
        let mut conn = base();
        conn.save_password = true;
        assert!(saved_for_test(Some(&listed(conn.clone(), Origin::Global)), &conn, true).is_none());
        assert!(
            saved_for_test(Some(&listed(conn.clone(), Origin::Project)), &conn, true).is_none()
        );
    }

    #[test]
    fn test_does_not_borrow_a_password_for_a_new_host() {
        let mut saved = base();
        saved.save_password = true;
        let mut edited = saved.clone();
        edited.host = "attacker.example".into();
        assert!(saved_for_test(Some(&listed(saved, Origin::Global)), &edited, false).is_none());
    }

    #[test]
    fn edit_to_new_host_needs_a_typed_password() {
        let mut old = base();
        old.save_password = true;
        let mut new = old.clone();
        new.host = "attacker.example".into();
        assert!(needs_typed_password(
            Some(&old),
            &new,
            &PasswordChange::Keep
        ));
        let typed = PasswordChange::Set(Secret::new("pw"));
        assert!(!needs_typed_password(Some(&old), &new, &typed));
        assert!(!needs_typed_password(
            Some(&old),
            &old,
            &PasswordChange::Keep
        ));
        assert!(!needs_typed_password(None, &new, &PasswordChange::Keep));
    }
}
