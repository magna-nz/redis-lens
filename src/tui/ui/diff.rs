//! Analyze › Changes: the current analysis against the previous snapshot of the same
//! profile, with a trend over the earlier snapshots when there are enough of them.
//! The patterns that changed on the left, what changed in the selected one on the right.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::diff::{Cause, PatternDiff, ReportDiff};
use crate::format;
use crate::snapshot::{PatternTrend, Trend, TrendMetric, format_utc};
use crate::tui::app::{App, History};

use super::analysis::{
    Col, MARK, Tier, head_row, left_width, need_report, notice, paragraph, pick_cols, title_right,
};
use super::delta::{Change, cause_words, ratio_change, signed_bytes, signed_percent};
use super::widgets::{
    Pane, Title, fit, marquee_spans, pad_l, pad_r, pattern_spans, rows, scroll_offset, split_frame,
};
use super::{mark_previous, spinner, style};

const NUM_W: usize = 7;
const PCT_W: usize = 6;
/// Cells on each side of the bar's centre line.
const HALF: usize = 12;
const BAR_W: usize = HALF * 2 + 1;
/// Wide enough for `smaller values`.
const WHY_W: usize = 14;

/// The list pane shows the Wide columns from this many cells.
const WIDE_AT: usize = 56;
/// The detail pane uses the roomy table from this many cells.
const DETAIL_WIDE: usize = 46;

const MARK_COL: usize = 1;
const DELTA: usize = 2;
const PCT: usize = 3;
const NOW: usize = 4;
const WHY: usize = 5;
const BEFORE: usize = 6;
const TREND: usize = 7;
const BAR: usize = 8;

/// The columns; the trend's width depends on the snapshots.
fn columns(trend_w: usize) -> [Col; 9] {
    [
        Col::new("PATTERN", 14, Tier::Base),
        Col::new("", 1, Tier::Base),
        Col::new("Δ", NUM_W, Tier::Base),
        Col::new("Δ%", PCT_W, Tier::Base),
        Col::new("NOW", NUM_W - 1, Tier::Wide),
        Col::new("WHY", WHY_W, Tier::Wide),
        Col::new("BEFORE", NUM_W, Tier::More),
        // No trend: a column of width 0 is never picked.
        // No trend: a column of width 0 is never picked.
        Col::new("TREND", trend_w, Tier::Wide),
        Col::new("", BAR_W, Tier::More),
    ]
}

