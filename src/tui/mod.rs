//! The panel.
//!
//! It starts on a login screen: host address, username, password. Once
//! connected it is a thin client — every number on screen comes from the
//! host, and every action is a request the host checks against the account's
//! role. Closing the panel never touches the servers.

mod editor;
mod form;
mod theme;
mod views;
mod widgets;

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{DefaultTerminal, Frame};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::net::{self, Link, Outcome};
use crate::paths;
use crate::proto::*;
use editor::Editor;
use form::Form;

const POLL: Duration = Duration::from_millis(50);
const SNAPSHOT_EVERY: Duration = Duration::from_millis(1000);
const CONSOLE_EVERY: Duration = Duration::from_millis(400);
const TAB_DATA_EVERY: Duration = Duration::from_secs(3);
const TOAST_TTL: Duration = Duration::from_secs(5);
const CONSOLE_KEEP: usize = 5000;

// ------------------------------------------------------------ profiles ---

#[derive(Serialize, Deserialize, Clone)]
pub struct Profile {
    pub address: String,
    pub username: String,
    pub last_used: i64,
}

fn profiles_path() -> std::path::PathBuf {
    paths::client_dir().join("profiles.json")
}

fn load_profiles() -> Vec<Profile> {
    let mut list: Vec<Profile> = std::fs::read_to_string(profiles_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    list.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    list
}

/// Remember where we logged in — never the password.
fn remember(address: &str, username: &str) {
    let mut list = load_profiles();
    list.retain(|p| !(p.address == address && p.username == username));
    list.insert(
        0,
        Profile {
            address: address.into(),
            username: username.into(),
            last_used: now(),
        },
    );
    list.truncate(12);
    if paths::ensure_client_dir().is_ok()
        && let Ok(text) = serde_json::to_string_pretty(&list)
    {
        let _ = paths::write_atomic(&profiles_path(), text.as_bytes());
    }
}

// --------------------------------------------------------------- login ---

pub enum LoginState {
    Idle,
    Connecting(Instant),
    Trust { address: String, fingerprint: String },
    Error(String),
}

type ConnectResult = Result<(Outcome, String), String>;

pub struct Login {
    pub form: Form,
    pub profiles: Vec<Profile>,
    pub profile_idx: Option<usize>,
    pub state: LoginState,
    rx: Option<Receiver<ConnectResult>>,
}

impl Login {
    fn new(address: Option<String>, user: Option<String>) -> Login {
        let profiles = load_profiles();
        let (address, user, idx) = match (address, user) {
            (Some(a), u) => {
                let found = profiles.iter().position(|p| net::normalize_address(&a).ok().as_deref() == Some(&p.address));
                let u = u.or_else(|| found.map(|i| profiles[i].username.clone()));
                (a, u.unwrap_or_default(), found)
            }
            (None, u) => match profiles.first() {
                Some(p) => (p.address.clone(), u.unwrap_or_else(|| p.username.clone()), Some(0)),
                None => ("localhost".into(), u.unwrap_or_else(|| "admin".into()), None),
            },
        };
        Login {
            form: Form::login(&address, &user),
            profiles,
            profile_idx: idx,
            state: LoginState::Idle,
            rx: None,
        }
    }

    fn connect(&mut self, accept: Option<String>) {
        let address = self.form.value(0);
        let username = self.form.value(1);
        let password = self.form.fields[2].value.clone();
        if address.is_empty() || username.is_empty() || password.is_empty() {
            self.state = LoginState::Error("fill in the address, username and password".into());
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = net::normalize_address(&address)
                .and_then(|normalized| {
                    net::connect(&normalized, &username, &password, accept.as_deref()).map(|o| (o, normalized))
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.rx = Some(rx);
        self.state = LoginState::Connecting(Instant::now());
    }

    fn cycle_profile(&mut self, delta: isize) {
        if self.profiles.is_empty() {
            return;
        }
        let len = self.profiles.len() as isize;
        let next = match self.profile_idx {
            Some(i) => (i as isize + delta).rem_euclid(len),
            None => 0,
        } as usize;
        self.profile_idx = Some(next);
        let p = self.profiles[next].clone();
        self.form.fields[0].set(p.address);
        self.form.fields[1].set(p.username);
        self.form.fields[2].set("");
        self.form.focus = 2;
    }
}

// --------------------------------------------------------------- panel ---

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Console,
    Players,
    Files,
    Config,
    Backups,
    Tasks,
    Users,
    Activity,
}

impl Tab {
    pub const ALL: [Tab; 9] = [
        Tab::Overview,
        Tab::Console,
        Tab::Players,
        Tab::Files,
        Tab::Config,
        Tab::Backups,
        Tab::Tasks,
        Tab::Users,
        Tab::Activity,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Console => "Console",
            Tab::Players => "Players",
            Tab::Files => "Files",
            Tab::Config => "Config",
            Tab::Backups => "Backups",
            Tab::Tasks => "Tasks",
            Tab::Users => "Users",
            Tab::Activity => "Activity",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// How to interpret the reply to a request in flight.
enum Expect {
    Snapshot,
    Console(String),
    Players(String),
    Files(String),
    Properties(String),
    Settings(String),
    SettingsForm(String),
    Backups(String),
    Schedules,
    Users,
    Activity,
    OpenFile(String),
    SaveFile,
    /// Toast the outcome and reload the current view.
    Action,
    /// From a form: close on success, show the error in the form otherwise.
    Form,
}

struct Confirm {
    prompt: String,
    request: Request,
    danger: bool,
}

/// One-line text input for the console prompt.
#[derive(Default)]
pub struct Input {
    pub value: String,
    pub cursor: usize,
}

impl Input {
    fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }
    fn backspace(&mut self) {
        if let Some(c) = self.value[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.value.remove(self.cursor);
        }
    }
    fn delete(&mut self) {
        if self.cursor < self.value.len() {
            self.value.remove(self.cursor);
        }
    }
    fn left(&mut self) {
        if let Some(c) = self.value[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }
    fn right(&mut self) {
        if let Some(c) = self.value[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
    fn set(&mut self, text: &str) {
        self.value = text.to_string();
        self.cursor = self.value.len();
    }
    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.value)
    }
}

pub struct PlayerRow {
    pub name: String,
    pub online: bool,
    pub op: bool,
    pub whitelisted: bool,
    pub banned: Option<String>,
}

pub struct Panel {
    link: Link,
    pub address: String,
    pub welcome: Welcome,
    pub snapshot: Option<Snapshot>,
    pub tab: Tab,
    pub selected: Option<String>,
    pub server_idx: usize,
    pending: HashMap<u64, Expect>,
    snapshot_inflight: bool,
    console_inflight: bool,
    tab_inflight: bool,
    last_snapshot: Instant,
    last_console: Instant,
    last_tab_data: Instant,
    pub last_error: Option<String>,

    pub console_server: String,
    pub console: VecDeque<ConsoleLine>,
    console_next: u64,
    pub console_scroll: usize,
    pub input: Input,
    pub input_focus: bool,
    history: Vec<String>,
    history_idx: Option<usize>,

    pub players: Option<PlayersInfo>,
    pub player_sel: usize,
    pub files: Option<Listing>,
    pub file_path: String,
    pub file_sel: usize,
    pub editor: Option<Editor>,
    pub properties: Vec<Property>,
    pub prop_sel: usize,
    pub settings: Option<ServerSettings>,
    pub backups: Vec<BackupInfo>,
    pub backup_sel: usize,
    pub schedules: Vec<Schedule>,
    pub task_sel: usize,
    pub users: Vec<UserInfo>,
    pub user_sel: usize,
    pub activity: Vec<ActivityEntry>,
    pub activity_scroll: usize,
    pub loaded: bool,

    pub toast: Option<(String, bool, Instant)>,
    confirm: Option<Confirm>,
    pub form: Option<Form>,
    pub help: bool,
    pub tick: usize,
}

pub enum After {
    Stay,
    Logout,
    Quit,
}

fn message_of(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("done")
        .to_string()
}

impl Panel {
    fn new(link: Link, address: String, welcome: Welcome) -> Panel {
        let past = Instant::now() - Duration::from_secs(60);
        Panel {
            link,
            address,
            welcome,
            snapshot: None,
            tab: Tab::Overview,
            selected: None,
            server_idx: 0,
            pending: HashMap::new(),
            snapshot_inflight: false,
            console_inflight: false,
            tab_inflight: false,
            last_snapshot: past,
            last_console: past,
            last_tab_data: past,
            last_error: None,
            console_server: String::new(),
            console: VecDeque::new(),
            console_next: 0,
            console_scroll: 0,
            input: Input::default(),
            input_focus: true,
            history: Vec::new(),
            history_idx: None,
            players: None,
            player_sel: 0,
            files: None,
            file_path: String::new(),
            file_sel: 0,
            editor: None,
            properties: Vec::new(),
            prop_sel: 0,
            settings: None,
            backups: Vec::new(),
            backup_sel: 0,
            schedules: Vec::new(),
            task_sel: 0,
            users: Vec::new(),
            user_sel: 0,
            activity: Vec::new(),
            activity_scroll: 0,
            loaded: false,
            toast: None,
            confirm: None,
            form: None,
            help: false,
            tick: 0,
        }
    }

    pub fn role(&self) -> Role {
        self.snapshot.as_ref().map(|s| s.me.role).unwrap_or(self.welcome.role)
    }

    pub fn server(&self) -> Option<&ServerStatus> {
        self.snapshot.as_ref().and_then(|s| s.servers.get(self.server_idx))
    }

    pub fn server_name(&self) -> Option<String> {
        self.server().map(|s| s.name.clone())
    }

    pub fn is_online(&self) -> bool {
        self.link.is_online()
    }

    fn send(&mut self, request: Request, expect: Expect) {
        let id = self.link.send(request);
        self.pending.insert(id, expect);
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some((text.into(), error, Instant::now()));
    }

    /// Ask for something that changes state; the result is toasted.
    fn act(&mut self, request: Request) {
        if self.role() == Role::Viewer {
            self.toast("your account is view-only", true);
            return;
        }
        self.send(request, Expect::Action);
    }

    fn ask(&mut self, prompt: impl Into<String>, request: Request, danger: bool) {
        self.confirm = Some(Confirm {
            prompt: prompt.into(),
            request,
            danger,
        });
    }

    // ------------------------------------------------------------ tick ---

    fn pump(&mut self) {
        for done in self.link.poll() {
            let Some(expect) = self.pending.remove(&done.id) else {
                continue;
            };
            self.receive(expect, done.result);
        }

        if !self.snapshot_inflight && self.last_snapshot.elapsed() >= SNAPSHOT_EVERY {
            self.snapshot_inflight = true;
            self.last_snapshot = Instant::now();
            self.send(Request::Snapshot, Expect::Snapshot);
        }

        if let Some(server) = self.server_name() {
            if server != self.console_server {
                self.switch_server(&server);
            }
            let wants_console = matches!(self.tab, Tab::Overview | Tab::Console);
            if wants_console && !self.console_inflight && self.last_console.elapsed() >= CONSOLE_EVERY {
                self.console_inflight = true;
                self.last_console = Instant::now();
                self.send(
                    Request::Console {
                        server: server.clone(),
                        since: self.console_next,
                    },
                    Expect::Console(server),
                );
            }
        }

        let periodic = matches!(self.tab, Tab::Players | Tab::Backups | Tab::Tasks | Tab::Activity);
        if !self.tab_inflight
            && (!self.loaded || (periodic && self.last_tab_data.elapsed() >= TAB_DATA_EVERY))
            && self.snapshot.is_some()
        {
            self.load_tab();
        }

        if let Some((_, _, at)) = &self.toast
            && at.elapsed() > TOAST_TTL
        {
            self.toast = None;
        }
    }

    fn switch_server(&mut self, server: &str) {
        self.console_server = server.to_string();
        self.console.clear();
        self.console_next = 0;
        self.console_scroll = 0;
        self.players = None;
        self.files = None;
        self.file_path.clear();
        self.file_sel = 0;
        self.properties.clear();
        self.settings = None;
        self.backups.clear();
        self.loaded = false;
        self.last_console = Instant::now() - CONSOLE_EVERY;
    }

    /// Fetch whatever the current view shows.
    fn load_tab(&mut self) {
        self.loaded = true;
        self.last_tab_data = Instant::now();
        let operator = self.role() >= Role::Operator;
        let Some(server) = self.server_name() else {
            // Tabs that do not depend on a server still load.
            match self.tab {
                Tab::Tasks => self.request_tab(Request::Schedules, Expect::Schedules),
                Tab::Users => self.request_tab(Request::Users, Expect::Users),
                Tab::Activity => self.request_tab(Request::Activity, Expect::Activity),
                _ => {}
            }
            return;
        };
        match self.tab {
            Tab::Overview | Tab::Console => {}
            Tab::Players => self.request_tab(Request::Players { server: server.clone() }, Expect::Players(server)),
            Tab::Files if operator => self.request_tab(
                Request::ListFiles {
                    server: server.clone(),
                    path: self.file_path.clone(),
                },
                Expect::Files(server),
            ),
            Tab::Config => {
                self.send(Request::GetSettings { server: server.clone() }, Expect::Settings(server.clone()));
                if operator {
                    self.request_tab(Request::Properties { server: server.clone() }, Expect::Properties(server));
                }
            }
            Tab::Backups if operator => {
                self.request_tab(Request::Backups { server: server.clone() }, Expect::Backups(server))
            }
            Tab::Tasks => self.request_tab(Request::Schedules, Expect::Schedules),
            Tab::Users => self.request_tab(Request::Users, Expect::Users),
            Tab::Activity => self.request_tab(Request::Activity, Expect::Activity),
            _ => {}
        }
    }

    fn request_tab(&mut self, request: Request, expect: Expect) {
        self.tab_inflight = true;
        self.send(request, expect);
    }

    fn reload(&mut self) {
        self.loaded = false;
    }

    fn receive(&mut self, expect: Expect, result: Result<Value, String>) {
        let current = self.server_name();
        let is_current = |s: &String| current.as_ref() == Some(s);
        match expect {
            Expect::Snapshot => {
                self.snapshot_inflight = false;
                match result.and_then(|v| serde_json::from_value::<Snapshot>(v).map_err(|e| e.to_string())) {
                    Ok(snapshot) => {
                        self.last_error = None;
                        self.apply_snapshot(snapshot);
                    }
                    Err(err) => self.last_error = Some(err),
                }
            }
            Expect::Console(server) => {
                self.console_inflight = false;
                if server != self.console_server {
                    return;
                }
                if let Ok(chunk) = result.and_then(|v| serde_json::from_value::<ConsoleChunk>(v).map_err(|e| e.to_string())) {
                    let added = chunk.lines.len();
                    if chunk.next < self.console_next {
                        // The host restarted; its sequence numbers did too.
                        self.console.clear();
                    }
                    for line in chunk.lines {
                        if self.console.len() >= CONSOLE_KEEP {
                            self.console.pop_front();
                        }
                        self.console.push_back(line);
                    }
                    self.console_next = chunk.next;
                    if self.console_scroll > 0 {
                        // Keep the view still while reading back.
                        self.console_scroll += added;
                    }
                    if added >= 2000 {
                        // More waiting; fetch again straight away.
                        self.last_console = Instant::now() - CONSOLE_EVERY;
                    }
                }
            }
            Expect::Players(server) => {
                self.tab_inflight = false;
                if is_current(&server) {
                    match result.and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
                        Ok(players) => self.players = Some(players),
                        Err(err) => self.toast(err, true),
                    }
                }
            }
            Expect::Files(server) => {
                self.tab_inflight = false;
                if is_current(&server) {
                    match result.and_then(|v| serde_json::from_value::<Listing>(v).map_err(|e| e.to_string())) {
                        Ok(listing) => {
                            self.file_path = listing.path.clone();
                            self.files = Some(listing);
                            let len = self.file_rows_len();
                            self.file_sel = self.file_sel.min(len.saturating_sub(1));
                        }
                        Err(err) => {
                            self.toast(err, true);
                            if !self.file_path.is_empty() {
                                self.file_path.clear();
                                self.reload();
                            }
                        }
                    }
                }
            }
            Expect::Properties(server) => {
                self.tab_inflight = false;
                if is_current(&server) {
                    match result.and_then(|v| serde_json::from_value::<Vec<Property>>(v).map_err(|e| e.to_string())) {
                        Ok(mut props) => {
                            props.sort_by(|a, b| a.key.cmp(&b.key));
                            self.properties = props;
                            self.prop_sel = self.prop_sel.min(self.properties.len().saturating_sub(1));
                        }
                        Err(err) => self.toast(err, true),
                    }
                }
            }
            Expect::Settings(server) => {
                if is_current(&server) {
                    match result.and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
                        Ok(settings) => self.settings = Some(settings),
                        Err(err) => self.toast(err, true),
                    }
                }
            }
            Expect::SettingsForm(server) => {
                match result.and_then(|v| serde_json::from_value::<ServerSettings>(v).map_err(|e| e.to_string())) {
                    Ok(settings) => {
                        self.settings = Some(settings.clone());
                        self.form = Some(Form::settings(&server, settings));
                    }
                    Err(err) => self.toast(err, true),
                }
            }
            Expect::Backups(server) => {
                self.tab_inflight = false;
                if is_current(&server) {
                    match result.and_then(|v| serde_json::from_value::<Vec<BackupInfo>>(v).map_err(|e| e.to_string())) {
                        Ok(list) => {
                            self.backups = list;
                            self.backup_sel = self.backup_sel.min(self.backups.len().saturating_sub(1));
                        }
                        Err(err) => self.toast(err, true),
                    }
                }
            }
            Expect::Schedules => {
                self.tab_inflight = false;
                match result.and_then(|v| serde_json::from_value::<Vec<Schedule>>(v).map_err(|e| e.to_string())) {
                    Ok(list) => {
                        self.schedules = list;
                        self.task_sel = self.task_sel.min(self.schedules.len().saturating_sub(1));
                    }
                    Err(err) => self.toast(err, true),
                }
            }
            Expect::Users => {
                self.tab_inflight = false;
                match result.and_then(|v| serde_json::from_value::<Vec<UserInfo>>(v).map_err(|e| e.to_string())) {
                    Ok(list) => {
                        self.users = list;
                        self.user_sel = self.user_sel.min(self.users.len().saturating_sub(1));
                    }
                    Err(err) => self.toast(err, true),
                }
            }
            Expect::Activity => {
                self.tab_inflight = false;
                match result.and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
                    Ok(list) => self.activity = list,
                    Err(err) => self.toast(err, true),
                }
            }
            Expect::OpenFile(server) => match result.and_then(|v| serde_json::from_value::<FileContent>(v).map_err(|e| e.to_string())) {
                Ok(file) => {
                    let readonly = self.role() < Role::Operator;
                    self.editor = Some(Editor::new(&server, &file.path, &file.content, readonly));
                }
                Err(err) => self.toast(err, true),
            },
            Expect::SaveFile => {
                if let Some(editor) = self.editor.as_mut() {
                    editor.saving = false;
                    if result.is_ok() {
                        editor.dirty = false;
                        editor.warned = false;
                    }
                }
                match result {
                    Ok(v) => self.toast(message_of(&v), false),
                    Err(err) => self.toast(err, true),
                }
                self.reload();
            }
            Expect::Action => {
                match result {
                    Ok(v) => self.toast(message_of(&v), false),
                    Err(err) => self.toast(err, true),
                }
                self.reload();
                self.last_snapshot = Instant::now() - SNAPSHOT_EVERY;
                // Player lists update on disk a moment after the command.
                if self.tab == Tab::Players {
                    self.last_tab_data = Instant::now() - TAB_DATA_EVERY + Duration::from_millis(800);
                }
            }
            Expect::Form => match result {
                Ok(v) => {
                    self.form = None;
                    self.toast(message_of(&v), false);
                    self.reload();
                    self.last_snapshot = Instant::now() - SNAPSHOT_EVERY;
                }
                Err(err) => {
                    if let Some(form) = self.form.as_mut() {
                        form.busy = false;
                        form.error = Some(err);
                    } else {
                        self.toast(err, true);
                    }
                }
            },
        }
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        // Keep the same server selected by name as the list changes.
        let index = self
            .selected
            .as_ref()
            .and_then(|name| snapshot.servers.iter().position(|s| &s.name == name));
        self.server_idx = match index {
            Some(i) => i,
            None => self.server_idx.min(snapshot.servers.len().saturating_sub(1)),
        };
        self.selected = snapshot.servers.get(self.server_idx).map(|s| s.name.clone());
        self.snapshot = Some(snapshot);
    }

    fn select_server(&mut self, index: usize) {
        if let Some(snapshot) = &self.snapshot
            && let Some(server) = snapshot.servers.get(index)
        {
            self.server_idx = index;
            self.selected = Some(server.name.clone());
        }
    }

    fn cycle_server(&mut self, delta: isize) {
        let len = self.snapshot.as_ref().map(|s| s.servers.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        let next = (self.server_idx as isize + delta).rem_euclid(len as isize) as usize;
        self.select_server(next);
        if let Some(name) = self.server_name() {
            self.toast(format!("server: {name}"), false);
        }
    }

    pub fn player_rows(&self) -> Vec<PlayerRow> {
        let Some(info) = &self.players else {
            return Vec::new();
        };
        let lower = |s: &str| s.to_ascii_lowercase();
        let mut names: Vec<String> = Vec::new();
        let mut add = |n: &str| {
            if !names.iter().any(|x| lower(x) == lower(n)) {
                names.push(n.to_string());
            }
        };
        for n in &info.online {
            add(n);
        }
        for n in info.ops.iter().chain(&info.whitelist) {
            add(n);
        }
        for b in &info.banned {
            add(&b.name);
        }
        let mut rows: Vec<PlayerRow> = names
            .into_iter()
            .map(|name| {
                let key = lower(&name);
                PlayerRow {
                    online: info.online.iter().any(|n| lower(n) == key),
                    op: info.ops.iter().any(|n| lower(n) == key),
                    whitelisted: info.whitelist.iter().any(|n| lower(n) == key),
                    banned: info
                        .banned
                        .iter()
                        .find(|b| lower(&b.name) == key)
                        .map(|b| b.reason.clone()),
                    name,
                }
            })
            .collect();
        rows.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| lower(&a.name).cmp(&lower(&b.name))));
        rows
    }

    pub fn file_rows_len(&self) -> usize {
        let entries = self.files.as_ref().map(|f| f.entries.len()).unwrap_or(0);
        entries + usize::from(!self.file_path.is_empty())
    }

    /// The selected file row: `None` for the `..` row.
    fn selected_file(&self) -> Option<&FileEntry> {
        let files = self.files.as_ref()?;
        let offset = usize::from(!self.file_path.is_empty());
        if self.file_sel < offset {
            return None;
        }
        files.entries.get(self.file_sel - offset)
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Overview => self.snapshot.as_ref().map(|s| s.servers.len()).unwrap_or(0),
            Tab::Players => self.player_rows().len(),
            Tab::Files => self.file_rows_len(),
            Tab::Config => self.properties.len(),
            Tab::Backups => self.backups.len(),
            Tab::Tasks => self.schedules.len(),
            Tab::Users => self.users.len(),
            Tab::Activity => self.activity.len(),
            Tab::Console => self.console.len(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.list_len();
        if self.tab == Tab::Console {
            let scroll = self.console_scroll as isize - delta;
            self.console_scroll = scroll.clamp(0, len.saturating_sub(1) as isize) as usize;
            return;
        }
        if len == 0 {
            return;
        }
        let clamp = |v: usize| (v as isize + delta).clamp(0, len as isize - 1) as usize;
        match self.tab {
            Tab::Overview => {
                let next = clamp(self.server_idx);
                self.select_server(next);
            }
            Tab::Players => self.player_sel = clamp(self.player_sel),
            Tab::Files => self.file_sel = clamp(self.file_sel),
            Tab::Config => self.prop_sel = clamp(self.prop_sel),
            Tab::Backups => self.backup_sel = clamp(self.backup_sel),
            Tab::Tasks => self.task_sel = clamp(self.task_sel),
            Tab::Users => self.user_sel = clamp(self.user_sel),
            Tab::Activity => self.activity_scroll = clamp(self.activity_scroll),
            Tab::Console => {}
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.loaded = false;
            self.tab_inflight = false;
            if tab == Tab::Console {
                self.input_focus = true;
            }
        }
    }

    // ------------------------------------------------------------ keys ---

    fn handle_key(&mut self, key: KeyEvent) -> After {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.form.is_some() {
            self.form_key(key);
            return After::Stay;
        }
        if self.editor.is_some() {
            self.editor_key(key);
            return After::Stay;
        }
        if let Some(confirm) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.send(confirm.request, Expect::Action);
            } else {
                self.toast("cancelled", false);
            }
            return After::Stay;
        }
        if self.help {
            self.help = false;
            return After::Stay;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            return After::Quit;
        }

        // Tab switching works even while typing in the console.
        match key.code {
            KeyCode::Tab => {
                self.set_tab(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]);
                return After::Stay;
            }
            KeyCode::BackTab => {
                self.set_tab(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]);
                return After::Stay;
            }
            _ => {}
        }

        if self.tab == Tab::Console && self.input_focus {
            self.console_key(key);
            return After::Stay;
        }

        let server = self.server_name();
        match key.code {
            KeyCode::Char('q') => return After::Quit,
            KeyCode::Char('L') => return After::Logout,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = true,
            KeyCode::Char(c @ '1'..='9') => self.set_tab(Tab::ALL[c as usize - '1' as usize]),
            KeyCode::Char('[') => self.cycle_server(-1),
            KeyCode::Char(']') => self.cycle_server(1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(15),
            KeyCode::PageUp => self.move_selection(-15),
            KeyCode::Home => self.move_selection(-100_000),
            KeyCode::End => {
                if self.tab == Tab::Console {
                    self.console_scroll = 0;
                } else {
                    self.move_selection(100_000);
                }
            }
            KeyCode::Char('s') if server.is_some() && self.tab != Tab::Files => {
                self.act(Request::Power {
                    server: server.unwrap(),
                    action: Power::Start,
                });
            }
            KeyCode::Char('x') if server.is_some() => {
                self.act(Request::Power {
                    server: server.unwrap(),
                    action: Power::Stop,
                });
            }
            KeyCode::Char('R') if server.is_some() => {
                self.act(Request::Power {
                    server: server.unwrap(),
                    action: Power::Restart,
                });
            }
            KeyCode::Char('X') if server.is_some() => {
                let name = server.unwrap();
                self.ask(
                    format!("Kill `{name}` now? Unsaved world changes are lost."),
                    Request::Power { server: name, action: Power::Kill },
                    true,
                );
            }
            KeyCode::Char('c') if self.tab != Tab::Players => {
                if self.role() < Role::Admin {
                    self.toast("creating servers needs the admin role", true);
                } else {
                    let next_port = self
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.servers.iter().map(|s| s.port).max())
                        .map(|p| p + 1)
                        .unwrap_or(25565);
                    self.form = Some(Form::create_server(next_port));
                }
            }
            KeyCode::Char('D') if server.is_some() => {
                if self.role() < Role::Admin {
                    self.toast("deleting servers needs the admin role", true);
                } else {
                    self.form = Some(Form::delete_server(&server.unwrap()));
                }
            }
            _ => self.tab_key(key, server),
        }
        After::Stay
    }

    fn tab_key(&mut self, key: KeyEvent, server: Option<String>) {
        match self.tab {
            Tab::Overview => {
                if key.code == KeyCode::Enter {
                    self.set_tab(Tab::Console);
                }
            }
            Tab::Console => {
                if matches!(key.code, KeyCode::Char('i') | KeyCode::Char('/') | KeyCode::Char(':') | KeyCode::Enter) {
                    self.input_focus = true;
                }
            }
            Tab::Players => self.players_key(key, server),
            Tab::Files => self.files_key(key, server),
            Tab::Config => self.config_key(key, server),
            Tab::Backups => self.backups_key(key, server),
            Tab::Tasks => self.tasks_key(key, server),
            Tab::Users => self.users_key(key),
            Tab::Activity => {}
        }
    }

    fn console_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.input_focus = false,
            KeyCode::Enter => {
                let line = self.input.take();
                let line = line.trim().trim_start_matches('/').to_string();
                if line.is_empty() {
                    return;
                }
                if self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                self.history_idx = None;
                self.console_scroll = 0;
                if let Some(server) = self.server_name() {
                    self.act(Request::Command { server, line });
                    self.last_console = Instant::now() - CONSOLE_EVERY + Duration::from_millis(150);
                }
            }
            KeyCode::Up => {
                if self.history.is_empty() {
                    return;
                }
                let idx = match self.history_idx {
                    Some(i) => i.saturating_sub(1),
                    None => self.history.len() - 1,
                };
                self.history_idx = Some(idx);
                let text = self.history[idx].clone();
                self.input.set(&text);
            }
            KeyCode::Down => match self.history_idx {
                Some(i) if i + 1 < self.history.len() => {
                    self.history_idx = Some(i + 1);
                    let text = self.history[i + 1].clone();
                    self.input.set(&text);
                }
                _ => {
                    self.history_idx = None;
                    self.input.set("");
                }
            },
            KeyCode::PageUp => self.move_selection(-15),
            KeyCode::PageDown => self.move_selection(15),
            KeyCode::Char('u') if ctrl => self.input.set(""),
            KeyCode::Char('a') if ctrl => self.input.cursor = 0,
            KeyCode::Char('e') if ctrl => self.input.cursor = self.input.value.len(),
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.cursor = 0,
            KeyCode::End => {
                self.input.cursor = self.input.value.len();
                self.console_scroll = 0;
            }
            _ => {}
        }
    }

    fn players_key(&mut self, key: KeyEvent, server: Option<String>) {
        let Some(server) = server else { return };
        if key.code == KeyCode::Char('a') {
            self.form = Some(Form::player_add(&server));
            return;
        }
        if key.code == KeyCode::Char('W') {
            let on = self.players.as_ref().map(|p| p.whitelist_enabled).unwrap_or(false);
            self.act(Request::Player {
                server,
                action: if on { PlayerAction::WhitelistOff } else { PlayerAction::WhitelistOn },
                name: String::new(),
                extra: String::new(),
            });
            return;
        }
        let rows = self.player_rows();
        let Some(row) = rows.get(self.player_sel) else { return };
        let name = row.name.clone();
        let simple = |action| Request::Player {
            server: server.clone(),
            action,
            name: name.clone(),
            extra: String::new(),
        };
        match key.code {
            KeyCode::Char('o') => {
                let action = if row.op { PlayerAction::Deop } else { PlayerAction::Op };
                self.act(simple(action));
            }
            KeyCode::Char('w') => {
                let action = if row.whitelisted {
                    PlayerAction::WhitelistRemove
                } else {
                    PlayerAction::WhitelistAdd
                };
                self.act(simple(action));
            }
            KeyCode::Char('b') => {
                if row.banned.is_some() {
                    self.act(simple(PlayerAction::Pardon));
                } else {
                    self.form = Some(Form::player_text(&server, &name, PlayerAction::Ban));
                }
            }
            KeyCode::Char('K') => self.form = Some(Form::player_text(&server, &name, PlayerAction::Kick)),
            KeyCode::Char('m') => self.form = Some(Form::player_text(&server, &name, PlayerAction::Message)),
            KeyCode::Char('g') => self.form = Some(Form::player_text(&server, &name, PlayerAction::Gamemode)),
            _ => {}
        }
    }

    fn files_key(&mut self, key: KeyEvent, server: Option<String>) {
        let Some(server) = server else { return };
        if self.role() < Role::Operator {
            return;
        }
        let up = |p: &mut Panel| {
            if p.file_path.is_empty() {
                return;
            }
            let path = p.file_path.trim_end_matches('/');
            p.file_path = path.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default();
            p.file_sel = 0;
            p.reload();
        };
        match key.code {
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => up(self),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('e') => {
                match self.selected_file().cloned() {
                    None if !self.file_path.is_empty() => up(self),
                    None => {}
                    Some(entry) if entry.dir => {
                        self.file_path = form::join(&self.file_path, &entry.name);
                        self.file_sel = 0;
                        self.reload();
                    }
                    Some(entry) => {
                        let path = form::join(&self.file_path, &entry.name);
                        self.send(Request::ReadFile { server: server.clone(), path }, Expect::OpenFile(server));
                    }
                }
            }
            KeyCode::Char('n') => self.form = Some(Form::new_file(&server, &self.file_path)),
            KeyCode::Char('N') => self.form = Some(Form::new_dir(&server, &self.file_path)),
            KeyCode::Char('r') => {
                if let Some(entry) = self.selected_file() {
                    let path = form::join(&self.file_path, &entry.name);
                    self.form = Some(Form::rename(&server, &path));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(entry) = self.selected_file().cloned() {
                    let path = form::join(&self.file_path, &entry.name);
                    let what = if entry.dir { "folder and everything in it" } else { "file" };
                    self.ask(
                        format!("Delete the {what} `{path}`?"),
                        Request::DeletePath { server, path },
                        true,
                    );
                }
            }
            KeyCode::Char('s') => {} // start is disabled here to avoid surprises
            _ => {}
        }
    }

    fn config_key(&mut self, key: KeyEvent, server: Option<String>) {
        let Some(server) = server else { return };
        match key.code {
            KeyCode::Char('e') => {
                if self.role() < Role::Operator {
                    self.toast("your account is view-only", true);
                } else {
                    self.send(Request::GetSettings { server: server.clone() }, Expect::SettingsForm(server));
                }
            }
            KeyCode::Enter => {
                if let Some(prop) = self.properties.get(self.prop_sel) {
                    self.form = Some(Form::property(&server, Some(&prop.key), &prop.value));
                }
            }
            KeyCode::Char('a') => self.form = Some(Form::property(&server, None, "")),
            KeyCode::Char('I') => {
                if self.role() < Role::Admin {
                    self.toast("changing the server jar needs the admin role", true);
                } else if let Some(s) = self.server() {
                    let kind = if s.kind == ServerKind::Custom { ServerKind::Paper } else { s.kind };
                    self.form = Some(Form::reinstall(&server, kind, "latest"));
                }
            }
            _ => {}
        }
    }

    fn backups_key(&mut self, key: KeyEvent, server: Option<String>) {
        let Some(server) = server else { return };
        match key.code {
            KeyCode::Char('b') | KeyCode::Char('a') => self.form = Some(Form::backup(&server)),
            KeyCode::Char('r') | KeyCode::Enter => {
                if let Some(b) = self.backups.get(self.backup_sel) {
                    let id = b.id.clone();
                    self.ask(
                        format!("Restore `{server}` to {id}? The current files are replaced (stop the server first)."),
                        Request::RestoreBackup { server, id },
                        true,
                    );
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(b) = self.backups.get(self.backup_sel) {
                    let id = b.id.clone();
                    self.ask(format!("Delete backup {id}?"), Request::DeleteBackup { server, id }, true);
                }
            }
            _ => {}
        }
    }

    fn tasks_key(&mut self, key: KeyEvent, server: Option<String>) {
        match key.code {
            KeyCode::Char('a') => self.form = Some(Form::schedule(&server.unwrap_or_default())),
            KeyCode::Char('e') => {
                if let Some(t) = self.schedules.get(self.task_sel) {
                    let id = t.id;
                    self.act(Request::ToggleSchedule { id });
                }
            }
            KeyCode::Enter => {
                if let Some(t) = self.schedules.get(self.task_sel) {
                    let id = t.id;
                    self.act(Request::RunSchedule { id });
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(t) = self.schedules.get(self.task_sel) {
                    let (id, name) = (t.id, t.name.clone());
                    self.ask(format!("Delete task `{name}`?"), Request::RemoveSchedule { id }, false);
                }
            }
            _ => {}
        }
    }

    fn users_key(&mut self, key: KeyEvent) {
        let admin = self.role() == Role::Admin;
        let selected = self.users.get(self.user_sel).cloned();
        match key.code {
            KeyCode::Char('a') if admin => self.form = Some(Form::add_user()),
            KeyCode::Char('p') => {
                if let Some(u) = selected {
                    self.form = Some(Form::password(&u.username));
                }
            }
            KeyCode::Char('e') if admin => {
                if let Some(u) = selected {
                    self.form = Some(Form::role(&u.username, u.role, &u.servers));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete if admin => {
                if let Some(u) = selected {
                    self.ask(
                        format!("Remove user `{}`? Their open sessions end.", u.username),
                        Request::RemoveUser { username: u.username },
                        true,
                    );
                }
            }
            KeyCode::Char('a' | 'e' | 'd') => self.toast("managing users needs the admin role", true),
            _ => {}
        }
    }

    fn form_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(form) = self.form.as_mut() else { return };
        match key.code {
            KeyCode::Esc => {
                self.form = None;
            }
            KeyCode::Char('c') if ctrl => self.form = None,
            KeyCode::Char('u') if ctrl => form.clear_field(),
            KeyCode::Tab | KeyCode::Down => form.next(),
            KeyCode::BackTab | KeyCode::Up => form.previous(),
            KeyCode::Left => form.move_left(),
            KeyCode::Right => form.move_right(),
            KeyCode::Home => form.home(),
            KeyCode::End => form.end(),
            KeyCode::Backspace => form.backspace(),
            KeyCode::Delete => form.delete(),
            KeyCode::Char(c) if !ctrl => form.insert(c),
            KeyCode::Enter => {
                if form.busy {
                    return;
                }
                match form.build() {
                    Ok(request) => {
                        form.busy = true;
                        form.error = None;
                        // Moving into a new folder? Follow it.
                        if let Request::MakeDir { .. } = &request {
                            self.file_sel = 0;
                        }
                        self.send(request, Expect::Form);
                    }
                    Err(err) => form.error = Some(err),
                }
            }
            _ => {}
        }
    }

    fn editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(editor) = self.editor.as_mut() else { return };
        match key.code {
            KeyCode::Esc => {
                if editor.dirty && !editor.warned {
                    editor.warned = true;
                } else {
                    self.editor = None;
                }
            }
            KeyCode::Char('s') if ctrl => {
                if editor.readonly || editor.saving {
                    return;
                }
                editor.saving = true;
                let request = Request::WriteFile {
                    server: editor.server.clone(),
                    path: editor.path.clone(),
                    content: editor.content(),
                };
                self.send(request, Expect::SaveFile);
            }
            KeyCode::Char('k') if ctrl => editor.delete_line(),
            KeyCode::Char(c) if !ctrl => editor.insert(c),
            KeyCode::Tab => {
                editor.insert(' ');
                editor.insert(' ');
            }
            KeyCode::Enter => editor.newline(),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete(),
            KeyCode::Left => editor.left(),
            KeyCode::Right => editor.right(),
            KeyCode::Up => editor.up(1),
            KeyCode::Down => editor.down(1),
            KeyCode::PageUp => editor.up(20),
            KeyCode::PageDown => editor.down(20),
            KeyCode::Home => editor.home(),
            KeyCode::End => editor.end(),
            _ => {}
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(form) = self.form.as_mut() {
            form.insert_str(text);
        } else if let Some(editor) = self.editor.as_mut() {
            for c in text.chars() {
                match c {
                    '\n' => editor.newline(),
                    '\r' => {}
                    c => editor.insert(c),
                }
            }
        } else if self.tab == Tab::Console && self.input_focus {
            for c in text.chars().filter(|c| !c.is_control()) {
                self.input.insert(c);
            }
        }
    }
}

// ----------------------------------------------------------------- app ---

pub enum Screen {
    Login(Login),
    Panel(Box<Panel>),
}

pub struct App {
    pub screen: Screen,
    pub tick: usize,
}

pub fn run(address: Option<String>, user: Option<String>) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::EnableBracketedPaste);
    let mut app = App {
        screen: Screen::Login(Login::new(address, user)),
        tick: 0,
    };
    let result = event_loop(&mut terminal, &mut app);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last_tick = Instant::now();
    loop {
        if let Screen::Panel(panel) = &mut app.screen {
            panel.pump();
            panel.tick = app.tick;
        }
        if let Screen::Login(login) = &mut app.screen
            && let Some(rx) = &login.rx
            && let Ok(result) = rx.try_recv()
        {
            login.rx = None;
            match result {
                Ok((Outcome::Untrusted { fingerprint }, address)) => {
                    login.state = LoginState::Trust { address, fingerprint };
                }
                Ok((Outcome::Connected(session), address)) => {
                    let username = login.form.value(1);
                    let password = std::mem::take(&mut login.form.fields[2].value);
                    remember(&address, &session.welcome.username);
                    let welcome = session.welcome.clone();
                    let link = Link::spawn(session, address.clone(), username, password);
                    app.screen = Screen::Panel(Box::new(Panel::new(link, address, welcome)));
                }
                Err(err) => login.state = LoginState::Error(err),
            }
        }

        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(app, key) {
                        return Ok(());
                    }
                }
                Event::Paste(text) => match &mut app.screen {
                    Screen::Panel(panel) => panel.paste(&text),
                    Screen::Login(login) => login.form.insert_str(&text),
                },
                _ => {}
            }
        }
        if last_tick.elapsed() >= Duration::from_millis(120) {
            app.tick = app.tick.wrapping_add(1);
            last_tick = Instant::now();
        }
    }
}

