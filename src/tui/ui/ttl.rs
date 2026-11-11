//! View 2: patterns ranked by TTL risk, each with a TTL histogram and a verdict.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::diff::ReportDiff;
use crate::format;
use crate::model::{PatternStats, TTL_BUCKET_LABELS};
use crate::tui::app::App;
use crate::tui::clipboard::{NO_COMMAND, suggested_command};
use crate::verdict::{TtlVerdict, explain, ttl_verdict};

use super::widgets::ttl_hist_spans;
use super::widgets::{
    fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset, trunc,
};
use super::{mark_previous, notice_panels, report_or_wait, split_body, style};

const NUM_W: usize = 7;
const TTL_W: usize = 6;
const HIST_W: usize = 5 * TTL_BUCKET_LABELS.len();
/// The longest label, `persistent · growing`, plus a space before the border.
const VERDICT_W: usize = 21;

fn name_width(inner: usize) -> usize {
    let fixed = NUM_W + 2 + TTL_W + 3 + HIST_W + VERDICT_W;
    inner.saturating_sub(fixed).max(12)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some((session, report)) = report_or_wait(frame, area, app, "TTL") else {
        return;
    };
    let order = app.ttl_rows();
    if order.is_empty() {
        notice_panels(
            frame,
            area,
            "TTL",
            &mark_previous(app, ""),
            vec![Line::from(Span::styled(
                " No keys were sampled.",
                style::dim(),
            ))],
        );
        return;
    }
    let diff = session.diff();
    let (main, detail) = split_body(area);
    let sel = app.sel.ttl.min(order.len() - 1);
    let rows_: Vec<(&PatternStats, TtlVerdict)> = order
        .iter()
        .map(|&i| {
            let p = &report.patterns[i];
            (p, ttl_verdict(p, change(diff, p)))
        })
        .collect();

    let right = mark_previous(app, &format!("{} patterns · riskiest first", rows_.len()));
    let inner = panel(frame, main, "TTL", &right);
    let name = name_width(usize::from(inner.width));
    let head = Line::from(vec![
        Span::styled(pad_r(" PATTERN", name), style::hd()),
        Span::styled(format!("{}  ", pad_l("KEYS", NUM_W)), style::hd()),
        Span::styled(format!("{}   ", pad_l("NO TTL", TTL_W)), style::hd()),
        Span::styled(
            TTL_BUCKET_LABELS
                .iter()
                .map(|l| pad_r(l, 5))
                .collect::<String>(),
            style::hd(),
        ),
        Span::styled("VERDICT", style::hd()),
    ]);
    rows(frame, inner, vec![head], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = rows_
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .enumerate()
        .map(|(i, (p, v))| ttl_row(p, *v, name, (off + i == sel).then(|| app.sel_ticks())))
        .collect();
    rows(frame, body, lines, Some(sel - off));

    let (p, v) = rows_[sel];
    detail_panel(frame, detail, p, v, change(diff, p));
}

fn change<'a>(
    diff: Option<&'a ReportDiff>,
    p: &PatternStats,
) -> Option<&'a crate::diff::PatternDiff> {
    diff.and_then(|d| d.pattern(&p.pattern))
}

fn verdict_style(v: TtlVerdict) -> ratatui::style::Style {
    if v.is_warning() {
        style::warn()
    } else {
        style::dim()
    }
}

fn ttl_row(p: &PatternStats, v: TtlVerdict, name: usize, scroll: Option<u64>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(pattern_spans(&p.pattern, style::fg(), style::ph()));
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(spans, name.saturating_sub(1), ticks),
        None => fit(spans, name.saturating_sub(1)),
    };
    spans.push(Span::raw(" "));
    let pct = format::percent(p.no_ttl_ratio());
    let (no_ttl, no_ttl_style) = if v.is_warning() {
        (format!("▲ {pct}"), style::warn())
    } else {
        (pct, style::dim())
    };
    spans.extend([
        Span::styled(
            format!("{}  ", pad_l(&format::count(p.est_keys), NUM_W)),
            style::fg(),
        ),
        Span::styled(format!("{}   ", pad_l(&no_ttl, TTL_W)), no_ttl_style),
    ]);
    let none = if v.is_warning() {
        style::warn()
    } else {
        style::amber()
    };
    spans.extend(ttl_hist_spans(&p.ttl_hist, none));
    spans.push(Span::styled(v.label(), verdict_style(v)));
    Line::from(spans)
}

/// The most common TTL bucket among keys that expire, as a range.
fn typical_ttl(p: &PatternStats) -> String {
    const RANGES: [&str; 6] = [
        "under 1m",
        "1m to 1h",
        "1h to 1d",
        "1d to 7d",
        "7d to 30d",
        "over 30d",
    ];
    p.ttl_hist[1..]
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .max_by_key(|(i, n)| (**n, std::cmp::Reverse(*i)))
        .map_or_else(|| "none expire".to_string(), |(i, _)| RANGES[i].to_string())
}

fn detail_panel(
    frame: &mut Frame,
    area: Rect,
    p: &PatternStats,
    v: TtlVerdict,
    change: Option<&crate::diff::PatternDiff>,
) {
    let pct = format::percent(p.no_ttl_ratio());
    let right = format!("{} keys · {pct} without TTL", format::count(p.est_keys));
    let inner = panel(frame, area, &p.pattern, &right);
    let w = usize::from(inner.width);
    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(" WHY", style::hd()))];
    for text in explain(p, change).into_iter().take(4) {
        lines.push(Line::from(fit(
            vec![Span::styled(format!(" {text}"), style::fg())],
            w,
        )));
    }
    while lines.len() < 5 {
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![
        Span::styled(" NO TTL ", style::dim()),
        Span::styled(pct, style::fgb()),
        Span::styled("    TYPICAL TTL ", style::dim()),
        Span::styled(typical_ttl(p), style::fgb()),
        Span::styled("    VERDICT ", style::dim()),
        Span::styled(v.label(), verdict_style(v)),
    ]));
    lines.push(Line::default());
    // The same text `Y` copies.
    // Without a command there is no `$ ` prompt, so copying the line with the mouse
    // gives nothing that looks like a command.
    let (prompt, cmd, cmd_style) = match suggested_command(&p.pattern) {
        Some(c) => (" $ ", c, style::cmd()),
        None => ("   ", NO_COMMAND.to_string(), style::dim()),
    };
    lines.push(Line::from(vec![
        Span::styled(prompt, style::dim()),
        Span::styled(trunc(&cmd, w.saturating_sub(3)), cmd_style),
    ]));
    rows(frame, inner, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_ttl_is_most_common_expiring_bucket() {
        let mut p = crate::tui::fixtures::report(true).patterns[0].clone();
        p.ttl_hist = [50, 0, 3, 9, 9, 0, 0];
        // Ties go to the shorter bucket.
        assert_eq!(typical_ttl(&p), "1h to 1d");
        p.ttl_hist = [10, 0, 0, 0, 0, 0, 0];
        assert_eq!(typical_ttl(&p), "none expire");
    }
}
