//! Every screen, drawn straight from the panel state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Tabs, Wrap};

use super::widgets::{self, centered, focus_panel, kv, meter, panel, ratio, truncate};
use super::{Login, LoginState, Panel, Tab, theme};
use crate::proto::*;

// --------------------------------------------------------------- login ---

const LOGO: [&str; 3] = [
    "█▀▄▀█ █▀▀ █ █ █▀█ █▀ ▀█▀",
    "█ ▀ █ █▄▄ █▀█ █▄█ ▄█  █ ",
    "",
];

pub fn login(frame: &mut Frame, login: &mut Login, tick: usize) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(ratatui::style::Color::Reset)), area);
    let recent_rows = login.profiles.len().min(6) as u16;
    let height = 3 + 2 + 9 + 2 + if recent_rows > 0 { recent_rows + 2 } else { 0 };
    let column = centered(area, 72, height);
    let [logo, subtitle, form_area, status, recent] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(9),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas(column);

    let logo_lines: Vec<Line> = LOGO
        .iter()
        .enumerate()
        .map(|(i, l)| Line::from(Span::styled(*l, if i == 0 { theme::accent() } else { theme::title() })).centered())
        .collect();
    frame.render_widget(Paragraph::new(logo_lines), logo);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("minecraft server panel ", theme::muted()),
            Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), theme::label()),
        ]))
        .centered(),
        subtitle,
    );

    login.form.render_in(frame, form_area, "connect");

    let status_line = match &login.state {
        LoginState::Idle if login.profiles.len() > 1 => Line::from(vec![
            Span::styled("PgUp/PgDn", theme::accent()),
            Span::styled(" switch between recent hosts", theme::muted()),
        ]),
        LoginState::Idle => Line::from(Span::styled(
            "the first `mchost serve` on the host prints the admin password",
            theme::muted(),
        )),
        LoginState::Connecting(since) => Line::from(vec![
            Span::styled(widgets::SPINNER[tick % widgets::SPINNER.len()], theme::accent()),
            Span::styled(
                format!(" connecting to {}… {}s", login.form.value(0), since.elapsed().as_secs()),
                theme::accent(),
            ),
        ]),
        LoginState::Error(err) => Line::from(Span::styled(format!("✗ {err}"), theme::bad())),
        LoginState::Trust { .. } => Line::from(Span::styled("waiting for you to check the fingerprint", theme::warn())),
    };
    frame.render_widget(Paragraph::new(status_line).wrap(Wrap { trim: true }), status);

    if recent_rows > 0 {
        let lines: Vec<Line> = login
            .profiles
            .iter()
            .take(6)
            .enumerate()
            .map(|(i, p)| {
                let chosen = login.profile_idx == Some(i);
                Line::from(vec![
                    Span::styled(if chosen { "▸ " } else { "  " }, theme::accent()),
                    Span::styled(format!("{:<34}", truncate(&p.address, 34)), if chosen { theme::value() } else { theme::base() }),
                    Span::styled(format!("{:<16}", truncate(&p.username, 16)), theme::label()),
                    Span::styled(fmt_relative(p.last_used), theme::muted()),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).block(panel("Recent")), recent);
    }

    if let LoginState::Trust { address, fingerprint } = &login.state {
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  First connection to ", theme::base()),
                Span::styled(address.clone(), theme::value()),
            ]),
            Line::from(""),
            Line::from(Span::styled("  Its certificate fingerprint is", theme::muted())),
            Line::from(Span::styled(format!("  {}", &fingerprint[..fingerprint.len().min(39)]), theme::accent())),
            Line::from(Span::styled(
                format!("  {}", fingerprint.get(39..).unwrap_or("")),
                theme::accent(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Check it against `mchost fingerprint` on the host. It is",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  remembered, and any change later is refused. No password",
                theme::muted(),
            )),
            Line::from(Span::styled("  has been sent yet.", theme::muted())),
            Line::from(""),
            Line::from(vec![
                Span::styled("  y", theme::accent()),
                Span::styled(" trust and log in    ", theme::muted()),
                Span::styled("any other key", theme::accent()),
                Span::styled(" cancel", theme::muted()),
            ]),
        ];
        let target = centered(area, 66, lines.len() as u16 + 2);
        frame.render_widget(Clear, target);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::WARN))
                    .title(Span::styled(" New host ", theme::title())),
            ),
            target,
        );
    }
}

// --------------------------------------------------------------- panel ---

