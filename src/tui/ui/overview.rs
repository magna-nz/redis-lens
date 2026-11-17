//! 1 Overview: "Now" (the live figures of the Server view's Overview poll, and on a
//! cluster one row per shard) above "Keys" (what the server says about its keys, then
//! what an analysis found), and the "Analyze the keys?" dialog.
//!
//! [`draw`] is the one entry point `ui::draw` calls for the Overview screen, and
//! [`draw_analyze`] the dialog's.
//!
//! "Keys" is built from blocks (a heading and its rows) that are fitted to the height:
//! a block is shown only with at least one of its rows, so a heading never stands
//! alone, and the rows left over go to the blocks in order.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::analysis::{Severity, expiry};
use crate::format;
use crate::live::server::{InfoSample, Role};
use crate::model::{NodeRole, Profile, ScanReport};
use crate::tui::app::views::overview::{
    AnalyzeDialog, Depth, Estimate, Finding, about_time, at_utc, estimate_for, findings,
    off_reasons, shows_analysis, stopped_early, time_left,
};
use crate::tui::app::views::server::{HISTORY, Metric, SHARDS_EVERY, sparkline};
use crate::tui::app::views::{Ctx, LiveState};
use crate::tui::app::{App, Phase, Session};

use super::widgets::{
    Section, Title, bar, dialog, fit, pad_l, pad_r, rows, scroll_offset, sections, trunc,
    wrap_spans,
};
use super::{CONNECT_FIRST, hh_mm, server, server_label, spinner, style};

/// The six figures "Now" shows; `v` shows all of [`Metric::ALL`].
/// The grid's six: memory is not among them, it is the bar line under the grid.
const SHORT: [Metric; 6] = [
    Metric::Ops,
    Metric::Clients,
    Metric::HitRate,
    Metric::Evicted,
    Metric::Expired,
    Metric::Fragmentation,
];

const LABEL_W: usize = 10;
const VALUE_W: usize = 7;
/// Patterns named under "Where the memory is"; the rest share one row.
const TOP_PATTERNS: usize = 3;
/// From this width on, "Keys" also shows what TTLs free.
const WIDE: usize = 100;
/// From this body width on, Overview draws two panes under "Now": the findings at the
/// left, the memory at the right.
pub const TWO_PANES: u16 = 120;
/// The panes' rows at the least (borders included); with fewer, one column.
const PANES_MIN_H: u16 = 8;
/// The left pane's share of the width.
const THINGS_SHARE: f64 = 0.55;
/// Slow-log entries "SLOWEST IN THE SLOW LOG" names at most.
const SLOWEST: usize = 3;
/// Figures "Now" puts on a row when it is this wide; narrower, two.
const GRID_3_AT: usize = 72;

/// Draws Overview into `area`, the body between the area row and the footer.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ctx) = app.ctx() else {
        let inner = sections(
            frame,
            area,
            &[Section {
                title: Title::new("Overview", ""),
                rows: None,
            }],
        );
        let line = Line::from(Span::styled(CONNECT_FIRST, style::dim()));
        rows(frame, inner[0], vec![Line::default(), line], None);
        return;
    };
    let w = usize::from(area.width.saturating_sub(2));
    let now = now_lines(&ctx, app, w);
    let now_rows = u16::try_from(now.len()).unwrap_or(u16::MAX);
    let (now_left, now_right) = now_titles(&ctx, app);
    // From 120 columns, two panes under "Now", when there is room for them.
    if area.width >= TWO_PANES
        && two_pane_state(ctx.session)
        && area.height >= now_rows.saturating_add(2 + PANES_MIN_H)
    {
        two_panes(frame, area, app, ctx.session, now, (&now_left, &now_right));
        return;
    }
    // The frame's top border, the divider and the bottom border.
    let keys_h = usize::from(area.height.saturating_sub(3).saturating_sub(now_rows));
    let (keys, keys_right) = keys_lines(app, ctx.session, w, keys_h);
    // The keys move the findings in "Keys": that section has the focus.
    let inner = super::widgets::sections_focused(
        frame,
        area,
        &[
            Section {
                title: Title::new(&now_left, &now_right),
                rows: Some(now_rows),
            },
            Section {
                title: Title::new("Keys", &keys_right),
                rows: None,
            },
        ],
        1,
    );
    rows(frame, inner[0], now, None);
    rows(frame, inner[1], keys, None);
}

/// "Now" on its own, or on a cluster the picked node (`[` `]` pick another); on the
/// right how often it polls, or that it is paused or off.
fn now_titles(ctx: &Ctx<'_>, app: &App) -> (String, String) {
    let state = &app.views.server;
    let every = server::secs(state.interval());
    let cluster = ctx.session.is_cluster();
    let left = match ctx.node_title() {
        Some(n) if cluster => n,
        _ => "Now".to_string(),
    };
    let right = if state.overview.poll.off.is_some() {
        "off".to_string()
    } else if state.paused {
        "INFO paused · space resumes".to_string()
    } else if cluster {
        format!("[ ] picks a node · INFO every {every}")
    } else {
        format!("INFO every {every} · space pauses")
    };
    (left, right)
}

/// The "Now" lines: the figures two to a row, the server line, and on a cluster a row
/// per shard from the per-shard poll.
fn now_lines(ctx: &Ctx<'_>, app: &App, w: usize) -> Vec<Line<'static>> {
    let state = &app.views.server;
    let o = &state.overview;
    if let Some(off) = &o.poll.off {
        return vec![Line::from(Span::styled(
            trunc(&format!(" ▲ {off}"), w),
            style::warning(),
        ))];
    }
    let mut lines = Vec::new();
    if !o.poll.got {
        let text = match ctx.live() {
            LiveState::Closed(why) => format!(
                " ▲ {}",
                why.as_deref().unwrap_or("The live connection is closed.")
            ),
            LiveState::Waiting => " Waiting for the live connection…".to_string(),
            LiveState::Ready => match &o.poll.failed {
                Some(f) => format!(" ▲ {f}"),
                None => " Waiting for the server…".to_string(),
            },
        };
        lines.push(Line::from(Span::styled(trunc(&text, w), style::dim())));
    } else {
        if let Some(t) = server::trouble(&o.poll, ctx) {
            lines.push(Line::from(fit(t.spans, w)));
        }
        // With `v` every figure the poll reads, memory still only as the bar.
        let all: Vec<Metric> = Metric::ALL
            .into_iter()
            .filter(|m| *m != Metric::Memory)
            .collect();
        let list: &[Metric] = if ctx.more { &all } else { &SHORT };
        // Three figures a row (a 3 × 2 grid of the six) where they fit, else two.
        let per_row = if w >= GRID_3_AT { 3 } else { 2 };
        let col = w / per_row;
        for row in list.chunks(per_row) {
            let mut spans = Vec::new();
            for m in row {
                let (text, st, _) = figure(*m, o);
                let spark_w = col.saturating_sub(1 + LABEL_W + VALUE_W + 3).min(HISTORY);
                let cell = vec![
                    Span::styled(format!(" {}", pad_r(label(*m), LABEL_W)), style::dim()),
                    Span::styled(pad_l(&text, VALUE_W), st),
                    Span::raw(" "),
                    Span::styled(sparkline(&o.history, *m, spark_w), style::data()),
                ];
                // Cut or padded to its column.
                spans.extend(fit(cell, col));
            }
            lines.push(Line::from(fit(spans, w)));
        }
        lines.extend(memory_bar(o, w));
        if let Some(l) = server_line(ctx.session, o.last.as_ref(), w) {
            lines.push(l);
        }
    }
    if ctx.session.is_cluster() {
        lines.push(Line::default());
        lines.extend(shard_lines(ctx.session, app, w));
    }
    lines
}

/// The label of a figure of "Now": the short six as the mockup names them.
fn label(m: Metric) -> &'static str {
    match m {
        Metric::Ops => "ops/s",
        Metric::Clients => "clients",
        Metric::Memory => "memory",
        Metric::HitRate => "hit rate",
        Metric::Evicted => "evicted",
        Metric::Expired => "expired",
        Metric::Fragmentation => "frag ratio",
        other => other.label(),
    }
}

/// A figure of "Now": its value and style, and for memory the limit it is "of".
fn figure(
    m: Metric,
    o: &crate::tui::app::views::server::Overview,
) -> (String, Style, Option<String>) {
    let last = o.last.as_ref();
    if m == Metric::Memory
        && let Some(used) = o
            .now()
            .and_then(|p| p.used_memory)
            .or(last.and_then(|l| l.used_memory))
    {
        let max = last.and_then(|l| l.maxmemory).filter(|m| *m > 0);
        let st = match max {
            Some(max) if used as f64 / max as f64 >= 0.9 => style::warning(),
            _ => style::fgb(),
        };
        return (format::bytes(used), st, max.map(format::bytes));
    }
    let (text, st) = server::metric_text(m, o);
    // The label says "ops/s" already.
    let text = if m == Metric::Ops {
        text.trim_end_matches("/s").to_string()
    } else {
        text
    };
    (text, st, None)
}

