//! Browse › Keys: the pattern tree on the left, the selected row's details on the
//! right, in one frame (brief, "Layout rules"; mockup "Browse").
//!
//! - **Before an analysis** the rows come from key names alone
//!   ([`crate::tui::app::views::names`]): PATTERN and KEYS, the keys estimated
//!   (`~1.9M`), and nothing a name does not tell (no type, size, TTL or `▲`). The
//!   bottom line says how many names were read and that `n` reads more.
//! - **After an analysis** the rows are the analysis's patterns, sorted by memory:
//!   PATTERN, KEYS, MEMORY and a `▲` mark at 80 columns; TYPE and the share bar join in
//!   at wider widths, and `v` adds the average size, the no-TTL share and the change
//!   since the last snapshot. The detail pane says why a row is marked in a sentence.
//! - **While an analysis runs** the names tree stays until its first partial results,
//!   which then show as before, marked partial.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::diff::ReportDiff;
use crate::format;
use crate::model::{ContextGap, PatternStats, ScanReport, TTL_BUCKET_LABELS, Ttl};
use crate::scan::context::{bias_warnings, big_keys_total, coverage};
use crate::tui::app::views::names::NamesState;
use crate::tui::app::{App, PatternData, Phase};
use crate::tui::clipboard::suggested_command;
use crate::tui::tree::{Root, Row, SortBy, Summary, Verdict, summarize, summarize_group};
use crate::verdict::{PatternVerdict, explain, pattern_verdict};

use super::delta::Change;
use super::widgets::{
    Pane, Title, bar, fit, marquee_spans, pad_l, pad_r, pattern_spans, rows, scroll_offset,
    split_frame, trunc, ttl_hist_spans, width,
};
use super::{CONNECT_FIRST, spinner, style};

/// The left pane's width, from the frame's left edge to the divider, on Browse › Keys:
/// 41 of 80 columns and 74 of 120, as in the mockups; with `v` the list takes more.
pub fn tree_width(total: u16, more: bool) -> u16 {
    let w = u32::from(total);
    let at = if more {
        w * 7 / 10
    } else if total >= 110 {
        w * 37 / 60
    } else {
        w * 41 / 80
    };
    super::widgets::split_at(total, f64::from(at) / f64::from(total.max(1)))
}

/// `text` for a pane `w` cells wide, broken at spaces: at most `w - 1` cells a line,
/// so the last cell stays free like the first. A leading `▲ ` or `✗ ` indents the
/// lines after the first by two.
pub fn wrap_words(text: &str, w: usize) -> Vec<String> {
    let w = w.saturating_sub(1).max(4);
    let body = text.trim_start_matches(' ');
    let lead = " ".repeat(text.len() - body.len());
    let hang = if body.starts_with("▲ ") || body.starts_with("✗ ") {
        format!("{lead}  ")
    } else {
        lead.clone()
    };
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in body.split(' ') {
        let start = if out.is_empty() { &lead } else { &hang };
        let candidate = if line.is_empty() {
            format!("{start}{word}")
        } else {
            format!("{line} {word}")
        };
        if width(&candidate) > w && !line.is_empty() {
            out.push(line);
            line = format!("{hang}{word}");
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.into_iter().map(|l| trunc(&l, w)).collect()
}

/// Lines of `text` wrapped to `w` in `st`.
fn para(text: &str, w: usize, st: Style) -> Vec<Line<'static>> {
    wrap_words(text, w)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, st)))
        .collect()
}

/// The left pane's body under its header row, less the bottom line.
fn list_area(inner: Rect) -> Rect {
    Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(2),
        ..inner
    }
}

/// The left pane's bottom line.
fn bottom_area(inner: Rect) -> Rect {
    Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: u16::from(inner.height >= 2),
        ..inner
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(data) = app.pattern_data() else {
        waiting(frame, area, app);
        return;
    };
    let rows_ = app.pattern_rows();
    let sel = app.patterns.sel.min(rows_.len().saturating_sub(1));
    let patterns = data.patterns;
    let summaries: Vec<(Summary, usize, Option<bool>)> = rows_
        .iter()
        .map(|r| row_summary(app, patterns, *r))
        .collect();
    let narrowed = app.narrowed_node().is_some();
    // Changes only for a finished analysis: a snapshot diff is of the final report.
    let diff = app
        .session
        .as_ref()
        .and_then(|s| s.diff())
        .filter(|_| data.partial.is_none() && !narrowed && !data.names);
    let verdicts: Vec<PatternVerdict> = rows_
        .iter()
        .map(|r| row_verdict(app, patterns, diff, *r))
        .collect();
    let more = app.more_here();
    let left_w = tree_width(area.width, more);
    let (left_title, left_right) = tree_titles(app, data, summaries.len(), left_w);
    let (right_title, right_right) = match summaries.get(sel) {
        Some((s, _, _)) if data.names => (crate::value::escape(&s.label), String::new()),
        Some((s, _, _)) => (crate::value::escape(&s.label), type_label(s).to_string()),
        None => ("Details".to_string(), String::new()),
    };
    let (left, right) = split_frame(
        frame,
        area,
        left_w,
        Title::new(&left_title, &left_right),
        Title::new(&right_title, &right_right),
        Pane::Left,
    );
    let changes: Option<Vec<Option<Change>>> = diff.map(|d| {
        rows_
            .iter()
            .map(|r| row_change(app, patterns, d, *r))
            .collect()
    });
    if data.names {
        names_table(frame, left, app, &summaries, sel);
        names_detail(frame, right, app, summaries.get(sel));
    } else {
        let c = cols(
            usize::from(left.width),
            more,
            changes.is_some(),
            data.sizes_available,
        );
        table(
            frame,
            left,
            app,
            data,
            &summaries,
            changes.as_deref(),
            &verdicts,
            sel,
            &c,
        );
        bottom_line(frame, left, app, data, &c);
        match summaries.get(sel) {
            Some((s, _, _)) => {
                let v = verdicts.get(sel).copied().unwrap_or(PatternVerdict::Ok);
                let stats = single_stats(app, patterns, rows_[sel]);
                detail(frame, right, data, s, v, stats, diff, narrowed);
            }
            None => no_rows(frame, right, app, data, narrowed),
        }
    }
    // The server context of a finished report, on the frame's bottom border under the
    // list, so it takes no row.
    if data.partial.is_none()
        && !data.names
        && !narrowed
        && let Some(report) = app.session.as_ref().and_then(|s| s.report.as_ref())
    {
        context_strip(frame, area, left_w, report);
    }
}

