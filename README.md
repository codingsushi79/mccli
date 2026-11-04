<div align="center">

# mchost

**Host Minecraft servers on one machine and control them from any other machine through a terminal panel.**

[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![rust](https://img.shields.io/badge/rust-stable-orange.svg)

</div>

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ 1 Overview │ 2 Console │ 3 Players │ 4 Files │ 5 Config │ 6 Backups │ 7 Tasks │ 8 Users │ 9 Activity   ● smp running  ▲ 2/20  │ ● admin@box │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ Servers  3 ─────────────────────────────────────────────────────────────┐┌ Host — box ───────────────────────────┐
│SERVER     TYPE           STATE      PLAYERS  CPU   MEMORY    UPTIME  PORT││cpu     █████░░░░░░░░░░░░░░░░░░ 10%    │
│smp        paper 1.21.4   running    2/20     34%   3.1 GiB   2h 4m  25565││memory  ███████░░░░░░░ 32.8 / 128.0 GiB│
│creative   fabric 1.21.4  stopped    –        –     –         –      25566││disk    █████████████░░░ 2.9 / 3.6 TiB │
```

## How it works

There are two halves, both in the one `mchost` binary:

- **The host** (`mchost serve`) runs on the machine that hosts the Minecraft
  servers. It starts and watches the server processes, and listens for panel
  logins on port **25580** over TLS.
- **The panel** (`mchost`) runs anywhere. Type the host's IP or URL, a
  username and a password, and you have control. Closing the panel never stops
  a server.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/codingsushi79/mccli/master/install.sh | bash
```

Builds from source and installs `mchost` (also linked as `mccli`), and tells
you what to add to your shell rc if the install directory isn't on your
`PATH`. Needs Rust — the script tells you how to get it if you don't have it.
Java is only needed on the machine that hosts the servers (Java 21+ for
Minecraft 1.20.5 and newer).

<details>
<summary>Manual install</summary>

```bash
git clone https://github.com/codingsushi79/mccli
cd mccli
cargo install --path .
```

Environment overrides for the script: `MCHOST_INSTALL_DIR`, `MCHOST_REPO`,
`MCHOST_BRANCH`, `MCHOST_NO_COLOR`.
</details>

## Quick start

On the machine that will host the servers:

```bash
mchost serve
```

The first run creates an `admin` account and prints its password once. It also
prints the host's TLS fingerprint. Open TCP port 25580 if panels will connect
from other machines.

From any machine, including the host itself:

```bash
mchost                                # login screen
mchost connect 203.0.113.7 -u admin   # or go straight to a host
mchost connect play.example.com:25580
```

The address can be an IP, a hostname, `host:port`, or a pasted URL. On the
first connection the panel shows the host's fingerprint and asks you to trust
it. Your password isn't sent until you do. After that the fingerprint is
pinned, and if it ever changes the panel refuses to connect.

Press `c` in the panel to create a server. Pick Paper, Vanilla, Purpur, Fabric
or a custom jar, a version (`latest` or a specific one), the memory and the
port. mchost downloads the jar and writes `eula.txt` (only if you accept the
EULA). Press `s` to start it.

## What the panel does

| View | |
|---|---|
| **Overview** | Every server with its state, players, CPU, memory, uptime and port. CPU and memory history for the selected server. Host CPU, memory, disk, load and Java version. A live console tail. |
| **Console** | Live console with colour-coded warnings and errors. Command prompt with history. Scroll back through 5,000 lines. |
| **Players** | Who's online, plus ops, whitelist and bans. Op/deop, whitelist, ban/pardon, kick, `/tell` and game mode in one key each. Turn the whitelist on or off. |
| **Files** | Browse the server folder. Built-in editor for configs (Ctrl-S saves). New file or folder, rename/move, delete. Paths are locked to the server folder, and symlinks can't lead out of it. |
| **Config** | Edit `server.properties` one key at a time. Launch settings: memory, port, auto-start on boot, auto-restart on crash, stop timeout, Java path, JVM flags. Change the server type or version. |
| **Backups** | Gzipped snapshots of the whole server folder. A running server gets `save-off` + `save-all flush` first, so the snapshot is consistent. Restore swaps the folder only after the archive has unpacked cleanly. |
| **Tasks** | Scheduled restarts (with an optional warning broadcast), backups, broadcasts, commands, starts and stops. Times look like `30m`, `6h` or `daily 04:00`. |
| **Users** | Accounts with roles, optionally limited to certain servers. Change passwords. |
| **Activity** | Audit log of who did what and from which IP, including refused attempts. |

What the host does on its own:

- If a server crashes, the host restarts it. After 4 crashes in 10 minutes it
  stops trying, so a broken server doesn't loop forever.
- A stop sends `stop` and waits. If the server hangs, the host kills it after
  the stop timeout.
- When the host itself is stopped (Ctrl-C or SIGTERM), it shuts every server
  down cleanly first.
- Each server runs in its own process group, so a Ctrl-C meant for the host
  never reaches the servers.

## Accounts and security

| Role | Can |
|---|---|
| `viewer` | See servers, stats, console and players |
| `operator` | Everything a viewer can, plus power, console commands, players, files, properties, backups and tasks |
| `admin` | Everything, including creating and deleting servers, changing the Java path, jar or JVM flags (these decide what the host runs), and managing users |

- Passwords are stored as Argon2id hashes and only ever travel inside TLS.
- Five failed logins from one address lock that address out for 10 minutes.
- Role changes and account removals take effect on sessions that are already
  open.
- Every change is recorded in `activity.log`.

Managing accounts on the host from the shell:

```bash
mchost user list
mchost user add alex --role operator --servers smp,creative
mchost user passwd admin
mchost user role alex viewer
mchost fingerprint                 # compare with what the panel shows
mchost forget 203.0.113.7          # on a panel, after the host was reinstalled
```

## Keys

`1`–`9` or `Tab` switch views · `[` `]` switch servers · `s` `x` `R` start/stop/restart ·
`X` kill · `c` create · `D` delete · `L` log out · `?` all keys · `q` quit (servers keep running)

## Files

- **Host:** `~/.local/share/mchost` on Linux, `~/Library/Application Support/mchost` on macOS.
  It holds `servers/<name>/`, `backups/<name>/`, `users.json`, `servers.json`,
  `schedules.json`, `activity.log`, and `host.crt` / `host.key`.
- **Panel:** `~/.config/mchost`. It holds `known_hosts.json` and
  `profiles.json` (recent hosts and usernames, never passwords).
- `MCHOST_HOME=/some/dir` moves both, which is useful for running more than
  one host on a machine.

## Running the host as a service (Linux)

```ini
# /etc/systemd/system/mchost.service
[Unit]
Description=mchost
After=network-online.target

[Service]
User=minecraft
ExecStart=/usr/local/bin/mchost serve
Restart=on-failure
KillSignal=SIGTERM
TimeoutStopSec=150

[Install]
WantedBy=multi-user.target
```

Set **auto start** on a server in Config, and it boots with the host.
