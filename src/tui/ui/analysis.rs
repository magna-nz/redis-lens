//! Draws [`AnalysisState`]: the Big keys panel with Recommendations below it, and the
//! Expiry panel (spec "2 Analysis"). Owner: wave B2, with `views/analysis.rs`.
//!
//! Drawn only once the scan has finished (`ctx.report()` is `Some`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::analysis::{
    BIG_KEY_BYTES, EXPIRY_LABELS, Expiry, NEVER_EXPIRES_SHARE, Recommendation, Severity, expiry,
    length_unit, pattern_of, recommendations,
};
use crate::format;
use crate::model::{KeySample, ScanReport, ServerMode, Ttl};
use crate::tui::app::Screen;
use crate::tui::app::views::Ctx;
use crate::tui::app::views::analysis::AnalysisState;

use super::widgets::{
    bar, fit, marquee_spans, pad_l, pad_r, panel, pattern_spans, rows, scroll_offset, trunc,
};
use super::{spinner, split_body, style};

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &AnalysisState, ctx: &Ctx<'_>) {
    let Some(report) = ctx.report() else {
        return;
    };
    match ctx.screen {
        Screen::Expiry => draw_expiry(frame, area, report, ctx),
        _ => draw_big_keys(frame, area, state, report, ctx),
    }
}

