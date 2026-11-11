//! View 1: key patterns as a tree, with a detail panel for the selected row.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::diff::ReportDiff;
use crate::format;
use crate::model::{PatternStats, TTL_BUCKET_LABELS, Ttl};
use crate::tui::app::{App, PatternData};
use crate::tui::clipboard::{NO_COMMAND, suggested_command};
use crate::tui::tree::{Root, Row, SortBy, Summary, Verdict, summarize, summarize_group};

use super::delta::Change;
use super::widgets::{
    bar, fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset, split_cols,
    trunc, ttl_hist_spans, width,
};
use super::{mark_previous, report_or_wait, split_body, style};

/// Draws the finished report or, while the scan runs, its partial results (marked as
/// such, without the Δ column). Otherwise the waiting or failure message.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(data) = app.pattern_data() else {
        // No report and no partial results: draws the waiting or failure message.
        let _ = report_or_wait(frame, area, app, "Patterns");
        return;
    };
    let (main, detail) = split_body(area);
    let rows_ = app.pattern_rows();
    let sel = app.patterns.sel.min(rows_.len().saturating_sub(1));
    let patterns = data.patterns;
    let summaries: Vec<(Summary, usize, Option<bool>)> = rows_
        .iter()
        .map(|r| row_summary(app, patterns, *r))
        .collect();
    // Changes only for a finished scan: a snapshot diff is of the final report.
    let diff = app
        .session
        .as_ref()
        .and_then(|s| s.diff())
        .filter(|_| data.partial.is_none());
    let changes: Option<Vec<Option<Change>>> = diff.map(|d| {
        rows_
            .iter()
            .map(|r| row_change(app, patterns, d, *r))
            .collect()
    });
    table(frame, main, app, data, &summaries, changes.as_deref(), sel);
    match summaries.get(sel) {
        Some((s, _, _)) => detail_panel(frame, detail, s, data.sizes_available),
        None => {
            let inner = panel(frame, detail, "No match", "");
            let text = if !app.patterns.filter.is_empty() {
                format!(
                    " No pattern matches \"{}\". Press esc to clear the filter.",
                    app.patterns.filter
                )
            } else if data.partial.is_some() {
                " No keys sampled yet.".to_string()
            } else {
                " No keys were sampled.".to_string()
            };
            rows(
                frame,
                inner,
                vec![
                    Line::default(),
                    Line::from(Span::styled(text, style::dim())),
                ],
                None,
            );
        }
    }
}

/// The change since the snapshot for a row. Parent rows sum their children.
fn row_change(app: &App, patterns: &[PatternStats], d: &ReportDiff, row: Row) -> Option<Change> {
    let of = |i: usize| d.pattern(&patterns[i].pattern).map(Change::of);
    match row {
        Row::Group { root } => match &app.patterns.tree.roots[root] {
            Root::Group(g) => Change::sum(g.children.iter().filter_map(|&i| of(i))),
            Root::Single(i) => of(*i),
        },
        Row::Pattern { idx, .. } => of(idx),
    }
}

/// The summary for a row, its depth, and for groups whether they are expanded.
fn row_summary(app: &App, patterns: &[PatternStats], row: Row) -> (Summary, usize, Option<bool>) {
    match row {
        Row::Group { root } => match &app.patterns.tree.roots[root] {
            Root::Group(g) => (
                summarize_group(g, patterns),
                0,
                Some(!app.patterns.collapsed.contains(&g.key)),
            ),
            Root::Single(i) => (summarize(&patterns[*i]), 0, None),
        },
        Row::Pattern { idx, parent } => (
            summarize(&patterns[idx]),
            usize::from(parent.is_some()),
            None,
        ),
    }
}

/// Column widths of the table. A hidden column is 0 or false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cols {
    name: usize,
    /// The share bar; 0 when hidden. The share percentage always shows.
    share: usize,
    /// The Δ column.
    delta: bool,
    /// The average size column.
    avg: bool,
}

const TYPE_W: usize = 8;
const NUM_W: usize = 7;
const PCT_W: usize = 4;
const TTL_W: usize = 6;
/// The Δ column, shown only when there is a snapshot to compare with.
const DELTA_W: usize = 7;
/// The narrowest the pattern column gets before another column is hidden.
const NAME_MIN: usize = 12;

