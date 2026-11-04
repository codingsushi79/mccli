//! Modal forms. They only collect text; the host validates and any refusal
//! comes back into the open form, so nothing typed has to be typed again.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme;
use super::widgets::centered;
use crate::proto::*;

pub struct Field {
    pub label: &'static str,
    pub value: String,
    /// Byte offset, always on a char boundary.
    cursor: usize,
    pub hint: &'static str,
    pub required: bool,
    pub secret: bool,
}

impl Field {
    pub fn new(label: &'static str, hint: &'static str, required: bool) -> Self {
        Self {
            label,
            value: String::new(),
            cursor: 0,
            hint,
            required,
            secret: false,
        }
    }

    pub fn with(label: &'static str, hint: &'static str, value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label,
            cursor: value.len(),
            value,
            hint,
            required: false,
            secret: false,
        }
    }

    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    fn display(&self) -> String {
        if self.secret {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }

    fn split_at_cursor(&self) -> (String, String) {
        let before = self.value[..self.cursor].chars().count();
        let shown = self.display();
        let split = shown
            .char_indices()
            .nth(before)
            .map(|(i, _)| i)
            .unwrap_or(shown.len());
        (shown[..split].to_string(), shown[split..].to_string())
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.value[..self.cursor]
            .chars()
            .next_back()
            .map(|c| self.cursor - c.len_utf8())
    }

    fn next_boundary(&self) -> usize {
        match self.value[self.cursor..].chars().next() {
            Some(c) => self.cursor + c.len_utf8(),
            None => self.cursor,
        }
    }

    pub fn set(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
    }
}

/// What the form is for, with whatever context it needs to build a request.
#[derive(Clone)]
pub enum Kind {
    Login,
    CreateServer,
    DeleteServer { server: String },
    Settings { server: String, base: ServerSettings },
    Reinstall { server: String },
    Property { server: String, key: Option<String> },
    PlayerAdd { server: String },
    PlayerText { server: String, name: String, action: PlayerAction },
    NewFile { server: String, dir: String },
    NewDir { server: String, dir: String },
    Rename { server: String, from: String },
    Backup { server: String },
    Schedule,
    AddUser,
    Password { username: String },
    Role { username: String },
}

pub struct Form {
    pub kind: Kind,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    pub busy: bool,
}

fn yes(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "on" | "1" => Ok(true),
        "n" | "no" | "false" | "off" | "0" | "" => Ok(false),
        other => Err(format!("`{other}` — answer yes or no")),
    }
}

fn yn(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn number<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .trim()
        .parse::<T>()
        .map_err(|_| format!("`{label}` should be a number, got `{}`", value.trim()))
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "all" && s != "*")
        .collect()
}

impl Form {
    fn new(kind: Kind, title: impl Into<String>, fields: Vec<Field>) -> Form {
        Form {
            kind,
            title: title.into(),
            fields,
            focus: 0,
            error: None,
            busy: false,
        }
    }

    pub fn login(address: &str, username: &str) -> Form {
        let mut form = Form::new(
            Kind::Login,
            "Connect to a host",
            vec![
                Field::with("address", "ip, hostname or url — port defaults to 25580", address).required(),
                Field::with("username", "your panel account", username).required(),
                Field::new("password", "", true).secret(),
            ],
        );
        form.focus = if address.is_empty() {
            0
        } else if username.is_empty() {
            1
        } else {
            2
        };
        form
    }

    pub fn create_server(next_port: u16) -> Form {
        Form::new(
            Kind::CreateServer,
            "Create server",
            vec![
                Field::new("name", "lowercase, e.g. survival", true),
                Field::with("type", "paper, vanilla, purpur, fabric or custom", "paper"),
                Field::with("version", "latest, or e.g. 1.21.4", "latest"),
                Field::with("memory MiB", "max heap, e.g. 4096", "4096"),
                Field::with("port", "the port players join on", next_port.to_string()),
                Field::with("java", "java binary to run", "java"),
                Field::new("accept EULA", "yes — https://aka.ms/MinecraftEULA", true),
            ],
        )
    }

    pub fn delete_server(server: &str) -> Form {
        Form::new(
            Kind::DeleteServer { server: server.into() },
            format!("Delete `{server}`"),
            vec![
                Field::new("confirm", "type the server name to confirm", true),
                Field::with("delete files", "yes also deletes the world and backups", "no"),
            ],
        )
    }

