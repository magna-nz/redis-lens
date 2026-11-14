//! The `?` overlay, rendered from [`crate::tui::keymap`]: this screen's keys and the
//! keys that work everywhere, or, once something is typed, every screen and every
//! action of every screen that matches, each with where it is. `enter` runs the
//! selected one (see `App::help_key`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::style;
use super::widgets::{dialog, pad_r, rows, scroll_offset, trunc, width};
use crate::tui::app::{App, Screen};
use crate::tui::keymap::{self, KeyHelp};

/// Widest the overlay gets.
const MAX_W: u16 = 72;
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
    Line::from(Span::styled(trunc(text, inner), style::hd()))
}

/// The search line at the top: what is typed, or how to start.
fn prompt(query: &str, inner: usize) -> Line<'static> {
    if query.is_empty() {
        Line::from(vec![
            Span::styled("› ".to_string(), style::amber()),
            Span::styled(
                trunc(
                    "type to find any action or screen…",
                    inner.saturating_sub(2),
                ),
                style::dim(),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ".to_string(), style::amber()),
            Span::styled(trunc(query, inner.saturating_sub(3)), style::fgb()),
            Span::styled("▏".to_string(), style::amber()),
        ])
    }
}

/// Draws the overlay for `app`'s screen, centred in `area`, over what is there (the
/// caller dims it first).
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    draw_for(
        frame,
        area,
        app.screen(),
        &app.help_find.query,
        app.help_find.sel,
        app.is_cluster(),
    );
}

/// [`draw`] with its inputs spelled out: the screen it was opened on, what is typed,
/// the selected hit, and whether the server is a cluster (Nodes and Slots).
pub fn draw_for(
    frame: &mut Frame,
    area: Rect,
    screen: Screen,
    query: &str,
    sel: usize,
    cluster: bool,
) {
    let w = area.width.min(MAX_W);
    // Inside the frame, one cell of space on each side.
    let inner = usize::from(w.saturating_sub(4));
    let mut lines: Vec<Line<'static>> = vec![prompt(query, inner), Line::raw("")];
    let mut selected = None;
    let hits = keymap::find(query, cluster);
    if query.trim().is_empty() {
        lines.push(heading("HERE", inner));
        lines.extend(keymap::for_screen(screen).iter().map(|h| entry(h, inner)));
        lines.push(Line::raw(""));
        lines.push(heading("EVERYWHERE", inner));
        lines.extend(keymap::global().iter().map(|h| entry(h, inner)));
    } else if hits.is_empty() {
        lines.push(Line::from(Span::styled(
            trunc("Nothing matches. Backspace edits, esc clears.", inner),
            style::dim(),
        )));
    } else {
        let sel = sel.min(hits.len() - 1);
        // The list scrolls to keep the selection in view, under the prompt, above the
        // closing hint.
        let room = usize::from(area.height.saturating_sub(2))
            .saturating_sub(lines.len() + 3)
            .max(1);
        let off = scroll_offset(sel, room);
        for (i, hit) in hits.iter().enumerate().skip(off).take(room) {
            let place = hit.place();
            let place_w = width(&place).min(inner / 2);
            let text_w = inner.saturating_sub(place_w + 3);
            let mark = if i == sel { "› " } else { "  " };
            if i == sel {
                selected = Some(lines.len());
            }
            let text = trunc(&hit.text(), text_w);
            let gap = text_w.saturating_sub(width(&text)) + 1;
            lines.push(Line::from(vec![
                Span::styled(mark.to_string(), style::amber()),
                Span::styled(text, style::fg()),
                Span::raw(" ".repeat(gap)),
                Span::styled(trunc(&place, place_w), style::dim()),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            trunc(
                "enter goes there or runs it, as if you pressed its key.",
                inner,
            ),
            style::dim(),
        )));
        lines.push(Line::from(Span::styled(
            trunc("Writes still ask first. esc clears the search.", inner),
            style::dim(),
        )));
    }
    let h = (lines.len() as u16).saturating_add(2);
    let title = format!("Help · {}", screen.title());
    // The way out sits in the top border, so it shows however many keys there are.
    let body = dialog(frame, area, w, h, &title, "esc closes");
    let body = Rect {
        x: body.x.saturating_add(1).min(body.right()),
        width: body.width.saturating_sub(2),
        ..body
    };
    rows(frame, body, lines, selected);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(screen: Screen, query: &str, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw_for(f, f.area(), screen, query, 0, false))
            .unwrap();
        t
    }

    #[test]
    fn connections_80x24() {
        insta::assert_snapshot!(render(Screen::Connections, "", 80, 24).backend());
    }

    #[test]
    fn patterns_80x24() {
        insta::assert_snapshot!(render(Screen::Patterns, "", 80, 24).backend());
    }

    #[test]
    fn overview_80x24() {
        insta::assert_snapshot!(render(Screen::Overview, "", 80, 24).backend());
    }

    #[test]
    fn overview_120x36() {
        insta::assert_snapshot!(render(Screen::Overview, "", 120, 36).backend());
    }

    #[test]
    fn filtered_80x24() {
        insta::assert_snapshot!(render(Screen::Overview, "slow", 80, 24).backend());
    }

    #[test]
    fn filtered_120x36() {
        insta::assert_snapshot!(render(Screen::Overview, "slow", 120, 36).backend());
    }

    #[test]
    fn nothing_matches() {
        let t = render(Screen::Overview, "zzzz", 80, 24);
        let text = format!("{:?}", t.backend().buffer());
        assert!(text.contains("Nothing matches"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for v in Screen::ALL {
            for q in ["", "key", "zz"] {
                for (w, h) in [(20, 5), (40, 12), (1, 1), (0, 0), (3, 30), (200, 2)] {
                    render(v, q, w, h);
                }
            }
        }
    }
}