/// `memory    15G ████████░░░░ of 24G`: the memory in use against the limit, or
/// `no limit`. Nothing before the first `INFO`.
fn memory_bar(o: &crate::tui::app::views::server::Overview, w: usize) -> Option<Line<'static>> {
    let used = o
        .now()
        .and_then(|p| p.used_memory)
        .or(o.last.as_ref().and_then(|l| l.used_memory))?;
    let (text, st, of) = figure(Metric::Memory, o);
    let mut spans = vec![
        Span::styled(format!(" {}", pad_r("memory", LABEL_W)), style::dim()),
        Span::styled(pad_l(&text, VALUE_W), st),
        Span::raw(" "),
    ];
    match (
        of,
        o.last.as_ref().and_then(|l| l.maxmemory).filter(|m| *m > 0),
    ) {
        (Some(of), Some(max)) => {
            let tail = format!(" of {of}");
            let room =
                w.saturating_sub(1 + LABEL_W + VALUE_W + 1 + super::widgets::width(&tail) + 1);
            let (fill, track) = bar(used as f64 / max as f64, room.min(60));
            spans.push(Span::styled(fill, style::data()));
            spans.push(Span::styled(track, style::track()));
            spans.push(Span::styled(tail, style::dim()));
        }
        _ => spans.push(Span::styled("no limit (maxmemory 0)", style::dim())),
    }
    Some(Line::from(fit(spans, w)))
}

/// `Redis 7.4.2 · up 41 d · primary, 2 replicas · RDB ok · AOF off · allkeys-lfu`:
/// the version, uptime, role and replicas, persistence and policy of the last `INFO`.
fn server_line(s: &Session, last: Option<&InfoSample>, w: usize) -> Option<Line<'static>> {
    let i = last?;
    let mut parts: Vec<(String, Style)> = Vec::new();
    if let Some(srv) = &s.server {
        parts.push((server_label(srv), style::fg()));
    }
    if let Some(u) = i.uptime_secs {
        parts.push((
            format!("up {}", format::duration(std::time::Duration::from_secs(u))),
            style::fg(),
        ));
    }
    let replicas = match i.replicas {
        Some(1) => ", 1 replica".to_string(),
        Some(n) if n > 0 => format!(", {n} replicas"),
        _ => String::new(),
    };
    match i.role {
        Some(Role::Primary) => parts.push((format!("primary{replicas}"), style::fg())),
        Some(Role::Replica) => match i.link_up {
            Some(false) => parts.push(("replica, link down".to_string(), style::danger())),
            _ => parts.push(("replica".to_string(), style::fg())),
        },
        None => {}
    }
    match i.rdb_ok {
        Some(false) => parts.push(("RDB last save failed".to_string(), style::danger())),
        Some(true) => parts.push(("RDB ok".to_string(), style::fg())),
        None => {}
    }
    match (i.aof_on, i.aof_ok) {
        (Some(true), Some(false)) => parts.push(("AOF write failed".to_string(), style::danger())),
        (Some(true), _) => parts.push(("AOF on".to_string(), style::fg())),
        (Some(false), _) => parts.push(("AOF off".to_string(), style::fg())),
        (None, _) => {}
    }
    if let Some(p) = &i.policy {
        let p = if w >= WIDE {
            format!("policy {p}")
        } else {
            p.clone()
        };
        parts.push((p, style::fg()));
    }
    let mut spans = vec![Span::raw(" ")];
    for (n, (text, st)) in parts.into_iter().enumerate() {
        if n > 0 {
            spans.push(Span::styled(" · ", style::dim()));
        }
        spans.push(Span::styled(text, st));
    }
    Some(Line::from(fit(spans, w)))
}

const SLOTS_W: usize = 13;
const NODE_W: usize = 17;
const ROLE_W: usize = 9;

/// `0-5460` of a node's slots, with `+2` for more ranges.
fn slots_text(slots: &[(u16, u16)]) -> String {
    match slots {
        [] => "—".to_string(),
        [(a, b), rest @ ..] => {
            let first = if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            };
            if rest.is_empty() {
                first
            } else {
                format!("{first} +{}", rest.len())
            }
        }
    }
}

/// The per-shard table under a cluster's "Now": slots, node, role, ops/s, memory and
/// keys of each node, from its last per-shard `INFO` (the connect's figures until it
/// answers), the picked node marked.
fn shard_lines(s: &Session, app: &App, w: usize) -> Vec<Line<'static>> {
    let state = &app.views.server;
    let wide = w >= WIDE;
    let (sw, nw, rw, vw) = if wide {
        (SLOTS_W + 2, NODE_W + 2, ROLE_W + 3, 9)
    } else {
        (SLOTS_W, NODE_W, ROLE_W, 7)
    };
    let mut head = format!(
        " {}{}{}{}{}{}",
        pad_r("SHARD SLOTS", sw),
        pad_r("NODE", nw),
        pad_r("ROLE", rw),
        pad_l("OPS/S", vw - 2),
        pad_l("MEMORY", vw),
        pad_l("KEYS", vw),
    );
    if state.paused {
        head.push_str("   paused");
    } else if wide {
        head.push_str(&format!(
            "   every {}",
            server::secs(state.interval().max(SHARDS_EVERY))
        ));
    }
    let mut out = vec![Line::from(Span::styled(trunc(&head, w), style::hd()))];
    let figures = s.figures.as_ref().map_or(&[][..], |f| &f.nodes[..]);
    for (i, n) in s.nodes.iter().enumerate() {
        let mark = if i == s.picked { "›" } else { " " };
        let role = match n.role {
            NodeRole::Primary => "primary",
            NodeRole::Replica => "replica",
        };
        let mut spans = vec![
            Span::styled(mark, style::focus()),
            Span::styled(pad_r(&slots_text(&n.slots), sw), style::fg()),
            Span::styled(
                pad_r(&trunc(&n.endpoint.to_string(), nw - 1), nw),
                style::fg(),
            ),
            Span::styled(pad_r(role, rw), style::dim()),
        ];
        let row = state.shards.rows.get(i);
        let known = figures.get(i).and_then(|f| f.figures.as_ref());
        match row {
            _ if !n.connected => spans.push(Span::styled("✗ not connected", style::danger())),
            Some(r) if r.failed => spans.push(Span::styled("✗ no answer", style::danger())),
            _ => {
                let last = row.and_then(|r| r.last.as_ref());
                let ops = last
                    .and_then(|l| l.ops_now)
                    .map_or_else(|| "—".to_string(), format::count);
                let mem = last
                    .and_then(|l| l.used_memory)
                    .or(known.map(|f| f.server.used_memory))
                    .map_or_else(|| "—".to_string(), format::bytes);
                let keys = last
                    .and_then(|l| l.keys)
                    .or(known.map(|f| f.server.total_keys))
                    .map_or_else(|| "—".to_string(), format::count);
                spans.push(Span::styled(pad_l(&ops, vw - 2), style::fgb()));
                spans.push(Span::styled(pad_l(&mem, vw), style::fg()));
                spans.push(Span::styled(pad_l(&keys, vw), style::fg()));
            }
        }
        out.push(Line::from(fit(spans, w)));
    }
    out
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A heading and its rows, fitted to the height by [`fit_blocks`].
struct Block {
    /// Heading lines; empty for a block of rows alone.
    head: Vec<Line<'static>>,
    rows: Vec<Line<'static>>,
    /// The selected row, kept on screen when the rows are cut.
    sel: Option<usize>,
}

impl Block {
    fn new(head: Option<Line<'static>>, rows: Vec<Line<'static>>) -> Self {
        Self {
            head: head.into_iter().collect(),
            rows,
            sel: None,
        }
    }
}

/// `prefix`, then the blocks that fit in `h` rows, one blank line between two. A block
/// is shown only with at least one row, so a heading never stands without rows; the
/// rows left over then go to the blocks in order, and a block's cut rows keep its
/// selected one on screen.
fn fit_blocks(prefix: Vec<Line<'static>>, blocks: Vec<Block>, h: usize) -> Vec<Line<'static>> {
    fit_blocks_sel(prefix, blocks, h).0
}

/// [`fit_blocks`], and the rows drawn of the block with a selection (the findings),
/// 0 when it is not drawn: what PgUp and PgDn move by on Overview.
fn fit_blocks_sel(
    prefix: Vec<Line<'static>>,
    blocks: Vec<Block>,
    h: usize,
) -> (Vec<Line<'static>>, usize) {
    let mut sel_rows = 0;
    let mut left = h.saturating_sub(prefix.len());
    let mut shown: Vec<(Block, usize)> = Vec::new();
    for b in blocks.into_iter().filter(|b| !b.rows.is_empty()) {
        let sep = usize::from(!shown.is_empty());
        let need = sep + b.head.len() + 1;
        if need > left {
            // Later blocks may be smaller; keep trying.
            continue;
        }
        left -= need;
        shown.push((b, 1));
    }
    for (b, n) in &mut shown {
        let more = (b.rows.len() - *n).min(left);
        *n += more;
        left -= more;
    }
    let mut out = prefix;
    for (i, (b, n)) in shown.into_iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        out.extend(b.head);
        if b.sel.is_some() {
            sel_rows = n;
        }
        let from = b.sel.map_or(0, |s| scroll_offset(s, n));
        out.extend(b.rows.into_iter().skip(from).take(n));
    }
    (out, sel_rows)
}

/// `7.92M keys · 31% have a TTL · 13G of data, 2.1G overhead`, leaving out the parts
/// the server did not give; or why the figures are missing.
fn headline(s: &Session, w: usize) -> Line<'static> {
    let spans = headline_spans(s);
    if spans.is_empty() {
        return Line::default();
    }
    Line::from(fit(spans, w))
}

