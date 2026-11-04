//! Small shared building blocks: panels, inline meters, overlays.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme;

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(format!(" {} ", title.into()), theme::title()))
}

/// A panel whose border lights up when it has the keyboard.
pub fn focus_panel(title: impl Into<String>, focused: bool) -> Block<'static> {
    let block = panel(title);
    if focused {
        block.border_style(Style::default().fg(theme::ACCENT))
    } else {
        block
    }
}

/// `label   value` on one line, label muted and value bold.
pub fn kv(label: &str, value: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<13}"), theme::label()),
        Span::styled(value.into(), style),
    ])
}

/// A compact inline bar: `label ██████░░░░ 42%`.
pub fn meter(label: &str, ratio: f64, text: &str, avail: usize, color: Color) -> Line<'static> {
    let ratio = if ratio.is_finite() { ratio.clamp(0.0, 1.0) } else { 0.0 };
    let width = avail
        .saturating_sub(8 + text.chars().count() + 1)
        .clamp(3, 60);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    Line::from(vec![
        Span::styled(format!("{label:<8}"), theme::label()),
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(width - filled), Style::default().fg(theme::BORDER)),
        Span::styled(format!(" {text}"), theme::value()),
    ])
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    cell
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub fn ratio(used: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { used as f64 / total as f64 }
}

pub fn confirm_overlay(frame: &mut Frame, area: Rect, prompt: &str, danger: bool) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(format!("  {prompt}"), theme::base())),
        Line::from(""),
        Line::from(vec![
            Span::styled("  y", theme::accent()),
            Span::styled(" confirm    ", theme::muted()),
            Span::styled("any other key", theme::accent()),
            Span::styled(" cancel", theme::muted()),
        ]),
    ];
    let height = 7 + (prompt.chars().count() as u16 / 56);
    let target = centered(area, 64, height);
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if danger { theme::BAD } else { theme::WARN }))
                .title(Span::styled(" Confirm ", theme::title())),
        ),
        target,
    );
}

pub fn help_overlay(frame: &mut Frame, area: Rect) {
    let sections: &[(&str, &[(&str, &str)])] = &[
        (
            "Everywhere",
            &[
                ("1-9 / Tab", "switch view"),
                ("[ ]", "previous / next server"),
                ("↑ ↓ / j k", "move selection, scroll"),
                ("s  x  R", "start / stop / restart the server"),
                ("X", "kill the server process (no save)"),
                ("c", "create a server (admin)"),
                ("D", "delete the server (admin)"),
                ("L", "log out"),
                ("q / Ctrl-C", "close the panel — servers keep running"),
            ],
        ),
        (
            "Console",
            &[("type + Enter", "run a command"), ("Esc / i", "leave / focus the prompt"), ("PgUp PgDn", "scroll")],
        ),
        (
            "Players",
            &[
                ("o w b", "toggle op / whitelist / ban"),
                ("K m g", "kick / message / game mode"),
                ("a  W", "add by name / whitelist on-off"),
            ],
        ),
        (
            "Files",
            &[
                ("Enter ⌫", "open / go up"),
                ("n N r d", "new file / folder, rename, delete"),
                ("in editor", "Ctrl-S save, Esc close"),
            ],
        ),
        (
            "Other views",
            &[
                ("Config", "Enter edit property, e settings, I reinstall"),
                ("Backups", "b back up now, r restore, d delete"),
                ("Tasks", "a add, e pause/resume, Enter run now, d delete"),
                ("Users", "a add, p password, e role, d remove"),
            ],
        ),
    ];
    let mut lines = vec![Line::from("")];
    for (title, entries) in sections {
        lines.push(Line::from(Span::styled(format!("  {title}"), theme::title())));
        for (key, description) in entries.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("    {key:<14}"), theme::accent()),
                Span::styled(*description, theme::base()),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled("  press any key to close", theme::muted())));
    let height = lines.len() as u16 + 2;
    let target = centered(area, 70, height);
    frame.render_widget(Clear, target);
    frame.render_widget(Paragraph::new(lines).block(panel("Keys")), target);
}