/// The left title and its right part: `Patterns` and `by memory`, the filter, a
/// partial marker, the node it is narrowed to. What the rows are during an analysis
/// (partial results, or the previous ones) comes first; when the pane, `left_w` wide,
/// has no room for it beside `Patterns`, it takes the title's place.
fn tree_titles(app: &App, data: PatternData<'_>, shown: usize, left_w: u16) -> (String, String) {
    let sizes = data.sizes_available;
    let sort = app.patterns.sort.effective(sizes);
    let mut rest = if !app.patterns.filter.is_empty() {
        format!("filter \"{}\" · {shown} rows", app.patterns.filter)
    } else {
        format!("by {}", sort.label())
    };
    let mut status = data.partial.map(|p| {
        format!(
            "partial · {} / {} sampled",
            format::grouped(p.sampled),
            format::grouped(p.target)
        )
    });
    if let Some(m) = super::previous_marker(app) {
        status = Some(match status {
            Some(st) => format!("{m} · {st}"),
            None => m.to_string(),
        });
    }
    let title = match app.narrowed_node() {
        Some((_, n)) => {
            rest = format!("esc for all nodes · {rest}");
            format!("Patterns · node {}", n.endpoint)
        }
        None => "Patterns".to_string(),
    };
    let Some(status) = status else {
        return (title, rest);
    };
    // `┌─ title ─ right ─┬`: the corners, dashes and spaces take 7.
    let room = usize::from(left_w).saturating_sub(7);
    let full = format!("{status} · {rest}");
    if width(&title) + width(&full) <= room {
        (title, full)
    } else if width(&title) + width(&status) <= room {
        (title, status)
    } else {
        (status, String::new())
    }
}

/// No rows to draw yet: not connected, connecting, or waiting for the live connection
/// to read key names. Failed connects say why.
fn waiting(frame: &mut Frame, area: Rect, app: &App) {
    let left_w = tree_width(area.width, false);
    let (left, right) = split_frame(
        frame,
        area,
        left_w,
        Title::new("Patterns", ""),
        Title::new("Details", ""),
        Pane::Left,
    );
    let w = usize::from(left.width);
    let spin = spinner(app.tick);
    let lines: Vec<Line<'static>> = match &app.session {
        None => para(CONNECT_FIRST, w, style::dim()),
        Some(s) => match &s.phase {
            Phase::Failed(e) => {
                let mut l = para(&format!(" ✗ {e}"), w, style::warn());
                l.extend(para(
                    " Press 0 to go back to Connections and try again.",
                    w,
                    style::dim(),
                ));
                l
            }
            Phase::Connecting => para(
                &format!(" {spin} Connecting to {}…", s.profile.name),
                w,
                style::dim(),
            ),
            _ => match &s.live {
                crate::tui::app::views::LiveState::Closed(why) => para(
                    &format!(
                        " ✗ The live connection is closed: {}. Connect again to read key names.",
                        why.as_deref().unwrap_or("it was closed")
                    ),
                    w,
                    style::warn(),
                ),
                _ => para(
                    &format!(" {spin} Waiting for the live connection…"),
                    w,
                    style::dim(),
                ),
            },
        },
    };
    rows(frame, left, lines, None);
    rows(frame, right, intro(usize::from(right.width)), None);
}

/// What Browse reads before an analysis, for the right pane.
fn intro(w: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::default()];
    out.extend(para(
        " Browse reads key names with SCAN, 1,000 a call, and groups them. Nothing else about a key is read until you open it.",
        w,
        style::fg(),
    ));
    out.push(Line::default());
    out.extend(para(
        " Sizes, TTLs and types per pattern come from an analysis: r samples 10,000 keys.",
        w,
        style::dim(),
    ));
    out
}

/// A key count from names: `~390K` estimated, exact once every name is read, and `1
/// seen` for a pattern that names one key while the walk goes on.
fn names_count(s: &Summary, names: &NamesState, complete: bool) -> String {
    if complete {
        format::count(s.est_keys)
    } else if names.is_literal(&s.label) {
        format!("{} seen", format::count(s.sampled_keys))
    } else {
        format!("~{}", format::count(s.est_keys))
    }
}

/// The names column of KEYS: wide enough for `~1.9M` and `1 seen`.
const NAMES_KEYS_W: usize = 6;
/// Space after the KEYS column, where an analysis puts its `▲`.
const NAMES_TAIL: usize = 4;