pub fn dashboard(frame: &mut Frame, app: &mut Panel) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(6), Constraint::Length(1)]).areas(frame.area());

    draw_header(frame, app, header);

    if app.snapshot.is_none() {
        let text = match &app.last_error {
            Some(err) => Line::from(Span::styled(format!("  {err}"), theme::bad())),
            None => Line::from(Span::styled("  loading…", theme::muted())),
        };
        frame.render_widget(Paragraph::new(vec![Line::from(""), text]).block(panel("mchost")), body);
    } else if let Some(editor) = app.editor.as_mut() {
        editor.render(frame, body);
    } else {
        match app.tab {
            Tab::Overview => overview(frame, app, body),
            Tab::Console => console(frame, app, body),
            Tab::Players => players(frame, app, body),
            Tab::Files => files(frame, app, body),
            Tab::Config => config(frame, app, body),
            Tab::Backups => backups(frame, app, body),
            Tab::Tasks => tasks(frame, app, body),
            Tab::Users => users(frame, app, body),
            Tab::Activity => activity(frame, app, body),
        }
    }

    draw_footer(frame, app, footer);

    if app.help {
        widgets::help_overlay(frame, frame.area());
    }
    if let Some(confirm) = &app.confirm {
        widgets::confirm_overlay(frame, frame.area(), &confirm.prompt, confirm.danger);
    }
    if let Some(form) = &app.form {
        form.render(frame, frame.area());
    }
}

fn draw_header(frame: &mut Frame, app: &Panel, area: Rect) {
    let block = Block::default().borders(Borders::ALL).border_style(theme::border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let status = header_status(app);
    let status_width = status.width() as u16 + 1;
    let [tabs_area, status_area] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(status_width)]).areas(inner);

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(vec![Span::styled(format!("{} ", i + 1), theme::muted()), Span::raw(t.title())]))
        .collect();
    frame.render_widget(
        Tabs::new(titles)
            .select(app.tab.index())
            .style(theme::base())
            .highlight_style(theme::accent())
            .divider(Span::styled("│", theme::border())),
        tabs_area,
    );
    frame.render_widget(Paragraph::new(status).right_aligned(), status_area);
}

fn header_status(app: &Panel) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(server) = app.server() {
        let live = server.state == ServerState::Running;
        let dot = if server.state.is_live() && server.state != ServerState::Running || server.state == ServerState::Installing {
            widgets::SPINNER[app.tick % widgets::SPINNER.len()]
        } else if live {
            "●"
        } else {
            "○"
        };
        spans.push(Span::styled(format!("{dot} "), Style::default().fg(theme::state_color(server.state))));
        spans.push(Span::styled(truncate(&server.name, 14), theme::accent()));
        spans.push(Span::styled(format!(" {}", server.state), Style::default().fg(theme::state_color(server.state))));
        if server.state.is_live() {
            spans.push(Span::styled("  ▲ ", theme::label()));
            spans.push(Span::styled(format!("{}/{}", server.players.len(), server.max_players), theme::value()));
        }
        spans.push(Span::styled("  │ ", theme::border()));
    }
    let online = app.is_online() && app.last_error.is_none();
    spans.push(Span::styled(
        if online { "● " } else { "✗ " },
        if online { theme::good() } else { theme::bad() },
    ));
    spans.push(Span::styled(app.welcome.username.clone(), theme::value()));
    spans.push(Span::styled("@", theme::muted()));
    spans.push(Span::styled(truncate(&app.welcome.host_name, 16), theme::base()));
    Line::from(spans)
}