/// The trend column's width for `trend`, 0 without one: a cell per point, at least as
/// wide as its heading.
fn trend_width(trend: Option<&Trend>) -> usize {
    trend.map_or(0, |t| t.points.len().max("TREND".len()))
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if need_report(
        frame,
        area,
        app,
        "Changes",
        "compares this analysis with the last snapshot.",
    ) {
        return;
    }
    let Some(session) = app.session.as_ref() else {
        return;
    };
    let line = |text: String, st: Style| Line::from(Span::styled(text, st));
    match &session.history {
        History::Pending => notice(
            frame,
            area,
            "Changes",
            &mark_previous(app, ""),
            vec![Line::from(vec![
                Span::raw(" "),
                Span::styled(spinner(app.tick), style::amber()),
                Span::styled(" Looking for an earlier snapshot…", style::dim()),
            ])],
        ),
        History::NoSnapshot => notice(
            frame,
            area,
            "Changes",
            &mark_previous(app, "no history"),
            vec![
                line(
                    format!(" No earlier snapshot for {}.", session.profile.name),
                    style::fgb(),
                ),
                line(
                    " Press s to save one; the next analysis compares against it.".to_string(),
                    style::dim(),
                ),
            ],
        ),
        History::Failed(e) => notice(
            frame,
            area,
            "Changes",
            &mark_previous(app, ""),
            vec![
                line(format!(" ✗ Could not load snapshots: {e}"), style::warn()),
                line(
                    " The other panels still work; only the comparison is missing.".to_string(),
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
        notice(
            frame,
            area,
            "Changes",
            &mark_previous(app, &header(d)),
            vec![Line::from(Span::styled(
                " Neither analysis sampled any keys.",
                style::dim(),
            ))],
        );
        return;
    }
    let sel = app.sel.diff.min(d.patterns.len() - 1);
    let p = &d.patterns[sel];
    let c = Change::of(p);
    let more = app.more_here();
    let right = title_right(
        app,
        area.width,
        &format!("since {}", format_utc(d.before_taken_at_unix)),
    );
    let kind = format!("{} since the snapshot", c.text());
    let (left, detail) = split_frame(
        frame,
        area,
        left_width(area.width),
        Title::new("Changes", &right),
        Title::new(&p.pattern, &kind),
        Pane::Left,
    );
    let trend = app.trend();
    let trend_w = trend_width(trend);
    let cols = columns(trend_w);
    let (keep, name) = pick_cols(&cols, usize::from(left.width), WIDE_AT, more);
    // A pattern's bar and trend are for `v`; without a trend there is no such column.
    let keep: Vec<usize> = keep
        .into_iter()
        .filter(|&i| i != TREND || trend_w > 0)
        .collect();
    rows(
        frame,
        left,
        vec![head_row(&cols, &keep, name, &[DELTA, PCT, NOW, BEFORE])],
        None,
    );
    let body = Rect {
        y: left.y + 1,
        height: left.height.saturating_sub(1),
        ..left
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
            let scroll = (off + i == sel).then(|| app.sel_ticks());
            diff_row(&keep, &cols, p, max, name, spark, scroll)
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
    detail_lines(frame, detail, p, trend, more.then(|| header(d)));
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

/// Whether the row gets a `▲`: a new pattern, or growth of at least 300M.
fn worth_a_look(c: Change) -> bool {
    c.is_alarming()
}

/// One row. `spark` is the trend cell and the column's width, when there is a trend.
fn diff_row(
    keep: &[usize],
    cols: &[Col],
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
        Some(ticks) => marquee_spans(spans, name, ticks),
        None => fit(spans, name),
    };
    let why_style = if p.cause == Cause::New {
        style::warn()
    } else {
        style::dim()
    };
    for &i in &keep[1..] {
        spans.push(Span::raw(" "));
        let w = cols[i].w;
        match i {
            MARK_COL => spans.push(Span::styled(
                if worth_a_look(c) { MARK } else { " " },
                c.style(),
            )),
            DELTA => spans.push(Span::styled(pad_l(&c.text(), w), c.style())),
            PCT => spans.push(Span::styled(
                pad_l(&percent_text(p, before, after), w),
                c.style(),
            )),
            NOW => spans.push(Span::styled(
                pad_l(&after.map_or_else(dash, fmt), w),
                style::fg(),
            )),
            WHY => spans.push(Span::styled(pad_r(cause_words(p.cause), w), why_style)),
            BEFORE => spans.push(Span::styled(
                pad_l(&before.map_or_else(dash, fmt), w),
                style::dim(),
            )),
            TREND => {
                let cells = spark.as_ref().map_or_else(String::new, |s| s.0.clone());
                spans.push(Span::styled(pad_r(&cells, w), style::fg()));
            }
            BAR => {
                let len = if c.unchanged || c.value == 0 || max == 0 {
                    0
                } else {
                    ((c.value.unsigned_abs() as f64 / max as f64 * HALF as f64).round() as usize)
                        .clamp(1, HALF)
                };
                let (neg, pos) = if c.value < 0 { (len, 0) } else { (0, len) };
                spans.extend([
                    Span::raw(" ".repeat(HALF - neg)),
                    Span::styled("█".repeat(neg), style::ok()),
                    Span::styled("│", style::border()),
                    Span::styled("█".repeat(pos), grew_style(c)),
                    Span::raw(" ".repeat(HALF - pos)),
                ]);
            }
            _ => {}
        }
    }
    Line::from(spans)
}

fn avg(bytes: Option<u64>, keys: Option<u64>) -> Option<u64> {
    match (bytes, keys) {
        (Some(b), Some(k)) if k > 0 => Some(b / k),
        _ => None,
    }
}

/// One `label  before  now  change` row of the detail pane: `lw` cells for the label,
/// `cw` for each figure.
fn compare_line(
    p: &PatternDiff,
    label: &str,
    figures: (Option<u64>, Option<u64>),
    fmt: fn(u64) -> String,
    available: bool,
    (lw, cw): (usize, usize),
) -> Line<'static> {
    let (before, after) = figures;
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
        Span::styled(pad_r(&format!(" {label}"), lw), style::dim()),
        Span::styled(pad_l(&before.map_or_else(dash, fmt), cw), style::fg()),
        Span::styled(pad_l(&after.map_or_else(dash, fmt), cw), style::fgb()),
        Span::styled(pad_l(&change, cw), st),
    ])
}

/// A sentence about why the pattern changed, and whether it carries the `▲`.
fn cause_sentence(p: &PatternDiff) -> (&'static str, Style, bool) {
    let c = Change::of(p);
    let text = match p.cause {
        Cause::New => "New since the snapshot: nothing matched this pattern then.",
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
    (text, st, p.cause == Cause::New)
}

/// Growth of at least this share a day, either way, is coloured in the trend line.
const NOTABLE_GROWTH: f64 = 0.005;

/// The detail pane's trend: the sparkline, then the least-squares growth a day in words.
fn trend_lines(t: &Trend, pattern: &str, w: usize) -> Vec<Line<'static>> {
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
    let mut lines = vec![Line::from(vec![
        Span::styled(" TREND ", style::hd()),
        Span::styled(sparkline_for(t, pattern), style::fg()),
    ])];
    lines.extend(paragraph(vec![Span::raw(" ")], " ", &text, st, w));
    lines
}

fn detail_lines(
    frame: &mut Frame,
    inner: Rect,
    p: &PatternDiff,
    trend: Option<&Trend>,
    memory: Option<String>,
) {
    let w = usize::from(inner.width);
    let sizes = p.delta_bytes.is_some();
    let dims = if w >= DETAIL_WIDE { (16, 10) } else { (10, 8) };
    let (sentence, st, marked) = cause_sentence(p);
    let mut lines = vec![
        Line::from(Span::styled(" WHERE THE CHANGE CAME FROM", style::hd())),
        Line::from(vec![
            Span::raw(" ".repeat(dims.0)),
            Span::styled(pad_l("BEFORE", dims.1), style::hd()),
            Span::styled(pad_l("NOW", dims.1), style::hd()),
            Span::styled(pad_l("CHANGE", dims.1), style::hd()),
        ]),
        compare_line(
            p,
            "keys",
            (p.keys_before, p.keys_after),
            format::count,
            true,
            dims,
        ),
        compare_line(
            p,
            "avg size",
            (
                avg(p.bytes_before, p.keys_before),
                avg(p.bytes_after, p.keys_after),
            ),
            format::bytes,
            sizes,
            dims,
        ),
        compare_line(
            p,
            "memory",
            (p.bytes_before, p.bytes_after),
            format::bytes,
            sizes,
            dims,
        ),
    ];
    let spacer = if let Some(t) = trend {
        lines.push(Line::default());
        lines.extend(trend_lines(t, &p.pattern, w));
        Some(5)
    } else {
        None
    };
    lines.push(Line::default());
    let lead = if marked {
        Span::styled(format!(" {MARK} "), st)
    } else {
        Span::raw("   ")
    };
    lines.extend(paragraph(vec![lead], "   ", sentence, st, w));
    if !sizes {
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            "Sizes unavailable: one of the scans could not run MEMORY USAGE.",
            style::dim(),
            w,
        ));
    }
    if let Some(m) = memory {
        lines.push(Line::default());
        lines.extend(paragraph(vec![Span::raw(" ")], " ", &m, style::dim(), w));
    }
    // The trend takes rows: give up the blank one before it rather than the last line,
    // when the pane is too short for both.
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

    #[test]
    fn changes_before_an_analysis_says_what_it_needs() {
        use super::super::analysis::support::no_analysis;
        use crate::tui::app::views::testing;
        for (w, h) in [(80, 24), (120, 36)] {
            let app = no_analysis(crate::tui::app::Screen::Diff);
            let s = testing::render(&app, w, h).backend().to_string();
            assert!(s.contains("waits for an analysis"), "{s}");
            assert!(
                s.contains("Changes compares this analysis with the last snapshot."),
                "{s}"
            );
            assert!(s.contains("r  Analyze: "), "{s}");
            insta::assert_snapshot!(format!("changes_no_analysis_{w}x{h}"), s);
        }
    }

    #[test]
    fn the_columns_follow_the_width_and_v() {
        let cols = columns(0);
        // 80 columns: the mark, the change and its percent.
        let (keep, _) = pick_cols(&cols, 41, WIDE_AT, false);
        assert_eq!(keep, vec![0, MARK_COL, DELTA, PCT]);
        // 120 columns add NOW and WHY.
        let (keep, _) = pick_cols(&cols, 69, WIDE_AT, false);
        assert_eq!(keep, vec![0, MARK_COL, DELTA, PCT, NOW, WHY]);
        // Without a trend there is no TREND column to pick with `v`.
        let (keep, _) = pick_cols(&cols, 200, WIDE_AT, true);
        assert!(keep.contains(&BEFORE) && keep.contains(&BAR));
    }
}
