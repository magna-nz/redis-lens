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
//!
//! From 120 columns each block is a box of its own instead, in two columns under
//! "Now" ([`box_layout`] places them, [`draw_boxes`] draws them).

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
/// From this body width on, Overview draws its blocks in boxes under "Now", in two
/// columns: the findings at the left, the memory at the right (see [`box_layout`]).
pub const TWO_PANES: u16 = super::widgets::SIDE_AT;
/// The left column's share of the width.
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
    // From 120 columns, boxes under "Now", when there is room for them.
    if area.width >= TWO_PANES
        && two_pane_state(ctx.session)
        && draw_boxes(frame, area, app, ctx.session, &now, (&now_left, &now_right))
    {
        return;
    }
    // "Keys" keeps a row for the line that counts the reasons a part is off, taken from
    // "Now" when the body is that short.
    let now_rows = if keys_reasons(app, ctx.session) {
        now_rows.min(area.height.saturating_sub(4))
    } else {
        now_rows
    };
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

/// True when "Keys" on one column lists reasons that a part is off: "FROM THE SERVER"
/// is shown (before an analysis and while one runs) and some part is off.
fn keys_reasons(app: &App, s: &Session) -> bool {
    let server_block = match &s.phase {
        Phase::Connecting if s.analysis.is_none() => false,
        Phase::Failed(_) if app.report().is_none() => false,
        _ => app.analysis_running() || !shows_analysis(app),
    };
    server_block && !off_reasons(app).is_empty()
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
    /// How many of the last rows are reasons that a part is off: never cut without a
    /// line that counts them.
    reasons: usize,
}