fn names_table(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    summaries: &[(Summary, usize, Option<bool>)],
    sel: usize,
) {
    let names = &app.patterns.names;
    let w = usize::from(inner.width);
    let name_w = w.saturating_sub(NAMES_KEYS_W + NAMES_TAIL).max(8);
    let arrow = |s: SortBy| {
        (app.patterns.sort.effective(false) == s).then_some(if s.ascending() {
            " ▲"
        } else {
            " ▼"
        })
    };
    let head = Line::from(vec![
        Span::styled(
            pad_r(
                &format!(" PATTERN{}", arrow(SortBy::Name).unwrap_or("")),
                name_w,
            ),
            style::hd(),
        ),
        Span::styled(pad_l("KEYS", NAMES_KEYS_W), style::hd()),
    ]);
    rows(frame, inner, vec![head], None);
    let body = list_area(inner);
    if summaries.is_empty() {
        let text = if !app.patterns.filter.is_empty() {
            format!(
                " No pattern matches \"{}\". esc clears the filter.",
                app.patterns.filter
            )
        } else if let Some(e) = &names.error {
            format!(" ▲ {e}")
        } else if names.done() {
            " No keys in this database.".to_string()
        } else {
            format!(" {} Reading key names with SCAN…", spinner(app.tick))
        };
        let st = if names.error.is_some() && app.patterns.filter.is_empty() {
            style::amber()
        } else {
            style::dim()
        };
        rows(frame, body, para(&text, w, st), None);
    } else {
        let complete = names.complete();
        let off = scroll_offset(sel, usize::from(body.height));
        let lines: Vec<Line<'static>> = summaries
            .iter()
            .enumerate()
            .skip(off)
            .take(usize::from(body.height))
            .map(|(i, (s, depth, open))| {
                let scroll = (i == sel).then(|| app.sel_ticks());
                let mut spans = name_cell(s, *depth, *open, name_w, scroll);
                spans.push(Span::styled(
                    pad_l(&names_count(s, names, complete), NAMES_KEYS_W),
                    style::fg(),
                ));
                Line::from(spans)
            })
            .collect();
        rows(frame, body, lines, Some(sel - off));
    }
    let (text, st) = names_bottom(app, names);
    rows(
        frame,
        bottom_area(inner),
        vec![Line::from(fit(vec![Span::styled(text, st)], w))],
        None,
    );
}

/// The names tree's bottom line: how many names were read, and that `n` reads more.
fn names_bottom(app: &App, names: &NamesState) -> (String, Style) {
    let read = format::grouped(names.names_read());
    if names.is_reading() {
        return (
            format!(" {} {read} names read, reading more…", spinner(app.tick)),
            style::dim(),
        );
    }
    if app.analysis_running() {
        return (
            format!(" {read} names read · analyzing, results follow"),
            style::dim(),
        );
    }
    if let Some(e) = &names.error
        && names.names_read() > 0
    {
        return (format!(" ▲ {e} n tries again."), style::amber());
    }
    let nodes = names
        .nodes_done()
        .filter(|(d, n)| d < n && !names.done())
        .map(|(d, n)| format!(" · {d} of {n} nodes done"))
        .unwrap_or_default();
    if names.complete() {
        (format!(" All {read} names read"), style::dim())
    } else if names.done() {
        (
            format!(" {read} names read · some nodes left out"),
            style::amber(),
        )
    } else {
        (
            format!(" {read} names read{nodes} · n reads more"),
            style::dim(),
        )
    }
}

/// The right pane before an analysis: the estimate of the selected row, what Browse
/// reads, what an analysis adds, and what `→` does.
fn names_detail(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    row: Option<&(Summary, usize, Option<bool>)>,
) {
    let w = usize::from(inner.width);
    let names = &app.patterns.names;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let Some((s, _, open)) = row else {
        rows(frame, inner, intro(w), None);
        return;
    };
    let complete = names.complete();
    let group = open.is_some();
    let keys = if complete {
        format!(
            " {} {} (every name read)",
            format::grouped(s.est_keys),
            if s.est_keys == 1 { "key" } else { "keys" }
        )
    } else if names.is_literal(&s.label) && !group {
        " One key: its name has no part that varies".to_string()
    } else {
        format!(
            " ~{} keys (estimated from names)",
            format::count(s.est_keys)
        )
    };
    lines.extend(para(&keys, w, style::fgb()));
    if group && let Verdict::Group { children, .. } = s.verdict {
        lines.push(Line::from(Span::styled(
            format!(
                " {children} patterns · {} names seen",
                format::grouped(s.sampled_keys)
            ),
            style::dim(),
        )));
    } else if !complete {
        lines.push(Line::from(Span::styled(
            format!(
                " {} of {} names read",
                format::grouped(s.sampled_keys),
                format::grouped(names.names_read())
            ),
            style::dim(),
        )));
    }
    lines.push(Line::default());
    lines.extend(para(
        " Browse reads key names with SCAN, 1,000 a call, and groups them. Nothing else about a key is read until you open it.",
        w,
        style::fg(),
    ));
    lines.push(Line::default());
    lines.extend(para(
        " Sizes, TTLs and types per pattern come from an analysis: r samples 10,000 keys.",
        w,
        style::dim(),
    ));
    for &i in names.missing() {
        if let Some(n) = app.session.as_ref().and_then(|s| s.nodes.get(i)) {
            lines.push(Line::default());
            lines.extend(para(
                &format!(
                    " ▲ {}: not connected, so its key names are not read.",
                    n.endpoint
                ),
                w,
                style::amber(),
            ));
        }
    }
    for &i in &names.skipped {
        if let Some(n) = app.session.as_ref().and_then(|s| s.nodes.get(i)) {
            lines.push(Line::default());
            lines.extend(para(
                &format!(
                    " ▲ {}: {}",
                    n.endpoint,
                    crate::tui::app::views::names::NODE_SKIPPED
                ),
                w,
                style::amber(),
            ));
        }
    }
    if !names.refused.is_empty() {
        lines.push(Line::default());
        lines.extend(para(
            &format!(" ▲ {}", crate::tui::app::views::names::SCAN_REFUSED),
            w,
            style::amber(),
        ));
    }
    lines.push(Line::default());
    let open_text = if group {
        " → expands the group, ← collapses it"
    } else {
        " → lists its keys"
    };
    lines.push(Line::from(Span::styled(open_text, style::dim())));
    rows(frame, inner, lines, None);
}

