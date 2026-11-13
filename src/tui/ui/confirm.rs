//! The write gate's question ([`crate::tui::app::confirm`]): a centred dialog over the
//! view, naming the profile and showing the exact command. The key bar shows its keys.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use super::style;
use super::widgets::{panel, rows, trunc, width};
use crate::tui::app::confirm::{Confirm, Expect};

/// Widest the dialog gets.
const MAX_W: u16 = 76;

/// `text` cut into lines of at most `w` columns.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for ch in text.chars() {
        if width(&line) + super::widgets::char_width(ch) > w {
            out.push(std::mem::take(&mut line));
        }
        line.push(ch);
    }
    out.push(line);
    out
}

/// Draws `c` centred in `area`.
pub fn draw(frame: &mut Frame, area: Rect, c: &Confirm) {
    let w = area.width.min(MAX_W);
    let inner = usize::from(w.saturating_sub(4));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for text in c.lines() {
        let s = if text.starts_with("Sends:") {
            style::fgb()
        } else {
            style::fg()
        };
        for part in wrap(&text, inner) {
            lines.push(Line::from(Span::styled(format!(" {part}"), s)));
        }
    }
    if let Expect::Type(_) = &c.expect {
        let room = inner.saturating_sub(3);
        let typed = crate::value::escape(&c.typed);
        let shown = if width(&typed) > room {
            let chars: Vec<char> = typed.chars().collect();
            let mut keep = String::from("…");
            let start = chars.len().saturating_sub(room.saturating_sub(1));
            keep.extend(&chars[start..]);
            keep
        } else {
            typed
        };
        lines.push(Line::from(vec![
            Span::styled(" › ", style::amber()),
            Span::styled(shown, style::fgb()),
            Span::styled("▏", style::amber()),
        ]));
    }
    lines.push(Line::from(""));
    let mut keys = vec![Span::raw(" ")];
    for (k, l) in c.keys() {
        keys.push(Span::styled(k, style::key()));
        keys.push(Span::styled(format!(" {l}   "), style::dim()));
    }
    lines.push(Line::from(keys));

    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let right = format!(
        "{}{}",
        crate::value::escape(&c.profile),
        if c.prod { " · PROD" } else { "" }
    );
    let title = trunc(&c.title(), inner.saturating_sub(width(&right) + 2));
    let body = panel(frame, rect, &title, &right);
    rows(frame, body, lines, None);
}

/// The key bar while the dialog is open.
pub fn footer(c: &Confirm) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, l) in c.keys() {
        spans.push(Span::styled(k, style::key()));
        spans.push(Span::styled(format!(" {l}   "), style::dim()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_wrap() {
        assert_eq!(
            wrap("abcdef", 4),
            vec!["abcd".to_string(), "ef".to_string()]
        );
        assert_eq!(wrap("", 4), vec![String::new()]);
    }
}