    pub fn settings(server: &str, base: ServerSettings) -> Form {
        let fields = vec![
            Field::with("memory MiB", "max heap (-Xmx)", base.memory_max_mb.to_string()),
            Field::with("min memory", "initial heap (-Xms)", base.memory_min_mb.to_string()),
            Field::with("port", "server-port", base.port.to_string()),
            Field::with("auto start", "start when the host starts", yn(base.auto_start)),
            Field::with("auto restart", "restart after a crash", yn(base.auto_restart)),
            Field::with("stop timeout", "seconds to wait before killing", base.stop_timeout_secs.to_string()),
            Field::with("java", "admin only", base.java.clone()),
            Field::with("jar", "admin only — file in the server folder", base.jar.clone()),
            Field::with("jvm args", "admin only, e.g. -XX:+UseG1GC", base.jvm_args.clone()),
        ];
        Form::new(
            Kind::Settings { server: server.into(), base },
            format!("Settings — {server}"),
            fields,
        )
    }

    pub fn reinstall(server: &str, kind: ServerKind, version: &str) -> Form {
        Form::new(
            Kind::Reinstall { server: server.into() },
            format!("Change version — {server}"),
            vec![
                Field::with("type", "paper, vanilla, purpur or fabric", kind.to_string()),
                Field::with("version", "latest, or e.g. 1.21.4", version),
            ],
        )
    }

    pub fn property(server: &str, key: Option<&str>, value: &str) -> Form {
        let mut fields = Vec::new();
        if key.is_none() {
            fields.push(Field::new("key", "e.g. view-distance", true));
        }
        fields.push(Field::with("value", "", value));
        Form::new(
            Kind::Property {
                server: server.into(),
                key: key.map(str::to_string),
            },
            match key {
                Some(k) => format!("Set {k}"),
                None => "Add property".into(),
            },
            fields,
        )
    }

    pub fn player_add(server: &str) -> Form {
        Form::new(
            Kind::PlayerAdd { server: server.into() },
            "Player by name",
            vec![
                Field::new("player", "exact Minecraft username", true),
                Field::with("action", "whitelist, op, ban, pardon, deop, unwhitelist", "whitelist"),
            ],
        )
    }

    pub fn player_text(server: &str, name: &str, action: PlayerAction) -> Form {
        let (title, label, hint, default) = match action {
            PlayerAction::Kick => (format!("Kick {name}"), "reason", "optional", ""),
            PlayerAction::Ban => (format!("Ban {name}"), "reason", "optional", ""),
            PlayerAction::Message => (format!("Message {name}"), "message", "sent with /tell", ""),
            _ => (format!("Game mode — {name}"), "mode", "survival, creative, adventure, spectator", "survival"),
        };
        Form::new(
            Kind::PlayerText {
                server: server.into(),
                name: name.into(),
                action,
            },
            title,
            vec![Field::with(label, hint, default)],
        )
    }

    pub fn new_file(server: &str, dir: &str) -> Form {
        Form::new(
            Kind::NewFile { server: server.into(), dir: dir.into() },
            "New file",
            vec![Field::new("name", "e.g. config.yml", true)],
        )
    }

    pub fn new_dir(server: &str, dir: &str) -> Form {
        Form::new(
            Kind::NewDir { server: server.into(), dir: dir.into() },
            "New folder",
            vec![Field::new("name", "folder name", true)],
        )
    }

    pub fn rename(server: &str, from: &str) -> Form {
        Form::new(
            Kind::Rename { server: server.into(), from: from.into() },
            "Rename / move",
            vec![Field::with("to", "new path, relative to the server folder", from)],
        )
    }

    pub fn backup(server: &str) -> Form {
        Form::new(
            Kind::Backup { server: server.into() },
            format!("Back up {server}"),
            vec![Field::new("note", "optional, e.g. before 1.21 upgrade", false)],
        )
    }

