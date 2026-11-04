//! One Minecraft server: its process, console, players and lifecycle.
//!
//! The process is a plain child of the host. Its stdin is how console
//! commands get in; stdout and stderr are read line by line into a ring
//! buffer that panels page through by sequence number. A watcher thread
//! notices when the process exits and decides between "stopped", "restart",
//! and "crashed — try again" (with a crash-loop guard).

use std::collections::{BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Host;
use crate::proto::{ConsoleLine, ServerSettings, ServerState, ServerStatus, now};

const CONSOLE_LINES: usize = 5000;
pub const HISTORY: usize = 120;
/// More crashes than this inside `CRASH_WINDOW` and auto-restart gives up.
const CRASH_LIMIT: usize = 4;
const CRASH_WINDOW: Duration = Duration::from_secs(600);
const RESTART_DELAY: Duration = Duration::from_secs(5);

pub struct Runtime {
    pub settings: ServerSettings,
    pub state: ServerState,
    child: Option<Arc<Mutex<Child>>>,
    stdin: Option<ChildStdin>,
    pub pid: Option<u32>,
    pub console: VecDeque<ConsoleLine>,
    pub next_seq: u64,
    pub players: BTreeSet<String>,
    pub max_players: u32,
    pub started_at: Option<Instant>,
    pub cpu: f32,
    pub mem: u64,
    pub cpu_hist: VecDeque<u64>,
    pub mem_hist: VecDeque<u64>,
    pub restarts: u32,
    crash_times: Vec<Instant>,
    pub last_exit: Option<String>,
    /// Progress text for long jobs (downloads).
    pub activity: Option<String>,
    pub backup_running: bool,
    /// The user asked for a stop; an exit is expected.
    stopping: bool,
    restart_after_stop: bool,
    /// Bumped on every launch so a stale watcher cannot touch a newer process.
    generation: u64,
    pub disk_used: u64,
}

pub struct Instance {
    pub name: String,
    pub dir: PathBuf,
    pub rt: Mutex<Runtime>,
    host: Weak<Host>,
}

impl Instance {
    pub fn new(name: &str, dir: PathBuf, settings: ServerSettings, host: Weak<Host>) -> Arc<Instance> {
        let max_players = super::files::read_properties(&dir)
            .ok()
            .and_then(|p| p.into_iter().find(|p| p.key == "max-players"))
            .and_then(|p| p.value.parse().ok())
            .unwrap_or(20);
        Arc::new(Instance {
            name: name.to_string(),
            dir,
            host,
            rt: Mutex::new(Runtime {
                settings,
                state: ServerState::Stopped,
                child: None,
                stdin: None,
                pid: None,
                console: VecDeque::with_capacity(256),
                next_seq: 1,
                players: BTreeSet::new(),
                max_players,
                started_at: None,
                cpu: 0.0,
                mem: 0,
                cpu_hist: VecDeque::new(),
                mem_hist: VecDeque::new(),
                restarts: 0,
                crash_times: Vec::new(),
                last_exit: None,
                activity: None,
                backup_running: false,
                stopping: false,
                restart_after_stop: false,
                generation: 0,
                disk_used: 0,
            }),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Runtime> {
        self.rt.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// A line from the panel itself, so the console tells the whole story.
    pub fn log(&self, text: impl AsRef<str>) {
        let mut rt = self.lock();
        rt.push(format!("[mchost] {}", text.as_ref()));
    }

    pub fn status(&self) -> ServerStatus {
        let rt = self.lock();
        ServerStatus {
            name: self.name.clone(),
            kind: rt.settings.kind,
            version: rt.settings.version.clone(),
            state: rt.state,
            port: rt.settings.port,
            players: rt.players.iter().cloned().collect(),
            max_players: rt.max_players,
            cpu: rt.cpu,
            mem: rt.mem,
            mem_limit: rt.settings.memory_max_mb as u64 * 1024 * 1024,
            uptime_secs: rt.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0),
            cpu_history: rt.cpu_hist.iter().copied().collect(),
            mem_history: rt.mem_hist.iter().copied().collect(),
            auto_start: rt.settings.auto_start,
            auto_restart: rt.settings.auto_restart,
            restarts: rt.restarts,
            last_exit: rt.last_exit.clone(),
            activity: rt.activity.clone(),
            backup_running: rt.backup_running,
            disk_used: rt.disk_used,
        }
    }

    pub fn console_since(&self, since: u64) -> (Vec<ConsoleLine>, u64) {
        let rt = self.lock();
        // A fresh panel gets the recent tail, not the start of the buffer.
        let skip = if since == 0 { rt.console.len().saturating_sub(1000) } else { 0 };
        let lines: Vec<ConsoleLine> = rt
            .console
            .iter()
            .skip(skip)
            .filter(|l| l.seq >= since)
            .take(2000)
            .cloned()
            .collect();
        let next = lines.last().map(|l| l.seq + 1).unwrap_or(rt.next_seq.max(since).min(rt.next_seq));
        (lines, next)
    }

    // ------------------------------------------------------------ power ---

    pub fn start(self: &Arc<Self>) -> Result<String> {
        let mut rt = self.lock();
        match rt.state {
            ServerState::Installing => bail!("{} is still installing", self.name),
            s if s.is_live() => bail!("{} is already {s}", self.name),
            _ => {}
        }
        if rt.backup_running {
            bail!("a backup restore or snapshot is in progress");
        }
        let settings = rt.settings.clone();
        let jar = self.dir.join(&settings.jar);
        if !jar.exists() {
            bail!(
                "{} is missing — reinstall from the Config view, or upload a jar and set its name",
                settings.jar
            );
        }
        if !eula_accepted(&self.dir) {
            bail!("the Minecraft EULA is not accepted — set eula=true in eula.txt (Files view)");
        }
        // Keep the port the panel shows and the port the server binds in step.
        let _ = super::files::set_property(&self.dir, "server-port", &settings.port.to_string());

        let mut command = Command::new(&settings.java);
        command
            .current_dir(&self.dir)
            .arg(format!("-Xms{}M", settings.memory_min_mb.min(settings.memory_max_mb)))
            .arg(format!("-Xmx{}M", settings.memory_max_mb))
            .args(settings.jvm_args.split_whitespace())
            .arg("-jar")
            .arg(&settings.jar)
            .arg("nogui")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            // Own process group: a Ctrl-C aimed at a foreground `mchost serve`
            // must not reach the Minecraft servers too, or they die unsaved.
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("launching `{}` — is Java installed?", settings.java))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        rt.generation += 1;
        let generation = rt.generation;
        rt.state = ServerState::Starting;
        rt.stdin = stdin;
        rt.pid = Some(pid);
        rt.child = Some(Arc::new(Mutex::new(child)));
        rt.started_at = Some(Instant::now());
        rt.stopping = false;
        rt.restart_after_stop = false;
        rt.players.clear();
        rt.last_exit = None;
        rt.max_players = super::files::read_properties(&self.dir)
            .ok()
            .and_then(|p| p.into_iter().find(|p| p.key == "max-players"))
            .and_then(|p| p.value.parse().ok())
            .unwrap_or(20);
        rt.push(format!(
            "[mchost] starting {} {} with {} MiB (pid {pid})",
            settings.kind, settings.version, settings.memory_max_mb
        ));
        drop(rt);

        if let Some(out) = stdout {
            self.spawn_reader(out, generation);
        }
        if let Some(err) = stderr {
            self.spawn_reader(err, generation);
        }
        self.spawn_watcher(generation);
        Ok(format!("starting {}", self.name))
    }

    pub fn stop(self: &Arc<Self>) -> Result<String> {
        let mut rt = self.lock();
        if !rt.state.is_live() {
            bail!("{} is not running", self.name);
        }
        if rt.stopping {
            return Ok(format!("{} is already stopping", self.name));
        }
        rt.stopping = true;
        rt.state = ServerState::Stopping;
        rt.push("[mchost] stopping (sending `stop`)".to_string());
        let wrote = rt.write_stdin("stop").is_ok();
        let timeout = Duration::from_secs(rt.settings.stop_timeout_secs.max(5));
        let generation = rt.generation;
        drop(rt);
        if !wrote {
            // No console to ask politely through.
            self.kill_process(generation);
            return Ok(format!("{} had no console; killed", self.name));
        }

        // If it hangs during shutdown, stop waiting.
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(250));
                let done = {
                    let rt = me.lock();
                    rt.generation != generation || rt.child.is_none()
                };
                if done {
                    return;
                }
            }
            me.log(format!("did not stop within {}s — killing", timeout.as_secs()));
            me.kill_process(generation);
        });
        Ok(format!("stopping {}", self.name))
    }

    pub fn restart(self: &Arc<Self>) -> Result<String> {
        let live = self.lock().state.is_live();
        if !live {
            return self.start();
        }
        self.stop()?;
        self.lock().restart_after_stop = true;
        Ok(format!("restarting {}", self.name))
    }

    pub fn kill(self: &Arc<Self>) -> Result<String> {
        let generation = {
            let mut rt = self.lock();
            if rt.child.is_none() {
                bail!("{} is not running", self.name);
            }
            rt.stopping = true;
            rt.restart_after_stop = false;
            rt.push("[mchost] killing the process".to_string());
            rt.generation
        };
        self.kill_process(generation);
        Ok(format!("killed {}", self.name))
    }

    fn kill_process(&self, generation: u64) {
        let child = {
            let rt = self.lock();
            if rt.generation != generation {
                return;
            }
            rt.child.clone()
        };
        if let Some(child) = child {
            let _ = child.lock().unwrap().kill();
        }
    }

    pub fn send_command(&self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() {
            bail!("empty command");
        }
        if line.contains(['\n', '\r']) {
            bail!("one command at a time");
        }
        let mut rt = self.lock();
        if !rt.state.is_live() {
            bail!("{} is not running", self.name);
        }
        rt.push(format!("> {line}"));
        rt.write_stdin(line)
    }

    pub fn is_running(&self) -> bool {
        self.lock().state.is_live()
    }

    // ---------------------------------------------------------- threads ---

    fn spawn_reader(self: &Arc<Self>, stream: impl Read + Send + 'static, generation: u64) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = strip_ansi(&String::from_utf8_lossy(&buf));
                let text = text.trim_end_matches(['\r', '\n']).to_string();
                let mut rt = me.lock();
                if rt.generation != generation {
                    break;
                }
                match parse_event(&text) {
                    Some(Event::Ready) => {
                        if rt.state == ServerState::Starting {
                            rt.state = ServerState::Running;
                            let secs = rt.started_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
                            rt.push(format!("[mchost] server is up ({secs:.1}s)"));
                        }
                    }
                    Some(Event::Joined(name)) => {
                        rt.players.insert(name);
                    }
                    Some(Event::Left(name)) => {
                        rt.players.remove(&name);
                    }
                    Some(Event::List(names)) => {
                        rt.players = names.into_iter().collect();
                    }
                    None => {}
                }
                rt.push(text);
            }
        });
    }

    fn spawn_watcher(self: &Arc<Self>, generation: u64) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let status = loop {
                std::thread::sleep(Duration::from_millis(250));
                let child = {
                    let rt = me.lock();
                    if rt.generation != generation {
                        return;
                    }
                    rt.child.clone()
                };
                let Some(child) = child else { return };
                let polled = child.lock().unwrap().try_wait();
                match polled {
                    Ok(Some(status)) => break describe_exit(status),
                    Ok(None) => continue,
                    Err(err) => break format!("lost track of the process: {err}"),
                }
            };
            me.on_exit(generation, status);
        });
    }

    fn on_exit(self: &Arc<Self>, generation: u64, status: String) {
        let mut rt = self.lock();
        if rt.generation != generation {
            return;
        }
        let uptime = rt.started_at.map(|t| t.elapsed()).unwrap_or_default();
        rt.child = None;
        rt.stdin = None;
        rt.pid = None;
        rt.started_at = None;
        rt.players.clear();
        rt.cpu = 0.0;
        rt.mem = 0;
        rt.last_exit = Some(status.clone());

        if rt.stopping {
            rt.state = ServerState::Stopped;
            rt.stopping = false;
            rt.push(format!("[mchost] stopped ({status})"));
            if rt.restart_after_stop {
                rt.restart_after_stop = false;
                drop(rt);
                if let Err(err) = self.start() {
                    self.log(format!("restart failed: {err:#}"));
                }
            }
            return;
        }

        rt.state = ServerState::Crashed;
        rt.push(format!(
            "[mchost] the server exited unexpectedly after {}s ({status})",
            uptime.as_secs()
        ));
        let now_i = Instant::now();
        rt.crash_times.retain(|t| now_i.duration_since(*t) < CRASH_WINDOW);
        rt.crash_times.push(now_i);
        if !rt.settings.auto_restart {
            return;
        }
        let crashes = rt.crash_times.len();
        if crashes > CRASH_LIMIT {
            rt.push(format!(
                "[mchost] crashed {} times in {} minutes — not restarting again; fix it and start by hand",
                crashes,
                CRASH_WINDOW.as_secs() / 60
            ));
            return;
        }
        rt.push(format!(
            "[mchost] auto-restart in {}s",
            RESTART_DELAY.as_secs()
        ));
        drop(rt);
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(RESTART_DELAY);
            let still_crashed = {
                let rt = me.lock();
                rt.state == ServerState::Crashed && rt.generation == generation
            };
            if still_crashed {
                me.lock().restarts += 1;
                if let Err(err) = me.start() {
                    me.log(format!("auto-restart failed: {err:#}"));
                }
            }
        });
    }

    /// Persist settings through the host, if it is still around.
    pub fn save(&self) {
        if let Some(host) = self.host.upgrade() {
            host.save_servers();
        }
    }
}

