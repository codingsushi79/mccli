//! The wire protocol between a panel (the TUI) and a host.
//!
//! One JSON object per line, inside TLS. The first message on a connection
//! must be `Login`; everything after it runs with that user's permissions.
//! Every request gets exactly one `Reply`, in order, so the client never has
//! to match ids.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_PORT: u16 = 25580;
pub const PROTOCOL: u32 = 1;
/// Longest single message either side will accept. File writes are the
/// biggest thing on the wire.
pub const MAX_LINE: usize = 8 * 1024 * 1024;

// ------------------------------------------------------------- enums ---

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Watch only: stats, console, players.
    Viewer,
    /// Run servers: power, console input, files, players, backups, tasks.
    Operator,
    /// Everything, including creating servers and managing users.
    Admin,
}

impl Role {
    pub fn parse(input: &str) -> Option<Role> {
        match input.trim().to_ascii_lowercase().as_str() {
            "viewer" | "view" | "read" | "readonly" => Some(Role::Viewer),
            "operator" | "op" | "mod" | "moderator" => Some(Role::Operator),
            "admin" | "owner" | "root" => Some(Role::Admin),
            _ => None,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    Vanilla,
    Paper,
    Purpur,
    Fabric,
    /// Bring your own jar; nothing is downloaded.
    Custom,
}

impl ServerKind {
    pub fn parse(input: &str) -> Option<ServerKind> {
        match input.trim().to_ascii_lowercase().as_str() {
            "vanilla" | "mojang" | "minecraft" => Some(ServerKind::Vanilla),
            "paper" | "papermc" => Some(ServerKind::Paper),
            "purpur" => Some(ServerKind::Purpur),
            "fabric" => Some(ServerKind::Fabric),
            "custom" | "jar" => Some(ServerKind::Custom),
            _ => None,
        }
    }
}

impl fmt::Display for ServerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ServerKind::Vanilla => "vanilla",
            ServerKind::Paper => "paper",
            ServerKind::Purpur => "purpur",
            ServerKind::Fabric => "fabric",
            ServerKind::Custom => "custom",
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    Stopped,
    Installing,
    Starting,
    Running,
    Stopping,
    Crashed,
}

impl ServerState {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            ServerState::Starting | ServerState::Running | ServerState::Stopping
        )
    }
}

impl fmt::Display for ServerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ServerState::Stopped => "stopped",
            ServerState::Installing => "installing",
            ServerState::Starting => "starting",
            ServerState::Running => "running",
            ServerState::Stopping => "stopping",
            ServerState::Crashed => "crashed",
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Power {
    Start,
    Stop,
    Restart,
    Kill,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerAction {
    Op,
    Deop,
    Kick,
    Ban,
    Pardon,
    WhitelistAdd,
    WhitelistRemove,
    WhitelistOn,
    WhitelistOff,
    /// Private message (`tell`).
    Message,
    /// `extra` is the game mode.
    Gamemode,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskAction {
    /// Run `payload` as a console command.
    Command,
    /// Broadcast `payload` with `say`.
    Say,
    Start,
    Stop,
    Restart,
    Backup,
}

impl TaskAction {
    pub fn parse(input: &str) -> Option<TaskAction> {
        match input.trim().to_ascii_lowercase().as_str() {
            "command" | "cmd" => Some(TaskAction::Command),
            "say" | "broadcast" => Some(TaskAction::Say),
            "start" => Some(TaskAction::Start),
            "stop" => Some(TaskAction::Stop),
            "restart" => Some(TaskAction::Restart),
            "backup" => Some(TaskAction::Backup),
            _ => None,
        }
    }
}

impl fmt::Display for TaskAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TaskAction::Command => "command",
            TaskAction::Say => "say",
            TaskAction::Start => "start",
            TaskAction::Stop => "stop",
            TaskAction::Restart => "restart",
            TaskAction::Backup => "backup",
        })
    }
}

/// When a scheduled task fires.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum When {
    Every { minutes: u32 },
    Daily { hour: u8, minute: u8 },
}