/// `right` with the rescan marker first, as the other Analysis panels mark results
/// kept from the previous run.
fn mark_previous(ctx: &Ctx<'_>, right: &str) -> String {
    let marker = ctx.session.previous.map(|_| match ctx.session.phase {
        crate::tui::app::Phase::Failed(_) => "rescan failed · previous results",
        _ => "rescanning · previous results",
    });
    match marker {
        Some(m) if right.is_empty() => m.to_string(),
        Some(m) => format!("{m} · {right}"),
        None => right.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Big keys
// ---------------------------------------------------------------------------

const TYPE_W: usize = 7;
const SIZE_W: usize = 6;
const LEN_W: usize = 13;
const TTL_W: usize = 7;

/// Widths of the key and pattern columns for a table `inner` cells wide.
fn name_widths(inner: usize) -> (usize, usize) {
    let fixed = (TYPE_W + 1) + (SIZE_W + 2) + (LEN_W + 2) + (TTL_W + 1);
    let rest = inner.saturating_sub(fixed);
    let pattern = (rest * 2 / 5).clamp(12, 28);
    (rest.saturating_sub(pattern).max(12), pattern)
}

fn ttl_text(t: Ttl) -> String {
    match t {
        Ttl::Persistent => "none".to_string(),
        Ttl::Expires(d) => format::duration(d),
    }
}

fn draw_big_keys(
    frame: &mut Frame,
    area: Rect,
    state: &AnalysisState,
    report: &ScanReport,
    ctx: &Ctx<'_>,
) {
    let (main, detail) = split_body(area);
    let keys = report.big_keys();
    let sel = state.sel.min(keys.len().saturating_sub(1));
    let mut right = format!(
        "top {} of {} sampled · by size",
        keys.len(),
        format::count(report.sampled_keys)
    );
    if state.is_loading() {
        right = format!("{} reading lengths · {right}", spinner(ctx.tick));
    }
    let inner = panel(frame, main, "Big keys", &mark_previous(ctx, &right));
    if keys.is_empty() {
        let text = if report.sizes_available {
            " No sampled key had a size, so there are no big keys to rank."
        } else {
            " No sizes: MEMORY USAGE is not allowed for this user, so keys can't be ranked by size."
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
    } else {
        let (kw, pw) = name_widths(usize::from(inner.width));
        let head = Line::from(vec![
            Span::styled(pad_r(" KEY", kw), style::hd()),
            Span::styled(pad_r("PATTERN", pw), style::hd()),
            Span::styled(pad_r("TYPE", TYPE_W + 1), style::hd()),
            Span::styled(format!("{}▼ ", pad_l("SIZE", SIZE_W)), style::hd()),
            Span::styled(format!("{}  ", pad_l("LENGTH", LEN_W)), style::hd()),
            Span::styled(pad_l("TTL", TTL_W), style::hd()),
        ]);
        rows(frame, inner, vec![head], None);
        let body = Rect {
            y: inner.y + 1,
            height: inner.height.saturating_sub(1),
            ..inner
        };
        let off = scroll_offset(sel, usize::from(body.height));
        let ticks = ctx.tick.saturating_sub(state.sel_since);
        let lines: Vec<Line<'static>> = keys
            .iter()
            .enumerate()
            .skip(off)
            .take(usize::from(body.height))
            .map(|(i, k)| {
                let len = length_cell(state, k, report);
                big_row(
                    k,
                    &pattern_of(report, &k.key),
                    &len,
                    kw,
                    pw,
                    (i == sel).then_some(ticks),
                )
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }
    let recs = recommendations(report, ctx.session.diff(), &state.facts);
    recommendations_panel(frame, detail, state, &recs, ctx);
}

/// The LENGTH cell: the length and its unit, `…` while it is being read, `—` when it
/// is unknown.
fn length_cell(state: &AnalysisState, k: &KeySample, report: &ScanReport) -> String {
    match state.facts.get(&k.key).and_then(|f| f.length) {
        Some(n) => match length_unit(k.key_type) {
            Some(u) => format!("{} {u}", format::count(n)),
            None => format::count(n),
        },
        None if state.is_loading() && !state.loaded(report) => "…".to_string(),
        None => "—".to_string(),
    }
}

fn big_row(
    k: &KeySample,
    pattern: &str,
    len: &str,
    kw: usize,
    pw: usize,
    scroll: Option<u64>,
) -> Line<'static> {
    let text = vec![Span::styled(format!(" {}", k.key), style::fg())];
    let w = kw.saturating_sub(1);
    let mut spans = match scroll {
        Some(ticks) => marquee_spans(text, w, ticks),
        None => fit(text, w),
    };
    spans.push(Span::raw(" "));
    spans.extend(fit(
        pattern_spans(pattern, style::dim(), style::ph()),
        pw.saturating_sub(1),
    ));
    spans.push(Span::raw(" "));
    let big = k.bytes.is_some_and(|b| b >= BIG_KEY_BYTES);
    let size = k.bytes.map_or_else(|| "—".to_string(), format::bytes);
    let ttl_style = match k.ttl {
        Ttl::Persistent => style::dim(),
        Ttl::Expires(_) => style::fg(),
    };
    spans.extend([
        Span::styled(pad_r(k.key_type.as_str(), TYPE_W + 1), style::dim()),
        Span::styled(
            format!("{}  ", pad_l(&size, SIZE_W)),
            if big { style::warn() } else { style::fgb() },
        ),
        Span::styled(
            format!("{}  ", pad_l(&trunc(len, LEN_W), LEN_W)),
            style::fg(),
        ),
        Span::styled(pad_l(&ttl_text(k.ttl), TTL_W), ttl_style),
    ]);
    Line::from(spans)
}

fn rec_line(r: &Recommendation, w: usize) -> Line<'static> {
    let (mark, mark_style, text_style) = match r.severity {
        Severity::Warn => ("▲", style::warn(), style::fg()),
        Severity::Note => ("●", style::amber(), style::fg()),
        Severity::Info => ("●", style::dim(), style::dim()),
    };
    let spans = vec![
        Span::styled(format!(" {mark} "), mark_style),
        Span::styled(r.text.clone(), text_style),
        Span::styled(format!(" {}", r.hint), style::dim()),
    ];
    Line::from(fit(spans, w))
}

fn recommendations_panel(
    frame: &mut Frame,
    area: Rect,
    state: &AnalysisState,
    recs: &[Recommendation],
    ctx: &Ctx<'_>,
) {
    let right = if recs.is_empty() {
        String::new()
    } else {
        recs.len().to_string()
    };
    let inner = panel(frame, area, "Recommendations", &right);
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    let mut notes: Vec<Line<'static>> = Vec::new();
    if let Some(e) = &state.error {
        notes.push(Line::from(fit(
            vec![Span::styled(format!(" ✗ {e}"), style::warn())],
            w,
        )));
    }
    for e in &state.refused {
        notes.push(Line::from(fit(
            vec![Span::styled(
                format!(" ▲ Without it, some rules are skipped: {e}."),
                style::amber(),
            )],
            w,
        )));
    }
    let cluster = ctx.server().is_some_and(|s| s.mode == ServerMode::Cluster);
    if !ctx.live_ready() && state.facts.is_empty() && !state.is_loading() {
        let why = if cluster {
            // A cluster opens no live connection (see `views::CLUSTER_OFF`).
            " Lengths and encodings are not read on a cluster yet, so the rules that need them are skipped."
        } else {
            " Lengths and encodings are read once the live connection is open."
        };
        notes.push(Line::from(fit(vec![Span::styled(why, style::dim())], w)));
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    if recs.is_empty() {
        lines.push(Line::from(Span::styled(
            " ✓ Nothing to recommend from this scan.",
            style::ok(),
        )));
    }
    let room = h.saturating_sub(notes.len());
    if recs.len() > room {
        let shown = room.saturating_sub(1);
        lines.extend(recs[..shown].iter().map(|r| rec_line(r, w)));
        lines.push(Line::from(Span::styled(
            format!(" … and {} more", recs.len() - shown),
            style::dim(),
        )));
    } else {
        lines.extend(recs.iter().map(|r| rec_line(r, w)));
    }
    lines.extend(notes);
    rows(frame, inner, lines, None);
}

// ---------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------

const LABEL_W: usize = 14;
const VALUE_W: usize = 7;

fn draw_expiry(frame: &mut Frame, area: Rect, report: &ScanReport, ctx: &Ctx<'_>) {
    let (main, detail) = split_body(area);
    let right = mark_previous(ctx, "memory freed by TTL · scaled like the estimates");
    let inner = panel(frame, main, "Expiry", &right);
    let w = usize::from(inner.width);
    let Some(e) = expiry(report) else {
        let text = if report.sizes_available {
            " This scan has no memory figures per TTL. Scan again to get them."
        } else {
            " No sizes: MEMORY USAGE is not allowed for this user, so there is no memory to sum."
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
        panel(frame, detail, "Details", "");
        return;
    };
    let mut lines = vec![
        Line::default(),
        Line::from(fit(
            vec![Span::styled(
                format!(
                    " From PTTL and MEMORY USAGE of {} sampled keys.",
                    format::grouped(report.sampled_keys)
                ),
                style::dim(),
            )],
            w,
        )),
        Line::default(),
    ];
    let bar_w = w.saturating_sub(1 + LABEL_W + 2 + VALUE_W + 2 + 5).min(40);
    let row = |label: String, v: u64, never: bool| {
        let ratio = if e.total == 0 {
            0.0
        } else {
            v as f64 / e.total as f64
        };
        let (fill, track) = bar(ratio, bar_w);
        let fill_style = if never && e.never_share() > NEVER_EXPIRES_SHARE {
            style::warn()
        } else if never {
            style::amber()
        } else {
            style::ok()
        };
        Line::from(fit(
            vec![
                Span::styled(format!(" {}", pad_r(&label, LABEL_W)), style::fg()),
                Span::styled(fill, fill_style),
                Span::styled(format!("{track}  "), style::track()),
                Span::styled(pad_l(&format::bytes(v), VALUE_W), style::fgb()),
                Span::styled(
                    format!("  {}", pad_l(&format::percent(ratio), 4)),
                    style::dim(),
                ),
            ],
            w,
        ))
    };
    let mut bars: Vec<Line> = EXPIRY_LABELS
        .iter()
        .zip(e.within)
        .map(|(label, v)| row(format!("within {label}"), v, false))
        .collect();
    bars.push(row("never".to_string(), e.never, true));
    // On a short terminal the blank lines go first, then the source line: every bar,
    // `never` above all, stays on screen.
    let room = usize::from(inner.height).saturating_sub(bars.len());
    if room < lines.len() {
        lines.retain(|l| l.width() > 0);
    }
    lines.truncate(room);
    lines.extend(bars);
    rows(frame, inner, lines, None);
    expiry_detail(frame, detail, &e);
}

fn expiry_detail(frame: &mut Frame, area: Rect, e: &Expiry) {
    let inner = panel(
        frame,
        area,
        "Details",
        &format!("{} in all", format::bytes(e.total)),
    );
    let w = usize::from(inner.width);
    let line = |spans: Vec<Span<'static>>| Line::from(fit(spans, w));
    let share = format::percent(e.never_share());
    let never = if e.never_share() > NEVER_EXPIRES_SHARE {
        Span::styled(
            format!(
                " ▲ {share} of memory never expires ({}): over a quarter, see Big keys › Recommendations.",
                format::bytes(e.never)
            ),
            style::amber(),
        )
    } else {
        Span::styled(
            format!(
                " {share} of memory never expires ({}).",
                format::bytes(e.never)
            ),
            style::dim(),
        )
    };
    let lines = vec![
        line(vec![Span::styled(
            " Each row adds up the keys that expire by then: within 1 d includes within 1 h.",
            style::dim(),
        )]),
        line(vec![never]),
        line(vec![Span::styled(
            format!(" {} expires later than 30 days.", format::bytes(e.later)),
            style::dim(),
        )]),
        line(vec![Span::styled(
            " Sampled sizes are scaled to the whole keyspace, like the estimates on Patterns.",
            style::dim(),
        )]),
    ];
    rows(frame, inner, lines, None);
}