/// The headline's spans, uncut; none when there is nothing to say yet.
fn headline_spans(s: &Session) -> Vec<Span<'static>> {
    let figures = s.figures.as_ref();
    if let Some(why) = figures.and_then(|f| f.failed) {
        return vec![Span::styled(format!(" ▲ {why}."), style::warning())];
    }
    let mut parts: Vec<String> = Vec::new();
    match figures.and_then(|f| f.server.as_ref()) {
        Some(f) => {
            let h = f.headline();
            parts.push(format!("{} keys", format::count(h.keys)));
            if let Some(share) = h.ttl_share {
                parts.push(format!("{} have a TTL", format::percent(share)));
            }
            let mem = match (h.dataset_bytes, h.overhead_bytes) {
                (Some(d), Some(o)) => Some(format!(
                    "{} of data, {} overhead",
                    format::bytes(d),
                    format::bytes(o)
                )),
                (Some(d), None) => Some(format!("{} of data", format::bytes(d))),
                (None, Some(o)) => Some(format!("{} overhead", format::bytes(o))),
                (None, None) => None,
            };
            parts.extend(mem);
        }
        None => match &s.server {
            Some(info) => {
                parts.push(format!("{} keys", format::count(info.total_keys)));
                parts.push(format!("{} used", format::bytes(info.used_memory)));
            }
            None => return Vec::new(),
        },
    }
    let mut spans = vec![Span::styled(format!(" {}", parts[0]), style::fgb())];
    for p in &parts[1..] {
        spans.push(Span::styled(" · ", style::dim()));
        spans.push(Span::styled(p.clone(), style::fg()));
    }
    spans
}

/// Where the WHERE column starts.
fn place_col(w: usize) -> usize {
    w.saturating_sub(if w >= WIDE { 24 } else { 15 })
}

/// A block heading with `WHERE` over the findings' column.
fn where_head(title: &str, note: &str, w: usize) -> Line<'static> {
    where_head_at(title, note, w, place_col(w))
}

/// Where the WHERE column starts in the two panes: as far right as its longest value
/// lets it, so the findings get the width.
fn place_col_for(found: &[Finding], w: usize) -> usize {
    let longest = found
        .iter()
        .map(|f| super::widgets::width(f.place()))
        .max()
        .unwrap_or(0)
        .max("WHERE".len());
    w.saturating_sub(longest + 2)
}

/// [`where_head`] with WHERE at column `at`.
fn where_head_at(title: &str, note: &str, w: usize, at: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(format!(" {title}"), style::hd())];
    if !note.is_empty() {
        spans.push(Span::styled(format!("  {note}"), style::dim()));
    }
    let used: usize = spans
        .iter()
        .map(|s| super::widgets::width(&s.content))
        .sum();
    if used + 2 <= at {
        spans.push(Span::raw(" ".repeat(at - used)));
        spans.push(Span::styled("WHERE", style::hd()));
    }
    Line::from(fit(spans, w))
}

/// The findings as rows: `› ▲ text … Big keys`, `sel` marked.
fn finding_rows(found: &[Finding], sel: usize, w: usize) -> Vec<Line<'static>> {
    finding_rows_at(found, sel, w, place_col(w))
}

/// The cells before a finding's text: the cursor and the mark.
const FINDING_LEAD: usize = 5;

/// True when `f`'s sentence is shown whole in its row with WHERE at `at`.
fn finding_fits(f: &Finding, at: usize) -> bool {
    FINDING_LEAD + super::widgets::width(&f.text) <= at.saturating_sub(1)
}

/// [`finding_rows`] with WHERE at column `at`.
fn finding_rows_at(found: &[Finding], sel: usize, w: usize, at: usize) -> Vec<Line<'static>> {
    let sel = sel.min(found.len().saturating_sub(1));
    found
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (mark, st) = match f.severity {
                Severity::Warn | Severity::Note => ("▲", style::warning()),
                Severity::Info => ("●", style::dim()),
            };
            let cursor = if i == sel { " › " } else { "   " };
            let text = vec![
                Span::styled(cursor, style::focus()),
                Span::styled(format!("{mark} "), st),
                Span::styled(f.text.clone(), style::fg()),
            ];
            let mut spans = fit(text, at.saturating_sub(1));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(f.place().to_string(), style::dim()));
            Line::from(fit(spans, w))
        })
        .collect()
}

/// One dim row per part that is off, and why.
fn reason_rows(app: &App, w: usize) -> Vec<Line<'static>> {
    off_reasons(app)
        .into_iter()
        .map(|r| Line::from(Span::styled(trunc(&format!("   · {r}"), w), style::dim())))
        .collect()
}

/// `today 09:12 UTC` when on the day of `now`, else `2026-11-14 09:12 UTC`.
fn when_utc(unix: i64, now: Option<i64>) -> String {
    if now.is_some_and(|n| n.div_euclid(86_400) == unix.div_euclid(86_400)) {
        return format!("today {} UTC", hh_mm(unix));
    }
    let full = crate::snapshot::format_utc(unix);
    // `2026-11-14 09:12:33 UTC` without the seconds.
    format!("{} UTC", full.get(..16).unwrap_or(&full))
}

/// `Analyze: 10,000 keys, about 30K read-only commands, about 6 s.`
fn analyze_text(e: &Estimate) -> String {
    let across = if e.shards > 1 {
        format!(" across {} shards", e.shards)
    } else {
        String::new()
    };
    format!(
        "  Analyze: {} keys{across}, about {} read-only commands, {}.",
        format::grouped(e.keys),
        format::count(e.commands),
        about_time(e)
    )
}

/// The "Keys" lines for a section `h` rows tall, and the section's right title.
fn keys_lines(app: &App, s: &Session, w: usize, h: usize) -> (Vec<Line<'static>>, String) {
    app.overview.drawn_rows.set(0);
    let p = &s.profile;
    let mut lines = Vec::new();
    let notice = provider_rows(p, w);
    match &s.phase {
        // A connect; an analysis connecting again shows ANALYZING below.
        Phase::Connecting if s.analysis.is_none() => {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spinner(app.tick), style::warning()),
                Span::styled(
                    trunc(
                        &format!(
                            " Connecting to {} ({}) and checking permissions…",
                            p.name,
                            s.endpoint()
                        ),
                        w.saturating_sub(2),
                    ),
                    style::dim(),
                ),
            ]));
            if let Some(hint) = app.keychain_hint() {
                lines.push(Line::from(Span::styled(
                    trunc(&format!(" {hint}"), w),
                    style::warning(),
                )));
            }
            let lines = fit_blocks(lines, vec![Block::new(None, notice)], h);
            return (lines, String::new());
        }
        Phase::Failed(e) if app.report().is_none() => {
            lines.push(Line::from(Span::styled(
                trunc(&format!(" ✗ {e}"), w),
                style::danger(),
            )));
            return (lines, String::new());
        }
        _ => {}
    }
    let prefix = vec![headline(s, w), Line::default()];
    let mut blocks = Vec::new();
    let right;
    if app.analysis_running() {
        let e = estimate_for(s.analysis.unwrap_or_default(), app);
        right = format!("about {} commands in all", format::count(e.commands));
        blocks.push(analyzing_block(s, &e, w, false));
        blocks.push(server_block(app, w, false));
    } else if shows_analysis(app) {
        let r = app.report().expect("shows_analysis has a report");
        let stopped = if stopped_early(s) {
            " · stopped early"
        } else {
            ""
        };
        let again = if w >= WIDE {
            " · a analyzes again"
        } else {
            ""
        };
        right = format!(
            "{} sampled keys at {}{stopped}{again}",
            format::grouped(r.sampled_keys),
            at_utc(r.taken_at_unix)
        );
        let found = findings(app);
        let mut things = if found.is_empty() {
            vec![Line::from(Span::styled(
                "   Nothing stands out in the sample.",
                style::dim(),
            ))]
        } else {
            finding_rows(&found, app.overview.sel, w)
        };
        if stopped_early(s) {
            things.push(Line::from(Span::styled(
                trunc(
                    &format!(
                        "   · The analysis was stopped early: these come from {} keys only.",
                        format::grouped(r.sampled_keys)
                    ),
                    w,
                ),
                style::dim(),
            )));
        }
        let mut b = Block::new(Some(where_head("THINGS TO LOOK AT", "", w)), things);
        b.sel = (!found.is_empty()).then(|| app.overview.sel.min(found.len() - 1));
        blocks.push(b);
        let widths = if w >= WIDE { (22, 26) } else { (18, 16) };
        blocks.push(memory_block(r, w, TOP_PATTERNS, widths));
        if w >= WIDE {
            blocks.push(Block::new(None, vec![freed_line(r, w)]));
        }
    } else {
        right = before_right(s);
        blocks.push(server_block(app, w, true));
        let e = estimate_for(Depth::Quick, app);
        let mut prompt = vec![Line::from(fit(
            vec![
                Span::styled("   a", style::focus()),
                Span::styled(analyze_text(&e), style::fg()),
            ],
            w,
        ))];
        if let Some(last) = s.last_snapshot {
            prompt.push(Line::from(Span::styled(
                trunc(
                    &format!(
                        "   Last analysis: {}, {} keys (a saved snapshot).",
                        when_utc(last.taken_at_unix, app.wall_clock().or(s.connected_at_unix)),
                        format::grouped(last.sampled_keys)
                    ),
                    w,
                ),
                style::dim(),
            )));
        }
        blocks.push(Block::new(
            Some(Line::from(vec![
                Span::styled(" WHAT USES THE MEMORY", style::hd()),
                Span::styled("  needs a sample of the keys", style::dim()),
            ])),
            prompt,
        ));
    }
    blocks.push(Block::new(None, notice));
    let (lines, sel_rows) = fit_blocks_sel(prefix, blocks, h);
    app.overview
        .drawn_rows
        .set(sel_rows.min(findings(app).len()));
    (lines, right)
}