/// Keep the end of a long path, where the interesting part is.
fn truncate_left(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max || max < 2 {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

fn draw_footer(frame: &mut Frame, app: &Panel, area: Rect) {
    if let Some((message, is_error, _)) = &app.toast {
        let style = if *is_error { theme::bad() } else { theme::good() };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if *is_error { " ✗ " } else { " ✓ " }, style),
                Span::styled(message.clone(), style),
            ])),
            area,
        );
        return;
    }
    let keys: &[(&str, &str)] = if app.editor.is_some() {
        &[("Ctrl-S", "save"), ("Esc", "close")]
    } else {
        match app.tab {
            Tab::Overview => &[
                ("↑↓", "server"),
                ("s/x/R", "start/stop/restart"),
                ("Enter", "console"),
                ("c", "create"),
                ("D", "delete"),
                ("?", "help"),
                ("q", "quit (servers keep running)"),
            ],
            Tab::Console if app.input_focus => &[
                ("Enter", "run"),
                ("↑↓", "history"),
                ("PgUp/PgDn", "scroll"),
                ("Esc", "leave prompt"),
                ("Tab", "next view"),
            ],
            Tab::Console => &[("i", "type a command"), ("↑↓", "scroll"), ("End", "follow"), ("s/x/R", "power"), ("?", "help")],
            Tab::Players => &[
                ("o", "op"),
                ("w", "whitelist"),
                ("b", "ban/pardon"),
                ("K", "kick"),
                ("m", "message"),
                ("g", "gamemode"),
                ("a", "by name"),
                ("W", "whitelist on/off"),
            ],
            Tab::Files => &[
                ("Enter", "open"),
                ("⌫", "up"),
                ("n", "new file"),
                ("N", "new folder"),
                ("r", "rename"),
                ("d", "delete"),
                ("?", "help"),
            ],
            Tab::Config => &[("Enter", "edit property"), ("a", "add property"), ("e", "edit settings"), ("I", "change version"), ("?", "help")],
            Tab::Backups => &[("b", "back up now"), ("r", "restore"), ("d", "delete"), ("?", "help")],
            Tab::Tasks => &[("a", "add"), ("e", "pause/resume"), ("Enter", "run now"), ("d", "delete"), ("?", "help")],
            Tab::Users => &[("a", "add"), ("p", "password"), ("e", "role"), ("d", "remove"), ("?", "help")],
            Tab::Activity => &[("↑↓", "scroll"), ("[ ]", "server"), ("L", "log out"), ("?", "help")],
        }
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, description) in keys {
        spans.push(Span::styled(*key, theme::accent()));
        spans.push(Span::styled(format!(" {description}   "), theme::muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn no_server(frame: &mut Frame, app: &Panel, area: Rect, title: &str) {
    let hint = if app.role() == Role::Admin {
        "  no servers yet — press c to create one"
    } else {
        "  no servers you can see on this host"
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(""), Line::from(Span::styled(hint, theme::muted()))]).block(panel(title)),
        area,
    );
}

fn needs_role(frame: &mut Frame, area: Rect, title: &str, role: Role) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(format!("  this view needs the {role} role"), theme::muted())),
        ])
        .block(panel(title)),
        area,
    );
}

fn table_block<'a>(
    rows: Vec<Row<'a>>,
    widths: Vec<Constraint>,
    header: &'a [&'a str],
    title: String,
    selected: Option<usize>,
    frame: &mut Frame,
    area: Rect,
) {
    let header = Row::new(header.iter().map(|h| Cell::from(*h)).collect::<Vec<_>>()).style(theme::label());
    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(title))
        .row_highlight_style(theme::selected())
        .column_spacing(2);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

// ------------------------------------------------------------ overview ---

