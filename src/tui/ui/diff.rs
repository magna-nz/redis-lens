//! View 3: the current scan against the previous snapshot of the same profile, with a
//! trend column over the earlier snapshots when there are enough of them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::diff::{Cause, PatternDiff, ReportDiff};
use crate::format;
use crate::snapshot::{PatternTrend, Trend, TrendMetric, format_utc};
use crate::tui::app::{App, History};

use super::delta::{Change, cause_words, ratio_change, signed_bytes, signed_percent};
use super::widgets::{fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset};
use super::{mark_previous, notice_panels, report_or_wait, spinner, split_body, style};

const NUM_W: usize = 7;
const PCT_W: usize = 6;
/// Cells on each side of the bar's centre line.
const HALF: usize = 12;
const BAR_W: usize = HALF * 2 + 1 + 3;
/// Wide enough for `smaller values`.
const WHY_W: usize = 14;

/// The trend column's width for `trend`, 0 without one: a cell per point, at least as
/// wide as its heading.
fn trend_width(trend: Option<&Trend>) -> usize {
    trend.map_or(0, |t| t.points.len().max("TREND".len()))
}

fn name_width(inner: usize, trend_w: usize) -> usize {
    let trend = if trend_w > 0 { trend_w + 2 } else { 0 };
    let fixed = (NUM_W + 2) * 3 + PCT_W + 2 + trend + BAR_W + WHY_W;
    inner.saturating_sub(fixed).max(12)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some((session, _)) = report_or_wait(frame, area, app, "Diff") else {
        return;
    };
    let line = |text: String, st: Style| Line::from(Span::styled(text, st));
    match &session.history {
        History::Pending => notice_panels(
            frame,
            area,
            "Diff",
            &mark_previous(app, ""),
            vec![Line::from(vec![
                Span::raw(" "),
                Span::styled(spinner(app.tick), style::amber()),
                Span::styled(" Looking for an earlier snapshot…", style::dim()),
            ])],
        ),
        History::NoSnapshot => notice_panels(
            frame,
            area,
            "Diff",
            &mark_previous(app, "no history"),
            vec![
                line(
                    format!(" No earlier snapshot for {}.", session.profile.name),
                    style::fgb(),
                ),
                line(
                    " Press s to save one; the next scan compares against it.".to_string(),
                    style::dim(),
                ),
            ],
        ),
        History::Failed(e) => notice_panels(
            frame,
            area,
            "Diff",
            &mark_previous(app, ""),
            vec![
                line(format!(" ✗ Could not load snapshots: {e}"), style::warn()),
                line(
                    " The other views still work; only the comparison is missing.".to_string(),
                    style::dim(),
                ),
            ],
        ),
        History::Diff(d) => draw_diff(frame, area, app, d),
    }
}