impl Cols {
    /// Everything but the pattern column.
    fn fixed(&self) -> usize {
        let mut w = TYPE_W + (NUM_W + 2) * 2 + self.share + 1 + PCT_W + 2 + TTL_W + 2;
        if self.avg {
            w += NUM_W + 2;
        }
        if self.delta {
            w += DELTA_W + 1;
        }
        w
    }
}

/// Column widths for a table `inner` cells wide. When the pattern column would get
/// narrower than [`NAME_MIN`], columns are hidden in this order: the share bar, Δ, the
/// average size. Only when all three are hidden is a row cut at the right edge.
fn cols(inner: usize, delta: bool) -> Cols {
    let share = if inner >= 100 { 14 } else { 8 };
    let full = Cols {
        name: 0,
        share,
        delta,
        avg: true,
    };
    let steps = [
        full,
        Cols { share: 0, ..full },
        Cols {
            share: 0,
            delta: false,
            ..full
        },
        Cols {
            share: 0,
            delta: false,
            avg: false,
            ..full
        },
    ];
    let pick = steps
        .iter()
        .find(|c| inner >= c.fixed() + NAME_MIN)
        .unwrap_or(&steps[3]);
    Cols {
        name: inner.saturating_sub(pick.fixed()).max(NAME_MIN),
        ..*pick
    }
}

/// A right-aligned header `w` wide and its two-cell gap, with the sort arrow first in
/// the gap when `arrow` is set.
fn head_num(label: &str, w: usize, arrow: Option<&str>) -> String {
    format!("{}{} ", pad_l(label, w), arrow.unwrap_or(" "))
}

fn type_label(s: &Summary) -> &'static str {
    s.key_type.map_or("mixed", |t| t.as_str())
}

