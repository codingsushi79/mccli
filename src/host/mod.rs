//! The host: runs Minecraft servers and lets panels log in to control them.
//!
//! `mchost serve` listens on a TLS port (25580 by default). A panel on any
//! machine connects with the host's address plus a username and password and
//! gets exactly the control that account's role allows. Servers are child
//! processes of the host, so closing a panel never affects them.

pub mod backup;
pub mod files;
pub mod install;
pub mod instance;
pub mod schedule;
pub mod users;

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustls::{ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::paths;
use crate::proto::*;
use instance::Instance;
use users::{Throttle, User, UserStore};

const MAX_SESSIONS: usize = 64;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const ACTIVITY_KEEP: usize = 1000;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Default)]
pub struct ScheduleStore {
    pub tasks: Vec<Schedule>,
    pub next_id: u64,
}

impl ScheduleStore {
    fn path() -> std::path::PathBuf {
        paths::data_dir().join("schedules.json")
    }

    fn load() -> ScheduleStore {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = paths::write_atomic(&Self::path(), text.as_bytes());
        }
    }
}

pub struct Host {
    pub name: String,
    pub instances: RwLock<BTreeMap<String, Arc<Instance>>>,
    pub users: Mutex<UserStore>,
    throttle: Mutex<Throttle>,
    pub schedules: Mutex<ScheduleStore>,
    activity: Mutex<VecDeque<ActivityEntry>>,
    stats: Mutex<HostInfo>,
    sessions: AtomicUsize,
    started: Instant,
}

fn servers_file() -> std::path::PathBuf {
    paths::data_dir().join("servers.json")
}

fn activity_file() -> std::path::PathBuf {
    paths::data_dir().join("activity.log")
}

impl Host {
    pub fn load(name: String) -> Result<Arc<Host>> {
        paths::ensure_host_dirs()?;
        let users = UserStore::load()?;
        let settings: BTreeMap<String, ServerSettings> = match std::fs::read_to_string(servers_file()) {
            Ok(text) => serde_json::from_str(&text).context("parsing servers.json")?,
            Err(_) => BTreeMap::new(),
        };
        let activity: VecDeque<ActivityEntry> = std::fs::read_to_string(activity_file())
            .unwrap_or_default()
            .lines()
            .rev()
            .take(ACTIVITY_KEEP)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        Ok(Arc::new_cyclic(|weak| {
            let instances = settings
                .into_iter()
                .map(|(name, s)| {
                    let inst = Instance::new(&name, paths::server_dir(&name), s, weak.clone());
                    (name, inst)
                })
                .collect();
            Host {
                name,
                instances: RwLock::new(instances),
                users: Mutex::new(users),
                throttle: Mutex::new(Throttle::default()),
                schedules: Mutex::new(ScheduleStore::load()),
                activity: Mutex::new(activity),
                stats: Mutex::new(HostInfo::default()),
                sessions: AtomicUsize::new(0),
                started: Instant::now(),
            }
        }))
    }

    pub fn instance(&self, name: &str) -> Option<Arc<Instance>> {
        self.instances.read().unwrap().get(name).cloned()
    }

    pub fn all_instances(&self) -> Vec<Arc<Instance>> {
        self.instances.read().unwrap().values().cloned().collect()
    }

    pub fn save_servers(&self) {
        let map: BTreeMap<String, ServerSettings> = self
            .all_instances()
            .into_iter()
            .map(|i| (i.name.clone(), i.lock().settings.clone()))
            .collect();
        if let Ok(text) = serde_json::to_string_pretty(&map) {
            let _ = paths::write_atomic(&servers_file(), text.as_bytes());
        }
    }

    fn record(&self, user: &str, ip: &str, action: &str, detail: String, ok: bool) {
        let entry = ActivityEntry {
            time: now(),
            user: user.to_string(),
            ip: ip.to_string(),
            action: action.to_string(),
            detail,
            ok,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            use std::fs::OpenOptions;
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(activity_file()) {
                let _ = writeln!(file, "{line}");
            }
        }
        let mut log = self.activity.lock().unwrap();
        if log.len() >= ACTIVITY_KEEP {
            log.pop_front();
        }
        log.push_back(entry);
    }