fn overview(frame: &mut Frame, app: &mut Panel, area: Rect) {
    let Some(snapshot) = app.snapshot.clone() else { return };
    let [main, log_area] = Layout::vertical([Constraint::Min(12), Constraint::Length(9)]).areas(area);
    let [left, right] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(main);
    let table_height = (snapshot.servers.len() as u16 + 3).clamp(5, 12);
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Length(table_height), Constraint::Min(6)]).areas(left);
    let [host_area, players_area] = Layout::vertical([Constraint::Length(14), Constraint::Min(3)]).areas(right);

    // Servers
    if snapshot.servers.is_empty() {
        no_server(frame, app, table_area, "Servers");
    } else {
        let rows: Vec<Row> = snapshot
            .servers
            .iter()
            .map(|s| {
                let state_style = Style::default().fg(theme::state_color(s.state));
                Row::new(vec![
                    Cell::from(Span::styled(s.name.clone(), theme::value())),
                    Cell::from(format!("{} {}", s.kind, s.version)),
                    Cell::from(Span::styled(s.state.to_string(), state_style)),
                    Cell::from(if s.state.is_live() {
                        format!("{}/{}", s.players.len(), s.max_players)
                    } else {
                        "–".into()
                    }),
                    Cell::from(if s.state.is_live() { format!("{:.0}%", s.cpu) } else { "–".into() }),
                    Cell::from(if s.state.is_live() { fmt_bytes(s.mem) } else { "–".into() }),
                    Cell::from(if s.state.is_live() { fmt_duration(s.uptime_secs) } else { "–".into() }),
                    Cell::from(s.port.to_string()),
                ])
            })
            .collect();
        table_block(
            rows,
            vec![
                Constraint::Min(10),
                Constraint::Length(15),
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(5),
            ],
            &["SERVER", "TYPE", "STATE", "PLAYERS", "CPU", "MEMORY", "UPTIME", "PORT"],
            format!("Servers  {}", snapshot.servers.len()),
            Some(app.server_idx),
            frame,
            table_area,
        );
    }

    // Selected server
    match app.server().cloned() {
        Some(server) => server_detail(frame, &server, detail_area),
        None => frame.render_widget(panel("Server"), detail_area),
    }

    // Host
    host_panel(frame, &snapshot, host_area);

    // Players
    let title = match app.server() {
        Some(s) => format!("Online  {}/{}", s.players.len(), s.max_players),
        None => "Online".into(),
    };
    let lines: Vec<Line> = match app.server() {
        Some(s) if !s.players.is_empty() => s
            .players
            .iter()
            .map(|p| Line::from(vec![Span::styled("● ", theme::good()), Span::styled(p.clone(), theme::base())]))
            .collect(),
        Some(s) if s.state.is_live() => vec![Line::from(Span::styled("nobody online", theme::muted()))],
        _ => vec![Line::from(Span::styled("server is not running", theme::muted()))],
    };
    frame.render_widget(Paragraph::new(lines).block(panel(title)), players_area);

    // Console tail
    let block = panel(format!("Console — {}", app.server_name().unwrap_or_default()));
    let inner = block.inner(log_area);
    frame.render_widget(block, log_area);
    let lines = console_lines(app, inner.width as usize, inner.height as usize, 0);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn server_detail(frame: &mut Frame, server: &ServerStatus, area: Rect) {
    let block = panel(format!("{} — {} {}", server.name, server.kind, server.version));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [info, charts] = Layout::vertical([Constraint::Length(3), Constraint::Min(2)]).areas(inner);

    let state_style = Style::default().fg(theme::state_color(server.state)).add_modifier(Modifier::BOLD);
    let mut first = vec![
        Span::styled("state ", theme::label()),
        Span::styled(server.state.to_string(), state_style),
    ];
    if let Some(activity) = &server.activity {
        first.push(Span::styled(format!("  {activity}"), theme::accent()));
    }
    if server.state.is_live() {
        first.push(Span::styled("   up ", theme::label()));
        first.push(Span::styled(fmt_duration(server.uptime_secs), theme::value()));
    }
    first.push(Span::styled("   port ", theme::label()));
    first.push(Span::styled(server.port.to_string(), theme::value()));
    let mut second = vec![
        Span::styled("disk ", theme::label()),
        Span::styled(fmt_bytes(server.disk_used), theme::value()),
        Span::styled("   auto-restart ", theme::label()),
        Span::styled(
            if server.auto_restart { "on" } else { "off" },
            if server.auto_restart { theme::good() } else { theme::muted() },
        ),
        Span::styled("   auto-start ", theme::label()),
        Span::styled(
            if server.auto_start { "on" } else { "off" },
            if server.auto_start { theme::good() } else { theme::muted() },
        ),
    ];
    if server.restarts > 0 {
        second.push(Span::styled("   restarts ", theme::label()));
        second.push(Span::styled(server.restarts.to_string(), theme::warn()));
    }
    if server.backup_running {
        second.push(Span::styled("   backup running…", theme::accent()));
    }
    let third = match &server.last_exit {
        Some(exit) if !server.state.is_live() => Line::from(vec![
            Span::styled("last exit ", theme::label()),
            Span::styled(
                exit.clone(),
                if server.state == ServerState::Crashed { theme::bad() } else { theme::muted() },
            ),
        ]),
        _ => Line::from(""),
    };
    frame.render_widget(Paragraph::new(vec![Line::from(first), Line::from(second), third]), info);

    let [cpu_area, mem_area] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(charts);
    for (target, title, data, headline, color) in [
        (
            cpu_area,
            "cpu",
            server.cpu_history.clone(),
            format!("{:.0}%", server.cpu),
            theme::ACCENT,
        ),
        (
            mem_area,
            "memory",
            server.mem_history.clone(),
            format!("{} / {}", fmt_bytes(server.mem), fmt_bytes(server.mem_limit)),
            theme::HEADER,
        ),
    ] {
        let [label, chart] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(target);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{title} "), theme::label()),
                Span::styled(headline, theme::value()),
            ])),
            label,
        );
        let width = chart.width.saturating_sub(1) as usize;
        let slice: Vec<u64> = data.iter().rev().take(width).rev().copied().collect();
        let max = if title == "memory" {
            (server.mem_limit / (1024 * 1024)).max(slice.iter().copied().max().unwrap_or(1))
        } else {
            slice.iter().copied().max().unwrap_or(100).max(100)
        };
        frame.render_widget(
            Sparkline::default().data(slice).max(max.max(1)).style(Style::default().fg(color)),
            Rect { width: chart.width.saturating_sub(1), ..chart },
        );
    }
}

