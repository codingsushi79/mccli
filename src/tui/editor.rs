//! A small full-screen text editor for configs: `server.properties`,
//! plugin YAML, `ops.json`. Enough to fix a value without leaving the panel.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::theme;
use super::widgets::focus_panel;

pub struct Editor {
    pub server: String,
    pub path: String,
    lines: Vec<String>,
    /// Cursor as (line, char column).
    row: usize,
    col: usize,
    top: usize,
    left: usize,
    pub dirty: bool,
    pub readonly: bool,
    /// Esc pressed once on a dirty buffer: the next Esc discards.
    pub warned: bool,
    pub saving: bool,
}

fn byte_index(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map(|(i, _)| i).unwrap_or(line.len())
}

fn char_len(line: &str) -> usize {
    line.chars().count()
}

impl Editor {
    pub fn new(server: &str, path: &str, content: &str, readonly: bool) -> Editor {
        let mut lines: Vec<String> = content.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Editor {
            server: server.into(),
            path: path.into(),
            lines,
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            dirty: false,
            readonly,
            warned: false,
            saving: false,
        }
    }

    pub fn content(&self) -> String {
        self.lines.join("\n")
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(char_len(&self.lines[self.row]));
    }

    fn edit(&mut self) -> bool {
        if self.readonly {
            return false;
        }
        self.dirty = true;
        self.warned = false;
        true
    }

    pub fn insert(&mut self, c: char) {
        if !self.edit() {
            return;
        }
        let line = &mut self.lines[self.row];
        let at = byte_index(line, self.col);
        line.insert(at, c);
        self.col += 1;
    }

    pub fn newline(&mut self) {
        if !self.edit() {
            return;
        }
        let line = &mut self.lines[self.row];
        let at = byte_index(line, self.col);
        let rest = line.split_off(at);
        // Keep the indentation, which is most of what YAML needs.
        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        self.row += 1;
        self.col = indent.chars().count();
        self.lines.insert(self.row, format!("{indent}{rest}"));
    }

    pub fn backspace(&mut self) {
        if !self.edit() {
            return;
        }
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let at = byte_index(line, self.col - 1);
            line.remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = char_len(&self.lines[self.row]);
            self.lines[self.row].push_str(&line);
        }
    }

    pub fn delete(&mut self) {
        if !self.edit() {
            return;
        }
        let len = char_len(&self.lines[self.row]);
        if self.col < len {
            let line = &mut self.lines[self.row];
            let at = byte_index(line, self.col);
            line.remove(at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn delete_line(&mut self) {
        if !self.edit() {
            return;
        }
        if self.lines.len() > 1 {
            self.lines.remove(self.row);
            self.row = self.row.min(self.lines.len() - 1);
        } else {
            self.lines[0].clear();
        }
        self.clamp_col();
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = char_len(&self.lines[self.row]);
        }
    }

    pub fn right(&mut self) {
        if self.col < char_len(&self.lines[self.row]) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self, n: usize) {
        self.row = self.row.saturating_sub(n);
        self.clamp_col();
    }

    pub fn down(&mut self, n: usize) {
        self.row = (self.row + n).min(self.lines.len() - 1);
        self.clamp_col();
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = char_len(&self.lines[self.row]);
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let title = format!(
            "{}{} — {}{}",
            self.path,
            if self.dirty { " ●" } else { "" },
            self.server,
            if self.readonly { " (read only)" } else { "" }
        );
        let block = focus_panel(title, true);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        let height = body.height as usize;
        let gutter = self.lines.len().to_string().len().max(3) + 1;
        let width = (body.width as usize).saturating_sub(gutter + 1).max(1);
        if self.row < self.top {
            self.top = self.row;
        } else if self.row >= self.top + height {
            self.top = self.row + 1 - height;
        }
        if self.col < self.left {
            self.left = self.col;
        } else if self.col >= self.left + width {
            self.left = self.col + 1 - width;
        }

        let lines: Vec<Line> = self
            .lines
            .iter()
            .enumerate()
            .skip(self.top)
            .take(height)
            .map(|(i, text)| {
                let mut spans = vec![Span::styled(
                    format!("{:>w$} ", i + 1, w = gutter - 1),
                    if i == self.row { theme::accent() } else { theme::muted() },
                )];
                let visible: Vec<char> = text.chars().skip(self.left).take(width).collect();
                let style = line_style(text);
                if i == self.row {
                    let cursor = self.col - self.left;
                    let before: String = visible.iter().take(cursor).collect();
                    let at: String = visible.get(cursor).map(|c| c.to_string()).unwrap_or(" ".into());
                    let after: String = visible.iter().skip(cursor + 1).collect();
                    spans.push(Span::styled(before, style));
                    spans.push(Span::styled(
                        at,
                        Style::default().bg(theme::ACCENT).fg(theme::SELECTED_BG).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(after, style));
                } else {
                    spans.push(Span::styled(visible.into_iter().collect::<String>(), style));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), body);

        let mut spans = vec![
            Span::styled(format!(" Ln {}, Col {}  ", self.row + 1, self.col + 1), theme::muted()),
            Span::styled("Ctrl-S", theme::accent()),
            Span::styled(" save   ", theme::muted()),
            Span::styled("Ctrl-K", theme::accent()),
            Span::styled(" delete line   ", theme::muted()),
            Span::styled("Esc", theme::accent()),
            Span::styled(" close", theme::muted()),
        ];
        if self.saving {
            spans.push(Span::styled("   saving…", theme::accent()));
        } else if self.warned {
            spans.push(Span::styled("   unsaved changes — Esc again to discard", theme::warn()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), status);
    }
}

/// Light syntax colouring that suits properties, YAML and logs alike.
fn line_style(text: &str) -> Style {
    let trimmed = text.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with("//") {
        theme::muted()
    } else {
        theme::base()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_joining_lines() {
        let mut e = Editor::new("s", "f", "ab\ncd", false);
        e.end();
        e.newline();
        e.insert('x');
        assert_eq!(e.content(), "ab\nx\ncd");
        e.home();
        e.backspace();
        assert_eq!(e.content(), "abx\ncd");
        e.end();
        e.delete();
        assert_eq!(e.content(), "abxcd");
        assert!(e.dirty);
    }

    #[test]
    fn indentation_carries_over() {
        let mut e = Editor::new("s", "f", "  key: 1", false);
        e.end();
        e.newline();
        e.insert('x');
        assert_eq!(e.content(), "  key: 1\n  x");
    }

    #[test]
    fn readonly_is_readonly() {
        let mut e = Editor::new("s", "f", "abc", true);
        e.insert('x');
        e.delete_line();
        assert_eq!(e.content(), "abc");
        assert!(!e.dirty);
    }

    #[test]
    fn multibyte_is_safe() {
        let mut e = Editor::new("s", "f", "héllo", false);
        e.right();
        e.right();
        e.backspace();
        assert_eq!(e.content(), "hllo");
    }
}