impl When {
    /// `30m`, `6h`, `every 2h`, `daily 04:00`, `04:00`.
    pub fn parse(input: &str) -> Result<When, String> {
        let text = input.trim().to_ascii_lowercase();
        let text = text.trim_start_matches("every").trim();
        if let Some(rest) = text.strip_prefix("daily").or_else(|| text.strip_prefix("at")) {
            return parse_clock(rest.trim());
        }
        if text.contains(':') {
            return parse_clock(text);
        }
        let (number, unit) = text
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| text.split_at(i))
            .unwrap_or((text, "m"));
        let n: u32 = number
            .trim()
            .parse()
            .map_err(|_| format!("`{input}` — try `30m`, `6h` or `daily 04:00`"))?;
        let minutes = match unit.trim() {
            "" | "m" | "min" | "mins" | "minute" | "minutes" => n,
            "h" | "hr" | "hrs" | "hour" | "hours" => n.saturating_mul(60),
            "d" | "day" | "days" => n.saturating_mul(1440),
            other => return Err(format!("unknown unit `{other}` — use m, h or d")),
        };
        if minutes == 0 {
            return Err("the interval has to be at least one minute".into());
        }
        Ok(When::Every { minutes })
    }
}

fn parse_clock(text: &str) -> Result<When, String> {
    let (h, m) = text
        .split_once(':')
        .ok_or_else(|| format!("`{text}` should look like HH:MM"))?;
    let hour: u8 = h.trim().parse().map_err(|_| format!("bad hour `{h}`"))?;
    let minute: u8 = m.trim().parse().map_err(|_| format!("bad minute `{m}`"))?;
    if hour > 23 || minute > 59 {
        return Err(format!("`{text}` is not a time of day"));
    }
    Ok(When::Daily { hour, minute })
}

impl fmt::Display for When {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            When::Every { minutes } if minutes % 1440 == 0 => write!(f, "every {}d", minutes / 1440),
            When::Every { minutes } if minutes % 60 == 0 => write!(f, "every {}h", minutes / 60),
            When::Every { minutes } => write!(f, "every {minutes}m"),
            When::Daily { hour, minute } => write!(f, "daily {hour:02}:{minute:02}"),
        }
    }
}

// ----------------------------------------------------------- payloads ---

/// Per-server launch settings, editable from the Config view.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ServerSettings {
    pub kind: ServerKind,
    pub version: String,
    /// Path to the java binary. Only admins may change it: it is, in effect,
    /// the command the host runs.
    pub java: String,
    pub jar: String,
    pub memory_min_mb: u32,
    pub memory_max_mb: u32,
    #[serde(default)]
    pub jvm_args: String,
    pub port: u16,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default = "yes")]
    pub auto_restart: bool,
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_secs: u64,
}

fn yes() -> bool {
    true
}