/// The right title of what the server says before an analysis: where the figures
/// come from, on a cluster how many shards answered.
fn before_right(s: &Session) -> String {
    if s.is_cluster() {
        // The shards whose figures are in the sum: those that answered.
        let answered = s
            .figures
            .as_ref()
            .filter(|f| !f.nodes.is_empty())
            .map_or(s.nodes.len(), |f| {
                f.nodes.iter().filter(|n| n.figures.is_some()).count()
            });
        let shards = if answered == 1 { "shard" } else { "shards" };
        format!("summed over {answered} {shards} · no keys read")
    } else {
        "from INFO, MEMORY STATS, SLOWLOG · no keys read".to_string()
    }
}

/// The profile's provider notice as a row, if it has one.
fn provider_rows(p: &Profile, w: usize) -> Vec<Line<'static>> {
    crate::conn::provider::notice(p.provider)
        .map(|n| Line::from(Span::styled(trunc(&format!(" ▲ {n}"), w), style::warning())))
        .into_iter()
        .collect()
}

/// "FROM THE SERVER": what the server says without a key read, `enter` going to each,
/// then the parts that are off and why. `note`: say "no keys read" by the heading.
fn server_block(app: &App, w: usize, note: bool) -> Block {
    let found = findings(app);
    let mut rows = finding_rows(&found, app.overview.sel, w);
    let reasons = reason_rows(app, w);
    if found.is_empty() && reasons.is_empty() {
        rows.push(Line::from(Span::styled(
            "   Nothing stands out in what the server reports.",
            style::dim(),
        )));
    }
    rows.extend(reasons);
    let mut b = Block::new(
        Some(where_head(
            "FROM THE SERVER",
            if note { "no keys read" } else { "" },
            w,
        )),
        rows,
    );
    b.sel = (!found.is_empty()).then(|| app.overview.sel.min(found.len() - 1));
    b
}

/// "ANALYZING": how far the sample is, a bar and the time left, what it sends per key,
/// and that `esc` stops it and keeps what it has.
/// `wrap`: its sentences broken over lines rather than cut (the narrower pane of the
/// two at 120 columns).
fn analyzing_block(s: &Session, e: &Estimate, w: usize, wrap: bool) -> Block {
    let pr = s.progress.unwrap_or_default();
    let target = if pr.target > 0 { pr.target } else { e.keys };
    let head = if wrap && s.phase != Phase::Connecting {
        Line::from(fit(
            vec![
                Span::styled(" ANALYZING", style::hd()),
                Span::styled(
                    format!(
                        "  sampling {} of {} keys",
                        format::grouped(pr.sampled),
                        format::grouped(target)
                    ),
                    style::dim(),
                ),
            ],
            w,
        ))
    } else if s.phase == Phase::Connecting {
        Line::from(vec![
            Span::styled(" ANALYZING", style::hd()),
            Span::styled(
                trunc(
                    "  connecting and checking permissions…",
                    w.saturating_sub(11),
                ),
                style::dim(),
            ),
        ])
    } else {
        let ratio = if target == 0 {
            0.0
        } else {
            pr.sampled as f64 / target as f64
        };
        let text = format!(
            "  sampling {} of {} keys   ",
            format::grouped(pr.sampled),
            format::grouped(target)
        );
        let left = time_left(e, pr.sampled, target).unwrap_or_default();
        let room =
            w.saturating_sub(11 + super::widgets::width(&text) + 2 + super::widgets::width(&left));
        let (fill, track) = bar(ratio, room.clamp(4, 30));
        Line::from(fit(
            vec![
                Span::styled(" ANALYZING", style::hd()),
                Span::styled(text, style::dim()),
                Span::styled(fill, style::data()),
                Span::styled(track, style::track()),
                Span::styled(format!("  {left}"), style::dim()),
            ],
            w,
        ))
    };
    let mut per_key = vec!["TYPE", "PTTL"];
    if e.sizes {
        per_key.push("MEMORY USAGE");
    }
    if e.freq {
        per_key.push("OBJECT FREQ");
    }
    let last = per_key.pop().unwrap_or_default();
    let cmds = if per_key.is_empty() {
        last.to_string()
    } else {
        format!("{} and {last}", per_key.join(", "))
    };
    let rate = match e.per_sec {
        Some(n) => format!("at most {} commands/s", format::grouped(u64::from(n))),
        None => "with no command limit".to_string(),
    };
    let stop = if s.stopped {
        " Stopping: it keeps what it has sampled so far…"
    } else {
        " Browse and Analyze fill in as it goes.  esc stops it and keeps what it has."
    };
    let said = format!(" {cmds} for each sampled key, {rate}.");
    let rows_ = if wrap {
        // The narrower pane: the progress on lines of its own, the bar the pane's width
        // and the time left under it, then the sentences wrapped.
        let mut v = Vec::new();
        if s.phase != Phase::Connecting {
            let ratio = if target == 0 {
                0.0
            } else {
                pr.sampled as f64 / target as f64
            };
            let pct = format!("  {:>3.0}%", (ratio * 100.0).min(100.0));
            let (fill, track) = bar(ratio, w.saturating_sub(2 + pct.len() + 1).max(4));
            v.push(Line::from(fit(
                vec![
                    Span::raw(" "),
                    Span::styled(fill, style::data()),
                    Span::styled(track, style::track()),
                    Span::styled(pct, style::dim()),
                ],
                w,
            )));
            if let Some(left) = time_left(e, pr.sampled, target) {
                v.extend(wrap_lines(&format!(" {left}"), w, style::dim()));
            }
        }
        v.extend(wrap_lines(&said, w, style::dim()));
        v.extend(wrap_lines(&stop.replace("  ", " "), w, style::dim()));
        v
    } else {
        vec![
            Line::from(Span::styled(trunc(&said, w), style::dim())),
            Line::from(Span::styled(trunc(stop, w), style::dim())),
        ]
    };
    Block::new(Some(head), rows_)
}

/// "WHERE THE MEMORY IS": the biggest patterns with their share, the rest on one row;
/// shares of keys, and why, without sizes.
fn memory_block(r: &ScanReport, w: usize, named_max: usize, widths: (usize, usize)) -> Block {
    let value = |p: &crate::model::PatternStats| {
        if r.sizes_available {
            p.est_bytes.unwrap_or(0)
        } else {
            p.est_keys
        }
    };
    let total: u64 = r.patterns.iter().map(value).fold(0, u64::saturating_add);
    let (name_w, bar_w) = widths;
    let row = |name: String, v: u64| {
        let ratio = if total == 0 {
            0.0
        } else {
            v as f64 / total as f64
        };
        let (fill, track) = bar(ratio, bar_w);
        let size = if r.sizes_available {
            format::bytes(v)
        } else {
            format!("{} keys", format::count(v))
        };
        Line::from(fit(
            vec![
                Span::raw("   "),
                Span::styled(pad_r(&trunc(&name, name_w), name_w), style::fg()),
                Span::raw(" "),
                Span::styled(fill, style::data()),
                Span::styled(track, style::track()),
                Span::styled(pad_l(&format::percent(ratio), 5), style::fgb()),
                Span::styled(pad_l(&size, 9), style::fg()),
            ],
            w,
        ))
    };
    // At most `named_max` named; one row for the rest.
    let named = r.patterns.len().min(named_max);
    let mut rows: Vec<Line<'static>> = r
        .patterns
        .iter()
        .take(named)
        .map(|p| row(p.pattern.clone(), value(p)))
        .collect();
    let rest = &r.patterns[named..];
    if !rest.is_empty() {
        let v = rest.iter().map(value).fold(0, u64::saturating_add);
        let name = match rest.len() {
            1 => "1 other pattern".to_string(),
            n => format!("{n} other patterns"),
        };
        rows.push(row(name, v));
    }
    if !r.sizes_available {
        rows.push(Line::from(Span::styled(
            trunc(
                "   · Sizes need MEMORY USAGE, which this user may not run: these are shares of the keys.",
                w,
            ),
            style::dim(),
        )));
    }
    if r.patterns.is_empty() {
        rows.push(Line::from(Span::styled(
            "   The sample found no keys.",
            style::dim(),
        )));
    }
    Block::new(
        Some(Line::from(Span::styled(
            " WHERE THE MEMORY IS",
            style::hd(),
        ))),
        rows,
    )
}

/// `FREED BY TTL  within 1 h 0.4G · 1 d 2.1G · 7 d 2.6G · never 11G`, or why not.
fn freed_line(r: &ScanReport, w: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(" FREED BY TTL  ", style::hd())];
    match expiry(r) {
        Some(e) => {
            spans.push(Span::styled(
                format!(
                    "within 1 h {} · 1 d {} · 7 d {} · never {}",
                    format::bytes(e.within[0]),
                    format::bytes(e.within[1]),
                    format::bytes(e.within[2]),
                    format::bytes(e.never)
                ),
                style::fg(),
            ));
        }
        None => spans.push(Span::styled(
            "needs key sizes (MEMORY USAGE), which this analysis did not read",
            style::dim(),
        )),
    }
    Line::from(fit(spans, w))
}

// ---------------------------------------------------------------------------
// Two panes (from 120 columns)
// ---------------------------------------------------------------------------

/// True when the two panes have something of their own to show: not while the first
/// connect runs, nor after one that failed with nothing to show (those say so on one
/// column, as at 80).
fn two_pane_state(s: &Session) -> bool {
    match &s.phase {
        Phase::Connecting => s.analysis.is_some(),
        Phase::Failed(_) => s.report.is_some(),
        _ => true,
    }
}