fn table(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    data: PatternData<'_>,
    summaries: &[(Summary, usize, Option<bool>)],
    changes: Option<&[Option<Change>]>,
    sel: usize,
) {
    let sizes = data.sizes_available;
    let sort = app.patterns.sort.effective(sizes);
    let mut right = if !app.patterns.filter.is_empty() {
        format!(
            "filter \"{}\" · {} rows",
            app.patterns.filter,
            summaries.len()
        )
    } else if !sizes {
        format!(
            "sizes unavailable: MEMORY USAGE denied · sorted by {}",
            sort.label()
        )
    } else {
        format!(
            "{} patterns · sorted by {}",
            data.patterns.len(),
            sort.label()
        )
    };
    // The marker comes first, so a narrow title cuts the rest, not the marker.
    if let Some(p) = data.partial {
        right = format!(
            "partial · {} / {} sampled · {right}",
            format::grouped(p.sampled),
            format::grouped(p.target)
        );
    }
    let right = mark_previous(app, &right);
    let inner = panel(frame, area, "Patterns", &right);
    let c = cols(usize::from(inner.width), changes.is_some());
    // ▼ beside the sorted column, ▲ for name (A to Z).
    let arrow = |s: SortBy| (sort == s).then_some(if s.ascending() { "▲" } else { "▼" });
    let pattern_head = match arrow(SortBy::Name) {
        Some(a) => format!(" PATTERN {a}"),
        None => " PATTERN".to_string(),
    };
    let mut head = Line::from(vec![
        Span::styled(pad_r(&pattern_head, c.name), style::hd()),
        Span::styled(pad_r("TYPE", TYPE_W), style::hd()),
        Span::styled(head_num("KEYS", NUM_W, arrow(SortBy::Keys)), style::hd()),
        Span::styled(
            head_num("MEMORY", NUM_W, arrow(SortBy::Memory)),
            style::hd(),
        ),
    ]);
    if c.share > 0 {
        head.push_span(Span::styled(pad_r("SHARE", c.share), style::hd()));
    }
    head.push_span(Span::styled(
        format!(" {}  ", pad_l("%", PCT_W)),
        style::hd(),
    ));
    if c.avg {
        head.push_span(Span::styled(
            head_num("AVG", NUM_W, arrow(SortBy::AvgSize)),
            style::hd(),
        ));
    }
    head.push_span(Span::styled(
        head_num("NO TTL", TTL_W, arrow(SortBy::NoTtl)),
        style::hd(),
    ));
    if c.delta {
        head.push_span(Span::styled(
            format!("{} ", pad_l("Δ", DELTA_W)),
            style::hd(),
        ));
    }
    let total: u64 = data
        .patterns
        .iter()
        .map(|p| {
            if sizes {
                p.est_bytes.unwrap_or(0)
            } else {
                p.est_keys
            }
        })
        .sum();
    let lines: Vec<Line<'static>> = summaries
        .iter()
        .enumerate()
        .map(|(i, (s, depth, open))| {
            let scroll = (i == sel).then(|| app.sel_ticks());
            let mut line = pattern_row(s, *depth, *open, total, sizes, &c, scroll);
            if let Some(ch) = changes.filter(|_| c.delta) {
                line.push_span(match ch.get(i).copied().flatten() {
                    Some(d) => Span::styled(format!("{} ", pad_l(&d.text(), DELTA_W)), d.style()),
                    None => Span::styled(format!("{} ", pad_l("—", DELTA_W)), style::dim()),
                });
            }
            line
        })
        .collect();
    rows(frame, inner, vec![head], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let visible: Vec<Line<'static>> = lines.into_iter().skip(off).collect();
    rows(frame, body, visible, Some(sel - off));
}

fn pattern_row(
    s: &Summary,
    depth: usize,
    open: Option<bool>,
    total: u64,
    sizes: bool,
    c: &Cols,
    scroll: Option<u64>,
) -> Line<'static> {
    let mark = match open {
        Some(true) => "▾ ",
        Some(false) => "▸ ",
        None => "  ",
    };
    let (base, ph) = if open.is_some() {
        (style::fgb(), style::phb())
    } else {
        (style::fg(), style::ph())
    };
    // The indent and the fold mark stay put; only the text after them scrolls.
    let lead = format!(" {}{mark}", "  ".repeat(depth));
    let text_w = (c.name - 1).saturating_sub(width(&lead));
    let text = pattern_spans(&s.label, base, ph);
    let mut spans = vec![Span::styled(lead, style::dim())];
    spans.extend(match scroll {
        Some(ticks) => marquee_spans(text, text_w, ticks),
        None => fit(text, text_w),
    });
    spans.push(Span::raw(" "));

    let weight = if sizes {
        s.est_bytes.unwrap_or(0)
    } else {
        s.est_keys
    };
    let share = if total == 0 {
        0.0
    } else {
        weight as f64 / total as f64
    };
    let (fill, track) = bar(share, c.share);
    let dash = || "—".to_string();
    let mem = s.est_bytes.map_or_else(dash, format::bytes);
    let avg = s.avg_bytes().map_or_else(dash, format::bytes);
    let (avg, avg_style) = match s.verdict {
        Verdict::Large(_) => (format!("▲ {avg}"), style::warn()),
        _ => (avg, style::fg()),
    };
    let ratio = s.no_ttl_ratio();
    let pct = format::percent(ratio);
    let (ttl, ttl_style) = match s.verdict {
        Verdict::MixedTtl(_) => (format!("▲ {pct}"), style::warn()),
        _ if s.no_ttl == 0 || s.no_ttl == s.sampled_keys => (pct, style::dim()),
        _ => (pct, style::fg()),
    };
    spans.extend([
        Span::styled(pad_r(type_label(s), TYPE_W), style::dim()),
        Span::styled(
            format!("{}  ", pad_l(&format::count(s.est_keys), NUM_W)),
            style::fg(),
        ),
        Span::styled(format!("{}  ", pad_l(&mem, NUM_W)), style::fg()),
    ]);
    if c.share > 0 {
        spans.push(Span::styled(fill, style::amber()));
        spans.push(Span::styled(track, style::track()));
    }
    spans.push(Span::styled(
        format!(" {}  ", pad_l(&format::percent(share), PCT_W)),
        style::dim(),
    ));
    if c.avg {
        spans.push(Span::styled(format!("{}  ", pad_l(&avg, NUM_W)), avg_style));
    }
    spans.push(Span::styled(format!("{}  ", pad_l(&ttl, TTL_W)), ttl_style));
    Line::from(spans)
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

fn ttl_bars(s: &Summary) -> Vec<Span<'static>> {
    let none = if matches!(s.verdict, Verdict::MixedTtl(_)) {
        style::warn()
    } else {
        style::amber()
    };
    ttl_hist_spans(&s.ttl_hist, none)
}