/// The context parts the bottom border shows: a sampling-bias warning and a missing
/// `MEMORY STATS` first (so a narrow strip cuts the rest), how much of
/// `dataset.bytes` the sample explains, the exact key count and TTL share, and big
/// keys from `INFO keysizes`. Empty without a server context.
pub(super) fn context_parts(report: &ScanReport) -> Vec<Vec<Span<'static>>> {
    let Some(ctx) = &report.server_context else {
        return Vec::new();
    };
    // Warnings and what is missing first: on a narrow list the strip is cut from the
    // right.
    let mut parts: Vec<Vec<Span<'static>>> = Vec::new();
    if !bias_warnings(report).is_empty() {
        parts.push(vec![Span::styled(
            "▲ possible sampling bias (keysizes)",
            style::warn(),
        )]);
    }
    for gap in &ctx.missing {
        match gap {
            ContextGap::MemoryStatsDenied => {
                parts.push(vec![Span::styled("no MEMORY STATS: denied", style::dim())])
            }
            ContextGap::MemoryStatsUnavailable => parts.push(vec![Span::styled(
                "no MEMORY STATS on this server",
                style::dim(),
            )]),
            _ => {}
        }
    }
    if let Some((ratio, dataset)) = coverage(report) {
        parts.push(vec![
            Span::styled("sample explains ", style::dim()),
            Span::styled(format::percent(ratio), style::fgb()),
            Span::styled(
                format!(" of dataset.bytes {}", format::bytes(dataset)),
                style::dim(),
            ),
        ]);
    } else if let Some(d) = ctx.memory.and_then(|m| m.dataset_bytes) {
        parts.push(vec![
            Span::styled("dataset.bytes ", style::dim()),
            Span::styled(format::bytes(d), style::fg()),
        ]);
    }
    if let Some(k) = &ctx.keyspace {
        let mut spans = vec![
            Span::styled("exact keys ", style::dim()),
            Span::styled(format::grouped(k.keys), style::fg()),
        ];
        if k.keys > 0 {
            spans.push(Span::styled(
                format!(
                    " · {} with TTL",
                    format::percent(k.expires as f64 / k.keys as f64)
                ),
                style::dim(),
            ));
        }
        parts.push(spans);
    }
    if let Some(h) = &ctx.keysizes {
        let big = big_keys_total(h);
        if big > 0 {
            parts.push(vec![Span::styled(
                format!(
                    "{} big {} (INFO keysizes)",
                    format::grouped(big),
                    plural(big, "key", "keys")
                ),
                style::amber(),
            )]);
        }
    }
    parts
}

/// [`context_parts`] on the bottom border under the list (`left_w` cells from the
/// frame's left edge), cut with `…` when narrow.
fn context_strip(frame: &mut Frame, area: Rect, left_w: u16, report: &ScanReport) {
    let parts = context_parts(report);
    if parts.is_empty() || area.height < 2 || left_w < 12 {
        return;
    }
    let mut spans = vec![Span::styled(" ", style::border())];
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", style::dim()));
        }
        spans.extend(p);
    }
    spans.push(Span::styled(" ", style::border()));
    // Two border cells stay on the left, and the divider's `┴` with one before it.
    let room = usize::from(left_w).saturating_sub(4);
    let total: usize = spans.iter().map(|s| width(&s.content)).sum();
    let w = total.min(room);
    let spans = if total > room {
        fit(spans, room)
    } else {
        spans
    };
    let strip = Rect {
        x: area.x + 2,
        y: area.y + area.height - 1,
        width: w as u16,
        height: 1,
    };
    frame.render_widget(Line::from(spans), strip);
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

/// The verdict of a row (spec "1 Patterns, additions"): a pattern's own, and for a
/// group the most worrying of its children's. `Ok` for rows from names.
fn row_verdict(
    app: &App,
    patterns: &[PatternStats],
    d: Option<&ReportDiff>,
    row: Row,
) -> PatternVerdict {
    if app.pattern_data().is_some_and(|d| d.names) {
        return PatternVerdict::Ok;
    }
    let of = |i: usize| {
        let p = &patterns[i];
        pattern_verdict(p, d.and_then(|d| d.pattern(&p.pattern)))
    };
    match row {
        Row::Group { root } => match &app.patterns.tree.roots[root] {
            Root::Group(g) => g
                .children
                .iter()
                .map(|&i| of(i))
                .min()
                .unwrap_or(PatternVerdict::Ok),
            Root::Single(i) => of(*i),
        },
        Row::Pattern { idx, .. } => of(idx),
    }
}

/// True for the verdicts a row is marked `▲` for: something to look at. A pattern whose
/// keys never expire and that does not grow looks intentional, so it is not marked.
pub fn flagged(v: PatternVerdict) -> bool {
    !matches!(v, PatternVerdict::Ok | PatternVerdict::Persistent)
}

fn mark_style(v: PatternVerdict) -> Style {
    if v.is_warning() {
        style::warn()
    } else {
        style::amber()
    }
}