fn default_stop_timeout() -> u64 {
    60
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NewServer {
    pub name: String,
    pub kind: ServerKind,
    /// `latest` or a concrete game version.
    pub version: String,
    pub memory_mb: u32,
    pub port: u16,
    pub java: String,
    /// The Minecraft EULA has to be accepted before a server will start.
    pub eula: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NewSchedule {
    pub name: String,
    pub server: String,
    pub action: TaskAction,
    #[serde(default)]
    pub payload: String,
    pub when: When,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Login {
        username: String,
        password: String,
        protocol: u32,
    },
    Ping,
    Snapshot,
    Console {
        server: String,
        since: u64,
    },
    Command {
        server: String,
        line: String,
    },
    Power {
        server: String,
        action: Power,
    },
    CreateServer(NewServer),
    DeleteServer {
        server: String,
        delete_files: bool,
    },
    GetSettings {
        server: String,
    },
    UpdateSettings {
        server: String,
        settings: ServerSettings,
    },
    /// Download a different jar (version change, or a repair).
    Reinstall {
        server: String,
        kind: ServerKind,
        version: String,
    },
    Players {
        server: String,
    },
    Player {
        server: String,
        action: PlayerAction,
        name: String,
        #[serde(default)]
        extra: String,
    },
    ListFiles {
        server: String,
        path: String,
    },
    ReadFile {
        server: String,
        path: String,
    },
    WriteFile {
        server: String,
        path: String,
        content: String,
    },
    DeletePath {
        server: String,
        path: String,
    },
    RenamePath {
        server: String,
        from: String,
        to: String,
    },
    MakeDir {
        server: String,
        path: String,
    },
    Properties {
        server: String,
    },
    SetProperty {
        server: String,
        key: String,
        value: String,
    },
    Backups {
        server: String,
    },
    CreateBackup {
        server: String,
        note: String,
    },
    RestoreBackup {
        server: String,
        id: String,
    },
    DeleteBackup {
        server: String,
        id: String,
    },
    Schedules,
    AddSchedule(NewSchedule),
    RemoveSchedule {
        id: u64,
    },
    ToggleSchedule {
        id: u64,
    },
    RunSchedule {
        id: u64,
    },
    Users,
    AddUser {
        username: String,
        password: String,
        role: Role,
        servers: Vec<String>,
    },
    RemoveUser {
        username: String,
    },
    SetPassword {
        username: String,
        password: String,
    },
    SetRole {
        username: String,
        role: Role,
        servers: Vec<String>,
    },
    Activity,
}

impl Request {
    /// Short name for the activity log.
    pub fn name(&self) -> &'static str {
        match self {
            Request::Login { .. } => "login",
            Request::Ping => "ping",
            Request::Snapshot => "snapshot",
            Request::Console { .. } => "console",
            Request::Command { .. } => "command",
            Request::Power { .. } => "power",
            Request::CreateServer(_) => "create_server",
            Request::DeleteServer { .. } => "delete_server",
            Request::GetSettings { .. } => "get_settings",
            Request::UpdateSettings { .. } => "update_settings",
            Request::Reinstall { .. } => "reinstall",
            Request::Players { .. } => "players",
            Request::Player { .. } => "player",
            Request::ListFiles { .. } => "list_files",
            Request::ReadFile { .. } => "read_file",
            Request::WriteFile { .. } => "write_file",
            Request::DeletePath { .. } => "delete_path",
            Request::RenamePath { .. } => "rename_path",
            Request::MakeDir { .. } => "make_dir",
            Request::Properties { .. } => "properties",
            Request::SetProperty { .. } => "set_property",
            Request::Backups { .. } => "backups",
            Request::CreateBackup { .. } => "create_backup",
            Request::RestoreBackup { .. } => "restore_backup",
            Request::DeleteBackup { .. } => "delete_backup",
            Request::Schedules => "schedules",
            Request::AddSchedule(_) => "add_schedule",
            Request::RemoveSchedule { .. } => "remove_schedule",
            Request::ToggleSchedule { .. } => "toggle_schedule",
            Request::RunSchedule { .. } => "run_schedule",
            Request::Users => "users",
            Request::AddUser { .. } => "add_user",
            Request::RemoveUser { .. } => "remove_user",
            Request::SetPassword { .. } => "set_password",
            Request::SetRole { .. } => "set_role",
            Request::Activity => "activity",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn ok(data: impl Serialize) -> Reply {
        Reply {
            ok: true,
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            error: None,
        }
    }

    pub fn err(text: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            data: Value::Null,
            error: Some(text.into()),
        }
    }
}

// ------------------------------------------------------------ results ---

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Welcome {
    pub username: String,
    pub role: Role,
    pub host_name: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HostInfo {
    pub name: String,
    pub version: String,
    pub os: String,
    pub cpu_brand: String,
    pub cores: usize,
    pub cpu_usage: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub load: [f64; 3],
    pub uptime_secs: u64,
    pub panel_uptime_secs: u64,
    pub cpu_history: Vec<u64>,
    pub java: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerStatus {
    pub name: String,
    pub kind: ServerKind,
    pub version: String,
    pub state: ServerState,
    pub port: u16,
    pub players: Vec<String>,
    pub max_players: u32,
    /// Percent of one core, as `top` reports it.
    pub cpu: f32,
    pub mem: u64,
    pub mem_limit: u64,
    pub uptime_secs: u64,
    pub cpu_history: Vec<u64>,
    pub mem_history: Vec<u64>,
    pub auto_start: bool,
    pub auto_restart: bool,
    pub restarts: u32,
    pub last_exit: Option<String>,
    /// Progress text while a jar downloads.
    pub activity: Option<String>,
    pub backup_running: bool,
    pub disk_used: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Me {
    pub username: String,
    pub role: Role,
    pub servers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub host: HostInfo,
    pub servers: Vec<ServerStatus>,
    pub me: Me,
    pub sessions: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsoleLine {
    pub seq: u64,
    pub time: i64,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsoleChunk {
    pub lines: Vec<ConsoleLine>,
    /// Pass back as `since` next time.
    pub next: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub modified: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Listing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileContent {
    pub path: String,
    pub content: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BanEntry {
    pub name: String,
    pub reason: String,
    pub source: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PlayersInfo {
    pub running: bool,
    pub online: Vec<String>,
    pub ops: Vec<String>,
    pub whitelist: Vec<String>,
    pub banned: Vec<BanEntry>,
    pub whitelist_enabled: bool,
    pub max_players: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Property {
    pub key: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BackupInfo {
    pub id: String,
    pub created: i64,
    pub size: u64,
    pub note: String,
    #[serde(default)]
    pub by: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Schedule {
    pub id: u64,
    pub name: String,
    pub server: String,
    pub action: TaskAction,
    #[serde(default)]
    pub payload: String,
    pub when: When,
    pub enabled: bool,
    pub created: i64,
    pub last_run: Option<i64>,
    pub last_result: Option<String>,
    pub next_run: Option<i64>,
    #[serde(default)]
    pub created_by: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserInfo {
    pub username: String,
    pub role: Role,
    pub servers: Vec<String>,
    pub created: i64,
    pub last_login: Option<i64>,
    pub last_ip: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ActivityEntry {
    pub time: i64,
    pub user: String,
    pub ip: String,
    pub action: String,
    pub detail: String,
    pub ok: bool,
}

// ------------------------------------------------------------- format ---

pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `3m ago`, `in 2h` — relative to now.
pub fn fmt_relative(ts: i64) -> String {
    let delta = ts - now();
    let span = fmt_duration(delta.unsigned_abs());
    let short = span.split(' ').next().unwrap_or("").to_string();
    if delta.abs() < 5 {
        "now".into()
    } else if delta < 0 {
        format!("{short} ago")
    } else {
        format!("in {short}")
    }
}

/// Local wall-clock time, `2026-11-04 17:30`.
pub fn fmt_time(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_times_parse() {
        assert_eq!(When::parse("30m"), Ok(When::Every { minutes: 30 }));
        assert_eq!(When::parse("every 6h"), Ok(When::Every { minutes: 360 }));
        assert_eq!(When::parse("2 hours"), Ok(When::Every { minutes: 120 }));
        assert_eq!(When::parse("1d"), Ok(When::Every { minutes: 1440 }));
        assert_eq!(
            When::parse("daily 04:30"),
            Ok(When::Daily { hour: 4, minute: 30 })
        );
        assert_eq!(When::parse("23:05"), Ok(When::Daily { hour: 23, minute: 5 }));
        assert!(When::parse("25:00").is_err());
        assert!(When::parse("0m").is_err());
        assert!(When::parse("soon").is_err());
        assert_eq!(When::Every { minutes: 360 }.to_string(), "every 6h");
        assert_eq!(When::Daily { hour: 4, minute: 0 }.to_string(), "daily 04:00");
    }

    #[test]
    fn requests_round_trip() {
        let request = Request::Power {
            server: "survival".into(),
            action: Power::Restart,
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"op\":\"power\""));
        let back: Request = serde_json::from_str(&text).unwrap();
        assert!(matches!(back, Request::Power { action: Power::Restart, .. }));
    }

    #[test]
    fn bytes_format() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