impl Block {
    fn new(head: Option<Line<'static>>, rows: Vec<Line<'static>>) -> Self {
        Self {
            head: head.into_iter().collect(),
            rows,
            sel: None,
            reasons: 0,
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

/// How [`fit_blocks_sel`] lays the blocks out: the prefix lines kept, and per block
/// whether its heading is shown and its rows (0: not shown).
struct Plan {
    prefix: usize,
    heads: Vec<bool>,
    rows: Vec<usize>,
}

/// The plan of `blocks` under `prefix` lines in `h` rows. `force`: the block (one with
/// reasons) that is shown whatever it costs the others, with its heading, its selected
/// row and a line that counts its reasons when there is room, else that line alone.
fn plan_blocks(prefix: usize, blocks: &[Block], h: usize, force: Option<usize>) -> Plan {
    let k = blocks.len();
    let mut plan = Plan {
        prefix: prefix.min(h),
        heads: vec![true; k],
        rows: vec![0; k],
    };
    let mut left = h.saturating_sub(prefix);
    if let Some(f) = force {
        let b = &blocks[f];
        let finds = usize::from(b.rows.len() > b.reasons);
        let least = b.rows.len().min(1 + finds);
        let mut need = b.head.len() + least;
        if need > h {
            // The heading and the selected row go; the count line stays.
            plan.heads[f] = false;
            need = 1;
        }
        plan.prefix = prefix.min(h.saturating_sub(need));
        left = h.saturating_sub(plan.prefix + need);
        plan.rows[f] = if plan.heads[f] { least } else { 1 };
    }
    for (i, b) in blocks.iter().enumerate() {
        if plan.rows[i] > 0 {
            continue;
        }
        let shown_before = plan.rows[..i].iter().any(|&n| n > 0);
        let sep = usize::from(shown_before);
        // Showing the first block before the forced one puts a blank line above that.
        let pushes = usize::from(!shown_before && force.is_some_and(|f| f > i));
        let need = sep + pushes + b.head.len() + 1;
        if need > left {
            // Later blocks may be smaller; keep trying.
            continue;
        }
        left -= need;
        plan.rows[i] = 1;
    }
    for (i, b) in blocks.iter().enumerate() {
        if plan.rows[i] == 0 {
            continue;
        }
        let more = b.rows.len().saturating_sub(plan.rows[i]).min(left);
        plan.rows[i] += more;
        left -= more;
    }
    plan
}

/// [`fit_blocks`], and the rows drawn of the block with a selection (the findings),
/// 0 when it is not drawn: what PgUp and PgDn move by on Overview.
///
/// A block with reasons that a part is off never loses them silently: when its rows
/// are cut, its last row counts the reasons that are not shown (`… 2 more reasons: v
/// shows them`), and when it would not be shown at all it is shown first, at the cost
/// of the other blocks and the prefix.
fn fit_blocks_sel(
    prefix: Vec<Line<'static>>,
    blocks: Vec<Block>,
    h: usize,
) -> (Vec<Line<'static>>, usize) {
    let blocks: Vec<Block> = blocks.into_iter().filter(|b| !b.rows.is_empty()).collect();
    let mut plan = plan_blocks(prefix.len(), &blocks, h, None);
    if let Some(f) = blocks.iter().position(|b| b.reasons > 0)
        && plan.rows[f] == 0
        && h > 0
    {
        plan = plan_blocks(prefix.len(), &blocks, h, Some(f));
    }
    let mut sel_rows = 0;
    let mut out: Vec<Line<'static>> = prefix.into_iter().take(plan.prefix).collect();
    let mut first = true;
    for (i, b) in blocks.into_iter().enumerate() {
        let n = plan.rows[i];
        if n == 0 {
            continue;
        }
        if !first {
            out.push(Line::default());
        }
        first = false;
        if plan.heads[i] {
            out.extend(b.head);
        }
        let total = b.rows.len();
        let finds = total - b.reasons;
        // Cut with reasons out of view: the last row counts them.
        let in_view = |n: usize| {
            let from = b.sel.map_or(0, |s| scroll_offset(s, n));
            (from..(from + n).min(total))
                .filter(|&r| r >= finds)
                .count()
        };
        let counted = b.reasons > 0 && in_view(n) < b.reasons;
        let window = if counted { n - 1 } else { n };
        let from = b.sel.map_or(0, |s| scroll_offset(s, window));
        let shown = from..(from + window).min(total);
        if b.sel.is_some() {
            sel_rows = shown.clone().filter(|&r| r < finds).count();
        }
        let hidden = b.reasons - shown.clone().filter(|&r| r >= finds).count();
        out.extend(b.rows.into_iter().skip(from).take(window));
        if counted {
            let s_ = if hidden == 1 { "reason" } else { "reasons" };
            out.push(Line::from(Span::styled(
                format!("   … {hidden} more {s_}: v shows them"),
                style::dim(),
            )));
        }
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

/// Where the WHERE column starts in the boxes: as far right as its longest value
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
    let n = reasons.len();
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
    b.reasons = n;
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
        None => spans.push(Span::styled(FREED_NEEDS, style::dim())),
    }
    Line::from(fit(spans, w))
}

// ---------------------------------------------------------------------------
// Boxes (from 120 columns)
// ---------------------------------------------------------------------------

/// True when the boxes have something of their own to show: not while the first
/// connect runs, nor after one that failed with nothing to show (those say so on one
/// column, as at 80).
fn two_pane_state(s: &Session) -> bool {
    match &s.phase {
        Phase::Connecting => s.analysis.is_some(),
        Phase::Failed(_) => s.report.is_some(),
        _ => true,
    }
}

/// One way a column's two boxes under its filling box are drawn: each one's content
/// rows, `None` when it is not drawn. A column's steps go from all of it to the least.
type Step = [Option<u16>; 2];

/// What the boxes under "Now" hold, in content rows: the input of [`box_layout`].
#[derive(Debug, Clone, Default)]
struct Wants {
    now: u16,
    /// Rows "Things to look at" keeps before the other boxes give up theirs (its header
    /// and findings, at most [`THINGS_WANT`]).
    things: u16,
    /// Rows "Things to look at" needs to show every finding.
    things_all: u16,
    /// The left column's steps, "Selected" and "Off"; at least one.
    left: Vec<Step>,
    /// The right column's steps, "Freed by TTL" and "Slowest"; at least one.
    right: Vec<Step>,
}

/// Where each box goes, borders included; a box that is not drawn is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Boxes {
    now: Rect,
    things: Rect,
    selected: Option<Rect>,
    off: Option<Rect>,
    memory: Rect,
    freed: Option<Rect>,
    slow: Option<Rect>,
    /// Blank rows between two boxes: 1, or 0 once they were given up.
    gap: u16,
    /// The step each column draws (an index into [`Wants::left`] and [`Wants::right`]).
    left_step: usize,
    right_step: usize,
}

/// Content rows "Where the memory is" keeps before the boxes under it give up theirs.
const FILL_MIN: u16 = 5;
/// The content rows "Things to look at" and "Where the memory is" get at the least,
/// once every other box has given up what it can.
const FILL_FLOOR: u16 = 3;
/// The most rows "Things to look at" keeps before the other boxes give up theirs: its
/// header and five findings.
const THINGS_WANT: u16 = 6;
/// The columns' rows at the least (borders included); with fewer, one column.
const COLS_MIN_H: u16 = 8;
/// A box's top and bottom border.
const BORDERS: u16 = 2;

/// The widths of the two columns of boxes in a body `width` wide: the left (the
/// findings) about 55%, one blank column, the right the rest.
fn column_widths(width: u16) -> (u16, u16) {
    let both = width.saturating_sub(1);
    let left = super::widgets::split_at(both, THINGS_SHARE);
    (left, both - left)
}

/// The rows a step's boxes take, borders and the blank rows above them included.
fn step_cost(s: &Step, gap: u16) -> u16 {
    s.iter().flatten().fold(0u16, |a, n| {
        a.saturating_add(n.saturating_add(BORDERS + gap))
    })
}

/// The step a column draws in `h` rows, and the outer height its filling box gets:
/// the first step that leaves the filling box `want` content rows; else, when
/// `give` allows giving up rows, the last step, if it leaves `FILL_FLOOR`.
fn fit_column(h: u16, gap: u16, steps: &[Step], want: u16, give: bool) -> Option<(usize, u16)> {
    let n = if give { steps.len() } else { 1 };
    for (i, s) in steps.iter().take(n).enumerate() {
        if let Some(fill) = h.checked_sub(step_cost(s, gap))
            && fill >= want + BORDERS
        {
            return Some((i, fill));
        }
    }
    if !give {
        return None;
    }
    let last = steps.len().checked_sub(1)?;
    let fill = h.checked_sub(step_cost(&steps[last], gap))?;
    (fill >= FILL_FLOOR + BORDERS).then_some((last, fill))
}

/// What [`place`] decides: the gap, and each column's step with its filling box's
/// outer height.
type Placed = (u16, (usize, u16), (usize, u16));

/// The gap, each column's step and its filling box's outer height in `area`, before
/// "Things to look at" is held to what a taller terminal gives it.
fn place(area: Rect, w: &Wants) -> Option<Placed> {
    let (lw, rw) = column_widths(area.width);
    if lw < 12 || rw < 12 || w.left.is_empty() || w.right.is_empty() {
        return None;
    }
    let now_h = w.now.checked_add(BORDERS)?;
    let h = area.height.checked_sub(now_h)?;
    if h < COLS_MIN_H {
        return None;
    }
    // The blank rows stay while every box is whole beside them.
    if let Some(h1) = h.checked_sub(1)
        && let Some(l) = fit_column(h1, 1, &w.left, w.things, false)
        && let Some(r) = fit_column(h1, 1, &w.right, FILL_MIN, false)
    {
        return Some((1, l, r));
    }
    let l = fit_column(h, 0, &w.left, w.things, true)?;
    let r = fit_column(h, 0, &w.right, FILL_MIN, true)?;
    Some((0, l, r))
}

/// Where the boxes go in `area` (the body): "Now" across the top, the left column
/// under it at the left and the right column at the right, a blank row between two
/// boxes and a blank column between the columns.
///
/// When the rows do not fit, they are given up in this order: first the blank rows;
/// then each column on its own goes through its steps (the right: "Slowest", then
/// "Freed by TTL" short, then without it; the left: "Selected" short, then "Off" a
/// reason at a time down to the line that counts them, then without "Selected"), while
/// its filling box keeps [`THINGS_WANT`] or [`FILL_MIN`] rows; only then is the filling
/// box squeezed, to [`FILL_FLOOR`] rows at the least. "Things to look at" never gets
/// more rows than a taller terminal gives it (the rest stay blank under the column),
/// so a shorter terminal never shows more findings. `None` when the columns would get fewer
/// than [`COLS_MIN_H`] rows, when they are too narrow, or when a column does not fit
/// even then: Overview is then drawn on one column.
fn box_layout(area: Rect, w: &Wants) -> Option<Boxes> {
    let (gap, (li, lraw), (ri, rfill)) = place(area, w)?;
    // Held to the least a taller terminal gives it. With the blank rows every box is
    // whole, and a taller terminal only gives it more.
    let mut lfill = lraw;
    if gap == 0 {
        let reach = step_cost(&w.left[0], 1)
            .saturating_add(step_cost(&w.right[0], 1))
            .saturating_add(w.things_all)
            .saturating_add(8);
        for k in 1..=reach {
            let taller = Rect {
                height: area.height.saturating_add(k),
                ..area
            };
            if taller.height == area.height {
                break;
            }
            if let Some((_, (_, l), _)) = place(taller, w) {
                lfill = lfill.min(l);
            }
        }
    }
    let (lw, rw) = column_widths(area.width);
    let now_h = w.now + BORDERS;
    let top = area.y + now_h + gap;
    let stack = |x: u16, width: u16, fill: u16, step: &Step| {
        let mut y = top;
        [
            Some(fill),
            step[0].map(|n| n + BORDERS),
            step[1].map(|n| n + BORDERS),
        ]
        .map(|h| {
            h.map(|h| {
                let r = Rect::new(x, y, width, h);
                y = y.saturating_add(h).saturating_add(gap);
                r
            })
        })
    };
    let [things, selected, off] = stack(area.x, lw, lfill, &w.left[li]);
    let [memory, freed, slow] = stack(area.x.saturating_add(lw + 1), rw, rfill, &w.right[ri]);
    Some(Boxes {
        now: Rect {
            height: now_h,
            ..area
        },
        things: things?,
        selected,
        off,
        memory: memory?,
        freed,
        slow,
        gap,
        left_step: li,
        right_step: ri,
    })
}

/// The left column's steps: everything; "Selected" short; without "Selected" (its
/// finding is still in the list); then "Off" a reason at a time down to the line that
/// counts them. A reason is the last thing to go. `sel` is "Selected"'s rows and its
/// short form's, `off` the sizes "Off" can take, largest first.
fn left_steps(sel: Option<(u16, u16)>, off: &[u16]) -> Vec<Step> {
    let first = off.first().copied();
    let mut out = vec![
        [sel.map(|s| s.0), first],
        [sel.map(|s| s.1.min(s.0)), first],
        [None, first],
    ];
    out.extend(off.iter().skip(1).map(|o| [None, Some(*o)]));
    out.dedup();
    out
}

/// The right column's steps: everything; without "Slowest"; "Freed by TTL" short;
/// without it. `freed` is its rows and its short form's.
fn right_steps(freed: Option<(u16, u16)>, slow: Option<u16>) -> Vec<Step> {
    let mut out = vec![
        [freed.map(|f| f.0), slow],
        [freed.map(|f| f.0), None],
        [freed.map(|f| f.1.min(f.0)), None],
        [None, None],
    ];
    out.dedup();
    out
}

/// What the "Off" box holds: a stopped analysis's note, each reason that is off
/// (wrapped), and the provider's notice.
struct OffParts {
    notes: Vec<Line<'static>>,
    reasons: Vec<Vec<Line<'static>>>,
    provider: Vec<Line<'static>>,
    /// The notes there are: the stopped analysis's and the provider's, 0 to 2.
    note_count: usize,
    /// `v` is on: everything is meant to show, so the count line does not offer it.
    more: bool,
}

impl OffParts {
    fn new(app: &App, s: &Session, w: usize) -> Self {
        let mut notes = Vec::new();
        if stopped_early(s)
            && let Some(r) = app.report()
        {
            notes.extend(wrap_lines(
                &format!(
                    " The analysis was stopped early: these come from {} keys only.",
                    format::grouped(r.sampled_keys)
                ),
                w,
                style::dim(),
            ));
        }
        let reasons = off_reasons(app)
            .iter()
            .map(|r| wrap_lines(&format!(" Off: {}", off_text(r)), w, style::dim()))
            .collect();
        // The provider's notice wrapped, not cut: the box has the rows for it.
        let provider: Vec<Line<'static>> = crate::conn::provider::notice(s.profile.provider)
            .map(|n| wrap_lines(&format!(" ▲ {n}"), w, style::warning()))
            .unwrap_or_default();
        let note_count = usize::from(!notes.is_empty()) + usize::from(!provider.is_empty());
        Self {
            notes,
            reasons,
            provider,
            note_count,
            more: app.more_here(),
        }
    }

    fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.reasons.is_empty() && self.provider.is_empty()
    }

    /// "Off" when it holds only reasons that are off, else "Notes".
    fn title(&self) -> &'static str {
        if self.note_count == 0 { "Off" } else { "Notes" }
    }