/// The pattern a row stands for when it is one pattern (not a group).
fn single_stats<'a>(app: &App, patterns: &'a [PatternStats], row: Row) -> Option<&'a PatternStats> {
    match row {
        Row::Group { root } => match app.patterns.tree.roots.get(root)? {
            Root::Single(i) => patterns.get(*i),
            Root::Group(_) => None,
        },
        Row::Pattern { idx, .. } => patterns.get(idx),
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

/// The columns of the analysed table. A hidden column is 0 or false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cols {
    name: usize,
    ty: bool,
    mem: bool,
    /// The share bar's cells (and its percentage); 0 when hidden.
    share: usize,
    avg: bool,
    no_ttl: bool,
    delta: bool,
    /// Wide enough for the long bottom line.
    wide: bool,
}

const TYPE_W: usize = 7;
const NUM_W: usize = 6;
/// MEMORY and NO TTL: their headers, with a sort arrow, are seven wide.
const WIDE_NUM_W: usize = 7;
const PCT_W: usize = 4;
const DELTA_W: usize = 7;
/// The `▲` mark: a space, the mark, a space.
const MARK_W: usize = 3;
/// The narrowest the pattern column gets before an optional column goes.
const NAME_MIN: usize = 20;
/// From this inner width on, TYPE and the share bar show without `v`.
const WIDE: usize = 60;

impl Cols {
    /// Everything but the pattern column.
    fn fixed(&self) -> usize {
        let mut w = NUM_W + MARK_W;
        if self.ty {
            w += TYPE_W;
        }
        if self.mem {
            w += 1 + WIDE_NUM_W;
        }
        if self.share > 0 {
            w += 1 + self.share + 1 + PCT_W;
        }
        if self.avg {
            w += 1 + NUM_W;
        }
        if self.no_ttl {
            w += 1 + WIDE_NUM_W;
        }
        if self.delta {
            w += 1 + DELTA_W;
        }
        w
    }
}

/// The columns for a list `inner` cells wide: PATTERN, KEYS, MEMORY and `▲` always;
/// TYPE and SHARE from [`WIDE`] on or with `v`; AVG, NO TTL and Δ (with a snapshot to
/// compare with) with `v`. When the pattern column would get narrower than
/// [`NAME_MIN`] the optional columns go: the share bar, Δ, AVG, TYPE, NO TTL.
fn cols(inner: usize, more: bool, delta: bool, sizes: bool) -> Cols {
    let wide = inner >= WIDE;
    let mut c = Cols {
        name: 0,
        ty: wide || more,
        mem: sizes,
        share: if wide || more {
            if inner >= 90 { 14 } else { 12 }
        } else {
            0
        },
        avg: more && sizes,
        no_ttl: more,
        delta: more && delta,
        wide,
    };
    let fits = |c: &Cols| inner >= c.fixed() + NAME_MIN;
    if !fits(&c) && c.share > 8 {
        c.share = 8;
    }
    for step in 0..5 {
        if fits(&c) {
            break;
        }
        match step {
            0 => c.share = 0,
            1 => c.delta = false,
            2 => c.avg = false,
            3 => c.ty = false,
            _ => c.no_ttl = false,
        }
    }
    c.name = inner.saturating_sub(c.fixed()).max(8);
    c
}

/// A right-aligned header `w` wide, with the sort arrow when sorted by it.
fn head_num(label: &str, w: usize, arrow: Option<&str>) -> String {
    match arrow {
        Some(a) => pad_l(&format!("{label}{a}"), w),
        None => pad_l(label, w),
    }
}

fn type_label(s: &Summary) -> &'static str {
    s.key_type.map_or("mixed", |t| t.as_str())
}

#[allow(clippy::too_many_arguments)]
fn table(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    data: PatternData<'_>,
    summaries: &[(Summary, usize, Option<bool>)],
    changes: Option<&[Option<Change>]>,
    verdicts: &[PatternVerdict],
    sel: usize,
    c: &Cols,
) {
    let sizes = data.sizes_available;
    let sort = app.patterns.sort.effective(sizes);
    let arrow = |s: SortBy| (sort == s).then_some(if s.ascending() { "▲" } else { "▼" });
    let pattern_head = match arrow(SortBy::Name) {
        Some(a) => format!(" PATTERN {a}"),
        None => " PATTERN".to_string(),
    };
    let mut head = vec![Span::styled(pad_r(&pattern_head, c.name), style::hd())];
    if c.ty {
        head.push(Span::styled(pad_r("TYPE", TYPE_W), style::hd()));
    }
    head.push(Span::styled(
        head_num("KEYS", NUM_W, arrow(SortBy::Keys)),
        style::hd(),
    ));
    if c.mem {
        head.push(Span::styled(
            format!(" {}", head_num("MEMORY", WIDE_NUM_W, arrow(SortBy::Memory))),
            style::hd(),
        ));
    }
    if c.share > 0 {
        head.push(Span::styled(
            format!(" {}", pad_r("SHARE", c.share + 1 + PCT_W)),
            style::hd(),
        ));
    }
    if c.avg {
        head.push(Span::styled(
            format!(" {}", head_num("AVG", NUM_W, arrow(SortBy::AvgSize))),
            style::hd(),
        ));
    }
    if c.no_ttl {
        head.push(Span::styled(
            format!(" {}", head_num("NO TTL", WIDE_NUM_W, arrow(SortBy::NoTtl))),
            style::hd(),
        ));
    }
    if c.delta {
        head.push(Span::styled(
            format!(" {}", pad_l("Δ", DELTA_W)),
            style::hd(),
        ));
    }
    rows(frame, inner, vec![Line::from(head)], None);
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
    let body = list_area(inner);
    if summaries.is_empty() {
        let text = if !app.patterns.filter.is_empty() {
            format!(
                " No pattern matches \"{}\". esc clears the filter.",
                app.patterns.filter
            )
        } else if data.partial.is_some() {
            " No keys sampled yet.".to_string()
        } else {
            " No keys were sampled.".to_string()
        };
        rows(
            frame,
            body,
            para(&text, usize::from(inner.width), style::dim()),
            None,
        );
        return;
    }
    let off = scroll_offset(sel, usize::from(body.height));
    let lines: Vec<Line<'static>> = summaries
        .iter()
        .enumerate()
        .skip(off)
        .take(usize::from(body.height))
        .map(|(i, (s, depth, open))| {
            let scroll = (i == sel).then(|| app.sel_ticks());
            let ch = changes.and_then(|ch| ch.get(i).copied().flatten());
            let v = verdicts.get(i).copied().unwrap_or(PatternVerdict::Ok);
            analysed_row(s, *depth, *open, total, sizes, c, scroll, ch, v)
        })
        .collect();
    rows(frame, body, lines, Some(sel - off));
}