    pub fn schedule(server: &str) -> Form {
        Form::new(
            Kind::Schedule,
            "Add scheduled task",
            vec![
                Field::new("name", "e.g. nightly restart", true),
                Field::with("server", "which server", server),
                Field::with("action", "restart, backup, command, say, start, stop", "restart"),
                Field::new("payload", "command / message (restart: optional warning)", false),
                Field::with("when", "30m, 6h, every 2h, daily 04:00", "daily 04:00"),
            ],
        )
    }

    pub fn add_user() -> Form {
        Form::new(
            Kind::AddUser,
            "Add user",
            vec![
                Field::new("username", "letters, digits, - _ .", true),
                Field::new("password", "at least 8 characters", true).secret(),
                Field::with("role", "viewer, operator or admin", "operator"),
                Field::with("servers", "comma separated, or all", "all"),
            ],
        )
    }

    pub fn password(username: &str) -> Form {
        Form::new(
            Kind::Password { username: username.into() },
            format!("New password — {username}"),
            vec![
                Field::new("password", "at least 8 characters", true).secret(),
                Field::new("again", "repeat it", true).secret(),
            ],
        )
    }

    pub fn role(username: &str, role: Role, servers: &[String]) -> Form {
        Form::new(
            Kind::Role { username: username.into() },
            format!("Role — {username}"),
            vec![
                Field::with("role", "viewer, operator or admin", role.to_string()),
                Field::with(
                    "servers",
                    "comma separated, or all",
                    if servers.is_empty() { "all".to_string() } else { servers.join(", ") },
                ),
            ],
        )
    }

