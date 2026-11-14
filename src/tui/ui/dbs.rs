//! The database list (`b`, spec "Database switch (P4)"): a centred overlay over the
//! view, with the databases the server holds, the current one marked. The key bar
//! shows its keys. Nothing here is a credential: database numbers, key counts and the
//! profile's name.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use super::style;
use super::widgets::{pad_l, pad_r, panel, rows, scroll_offset, trunc, width};
use crate::format;
use crate::tui::app::views::dbs::{DbList, Stage};

/// Widest the overlay gets.
const MAX_W: u16 = 64;

/// `text` cut into lines of at most `w` columns, at spaces.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && width(&line) + 1 + width(word) > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// The keys the bar shows while the list is open: `(key, label)`.
pub fn keys(l: &DbList) -> Vec<(&'static str, &'static str)> {
    if l.ask.is_some() {
        vec![
            ("y", "Switch and save"),
            ("n", "Don't save"),
            ("esc", "Back"),
        ]
    } else {
        vec![
            ("↑↓", "Move"),
            ("enter", "Switch"),
            ("s", "Switch and save"),
            ("esc", "Close"),
        ]
    }
}

/// The key bar while the list is open.
pub fn footer(l: &DbList) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, label) in keys(l) {
        spans.push(Span::styled(k, style::key()));
        spans.push(Span::styled(format!(" {label}   "), style::dim()));
    }
    Line::from(spans)
}

/// Draws `l` centred in `area`.
pub fn draw(frame: &mut Frame, area: Rect, l: &DbList) {
    let w = area.width.min(MAX_W);
    let inner = usize::from(w.saturating_sub(4));
    let num_w = 9;

    // What goes below the rows: the reason, the note, the question, the keys.
    let mut below: Vec<Line<'static>> = Vec::new();
    let mut texts: Vec<(String, Style)> = Vec::new();
    if let Stage::Unread(why) = &l.stage {
        texts.push(((*why).to_string(), style::amber()));
    }
    if let Some(note) = l.note {
        texts.push((note.to_string(), style::amber()));
    }
    if let Some(db) = l.ask {
        let name = l
            .saved_as
            .as_deref()
            .map_or_else(String::new, crate::value::escape);
        texts.push((
            format!("Switch to db {db} and save it as the database of {name} in the config file?"),
            style::fgb(),
        ));
        texts.push((
            "Only db is written. The next connect starts on it.".to_string(),
            style::dim(),
        ));
    }
    for (text, st) in texts {
        for part in wrap(&text, inner.saturating_sub(1)) {
            below.push(Line::from(Span::styled(format!(" {part}"), st)));
        }
    }
    let mut keyline = vec![Span::raw(" ")];
    for (k, label) in keys(l) {
        keyline.push(Span::styled(k, style::key()));
        keyline.push(Span::styled(format!(" {label}   "), style::dim()));
    }

    // Borders (2), the header row, a blank line before `below`, and the key line.
    let fixed = 2 + 1 + 1 + below.len() + 1;
    let room = usize::from(area.height).saturating_sub(fixed).max(1);
    let shown = l.rows.len().min(room);
    let offset = scroll_offset(l.sel, shown).min(l.rows.len().saturating_sub(shown));

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        format!(
            " {}{}{}",
            pad_r("Database", inner.saturating_sub(2 * num_w + 1)),
            pad_l("Keys", num_w),
            pad_l("Expires", num_w)
        ),
        style::hd(),
    ))];
    for r in l.rows.iter().skip(offset).take(shown) {
        let name = if r.db == l.current {
            format!("db {} (current)", r.db)
        } else {
            format!("db {}", r.db)
        };
        let (keys, expires) = match r.counts {
            Some((k, e)) => (format::grouped(k), format::grouped(e)),
            // Still loading, or never read.
            None if matches!(l.stage, Stage::Loading(_)) => ("…".to_string(), "…".to_string()),
            None => ("—".to_string(), "—".to_string()),
        };
        lines.push(Line::from(Span::styled(
            format!(
                " {}{}{}",
                pad_r(&name, inner.saturating_sub(2 * num_w + 1)),
                pad_l(&keys, num_w),
                pad_l(&expires, num_w)
            ),
            style::fg(),
        )));
    }
    let sel_line = l.sel.checked_sub(offset).map(|i| i + 1);
    let rows_end = lines.len();
    lines.push(Line::from(""));
    lines.extend(below);
    lines.push(Line::from(keyline));

    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let right = match &l.saved_as {
        Some(name) => format!(
            "{}{}",
            crate::value::escape(name),
            if l.prod { " · PROD" } else { "" }
        ),
        None => "container".to_string(),
    };
    let title = trunc("Databases", inner.saturating_sub(width(&right) + 2));
    let body = panel(frame, rect, &title, &right);
    let sel = sel_line.filter(|&i| i < rows_end);
    rows(frame, body, lines, sel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_cuts_at_spaces() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert!(wrap("", 7).is_empty());
    }
}