    /// The rows of each form it can take, largest first: everything; the notes, the
    /// first reasons and a line that counts the rest, one reason less each time; one
    /// line that counts the reasons and the notes.
    fn sizes(&self) -> Vec<u16> {
        if self.is_empty() {
            return Vec::new();
        }
        let n = |v: usize| u16::try_from(v).unwrap_or(u16::MAX);
        let lens: Vec<usize> = self.reasons.iter().map(Vec::len).collect();
        let fixed = self.notes.len() + self.provider.len();
        let mut out = vec![n(fixed + lens.iter().sum::<usize>())];
        for j in (0..lens.len()).rev() {
            out.push(n(fixed + lens[..j].iter().sum::<usize>() + 1));
        }
        out.push(1);
        out.sort_unstable_by(|a, b| b.cmp(a));
        out.dedup();
        out
    }

    /// Its lines in `room` rows: the largest of [`OffParts::sizes`] that fits.
    fn lines(self, room: usize, w: usize) -> Vec<Line<'static>> {
        let lens: Vec<usize> = self.reasons.iter().map(Vec::len).collect();
        let fixed = self.notes.len() + self.provider.len();
        let all = fixed + lens.iter().sum::<usize>();
        if all <= room {
            let mut out = self.notes;
            out.extend(self.reasons.into_iter().flatten());
            out.extend(self.provider);
            return out;
        }
        // Whole reasons while they leave a line for the count, with the notes.
        let shown = (0..lens.len())
            .rev()
            .find(|&j| fixed + lens[..j].iter().sum::<usize>() < room)
            .filter(|_| fixed < room);
        // With `v` on, what it would offer is on already: say they do not fit.
        let how = if self.more {
            " do not fit"
        } else {
            ": v shows them"
        };
        let count = |reasons: usize, notes: usize| {
            let r = if reasons == 1 { "reason" } else { "reasons" };
            let n = if notes == 1 { "note" } else { "notes" };
            let text = match (reasons, notes) {
                (0, k) => format!(" … {k} {n}{how}"),
                (k, 0) => format!(" … {k} more {r}{how}"),
                (k, m) => format!(" … {k} more {r} and {m} {n}{how}"),
            };
            Line::from(Span::styled(trunc(&text, w), style::dim()))
        };
        match shown {
            Some(j) => {
                let mut out = self.notes;
                let cut = self.reasons.len() - j;
                out.extend(self.reasons.into_iter().take(j).flatten());
                out.push(count(cut, 0));
                out.extend(self.provider);
                out
            }
            None => vec![count(self.reasons.len(), self.note_count)],
        }
    }
}

/// The selected finding explained, as the "Selected" box holds it: its sentence and
/// its hint (for a recommendation), wrapped; its short form, the sentence on one line;
/// and the `Area › Panel` `enter` opens, which the box's title says.
fn selected_lines(f: &Finding, w: usize) -> (Vec<Line<'static>>, Vec<Line<'static>>, String) {
    let (mark, st) = match f.severity {
        Severity::Warn | Severity::Note => ("▲", style::warning()),
        Severity::Info => ("●", style::dim()),
    };
    let to = match f.to.panel() {
        Some(p) => format!("{} › {}", f.to.area().title(), p.name()),
        None => f.to.area().title().to_string(),
    };
    let sentence = format!(" {mark} {}", f.text);
    let mut full = wrap_lines(&sentence, w, st);
    if !f.hint.is_empty() {
        full.extend(wrap_lines(&format!(" {}", f.hint), w, style::fg()));
    }
    let short = vec![Line::from(Span::styled(trunc(&sentence, w), st))];
    (full, short, to)
}