/// "Now" across the top, then "Things to look at" (the findings, the selected one
/// explained under them, what is off) at the left and "Where the memory is" at the
/// right, filling the rest of the height.
fn two_panes(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    s: &Session,
    now: Vec<Line<'static>>,
    (now_left, now_right): (&str, &str),
) {
    let now_rows = u16::try_from(now.len()).unwrap_or(u16::MAX);
    // "Now" in a frame of its own whose bottom border is the panes' top border.
    let top = Rect {
        height: now_rows + 2,
        ..area
    };
    // No section has the keys there: the findings do.
    let inner = super::widgets::sections_focused(
        frame,
        top,
        &[Section {
            title: Title::new(now_left, now_right),
            rows: Some(now_rows),
        }],
        usize::MAX - 1,
    );
    rows(frame, inner[0], now, None);
    let sep = area.y + now_rows + 1;
    let bottom = Rect {
        y: sep,
        height: area.y + area.height - sep,
        ..area
    };
    let left_w = super::widgets::split_at(area.width, THINGS_SHARE);
    let (things_right, mem_right) = pane_rights(app, s);
    let panes = super::widgets::frame_panes(
        frame,
        bottom,
        &[left_w, area.width - left_w],
        &[
            Title::new("Things to look at", &things_right),
            Title::new("Where the memory is", &mem_right),
        ],
        0,
    );
    // The panes' top border joins the frame of "Now": tees, not corners.
    let tee = |frame: &mut Frame, x: u16, c: &str, st: Style| {
        frame.render_widget(Span::styled(c.to_string(), st), Rect::new(x, sep, 1, 1));
    };
    tee(frame, area.x, "├", style::focus());
    tee(frame, area.x + area.width - 1, "┤", style::border());
    things_pane(frame, panes[0], app, s);
    memory_pane(frame, panes[1], app, s);
}

/// The panes' right titles: how many findings and where from; the sample, or what an
/// analysis costs.
fn pane_rights(app: &App, s: &Session) -> (String, String) {
    let n = findings(app).len();
    let found = match n {
        0 => String::new(),
        1 => "1 finding".to_string(),
        n => format!("{n} findings"),
    };
    if app.analysis_running() {
        let e = estimate_for(s.analysis.unwrap_or_default(), app);
        let left = join_parts(&[&found, "from the server"]);
        return (
            left,
            format!("about {} commands in all", format::count(e.commands)),
        );
    }
    if shows_analysis(app) {
        let r = app.report().expect("shows_analysis has a report");
        // The time is in the context bar ("analyzed 14:02 UTC"); the pane's title has
        // room for the sample only, so a stopped run says so on the left.
        let stopped = if stopped_early(s) {
            "stopped early"
        } else {
            ""
        };
        return (
            join_parts(&[&found, stopped]),
            format!("{} sampled keys", format::grouped(r.sampled_keys)),
        );
    }
    // The pane's title is short: on a cluster how many shards are summed, else that
    // nothing is read key by key.
    let right = if s.is_cluster() {
        before_right(s)
            .trim_end_matches(" · no keys read")
            .to_string()
    } else {
        "no keys read".to_string()
    };
    (join_parts(&[&found, "from the server"]), right)
}

fn join_parts(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The left pane: the findings with their WHERE column (`↑↓` moves, `enter` goes),
/// a rule, the selected finding explained in the words the app has for it and what
/// `enter` opens; at the bottom what is off and why.
fn things_pane(frame: &mut Frame, inner: Rect, app: &App, s: &Session) {
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    if w == 0 || h == 0 {
        return;
    }
    let found = findings(app);
    let sel = app.overview.sel.min(found.len().saturating_sub(1));
    // The bottom: what is off, the provider's notice, a stopped analysis.
    let mut tail: Vec<Line<'static>> = Vec::new();
    if stopped_early(s)
        && let Some(r) = app.report()
    {
        tail.extend(wrap_lines(
            &format!(
                " The analysis was stopped early: these come from {} keys only.",
                format::grouped(r.sampled_keys)
            ),
            w,
            style::dim(),
        ));
    }
    let provider = provider_rows(&s.profile, w);
    // At most a third of the pane (all of them with `v`); when some do not fit, the
    // last line says how many and that `v` shows them.
    let cap = if app.more_here() { h } else { h / 3 };
    let reasons: Vec<Vec<Line<'static>>> = off_reasons(app)
        .iter()
        .map(|r| wrap_lines(&format!(" Off: {}", off_text(r)), w, style::dim()))
        .collect();
    let room = cap.saturating_sub(tail.len() + provider.len());
    let all: usize = reasons.iter().map(Vec::len).sum();
    if all <= room {
        tail.extend(reasons.into_iter().flatten());
    } else {
        // Whole reasons while they leave a line for the count.
        let mut used = 0;
        let mut shown = 0;
        for r in &reasons {
            if used + r.len() + 1 > room {
                break;
            }
            used += r.len();
            shown += 1;
        }
        let n = reasons.len() - shown;
        tail.extend(reasons.into_iter().take(shown).flatten());
        if room > 0 {
            let s_ = if n == 1 { "reason" } else { "reasons" };
            tail.push(Line::from(Span::styled(
                trunc(&format!(" … {n} more {s_}: v shows them"), w),
                style::dim(),
            )));
        }
    }
    tail.extend(provider);
    let tail_h = tail.len().min(h);
    let tail: Vec<Line<'static>> = tail.into_iter().take(tail_h).collect();
    // The list: its head and its rows, then a rule; the explanation gets the rest, at
    // least four rows when there is room.
    let body_h = h - tail_h;
    let want = found.len().max(1);
    let list_h = want.min(body_h.saturating_sub(1 + 1 + 4).max(1));
    let at = place_col_for(&found, w);
    let mut lines = vec![where_head_at("FINDING", "", w, at)];
    if found.is_empty() {
        let none = if shows_analysis(app) {
            "   Nothing stands out in the sample."
        } else {
            "   Nothing stands out in what the server reports."
        };
        lines.push(Line::from(Span::styled(trunc(none, w), style::dim())));
    } else {
        let off = scroll_offset(sel, list_h);
        let drawn = finding_rows_at(&found, sel, w, at)
            .into_iter()
            .skip(off)
            .take(list_h.min(body_h.saturating_sub(1)));
        let before = lines.len();
        lines.extend(drawn);
        app.overview.drawn_rows.set(lines.len() - before);
    }
    if found.is_empty() {
        app.overview.drawn_rows.set(0);
    }
    if let Some(f) = found.get(sel) {
        lines.push(Line::from(Span::styled("─".repeat(w), style::border())));
        let (mark, st) = match f.severity {
            Severity::Warn | Severity::Note => ("▲", style::warning()),
            Severity::Info => ("●", style::dim()),
        };
        // The sentence again only when its row had to cut it.
        let whole = finding_fits(f, at);
        if !whole {
            lines.extend(wrap_lines(&format!(" {mark} {}", f.text), w, st));
        }
        if !f.hint.is_empty() {
            if !whole {
                lines.push(Line::default());
            }
            lines.extend(wrap_lines(&format!(" {}", f.hint), w, style::fg()));
        }
        if !whole || !f.hint.is_empty() {
            lines.push(Line::default());
        }
        let to = match f.to.panel() {
            Some(p) => format!("{} › {}", f.to.area().title(), p.name()),
            None => f.to.area().title().to_string(),
        };
        lines.push(Line::from(fit(
            vec![
                Span::styled(" enter", style::focus()),
                Span::styled(format!(" opens {to}"), style::dim()),
            ],
            w,
        )));
    }
    lines.truncate(body_h);
    rows(frame, inner, lines, None);
    let tail_area = Rect {
        y: inner.y + inner.height - tail_h as u16,
        height: tail_h as u16,
        ..inner
    };
    rows(frame, tail_area, tail, None);
}

/// An "off" reason after the pane's "Off:": `Slow log is off: why` reads `Slow log:
/// why`, so "off" is not said twice.
fn off_text(r: &str) -> String {
    for said in [" is off: ", " are off: "] {
        if let Some((what, why)) = r.split_once(said) {
            return format!("{what}: {why}");
        }
    }
    r.to_string()
}

/// `text` wrapped to `w` in `st`, each line after the first indented under the first
/// word after a leading mark.
fn wrap_lines(text: &str, w: usize, st: Style) -> Vec<Line<'static>> {
    super::patterns::wrap_words(text, w)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, st)))
        .collect()
}

/// The right pane: the server's headline, then before an analysis what one costs,
/// while one runs its progress, and after it every top pattern that fits with its
/// share and size, what TTLs free, and the slowest commands the slow log holds.
fn memory_pane(frame: &mut Frame, inner: Rect, app: &App, s: &Session) {
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    let mut prefix = wrap_spans(headline_spans(s), w, 1);
    prefix.push(Line::default());
    let mut blocks: Vec<Block> = Vec::new();
    if app.analysis_running() {
        let e = estimate_for(s.analysis.unwrap_or_default(), app);
        blocks.push(analyzing_block(s, &e, w, true));
    } else if shows_analysis(app) {
        let r = app.report().expect("shows_analysis has a report");
        let freed = freed_block(r, w);
        let slow = slow_block(&app.views.server.slow.entries, w);
        // What the memory block may take: the rest less the blocks after it, which go
        // (the slow log first) when they would leave it fewer than three rows.
        let room = h.saturating_sub(prefix.len());
        let need = |b: &Option<Block>| b.as_ref().map_or(0, |b| 1 + b.head.len() + b.rows.len());
        let mut after = vec![freed, slow];
        while room.saturating_sub(1 + after.iter().map(need).sum::<usize>()) < 3
            && after.iter().any(Option::is_some)
        {
            if let Some(last) = after.iter_mut().rev().find(|b| b.is_some()) {
                *last = None;
            }
        }
        // Every pattern when they fit; else one row less, and one for the rest.
        let top = room
            .saturating_sub(1 + after.iter().map(need).sum::<usize>())
            .max(1);
        let named = if r.patterns.len() <= top {
            top
        } else {
            top - 1
        };
        // The pane's title already says "Where the memory is": no heading again.
        let mut patterns = memory_block(r, w, named, share_widths(w));
        patterns.head.clear();
        blocks.push(patterns);
        blocks.extend(after.into_iter().flatten());
    } else {
        let e = estimate_for(Depth::Quick, app);
        let mut prompt = wrap_spans(
            vec![
                Span::styled("   a", style::focus()),
                Span::styled(analyze_text(&e), style::fg()),
            ],
            w,
            6,
        );
        if let Some(last) = s.last_snapshot {
            prompt.extend(wrap_spans(
                vec![Span::styled(
                    format!(
                        "   Last analysis: {}, {} keys (a saved snapshot).",
                        when_utc(last.taken_at_unix, app.wall_clock().or(s.connected_at_unix)),
                        format::grouped(last.sampled_keys)
                    ),
                    style::dim(),
                )],
                w,
                3,
            ));
        }
        blocks.push(Block::new(
            Some(Line::from(vec![
                Span::styled(" WHAT USES THE MEMORY", style::hd()),
                Span::styled("  needs a sample of the keys", style::dim()),
            ])),
            prompt,
        ));
    }
    rows(frame, inner, fit_blocks(prefix, blocks, h), None);
}

