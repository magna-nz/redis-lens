//! The `?` key overlay, rendered from [`crate::tui::keymap`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use super::style;
use super::widgets::{pad_r, panel, rows, trunc, width};
use crate::tui::app::View;
use crate::tui::keymap::{self, KeyHelp};

/// Widest the overlay gets.
const MAX_W: u16 = 66;
/// Width of the keys column.
const KEYS_W: usize = 14;

fn entry(h: &KeyHelp, inner: usize) -> Line<'static> {
    let keys = pad_r(h.keys, KEYS_W.min(inner));
    let room = inner.saturating_sub(width(&keys) + 1);
    Line::from(vec![
        Span::styled(keys, style::amber()),
        Span::raw(" "),
        Span::styled(trunc(h.help, room), style::fg()),
    ])
}

fn heading(text: &str, inner: usize) -> Line<'static> {
    Line::from(Span::styled(trunc(text, inner), style::title()))
}

/// Draw the overlay centred in `area`, over whatever is already there.
pub fn draw(frame: &mut Frame, area: Rect, view: View) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Inner width is known only after the box is sized, so size from the area.
    let w = area.width.min(MAX_W);
    let inner = usize::from(w.saturating_sub(2));
    lines.push(heading("Everywhere", inner));
    lines.extend(keymap::global().iter().map(|h| entry(h, inner)));
    lines.push(heading(view.title(), inner));
    lines.extend(keymap::for_view(view).iter().map(|h| entry(h, inner)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        trunc("? or esc closes", inner),
        style::dim(),
    )));

    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let title = format!("Keys · {}", view.title());
    let body = panel(frame, rect, &title, "");
    rows(frame, body, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(view: View, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, f.area(), view)).unwrap();
        t
    }

    #[test]
    fn connections_80x24() {
        insta::assert_snapshot!(render(View::Connections, 80, 24).backend());
    }

    #[test]
    fn patterns_80x24() {
        insta::assert_snapshot!(render(View::Patterns, 80, 24).backend());
    }

    #[test]
    fn ttl_80x24() {
        insta::assert_snapshot!(render(View::Ttl, 80, 24).backend());
    }

    #[test]
    fn diff_80x24() {
        insta::assert_snapshot!(render(View::Diff, 80, 24).backend());
    }

    #[test]
    fn hot_keys_80x24() {
        insta::assert_snapshot!(render(View::HotKeys, 80, 24).backend());
    }

    #[test]
    fn patterns_narrow_40x12() {
        insta::assert_snapshot!(render(View::Patterns, 40, 12).backend());
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for v in View::ALL {
            for (w, h) in [(20, 5), (40, 12), (1, 1), (0, 0), (3, 30), (200, 2)] {
                render(v, w, h);
            }
        }
    }
}
