//! View 1: key patterns as a tree, with a detail panel for the selected row.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format;
use crate::model::{ScanReport, TTL_BUCKET_LABELS, Ttl};
use crate::tui::app::{App, Phase, Session};
use crate::tui::tree::{Root, Row, Summary, Verdict, summarize, summarize_group};

use super::widgets::{
    bar, fit, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset, split_cols, trunc,
};
use super::{spinner, split_body, style};

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let (main, detail) = split_body(area);
    let Some(session) = &app.session else {
        message(
            frame,
            main,
            detail,
            Span::styled(
                " No scan yet. Open 0 Connections, pick a connection and press enter.",
                style::dim(),
            ),
        );
        return;
    };
    let Some(report) = &session.report else {
        waiting(frame, main, detail, app, session);
        return;
    };
    let rows_ = app.pattern_rows();
    let sel = app.patterns.sel.min(rows_.len().saturating_sub(1));
    let summaries: Vec<(Summary, usize, Option<bool>)> =
        rows_.iter().map(|r| row_summary(app, report, *r)).collect();
    table(frame, main, app, report, &summaries, sel);
    match summaries.get(sel) {
        Some((s, _, _)) => detail_panel(frame, detail, s, report.sizes_available),
        None => {
            let inner = panel(frame, detail, "No match", "");
            let text = if app.patterns.filter.is_empty() {
                " No keys were sampled.".to_string()
            } else {
                format!(
                    " No pattern matches \"{}\". Press esc to clear the filter.",
                    app.patterns.filter
                )
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

fn message(frame: &mut Frame, main: Rect, detail: Rect, text: Span<'static>) {
    let inner = panel(frame, main, "Patterns", "");
    rows(frame, inner, vec![Line::default(), Line::from(text)], None);
    panel(frame, detail, "Details", "");
}

fn waiting(frame: &mut Frame, main: Rect, detail: Rect, app: &App, s: &Session) {
    let spin = spinner(app.tick);
    let p = &s.profile;
    let inner = panel(frame, main, "Patterns", &p.name);
    let mut lines = vec![Line::default()];
    match &s.phase {
        Phase::Failed(e) => {
            lines.push(Line::from(Span::styled(format!(" ✗ {e}"), style::warn())));
            lines.push(Line::from(Span::styled(
                " Press 0 to go back to Connections and try again.",
                style::dim(),
            )));
        }
        Phase::Connecting => lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(spin, style::amber()),
            Span::styled(
                format!(
                    " Connecting to {} ({}) and checking permissions…",
                    p.name, p.endpoint
                ),
                style::dim(),
            ),
        ])),
        Phase::Scanning | Phase::Done => {
            let pr = s.progress.unwrap_or_default();
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spin, style::amber()),
                Span::styled(" Sampled ", style::dim()),
                Span::styled(
                    format!(
                        "{} / {}",
                        format::grouped(pr.sampled),
                        format::grouped(pr.target)
                    ),
                    style::fgb(),
                ),
                Span::styled(" keys", style::dim()),
            ]));
            let ratio = if pr.target == 0 {
                0.0
            } else {
                pr.sampled as f64 / pr.target as f64
            };
            let w = usize::from(inner.width).saturating_sub(4).min(40);
            let (fill, track) = bar(ratio, w);
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(fill, style::amber()),
                Span::styled(track, style::track()),
            ]));
        }
    }
    rows(frame, inner, lines, None);
    panel(frame, detail, "Details", "");
}

/// The summary for a row, its depth, and for groups whether they are expanded.
fn row_summary(app: &App, report: &ScanReport, row: Row) -> (Summary, usize, Option<bool>) {
    match row {
        Row::Group { root } => match &app.patterns.tree.roots[root] {
            Root::Group(g) => (
                summarize_group(g, &report.patterns),
                0,
                Some(!app.patterns.collapsed.contains(&g.key)),
            ),
            Root::Single(i) => (summarize(&report.patterns[*i]), 0, None),
        },
        Row::Pattern { idx, parent } => (
            summarize(&report.patterns[idx]),
            usize::from(parent.is_some()),
            None,
        ),
    }
}