    fn port_in_use(&self, port: u16, except: Option<&str>) -> Option<String> {
        self.all_instances()
            .into_iter()
            .find(|i| Some(i.name.as_str()) != except && i.lock().settings.port == port)
            .map(|i| i.name.clone())
    }
}

// --------------------------------------------------------------- serve ---

pub struct ServeOptions {
    pub bind: String,
    pub name: Option<String>,
}

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn serve(options: ServeOptions) -> Result<()> {
    let name = options
        .name
        .or_else(sysinfo::System::host_name)
        .unwrap_or_else(|| "mchost".into());
    let host = Host::load(name)?;

    {
        let mut users = host.users.lock().unwrap();
        if users.users.is_empty() {
            let password = users::generate_password();
            users.add("admin", &password, Role::Admin, vec![])?;
            users.save()?;
            println!();
            println!("  First run — created the admin account:");
            println!();
            println!("      username  admin");
            println!("      password  {password}");
            println!();
            println!("  It is shown once. Change it with `mchost user passwd admin`,");
            println!("  or from the Users view in the panel.");
            println!();
        }
    }

    let tls = crate::tls::server_config()?;
    let fingerprint = crate::tls::local_fingerprint()?;
    let listener = TcpListener::bind(&options.bind)
        .with_context(|| format!("cannot listen on {}", options.bind))?;
    listener.set_nonblocking(true)?;

    println!("mchost {} — host `{}`", env!("CARGO_PKG_VERSION"), host.name);
    println!("  listening    {}", options.bind);
    println!("  data         {}", paths::data_dir().display());
    println!("  fingerprint  {fingerprint}");
    println!("  connect with `mchost connect <this machine's address>`");
    println!("  Ctrl-C stops the host and shuts every server down cleanly.");

    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        // A panel hanging up mid-write must not kill the host.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    spawn_stats(host.clone());
    schedule::spawn_runner(host.clone());
    {
        // Give each task a next run if it lacks one (new install, or clock moved).
        let mut tasks = host.schedules.lock().unwrap();
        let t = now();
        for task in tasks.tasks.iter_mut() {
            if task.next_run.is_none_or(|n| n < t - 3600) {
                task.next_run = Some(schedule::next_after(task.when, t));
            }
        }
        tasks.save();
    }
    for inst in host.all_instances() {
        if inst.lock().settings.auto_start {
            match inst.start() {
                Ok(_) => println!("  auto-start   {}", inst.name),
                Err(err) => println!("  auto-start   {} failed: {err:#}", inst.name),
            }
        }
    }

    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let host = host.clone();
                let tls = tls.clone();
                std::thread::spawn(move || {
                    if host.sessions.load(Ordering::SeqCst) >= MAX_SESSIONS {
                        return;
                    }
                    host.sessions.fetch_add(1, Ordering::SeqCst);
                    let _ = handle_connection(&host, tls, stream, peer);
                    host.sessions.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                eprintln!("accept failed: {err}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    println!("\nshutting down — stopping servers…");
    let live: Vec<_> = host.all_instances().into_iter().filter(|i| i.is_running()).collect();
    for inst in &live {
        let _ = inst.stop();
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && live.iter().any(|i| i.lock().pid.is_some()) {
        std::thread::sleep(Duration::from_millis(250));
    }
    host.save_servers();
    println!("bye");
    Ok(())
}

fn spawn_stats(host: Arc<Host>) {
    use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    std::thread::Builder::new()
        .name("mchost-stats".into())
        .spawn(move || {
            let mut system = System::new();
            let mut disks = Disks::new_with_refreshed_list();
            let cpu_brand = {
                system.refresh_cpu_all();
                system
                    .cpus()
                    .first()
                    .map(|c| c.brand().trim().to_string())
                    .unwrap_or_default()
            };
            let os = format!(
                "{} {}",
                System::name().unwrap_or_default(),
                System::os_version().unwrap_or_default()
            )
            .trim()
            .to_string();
            let java = detect_java();
            let data_dir = paths::data_dir().canonicalize().unwrap_or_else(|_| paths::data_dir());
            let mut cpu_hist = VecDeque::new();
            let mut tick: u64 = 0;
            loop {
                system.refresh_cpu_usage();
                system.refresh_memory();
                let instances = host.all_instances();
                let pids: Vec<(Arc<Instance>, Pid)> = instances
                    .iter()
                    .filter_map(|i| i.lock().pid.map(|p| (i.clone(), Pid::from_u32(p))))
                    .collect();
                let only: Vec<Pid> = pids.iter().map(|(_, p)| *p).collect();
                if !only.is_empty() {
                    system.refresh_processes_specifics(
                        ProcessesToUpdate::Some(&only),
                        true,
                        ProcessRefreshKind::nothing().with_cpu().with_memory(),
                    );
                }
                for inst in &instances {
                    let pid = inst.lock().pid.map(Pid::from_u32);
                    let (cpu, mem) = pid
                        .and_then(|p| system.process(p))
                        .map(|p| (p.cpu_usage(), p.memory()))
                        .unwrap_or((0.0, 0));
                    let mut rt = inst.lock();
                    if rt.pid.is_some() {
                        rt.record_sample(cpu, mem);
                    } else if tick % 5 == 0 {
                        rt.record_sample(0.0, 0);
                    }
                }
                if tick % 15 == 0 {
                    disks.refresh(true);
                    for inst in &instances {
                        let used = files::du(&inst.dir);
                        inst.lock().disk_used = used;
                    }
                }
                let disk = disks
                    .list()
                    .iter()
                    .filter(|d| data_dir.starts_with(d.mount_point()))
                    .max_by_key(|d| d.mount_point().as_os_str().len());
                let cpu = system.global_cpu_usage();
                instance::push_capped(&mut cpu_hist, cpu.round() as u64);
                let load = System::load_average();
                *host.stats.lock().unwrap() = HostInfo {
                    name: host.name.clone(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    os: os.clone(),
                    cpu_brand: cpu_brand.clone(),
                    cores: system.cpus().len(),
                    cpu_usage: cpu,
                    mem_used: system.used_memory(),
                    mem_total: system.total_memory(),
                    disk_used: disk.map(|d| d.total_space() - d.available_space()).unwrap_or(0),
                    disk_total: disk.map(|d| d.total_space()).unwrap_or(0),
                    load: [load.one, load.five, load.fifteen],
                    uptime_secs: System::uptime(),
                    panel_uptime_secs: host.started.elapsed().as_secs(),
                    cpu_history: cpu_hist.iter().copied().collect(),
                    java: java.clone(),
                };
                tick += 1;
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawning the stats thread");
}

fn detect_java() -> String {
    std::process::Command::new("java")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stderr).into_owned();
            text.lines().next().map(|l| l.trim().to_string())
        })
        .unwrap_or_else(|| "not found on PATH".into())
}

// ---------------------------------------------------------- connection ---

type Tls = StreamOwned<ServerConnection, TcpStream>;

fn read_message(reader: &mut BufReader<Tls>) -> Result<Option<Request>> {
    let mut buf = Vec::new();
    let n = (&mut *reader)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE {
        bail!("message too large");
    }
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| anyhow!("bad request: {e}"))
}

fn write_reply(reader: &mut BufReader<Tls>, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    let stream = reader.get_mut();
    stream.write_all(&line)?;
    stream.flush()?;
    Ok(())
}

fn handle_connection(
    host: &Arc<Host>,
    tls: Arc<rustls::ServerConfig>,
    stream: TcpStream,
    peer: SocketAddr,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(LOGIN_TIMEOUT))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    stream.set_nodelay(true).ok();
    let connection = ServerConnection::new(tls)?;
    let mut reader = BufReader::new(StreamOwned::new(connection, stream));
    let ip = peer.ip();
    let ip_text = ip.to_string();

    // ---- login
    let Some(first) = read_message(&mut reader)? else {
        return Ok(());
    };
    let Request::Login {
        username,
        password,
        protocol,
    } = first
    else {
        write_reply(&mut reader, &Reply::err("log in first"))?;
        return Ok(());
    };
    if protocol != PROTOCOL {
        write_reply(
            &mut reader,
            &Reply::err(format!(
                "this host speaks protocol {PROTOCOL}, the panel speaks {protocol} — update mchost on both sides"
            )),
        )?;
        return Ok(());
    }
    if let Some(wait) = host.throttle.lock().unwrap().locked(ip) {
        write_reply(
            &mut reader,
            &Reply::err(format!(
                "too many failed logins from {ip}; try again in {}",
                fmt_duration(wait.as_secs())
            )),
        )?;
        return Ok(());
    }
    let authed = {
        let mut store = host.users.lock().unwrap();
        let user = store.authenticate(&username, &password, &ip_text);
        if user.is_some() {
            let _ = store.save();
        }
        user
    };
    let Some(user) = authed else {
        host.throttle.lock().unwrap().miss(ip);
        host.record(&username, &ip_text, "login", "wrong username or password".into(), false);
        // Slow down guessing.
        std::thread::sleep(Duration::from_millis(800));
        write_reply(&mut reader, &Reply::err("wrong username or password"))?;
        return Ok(());
    };
    host.throttle.lock().unwrap().clear(ip);
    host.record(&user.username, &ip_text, "login", String::new(), true);
    write_reply(
        &mut reader,
        &Reply::ok(Welcome {
            username: user.username.clone(),
            role: user.role,
            host_name: host.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        }),
    )?;
    reader.get_ref().sock.set_read_timeout(Some(IDLE_TIMEOUT))?;

    // ---- requests
    let username = user.username;
    loop {
        let request = match read_message(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(err) => {
                if err.downcast_ref::<std::io::Error>().is_some() {
                    break;
                }
                write_reply(&mut reader, &Reply::err(format!("{err:#}")))?;
                continue;
            }
        };
        // Re-read the account every time: a role change or removal applies
        // to sessions that are already open.
        let current = host.users.lock().unwrap().get(&username).cloned();
        let Some(current) = current else {
            write_reply(&mut reader, &Reply::err("your account was removed"))?;
            break;
        };
        let reply = dispatch(host, &current, &ip_text, request);
        write_reply(&mut reader, &reply)?;
    }
    Ok(())
}

// ------------------------------------------------------------ dispatch ---

fn need(user: &User, role: Role) -> Result<()> {
    if user.role < role {
        bail!("permission denied — this needs the {role} role (you are {})", user.role);
    }
    Ok(())
}

fn server_for(host: &Host, user: &User, name: &str) -> Result<Arc<Instance>> {
    if !user.can_see(name) {
        bail!("no server `{name}`");
    }
    host.instance(name).with_context(|| format!("no server `{name}`"))
}

/// Requests that change something are written to the activity log.
fn audit_detail(request: &Request) -> Option<String> {
    Some(match request {
        Request::Command { server, line } => format!("{server}: {line}"),
        Request::Power { server, action } => format!("{server}: {action:?}").to_lowercase(),
        Request::CreateServer(new) => format!("{} ({} {})", new.name, new.kind, new.version),
        Request::DeleteServer { server, delete_files } => {
            format!("{server}{}", if *delete_files { " + files" } else { "" })
        }
        Request::UpdateSettings { server, .. } => server.clone(),
        Request::Reinstall { server, kind, version } => format!("{server}: {kind} {version}"),
        Request::Player { server, action, name, .. } => {
            format!("{server}: {} {name}", format!("{action:?}").to_lowercase())
        }
        Request::WriteFile { server, path, .. } => format!("{server}: {path}"),
        Request::DeletePath { server, path } => format!("{server}: {path}"),
        Request::RenamePath { server, from, to } => format!("{server}: {from} → {to}"),
        Request::MakeDir { server, path } => format!("{server}: {path}"),
        Request::SetProperty { server, key, value } => format!("{server}: {key}={value}"),
        Request::CreateBackup { server, .. } => server.clone(),
        Request::RestoreBackup { server, id } => format!("{server}: {id}"),
        Request::DeleteBackup { server, id } => format!("{server}: {id}"),
        Request::AddSchedule(s) => format!("{} on {} ({})", s.name, s.server, s.when),
        Request::RemoveSchedule { id } | Request::ToggleSchedule { id } | Request::RunSchedule { id } => {
            format!("task #{id}")
        }
        Request::AddUser { username, role, .. } => format!("{username} ({role})"),
        Request::RemoveUser { username } | Request::SetPassword { username, .. } => username.clone(),
        Request::SetRole { username, role, .. } => format!("{username} → {role}"),
        _ => return None,
    })
}

fn dispatch(host: &Arc<Host>, user: &User, ip: &str, request: Request) -> Reply {
    let detail = audit_detail(&request)
        .map(|d| d.chars().map(|c| if c.is_control() { '⏎' } else { c }).collect::<String>());
    let action = request.name();
    let result = handle(host, user, request);
    if let Some(detail) = detail {
        let detail = match &result {
            Err(err) => format!("{detail} — {err:#}"),
            Ok(_) => detail,
        };
        host.record(&user.username, ip, action, detail, result.is_ok());
    }
    match result {
        Ok(value) => Reply::ok(value),
        Err(err) => Reply::err(format!("{err:#}")),
    }
}

fn message(text: impl Into<String>) -> Value {
    json!({ "message": text.into() })
}

fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        bail!("server names are 1–32 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        bail!("server names use lowercase letters, digits, `-` and `_`");
    }
    Ok(())
}

fn validate_player(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 || name.contains(char::is_whitespace) {
        bail!("`{name}` is not a player name");
    }
    Ok(())
}

fn one_line(text: &str) -> Result<&str> {
    if text.contains(['\n', '\r']) {
        bail!("text must be a single line");
    }
    Ok(text.trim())
}

fn handle(host: &Arc<Host>, user: &User, request: Request) -> Result<Value> {
    Ok(match request {
        Request::Login { .. } => bail!("already logged in"),
        Request::Ping => message("pong"),

        Request::Snapshot => {
            let mut info = host.stats.lock().unwrap().clone();
            info.panel_uptime_secs = host.started.elapsed().as_secs();
            let servers: Vec<ServerStatus> = host
                .all_instances()
                .iter()
                .filter(|i| user.can_see(&i.name))
                .map(|i| i.status())
                .collect();
            serde_json::to_value(Snapshot {
                host: info,
                servers,
                me: Me {
                    username: user.username.clone(),
                    role: user.role,
                    servers: user.servers.clone(),
                },
                sessions: host.sessions.load(Ordering::SeqCst),
            })?
        }

        Request::Console { server, since } => {
            let inst = server_for(host, user, &server)?;
            let (lines, next) = inst.console_since(since);
            serde_json::to_value(ConsoleChunk { lines, next })?
        }

        Request::Command { server, line } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            inst.send_command(&line)?;
            message("sent")
        }

        Request::Power { server, action } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            message(match action {
                Power::Start => inst.start()?,
                Power::Stop => inst.stop()?,
                Power::Restart => inst.restart()?,
                Power::Kill => inst.kill()?,
            })
        }

        Request::CreateServer(new) => {
            need(user, Role::Admin)?;
            validate_server_name(&new.name)?;
            if !new.eula {
                bail!("the Minecraft EULA (https://aka.ms/MinecraftEULA) must be accepted to run a server");
            }
            if host.instance(&new.name).is_some() {
                bail!("a server called `{}` already exists", new.name);
            }
            if new.port < 1024 {
                bail!("pick a port of 1024 or above");
            }
            if let Some(other) = host.port_in_use(new.port, None) {
                bail!("port {} is already used by `{other}`", new.port);
            }
            if !(256..=262_144).contains(&new.memory_mb) {
                bail!("memory should be between 256 and 262144 MiB");
            }
            let dir = paths::server_dir(&new.name);
            std::fs::create_dir_all(&dir)?;
            instance::write_eula(&dir)?;
            if !dir.join("server.properties").exists() {
                std::fs::write(
                    dir.join("server.properties"),
                    format!("server-port={}\nmotd={} — hosted with mchost\n", new.port, new.name),
                )?;
            }
            let settings = ServerSettings {
                kind: new.kind,
                version: new.version.trim().to_string(),
                java: if new.java.trim().is_empty() { "java".into() } else { new.java.trim().into() },
                jar: install::JAR.into(),
                memory_min_mb: new.memory_mb.min(1024),
                memory_max_mb: new.memory_mb,
                jvm_args: String::new(),
                port: new.port,
                auto_start: false,
                auto_restart: true,
                stop_timeout_secs: 60,
            };
            let inst = Instance::new(&new.name, dir, settings, Arc::downgrade(host));
            host.instances
                .write()
                .unwrap()
                .insert(new.name.clone(), inst.clone());
            host.save_servers();
            if new.kind == ServerKind::Custom {
                inst.log("custom server: upload your jar as server.jar (or change the jar name in Config)");
                message(format!("created {} — upload a jar to start it", new.name))
            } else {
                install::spawn(inst, new.kind, new.version, false)?;
                message(format!("created {} — downloading {}", new.name, new.kind))
            }
        }

        Request::DeleteServer { server, delete_files } => {
            need(user, Role::Admin)?;
            let inst = server_for(host, user, &server)?;
            if inst.is_running() || inst.lock().state == ServerState::Installing {
                bail!("stop {server} first");
            }
            host.instances.write().unwrap().remove(&server);
            host.save_servers();
            {
                let mut tasks = host.schedules.lock().unwrap();
                tasks.tasks.retain(|t| t.server != server);
                tasks.save();
            }
            if delete_files {
                let _ = std::fs::remove_dir_all(&inst.dir);
                let _ = std::fs::remove_dir_all(paths::backups_dir(&server));
                message(format!("deleted {server} and its files"))
            } else {
                message(format!("removed {server} from the panel; its folder is kept"))
            }
        }

        Request::GetSettings { server } => {
            let inst = server_for(host, user, &server)?;
            let settings = inst.lock().settings.clone();
            serde_json::to_value(settings)?
        }

        Request::UpdateSettings { server, settings } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            let current = inst.lock().settings.clone();
            let launch_changed = settings.java != current.java
                || settings.jvm_args != current.jvm_args
                || settings.jar != current.jar;
            if launch_changed {
                // These decide what program the host runs.
                need(user, Role::Admin)?;
            }
            if settings.jar.contains("..") || settings.jar.starts_with('/') || settings.jar.is_empty() {
                bail!("the jar must be a file inside the server folder");
            }
            if !(256..=262_144).contains(&settings.memory_max_mb) {
                bail!("memory should be between 256 and 262144 MiB");
            }
            if settings.port < 1024 {
                bail!("pick a port of 1024 or above");
            }
            if let Some(other) = host.port_in_use(settings.port, Some(&server)) {
                bail!("port {} is already used by `{other}`", settings.port);
            }
            {
                let mut rt = inst.lock();
                let mut next = settings;
                // The installer owns these two.
                next.kind = current.kind;
                next.version = current.version.clone();
                next.memory_min_mb = next.memory_min_mb.min(next.memory_max_mb).max(128);
                rt.settings = next;
            }
            if settings_port_changed(&current, &inst) {
                let port = inst.lock().settings.port;
                files::set_property(&inst.dir, "server-port", &port.to_string())?;
            }
            host.save_servers();
            message(if inst.is_running() {
                "saved — restart the server to apply"
            } else {
                "saved"
            })
        }

        Request::Reinstall { server, kind, version } => {
            need(user, Role::Admin)?;
            let inst = server_for(host, user, &server)?;
            install::spawn(inst, kind, version, false)?;
            message(format!("reinstalling {server}"))
        }

        Request::Players { server } => {
            let inst = server_for(host, user, &server)?;
            let (running, online, max_players) = {
                let rt = inst.lock();
                (rt.state.is_live(), rt.players.iter().cloned().collect(), rt.max_players)
            };
            let whitelist_enabled = files::read_properties(&inst.dir)
                .unwrap_or_default()
                .iter()
                .any(|p| (p.key == "white-list" || p.key == "enforce-whitelist") && p.value == "true");
            serde_json::to_value(PlayersInfo {
                running,
                online,
                ops: files::ops(&inst.dir),
                whitelist: files::whitelist(&inst.dir),
                banned: files::banned(&inst.dir),
                whitelist_enabled,
                max_players,
            })?
        }

        Request::Player { server, action, name, extra } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            let extra = one_line(&extra)?;
            let needs_name = !matches!(action, PlayerAction::WhitelistOn | PlayerAction::WhitelistOff);
            if needs_name {
                validate_player(&name)?;
            }
            let command = match action {
                PlayerAction::Op => format!("op {name}"),
                PlayerAction::Deop => format!("deop {name}"),
                PlayerAction::Kick => format!("kick {name} {extra}"),
                PlayerAction::Ban => format!("ban {name} {extra}"),
                PlayerAction::Pardon => format!("pardon {name}"),
                PlayerAction::WhitelistAdd => format!("whitelist add {name}"),
                PlayerAction::WhitelistRemove => format!("whitelist remove {name}"),
                PlayerAction::WhitelistOn => "whitelist on".into(),
                PlayerAction::WhitelistOff => "whitelist off".into(),
                PlayerAction::Message => {
                    if extra.is_empty() {
                        bail!("write a message");
                    }
                    format!("tell {name} {extra}")
                }
                PlayerAction::Gamemode => {
                    let mode = extra.to_ascii_lowercase();
                    if !["survival", "creative", "adventure", "spectator"].contains(&mode.as_str()) {
                        bail!("game mode is survival, creative, adventure or spectator");
                    }
                    format!("gamemode {mode} {name}")
                }
            };
            if !inst.is_running() {
                bail!("{server} must be running for that");
            }
            inst.send_command(command.trim())?;
            message(format!("sent `{}`", command.trim()))
        }

        Request::ListFiles { server, path } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            serde_json::to_value(files::list(&inst.dir, &path)?)?
        }
        Request::ReadFile { server, path } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            let content = files::read(&inst.dir, &path)?;
            serde_json::to_value(FileContent { path, content })?
        }
        Request::WriteFile { server, path, content } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            files::write(&inst.dir, &path, &content)?;
            message(format!("saved {path}"))
        }
        Request::DeletePath { server, path } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            files::delete(&inst.dir, &path)?;
            message(format!("deleted {path}"))
        }
        Request::RenamePath { server, from, to } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            files::rename(&inst.dir, &from, &to)?;
            message(format!("renamed to {to}"))
        }
        Request::MakeDir { server, path } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            files::mkdir(&inst.dir, &path)?;
            message(format!("created {path}/"))
        }

        Request::Properties { server } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            serde_json::to_value(files::read_properties(&inst.dir)?)?
        }
        Request::SetProperty { server, key, value } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            let value = one_line(&value)?.to_string();
            if key == "server-port" {
                let port: u16 = value.parse().map_err(|_| anyhow!("`{value}` is not a port"))?;
                if let Some(other) = host.port_in_use(port, Some(&server)) {
                    bail!("port {port} is already used by `{other}`");
                }
                inst.lock().settings.port = port;
                host.save_servers();
            }
            files::set_property(&inst.dir, &key, &value)?;
            if key == "max-players"
                && let Ok(n) = value.parse()
            {
                inst.lock().max_players = n;
            }
            message(if inst.is_running() {
                format!("{key} = {value} — restart to apply")
            } else {
                format!("{key} = {value}")
            })
        }

        Request::Backups { server } => {
            need(user, Role::Operator)?;
            server_for(host, user, &server)?;
            serde_json::to_value(backup::list(&server)?)?
        }
        Request::CreateBackup { server, note } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            message(backup::spawn_create(inst, one_line(&note)?.to_string(), user.username.clone())?)
        }
        Request::RestoreBackup { server, id } => {
            need(user, Role::Operator)?;
            let inst = server_for(host, user, &server)?;
            message(backup::restore(inst, &id)?)
        }
        Request::DeleteBackup { server, id } => {
            need(user, Role::Operator)?;
            server_for(host, user, &server)?;
            backup::delete(&server, &id)?;
            message(format!("deleted backup {id}"))
        }

        Request::Schedules => {
            let tasks = host.schedules.lock().unwrap();
            let visible: Vec<&Schedule> = tasks.tasks.iter().filter(|t| user.can_see(&t.server)).collect();
            serde_json::to_value(visible)?
        }
        Request::AddSchedule(new) => {
            need(user, Role::Operator)?;
            server_for(host, user, &new.server)?;
            if new.name.trim().is_empty() {
                bail!("give the task a name");
            }
            let payload = one_line(&new.payload)?.to_string();
            if new.action == TaskAction::Command && payload.is_empty() {
                bail!("a command task needs a command");
            }
            if new.action == TaskAction::Say && payload.is_empty() {
                bail!("a say task needs a message");
            }
            let mut tasks = host.schedules.lock().unwrap();
            tasks.next_id += 1;
            let id = tasks.next_id;
            let t = now();
            tasks.tasks.push(Schedule {
                id,
                name: new.name.trim().to_string(),
                server: new.server,
                action: new.action,
                payload,
                when: new.when,
                enabled: true,
                created: t,
                last_run: None,
                last_result: None,
                next_run: Some(schedule::next_after(new.when, t)),
                created_by: user.username.clone(),
            });
            tasks.save();
            message(format!("task #{id} scheduled ({})", new.when))
        }
        Request::RemoveSchedule { id } => {
            need(user, Role::Operator)?;
            let mut tasks = host.schedules.lock().unwrap();
            let task = tasks.tasks.iter().find(|t| t.id == id).context("no such task")?;
            if !user.can_see(&task.server) {
                bail!("no such task");
            }
            tasks.tasks.retain(|t| t.id != id);
            tasks.save();
            message(format!("removed task #{id}"))
        }
        Request::ToggleSchedule { id } => {
            need(user, Role::Operator)?;
            let mut tasks = host.schedules.lock().unwrap();
            let task = tasks.tasks.iter_mut().find(|t| t.id == id).context("no such task")?;
            if !user.can_see(&task.server) {
                bail!("no such task");
            }
            task.enabled = !task.enabled;
            if task.enabled {
                task.next_run = Some(schedule::next_after(task.when, now()));
            }
            let text = format!("task #{id} {}", if task.enabled { "enabled" } else { "paused" });
            tasks.save();
            message(text)
        }
        Request::RunSchedule { id } => {
            need(user, Role::Operator)?;
            let task = {
                let tasks = host.schedules.lock().unwrap();
                tasks.tasks.iter().find(|t| t.id == id).cloned().context("no such task")?
            };
            if !user.can_see(&task.server) {
                bail!("no such task");
            }
            let outcome = schedule::run(host, &task);
            let mut tasks = host.schedules.lock().unwrap();
            if let Some(entry) = tasks.tasks.iter_mut().find(|t| t.id == id) {
                entry.last_run = Some(now());
                entry.last_result = Some(match &outcome {
                    Ok(m) => m.clone(),
                    Err(e) => format!("error: {e}"),
                });
            }
            tasks.save();
            message(outcome.map_err(|e| anyhow!(e))?)
        }

        Request::Users => {
            let store = host.users.lock().unwrap();
            let list: Vec<UserInfo> = store
                .users
                .iter()
                .filter(|u| user.role == Role::Admin || u.username == user.username)
                .map(User::info)
                .collect();
            serde_json::to_value(list)?
        }
        Request::AddUser { username, password, role, servers } => {
            need(user, Role::Admin)?;
            let mut store = host.users.lock().unwrap();
            store.add(username.trim(), &password, role, clean_list(servers))?;
            store.save()?;
            message(format!("added {} ({role})", username.trim()))
        }
        Request::RemoveUser { username } => {
            need(user, Role::Admin)?;
            let mut store = host.users.lock().unwrap();
            store.remove(&username)?;
            store.save()?;
            message(format!("removed {username}"))
        }
        Request::SetPassword { username, password } => {
            if !username.eq_ignore_ascii_case(&user.username) {
                need(user, Role::Admin)?;
            }
            let mut store = host.users.lock().unwrap();
            store.set_password(&username, &password)?;
            store.save()?;
            message(format!("password changed for {username}"))
        }
        Request::SetRole { username, role, servers } => {
            need(user, Role::Admin)?;
            let mut store = host.users.lock().unwrap();
            store.set_role(&username, role, clean_list(servers))?;
            store.save()?;
            message(format!("{username} is now {role}"))
        }

        Request::Activity => {
            let log = host.activity.lock().unwrap();
            let entries: Vec<&ActivityEntry> = log
                .iter()
                .rev()
                .filter(|e| user.role == Role::Admin || e.user == user.username)
                .take(500)
                .collect();
            serde_json::to_value(entries)?
        }
    })
}

fn settings_port_changed(previous: &ServerSettings, inst: &Instance) -> bool {
    inst.lock().settings.port != previous.port
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Used by `mchost user …` on the host itself.
pub fn with_users<T>(f: impl FnOnce(&mut UserStore) -> Result<T>) -> Result<T> {
    paths::ensure_host_dirs()?;
    let mut store = UserStore::load()?;
    let out = f(&mut store)?;
    store.save()?;
    Ok(out)
}