/// The pattern cell: the indent and fold mark, then the name (the selected one
/// scrolls when it is too long), `w` cells.
fn name_cell(
    s: &Summary,
    depth: usize,
    open: Option<bool>,
    w: usize,
    scroll: Option<u64>,
) -> Vec<Span<'static>> {
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
    let text_w = w.saturating_sub(1).saturating_sub(width(&lead));
    // A pattern keeps the literal parts of key names: control characters escaped.
    let text = pattern_spans(&crate::value::escape(&s.label), base, ph);
    let mut spans = vec![Span::styled(lead, style::dim())];
    spans.extend(match scroll {
        Some(ticks) => marquee_spans(text, text_w, ticks),
        None => fit(text, text_w),
    });
    let used: usize = spans.iter().map(|s| width(&s.content)).sum();
    spans.push(Span::raw(" ".repeat(w.saturating_sub(used))));
    spans
}

#[allow(clippy::too_many_arguments)]
fn analysed_row(
    s: &Summary,
    depth: usize,
    open: Option<bool>,
    total: u64,
    sizes: bool,
    c: &Cols,
    scroll: Option<u64>,
    change: Option<Change>,
    v: PatternVerdict,
) -> Line<'static> {
    let mut spans = name_cell(s, depth, open, c.name, scroll);
    let dash = || "—".to_string();
    if c.ty {
        spans.push(Span::styled(pad_r(type_label(s), TYPE_W), style::dim()));
    }
    spans.push(Span::styled(
        pad_l(&format::count(s.est_keys), NUM_W),
        style::fg(),
    ));
    if c.mem {
        let mem = s.est_bytes.map_or_else(dash, format::bytes);
        spans.push(Span::styled(
            format!(" {}", pad_l(&mem, WIDE_NUM_W)),
            style::fg(),
        ));
    }
    if c.share > 0 {
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
        spans.push(Span::raw(" "));
        spans.push(Span::styled(fill, style::amber()));
        spans.push(Span::styled(track, style::track()));
        spans.push(Span::styled(
            format!(" {}", pad_l(&format::percent(share), PCT_W)),
            style::dim(),
        ));
    }
    if c.avg {
        let avg = s.avg_bytes().map_or_else(dash, format::bytes);
        let st = match s.verdict {
            Verdict::Large(_) => style::amber(),
            _ => style::fg(),
        };
        spans.push(Span::styled(format!(" {}", pad_l(&avg, NUM_W)), st));
    }
    if c.no_ttl {
        let pct = format::percent(s.no_ttl_ratio());
        let st = match s.verdict {
            Verdict::MixedTtl(_) => style::warn(),
            _ if s.no_ttl == 0 || s.no_ttl == s.sampled_keys => style::dim(),
            _ => style::fg(),
        };
        spans.push(Span::styled(format!(" {}", pad_l(&pct, WIDE_NUM_W)), st));
    }
    if c.delta {
        spans.push(match change {
            Some(d) => Span::styled(format!(" {}", pad_l(&d.text(), DELTA_W)), d.style()),
            None => Span::styled(format!(" {}", pad_l("—", DELTA_W)), style::dim()),
        });
    }
    if flagged(v) {
        spans.push(Span::styled(" ▲ ", mark_style(v)));
    } else {
        spans.push(Span::raw("   "));
    }
    Line::from(spans)
}

