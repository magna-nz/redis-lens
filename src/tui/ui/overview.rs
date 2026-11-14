//! 1 Overview: "Now" (the live figures of the Server view's Overview poll, and on a
//! cluster one row per shard) above "Keys" (what the server says about its keys, then
//! what an analysis found), and the "Analyze the keys?" dialog.
//!
//! [`draw`] is the one entry point `ui::draw` calls for the Overview screen,
//! [`draw_analyze`] the dialog's, and [`analyze_prompt`] the lines a scan view shows
//! before an analysis.
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
    AnalyzeDialog, Depth, Estimate, Finding, about_time, at_utc, estimate, estimate_for, findings,
    off_reasons, shows_analysis, stopped_early, time_left,
};
use crate::tui::app::views::server::{HISTORY, Metric, SHARDS_EVERY, sparkline};
use crate::tui::app::views::{Ctx, LiveState};
use crate::tui::app::{App, Phase, Session};

use super::widgets::{
    Section, Title, bar, dialog, fit, pad_l, pad_r, rows, scroll_offset, sections, trunc,
};
use super::{CONNECT_FIRST, hh_mm, server, server_label, spinner, style};

/// The six figures "Now" shows; `v` shows all of [`Metric::ALL`].
const SHORT: [Metric; 6] = [
    Metric::Ops,
    Metric::Clients,
    Metric::Memory,
    Metric::HitRate,
    Metric::Evicted,
    Metric::Expired,
];

const LABEL_W: usize = 10;
const VALUE_W: usize = 7;
/// Patterns named under "Where the memory is"; the rest share one row.
const TOP_PATTERNS: usize = 3;
/// From this width on, "Keys" also shows what TTLs free.
const WIDE: usize = 100;

/// Draws Overview into `area`, the body between the tab row and the footer.
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
    // The frame's top border, the divider and the bottom border.
    let keys_h = usize::from(area.height.saturating_sub(3).saturating_sub(now_rows));
    let (keys, keys_right) = keys_lines(app, ctx.session, w, keys_h);
    let inner = sections(
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
        "INFO paused · p resumes".to_string()
    } else if cluster {
        format!("[ ] picks a node · INFO every {every}")
    } else {
        format!("INFO every {every} · p pauses")
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
            style::amber(),
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
        let list: &[Metric] = if ctx.more { &Metric::ALL } else { &SHORT };
        let col = w / 2;
        for pair in list.chunks(2) {
            let mut spans = Vec::new();
            for m in pair {
                let (text, st, of) = figure(*m, o);
                let of = of.map(|of| format!(" of {of}")).unwrap_or_default();
                let spark_w = col
                    .saturating_sub(1 + LABEL_W + VALUE_W + 3 + super::widgets::width(&of))
                    .min(HISTORY);
                let cell = vec![
                    Span::styled(format!(" {}", pad_r(label(*m), LABEL_W)), style::dim()),
                    Span::styled(pad_l(&text, VALUE_W), st),
                    Span::raw(" "),
                    Span::styled(sparkline(&o.history, *m, spark_w), style::ok()),
                    Span::styled(of, style::dim()),
                ];
                // Cut or padded to its column.
                spans.extend(fit(cell, col));
            }
            lines.push(Line::from(fit(spans, w)));
        }
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
            Some(max) if used as f64 / max as f64 >= 0.9 => style::warn(),
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
            Some(false) => parts.push(("replica, link down".to_string(), style::warn())),
            _ => parts.push(("replica".to_string(), style::fg())),
        },
        None => {}
    }
    match i.rdb_ok {
        Some(false) => parts.push(("RDB last save failed".to_string(), style::warn())),
        Some(true) => parts.push(("RDB ok".to_string(), style::fg())),
        None => {}
    }
    match (i.aof_on, i.aof_ok) {
        (Some(true), Some(false)) => parts.push(("AOF write failed".to_string(), style::warn())),
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
            Span::styled(mark, style::amber()),
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
            _ if !n.connected => spans.push(Span::styled("✗ not connected", style::warn())),
            Some(r) if r.failed => spans.push(Span::styled("✗ no answer", style::warn())),
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
        let from = b.sel.map_or(0, |s| scroll_offset(s, n));
        out.extend(b.rows.into_iter().skip(from).take(n));
    }
    out
}