    pub fn value(&self, index: usize) -> String {
        self.fields
            .get(index)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------- input ---

    pub fn insert(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.insert(field.cursor, c);
            field.cursor += c.len_utf8();
            self.error = None;
        }
    }

    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            if let Some(previous) = field.previous_boundary() {
                field.value.remove(previous);
                field.cursor = previous;
            }
            self.error = None;
        }
    }

    pub fn delete(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            if field.cursor < field.value.len() {
                field.value.remove(field.cursor);
            }
            self.error = None;
        }
    }

    pub fn clear_field(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.clear();
            field.cursor = 0;
        }
    }

    pub fn move_left(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus)
            && let Some(previous) = field.previous_boundary()
        {
            field.cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cursor = field.next_boundary();
        }
    }

    pub fn home(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cursor = 0;
        }
    }

    pub fn end(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cursor = field.value.len();
        }
    }

    pub fn next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
        self.end();
    }

    pub fn previous(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
        self.end();
    }

    pub fn on_last_field(&self) -> bool {
        self.focus + 1 == self.fields.len()
    }

    // ------------------------------------------------------------- build ---

    pub fn build(&self) -> Result<Request, String> {
        for field in &self.fields {
            if field.required && field.value.trim().is_empty() {
                return Err(format!("`{}` is required", field.label));
            }
        }
        let v = |i: usize| self.value(i);
        Ok(match &self.kind {
            Kind::Login => return Err("login is handled by the login screen".into()),
            Kind::CreateServer => {
                let kind = ServerKind::parse(&v(1))
                    .ok_or_else(|| format!("`{}` — type is paper, vanilla, purpur, fabric or custom", v(1)))?;
                if !yes(&v(6))? {
                    return Err("you must accept the Minecraft EULA to run a server".into());
                }
                Request::CreateServer(NewServer {
                    name: v(0).to_lowercase(),
                    kind,
                    version: if v(2).is_empty() { "latest".into() } else { v(2) },
                    memory_mb: number(&v(3), "memory")?,
                    port: number(&v(4), "port")?,
                    java: v(5),
                    eula: true,
                })
            }
            Kind::DeleteServer { server } => {
                if v(0) != *server {
                    return Err(format!("type `{server}` to confirm"));
                }
                Request::DeleteServer {
                    server: server.clone(),
                    delete_files: yes(&v(1))?,
                }
            }
            Kind::Settings { server, base } => Request::UpdateSettings {
                server: server.clone(),
                settings: ServerSettings {
                    kind: base.kind,
                    version: base.version.clone(),
                    memory_max_mb: number(&v(0), "memory")?,
                    memory_min_mb: number(&v(1), "min memory")?,
                    port: number(&v(2), "port")?,
                    auto_start: yes(&v(3))?,
                    auto_restart: yes(&v(4))?,
                    stop_timeout_secs: number(&v(5), "stop timeout")?,
                    java: v(6),
                    jar: v(7),
                    jvm_args: v(8),
                },
            },
            Kind::Reinstall { server } => Request::Reinstall {
                server: server.clone(),
                kind: ServerKind::parse(&v(0)).ok_or_else(|| format!("unknown type `{}`", v(0)))?,
                version: if v(1).is_empty() { "latest".into() } else { v(1) },
            },
            Kind::Property { server, key } => match key {
                Some(key) => Request::SetProperty {
                    server: server.clone(),
                    key: key.clone(),
                    value: self.fields[0].value.clone(),
                },
                None => Request::SetProperty {
                    server: server.clone(),
                    key: v(0),
                    value: self.fields[1].value.clone(),
                },
            },
            Kind::PlayerAdd { server } => {
                let action = match v(1).to_ascii_lowercase().as_str() {
                    "whitelist" | "wl" | "allow" => PlayerAction::WhitelistAdd,
                    "unwhitelist" | "unwl" | "whitelist remove" => PlayerAction::WhitelistRemove,
                    "op" => PlayerAction::Op,
                    "deop" => PlayerAction::Deop,
                    "ban" => PlayerAction::Ban,
                    "pardon" | "unban" => PlayerAction::Pardon,
                    "kick" => PlayerAction::Kick,
                    other => return Err(format!("unknown action `{other}`")),
                };
                Request::Player {
                    server: server.clone(),
                    action,
                    name: v(0),
                    extra: String::new(),
                }
            }
            Kind::PlayerText { server, name, action } => Request::Player {
                server: server.clone(),
                action: *action,
                name: name.clone(),
                extra: v(0),
            },
            Kind::NewFile { server, dir } => Request::WriteFile {
                server: server.clone(),
                path: join(dir, &v(0)),
                content: String::new(),
            },
            Kind::NewDir { server, dir } => Request::MakeDir {
                server: server.clone(),
                path: join(dir, &v(0)),
            },
            Kind::Rename { server, from } => Request::RenamePath {
                server: server.clone(),
                from: from.clone(),
                to: v(0),
            },
            Kind::Backup { server } => Request::CreateBackup {
                server: server.clone(),
                note: v(0),
            },
            Kind::Schedule => Request::AddSchedule(NewSchedule {
                name: v(0),
                server: v(1),
                action: TaskAction::parse(&v(2)).ok_or_else(|| format!("unknown action `{}`", v(2)))?,
                payload: v(3),
                when: When::parse(&v(4))?,
            }),
            Kind::AddUser => Request::AddUser {
                username: v(0),
                password: self.fields[1].value.clone(),
                role: Role::parse(&v(2)).ok_or("role is viewer, operator or admin")?,
                servers: list(&v(3)),
            },
            Kind::Password { username } => {
                if self.fields[0].value != self.fields[1].value {
                    return Err("the passwords do not match".into());
                }
                Request::SetPassword {
                    username: username.clone(),
                    password: self.fields[0].value.clone(),
                }
            }
            Kind::Role { username } => Request::SetRole {
                username: username.clone(),
                role: Role::parse(&v(0)).ok_or("role is viewer, operator or admin")?,
                servers: list(&v(1)),
            },
        })
    }

    // ------------------------------------------------------------ render ---

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let height = self.fields.len() as u16 + 7;
        let target = centered(area, 78, height);
        frame.render_widget(Clear, target);
        self.render_in(frame, target, "save");
    }

    pub fn render_in(&self, frame: &mut Frame, target: Rect, verb: &str) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ACCENT))
            .title(Span::styled(format!(" {} ", self.title), theme::title()));
        let inner = block.inner(target);
        frame.render_widget(block, target);

        let [rows, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);
        let lines: Vec<Line> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let focused = i == self.focus;
                let marker = if focused { "▸" } else { " " };
                let mut spans = vec![Span::styled(
                    format!("{marker} {:<13}", field.label),
                    if focused { theme::accent() } else { theme::label() },
                )];
                let entry = Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD);
                if field.value.is_empty() {
                    if focused {
                        spans.push(Span::styled("█", theme::accent()));
                    }
                    spans.push(Span::styled(field.hint, theme::muted()));
                } else if focused {
                    let (before, after) = field.split_at_cursor();
                    spans.push(Span::styled(before, entry));
                    spans.push(Span::styled("█", theme::accent()));
                    spans.push(Span::styled(after, entry));
                } else {
                    spans.push(Span::styled(field.display(), entry));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows);

        let mut footer_lines = Vec::new();
        if let Some(error) = &self.error {
            footer_lines.push(Line::from(Span::styled(format!("✗ {error}"), theme::bad())));
        } else if self.busy {
            footer_lines.push(Line::from(Span::styled("working…", theme::accent())));
        } else {
            let hint = self
                .fields
                .get(self.focus)
                .map(|f| match (f.hint.is_empty(), f.required) {
                    (true, _) => String::new(),
                    (false, true) => format!("{} — required", f.hint),
                    (false, false) => f.hint.to_string(),
                })
                .unwrap_or_default();
            footer_lines.push(Line::from(Span::styled(hint, theme::muted())));
        }
        footer_lines.push(Line::from(vec![
            Span::styled("Tab/↑↓", theme::accent()),
            Span::styled(" move   ", theme::muted()),
            Span::styled("Enter", theme::accent()),
            Span::styled(format!(" {verb}   "), theme::muted()),
            Span::styled("Ctrl-U", theme::accent()),
            Span::styled(" clear   ", theme::muted()),
            Span::styled("Esc", theme::accent()),
            Span::styled(if matches!(self.kind, Kind::Login) { " quit" } else { " cancel" }, theme::muted()),
        ]));
        frame.render_widget(Paragraph::new(footer_lines).wrap(Wrap { trim: false }), footer);
    }
}