/// "Now" across the top, then the findings, the selected one explained and what is
/// off in boxes at the left, the memory, what TTLs free and the slow log's slowest in
/// boxes at the right (see [`box_layout`]). False, with nothing drawn, when they do not
/// fit: Overview is then drawn on one column.
fn draw_boxes(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    s: &Session,
    now: &[Line<'static>],
    (now_left, now_right): (&str, &str),
) -> bool {
    let (lw, rw) = column_widths(area.width);
    let lw_in = usize::from(lw.saturating_sub(2));
    let rw_in = usize::from(rw.saturating_sub(2));
    let found = findings(app);
    let sel = app.overview.sel.min(found.len().saturating_sub(1));
    let at = place_col_for(&found, lw_in);
    let selected = found.get(sel).map(|f| selected_lines(f, lw_in));
    let off = OffParts::new(app, s, lw_in);
    // What TTLs free and the slowest in the slow log, once an analysis is shown.
    let report = if app.analysis_running() || !shows_analysis(app) {
        None
    } else {
        app.report()
    };
    let freed = report.map(|r| freed_box(r, rw_in));
    let slow = report
        .and_then(|_| slow_block(&app.views.server.slow.entries, rw_in))
        .map(|b| b.rows);
    let n = |v: usize| u16::try_from(v).unwrap_or(u16::MAX);
    let things_all = n(found.len().max(1) + 1);
    let wants = Wants {
        now: n(now.len()),
        things: things_all.clamp(FILL_FLOOR, THINGS_WANT),
        things_all,
        left: left_steps(
            selected
                .as_ref()
                .map(|(full, short, _)| (n(full.len()), n(short.len().max(1)))),
            &off.sizes(),
        ),
        right: right_steps(
            freed.as_ref().map(|f| (n(f.full.len()), n(f.short.len()))),
            slow.as_ref().map(|l| n(l.len())),
        ),
    };
    let Some(b) = box_layout(area, &wants) else {
        return false;
    };
    let inner = super::widgets::titled_box(frame, b.now, Title::new(now_left, now_right), false);
    rows(frame, inner, now.to_vec(), None);
    // "Things to look at" has the keys: `↑↓` move its selection, `enter` opens it.
    let list_h = usize::from(b.things.height.saturating_sub(BORDERS + 1));
    let drawn = found.len().min(list_h);
    let (things_right, mem_right) = pane_rights(app, s, drawn);
    let inner = super::widgets::titled_box(
        frame,
        b.things,
        Title::new("Things to look at", &things_right),
        true,
    );
    things_box(frame, inner, app, &found, sel, at);
    if let (Some(r), Some((full, short, to))) = (b.selected, selected) {
        let right = format!("enter opens {to}");
        let inner = super::widgets::titled_box(frame, r, Title::new("Selected", &right), false);
        let lines = if usize::from(inner.height) >= full.len() {
            full
        } else {
            short
        };
        rows(frame, inner, lines, None);
    }
    if let Some(r) = b.off {
        let right = match off.reasons.len() {
            0 => String::new(),
            1 => "1 reason".to_string(),
            n => format!("{n} reasons"),
        };
        let inner = super::widgets::titled_box(frame, r, Title::new(off.title(), &right), false);
        rows(
            frame,
            inner,
            off.lines(usize::from(inner.height), lw_in),
            None,
        );
    }
    let inner = super::widgets::titled_box(
        frame,
        b.memory,
        Title::new("Where the memory is", &mem_right),
        false,
    );
    memory_box(frame, inner, app, s);
    if let (Some(r), Some(f)) = (b.freed, freed) {
        let inner =
            super::widgets::titled_box(frame, r, Title::new("Freed by TTL", &f.right), false);
        let lines = if usize::from(inner.height) >= f.full.len() {
            f.full
        } else {
            f.short
        };
        rows(frame, inner, lines, None);
    }
    if let (Some(r), Some(lines)) = (b.slow, slow) {
        let inner = super::widgets::titled_box(
            frame,
            r,
            Title::new("Slowest in the slow log", "Server › Slow log"),
            false,
        );
        rows(frame, inner, lines, None);
    }
    true
}

/// The boxes' right titles of the findings and the memory: how many findings (`7 of 8
/// findings` when `drawn` of them fit) and where from; the sample, or what an
/// analysis costs.
fn pane_rights(app: &App, s: &Session, drawn: usize) -> (String, String) {
    let n = findings(app).len();
    let found = match n {
        0 => String::new(),
        n if drawn < n => format!("{drawn} of {n} findings"),
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
        // The time is in the context bar ("analyzed 14:02 UTC"); the box's title has
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
    // The box's title is short: on a cluster how many shards are summed, else that
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

/// "Things to look at": the findings with their WHERE column (`↑↓` moves, `enter`
/// goes), as many as its rows hold, the selected one kept on screen.
fn things_box(frame: &mut Frame, inner: Rect, app: &App, found: &[Finding], sel: usize, at: usize) {
    let w = usize::from(inner.width);
    let h = usize::from(inner.height);
    app.overview.drawn_rows.set(0);
    if w == 0 || h == 0 {
        return;
    }
    let mut lines = vec![where_head_at("FINDING", "", w, at)];
    if found.is_empty() {
        let none = if shows_analysis(app) {
            "   Nothing stands out in the sample."
        } else {
            "   Nothing stands out in what the server reports."
        };
        lines.push(Line::from(Span::styled(trunc(none, w), style::dim())));
    } else {
        let list_h = h - 1;
        let off = scroll_offset(sel, list_h);
        let drawn: Vec<Line<'static>> = finding_rows_at(found, sel, w, at)
            .into_iter()
            .skip(off)
            .take(list_h)
            .collect();
        app.overview.drawn_rows.set(drawn.len());
        lines.extend(drawn);
    }
    rows(frame, inner, lines, None);
}

/// "Where the memory is": the server's headline, then before an analysis what one
/// costs, while one runs its progress, and after it every top pattern that fits with
/// its share and size.
fn memory_box(frame: &mut Frame, inner: Rect, app: &App, s: &Session) {
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
        // The rows under the patterns: why there are no sizes, that there are no keys.
        let notes = usize::from(!r.sizes_available) + usize::from(r.patterns.is_empty());
        let top = h.saturating_sub(prefix.len() + notes).max(1);
        // Every pattern when they fit; else one row less, and one for the rest.
        let named = if r.patterns.len() <= top {
            top
        } else {
            top - 1
        };
        // The box's title already says "Where the memory is": no heading again.
        let mut patterns = memory_block(r, w, named, share_widths(w));
        patterns.head.clear();
        blocks.push(patterns);
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

/// What the "Freed by TTL" box holds: the bars (all of it, short or not), or why it is
/// not known (wrapped, and cut to one line when short); and its right title.
struct Freed {
    full: Vec<Line<'static>>,
    short: Vec<Line<'static>>,
    right: String,
}

fn freed_box(r: &ScanReport, w: usize) -> Freed {
    let rows_ = freed_block(r, w).map(|b| b.rows).unwrap_or_default();
    match expiry(r) {
        Some(e) => Freed {
            short: rows_.clone(),
            full: rows_,
            right: format!("of {}", format::bytes(e.total)),
        },
        None => Freed {
            short: vec![Line::from(Span::styled(
                trunc(&format!("   {FREED_NEEDS}"), w),
                style::dim(),
            ))],
            full: rows_,
            right: String::new(),
        },
    }
}

/// An "off" reason after the box's "Off:": `Slow log is off: why` reads `Slow log:
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

/// Why "Freed by TTL" is not known.
const FREED_NEEDS: &str = "needs key sizes (MEMORY USAGE), which this analysis did not read";

/// "FREED BY TTL" as bars: the memory that expires within an hour, a day and a week,
/// and that never does, of the sample's total; or why it is not known.
fn freed_block(r: &ScanReport, w: usize) -> Option<Block> {
    let head = Line::from(Span::styled(" FREED BY TTL", style::hd()));
    let Some(e) = expiry(r) else {
        return Some(Block::new(
            Some(head),
            wrap_spans(
                vec![Span::styled(format!("   {FREED_NEEDS}"), style::dim())],
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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

    /// Every size up to 60×16, and around where the boxes start, for each state the
    /// boxes draw: no panic.
    #[test]
    fn every_size_does_not_panic() {
        let mut with_more = all_six();
        with_more.handle_key(key(KeyCode::Char('v')));
        let apps = [
            overview_app(false),
            just_connected(Some(3)),
            analyzing(),
            all_six(),
            with_more,
            cluster_connected(),
        ];
        for app in &apps {
            for w in 0..=60 {
                for h in 0..=16 {
                    testing::render(app, w, h);
                }
            }
            for (w, h) in [
                (119, 16),
                (120, 10),
                (120, 16),
                (120, 24),
                (121, 17),
                (200, 60),
                (u16::MAX, 6),
            ] {
                testing::render(app, w, h);
            }
            for h in 0..40 {
                testing::render(app, 120, h);
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
        // There "Now" has every figure and every reason does not fit beside the
        // findings' least rows: the line that counts them says they do not fit.
        let all = text_of(&app, 120, 22);
        assert!(all.contains("more reasons do not fit"), "{all}");
        assert!(!all.contains("v shows them"), "{all}");
        // Taller, in the "Off" box.
        let all = text_of(&app, 120, 36);
        assert!(!all.contains("more reason"), "{all}");
        for want in ["Off: Slow log: ", "Off: Idle clients: ", "─ Off ─"] {
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

    /// [`analyzing`] finished with sizes, so "Freed by TTL" has its bars.
    fn analyzed_with_sizes() -> App {
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
        app
    }

    /// [`analyzed_with_sizes`] with two parts off: every box of the layout is drawn.
    fn all_six() -> App {
        let mut app = analyzed_with_sizes();
        let s = app.session.as_mut().unwrap();
        let f = s.figures.as_mut().unwrap().server.as_mut().unwrap();
        f.context.missing.push(crate::model::ContextGap::NoKeysizes);
        f.context
            .missing
            .push(crate::model::ContextGap::MemoryStatsDenied);
        app
    }

    /// The box titles of the layout from 120 columns, as they read in the top border.
    const BOX_TITLES: [&str; 7] = [
        "─ Now ─",
        "─ Things to look at ─",
        "─ Selected ─",
        "─ Off ─",
        "─ Where the memory is ─",
        "─ Freed by TTL ─",
        "─ Slowest in the slow log ─",
    ];

    /// Every box on `screen` is closed: each `┌` has a `┐` on its row, and under it a
    /// `└` in its column with `┘` at the matching column.
    fn boxes_closed(t: &Terminal<TestBackend>) {
        let buf = t.backend().buffer();
        let (w, h) = (buf.area.width, buf.area.height);
        let sym = |x: u16, y: u16| buf[(x, y)].symbol().to_string();
        for y in 0..h {
            for x in 0..w {
                if sym(x, y) != "┌" {
                    continue;
                }
                let right = (x + 1..w)
                    .find(|&xx| sym(xx, y) == "┐")
                    .unwrap_or_else(|| panic!("open box at {x},{y}:\n{}", t.backend()));
                let bottom = (y + 1..h)
                    .find(|&yy| sym(x, yy) == "└")
                    .unwrap_or_else(|| panic!("no bottom at {x},{y}:\n{}", t.backend()));
                assert_eq!(sym(right, bottom), "┘", "{x},{y}:\n{}", t.backend());
                for yy in y + 1..bottom {
                    assert_eq!(sym(x, yy), "│", "{x},{yy}:\n{}", t.backend());
                    assert_eq!(sym(right, yy), "│", "{right},{yy}:\n{}", t.backend());
                }
            }
        }
    }

    /// The finding rows on screen: `›` or a blank cursor, then `▲` or `●` at column 4,
    /// inside a frame that starts at column 0.
    fn finding_rows_on(t: &Terminal<TestBackend>) -> usize {
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .filter(|&y| {
                buf.area.width > 4
                    && buf[(0, y)].symbol() == "│"
                    && matches!(buf[(4, y)].symbol(), "▲" | "●")
                    && matches!(buf[(2, y)].symbol(), "›" | " ")
            })
            .count()
    }

    #[test]
    fn the_boxes_from_120_columns() {
        // Every box, each titled and closed.
        let app = all_six();
        let t = testing::render(&app, 120, 36);
        let text = t.backend().to_string();
        for title in BOX_TITLES {
            assert!(text.contains(title), "{title}:\n{text}");
        }
        for right in [
            "8 findings",
            "enter opens Analyze › Expiry",
            "2 reasons",
            "100,000 sampled keys",
            "of 9.0G",
            "Server › Slow log",
        ] {
            assert!(text.contains(right), "{right}:\n{text}");
        }
        boxes_closed(&t);
        assert!(!text.contains("secret-value"), "{text}");
        no_heading_without_rows(&text);
        insta::assert_snapshot!("boxes_all_six_120x36", text);
        boxes_closed(&testing::render(&app, 160, 45));

        // No slow-log entries: no "Slowest" box, the others as they were.
        let mut none = all_six();
        none.views.server.slow.entries.clear();
        let t = testing::render(&none, 120, 36);
        let text = t.backend().to_string();
        assert!(!text.contains("Slowest in the slow log"), "{text}");
        assert!(text.contains("─ Freed by TTL ─"), "{text}");
        boxes_closed(&t);
        insta::assert_snapshot!("boxes_no_slow_120x36", text);

        // No "Off" reasons: no "Off" box.
        let t = testing::render(&analyzed_with_sizes(), 120, 36);
        let text = t.backend().to_string();
        assert!(!text.contains("─ Off ─"), "{text}");
        boxes_closed(&t);

        // `enter opens …` is the "Selected" box's title only.
        let t = testing::render(&app, 120, 36).backend().to_string();
        assert_eq!(t.matches("enter opens Analyze › Expiry").count(), 1, "{t}");

        // A short terminal gives up the blank rows and "Slowest", then "Selected"; the
        // reasons stay.
        let t = testing::render(&app, 120, 24);
        let text = t.backend().to_string();
        assert!(!text.contains("Slowest in the slow log"), "{text}");
        assert!(!text.contains("─ Selected ─"), "{text}");
        for want in [
            "5 of 8 findings",
            "─ Off ─",
            "Off: Big keys",
            "Off: Data and overhead",
            "within 1 h",
            "never",
        ] {
            assert!(text.contains(want), "{want}:\n{text}");
        }
        boxes_closed(&t);
        insta::assert_snapshot!("boxes_short_120x24", text);

        // Shorter, the findings are cut: the title says how many show.
        let text = testing::render(&app, 120, 18).backend().to_string();
        assert!(text.contains("─ Things to look at ─"), "{text}");
        assert!(text.contains(" of 8 findings"), "{text}");
        assert!(text.contains("2 more reasons: v shows them"), "{text}");
        // The least two panes had (17 rows here), then one column.
        let text = testing::render(&app, 120, 17).backend().to_string();
        assert!(text.contains("─ Things to look at ─"), "{text}");
        let text = testing::render(&app, 120, 16).backend().to_string();
        assert!(!text.contains("─ Things to look at ─"), "{text}");
        assert!(text.contains("─ Keys ─"), "{text}");
    }

    /// Two reasons that the server refused (`overview_app_with` and the reads).
    fn refused() -> App {
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
        app
    }

    /// [`all_six`] with five reasons and the provider's notice.
    fn many_off() -> App {
        let mut app = all_six();
        let s = app.session.as_mut().unwrap();
        s.profile.provider = Some(crate::model::Provider::ElasticacheServerless);
        let f = s.figures.as_mut().unwrap().server.as_mut().unwrap();
        f.context
            .missing
            .push(crate::model::ContextGap::MemoryStatsUnavailable);
        f.context
            .missing
            .push(crate::model::ContextGap::KeyspaceUnavailable);
        let (id, scan) = ids(&app);
        app.apply(Msg::ServerReads {
            id,
            scan,
            node: 0,
            slow: None,
            clients: Some(Err(crate::live::LiveError::Denied("CLIENT LIST".into()))),
        });
        app
    }

    /// [`analyzing`] stopped early with what it had.
    fn stopped() -> App {
        let mut app = analyzing();
        let (id, scan) = ids(&app);
        app.handle_key(key(KeyCode::Esc));
        let mut r = fixtures::report(true);
        r.sampled_keys = 4_200;
        r.complete = false;
        app.apply(Msg::ScanDone {
            id,
            scan,
            report: Box::new(r),
        });
        app
    }

    /// `app` with `v` pressed.
    fn with_v(mut app: App) -> App {
        app.handle_key(key(KeyCode::Char('v')));
        app
    }

    /// Every state the boxes and the one column draw, named.
    fn states() -> Vec<(&'static str, App)> {
        let mut provider = overview_app(false);
        provider.session.as_mut().unwrap().profile.provider =
            Some(crate::model::Provider::ElasticacheServerless);
        let mut no_slow = all_six();
        no_slow.views.server.slow.entries.clear();
        let mut far = all_six();
        for _ in 0..7 {
            far.handle_key(key(KeyCode::Down));
        }
        vec![
            ("before", overview_app(false)),
            ("connected", just_connected(Some(3))),
            ("analyzing", analyzing()),
            ("all_six", all_six()),
            ("no_off", analyzed_with_sizes()),
            ("no_slow", no_slow),
            ("far", far),
            ("cluster", cluster_connected()),
            ("refused", refused()),
            ("many_off", many_off()),
            ("stopped", stopped()),
            ("provider", provider),
            ("v_all_six", with_v(all_six())),
            ("v_refused", with_v(refused())),
            ("v_many_off", with_v(many_off())),
            ("v_stopped", with_v(stopped())),
        ]
    }

    /// The off reasons on screen: those shown (a box's `Off:` lines, the one column's
    /// `·` rows) and those counted by a `… n more reasons` line.
    fn reasons_seen(text: &str) -> (usize, usize) {
        let shown = text
            .lines()
            .filter(|l| {
                l.contains("│ Off: ")
                    || (l.contains("│   · ")
                        && !l.contains("· The analysis was stopped")
                        && !l.contains("· Sizes need"))
            })
            .count();
        let counted = text
            .split("… ")
            .skip(1)
            .filter_map(|s| {
                let n: String = s.chars().take_while(char::is_ascii_digit).collect();
                s[n.len()..]
                    .starts_with(" more reason")
                    .then(|| n.parse::<usize>().ok())
                    .flatten()
            })
            .sum();
        (shown, counted)
    }

    /// Every reason a part is off is shown or counted, at every width and height that
    /// has a row for it; with `v` the boxes show them all.
    #[test]
    fn no_off_reason_is_hidden() {
        for (name, app) in states() {
            let total = ov::off_reasons(&app).len();
            let s = app.session.as_ref().unwrap();
            for w in [80u16, 100, 119, 120, 140, 160, 200] {
                // Height 6 leaves no row inside the frame.
                for h in 7..=45u16 {
                    let text = testing::render(&app, w, h).backend().to_string();
                    let boxes = text.contains("─ Things to look at ─");
                    // On one column after an analysis, "Keys" has no reasons (as on
                    // main): the one case left, see the spec.
                    if !boxes && !keys_reasons(&app, s) {
                        continue;
                    }
                    let (shown, counted) = reasons_seen(&text);
                    assert_eq!(
                        shown + counted,
                        total,
                        "{name} {w}x{h}: {shown} shown + {counted} counted:\n{text}"
                    );
                    // With `v` the boxes give up "Selected" before a reason; a
                    // count then says they do not fit.
                    if boxes && app.more_here() && counted > 0 {
                        assert!(!text.contains("─ Selected ─"), "{name} {w}x{h}:\n{text}");
                        assert!(text.contains("do not fit"), "{name} {w}x{h}:\n{text}");
                    }
                    if boxes && app.more_here() && h >= 36 {
                        assert_eq!(counted, 0, "{name} {w}x{h} with v:\n{text}");
                    }
                }
            }
        }
    }

    /// In the boxes, a terminal a row shorter never shows more findings, and the switch
    /// to one column never shows more either. (Below that, the one column fits its
    /// blocks as below 120 columns, as on main.)
    #[test]
    fn a_shorter_terminal_never_shows_more_findings() {
        for (name, app) in states() {
            for w in [120u16, 140, 160, 200] {
                let mut taller: Option<(usize, bool)> = None;
                for h in (7..=60u16).rev() {
                    let t = testing::render(&app, w, h);
                    let drawn = finding_rows_on(&t);
                    let boxes = t.backend().to_string().contains("─ Things to look at ─");
                    if let Some((up, up_boxes)) = taller
                        && up_boxes
                    {
                        assert!(
                            drawn <= up,
                            "{name} {w}x{h}: {drawn} findings, {up} one row taller:\n{}",
                            t.backend()
                        );
                    }
                    taller = Some((drawn, boxes));
                }
            }
        }
    }

    #[test]
    fn the_boxes_at_20_rows() {
        // Two reasons, four (refused) and five with the provider's notice: each shown
        // or counted, the box sized to what it shows.
        for (name, app) in [
            ("off2", all_six()),
            ("off4", refused()),
            ("off5_notice", many_off()),
        ] {
            let t = testing::render(&app, 120, 20);
            let text = t.backend().to_string();
            assert!(text.contains("─ Things to look at ─"), "{name}:\n{text}");
            boxes_closed(&t);
            let (shown, counted) = reasons_seen(&text);
            assert_eq!(
                shown + counted,
                ov::off_reasons(&app).len(),
                "{name}:\n{text}"
            );
            insta::assert_snapshot!(format!("boxes_{name}_120x20"), text);
        }
        // The provider's notice and a stopped analysis are notes, not parts off.
        let text = testing::render(&many_off(), 120, 45).backend().to_string();
        assert!(text.contains("─ Notes ─"), "{text}");
        assert!(text.contains("5 reasons"), "{text}");
        let text = testing::render(&stopped(), 120, 36).backend().to_string();
        assert!(text.contains("─ Notes ─"), "{text}");
        let text = testing::render(&all_six(), 120, 36).backend().to_string();
        assert!(
            text.contains("─ Off ─") && !text.contains("─ Notes ─"),
            "{text}"
        );
    }

    /// The focused box ("Things to look at") has its border in the focus colour; the
    /// other boxes have the normal border.
    #[test]
    fn the_focused_box_is_drawn_in_the_focus_colour() {
        use crate::tui::ui::style::Mode;
        let app = all_six();
        style::with_mode(Mode::Dark, || {
            let t = testing::render(&app, 120, 36);
            let buf = t.backend().buffer();
            let lines: Vec<String> = (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect()
                })
                .collect();
            let (focus, border) = (style::focus().fg, style::border().fg);
            assert_ne!(focus, border);
            let mut seen = 0;
            for (y, l) in lines.iter().enumerate() {
                let y = y as u16;
                let cols: Vec<String> = l.chars().map(String::from).collect();
                for (x, c) in cols.iter().enumerate() {
                    if c != "┌" {
                        continue;
                    }
                    let rest: String = cols[x..].concat();
                    let focused = rest.starts_with("┌─ Things to look at ─");
                    let cell = &buf[(x as u16, y)];
                    let side = &buf[(x as u16, y + 1)];
                    if focused {
                        assert_eq!(cell.fg, focus.unwrap(), "Things corner");
                        assert_eq!(side.fg, focus.unwrap(), "Things side");
                        // Its title bold in the focus colour.
                        let tx = x as u16 + 3;
                        assert_eq!(buf[(tx, y)].symbol(), "T");
                        assert_eq!(buf[(tx, y)].fg, focus.unwrap());
                        assert!(
                            buf[(tx, y)]
                                .modifier
                                .contains(ratatui::style::Modifier::BOLD)
                        );
                    } else {
                        assert_eq!(cell.fg, border.unwrap(), "box at {x},{y}: {rest}");
                        assert_eq!(side.fg, border.unwrap(), "box side at {x},{y}");
                    }
                    seen += 1;
                }
            }
            // "Now" and the six boxes.
            assert_eq!(seen, 7, "{}", t.backend());
        });
    }

    /// PgUp and PgDn move by the finding rows drawn, at every size.
    #[test]
    fn drawn_rows_is_the_finding_rows_drawn() {
        let sizes = [
            (80, 24),
            (120, 36),
            (160, 45),
            (120, 24),
            (120, 16),
            (120, 20),
            (200, 60),
        ];
        let mut apps = vec![
            all_six(),
            just_connected(Some(3)),
            analyzing(),
            overview_app(false),
        ];
        // The selection further down the list, so it scrolls.
        let mut far = all_six();
        for _ in 0..7 {
            far.handle_key(key(KeyCode::Down));
        }
        apps.push(far);
        for app in &apps {
            for (w, h) in sizes {
                let t = testing::render(app, w, h);
                let rows = crate::tui::ui::list_rows(Rect::new(0, 0, w, h), app);
                assert_eq!(rows, finding_rows_on(&t), "{w}x{h}:\n{}", t.backend());
            }
        }
    }

    /// `↓` and `enter` do at every size what they do on one column at 80.
    #[test]
    fn the_findings_move_and_open_the_same_in_the_boxes() {
        let sizes = [(80, 24), (120, 36), (160, 45), (120, 24), (120, 16)];
        for downs in 0..4 {
            let mut want = None;
            for (w, h) in sizes {
                let mut app = all_six();
                let _ = text_of(&app, w, h);
                for _ in 0..downs {
                    let effects = app.handle_key(key(KeyCode::Down));
                    assert!(effects.is_empty(), "{w}x{h}: {effects:?}");
                    let _ = text_of(&app, w, h);
                }
                let sel = app.overview.sel;
                let effects = app.handle_key(key(KeyCode::Enter));
                let got = (sel, app.screen(), format!("{effects:?}"));
                match &want {
                    None => want = Some(got),
                    Some(w0) => assert_eq!(&got, w0, "{w}x{h} after {downs} downs"),
                }
            }
        }
    }

    /// Wants like [`all_six`]'s at 120 columns: "Now" 4 rows, 8 findings (it keeps 6
    /// rows, 9 show them all), "Selected" 4 rows (1 short), "Off" two reasons of two
    /// lines (4, 3 or 1 rows), "Freed by TTL" 4 bars, "Slowest" 2.
    fn six() -> Wants {
        Wants {
            now: 4,
            things: THINGS_WANT,
            things_all: 9,
            left: left_steps(Some((4, 1)), &[4, 3, 1]),
            right: right_steps(Some((4, 4)), Some(2)),
        }
    }

    /// The body of a `w` × `h` terminal: under the two top rows, over the footer.
    fn body(w: u16, h: u16) -> Rect {
        Rect::new(0, 2, w, h.saturating_sub(3))
    }

    /// No two boxes overlap, each is inside `area`, and the filling boxes have their
    /// floor.
    fn sane(area: Rect, b: &Boxes) {
        let all: Vec<Rect> = [
            Some(b.now),
            Some(b.things),
            b.selected,
            b.off,
            Some(b.memory),
            b.freed,
            b.slow,
        ]
        .into_iter()
        .flatten()
        .collect();
        for (i, r) in all.iter().enumerate() {
            assert_eq!(r.intersection(area), *r, "{r:?} outside {area:?}");
            assert!(r.height > BORDERS && r.width >= 4, "{r:?}");
            for o in &all[i + 1..] {
                assert!(!r.intersects(*o), "{r:?} overlaps {o:?}");
            }
        }
        assert!(b.things.height >= FILL_FLOOR + BORDERS, "{b:?}");
        assert!(b.memory.height >= FILL_FLOOR + BORDERS, "{b:?}");
    }

    #[test]
    fn the_box_layout() {
        // 120×36: every box whole, a blank row between two, a blank column between the
        // columns, both columns down to the body's last row.
        let a = body(120, 36);
        let b = box_layout(a, &six()).unwrap();
        sane(a, &b);
        assert_eq!((b.gap, b.left_step, b.right_step), (1, 0, 0));
        assert_eq!(b.now, Rect::new(0, 2, 120, 6));
        assert_eq!(b.things, Rect::new(0, 9, 65, 12));
        assert_eq!(b.selected, Some(Rect::new(0, 22, 65, 6)));
        assert_eq!(b.off, Some(Rect::new(0, 29, 65, 6)));
        assert_eq!(b.memory, Rect::new(66, 9, 54, 14));
        assert_eq!(b.freed, Some(Rect::new(66, 24, 54, 6)));
        assert_eq!(b.slow, Some(Rect::new(66, 31, 54, 4)));
        assert_eq!(b.off.unwrap().bottom(), a.bottom());
        assert_eq!(b.slow.unwrap().bottom(), a.bottom());

        // 160×45: the same boxes, the filling ones taller.
        let a = body(160, 45);
        let b = box_layout(a, &six()).unwrap();
        sane(a, &b);
        assert_eq!((b.gap, b.left_step, b.right_step), (1, 0, 0));
        assert_eq!(b.things.height, 21);
        assert_eq!(b.memory.height, 23);
        assert_eq!(b.things.x + b.things.width + 1, b.memory.x);
        assert_eq!(b.memory.right(), a.right());

        // 120×30: the blank rows gone first, every box still whole; "Things to look
        // at" no taller than one row up gives it (5 findings).
        let b = box_layout(body(120, 30), &six()).unwrap();
        assert_eq!((b.gap, b.left_step, b.right_step), (0, 0, 0));
        assert_eq!(b.things.height, 8);
        // 120×28: "Selected" short.
        let b = box_layout(body(120, 28), &six()).unwrap();
        assert_eq!(b.selected.unwrap().height, 1 + BORDERS);
        assert_eq!(b.off.unwrap().height, 4 + BORDERS);

        // 120×24: "Slowest" gone at the right; "Selected" gone at the left, "Off"
        // still whole; the findings keep their six rows.
        let a = body(120, 24);
        let b = box_layout(a, &six()).unwrap();
        sane(a, &b);
        assert_eq!(b.gap, 0);
        assert_eq!((b.slow, b.selected), (None, None));
        assert_eq!(b.freed.unwrap().height, 4 + BORDERS);
        assert_eq!(b.off.unwrap().height, 4 + BORDERS);
        // Five findings: what one row taller gives it (the row left is blank).
        assert_eq!(b.things.height, 5 + BORDERS + 1);

        // 120×17, the least two panes had: "Things to look at" at its floor, "Off" its
        // count line, the right column only "Where the memory is"; 120×16: one column.
        let a = body(120, 17);
        let b = box_layout(a, &six()).unwrap();
        sane(a, &b);
        assert_eq!(b.selected, None);
        assert_eq!(b.off.unwrap().height, 1 + BORDERS);
        assert_eq!(b.things.height, FILL_FLOOR + BORDERS);
        assert_eq!((b.freed, b.slow), (None, None));
        assert_eq!(box_layout(body(120, 16), &six()), None);

        // Every height and a few widths: sane when it fits, and it fits from the
        // least two panes had.
        for w in [120, 121, 160, 200, 300, u16::MAX] {
            for h in 0..80 {
                let a = Rect::new(0, 0, w, h);
                let b = box_layout(a, &six());
                if h >= 4 + BORDERS + COLS_MIN_H {
                    assert!(b.is_some(), "{w}x{h}");
                }
                if let Some(b) = b {
                    sane(a, &b);
                }
            }
        }
        // Too narrow for two columns.
        assert_eq!(box_layout(Rect::new(0, 0, 20, 60), &six()), None);
    }

    /// Rows are given up in order as the height falls: first the blank rows, then each
    /// column's steps, then the filling boxes; "Off" is never dropped, and "Things to
    /// look at" never shows more findings.
    #[test]
    fn the_boxes_give_up_rows_in_order() {
        let wants = six();
        let mut last: Option<Boxes> = None;
        for h in (0..80u16).rev() {
            let a = Rect::new(0, 0, 120, h);
            let Some(b) = box_layout(a, &wants) else {
                // Once it does not fit, it never fits lower down.
                for lower in 0..h {
                    assert_eq!(box_layout(Rect::new(0, 0, 120, lower), &wants), None);
                }
                break;
            };
            sane(a, &b);
            // The blank rows only while every box is whole.
            if b.gap == 1 {
                assert_eq!((b.left_step, b.right_step), (0, 0), "{h}");
            }
            // The filling boxes squeezed only at the last step.
            if b.things.height < wants.things + BORDERS {
                assert_eq!(b.left_step, wants.left.len() - 1, "{h}");
            }
            if b.memory.height < FILL_MIN + BORDERS {
                assert_eq!(b.right_step, wants.right.len() - 1, "{h}");
            }
            // "Off" is always there.
            assert!(b.off.is_some(), "{h}");
            if let Some(p) = last {
                assert!(b.gap <= p.gap, "{h}");
                if b.gap == p.gap {
                    assert!(b.left_step >= p.left_step, "{h}");
                    assert!(b.right_step >= p.right_step, "{h}");
                }
                // The findings it shows (8 of them at the most).
                let shows = |b: &Boxes| (b.things.height - BORDERS - 1).min(8);
                assert!(shows(&b) <= shows(&p), "{h}");
            }
            last = Some(b);
        }
        assert!(last.is_some());
        // The steps, in order: the right column drops "Slowest" before "Freed by
        // TTL" goes short or goes; the left one shortens "Selected", drops it, then
        // gives up "Off" a reason at a time.
        assert_eq!(
            right_steps(Some((2, 1)), Some(3)),
            vec![
                [Some(2), Some(3)],
                [Some(2), None],
                [Some(1), None],
                [None, None]
            ]
        );
        assert_eq!(
            left_steps(Some((4, 1)), &[5, 3, 1]),
            vec![
                [Some(4), Some(5)],
                [Some(1), Some(5)],
                [None, Some(5)],
                [None, Some(3)],
                [None, Some(1)]
            ]
        );
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
        // Boxes: what TTLs free as bars, and the patterns that fit, the rest on one row.
        for want in [
            "─ Freed by TTL ─",
            "of 9.0G",
            "within 1 h",
            "within 7 d",
            "never",
            "3 other patterns",
        ] {
            assert!(t.contains(want), "{want}:\n{t}");
        }
        // No box repeats its title inside.
        for head in [
            "FREED BY TTL",
            "SLOWEST IN THE SLOW LOG",
            "WHERE THE MEMORY IS",
        ] {
            assert!(!t.contains(head), "{head}:\n{t}");
        }
        // Taller, every pattern.
        let tall = text_of(&app, 160, 45);
        assert!(tall.contains("lock:{var}"), "{tall}");
        assert!(!tall.contains("other patterns"), "{tall}");
        // The slowest entries by duration, command and time; never a key or argument.
        assert!(t.contains("─ Slowest in the slow log ─"), "{t}");
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