/// `7.92M keys · 31% have a TTL · 13G of data, 2.1G overhead`, leaving out the parts
/// the server did not give; or why the figures are missing.
fn headline(s: &Session, w: usize) -> Line<'static> {
    let figures = s.figures.as_ref();
    if let Some(why) = figures.and_then(|f| f.failed) {
        return Line::from(Span::styled(
            trunc(&format!(" ▲ {why}."), w),
            style::amber(),
        ));
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
            None => return Line::default(),
        },
    }
    let mut spans = vec![Span::styled(format!(" {}", parts[0]), style::fgb())];
    for p in &parts[1..] {
        spans.push(Span::styled(" · ", style::dim()));
        spans.push(Span::styled(p.clone(), style::fg()));
    }
    Line::from(fit(spans, w))
}

/// Where the WHERE column starts.
fn place_col(w: usize) -> usize {
    w.saturating_sub(if w >= WIDE { 24 } else { 15 })
}

/// A block heading with `WHERE` over the findings' column.
fn where_head(title: &str, note: &str, w: usize) -> Line<'static> {
    let at = place_col(w);
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
    let at = place_col(w);
    let sel = sel.min(found.len().saturating_sub(1));
    found
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (mark, st) = match f.severity {
                Severity::Warn => ("▲", style::warn()),
                Severity::Note => ("▲", style::amber()),
                Severity::Info => ("●", style::dim()),
            };
            let cursor = if i == sel { " › " } else { "   " };
            let text = vec![
                Span::styled(cursor, style::amber()),
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
    let p = &s.profile;
    let mut lines = Vec::new();
    let notice = provider_rows(p, w);
    match &s.phase {
        // A connect; an analysis connecting again shows ANALYZING below.
        Phase::Connecting if s.analysis.is_none() => {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spinner(app.tick), style::amber()),
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
                    style::amber(),
                )));
            }
            let lines = fit_blocks(lines, vec![Block::new(None, notice)], h);
            return (lines, String::new());
        }
        Phase::Failed(e) if app.report().is_none() => {
            lines.push(Line::from(Span::styled(
                trunc(&format!(" ✗ {e}"), w),
                style::warn(),
            )));
            return (lines, String::new());
        }
        _ => {}
    }
    let prefix = vec![headline(s, w), Line::default()];
    let cluster = s.is_cluster();
    let mut blocks = Vec::new();
    let right;
    if app.analysis_running() {
        let e = estimate_for(s.analysis.unwrap_or_default(), app);
        right = format!("about {} commands in all", format::count(e.commands));
        blocks.push(analyzing_block(s, &e, w));
        blocks.push(server_block(app, w, false));
    } else if shows_analysis(app) {
        let r = app.report().expect("shows_analysis has a report");
        let stopped = if stopped_early(s) {
            " · stopped early"
        } else {
            ""
        };
        let again = if w >= WIDE {
            " · r analyzes again"
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
        blocks.push(memory_block(r, w));
        if w >= WIDE {
            blocks.push(Block::new(None, vec![freed_line(r, w)]));
        }
    } else {
        right = if cluster {
            format!("summed over {} shards · no keys read", s.nodes.len())
        } else {
            "from INFO, MEMORY STATS, SLOWLOG · no keys read".to_string()
        };
        blocks.push(server_block(app, w, true));
        let e = estimate_for(Depth::Quick, app);
        let mut prompt = vec![Line::from(fit(
            vec![
                Span::styled("   r", style::amber()),
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
    (fit_blocks(prefix, blocks, h), right)
}

/// The profile's provider notice as a row, if it has one.
fn provider_rows(p: &Profile, w: usize) -> Vec<Line<'static>> {
    crate::conn::provider::notice(p.provider)
        .map(|n| Line::from(Span::styled(trunc(&format!(" ▲ {n}"), w), style::amber())))
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
fn analyzing_block(s: &Session, e: &Estimate, w: usize) -> Block {
    let pr = s.progress.unwrap_or_default();
    let target = if pr.target > 0 { pr.target } else { e.keys };
    let head = if s.phase == Phase::Connecting {
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
                Span::styled(fill, style::amber()),
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
    Block::new(
        Some(head),
        vec![
            Line::from(Span::styled(
                trunc(&format!(" {cmds} for each sampled key, {rate}."), w),
                style::dim(),
            )),
            Line::from(Span::styled(trunc(stop, w), style::dim())),
        ],
    )
}

/// "WHERE THE MEMORY IS": the biggest patterns with their share, the rest on one row;
/// shares of keys, and why, without sizes.
fn memory_block(r: &ScanReport, w: usize) -> Block {
    let value = |p: &crate::model::PatternStats| {
        if r.sizes_available {
            p.est_bytes.unwrap_or(0)
        } else {
            p.est_keys
        }
    };
    let total: u64 = r.patterns.iter().map(value).fold(0, u64::saturating_add);
    let (name_w, bar_w) = if w >= WIDE { (22, 26) } else { (18, 16) };
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
                Span::styled(fill, style::amber()),
                Span::styled(track, style::track()),
                Span::styled(pad_l(&format::percent(ratio), 5), style::fgb()),
                Span::styled(pad_l(&size, 9), style::fg()),
            ],
            w,
        ))
    };
    let mut rows: Vec<Line<'static>> = r
        .patterns
        .iter()
        .take(TOP_PATTERNS)
        .map(|p| row(p.pattern.clone(), value(p)))
        .collect();
    let rest = &r.patterns[r.patterns.len().min(TOP_PATTERNS)..];
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

/// The lines a screen that needs an analysis shows before one: what `r` would cost.
/// `title` names the screen (empty on Overview itself). The profile's estimate alone
/// ([`estimate`]): Overview has the session's own ([`estimate_for`]).
pub fn analyze_prompt(p: &Profile, title: &str) -> Vec<Line<'static>> {
    let e = estimate(Depth::Quick, p);
    let mut lines = Vec::new();
    if !title.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" No analysis yet: {title} needs a sample of the keys."),
            style::dim(),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled("   r", style::amber()),
        Span::styled(analyze_text(&e), style::fg()),
    ]));
    lines
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
                Span::styled(mark, style::amber()),
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
        Span::styled(" enter", style::amber()),
        Span::styled(" Start   ", style::fg()),
        Span::styled("↑↓", style::amber()),
        Span::styled(" Pick   ", style::fg()),
        Span::styled("esc", style::amber()),
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
    }

    #[test]
    fn overview_after_an_analysis() {
        let app = overview_app(true);
        insta::assert_snapshot!("after_80x24", snap(&app, 80, 24));
        insta::assert_snapshot!("after_120x36", snap(&app, 120, 36));
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
        app.handle_key(key(KeyCode::Char('r')));
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
            app.handle_key(key(KeyCode::Char('r')));
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
        app.handle_key(key(KeyCode::Char('r')));
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
        // Neither the slow command's arguments nor its key.
        assert!(!t.contains("secret-value"), "{t}");
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
        let t = text_of(&app, 120, 36);
        for want in [
            "Big keys from the server need INFO keysizes",
            "MEMORY STATS is denied for this user.",
            "Slow log is off:",
            "Idle clients are off: the server refused CLIENT LIST",
        ] {
            assert!(t.contains(want), "{want}:\n{t}");
        }
        no_heading_without_rows(&t);
        insta::assert_snapshot!("refused_120x36", t);
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
        assert!(t.contains("esc stops it and keeps what it has"), "{t}");
        no_heading_without_rows(&t);
        insta::assert_snapshot!("analyzing_120x36", t);
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
        assert!(t.contains("FREED BY TTL  within 1 h"), "{t}");
        assert!(t.contains("other patterns"), "{t}");
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
        app.handle_key(key(KeyCode::Char('r')));
        let t = text_of(&app, 80, 24);
        assert!(t.contains("across 3 shards"), "{t}");
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
        app.handle_key(key(KeyCode::Char('r')));
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