fn host_panel(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let host = &snapshot.host;
    let block = panel(format!("Host — {}", host.name));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let mem_ratio = ratio(host.mem_used, host.mem_total);
    let disk_ratio = ratio(host.disk_used, host.disk_total);
    let lines = vec![
        meter(
            "cpu",
            host.cpu_usage as f64 / 100.0,
            &format!("{:.0}%", host.cpu_usage),
            width,
            theme::threshold(host.cpu_usage as f64, 75.0, 92.0),
        ),
        meter(
            "memory",
            mem_ratio,
            &format!("{} / {}", fmt_bytes(host.mem_used), fmt_bytes(host.mem_total)),
            width,
            theme::threshold(mem_ratio * 100.0, 80.0, 92.0),
        ),
        meter(
            "disk",
            disk_ratio,
            &format!("{} / {}", fmt_bytes(host.disk_used), fmt_bytes(host.disk_total)),
            width,
            theme::threshold(disk_ratio * 100.0, 80.0, 92.0),
        ),
        Line::from(""),
        kv("cpu", truncate(&host.cpu_brand, width.saturating_sub(14)), theme::value()),
        kv("cores", format!("{} logical", host.cores), theme::value()),
        kv("load", format!("{:.2}  {:.2}  {:.2}", host.load[0], host.load[1], host.load[2]), theme::value()),
        kv("os", truncate(&host.os, width.saturating_sub(14)), theme::value()),
        kv(
            "java",
            truncate(&host.java, width.saturating_sub(14)),
            if host.java.contains("not found") { theme::warn() } else { theme::value() },
        ),
        kv(
            "panel",
            format!("v{} · up {} · {} session(s)", host.version, fmt_duration(host.panel_uptime_secs), snapshot.sessions),
            theme::value(),
        ),
        kv(
            "you",
            format!(
                "{} ({}{})",
                snapshot.me.username,
                snapshot.me.role,
                if snapshot.me.servers.is_empty() || snapshot.me.role == Role::Admin {
                    String::new()
                } else {
                    format!(", {} server(s)", snapshot.me.servers.len())
                }
            ),
            theme::accent(),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------- console ---

fn line_style(text: &str) -> Style {
    if text.starts_with("[mchost]") {
        theme::accent()
    } else if text.starts_with("> ") {
        Style::default().fg(theme::HEADER)
    } else if text.contains("ERROR") || text.contains("SEVERE") || text.contains("Exception") || text.contains("\tat ") {
        theme::bad()
    } else if text.contains("WARN") {
        theme::warn()
    } else if text.contains(" joined the game") || text.contains(" left the game") {
        Style::default().fg(theme::CYAN)
    } else if text.contains("Done (") {
        theme::good()
    } else {
        theme::base()
    }
}

/// The last `height` rows of console, wrapped to `width`, skipping the
/// newest `scroll` source lines.
fn console_lines(app: &Panel, width: usize, height: usize, scroll: usize) -> Vec<Line<'static>> {
    if app.console.is_empty() {
        let text = match app.server() {
            Some(s) if s.state == ServerState::Stopped => "server is stopped — press s to start it",
            Some(_) => "waiting for output…",
            None => "no server selected",
        };
        return vec![Line::from(Span::styled(text, theme::muted()))];
    }
    let width = width.max(10);
    let mut rows: Vec<Line> = Vec::new();
    let end = app.console.len().saturating_sub(scroll);
    for entry in app.console.iter().take(end).rev() {
        let style = line_style(&entry.text);
        let chars: Vec<char> = entry.text.replace('\t', "    ").chars().collect();
        let mut chunks: Vec<String> = chars.chunks(width).map(|c| c.iter().collect()).collect();
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        for chunk in chunks.into_iter().rev() {
            rows.push(Line::from(Span::styled(chunk, style)));
            if rows.len() >= height {
                break;
            }
        }
        if rows.len() >= height {
            break;
        }
    }
    rows.reverse();
    rows
}

fn console(frame: &mut Frame, app: &mut Panel, area: Rect) {
    if app.server().is_none() {
        no_server(frame, app, area, "Console");
        return;
    }
    let [log_area, input_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);
    let title = if app.console_scroll > 0 {
        format!(
            "Console — {}   ↑ {} lines back (End to follow)",
            app.server_name().unwrap_or_default(),
            app.console_scroll
        )
    } else {
        format!("Console — {}", app.server_name().unwrap_or_default())
    };
    let block = panel(title);
    let inner = block.inner(log_area);
    frame.render_widget(block, log_area);
    let lines = console_lines(app, inner.width as usize, inner.height as usize, app.console_scroll);
    frame.render_widget(Paragraph::new(lines), inner);

    let focused = app.input_focus;
    let block = focus_panel("Command", focused);
    let inner = block.inner(input_area);
    frame.render_widget(block, input_area);
    let can_type = app.role() >= Role::Operator;
    let mut spans = vec![Span::styled("› ", if focused { theme::accent() } else { theme::muted() })];
    if !can_type {
        spans.push(Span::styled("view-only account — commands are disabled", theme::muted()));
    } else if focused {
        let (before, after) = app.input.value.split_at(app.input.cursor);
        spans.push(Span::styled(before.to_string(), theme::value()));
        spans.push(Span::styled("█", theme::accent()));
        spans.push(Span::styled(after.to_string(), theme::value()));
        if app.input.value.is_empty() {
            spans.push(Span::styled(" e.g. say hello, whitelist add Alex, time set day", theme::muted()));
        }
    } else if app.input.value.is_empty() {
        spans.push(Span::styled("press i to type a command", theme::muted()));
    } else {
        spans.push(Span::styled(app.input.value.clone(), theme::base()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

// ------------------------------------------------------------- players ---

fn players(frame: &mut Frame, app: &mut Panel, area: Rect) {
    if app.server().is_none() {
        no_server(frame, app, area, "Players");
        return;
    }
    let Some(info) = app.players.clone() else {
        frame.render_widget(Paragraph::new("loading…").style(theme::muted()).block(panel("Players")), area);
        return;
    };
    let rows = app.player_rows();
    let title = format!(
        "Players  {}/{} online  ·  whitelist {}{}",
        info.online.len(),
        info.max_players,
        if info.whitelist_enabled { "on" } else { "off" },
        if info.running { "" } else { "  ·  server stopped: changes need it running" }
    );
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  nobody yet — players show up here when they join, or are opped, whitelisted or banned",
                    theme::muted(),
                )),
                Line::from(Span::styled("  press a to add a player by name", theme::muted())),
            ])
            .block(panel(title)),
            area,
        );
        return;
    }
    let mark = |on: bool, text: &'static str| {
        if on {
            Cell::from(Span::styled(text, theme::good()))
        } else {
            Cell::from(Span::styled("·", theme::muted()))
        }
    };
    let table_rows: Vec<Row> = rows
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(Span::styled(r.name.clone(), theme::value())),
                if r.online {
                    Cell::from(Span::styled("● online", theme::good()))
                } else {
                    Cell::from(Span::styled("offline", theme::muted()))
                },
                mark(r.op, "op"),
                mark(r.whitelisted, "yes"),
                match &r.banned {
                    Some(reason) => Cell::from(Span::styled(
                        if reason.is_empty() { "banned".to_string() } else { format!("banned — {reason}") },
                        theme::bad(),
                    )),
                    None => Cell::from(Span::styled("·", theme::muted())),
                },
            ])
        })
        .collect();
    app.player_sel = app.player_sel.min(rows.len() - 1);
    table_block(
        table_rows,
        vec![
            Constraint::Length(20),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Min(10),
        ],
        &["PLAYER", "STATUS", "OP", "WHITELIST", "BAN"],
        title,
        Some(app.player_sel),
        frame,
        area,
    );
}

