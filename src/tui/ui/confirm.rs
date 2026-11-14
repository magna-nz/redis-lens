//! The write gate's question ([`crate::tui::app::confirm`]): a dialog sized to its text,
//! over the dimmed screen, with the question first in its title and the profile at the
//! right. One line says what is sent, one whether it can be undone. The key bar shows
//! its keys. What each question asks for (typing the name on prod, `y`/`n` elsewhere)
//! is the gate's, not drawn here.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::analysis::wrap_hard;
use super::style;
use super::widgets::{dialog, rows, trunc, width};
use crate::tui::app::confirm::{Confirm, Expect};

/// Widest the dialog gets.
const MAX_W: u16 = 76;
/// Least wide, so a short question is not a sliver.
const MIN_W: u16 = 40;

/// The title as a question: "Delete key" is "Delete this key?".
fn question(title: &str) -> String {
    let q = match title {
        "Delete key" => "Delete this key?".to_string(),
        "Delete item" => "Delete this item?".to_string(),
        "Edit value" => "Save this change?".to_string(),
        "Add item" => "Add this item?".to_string(),
        "Add key" => "Add this key?".to_string(),
        "Rename key" => "Rename this key?".to_string(),
        "Set TTL" => "Set this TTL?".to_string(),
        "Kill connection" => "Close this connection?".to_string(),
        t => format!("{t}?"),
    };
    q.replace("??", "?")
}

/// The dialog's text lines for `c`: each wrapped to `w` cells, the first line of a
/// `Sends:` paragraph led by the label `Sends` (as the mockup: `Sends   UNLINK …`), the
/// rest under the command.
fn text_lines(c: &Confirm, w: usize) -> Vec<Line<'static>> {
    const LEAD: &str = " Sends   ";
    const UNDER: &str = "         ";
    let mut lines: Vec<Line<'static>> = Vec::new();
    for text in c.lines() {
        if let Some(cmd) = text.strip_prefix("Sends: ") {
            // One cell stays free at the right.
            let room = w.saturating_sub(LEAD.len() + 1);
            for (i, part) in wrap_hard(cmd, room).into_iter().enumerate() {
                let lead = if i == 0 { LEAD } else { UNDER };

                lines.push(Line::from(vec![
                    Span::styled(lead, style::dim()),
                    Span::styled(part, style::fgb()),
                ]));
            }
            continue;
        }
        let st = if text.starts_with("Type ") {
            style::amber()
        } else {
            style::fg()
        };
        for part in wrap_hard(&text, w.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(format!(" {part}"), st)));
        }
    }
    lines
}

fn typed_line(c: &Confirm, w: usize) -> Line<'static> {
    let room = w.saturating_sub(3);
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
    Line::from(vec![
        Span::styled(" › ", style::amber()),
        Span::styled(shown, style::fgb()),
        Span::styled("▏", style::amber()),
    ])
}

fn key_hints(c: &Confirm) -> Line<'static> {
    let mut keys = vec![Span::raw(" ")];
    for (k, l) in c.keys() {
        keys.push(Span::styled(k, style::key()));
        keys.push(Span::styled(format!(" {l}   "), style::dim()));
    }
    Line::from(keys)
}

/// Draws `c` centred in `area`, over what [`super::widgets::dim`] dimmed.
pub fn draw(frame: &mut Frame, area: Rect, c: &Confirm) {
    let max_w = area.width.min(MAX_W);
    let wrap_w = usize::from(max_w.saturating_sub(2));
    let mut lines = text_lines(c, wrap_w);
    if let Expect::Type(_) = &c.expect {
        lines.push(typed_line(c, wrap_w));
    }
    lines.push(Line::default());
    lines.push(key_hints(c));

    let right = format!(
        "{}{}",
        crate::value::escape(&c.profile),
        if c.prod { " · PROD" } else { "" }
    );
    let title = question(&c.title());
    // Sized to its text: the longest line, or the title and profile, with the border.
    let longest = lines.iter().map(Line::width).max().unwrap_or(0);
    let head = width(&title) + width(&right) + 7;
    let w = u16::try_from(longest.max(head) + 3)
        .unwrap_or(MAX_W)
        .clamp(MIN_W, MAX_W)
        .min(area.width);
    let h = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let inner = dialog(
        frame,
        area,
        w,
        h,
        &trunc(&title, usize::from(w).saturating_sub(width(&right) + 7)),
        &right,
    );
    rows(frame, inner, lines, None);
}

/// The key bar while the dialog is open.
pub fn footer(c: &Confirm) -> Line<'static> {
    key_hints(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_become_questions() {
        assert_eq!(question("Delete key"), "Delete this key?");
        assert_eq!(
            question("Set a TTL on every key of the pattern"),
            "Set a TTL on every key of the pattern?"
        );
        assert_eq!(
            question("Turn on writes for prod?"),
            "Turn on writes for prod?"
        );
    }

    #[test]
    fn a_long_name_is_split_not_cut() {
        let name = "user:".to_string() + &"x".repeat(30);
        let lines = wrap_hard(&name, 12);
        assert!(lines.iter().all(|l| width(l) <= 12), "{lines:?}");
        assert_eq!(lines.concat(), name);
        assert_eq!(wrap_hard("", 4), vec![String::new()]);
        assert_eq!(wrap_hard("ab cd ef", 5), vec!["ab cd", "ef"]);
    }
}