/// The analysed list's bottom line: how many keys the analysis sampled, and what `v`
/// does.
fn bottom_line(frame: &mut Frame, inner: Rect, app: &App, data: PatternData<'_>, c: &Cols) {
    let w = usize::from(inner.width);
    let v = if app.more_here() { "fewer" } else { "more" };
    let text = if let Some(p) = data.partial {
        format!(
            " partial: {} of {} sampled so far",
            format::grouped(p.sampled),
            format::grouped(p.target)
        )
    } else if let Some(r) = app.report() {
        let sampled = format::grouped(r.sampled_keys);
        let of = app
            .session
            .as_ref()
            .and_then(|s| s.server.as_ref())
            .map(|s| s.total_keys)
            .unwrap_or(r.server.total_keys);
        if c.wide {
            format!(
                " Estimated from {sampled} sampled keys of {}. v shows {v} columns.",
                format::count(of)
            )
        } else {
            format!(" from {sampled} sampled keys · v {v}")
        }
    } else {
        String::new()
    };
    rows(
        frame,
        bottom_area(inner),
        vec![Line::from(fit(vec![Span::styled(text, style::dim())], w))],
        None,
    );
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// Why a row is marked, in a sentence or two; for a row that is not, what its TTLs
/// say. `stats` is the pattern of a row that is one pattern.
fn reason(
    s: &Summary,
    v: PatternVerdict,
    stats: Option<&PatternStats>,
    diff: Option<&ReportDiff>,
) -> (String, Style) {
    if let Verdict::Group { children, .. } = s.verdict {
        return if flagged(v) {
            (
                format!(
                    "▲ Some of its {children} patterns have something to look at. → expands the group."
                ),
                mark_style(v),
            )
        } else {
            (
                format!("{children} patterns, nothing marked. → expands, ← collapses."),
                style::dim(),
            )
        };
    }
    let Some(p) = stats else {
        return (String::new(), style::dim());
    };
    let change = diff.and_then(|d| d.pattern(&p.pattern));
    let said = explain(p, change).join(" ");
    match v {
        PatternVerdict::Leak => (
            "▲ Lock keys that never expire: a holder that crashed never lets go of them."
                .to_string(),
            mark_style(v),
        ),
        PatternVerdict::BigKey => (
            format!(
                "▲ A sampled key is {}. A big key blocks the server while it is read, copied or deleted.",
                p.size_max
                    .map_or_else(|| "very large".to_string(), format::bytes)
            ),
            mark_style(v),
        ),
        PatternVerdict::Growing | PatternVerdict::Mixed => (format!("▲ {said}"), mark_style(v)),
        PatternVerdict::Persistent | PatternVerdict::Ok => (said, style::dim()),
    }
}

/// The right pane after an analysis: the row's keys, memory and share, the typical
/// size, why it is marked, its expiry, example keys, the command `Y` copies, and what
/// `→` does.
#[allow(clippy::too_many_arguments)]
fn detail(
    frame: &mut Frame,
    inner: Rect,
    data: PatternData<'_>,
    s: &Summary,
    v: PatternVerdict,
    stats: Option<&PatternStats>,
    diff: Option<&ReportDiff>,
    narrowed: bool,
) {
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    let wide = w >= 40;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let total_bytes: u64 = data.patterns.iter().filter_map(|p| p.est_bytes).sum();
    let mut facts = format!(
        " {} {}",
        format::count(s.est_keys),
        plural(s.est_keys, "key", "keys")
    );
    match s.est_bytes {
        Some(b) if data.sizes_available => {
            facts.push_str(&format!(" · {}", format::bytes(b)));
            if total_bytes > 0 {
                facts.push_str(&format!(
                    " · {} of memory",
                    format::percent(b as f64 / total_bytes as f64)
                ));
            }
        }
        _ => facts.push_str(" · no sizes (MEMORY USAGE not allowed)"),
    }
    lines.push(Line::from(fit(vec![Span::styled(facts, style::fgb())], w)));
    if data.sizes_available {
        let typical = s.size_p50.or_else(|| s.avg_bytes()).map(format::bytes);
        let largest = s.size_max.map(format::bytes);
        let mut size_lines: Vec<String> = Vec::new();
        match (typical, largest, wide) {
            (Some(t), Some(l), false) => size_lines.push(format!(" Typical key {t}, largest {l}")),
            (Some(t), l, true) => {
                let mut first = format!(" Typical key {t}");
                if let Some(p99) = s.size_p99 {
                    first.push_str(&format!(" · 99% under {}", format::bytes(p99)));
                }
                size_lines.push(first);
                if let Some(l) = l {
                    size_lines.push(format!(" Largest {l}"));
                }
            }
            (Some(t), None, false) => size_lines.push(format!(" Typical key {t}")),
            (None, Some(l), _) => size_lines.push(format!(" Largest {l}")),
            (None, None, _) => {}
        }
        for l in size_lines {
            lines.push(Line::from(fit(vec![Span::styled(l, style::fg())], w)));
        }
    }
    let (why, why_style) = reason(s, v, stats, diff);
    if !why.is_empty() {
        lines.push(Line::default());
        lines.extend(para(&format!(" {why}"), w, why_style));
    }
    // Expiry: the TTL buckets of the sample.
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(" EXPIRES IN", style::hd())));
    lines.push(Line::from(fit(
        vec![Span::styled(
            format!(
                " {}",
                TTL_BUCKET_LABELS
                    .iter()
                    .map(|l| pad_r(l, 5))
                    .collect::<String>()
            ),
            style::dim(),
        )],
        w,
    )));
    let none = if matches!(s.verdict, Verdict::MixedTtl(_)) {
        style::warn()
    } else {
        style::amber()
    };
    let mut hist = vec![Span::raw(" ")];
    hist.extend(ttl_hist_spans(&s.ttl_hist, none));
    lines.push(Line::from(fit(hist, w)));
    // What comes after the examples: the command, and what `→` does.
    let open = if s.is_group() {
        " → expands the group"
    } else if wide {
        " → lists its keys   y copies the pattern"
    } else {
        " → lists its keys"
    };
    let cmd = suggested_command(&s.label).filter(|_| wide);
    let after = 2 + cmd
        .as_ref()
        .map_or(0, |c| 1 + wrap_command(c, w.saturating_sub(3)).len());
    let room = h.saturating_sub(lines.len() + 2 + after);
    let examples = s.examples.len().min(room).min(5);
    if narrowed && s.examples.is_empty() {
        lines.push(Line::default());
        lines.extend(para(
            " Example keys are on the merged rows: esc shows every node.",
            w,
            style::dim(),
        ));
    } else if examples > 0 {
        let kw = w.saturating_sub(1 + NUM_W + 2 + 8 + 1).max(8);
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", pad_r("EXAMPLE KEYS", kw)), style::hd()),
            Span::styled(pad_l("SIZE", NUM_W), style::hd()),
            Span::styled(format!("  {}", pad_l("EXPIRES", 8)), style::hd()),
        ]));
        let mixed = matches!(s.verdict, Verdict::MixedTtl(_));
        for e in s.examples.iter().take(examples) {
            let (ttl, st) = match e.ttl {
                Ttl::Persistent => (
                    "never".to_string(),
                    if mixed { style::warn() } else { style::dim() },
                ),
                Ttl::Expires(d) => (format::duration(d), style::fg()),
            };
            let size = e.bytes.map_or_else(|| "—".to_string(), format::bytes);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {}", pad_r(&crate::value::escape(&e.key), kw)),
                    style::fg(),
                ),
                Span::styled(pad_l(&size, NUM_W), style::fg()),
                Span::styled(format!("  {}", pad_l(&ttl, 8)), st),
            ]));
        }
    }
    if let Some(cmd) = cmd {
        // Whole, so what is shown is what `Y` copies: broken at spaces, under the `$`.
        lines.push(Line::default());
        for (i, part) in wrap_command(&cmd, w.saturating_sub(3))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 { " $ " } else { "   " };
            lines.push(Line::from(vec![
                Span::styled(lead, style::dim()),
                Span::styled(part, style::cmd()),
            ]));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(open, style::dim())));
    let lines = lines
        .into_iter()
        .map(|l| Line::from(fit(l.spans, w)))
        .collect();
    rows(frame, inner, lines, None);
}