impl Runtime {
    pub fn push(&mut self, text: String) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.console.len() >= CONSOLE_LINES {
            self.console.pop_front();
        }
        self.console.push_back(ConsoleLine {
            seq,
            time: now(),
            text,
        });
    }

    fn write_stdin(&mut self, line: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().context("the server has no console input")?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|_| stdin.flush())
            .context("writing to the server console")
    }

    pub fn record_sample(&mut self, cpu: f32, mem: u64) {
        self.cpu = cpu;
        self.mem = mem;
        push_capped(&mut self.cpu_hist, cpu.round() as u64);
        push_capped(&mut self.mem_hist, mem / (1024 * 1024));
    }
}

pub fn push_capped(history: &mut VecDeque<u64>, value: u64) {
    if history.len() >= HISTORY {
        history.pop_front();
    }
    history.push_back(value);
}

fn describe_exit(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "exited".into()
}

pub fn eula_accepted(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(dir.join("eula.txt"))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .any(|l| l.eq_ignore_ascii_case("eula=true"))
        })
        .unwrap_or(false)
}

pub fn write_eula(dir: &std::path::Path) -> Result<()> {
    std::fs::write(
        dir.join("eula.txt"),
        format!(
            "# Accepted through mchost on {}\n# https://aka.ms/MinecraftEULA\neula=true\n",
            crate::proto::fmt_time(now())
        ),
    )
    .context("writing eula.txt")
}