// --------------------------------------------------------------- files ---

fn files(frame: &mut Frame, app: &mut Panel, area: Rect) {
    if app.server().is_none() {
        no_server(frame, app, area, "Files");
        return;
    }
    if app.role() < Role::Operator {
        needs_role(frame, area, "Files", Role::Operator);
        return;
    }
    let title = format!("Files — {}:/{}", app.server_name().unwrap_or_default(), app.file_path);
    let Some(listing) = app.files.clone() else {
        frame.render_widget(Paragraph::new("loading…").style(theme::muted()).block(panel(title)), area);
        return;
    };
    let mut rows: Vec<Row> = Vec::new();
    if !app.file_path.is_empty() {
        rows.push(Row::new(vec![
            Cell::from(Span::styled("↰ ..", theme::accent())),
            Cell::from(""),
            Cell::from(""),
        ]));
    }
    for entry in &listing.entries {
        rows.push(Row::new(vec![
            if entry.dir {
                Cell::from(Span::styled(format!("▸ {}/", entry.name), theme::accent()))
            } else {
                Cell::from(Span::styled(format!("  {}", entry.name), theme::base()))
            },
            Cell::from(if entry.dir { String::new() } else { fmt_bytes(entry.size) }),
            Cell::from(Span::styled(fmt_time(entry.modified), theme::muted())),
        ]));
    }
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  empty folder — n new file, N new folder", theme::muted())).block(panel(title)),
            area,
        );
        return;
    }
    table_block(
        rows,
        vec![Constraint::Min(20), Constraint::Length(11), Constraint::Length(17)],
        &["NAME", "SIZE", "MODIFIED"],
        title,
        Some(app.file_sel),
        frame,
        area,
    );
}