fn header(d: &ReportDiff) -> String {
    let (b, a) = (d.used_memory_before, d.used_memory_after);
    format!(
        "snapshot {} → now · used {} → {} ({})",
        format_utc(d.before_taken_at_unix),
        format::bytes(b),
        format::bytes(a),
        signed_bytes(crate::diff::sub(a, b))
    )
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &App, d: &ReportDiff) {
    if d.patterns.is_empty() {
        notice_panels(
            frame,
            area,
            "Diff",
            &mark_previous(app, &header(d)),
            vec![Line::from(Span::styled(
                " Neither scan sampled any keys.",
                style::dim(),
            ))],
        );
        return;
    }
    let (main, detail) = split_body(area);
    let sel = app.sel.diff.min(d.patterns.len() - 1);
    let inner = panel(frame, main, "Diff", &mark_previous(app, &header(d)));
    let trend = app.trend();
    let trend_w = trend_width(trend);
    let name = name_width(usize::from(inner.width), trend_w);
    let mut head = vec![
        Span::styled(pad_r(" PATTERN", name), style::hd()),
        Span::styled(format!("{}  ", pad_l("BEFORE", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("NOW", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("Δ", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("Δ%", PCT_W)), style::hd()),
    ];
    if trend.is_some() {
        head.push(Span::styled(
            format!("{}  ", pad_r("TREND", trend_w)),
            style::hd(),
        ));
    }
    head.extend([
        Span::styled(
            pad_r(&format!("{} grew", pad_l("shrank", HALF)), BAR_W),
            style::hd(),
        ),
        Span::styled("WHY", style::hd()),
    ]);
    let head = Line::from(head);
    rows(frame, inner, vec![head], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let max = d
        .patterns
        .iter()
        .map(|p| Change::of(p).value.unsigned_abs())
        .max()
        .unwrap_or(0);
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = d
        .patterns
        .iter()
        .skip(off)
        .take(usize::from(body.height))
        .enumerate()
        .map(|(i, p)| {
            let spark = trend.map(|t| (sparkline_for(t, &p.pattern), trend_w));
            diff_row(
                p,
                max,
                name,
                spark,
                (off + i == sel).then(|| app.sel_ticks()),
            )
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
    detail_panel(frame, detail, &d.patterns[sel], trend);
}

/// The pattern's sparkline over the trend's points, all gaps when it is in none of
/// them (only possible for a pattern gone from the current scan and every snapshot).
fn sparkline_for(t: &Trend, pattern: &str) -> String {
    match t.pattern(pattern) {
        Some(p) => format::sparkline(&p.values),
        None => format::sparkline(&vec![None; t.points.len()]),
    }
}

/// Formats a byte or key figure.
type Fmt = fn(u64) -> String;

/// Before and after as text: bytes when both scans had sizes, keys otherwise.
fn sides(p: &PatternDiff) -> (Option<u64>, Option<u64>, Fmt) {
    if p.delta_bytes.is_some() {
        (p.bytes_before, p.bytes_after, format::bytes)
    } else {
        (p.keys_before, p.keys_after, format::count)
    }
}

/// The relative change, `new` without a before, or `—` when before was empty.
fn percent_text(p: &PatternDiff, before: Option<u64>, after: Option<u64>) -> String {
    if p.cause == Cause::Unknown {
        return "—".to_string();
    }
    if p.cause == Cause::New || before.is_none() {
        return "new".to_string();
    }
    ratio_change(before, Some(after.unwrap_or(0))).map_or_else(|| "—".to_string(), signed_percent)
}

fn grew_style(c: Change) -> Style {
    if c.is_alarming() {
        style::warn()
    } else {
        style::amber()
    }
}

/// One row. `spark` is the trend cell and the column's width, when there is a trend.
fn diff_row(
    p: &PatternDiff,
    max: u64,
    name: usize,
    spark: Option<(String, usize)>,
    scroll: Option<u64>,
) -> Line<'static> {
    let dash = || "—".to_string();
    let c = Change::of(p);
    let (before, after, fmt) = sides(p);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(pattern_spans(&p.pattern, style::fg(), style::ph()));
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(spans, name.saturating_sub(1), ticks),
        None => fit(spans, name.saturating_sub(1)),
    };
    spans.push(Span::raw(" "));
    let len = if c.unchanged || c.value == 0 || max == 0 {
        0
    } else {
        ((c.value.unsigned_abs() as f64 / max as f64 * HALF as f64).round() as usize).clamp(1, HALF)
    };
    let (neg, pos) = if c.value < 0 { (len, 0) } else { (0, len) };
    let why_style = if p.cause == Cause::New {
        style::warn()
    } else {
        style::dim()
    };
    spans.extend([
        Span::styled(
            format!("{}  ", pad_l(&before.map_or_else(dash, fmt), NUM_W)),
            style::dim(),
        ),
        Span::styled(
            format!("{}  ", pad_l(&after.map_or_else(dash, fmt), NUM_W)),
            style::fg(),
        ),
        Span::styled(format!("{}  ", pad_l(&c.text(), NUM_W)), c.style()),
        Span::styled(
            format!("{}  ", pad_l(&percent_text(p, before, after), PCT_W)),
            c.style(),
        ),
    ]);
    if let Some((cells, w)) = spark {
        spans.push(Span::styled(format!("{}  ", pad_r(&cells, w)), style::fg()));
    }
    spans.extend([
        Span::raw(" ".repeat(HALF - neg)),
        Span::styled("█".repeat(neg), style::ok()),
        Span::styled("│", style::border()),
        Span::styled("█".repeat(pos), grew_style(c)),
        Span::raw(" ".repeat(HALF - pos + 3)),
        Span::styled(cause_words(p.cause), why_style),
    ]);
    Line::from(spans)
}

fn avg(bytes: Option<u64>, keys: Option<u64>) -> Option<u64> {
    match (bytes, keys) {
        (Some(b), Some(k)) if k > 0 => Some(b / k),
        _ => None,
    }
}

/// One `label  before  now  change` row of the detail panel.
fn compare_line(
    p: &PatternDiff,
    label: &str,
    before: Option<u64>,
    after: Option<u64>,
    fmt: fn(u64) -> String,
    available: bool,
) -> Line<'static> {
    let dash = || "—".to_string();
    let (change, st) = if !available {
        (dash(), style::dim())
    } else if p.cause == Cause::New {
        ("new".to_string(), style::warn())
    } else if p.cause == Cause::Gone {
        ("gone".to_string(), style::ok())
    } else {
        match ratio_change(before, after) {
            Some(r) if r > 0.05 => (signed_percent(r), style::amber()),
            Some(r) if r < -0.05 => (signed_percent(r), style::ok()),
            Some(r) => (signed_percent(r), style::dim()),
            None => (dash(), style::dim()),
        }
    };
    Line::from(vec![
        Span::styled(pad_r(&format!(" {label}"), 16), style::dim()),
        Span::styled(pad_l(&before.map_or_else(dash, fmt), 10), style::fg()),
        Span::styled(pad_l(&after.map_or_else(dash, fmt), 10), style::fgb()),
        Span::styled(pad_l(&change, 10), st),
    ])
}

/// A sentence about why the pattern changed.
fn cause_sentence(p: &PatternDiff) -> (&'static str, Style) {
    let c = Change::of(p);
    let text = match p.cause {
        Cause::New => "▲ New since the snapshot: nothing matched this pattern then.",
        Cause::Gone => "Gone: no sampled key matches this pattern any more.",
        Cause::MoreKeys => "The change comes mostly from more keys; the average size moved less.",
        Cause::FewerKeys => "The change comes mostly from fewer keys; the average size moved less.",
        Cause::BiggerValues => {
            "The change comes mostly from bigger values; the key count moved less."
        }
        Cause::SmallerValues => {
            "The change comes mostly from smaller values; the key count moved less."
        }
        Cause::Unchanged => "No meaningful change.",
        Cause::Unknown => {
            "Not in the other scan, which used a different MATCH, so it may not be new or gone."
        }
    };
    let st = match p.cause {
        Cause::Gone => style::ok(),
        Cause::Unchanged | Cause::Unknown => style::dim(),
        _ => c.style(),
    };
    (text, st)
}

/// Growth of at least this share a day, either way, is coloured in the trend line.
const NOTABLE_GROWTH: f64 = 0.005;

/// The detail panel's trend line: the sparkline and the least-squares growth a day.
fn trend_line(t: &Trend, pattern: &str) -> Line<'static> {
    let fmt: fn(u64) -> String = match t.metric {
        TrendMetric::Bytes => format::bytes,
        TrendMetric::Keys => format::count,
    };
    let scans = t.points.len();
    let partial = t.partial_points();
    let over = match partial {
        0 => format!("over {scans} scans"),
        n if n == scans => format!("over {scans} scans, all partial"),
        n => format!("over {scans} scans, {n} partial"),
    };
    let pt: Option<&PatternTrend> = t.pattern(pattern);
    let (text, st) = match pt.and_then(|p| p.growth) {
        Some(g) => {
            let abs = format::signed_with(g.per_day, fmt);
            let pct = g.ratio_per_day.map(format::signed_percent1);
            let text = match pct {
                Some(p) if p == "0.0%" => format!("flat {over}"),
                Some(p) => format!("{p} a day ({abs} a day) {over}"),
                None => format!("{abs} a day {over}"),
            };
            let st = match g.ratio_per_day {
                Some(r) if r >= NOTABLE_GROWTH => style::amber(),
                Some(r) if r <= -NOTABLE_GROWTH => style::ok(),
                _ => style::dim(),
            };
            (text, st)
        }
        None => (
            format!("too few scans with this pattern for a growth rate ({over})"),
            style::dim(),
        ),
    };
    Line::from(vec![
        Span::styled(pad_r(" trend", 16), style::dim()),
        Span::styled(format!("{}  ", sparkline_for(t, pattern)), style::fg()),
        Span::styled(text, st),
    ])
}

fn detail_panel(frame: &mut Frame, area: Rect, p: &PatternDiff, trend: Option<&Trend>) {
    let c = Change::of(p);
    let right = format!("{} since the snapshot", c.text());
    let inner = panel(frame, area, &p.pattern, &right);
    let sizes = p.delta_bytes.is_some();
    let (sentence, st) = cause_sentence(p);
    let mut lines = vec![
        Line::from(Span::styled(" WHERE THE CHANGE CAME FROM", style::hd())),
        Line::from(vec![
            Span::raw(" ".repeat(16)),
            Span::styled(pad_l("BEFORE", 10), style::hd()),
            Span::styled(pad_l("NOW", 10), style::hd()),
            Span::styled(pad_l("CHANGE", 10), style::hd()),
        ]),
        compare_line(p, "keys", p.keys_before, p.keys_after, format::count, true),
        compare_line(
            p,
            "avg size",
            avg(p.bytes_before, p.keys_before),
            avg(p.bytes_after, p.keys_after),
            format::bytes,
            sizes,
        ),
        compare_line(
            p,
            "memory",
            p.bytes_before,
            p.bytes_after,
            format::bytes,
            sizes,
        ),
    ];
    let spacer = if let Some(t) = trend {
        lines.push(Line::from(fit(
            trend_line(t, &p.pattern).spans,
            usize::from(inner.width),
        )));
        Some(lines.len())
    } else {
        None
    };
    lines.extend([
        Line::default(),
        Line::from(fit(
            vec![Span::styled(format!(" {sentence}"), st)],
            usize::from(inner.width),
        )),
    ]);
    if !sizes {
        lines.push(Line::from(Span::styled(
            " Sizes unavailable: one of the scans could not run MEMORY USAGE.",
            style::dim(),
        )));
    }
    // The trend line takes a row: give up the blank one before the sentence rather than
    // the last line, when the panel is too short for both.
    if let Some(i) = spacer
        && lines.len() > usize::from(inner.height)
    {
        lines.remove(i);
    }
    rows(frame, inner, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_survives_huge_memory() {
        let mut d = crate::tui::fixtures::history(true);
        d.used_memory_before = 0;
        d.used_memory_after = u64::MAX;
        assert!(header(&d).contains("(+"));
        d.used_memory_before = 1 << 63;
        d.used_memory_after = 0;
        assert!(header(&d).contains("(-"));
    }
}
