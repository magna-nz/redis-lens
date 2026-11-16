//! Draws the command line panel ([`CmdlineState`]) at the bottom of the body, over the
//! view. Its input line is drawn in place of the key bar by `ui::footer`.
//!
//! Everything drawn here is safe already: commands are masked lines
//! ([`crate::live::command::masked_line`]), replies were escaped by
//! [`crate::live::cmdline::format_reply`], notes are redis-lens's own words.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear};

use super::style;
use super::widgets::{char_width, rows, trunc, width};
use crate::live::cmdline::View;
use crate::tui::app::views::Ctx;
use crate::tui::app::views::cmdline::{
    Body, CmdlineState, Entry, Tone, dropped_text, numbered, runs_end, took_text,
};

/// The command line's own question, while a `@dangerous` command waits for it: the one
/// confirm sheet ([`super::sheet`]). The command is the masked line the panel shows
/// ([`crate::live::command::masked_line`]): an `AUTH` password never shows.
pub fn confirm_sheet(state: &CmdlineState, ctx: &Ctx<'_>) -> Option<super::sheet::Sheet> {
    use super::sheet::{Answer, Sheet, right_of};
    let c = state.confirm.as_ref()?;
    let times = match c.times {
        0 | 1 => String::new(),
        n => format!("  ({n} times)"),
    };
    let mut lines = vec![
        (
            "",
            format!("{} is @dangerous and asks first.", c.command),
            style::amber(),
        ),
        ("Sends", format!("{}{times}", c.line), style::fgb()),
        (
            "",
            "A command typed here can't be undone.".to_string(),
            style::fg(),
        ),
    ];
    if let Some(s) = &state.status {
        lines.push(("", s.clone(), style::warn()));
    }
    Some(Sheet {
        title: "Run this command?".into(),
        right: right_of(&c.profile, ctx.profile().env),
        lines,
        answer: match &c.typed {
            Some(t) => Answer::Type {
                name: c.profile.clone(),
                typed: t.clone(),
                verb: "Send",
            },
            None => Answer::YesNo("Send"),
        },
        wait: false,
    })
}

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
    // What the line does here, in words: the profile, then whether writes ask.
    let access = if ctx.profile().is_read_only() {
        "read-only: readonly commands"
    } else {
        "writes ask as usual"
    };
    // The view and pipeline mode, when not the defaults (text, off), before the access
    // text, which a narrow title cuts first.
    let mut right = ctx.profile().name.clone();
    if state.view != View::Text {
        right.push_str(&format!(" · {} view", state.view.label()));
    }
    if state.pipeline {
        right.push_str(" · pipeline");
    }
    right.push_str(&format!(" · {access}"));
    let inner = focus_panel(frame, area, ": Command line", &right);
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

/// A bordered panel like [`super::widgets::panel`] with its border in the focus colour,
/// so it is clear that the keys now type text. The `:` in the title says so without the
/// colour.
fn focus_panel(frame: &mut Frame, area: Rect, title: &str, right: &str) -> Rect {
    let avail = usize::from(area.width).saturating_sub(4);
    let title = trunc(title, avail.saturating_sub(4).max(1));
    let room = avail.saturating_sub(width(&title) + 4);
    let mut block = Block::bordered()
        .border_style(style::focus())
        .title_top(Line::from(vec![
            Span::styled("─ ", style::focus()),
            Span::styled(title, style::title()),
            Span::styled(" ", style::focus()),
        ]));
    if !right.is_empty() && room > 4 {
        block = block.title_top(
            Line::from(vec![
                Span::styled(" ", style::focus()),
                Span::styled(trunc(right, room - 3), style::dim()),
                Span::styled(" ─", style::focus()),
            ])
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
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
        let mut r = entry_rows(e, w, tick, state.view);
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
fn entry_rows(e: &Entry, w: usize, tick: u64, view: View) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(line) = &e.line {
        let mut first = true;
        for chunk in wrap(line, w.saturating_sub(2)) {
            out.push(Line::from(vec![
                Span::styled(if first { " ›" } else { "  " }, style::focus()),
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
        // A command that was not sent says so with a glyph, not only with colour.
        Body::Note(why) if e.line.is_some() => {
            plain(&mut out, &format!("✗ {why}"), style::amber());
        }
        Body::Note(why) => plain(&mut out, why, style::amber()),
        Body::Failed(why) => plain(&mut out, &format!("✗ (error) {why}"), style::warn()),
        Body::Out(o) => {
            // On a cluster: the node it ran on, then why it went there if that needs
            // saying (spec "Cluster live views (P4)", Command line).
            if let Some(node) = &e.node {
                plain(&mut out, &format!("{node}>"), style::dim());
            }
            if let Some(why) = e.route_note {
                plain(&mut out, why, style::amber());
            }
            let s = if o.error { style::warn() } else { style::fg() };
            for l in o.lines_in(view, e.pairs) {
                plain(&mut out, l, s);
            }
            plain(&mut out, &format!("({})", took_text(o.took)), style::dim());
        }
        Body::Runs(r) => {
            if let Some(node) = &e.node {
                plain(&mut out, &format!("{node}>"), style::dim());
            }
            if let Some(why) = e.route_note {
                plain(&mut out, why, style::amber());
            }
            if r.dropped > 0 {
                plain(&mut out, &dropped_text(r), style::dim());
            }
            for (line, error) in numbered(r, view, e.pairs) {
                let s = if error { style::warn() } else { style::fg() };
                plain(&mut out, &line, s);
            }
            let (text, tone) = runs_end(r);
            match tone {
                Tone::Running => out.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(super::spinner(tick), style::amber()),
                    Span::styled(format!(" {text}"), style::dim()),
                ])),
                Tone::Done => plain(&mut out, &text, style::dim()),
                Tone::Stopped => plain(&mut out, &text, style::amber()),
                Tone::Failed => plain(&mut out, &text, style::warn()),
            }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