// ------------------------------------------------------------ parsing ---

#[derive(Debug, PartialEq)]
pub enum Event {
    Ready,
    Joined(String),
    Left(String),
    List(Vec<String>),
}

/// The message part of a log line: everything after the `[time level]: `
/// prefix that vanilla, Paper and Fabric all use in some form.
fn message(line: &str) -> &str {
    match line.find("]: ") {
        Some(i) => &line[i + 3..],
        None => line,
    }
}

fn is_player_name(name: &str) -> bool {
    // Java names are 3–16 of [A-Za-z0-9_]; Bedrock players via Geyser carry a
    // prefix such as `.`, and some proxies allow longer names.
    let core = name.trim_start_matches(['.', '*']);
    !core.is_empty()
        && name.len() <= 32
        && core.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn parse_event(line: &str) -> Option<Event> {
    let msg = message(line).trim();
    // Chat lines look like `<name> text` — never treat them as events, or a
    // player could fake a join by typing one.
    if msg.starts_with('<') || msg.contains("] <") {
        return None;
    }
    if msg.starts_with("Done (") && msg.contains("help") {
        return Some(Event::Ready);
    }
    if let Some(name) = msg.strip_suffix(" joined the game")
        && is_player_name(name)
    {
        return Some(Event::Joined(name.to_string()));
    }
    if let Some(name) = msg.strip_suffix(" left the game")
        && is_player_name(name)
    {
        return Some(Event::Left(name.to_string()));
    }
    // `There are 2 of a max of 20 players online: Alex, Steve`
    if msg.starts_with("There are ") && msg.contains(" players online:") {
        let names = msg.split_once(" players online:").map(|(_, n)| n).unwrap_or("");
        let names: Vec<String> = names
            .split(',')
            .map(str::trim)
            .filter(|n| is_player_name(n))
            .map(str::to_string)
            .collect();
        return Some(Event::List(names));
    }
    None
}

/// Drop terminal colour codes; Paper and many plugins emit them.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else if c == '§' {
            // Minecraft formatting codes: `§a`, `§l`...
            chars.next();
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_is_detected_on_vanilla_and_paper() {
        assert_eq!(
            parse_event("[12:00:01] [Server thread/INFO]: Done (3.512s)! For help, type \"help\""),
            Some(Event::Ready)
        );
        assert_eq!(
            parse_event("[12:00:01 INFO]: Done (8.1s)! For help, type \"help\""),
            Some(Event::Ready)
        );
    }

    #[test]
    fn joins_and_leaves_are_tracked() {
        assert_eq!(
            parse_event("[12:00:01] [Server thread/INFO]: Alex joined the game"),
            Some(Event::Joined("Alex".into()))
        );
        assert_eq!(
            parse_event("[12:00:01 INFO]: Steve_2 left the game"),
            Some(Event::Left("Steve_2".into()))
        );
        assert_eq!(
            parse_event("[12:00:01 INFO]: .BedrockGuy joined the game"),
            Some(Event::Joined(".BedrockGuy".into()))
        );
    }

    #[test]
    fn chat_cannot_fake_a_join() {
        assert_eq!(parse_event("[12:00:01 INFO]: <Alex> Bob joined the game"), None);
        assert_eq!(
            parse_event("[12:00:01 INFO]: [Not Secure] <Alex> Bob joined the game"),
            None
        );
        assert_eq!(parse_event("[12:00:01 INFO]: two words joined the game"), None);
    }

    #[test]
    fn list_output_replaces_the_player_set() {
        assert_eq!(
            parse_event("[12:00:01] [Server thread/INFO]: There are 2 of a max of 20 players online: Alex, Steve"),
            Some(Event::List(vec!["Alex".into(), "Steve".into()]))
        );
        assert_eq!(
            parse_event("[12:00:01 INFO]: There are 0 of a max of 20 players online:"),
            Some(Event::List(vec![]))
        );
    }

    #[test]
    fn colour_codes_are_stripped() {
        assert_eq!(strip_ansi("\u{1b}[33;1mWARN\u{1b}[m text"), "WARN text");
        assert_eq!(strip_ansi("§aGreen §lbold"), "Green bold");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}after"), "after");
    }

    #[test]
    fn eula_detection() {
        let dir = std::env::temp_dir().join(format!("mchost-eula-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!eula_accepted(&dir));
        std::fs::write(dir.join("eula.txt"), "eula=false\n").unwrap();
        assert!(!eula_accepted(&dir));
        write_eula(&dir).unwrap();
        assert!(eula_accepted(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