pub fn join(dir: &str, name: &str) -> String {
    let dir = dir.trim_matches('/');
    if dir.is_empty() {
        name.trim_matches('/').to_string()
    } else {
        format!("{dir}/{}", name.trim_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(form: &mut Form, values: &[&str]) {
        for (i, v) in values.iter().enumerate() {
            form.fields[i].set(*v);
        }
    }

    #[test]
    fn create_server_needs_the_eula() {
        let mut form = Form::create_server(25565);
        fill(&mut form, &["Survival", "paper", "latest", "4096", "25565", "java", "no"]);
        assert!(form.build().unwrap_err().contains("EULA"));
        form.fields[6].set("yes");
        match form.build().unwrap() {
            Request::CreateServer(new) => {
                assert_eq!(new.name, "survival");
                assert_eq!(new.kind, ServerKind::Paper);
                assert_eq!(new.memory_mb, 4096);
                assert!(new.eula);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deleting_requires_typing_the_name() {
        let mut form = Form::delete_server("survival");
        form.fields[0].set("survivl");
        assert!(form.build().is_err());
        form.fields[0].set("survival");
        form.fields[1].set("yes");
        assert!(matches!(
            form.build().unwrap(),
            Request::DeleteServer { delete_files: true, .. }
        ));
    }

    #[test]
    fn passwords_must_match_and_are_masked() {
        let mut form = Form::password("alex");
        assert!(form.fields[0].secret);
        fill(&mut form, &["password1", "password2"]);
        assert!(form.build().is_err());
        form.fields[1].set("password1");
        assert!(form.build().is_ok());
    }

    #[test]
    fn schedule_form_parses_when() {
        let mut form = Form::schedule("survival");
        fill(&mut form, &["nightly", "survival", "restart", "restarting!", "daily 04:00"]);
        match form.build().unwrap() {
            Request::AddSchedule(s) => {
                assert_eq!(s.action, TaskAction::Restart);
                assert_eq!(s.when, When::Daily { hour: 4, minute: 0 });
            }
            other => panic!("unexpected {other:?}"),
        }
        form.fields[4].set("sometimes");
        assert!(form.build().is_err());
    }

    #[test]
    fn editing_in_the_middle() {
        let mut form = Form::new_file("s", "");
        form.insert_str("abd");
        form.move_left();
        form.insert('c');
        assert_eq!(form.fields[0].value, "abcd");
        form.home();
        form.delete();
        assert_eq!(form.fields[0].value, "bcd");
    }

    #[test]
    fn paths_join() {
        assert_eq!(join("", "a.txt"), "a.txt");
        assert_eq!(join("plugins/", "/x.yml"), "plugins/x.yml");
    }
}