/// `cmd` in lines at most `w` cells wide, broken at spaces; a word wider than `w` is
/// cut into pieces. Joined with spaces, the lines are `cmd` again.
fn wrap_command(cmd: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in cmd.split(' ') {
        let fits = if line.is_empty() {
            width(word) <= w
        } else {
            width(&line) + 1 + width(word) <= w
        };
        if fits {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        let mut piece = String::new();
        for c in word.chars() {
            if width(&piece) + super::widgets::char_width(c) > w {
                out.push(std::mem::take(&mut piece));
            }
            piece.push(c);
        }
        line = piece;
    }
    out.push(line);
    out
}

/// The right pane with no row selected: a filter matched nothing, or no key was
/// sampled.
fn no_rows(frame: &mut Frame, inner: Rect, app: &App, data: PatternData<'_>, narrowed: bool) {
    let text = if !app.patterns.filter.is_empty() {
        format!(
            " No pattern matches \"{}\". Press esc to clear the filter.",
            app.patterns.filter
        )
    } else if data.partial.is_some() {
        " No keys sampled yet.".to_string()
    } else if narrowed {
        " No keys were sampled on this node. Press esc to see every node.".to_string()
    } else {
        " No keys were sampled.".to_string()
    };
    let mut lines = vec![Line::default()];
    lines.extend(para(&text, usize::from(inner.width), style::dim()));
    rows(frame, inner, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_takes_41_of_80_and_74_of_120_columns_and_70_percent_with_v() {
        assert_eq!(tree_width(80, false), 41);
        assert_eq!(tree_width(120, false), 74);
        assert_eq!(tree_width(120, true), 84);
    }

    #[test]
    fn at_80_columns_only_pattern_keys_memory_and_the_mark_show() {
        let c = cols(39, false, true, true);
        assert!(c.mem && !c.ty && c.share == 0 && !c.avg && !c.no_ttl && !c.delta);
        assert!(c.name >= NAME_MIN, "{}", c.name);
        // From [`WIDE`] on, TYPE and the share bar join in.
        let c = cols(72, false, true, true);
        assert!(c.ty && c.share > 0 && !c.avg && !c.no_ttl && !c.delta);
        // `v`: the rest, the share bar going first when room runs out.
        let c = cols(82, true, true, true);
        assert!(c.ty && c.avg && c.no_ttl && c.delta);
        assert!(c.name >= NAME_MIN);
        let c = cols(54, true, true, true);
        assert_eq!(c.share, 0);
        assert!(c.name >= NAME_MIN);
        // Without sizes there is no MEMORY or AVG to show.
        let c = cols(82, true, false, false);
        assert!(!c.mem && !c.avg);
    }

    #[test]
    fn a_long_command_is_broken_at_spaces_and_rejoins_whole() {
        let cmd = r#"redis-cli --scan --pattern "cache:search:*""#;
        let lines = wrap_command(cmd, 30);
        assert_eq!(lines, ["redis-cli --scan --pattern", r#""cache:search:*""#]);
        assert_eq!(lines.join(" "), cmd);
        // A word wider than the line is cut into pieces.
        let long = "x".repeat(25);
        let lines = wrap_command(&long, 10);
        assert!(lines.iter().all(|l| width(l) <= 10), "{lines:?}");
        assert_eq!(lines.concat(), long);
    }

    #[test]
    fn wrapping_keeps_the_last_cell_free_and_hangs_under_a_mark() {
        let lines = wrap_words(" ▲ one two three four five six", 16);
        assert!(lines.iter().all(|l| width(l) <= 15), "{lines:?}");
        assert!(lines[0].starts_with(" ▲ "));
        assert!(lines[1..].iter().all(|l| l.starts_with("   ")), "{lines:?}");
    }

    #[test]
    fn a_mark_shows_for_anything_but_ok_and_persistent() {
        assert!(!flagged(PatternVerdict::Ok));
        assert!(!flagged(PatternVerdict::Persistent));
        assert!(flagged(PatternVerdict::BigKey));
        assert!(flagged(PatternVerdict::Leak));
    }
}