// -------------------------------------------------------------- config ---

fn config(frame: &mut Frame, app: &mut Panel, area: Rect) {
    if app.server().is_none() {
        no_server(frame, app, area, "Config");
        return;
    }
    let [left, right] = Layout::horizontal([Constraint::Length(46), Constraint::Min(30)]).areas(area);
    let room = (left.width as usize).saturating_sub(16);
    let lines = match &app.settings {
        Some(s) => {
            let onoff = |b: bool| if b { ("on", theme::good()) } else { ("off", theme::muted()) };
            let (ar, ars) = onoff(s.auto_restart);
            let (ast, asts) = onoff(s.auto_start);
            vec![
                kv("type", format!("{} {}", s.kind, s.version), theme::accent()),
                kv("memory", format!("{} – {} MiB", s.memory_min_mb, s.memory_max_mb), theme::value()),
                kv("port", s.port.to_string(), theme::value()),
                kv("auto start", ast, asts),
                kv("auto restart", ar, ars),
                kv("stop wait", format!("{}s", s.stop_timeout_secs), theme::value()),
                Line::from(""),
                kv("java", truncate_left(&s.java, room), theme::value()),
                kv("jar", truncate(&s.jar, room), theme::value()),
                kv("jvm args", if s.jvm_args.is_empty() { "–".into() } else { truncate(&s.jvm_args, room) }, theme::value()),
                Line::from(""),
                Line::from(vec![
                    Span::styled("e", theme::accent()),
                    Span::styled(" edit    ", theme::muted()),
                    Span::styled("I", theme::accent()),
                    Span::styled(" change type / version", theme::muted()),
                ]),
            ]
        }
        None => vec![Line::from(Span::styled("loading…", theme::muted()))],
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel("Launch settings")),
        left,
    );

    if app.role() < Role::Operator {
        needs_role(frame, right, "server.properties", Role::Operator);
        return;
    }
    if app.properties.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "  no server.properties yet — the server writes it on first start",
                    theme::muted(),
                )),
                Line::from(Span::styled("  press a to add a property now", theme::muted())),
            ])
            .block(panel("server.properties")),
            right,
        );
        return;
    }
    let rows: Vec<Row> = app
        .properties
        .iter()
        .map(|p| {
            let style = match p.value.as_str() {
                "true" => theme::good(),
                "false" => theme::muted(),
                _ => theme::value(),
            };
            let shown = if p.key.contains("password") && !p.value.is_empty() {
                "••••••".to_string()
            } else {
                p.value.clone()
            };
            Row::new(vec![
                Cell::from(Span::styled(p.key.clone(), theme::base())),
                Cell::from(Span::styled(shown, style)),
            ])
        })
        .collect();
    table_block(
        rows,
        vec![Constraint::Length(34), Constraint::Min(10)],
        &["KEY", "VALUE"],
        format!("server.properties  {}", app.properties.len()),
        Some(app.prop_sel),
        frame,
        right,
    );
}

// ------------------------------------------------------------- backups ---

fn backups(frame: &mut Frame, app: &mut Panel, area: Rect) {
    if app.server().is_none() {
        no_server(frame, app, area, "Backups");
        return;
    }
    if app.role() < Role::Operator {
        needs_role(frame, area, "Backups", Role::Operator);
        return;
    }
    let running = app.server().map(|s| s.backup_running).unwrap_or(false);
    let total: u64 = app.backups.iter().map(|b| b.size).sum();
    let title = format!(
        "Backups — {}  {} · {}{}",
        app.server_name().unwrap_or_default(),
        app.backups.len(),
        fmt_bytes(total),
        if running { "  ·  working…" } else { "" }
    );
    if app.backups.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  no backups yet — press b to back up now, or add a backup task in Tasks",
                    theme::muted(),
                )),
            ])
            .block(panel(title)),
            area,
        );
        return;
    }
    let rows: Vec<Row> = app
        .backups
        .iter()
        .map(|b| {
            Row::new(vec![
                Cell::from(Span::styled(b.id.clone(), theme::value())),
                Cell::from(format!("{} ({})", fmt_time(b.created), fmt_relative(b.created))),
                Cell::from(fmt_bytes(b.size)),
                Cell::from(Span::styled(b.by.clone(), theme::label())),
                Cell::from(b.note.clone()),
            ])
        })
        .collect();
    table_block(
        rows,
        vec![
            Constraint::Length(16),
            Constraint::Length(28),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Min(10),
        ],
        &["ID", "CREATED", "SIZE", "BY", "NOTE"],
        title,
        Some(app.backup_sel),
        frame,
        area,
    );
}