/// "FREED BY TTL" as bars: the memory that expires within an hour, a day and a week,
/// and that never does, of the sample's total; or why it is not known.
fn freed_block(r: &ScanReport, w: usize) -> Option<Block> {
    let head = Line::from(Span::styled(" FREED BY TTL", style::hd()));
    let Some(e) = expiry(r) else {
        return Some(Block::new(
            Some(head),
            wrap_spans(
                vec![Span::styled(
                    "   needs key sizes (MEMORY USAGE), which this analysis did not read",
                    style::dim(),
                )],
                w,
                3,
            ),
        ));
    };
    let rows_ = [
        ("within 1 h", e.within[0]),
        ("within 1 d", e.within[1]),
        ("within 7 d", e.within[2]),
        ("never", e.never),
    ]
    .into_iter()
    // Quantities in the data colour; the memory that never expires in the warning colour.
    .map(|(name, v)| {
        let st = if name == "never" {
            style::warning()
        } else {
            style::data()
        };
        share_row(name, v, e.total, &format::bytes(v), w, st)
    })
    .collect();
    Some(Block::new(Some(head), rows_))
}

/// "SLOWEST IN THE SLOW LOG": the slowest entries the slow log already holds (read
/// by the connect or the Slow log panel; nothing is read for it), their command and
/// first key, never their arguments. `None` without entries.
fn slow_block(entries: &[crate::live::server::SlowEntry], w: usize) -> Option<Block> {
    if entries.is_empty() {
        return None;
    }
    let mut by_time: Vec<&crate::live::server::SlowEntry> = entries.iter().collect();
    by_time.sort_by_key(|e| std::cmp::Reverse((e.micros, e.unix)));
    let rows_ = by_time
        .into_iter()
        .take(SLOWEST)
        .map(|e| {
            // Its duration, command name and time only: never its keys, arguments or
            // client (spec: the slow finding is shown without its arguments or keys).
            let name = if e.command.is_empty() {
                "—".to_string()
            } else {
                e.command.clone()
            };
            Line::from(fit(
                vec![
                    Span::raw("   "),
                    Span::styled(
                        pad_l(&crate::tui::app::views::overview::micros_text(e.micros), 8),
                        style::fgb(),
                    ),
                    Span::raw("  "),
                    Span::styled(pad_r(&trunc(&name, 14), 14), style::cmd()),
                    Span::raw(" "),
                    Span::styled(format!("{} UTC", hh_mm(e.unix)), style::dim()),
                ],
                w,
            ))
        })
        .collect();
    Some(Block::new(
        Some(Line::from(Span::styled(
            " SLOWEST IN THE SLOW LOG",
            style::hd(),
        ))),
        rows_,
    ))
}

/// `   name   ████░░░░  31%    4.2G`: a share of `total` as a bar, for a pane `w`
/// wide.
fn share_row(name: &str, v: u64, total: u64, size: &str, w: usize, bar_st: Style) -> Line<'static> {
    let ratio = if total == 0 {
        0.0
    } else {
        v as f64 / total as f64
    };
    let (name_w, bar_w) = share_widths(w);
    let (fill, track) = bar(ratio, bar_w);
    Line::from(fit(
        vec![
            Span::raw("   "),
            Span::styled(pad_r(&trunc(name, name_w), name_w), style::fg()),
            Span::raw(" "),
            Span::styled(fill, bar_st),
            Span::styled(track, style::track()),
            Span::styled(pad_l(&format::percent(ratio), 5), style::fgb()),
            Span::styled(pad_l(size, 9), style::fg()),
        ],
        w,
    ))
}

/// The name and bar widths of a share row in a pane `w` wide: as on one column at 80
/// and 120 (18 and 16, 22 and 26), and in a narrower pane what is left after the name.
fn share_widths(w: usize) -> (usize, usize) {
    if w >= WIDE {
        (22, 26)
    } else if w >= 60 {
        (22, w.saturating_sub(3 + 22 + 1 + 5 + 9 + 1).clamp(6, 26))
    } else {
        (18, w.saturating_sub(3 + 18 + 1 + 5 + 9 + 1).clamp(4, 16))
    }
}

/// Draws "Analyze the keys?" centred in `area`, over the dimmed screen: Quick and Deep
/// with the engine's estimate for this session.
pub fn draw_analyze(frame: &mut Frame, area: Rect, d: &AnalyzeDialog, app: &App) {
    let Some(s) = &app.session else {
        return;
    };
    let p = &s.profile;
    let right = format!("{} · {}", p.name, p.env.label());
    let w = area.width.min(72);
    let inner_w = usize::from(w.saturating_sub(2));
    let quick = estimate_for(Depth::Quick, app);
    let across = if quick.shards > 1 {
        format!(" across {} shards", quick.shards)
    } else {
        String::new()
    };
    let mut lines = vec![
        Line::from(Span::styled(
            trunc(
                &format!(" Sampling reads a few commands per key{across}; nothing is written."),
                inner_w,
            ),
            style::fg(),
        )),
        Line::default(),
    ];
    for depth in Depth::ALL {
        let e = estimate_for(depth, app);
        let mark = if depth == d.pick { " › " } else { "   " };
        let st = if depth == d.pick {
            style::fgb()
        } else {
            style::fg()
        };
        lines.push(Line::from(fit(
            vec![
                Span::styled(mark, style::focus()),
                Span::styled(pad_r(depth.label(), 8), st),
                Span::styled(pad_r(&format!("{} keys", format::grouped(e.keys)), 16), st),
                Span::styled(
                    pad_r(&format!("about {} commands", format::count(e.commands)), 23),
                    style::fg(),
                ),
                Span::styled(about_time(&e), style::fg()),
            ],
            inner_w,
        )));
    }
    lines.push(Line::default());
    let limit = match quick.per_sec {
        Some(n) => format!(
            " Deep finds rare patterns and rare big keys. Both stay under this profile's limit of {} commands/s, and esc stops either one and keeps what it has.",
            format::grouped(u64::from(n))
        ),
        None => " Deep finds rare patterns and rare big keys. This profile has no command limit, and esc stops either one and keeps what it has.".to_string(),
    };
    for chunk in wrap(&limit, inner_w) {
        lines.push(Line::from(Span::styled(chunk, style::dim())));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" enter", style::focus()),
        Span::styled(" Start   ", style::fg()),
        Span::styled("↑↓", style::focus()),
        Span::styled(" Pick   ", style::fg()),
        Span::styled("esc", style::focus()),
        Span::styled(" Cancel", style::fg()),
    ]));
    let h = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let body = dialog(frame, area, w, h, "Analyze the keys?", &right);
    rows(frame, body, lines, None);
}

