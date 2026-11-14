//! The bulk action's dialog ([`crate::tui::app::bulk`]): the TTL input, the dry run's
//! progress, the run's progress and how it ended, centred over Patterns. The question
//! between the dry run and the run is the confirm dialog's ([`super::confirm`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::analysis::wrap_hard;
use super::style;
use super::widgets::{bar, dialog, rows, trunc, width};
use crate::format::grouped;
use crate::tui::app::bulk::{Bulk, Stage};
use crate::write::BulkAction;

/// Widest the dialog gets.
const MAX_W: u16 = 76;

/// Least wide, so a short message is not a sliver.
const MIN_W: u16 = 40;

fn text(lines: &mut Vec<Line<'static>>, t: &str, s: ratatui::style::Style, w: usize) {
    for part in wrap_hard(t, w) {
        lines.push(Line::from(Span::styled(format!(" {part}"), s)));
    }
}

/// The progress bar line: `███░░░  1,000 / 2,345`.
fn progress(done: u64, total: u64, w: usize) -> Line<'static> {
    let label = format!("  {} / {}", grouped(done), grouped(total));
    let bw = w.saturating_sub(width(&label) + 1).min(40);
    let ratio = if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    };
    let (fill, track) = bar(ratio, bw);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(fill, style::ok()),
        Span::styled(track, style::track()),
        Span::styled(label, style::fgb()),
    ])
}

/// Draws `b` centred in `area`. Nothing while the confirm dialog asks.
pub fn draw(frame: &mut Frame, area: Rect, b: &Bulk, tick: u64) {
    if matches!(b.stage, Stage::Asking { .. }) {
        return;
    }
    let wide = area.width.min(MAX_W);
    let inner = usize::from(wide.saturating_sub(4));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let pattern = format!("Pattern {}", crate::value::escape(&b.pattern));
    text(&mut lines, &pattern, style::fgb(), inner);
    match &b.stage {
        Stage::Ttl { input, error } => {
            text(
                &mut lines,
                "Sets this TTL on each key of the pattern that has none (EXPIRE … NX). \
                 A dry run counts the keys first; nothing is written before you confirm.",
                style::fg(),
                inner,
            );
            lines.push(Line::from(""));
            text(&mut lines, "TTL (90s, 10m, 24h, 7d):", style::fg(), inner);
            let shown = trunc(&crate::value::escape(input), inner.saturating_sub(3));
            lines.push(Line::from(vec![
                Span::styled(" › ", style::amber()),
                Span::styled(shown, style::fgb()),
                Span::styled("▏", style::amber()),
            ]));
            if let Some(e) = error {
                text(&mut lines, e, style::warn(), inner);
            }
        }
        Stage::Counting { count, .. } => {
            text(
                &mut lines,
                &format!(
                    "{} Dry run: looking for {}",
                    super::spinner(tick),
                    crate::value::escape(&b.glob)
                ),
                style::fg(),
                inner,
            );
            text(
                &mut lines,
                &format!(
                    "Found {} keys of the pattern so far, {} with a TTL.",
                    grouped(count.matched),
                    grouped(count.with_ttl)
                ),
                style::fg(),
                inner,
            );
            text(&mut lines, "Nothing is written.", style::dim(), inner);
        }
        Stage::Asking { .. } => {}
        Stage::Running(r) => {
            let what = match r.action {
                BulkAction::Delete => "Deleting with UNLINK".to_string(),
                BulkAction::Expire { seconds } => format!(
                    "Setting a TTL of {} with EXPIRE … NX",
                    crate::live::bulk::ttl_text(seconds)
                ),
            };
            text(
                &mut lines,
                &format!("{} {what}, 500 keys a batch", super::spinner(tick)),
                style::fg(),
                inner,
            );
            lines.push(progress(r.seen, r.total.max(r.seen), inner));
            text(
                &mut lines,
                &format!(
                    "changed {} · skipped {}{}",
                    grouped(r.changed),
                    grouped(r.skipped),
                    if r.failed > 0 {
                        format!(" · failed {}", grouped(r.failed))
                    } else {
                        String::new()
                    }
                ),
                style::fg(),
                inner,
            );
            if let Some(f) = &r.file {
                text(
                    &mut lines,
                    &format!(
                        "Key names: {}",
                        crate::value::escape(&f.display().to_string())
                    ),
                    style::dim(),
                    inner,
                );
            }
            if r.stopping {
                text(
                    &mut lines,
                    "Stopping after this batch…",
                    style::amber(),
                    inner,
                );
            }
        }
        Stage::Done(d) => {
            for (i, l) in d.lines.iter().enumerate() {
                let s = if i == 0 && d.error {
                    style::warn()
                } else if i == 0 {
                    style::ok()
                } else {
                    style::fg()
                };
                text(&mut lines, l, s, inner);
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(key_line(b));

    let right = format!(
        "{}{}",
        crate::value::escape(&b.profile),
        if b.prod { " · PROD" } else { "" }
    );
    // Sized to its text: the longest line, or the title and profile, with the border.
    let longest = lines.iter().map(Line::width).max().unwrap_or(0);
    let head = width(b.title()) + width(&right) + 7;
    let w = u16::try_from(longest.max(head) + 3)
        .unwrap_or(MAX_W)
        .clamp(MIN_W, MAX_W)
        .min(area.width);
    let h = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    // The first dialog is a question: it asks for the TTL.
    let named = if matches!(b.stage, Stage::Ttl { .. }) {
        format!("{}?", b.title())
    } else {
        b.title().to_string()
    };
    let title = trunc(&named, usize::from(w).saturating_sub(width(&right) + 7));
    let body = dialog(frame, area, w, h, &title, &right);
    rows(frame, body, lines, None);
}

fn key_line(b: &Bulk) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, l) in b.keys() {
        spans.push(Span::styled(k, style::key()));
        spans.push(Span::styled(format!(" {l}   "), style::dim()));
    }
    Line::from(spans)
}

/// The key bar while the dialog is open.
pub fn footer(b: &Bulk) -> Line<'static> {
    key_line(b)
}
