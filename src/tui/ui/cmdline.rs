//! Draws the command line panel ([`CmdlineState`]) at the bottom of the body, over the
//! view. Its input line is drawn in place of the key bar by `ui::footer`.
//!
//! Everything drawn here is safe already: commands are masked lines
//! ([`crate::live::command::masked_line`]), replies were escaped by
//! [`crate::live::cmdline::format_reply`], notes are redis-lens's own words.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use super::style;
use super::widgets::{char_width, panel, rows, trunc};
use crate::tui::app::views::Ctx;
use crate::tui::app::views::cmdline::{Body, CmdlineState, Entry};

/// The panel's height in a body `body` rows tall: half of it, 6 to 16 rows.
pub fn height(body: u16) -> u16 {
    (body / 2).clamp(6, 16).min(body)
}

/// Rows under the output for the helper.
const HELPER_ROWS: u16 = 2;

pub fn draw(frame: &mut Frame, body: Rect, state: &CmdlineState, ctx: &Ctx<'_>) {
    let h = height(body.height);
    if h < 3 || body.width < 10 {
        return;
    }
    let area = Rect {
        y: body.y + body.height - h,
        height: h,
        ..body
    };
    frame.render_widget(Clear, area);
    let right = format!("readonly commands · {}", ctx.profile().name);
    let inner = panel(frame, area, "Command line", &right);
    let helper_rows = if inner.height >= HELPER_ROWS + 2 {
        HELPER_ROWS
    } else {
        0
    };
    let out_area = Rect {
        height: inner.height - helper_rows,
        ..inner
    };
    let help_area = Rect {
        y: inner.y + out_area.height,
        height: helper_rows,
        ..inner
    };
    output(frame, out_area, state, ctx.tick);
    let w = usize::from(inner.width).saturating_sub(1);
    let lines = state
        .helper()
        .into_iter()
        .take(usize::from(helper_rows))
        .map(|(text, warn)| {
            let s = if warn { style::amber() } else { style::dim() };
            Line::from(Span::styled(format!(" {}", trunc(&text, w)), s))
        })
        .collect();
    rows(frame, help_area, lines, None);
}

/// The output, its last rows at the bottom, scrolled up by `state.scroll` rows (which
/// is clamped here to what there is).
fn output(frame: &mut Frame, area: Rect, state: &CmdlineState, tick: u64) {
    let height = usize::from(area.height);
    if height == 0 {
        return;
    }
    let w = usize::from(area.width).saturating_sub(1).max(1);
    let want = height + state.scroll.get();
    let mut shown: Vec<Line<'static>> = Vec::new();
    let mut all = true;
    for (i, e) in state.entries.iter().enumerate().rev() {
        let mut r = entry_rows(e, w, tick);
        r.append(&mut shown);
        shown = r;
        if shown.len() >= want && i > 0 {
            // Older entries are not needed for this frame.
            all = false;
            break;
        }
    }
    let scroll = if all {
        state.scroll.get().min(shown.len().saturating_sub(height))
    } else {
        state.scroll.get()
    };
    state.scroll.set(scroll);
    let end = shown.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let mut lines: Vec<Line<'static>> = shown.drain(start..end).collect();
    // Bottom-aligned, like a terminal.
    let pad = height.saturating_sub(lines.len());
    let mut out = vec![Line::default(); pad];
    out.append(&mut lines);
    rows(frame, area, out, None);
}

/// The rows of one entry: its command, then the reply or why it was not sent.
fn entry_rows(e: &Entry, w: usize, tick: u64) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(line) = &e.line {
        let mut first = true;
        for chunk in wrap(line, w.saturating_sub(2)) {
            out.push(Line::from(vec![
                Span::styled(if first { " >" } else { "  " }, style::amber()),
                Span::styled(format!(" {chunk}"), style::fgb()),
            ]));
            first = false;
        }
    }
    let plain = |out: &mut Vec<Line<'static>>, text: &str, s: Style| {
        for chunk in wrap(text, w) {
            out.push(Line::from(Span::styled(format!(" {chunk}"), s)));
        }
    };
    match &e.body {
        Body::Waiting => out.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(super::spinner(tick), style::amber()),
            Span::styled(" running…", style::dim()),
        ])),
        Body::Asking => plain(&mut out, "Waiting for your answer below.", style::dim()),
        Body::Note(why) => plain(&mut out, why, style::amber()),
        Body::Failed(why) => plain(&mut out, &format!("(error) {why}"), style::warn()),
        Body::Out(o) => {
            let s = if o.error { style::warn() } else { style::fg() };
            for l in o.lines() {
                plain(&mut out, l, s);
            }
            plain(&mut out, &format!("({})", took_text(o.took)), style::dim());
        }
    }
    out
}

/// `s` in pieces of at most `w` columns. Always at least one piece.
fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cw = 0;
    for c in s.chars() {
        let x = char_width(c);
        if cw + x > w && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cw = 0;
        }
        cur.push(c);
        cw += x;
    }
    out.push(cur);
    out
}

/// The time a command took, as `0.42 ms`, `12 ms` or `1.30 s`.
pub fn took_text(d: Duration) -> String {
    let us = d.as_micros();
    if us < 10_000 {
        format!("{:.2} ms", us as f64 / 1000.0)
    } else if us < 1_000_000 {
        format!("{} ms", us / 1000)
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_wrapping() {
        assert_eq!(took_text(Duration::from_micros(420)), "0.42 ms");
        assert_eq!(took_text(Duration::from_millis(12)), "12 ms");
        assert_eq!(took_text(Duration::from_millis(1_300)), "1.30 s");
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("", 4), [""]);
        assert_eq!(wrap("界界界", 4), ["界界", "界"]);
        assert_eq!(height(19), 9);
        assert_eq!(height(4), 4);
        assert_eq!(height(60), 16);
    }
}