/// Returns true to exit.
fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match &mut app.screen {
        Screen::Panel(panel) => match panel.handle_key(key) {
            After::Stay => false,
            After::Quit => true,
            After::Logout => {
                let address = panel.address.clone();
                let user = panel.welcome.username.clone();
                app.screen = Screen::Login(Login::new(Some(address), Some(user)));
                false
            }
        },
        Screen::Login(login) => {
            if let LoginState::Trust { fingerprint, .. } = &login.state {
                let fingerprint = fingerprint.clone();
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    login.connect(Some(fingerprint));
                } else {
                    login.state = LoginState::Error("not trusted — nothing was sent to that host".into());
                }
                return false;
            }
            if matches!(login.state, LoginState::Connecting(_)) {
                return ctrl && key.code == KeyCode::Char('c');
            }
            let form = &mut login.form;
            match key.code {
                KeyCode::Esc => return true,
                KeyCode::Char('c') if ctrl => return true,
                KeyCode::Char('u') if ctrl => form.clear_field(),
                KeyCode::Char('r') if ctrl => login.cycle_profile(1),
                KeyCode::PageDown => login.cycle_profile(1),
                KeyCode::PageUp => login.cycle_profile(-1),
                KeyCode::Tab | KeyCode::Down => form.next(),
                KeyCode::BackTab | KeyCode::Up => form.previous(),
                KeyCode::Left => form.move_left(),
                KeyCode::Right => form.move_right(),
                KeyCode::Home => form.home(),
                KeyCode::End => form.end(),
                KeyCode::Backspace => form.backspace(),
                KeyCode::Delete => form.delete(),
                KeyCode::Char(c) if !ctrl => {
                    form.insert(c);
                    if !matches!(login.state, LoginState::Idle) {
                        login.state = LoginState::Idle;
                    }
                }
                KeyCode::Enter => {
                    if form.on_last_field() || !form.fields[2].value.is_empty() {
                        login.connect(None);
                    } else {
                        form.next();
                    }
                }
                _ => {}
            }
            false
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let tick = app.tick;
    match &mut app.screen {
        Screen::Login(login) => views::login(frame, login, tick),
        Screen::Panel(panel) => views::dashboard(frame, panel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_edits_safely() {
        let mut input = Input::default();
        for c in "say héllo".chars() {
            input.insert(c);
        }
        input.left();
        input.left();
        input.backspace();
        assert_eq!(input.value, "say hélo");
        input.cursor = 0;
        input.delete();
        assert_eq!(input.value, "ay hélo");
        assert_eq!(input.take(), "ay hélo");
        assert!(input.value.is_empty());
    }
}