// --------------------------------------------------------------- tasks ---

fn tasks(frame: &mut Frame, app: &mut Panel, area: Rect) {
    let title = format!("Scheduled tasks  {}", app.schedules.len());
    if app.schedules.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  nothing scheduled — press a to add a restart, backup, broadcast or command",
                    theme::muted(),
                )),
            ])
            .block(panel(title)),
            area,
        );
        return;
    }
    let rows: Vec<Row> = app
        .schedules
        .iter()
        .map(|t| {
            let action = if t.payload.is_empty() {
                t.action.to_string()
            } else {
                format!("{} `{}`", t.action, t.payload)
            };
            let result_style = match &t.last_result {
                Some(r) if r.starts_with("error") => theme::bad(),
                _ => theme::muted(),
            };
            Row::new(vec![
                Cell::from(Span::styled(t.name.clone(), theme::value())),
                Cell::from(t.server.clone()),
                Cell::from(action),
                Cell::from(t.when.to_string()),
                Cell::from(if t.enabled {
                    Span::styled(t.next_run.map(fmt_relative).unwrap_or_default(), theme::accent())
                } else {
                    Span::styled("paused", theme::warn())
                }),
                Cell::from(Span::styled(
                    t.last_run
                        .map(|r| format!("{} — {}", fmt_relative(r), t.last_result.clone().unwrap_or_default()))
                        .unwrap_or_else(|| "never".into()),
                    result_style,
                )),
            ])
        })
        .collect();
    table_block(
        rows,
        vec![
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(24),
            Constraint::Length(13),
            Constraint::Length(9),
            Constraint::Min(10),
        ],
        &["TASK", "SERVER", "ACTION", "WHEN", "NEXT", "LAST RUN"],
        title,
        Some(app.task_sel),
        frame,
        area,
    );
}

// --------------------------------------------------------------- users ---

fn users(frame: &mut Frame, app: &mut Panel, area: Rect) {
    let admin = app.role() == Role::Admin;
    let title = if admin {
        format!("Users  {}", app.users.len())
    } else {
        "Your account  (p to change your password)".into()
    };
    let me = app.welcome.username.clone();
    let rows: Vec<Row> = app
        .users
        .iter()
        .map(|u| {
            let role_style = match u.role {
                Role::Admin => theme::bad(),
                Role::Operator => theme::accent(),
                Role::Viewer => theme::muted(),
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(u.username.clone(), theme::value()),
                    Span::styled(if u.username == me { "  (you)" } else { "" }, theme::muted()),
                ])),
                Cell::from(Span::styled(u.role.to_string(), role_style)),
                Cell::from(if u.servers.is_empty() || u.role == Role::Admin {
                    "all".to_string()
                } else {
                    u.servers.join(", ")
                }),
                Cell::from(u.last_login.map(fmt_relative).unwrap_or_else(|| "never".into())),
                Cell::from(Span::styled(u.last_ip.clone().unwrap_or_default(), theme::muted())),
                Cell::from(Span::styled(fmt_time(u.created), theme::muted())),
            ])
        })
        .collect();
    table_block(
        rows,
        vec![
            Constraint::Length(22),
            Constraint::Length(9),
            Constraint::Min(12),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(17),
        ],
        &["USER", "ROLE", "SERVERS", "LAST LOGIN", "FROM", "CREATED"],
        title,
        Some(app.user_sel),
        frame,
        area,
    );
}

// ------------------------------------------------------------ activity ---

fn activity(frame: &mut Frame, app: &mut Panel, area: Rect) {
    let title = if app.role() == Role::Admin {
        format!("Activity — everyone  {}", app.activity.len())
    } else {
        format!("Activity — yours  {}", app.activity.len())
    };
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.activity.is_empty() {
        frame.render_widget(Paragraph::new(Span::styled("nothing yet", theme::muted())), inner);
        return;
    }
    let lines: Vec<Line> = app
        .activity
        .iter()
        .skip(app.activity_scroll)
        .take(inner.height as usize)
        .map(|e| {
            Line::from(vec![
                Span::styled(format!("{}  ", fmt_time(e.time)), theme::muted()),
                Span::styled(if e.ok { "✓ " } else { "✗ " }, if e.ok { theme::good() } else { theme::bad() }),
                Span::styled(format!("{:<14}", truncate(&e.user, 14)), theme::accent()),
                Span::styled(format!("{:<16}", truncate(&e.action, 16)), theme::value()),
                Span::styled(e.detail.clone(), theme::base()),
                Span::styled(format!("  {}", e.ip), theme::muted()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