/// `text` in lines at most `w` wide, broken at spaces; each line after the first
/// starts with a space, as the first does.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && super::widgets::width(&line) + 1 + super::widgets::width(word) > w {
            out.push(std::mem::take(&mut line));
        }
        line.push(' ');
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(trunc(&line, w));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::server::Reply;
    use crate::live::server::{InfoSample, Role};
    use crate::live::{LiveReply, LiveRequest};
    use crate::tui::app::views::testing;
    use crate::tui::app::{Effect, Msg, Screen};
    use crate::tui::fixtures;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// An INFO taken `t` seconds into a steady run.
    fn sample(t: u64) -> InfoSample {
        InfoSample {
            uptime_secs: Some(1000 + t),
            role: Some(Role::Primary),
            replicas: Some(1),
            clients: Some(10 + t % 3),
            blocked: Some(0),
            used_memory: Some(1_000_000 + t * 1000),
            maxmemory: Some(4_000_000),
            fragmentation: Some(1.1),
            policy: Some("allkeys-lfu".into()),
            commands: Some(10_000 + 100 * t),
            hits: Some(900 + 9 * t),
            misses: Some(100 + t),
            net_in: Some(50_000 + 1024 * t),
            net_out: Some(80_000 + 2048 * t),
            evicted: Some(t / 2),
            expired: Some(5 * t),
            cpu_user: Some(10.0 + 0.05 * t as f64),
            cpu_sys: Some(4.0 + 0.01 * t as f64),
            rdb_ok: Some(true),
            aof_on: Some(false),
            ..InfoSample::default()
        }
    }

    /// Answers the Overview's INFO polls in `effects` and the next few, so "Now" has
    /// rates and a short history.
    fn feed_info(app: &mut App, mut effects: Vec<Effect>) {
        let t0 = Instant::now();
        for t in 0..4u64 {
            let at = t0 + Duration::from_secs(2 * t);
            effects.extend(app.apply(Msg::TickAt(at)));
            for (id, req) in testing::requests(&effects) {
                if matches!(req, LiveRequest::Server(_)) {
                    testing::reply(
                        app,
                        id,
                        Ok(LiveReply::Server(Reply::Info {
                            at,
                            sample: Box::new(sample(2 * t)),
                        })),
                    );
                }
            }
            effects.clear();
        }
    }

    /// prod-cache-01 connected, the live connection open, on Overview; `analyzed` with
    /// the fixture scan finished.
    fn overview_app(analyzed: bool) -> App {
        overview_app_with(analyzed, fixtures::perms_with_probes())
    }

    /// [`overview_app`] with the permission check's answer `perms`.
    fn overview_app_with(analyzed: bool, perms: crate::model::PermissionReport) -> App {
        let mut app = if analyzed {
            testing::scanned()
        } else {
            let mut app = App::new(fixtures::profiles());
            let effects = app.handle_key(key(KeyCode::Enter));
            let [Effect::Connect { id, scan, .. }] = effects[..] else {
                panic!("expected a connect, got {effects:?}");
            };
            app.apply(Msg::Connected {
                id,
                scan,
                server: Some(fixtures::server()),
                perms,
                target: crate::tui::ScanTarget::Primary,
            });
            app
        };
        let mut out = testing::live_ready(&mut app);
        out.extend(testing::show(&mut app, Screen::Overview));
        feed_info(&mut app, out);
        app
    }

    fn snap(app: &App, w: u16, h: u16) -> String {
        testing::render(app, w, h).backend().to_string()
    }

    #[test]
    fn overview_before_an_analysis() {
        let app = overview_app(false);
        insta::assert_snapshot!("before_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("before_120x36", snap(&app, 120, 36));
        insta::assert_snapshot!("before_160x45", snap(&app, 160, 45));
    }

    #[test]
    fn overview_after_an_analysis() {
        let app = overview_app(true);
        insta::assert_snapshot!("after_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("after_120x36", snap(&app, 120, 36));
        insta::assert_snapshot!("after_160x45", snap(&app, 160, 45));
    }

    #[test]
    fn overview_with_all_figures() {
        let mut app = overview_app(true);
        app.handle_key(key(KeyCode::Char('v')));
        insta::assert_snapshot!("more_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn the_analyze_dialog() {
        let mut app = overview_app(false);
        app.handle_key(key(KeyCode::Char('a')));
        insta::assert_snapshot!("analyze_80x24", snap(&app, 80, 24));
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!("analyze_deep_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn help_opened_and_filtered() {
        let mut app = overview_app(true);
        app.handle_key(key(KeyCode::Char('?')));
        insta::assert_snapshot!("help_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("help_120x36", snap(&app, 120, 36));
        for c in "slow".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!("help_slow_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("help_slow_120x36", snap(&app, 120, 36));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for analyzed in [false, true] {
            let mut app = overview_app(analyzed);
            for (w, h) in [(1, 1), (0, 0), (20, 5), (40, 12), (200, 3), (3, 40)] {
                testing::render(&app, w, h);
            }
            app.handle_key(key(KeyCode::Char('a')));
            for (w, h) in [(1, 1), (0, 0), (20, 5), (40, 12), (200, 3), (3, 40)] {
                testing::render(&app, w, h);
            }
        }
    }

    use crate::tui::app::views::overview::{self as ov, Figures, LastSnapshot};
    use crate::tui::app::{ConnId, ScanId};

    /// 2026-11-15 15:00 UTC: "now" for the tests that show times.
    const NOW: i64 = 1_794_754_800;

    fn ids(app: &App) -> (ConnId, ScanId) {
        let s = app.session.as_ref().unwrap();
        (s.id, s.scan)
    }

    fn text_of(app: &App, w: u16, h: u16) -> String {
        snap(app, w, h)
    }

    /// No section heading is the last line of its section: each has a row under it.
    fn no_heading_without_rows(screen: &str) {
        let lines: Vec<&str> = screen.lines().collect();
        for head in [
            "FROM THE SERVER",
            "WHAT USES THE MEMORY",
            "THINGS TO LOOK AT",
            "WHERE THE MEMORY IS",
            "ANALYZING",
        ] {
            for (i, l) in lines.iter().enumerate() {
                if !l.contains(head) {
                    continue;
                }
                let next = lines.get(i + 1).copied().unwrap_or("");
                let body = next.trim_matches(|c: char| c == '"' || c == '│' || c == ' ');
                assert!(
                    !body.is_empty() && !next.contains('└') && !next.contains('├'),
                    "{head} has no row under it:\n{screen}"
                );
            }
        }
    }

    /// [`overview_app`] before an analysis, with the connect's figures (`big` big keys),
    /// slow log and client list, and the last saved analysis from yesterday.
    fn just_connected(big: Option<u64>) -> App {
        let mut app = overview_app(false);
        app.set_wall_clock(NOW);
        let (id, scan) = ids(&app);
        app.apply(Msg::Figures {
            id,
            scan,
            figures: Figures {
                server: Some(ov::tests::figures(big)),
                nodes: Vec::new(),
                failed: None,
            },
        });
        app.apply(Msg::ServerReads {
            id,
            scan,
            node: 0,
            slow: Some(Ok(ov::tests::slow_reply())),
            clients: Some(Ok(ov::tests::clients_reply())),
        });
        app.apply(Msg::LastSnapshot {
            id,
            scan,
            last: Some(LastSnapshot {
                taken_at_unix: NOW - 86_400 - 3_600,
                sampled_keys: 100_000,
            }),
        });
        app
    }

    /// [`just_connected`] with a Quick analysis 4,100 keys in.
    fn analyzing() -> App {
        let mut app = just_connected(Some(3));
        app.handle_key(key(KeyCode::Char('a')));
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("expected a connect, got {effects:?}");
        };
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(fixtures::server()),
            perms: fixtures::perms_with_probes(),
            target: crate::tui::ScanTarget::Primary,
        });
        app.apply(Msg::Figures {
            id,
            scan,
            figures: Figures {
                server: Some(ov::tests::figures(Some(3))),
                nodes: Vec::new(),
                failed: None,
            },
        });
        let out = testing::live_ready(&mut app);
        feed_info(&mut app, out);
        app.apply(Msg::ScanProgress {
            id,
            scan,
            progress: crate::scan::ScanProgress {
                sampled: 4_100,
                target: 10_000,
                done: false,
            },
        });
        app.apply(Msg::ScanPartial {
            id,
            scan,
            partial: fixtures::partial(true),
        });
        app
    }

    #[test]
    fn just_connected_shows_what_the_server_says_without_reading_a_key() {
        let app = just_connected(Some(3));
        let t = text_of(&app, 80, 24);
        for want in [
            "FROM THE SERVER",
            "3 big keys, counted by the server (INFO keysizes).",
            "KEYS took 812 ms at 14:01 UTC.",
            "212 connections from 10.0.9.17 idle for over an hour.",
            "WHAT USES THE MEMORY  needs a sample of the keys",
            "no keys read",
        ] {
            assert!(t.contains(want), "{want}:\n{t}");
        }
        // Neither the slow command's arguments nor its key, at any width.
        assert!(!t.contains("secret-value"), "{t}");
        for (w, h) in [(120, 36), (160, 45)] {
            let wide = text_of(&app, w, h);
            assert!(!wide.contains("secret-value"), "{w}x{h}:\n{wide}");
        }
        no_heading_without_rows(&t);
        insta::assert_snapshot!("connected_80x24", t);
        let t = text_of(&app, 120, 36);
        assert!(t.contains("Last analysis:"), "{t}");
        assert!(t.contains("UTC"), "{t}");
        no_heading_without_rows(&t);
        insta::assert_snapshot!("connected_120x36", t);
    }

    #[test]
    fn refused_reads_turn_their_part_off_with_a_reason() {
        // The permission check refused SLOWLOG; CLIENT LIST is refused when read.
        let mut app = overview_app_with(false, fixtures::perms_probes_off());
        let (id, scan) = ids(&app);
        let mut f = ov::tests::figures(None);
        f.context.memory = None;
        f.context
            .missing
            .push(crate::model::ContextGap::MemoryStatsDenied);
        app.apply(Msg::Figures {
            id,
            scan,
            figures: Figures {
                server: Some(f),
                nodes: Vec::new(),
                failed: None,
            },
        });
        app.apply(Msg::ServerReads {
            id,
            scan,
            node: 0,
            slow: None,
            clients: Some(Err(crate::live::LiveError::Denied("CLIENT LIST".into()))),
        });
        let reasons = ov::off_reasons(&app);
        assert_eq!(reasons.len(), 4, "{reasons:?}");
        // Each reason whole on a line where the left pane is wide enough.
        let wide = text_of(&app, 200, 50);
        let t = text_of(&app, 120, 36);
        for want in [
            "Big keys from the server need INFO keysizes",
            "MEMORY STATS is denied for this user.",
            "Off: Slow log: ",
            "Off: Idle clients: the server refused CLIENT LIST",
        ] {
            assert!(wide.contains(want), "{want}:\n{wide}");
        }
        // "off" is not said twice.
        assert!(
            !wide.contains("is off:") && !wide.contains("are off:"),
            "{wide}"
        );
        no_heading_without_rows(&t);
        insta::assert_snapshot!("refused_120x36", t);
        // At a short height the reasons that do not fit are counted, and `v` shows
        // them all.
        let short = text_of(&app, 120, 22);
        assert!(short.contains("more reason"), "{short}");
        assert!(short.contains("v shows them"), "{short}");
        let mut app = app;
        app.handle_key(key(KeyCode::Char('v')));
        let all = text_of(&app, 120, 22);
        assert!(!all.contains("more reason"), "{all}");
        for want in ["Off: Slow log: ", "Off: Idle clients: "] {
            assert!(all.contains(want), "{want}:\n{all}");
        }
        let t = text_of(&app, 80, 24);
        no_heading_without_rows(&t);
    }

    #[test]
    fn figures_that_failed_say_why_in_the_headline() {
        let mut app = overview_app(false);
        let (id, scan) = ids(&app);
        app.apply(Msg::Figures {
            id,
            scan,
            figures: Figures {
                server: None,
                nodes: Vec::new(),
                failed: Some("the server's figures could not be read"),
            },
        });
        let t = text_of(&app, 80, 24);
        assert!(
            t.contains("▲ the server's figures could not be read."),
            "{t}"
        );
    }

    #[test]
    fn analyzing_shows_progress_and_how_to_stop() {
        let app = analyzing();
        let t = text_of(&app, 80, 24);
        for want in ["ANALYZING", "4,100 of 10,000 keys", "left", "esc stops it"] {
            assert!(t.contains(want), "{want}:\n{t}");
        }
        no_heading_without_rows(&t);
        insta::assert_snapshot!("analyzing_80x24", t);
        let t = text_of(&app, 120, 36);
        // The narrower right pane wraps the sentence instead of cutting it.
        assert!(t.contains("esc stops"), "{t}");
        assert!(t.contains("it and keeps what it has."), "{t}");
        no_heading_without_rows(&t);
        insta::assert_snapshot!("analyzing_120x36", t);
    }

    #[test]
    fn the_findings_move_and_open_the_same_at_120_and_at_80_columns() {
        for (w, h) in [(80, 24), (120, 36), (160, 45)] {
            let mut app = just_connected(Some(3));
            let _ = text_of(&app, w, h);
            let effects = app.handle_key(key(KeyCode::Down));
            assert!(
                effects.is_empty(),
                "{w}x{h}: a move sends nothing: {effects:?}"
            );
            let t = text_of(&app, w, h);
            assert!(t.contains("› ▲ KEYS took 812 ms"), "{w}x{h}:\n{t}");
            if w >= 120 {
                // The selected finding explained, and where enter goes.
                assert!(t.contains("enter opens Server › Slow log"), "{w}x{h}:\n{t}");
            }
            app.handle_key(key(KeyCode::Enter));
            assert_eq!(app.screen(), Screen::SlowLog, "{w}x{h}");
        }
    }

    #[test]
    fn analyzed_shows_the_results_and_a_stopped_one_says_so() {
        let mut app = analyzing();
        let (id, scan) = ids(&app);
        let mut r = fixtures::report(true);
        let mb = 1024 * 1024;
        r.key_sizes = Some(Box::new(crate::model::KeySizes {
            big_keys: Vec::new(),
            ttl_bytes: [400 * mb, mb, 20 * mb, 60 * mb, 30 * mb, 10 * mb, 5 * mb],
        }));
        app.apply(Msg::ScanDone {
            id,
            scan,
            report: Box::new(r),
        });
        let t = text_of(&app, 80, 24);
        for want in ["THINGS TO LOOK AT", "WHERE THE MEMORY IS"] {
            assert!(t.contains(want), "{want}:\n{t}");
        }
        no_heading_without_rows(&t);
        insta::assert_snapshot!("analyzed_80x24", t);
        let t = text_of(&app, 120, 36);
        // Two panes: what TTLs free as bars, and every pattern that fits.
        for want in [
            "FREED BY TTL",
            "within 1 h",
            "within 7 d",
            "never",
            "lock:{var}",
        ] {
            assert!(t.contains(want), "{want}:\n{t}");
        }
        assert!(!t.contains("other patterns"), "{t}");
        // The slowest entries by duration, command and time; never a key or argument.
        assert!(t.contains("SLOWEST IN THE SLOW LOG"), "{t}");
        for (w, h) in [(80, 24), (120, 36), (160, 45)] {
            let any = text_of(&app, w, h);
            assert!(!any.contains("secret-value"), "{w}x{h}:\n{any}");
        }
        no_heading_without_rows(&t);
        insta::assert_snapshot!("analyzed_120x36", t);

        // esc stops the next one early: the partial results say so.
        let mut app = analyzing();
        let (id, scan) = ids(&app);
        app.handle_key(key(KeyCode::Esc));
        let t = text_of(&app, 80, 24);
        assert!(t.contains("stopping"), "{t}");
        let mut r = fixtures::report(true);
        r.sampled_keys = 4_200;
        r.complete = false;
        app.apply(Msg::ScanDone {
            id,
            scan,
            report: Box::new(r),
        });
        let t = text_of(&app, 120, 36);
        assert!(t.contains("(stopped)"), "{t}");
        assert!(t.contains("stopped early"), "{t}");
        no_heading_without_rows(&t);
        insta::assert_snapshot!("stopped_120x36", t);
    }

    /// prod-cache-01 as a three-shard cluster, connected without an analysis, the
    /// connect's figures per node and the per-shard poll answered.
    fn cluster_connected() -> App {
        let mut app = App::new(fixtures::profiles());
        app.set_wall_clock(NOW);
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Connect { id, scan, .. }] = effects[..] else {
            panic!("expected a connect, got {effects:?}");
        };
        let report = fixtures::cluster_report();
        app.apply(Msg::Connected {
            id,
            scan,
            server: Some(report.server.clone()),
            perms: fixtures::perms_with_probes(),
            target: crate::tui::ScanTarget::Cluster("cluster · 3 shards".into()),
        });
        let node = |keys: u64| {
            let mut f = ov::tests::figures(None);
            f.server.total_keys = keys;
            crate::scan::figures::NodeFigures {
                figures: Some(f),
                note: None,
            }
        };
        let mut merged = ov::tests::figures(None);
        merged.server = report.server.clone();
        app.apply(Msg::Figures {
            id,
            scan,
            figures: Figures {
                server: Some(merged),
                nodes: vec![node(400_000), node(401_000), node(402_000)],
                failed: None,
            },
        });
        app.apply(Msg::Nodes {
            id,
            scan,
            nodes: fixtures::live_nodes(),
        });
        app.apply(Msg::LiveReady { id, scan });
        let effects = testing::show(&mut app, Screen::Overview);
        feed_info(&mut app, effects);
        app
    }

    #[test]
    fn a_cluster_names_the_node_and_has_a_row_per_shard() {
        let app = cluster_connected();
        let t = text_of(&app, 80, 24);
        assert!(t.contains("[ ]"), "{t}");
        assert!(t.contains("SHARD"), "{t}");
        assert!(t.contains("10.0.1.11:6379"), "{t}");
        no_heading_without_rows(&t);
        insta::assert_snapshot!("cluster_80x24", t);
        let t = text_of(&app, 120, 36);
        no_heading_without_rows(&t);
        insta::assert_snapshot!("cluster_120x36", t);
        // Analyzed, at 80×24: what fits, every heading with a row.
        let mut app = cluster_connected();
        let (id, scan) = ids(&app);
        app.apply(Msg::ScanDone {
            id,
            scan,
            report: Box::new(fixtures::cluster_report()),
        });
        let t = text_of(&app, 80, 24);
        no_heading_without_rows(&t);
        insta::assert_snapshot!("cluster_analyzed_80x24", t);
        // The dialog splits the sample across the shards that answered.
        app.handle_key(key(KeyCode::Char('a')));
        let t = text_of(&app, 80, 24);
        assert!(t.contains("across 3 shards"), "{t}");
    }

    /// A shard whose figures did not come is not in the sum, and the title says so.
    #[test]
    fn the_sum_names_only_the_shards_that_answered() {
        let mut app = cluster_connected();
        let t = text_of(&app, 120, 36);
        assert!(t.contains("summed over 3 shards"), "{t}");
        let s = app.session.as_mut().unwrap();
        let f = s.figures.as_mut().unwrap();
        f.nodes[2].figures = None;
        let t = text_of(&app, 120, 36);
        assert!(t.contains("summed over 2 shards"), "{t}");
        assert!(!t.contains("summed over 3 shards"), "{t}");
    }

    #[test]
    fn the_dialog_shows_the_engine_estimate() {
        let app = just_connected(Some(3));
        let s = app.session.as_ref().unwrap();
        let rate = crate::scan::ScanOptions::for_profile(&s.profile).max_commands_per_sec;
        let keys = ov::tests::figures(Some(3)).server.total_keys;
        // The fixture server is LFU, and the permission check allowed OBJECT FREQ.
        let freq = ov::estimate_for(ov::Depth::Quick, &app).freq;
        let want = crate::scan::estimate::estimate_analysis(10_000, keys, true, freq, rate);
        let got = ov::estimate_for(ov::Depth::Quick, &app);
        assert_eq!((got.keys, got.commands), (want.keys, want.commands));
        assert_eq!(got.duration, want.duration);
        let mut app = app;
        app.handle_key(key(KeyCode::Char('a')));
        let t = text_of(&app, 80, 24);
        let line = format!("about {} commands", crate::format::count(want.commands));
        assert!(t.contains(&line), "{line}:\n{t}");
        insta::assert_snapshot!("dialog_80x24", t);
        app.handle_key(key(KeyCode::Down));
        insta::assert_snapshot!("dialog_deep_120x36", text_of(&app, 120, 36));
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        assert_eq!(
            wrap("one two three four", 10),
            [" one two", " three", " four"]
        );
        assert!(wrap("", 10).is_empty());
    }
}