struct Cols {
    name: usize,
    share: usize,
}

const TYPE_W: usize = 8;
const NUM_W: usize = 7;
const PCT_W: usize = 4;
const TTL_W: usize = 6;

fn cols(inner: usize) -> Cols {
    let share = if inner >= 100 { 14 } else { 8 };
    let fixed = TYPE_W + (NUM_W + 2) * 3 + share + 1 + PCT_W + 2 + TTL_W + 2;
    Cols {
        name: inner.saturating_sub(fixed).max(12),
        share,
    }
}

fn type_label(s: &Summary) -> &'static str {
    s.key_type.map_or("mixed", |t| t.as_str())
}

fn table(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    report: &ScanReport,
    summaries: &[(Summary, usize, Option<bool>)],
    sel: usize,
) {
    let sizes = report.sizes_available;
    let right = if !app.patterns.filter.is_empty() {
        format!(
            "filter \"{}\" · {} rows",
            app.patterns.filter,
            summaries.len()
        )
    } else if !sizes {
        "sizes unavailable: MEMORY USAGE denied · sorted by keys".to_string()
    } else {
        format!("{} patterns · sorted by memory", report.patterns.len())
    };
    let inner = panel(frame, area, "Patterns", &right);
    let c = cols(usize::from(inner.width));
    let head = Line::from(vec![
        Span::styled(pad_r(" PATTERN", c.name), style::hd()),
        Span::styled(pad_r("TYPE", TYPE_W), style::hd()),
        Span::styled(format!("{}  ", pad_l("KEYS", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("MEMORY", NUM_W)), style::hd()),
        Span::styled(pad_r("SHARE", c.share), style::hd()),
        Span::styled(format!(" {}  ", pad_l("%", PCT_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("AVG", NUM_W)), style::hd()),
        Span::styled(format!("{}  ", pad_l("NO TTL", TTL_W)), style::hd()),
    ]);
    let total: u64 = report
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
        .map(|(s, depth, open)| pattern_row(s, *depth, *open, total, sizes, &c))
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
    let mut name = vec![Span::styled(
        format!(" {}{mark}", "  ".repeat(depth)),
        style::dim(),
    )];
    name.extend(pattern_spans(&s.label, base, ph));
    let mut spans = fit(name, c.name - 1);
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
        Span::styled(fill, style::amber()),
        Span::styled(track, style::track()),
        Span::styled(
            format!(" {}  ", pad_l(&format::percent(share), PCT_W)),
            style::dim(),
        ),
        Span::styled(format!("{}  ", pad_l(&avg, NUM_W)), avg_style),
        Span::styled(format!("{}  ", pad_l(&ttl, TTL_W)), ttl_style),
    ]);
    Line::from(spans)
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

fn level(v: u64, max: u64) -> char {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let i = ((v as f64 / max as f64) * 8.0).ceil() as usize;
    LEVELS[i.clamp(1, 8) - 1]
}

fn ttl_bars(s: &Summary) -> Vec<Span<'static>> {
    let max = s.ttl_hist.iter().copied().max().unwrap_or(0);
    let mixed = matches!(s.verdict, Verdict::MixedTtl(_));
    s.ttl_hist
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            if v == 0 {
                Span::styled(pad_r("·", 5), style::track())
            } else {
                let st: Style = match i {
                    0 if mixed => style::warn(),
                    0 => style::amber(),
                    _ => style::ok(),
                };
                Span::styled(pad_r(&level(v, max).to_string().repeat(3), 5), st)
            }
        })
        .collect()
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
    let cmd = format!("redis-cli --scan --pattern '{}'", format::glob(&s.label));
    lines.push(Line::from(vec![
        Span::styled(" $ ", style::dim()),
        Span::styled(
            trunc(&cmd, usize::from(inner.width).saturating_sub(3)),
            style::cmd(),
        ),
    ]));
    rows(frame, inner, lines, None);
}