fn verdict_line(s: &Summary) -> Span<'static> {
    match s.verdict {
        Verdict::MixedTtl(r) => Span::styled(
            format!(
                " ▲ {} never expire while the rest do. A write path probably skips EXPIRE.",
                format::percent(r)
            ),
            style::warn(),
        ),
        Verdict::Large(max) => Span::styled(
            format!(" ▲ Very large keys: max {}.", format::bytes(max)),
            style::warn(),
        ),
        Verdict::Ok => Span::styled(" ✓ No TTL or size problems in the sample.", style::ok()),
        Verdict::Group {
            children,
            warnings: 0,
        } => Span::styled(
            format!(" {children} child patterns. → expands, ← collapses."),
            style::dim(),
        ),
        Verdict::Group { children, warnings } => Span::styled(
            format!(
                " ▲ {warnings} of {children} child patterns {} a warning. Expand the group to see them.",
                plural(warnings as u64, "has", "have")
            ),
            style::warn(),
        ),
    }
}

fn detail_panel(frame: &mut Frame, area: Rect, s: &Summary, sizes: bool) {
    let dash = || "—".to_string();
    let mut right_title = format!(
        "{} · {} {}",
        type_label(s),
        format::count(s.est_keys),
        plural(s.est_keys, "key", "keys")
    );
    if let Some(b) = s.est_bytes {
        right_title.push_str(&format!(" · {}", format::bytes(b)));
    }
    let inner = panel(frame, area, &s.label, &right_title);
    let (lw, rw) = split_cols(usize::from(inner.width));
    let kw = lw.saturating_sub(1 + NUM_W + 3 + 9).max(8);
    let mixed = matches!(s.verdict, Verdict::MixedTtl(_));

    let mut left: Vec<Vec<Span<'static>>> = vec![vec![
        Span::styled(format!(" {}", pad_r("SAMPLE KEYS", kw)), style::hd()),
        Span::styled(pad_l("SIZE", NUM_W), style::hd()),
        Span::styled("   TTL", style::hd()),
    ]];
    for e in s.examples.iter().take(5) {
        let (ttl, ttl_style) = match e.ttl {
            Ttl::Persistent => (
                "none".to_string(),
                if mixed { style::warn() } else { style::dim() },
            ),
            Ttl::Expires(d) => (format::duration(d), style::fg()),
        };
        left.push(vec![
            Span::styled(format!(" {}", pad_r(&e.key, kw)), style::fg()),
            Span::styled(
                pad_l(&e.bytes.map_or_else(dash, format::bytes), NUM_W),
                style::fg(),
            ),
            Span::styled(format!("   {ttl}"), ttl_style),
        ]);
    }

    let num = |v: Option<u64>| v.map_or_else(dash, format::bytes);
    let mut size_line = vec![
        Span::styled("p50 ", style::dim()),
        Span::styled(num(s.size_p50), style::fg()),
        Span::styled("   p99 ", style::dim()),
        Span::styled(num(s.size_p99), style::fg()),
        Span::styled("   max ", style::dim()),
        Span::styled(num(s.size_max), style::fg()),
    ];
    if !sizes {
        size_line.push(Span::styled("   (no MEMORY USAGE)", style::dim()));
    }
    let right: Vec<Vec<Span<'static>>> = vec![
        vec![Span::styled("SIZE DISTRIBUTION", style::hd())],
        size_line,
        vec![
            Span::styled("avg ", style::dim()),
            Span::styled(num(s.avg_bytes()), style::fg()),
            Span::styled("   sampled ", style::dim()),
            Span::styled(format::grouped(s.sampled_keys), style::fg()),
            Span::styled(
                format!(" {}", plural(s.sampled_keys, "key", "keys")),
                style::dim(),
            ),
        ],
        vec![Span::styled("TTL REMAINING", style::hd())],
        vec![Span::styled(
            TTL_BUCKET_LABELS
                .iter()
                .map(|l| pad_r(l, 5))
                .collect::<String>(),
            style::dim(),
        )],
        ttl_bars(s),
    ];

    let mut lines: Vec<Line<'static>> = Vec::new();
    for i in 0..6 {
        let mut spans = fit(left.get(i).cloned().unwrap_or_default(), lw);
        spans.extend(fit(right.get(i).cloned().unwrap_or_default(), rw));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(verdict_line(s)));
    // The same text `Y` copies.
    // Without a command there is no `$ ` prompt, so copying the line with the mouse
    // gives nothing that looks like a command.
    let (prompt, cmd, cmd_style) = match suggested_command(&s.label) {
        Some(c) => (" $ ", c, style::cmd()),
        None => ("   ", NO_COMMAND.to_string(), style::dim()),
    };
    lines.push(Line::from(vec![
        Span::styled(prompt, style::dim()),
        Span::styled(
            trunc(&cmd, usize::from(inner.width).saturating_sub(3)),
            cmd_style,
        ),
    ]));
    rows(frame, inner, lines, None);
}
