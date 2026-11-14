//! Draws [`ServerState`]: the Overview, Clients, Slow log and Monitor panels. Owner: wave
//! B, with `views/server.rs`.
//!
//! Every line is cut with `…` to the width of its panel, so nothing wraps at 80 columns.
//! Text from the server (names, keys, addresses) was cleaned of control characters when
//! it was parsed. Commands (Slow log arguments, Monitor lines) are drawn as
//! `live::command::masked_line` writes them, credentials masked, and through
//! `value::escape`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format;
use crate::live::monitor::{Entry, MAX_LINES, MAX_TIME};
use crate::live::server::{InfoSample, Role, SlowEntry};
use crate::snapshot::format_utc;
use crate::tui::app::Screen;
use crate::tui::app::views::server::{
    CLIENTS_EVERY, Clients, Group, HISTORY, IDLE_FLAG_SECS, Metric, Monitor, Overview, Poll,
    Prompt, SLOWLOG_EVERY, ServerState, ShardRow, SlowLog, lib_label, note_of, sparkline,
};
use crate::tui::app::views::{Ctx, LiveState};
use crate::value::escape;

use super::widgets::{fit, pad_l, pad_r, panel, rows, scroll_offset, trunc};
use super::{notice_panels, style, utc_time};

const LABEL_W: usize = 11;
const VALUE_W: usize = 19;

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    match ctx.screen {
        Screen::Overview => overview(frame, area, state, ctx),
        Screen::Clients => clients(frame, area, state, ctx),
        Screen::SlowLog => slow_log(frame, area, state, ctx),
        Screen::Monitor => monitor(frame, area, &state.monitor, ctx),
        _ => super::not_built(frame, area, ctx.screen.name(), ctx),
    }
    if ctx.screen == Screen::SlowLog
        && let (Some(form), Some(sends)) = (&state.form, state.form_sends())
    {
        super::streams::draw_form(frame, area, form, &sends, ctx);
    }
}

/// Draws a notice instead of the panel when it is off, or has nothing yet. True when it
/// did, and the caller returns.
fn gate(frame: &mut Frame, area: Rect, title: &str, poll: &Poll, ctx: &Ctx<'_>) -> bool {
    if let Some(off) = &poll.off {
        let line = Line::from(Span::styled(format!(" ▲ {off}"), style::amber()));
        let right = with_node(ctx.node_title(), "off".to_string());
        notice_panels(frame, area, title, &right, vec![line]);
        return true;
    }
    if poll.got {
        return false;
    }
    let line = match ctx.live() {
        LiveState::Closed(why) => Line::from(Span::styled(
            format!(
                " ▲ {}",
                why.as_deref().unwrap_or("The live connection is closed.")
            ),
            style::amber(),
        )),
        LiveState::Waiting => Line::from(Span::styled(
            " Waiting for the live connection…",
            style::dim(),
        )),
        LiveState::Ready => match &poll.failed {
            Some(f) => Line::from(Span::styled(format!(" ▲ {f}"), style::amber())),
            None => Line::from(Span::styled(" Waiting for the server…", style::dim())),
        },
    };
    let right = with_node(ctx.node_title(), String::new());
    notice_panels(frame, area, title, &right, vec![line]);
    true
}

/// A line under the panel title when something is wrong: the last poll failed, or the
/// live connection closed (what is shown is then the last data).
pub(super) fn trouble(poll: &Poll, ctx: &Ctx<'_>) -> Option<Line<'static>> {
    let text = if let LiveState::Closed(why) = ctx.live() {
        why.clone()
            .unwrap_or_else(|| "The live connection is closed.".to_string())
    } else {
        poll.failed.clone()?
    };
    Some(Line::from(Span::styled(
        format!(" ▲ {text} Showing the last data."),
        style::amber(),
    )))
}

/// The right of a polling panel's title: on a cluster the picked node first (spec
/// "Cluster live views (P4)"), then `extra` and how often it polls.
pub(super) fn right_title(state: &ServerState, ctx: &Ctx<'_>, every: &str, extra: &str) -> String {
    let when = if state.paused {
        "paused · r fetches".to_string()
    } else {
        format!("every {every}")
    };
    let text = if extra.is_empty() {
        when
    } else {
        format!("{extra} · {when}")
    };
    with_node(ctx.node_title(), text)
}

/// `node · text` when there is a node.
fn with_node(node: Option<String>, text: String) -> String {
    match node {
        Some(n) if text.is_empty() => n,
        Some(n) => format!("{n} · {text}"),
        None => text,
    }
}

pub(super) fn secs(d: std::time::Duration) -> String {
    format!("{} s", d.as_secs())
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

fn overview(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let o = &state.overview;
    if gate(frame, area, "Overview", &o.poll, ctx) {
        return;
    }
    // A cluster's per-shard summary sits between the figures and the server line.
    let shard_rows = if ctx.session.is_cluster() {
        u16::try_from(ctx.session.nodes.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
    } else {
        0
    };
    let [main, shards, server] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(shard_rows),
        Constraint::Length(4),
    ])
    .areas(area);
    if shard_rows > 0 {
        shard_summary(frame, shards, state, ctx);
    }
    let right = right_title(state, ctx, &secs(state.interval()), "");
    let right = if state.paused {
        right
    } else {
        format!("{right} · + - interval")
    };
    let inner = panel(frame, main, "Overview", &right);
    let w = usize::from(inner.width);
    let spark_w = w.saturating_sub(1 + LABEL_W + VALUE_W).min(HISTORY);
    let mut lines = vec![trouble(&o.poll, ctx).unwrap_or_default()];
    lines.extend(
        Metric::ALL
            .into_iter()
            .map(|m| metric_line(m, o, spark_w, w)),
    );
    rows(frame, inner, lines, None);

    let inner = panel(frame, server, "Server", "");
    rows(
        frame,
        inner,
        server_lines(o.last.as_ref(), usize::from(inner.width)),
        None,
    );
}

const SHARD_ADDR_W: usize = 22;
const SHARD_OPS_W: usize = 10;
const SHARD_MEM_W: usize = 22;
const SHARD_CLIENTS_W: usize = 12;

/// One row per shard's node, in slot order: address, ops/s, used memory against
/// `maxmemory`, clients and `db0` keys, or ✗ for a node that did not answer.
fn shard_summary(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let every = secs(
        state
            .interval()
            .max(crate::tui::app::views::server::SHARDS_EVERY),
    );
    let right = if state.paused {
        "paused".to_string()
    } else {
        format!("ops/s · memory · clients · keys · every {every}")
    };
    let inner = panel(frame, area, "Shards", &right);
    let w = usize::from(inner.width);
    let lines = ctx
        .session
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let row = state.shards.rows.get(i);
            let picked = i == ctx.session.picked;
            let mark = if picked { "›" } else { " " };
            let mut spans = vec![
                Span::styled(mark, style::ok()),
                Span::styled(
                    pad_r(&n.endpoint.to_string(), SHARD_ADDR_W),
                    if n.role == crate::model::NodeRole::Replica {
                        style::dim()
                    } else {
                        style::fg()
                    },
                ),
            ];
            match row {
                _ if !n.connected => spans.push(Span::styled("✗ not connected", style::warn())),
                Some(r) if r.failed => spans.push(Span::styled("✗ no answer", style::warn())),
                Some(ShardRow { last: Some(s), .. }) => spans.extend(shard_cells(s)),
                _ => spans.push(Span::styled("…", style::dim())),
            }
            Line::from(fit(spans, w))
        })
        .collect();
    rows(frame, inner, lines, None);
}

pub(super) fn shard_cells(s: &InfoSample) -> Vec<Span<'static>> {
    let ops = s
        .ops_now
        .map_or_else(|| "—".to_string(), |o| format!("{}/s", format::count(o)));
    let mem = match (s.used_memory, s.maxmemory.filter(|m| *m > 0)) {
        (Some(u), Some(m)) => format!(
            "{} / {} {}",
            format::bytes(u),
            format::bytes(m),
            format::percent(u as f64 / m as f64)
        ),
        (Some(u), None) => format!("{} (no limit)", format::bytes(u)),
        (None, _) => "—".to_string(),
    };
    let clients = s.clients.map_or_else(
        || "—".to_string(),
        |c| format!("{} clients", format::count(c)),
    );
    let keys = s
        .keys
        .map_or_else(|| "—".to_string(), |k| format!("{} keys", format::count(k)));
    vec![
        Span::styled(pad_l(&ops, SHARD_OPS_W - 1), style::fgb()),
        Span::raw("  "),
        Span::styled(pad_r(&mem, SHARD_MEM_W), style::fg()),
        Span::styled(pad_r(&clients, SHARD_CLIENTS_W), style::fg()),
        Span::styled(keys, style::fg()),
    ]
}

fn metric_line(m: Metric, o: &Overview, spark_w: usize, w: usize) -> Line<'static> {
    let (text, st) = metric_text(m, o);
    let mut spans = vec![
        Span::styled(format!(" {}", pad_r(m.label(), LABEL_W - 1)), style::dim()),
        Span::styled(pad_r(&text, VALUE_W), st),
        Span::styled(sparkline(&o.history, m, spark_w), style::ok()),
    ];
    spans = fit(spans, w);
    Line::from(spans)
}

/// `5.0` per second as `5.0`, `120` as `120`, `0` as `0`.
fn per_sec(v: f64) -> String {
    if v > 0.0 && v < 10.0 {
        format!("{v:.1}")
    } else {
        format::count(v.round() as u64)
    }
}

/// The current value of `m` as text, and its style.
pub(super) fn metric_text(m: Metric, o: &Overview) -> (String, Style) {
    let dash = || ("—".to_string(), style::dim());
    let (Some(p), Some(last)) = (o.now(), o.last.as_ref()) else {
        return dash();
    };
    let Some(v) = m.value(p) else {
        return dash();
    };
    let plain = |s: String| (s, style::fgb());
    match m {
        Metric::Ops => plain(format!("{}/s", format::count(v.round() as u64))),
        Metric::Memory => {
            let used = format::bytes(v as u64);
            match last.maxmemory.filter(|max| *max > 0) {
                Some(max) => {
                    let ratio = v / max as f64;
                    let st = if ratio >= 0.9 {
                        style::warn()
                    } else {
                        style::fgb()
                    };
                    (
                        format!("{used} / {} {}", format::bytes(max), format::percent(ratio)),
                        st,
                    )
                }
                None => plain(format!("{used} (no limit)")),
            }
        }
        Metric::HitRate => plain(format::percent(v)),
        Metric::Clients => plain(format::count(v as u64)),
        Metric::Blocked => {
            let st = if v > 0.0 {
                style::amber()
            } else {
                style::fgb()
            };
            (format::count(v as u64), st)
        }
        Metric::NetIn | Metric::NetOut => plain(format!("{}/s", format::bytes(v as u64))),
        Metric::Evicted => {
            let st = if v > 0.0 {
                style::amber()
            } else {
                style::fgb()
            };
            (format!("{}/s", per_sec(v)), st)
        }
        Metric::Expired => plain(format!("{}/s", per_sec(v))),
        Metric::CpuUser | Metric::CpuSys => plain(format!("{v:.1}%")),
        Metric::Fragmentation => {
            let st = if v > 1.5 {
                style::amber()
            } else {
                style::fgb()
            };
            (format!("{v:.2}"), st)
        }
    }
}

pub(super) fn server_lines(last: Option<&InfoSample>, w: usize) -> Vec<Line<'static>> {
    let Some(s) = last else {
        return Vec::new();
    };
    let sep = || Span::styled(" · ", style::dim());
    let mut first = vec![Span::raw(" ")];
    let part = |spans: &mut Vec<Span<'static>>, text: String, st: Style| {
        if spans.len() > 1 {
            spans.push(sep());
        }
        spans.push(Span::styled(text, st));
    };
    match s.uptime_secs {
        Some(u) => part(
            &mut first,
            format!("up {}", format::duration(std::time::Duration::from_secs(u))),
            style::fg(),
        ),
        None => part(&mut first, "up —".to_string(), style::dim()),
    }
    match s.role {
        Some(Role::Primary) => part(&mut first, "primary".to_string(), style::fg()),
        Some(Role::Replica) => {
            let (t, st) = match s.link_up {
                Some(false) => ("replica, link down", style::warn()),
                _ => ("replica", style::fg()),
            };
            part(&mut first, t.to_string(), st);
        }
        None => part(&mut first, "role —".to_string(), style::dim()),
    }
    if let Some(n) = s.replicas {
        let t = if n == 1 {
            "1 replica".to_string()
        } else {
            format!("{n} replicas")
        };
        part(&mut first, t, style::fg());
    }
    let rdb = match s.rdb_ok {
        Some(true) => ("RDB ok", style::fg()),
        Some(false) => ("RDB last save failed", style::warn()),
        None => ("RDB —", style::dim()),
    };
    part(&mut first, rdb.0.to_string(), rdb.1);
    let aof = match (s.aof_on, s.aof_ok) {
        (Some(true), Some(false)) => ("AOF write failed", style::warn()),
        (Some(true), _) => ("AOF on", style::fg()),
        (Some(false), _) => ("AOF off", style::fg()),
        (None, _) => ("AOF —", style::dim()),
    };
    part(&mut first, aof.0.to_string(), aof.1);
    let policy = s.policy.clone().unwrap_or_else(|| "—".to_string());
    let second = vec![
        Span::styled(" maxmemory-policy ", style::dim()),
        Span::styled(policy, style::fg()),
    ];
    vec![Line::from(fit(first, w)), Line::from(fit(second, w))]
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

const BY_W: usize = 5;
const CONNS_W: usize = 6;
const AGE_W: usize = 8;
const CMD_W: usize = 14;
const MEM_W: usize = 7;
const FLAG_W: usize = 13;

/// A duration in seconds as `45s`, `3m 05s`, `2h 10m`.
fn age(secs: u64) -> String {
    format::duration(std::time::Duration::from_secs(secs))
}

fn cmd_text(cmd: &str) -> &str {
    if cmd == "NULL" || cmd.is_empty() {
        "—"
    } else {
        cmd
    }
}

fn clients(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let c = &state.clients;
    if gate(frame, area, "Clients", &c.poll, ctx) {
        return;
    }
    if c.drill.is_some() {
        connections(frame, area, state, ctx);
    } else {
        groups(frame, area, state, ctx);
    }
}

fn groups(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let c = &state.clients;
    let right = right_title(
        state,
        ctx,
        &secs(CLIENTS_EVERY),
        &format!("{} connections · {} groups", c.conns.len(), c.groups.len()),
    );
    let inner = panel(frame, area, "Clients", &right);
    let w = usize::from(inner.width);
    let gw = w
        .saturating_sub(1 + BY_W + CONNS_W + 2 * AGE_W + CMD_W + MEM_W + FLAG_W)
        .max(8);
    let head = Line::from(fit(
        vec![Span::styled(
            format!(
                " {}{}{}{}{}{}{}",
                pad_r("BY", BY_W),
                pad_r("GROUP", gw),
                pad_l("CONNS", CONNS_W),
                pad_l("OLDEST", AGE_W),
                pad_l("IDLE", AGE_W),
                pad_r("  LAST CMD", CMD_W),
                pad_l("MEM", MEM_W),
            ),
            style::hd(),
        )],
        w,
    ));
    let mut top = vec![trouble(&c.poll, ctx).unwrap_or_default(), head];
    let body_h = usize::from(inner.height).saturating_sub(top.len());
    let off = scroll_offset(c.sel, body_h);
    top.extend(
        c.groups
            .iter()
            .skip(off)
            .take(body_h)
            .map(|g| group_line(g, gw, w)),
    );
    if c.groups.is_empty() {
        top.push(Line::from(Span::styled(" No connections.", style::dim())));
    }
    rows(frame, inner, top, Some(c.sel - off + 2));
}

fn group_line(g: &Group, gw: usize, w: usize) -> Line<'static> {
    let flag = if g.flagged() {
        Span::styled(
            format!(" ▲ {} idle {}", g.idle_conns, idle_unit()),
            style::amber(),
        )
    } else {
        Span::raw("")
    };
    let spans = vec![
        Span::styled(format!(" {}", pad_r(g.key.kind(), BY_W)), style::dim()),
        Span::styled(pad_r(g.key.label(), gw), style::fg()),
        Span::styled(
            pad_l(&format::count(g.connections() as u64), CONNS_W),
            style::fgb(),
        ),
        Span::styled(pad_l(&age(g.oldest_age), AGE_W), style::fg()),
        Span::styled(
            pad_l(&age(g.longest_idle), AGE_W),
            if g.flagged() {
                style::amber()
            } else {
                style::fg()
            },
        ),
        Span::styled(
            format!("  {}", pad_r(cmd_text(&g.top_cmd), CMD_W - 2)),
            style::fg(),
        ),
        Span::styled(pad_l(&format::bytes(g.mem), MEM_W), style::fg()),
        flag,
    ];
    Line::from(fit(spans, w))
}

/// The unit in the flag: `1h+` for the hour the flag is set at.
fn idle_unit() -> String {
    format!("{}h+", IDLE_FLAG_SECS / 3600)
}

fn connections(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let c: &Clients = &state.clients;
    let conns = c.open_conns();
    let key = c.drill.as_ref();
    let name = key.map_or("", |k| k.label());
    let kind = key.map_or("", |k| k.kind());
    let right = right_title(
        state,
        ctx,
        &secs(CLIENTS_EVERY),
        &format!("{kind} {name} · {} connections · esc back", conns.len()),
    );
    let inner = panel(frame, area, "Clients", &right);
    let w = usize::from(inner.width);
    const ID_W: usize = 7;
    const NAME_W: usize = 16;
    const CMD: usize = 12;
    let aw = w
        .saturating_sub(1 + ID_W + NAME_W + 2 * AGE_W + CMD + MEM_W)
        .max(10);
    let head = Line::from(fit(
        vec![Span::styled(
            format!(
                " {}{}{}{}{}{}{}",
                pad_l("ID", ID_W - 1),
                pad_r("  ADDRESS", aw),
                pad_r("LIBRARY / NAME", NAME_W),
                pad_l("AGE", AGE_W),
                pad_l("IDLE", AGE_W),
                pad_r("  LAST CMD", CMD),
                pad_l("MEM", MEM_W),
            ),
            style::hd(),
        )],
        w,
    ));
    let mut lines = vec![trouble(&c.poll, ctx).unwrap_or_default(), head];
    let body_h = usize::from(inner.height).saturating_sub(lines.len());
    let off = scroll_offset(c.conn_sel, body_h);
    lines.extend(conns.iter().skip(off).take(body_h).map(|cn| {
        let idle_long = cn.idle >= IDLE_FLAG_SECS;
        Line::from(fit(
            vec![
                Span::styled(
                    format!(" {}", pad_l(&cn.id.to_string(), ID_W - 1)),
                    style::dim(),
                ),
                Span::styled(format!("  {}", pad_r(&cn.addr, aw - 2)), style::fg()),
                Span::styled(pad_r(&lib_label(cn), NAME_W), style::fg()),
                Span::styled(pad_l(&age(cn.age), AGE_W), style::fg()),
                Span::styled(
                    pad_l(&age(cn.idle), AGE_W),
                    if idle_long {
                        style::amber()
                    } else {
                        style::fg()
                    },
                ),
                Span::styled(
                    format!("  {}", pad_r(cmd_text(&cn.cmd), CMD - 2)),
                    style::fg(),
                ),
                Span::styled(pad_l(&format::bytes(cn.mem), MEM_W), style::fg()),
            ],
            w,
        ))
    }));
    if conns.is_empty() {
        lines.push(Line::from(Span::styled(
            " No connections in this group any more. esc goes back.",
            style::dim(),
        )));
    }
    rows(
        frame,
        inner,
        lines,
        Some(c.conn_sel.saturating_sub(off) + 2),
    );
}

// ---------------------------------------------------------------------------
// Slow log
// ---------------------------------------------------------------------------

const TIME_W: usize = 9;
const DUR_W: usize = 9;
const COMMAND_W: usize = 15;
const CLIENT_W: usize = 21;

/// Microseconds as `812µs`, `25.3ms`, `1.25s`.
fn micros(m: u64) -> String {
    if m < 1_000 {
        format!("{m}µs")
    } else if m < 1_000_000 {
        format!("{:.1}ms", m as f64 / 1_000.0)
    } else {
        format!("{:.2}s", m as f64 / 1_000_000.0)
    }
}

fn threshold_text(micros_: i64) -> String {
    if micros_ < 0 {
        "log is off".to_string()
    } else if micros_ == 0 {
        "logs every command".to_string()
    } else {
        format!("threshold {}", micros(micros_.unsigned_abs()))
    }
}

fn keys_text(e: &SlowEntry) -> String {
    let mut s = e.keys.join(" ");
    if e.more_keys > 0 {
        s.push_str(&format!(" +{}", e.more_keys));
    }
    s
}

fn slow_log(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let s: &SlowLog = &state.slow;
    if gate(frame, area, "Slow log", &s.poll, ctx) {
        return;
    }
    let [main, detail] = Layout::vertical([Constraint::Min(0), Constraint::Length(7)]).areas(area);
    let right = right_title(
        state,
        ctx,
        &secs(SLOWLOG_EVERY),
        &format!("{} entries · UTC", s.entries.len()),
    );
    let inner = panel(frame, main, "Slow log", &right);
    let w = usize::from(inner.width);
    let kw = w
        .saturating_sub(2 + TIME_W + DUR_W + COMMAND_W + CLIENT_W)
        .max(8);

    let mut status: Vec<Span<'static>> = vec![Span::raw(" ")];
    match (&s.config, &s.config_note) {
        (Some(c), _) => status.push(Span::styled(
            format!(
                "{} · keeps {} entries",
                threshold_text(c.threshold_micros),
                c.max_len
            ),
            style::dim(),
        )),
        (None, Some(note)) => status.push(Span::styled(note.clone(), style::amber())),
        (None, None) => {}
    }
    let status = trouble(&s.poll, ctx).unwrap_or_else(|| Line::from(fit(status, w)));

    let head = Line::from(fit(
        vec![Span::styled(
            format!(
                "  {}{}{}{}{}",
                pad_r("TIME", TIME_W),
                pad_l("DURATION", DUR_W),
                pad_r("  COMMAND", COMMAND_W),
                pad_r("KEYS", kw),
                pad_r("CLIENT", CLIENT_W),
            ),
            style::hd(),
        )],
        w,
    ));
    let mut lines = vec![status, head];
    let body_h = usize::from(inner.height).saturating_sub(lines.len());
    let off = scroll_offset(s.sel, body_h);
    lines.extend(s.entries.iter().skip(off).take(body_h).map(|e| {
        let note = note_of(e).is_some();
        Line::from(fit(
            vec![
                Span::styled(if note { " ▲" } else { "  " }, style::amber()),
                Span::styled(pad_r(&utc_time(e.unix), TIME_W), style::dim()),
                Span::styled(pad_l(&micros(e.micros), DUR_W), style::fgb()),
                Span::styled(
                    format!("  {}", pad_r(&e.command, COMMAND_W - 2)),
                    style::cmd(),
                ),
                Span::styled(pad_r(&keys_text(e), kw - 1), style::fg()),
                Span::raw(" "),
                Span::styled(pad_r(&e.client, CLIENT_W), style::dim()),
            ],
            w,
        ))
    }));
    if s.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            " The slow log is empty.",
            style::dim(),
        )));
    }
    rows(frame, inner, lines, Some(s.sel.saturating_sub(off) + 2));
    entry_panel(frame, detail, s);
}

fn entry_panel(frame: &mut Frame, area: Rect, s: &SlowLog) {
    let inner = panel(frame, area, "Entry", "");
    let w = usize::from(inner.width);
    let Some(e) = s.selected() else {
        return;
    };
    let label = |t: &str| Span::styled(format!(" {}", pad_r(t, 7)), style::dim());
    let keys = if !e.keys.is_empty() {
        keys_text(e)
    } else if let Some(note) = &s.keys_note {
        note.clone()
    } else {
        "none".to_string()
    };
    let client = if e.client_name.is_empty() {
        e.client.clone()
    } else {
        format!("{} ({})", e.client, e.client_name)
    };
    let mut lines = vec![
        Line::from(fit(
            vec![
                label("when"),
                Span::styled(format_utc(e.unix), style::fg()),
                Span::styled(" · ", style::dim()),
                Span::styled(micros(e.micros), style::fgb()),
                Span::styled(" · ", style::dim()),
                Span::styled(e.command.clone(), style::cmd()),
            ],
            w,
        )),
        Line::from(fit(args_spans(label("args"), e), w)),
        Line::from(fit(vec![label("keys"), Span::styled(keys, style::fg())], w)),
        Line::from(fit(
            vec![label("client"), Span::styled(client, style::fg())],
            w,
        )),
    ];
    if let Some(n) = note_of(e) {
        lines.push(Line::from(fit(
            vec![Span::styled(format!(" ▲ {}", n.text()), style::amber())],
            w,
        )));
    }
    rows(frame, inner, lines, None);
}

/// The `args` line of a slow log entry: the command line as the masker wrote it, and how
/// many arguments the server left out.
fn args_spans(label: Span<'static>, e: &SlowEntry) -> Vec<Span<'static>> {
    let line = escape(&String::from_utf8_lossy(e.args.as_bytes()));
    let mut spans = vec![label, Span::styled(line, style::fg())];
    if e.args_cut > 0 {
        spans.push(Span::styled(
            format!(" … {} more arguments", format::grouped(e.args_cut)),
            style::dim(),
        ));
    }
    spans
}

// ---------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------

const M_TIME_W: usize = 13;
const M_DB_W: usize = 4;
const M_CLIENT_W: usize = 22;

/// `13:04:05.123` (UTC) for a line, `—` when its time is unreadable.
fn line_time(e: &Entry) -> String {
    e.unix().map_or_else(
        || "—".to_string(),
        |u| format!("{}.{:03}", utc_time(u), e.millis()),
    )
}

/// The Monitor's title: on a cluster, the node its run went to (it stays there), or
/// the picked node before any run.
fn monitor_right(m: &Monitor, ctx: &Ctx<'_>) -> String {
    let node = match &m.node {
        Some(ep) if m.running() || m.stopped.is_some() => Some(format!("node {ep}")),
        _ => ctx.node_title(),
    };
    with_node(node, monitor_state(m, ctx))
}

fn monitor_state(m: &Monitor, ctx: &Ctx<'_>) -> String {
    let lines = format::grouped(m.lines.len() as u64);
    if m.prompt.is_some() {
        "asking first".to_string()
    } else if m.running() && !m.ready {
        "starting…".to_string()
    } else if m.running() {
        let left = m.left(ctx.now).unwrap_or(MAX_TIME).as_secs();
        format!("running · {lines} lines · {left} s left · UTC")
    } else if m.stopped.is_some() {
        format!("stopped · {lines} lines kept · UTC")
    } else {
        "not started".to_string()
    }
}

fn monitor(frame: &mut Frame, area: Rect, m: &Monitor, ctx: &Ctx<'_>) {
    let [main, detail] = Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(area);
    let inner = panel(frame, main, "Monitor", &monitor_right(m, ctx));
    let w = usize::from(inner.width);
    let text = |t: String, st: Style| Line::from(fit(vec![Span::styled(t, st)], w));
    if let Some(p) = &m.prompt {
        let profile = &ctx.profile().name;
        let mut lines = vec![
            Line::default(),
            text(format!(" Send MONITOR to {profile}?"), style::fgb()),
            Line::default(),
            text(
                " MONITOR runs on a connection of its own, and the server copies every".to_string(),
                style::fg(),
            ),
            text(
                " command it runs to it. That slows a busy server.".to_string(),
                style::fg(),
            ),
            text(
                format!(
                    " It stops after {} s or {} lines, or with esc. The lines stay in memory.",
                    MAX_TIME.as_secs(),
                    format::grouped(MAX_LINES as u64)
                ),
                style::dim(),
            ),
            Line::default(),
        ];
        lines.push(match p {
            Prompt::YesNo => text(" y starts it, n cancels.".to_string(), style::amber()),
            Prompt::TypeName(_) => text(
                format!(" A production profile: type {profile}, then enter. esc cancels."),
                style::amber(),
            ),
        });
        rows(frame, inner, lines, None);
        panel(frame, detail, "Line", "");
        return;
    }
    let status = if let Some(why) = crate::conn::monitor_unsupported(ctx.profile()) {
        text(format!(" ▲ {why}"), style::amber())
    } else if let Some(stop) = &m.stopped {
        let st = if matches!(
            stop,
            crate::tui::app::views::server::Stop::Failed(_)
                | crate::tui::app::views::server::Stop::Lost
        ) {
            style::amber()
        } else {
            style::dim()
        };
        let mut t = format!(" {}", stop.text());
        if m.ranking().is_some() && ctx.report().is_some_and(|r| r.hot_keys.is_empty()) {
            t.push_str(" Hot keys (2) ranks its keys.");
        }
        text(t, st)
    } else if m.running() {
        text(" esc stops it.".to_string(), style::dim())
    } else {
        text(
            " enter starts MONITOR, after asking. It shows every command the server runs."
                .to_string(),
            style::dim(),
        )
    };
    let cw = w.saturating_sub(1 + M_TIME_W + M_DB_W + M_CLIENT_W).max(8);
    let head = Line::from(fit(
        vec![Span::styled(
            format!(
                " {}{}{}{}",
                pad_r("TIME", M_TIME_W),
                pad_r("DB", M_DB_W),
                pad_r("CLIENT", M_CLIENT_W),
                pad_r("COMMAND", cw),
            ),
            style::hd(),
        )],
        w,
    ));
    let mut lines = vec![status, head];
    let body_h = usize::from(inner.height).saturating_sub(lines.len());
    let off = scroll_offset(m.sel, body_h);
    lines.extend(m.lines.iter().skip(off).take(body_h).map(|e| {
        Line::from(fit(
            vec![
                Span::styled(format!(" {}", pad_r(&line_time(e), M_TIME_W)), style::dim()),
                Span::styled(pad_r(&escape(&e.line.db), M_DB_W), style::dim()),
                Span::styled(
                    pad_r(&trunc(&escape(&e.line.client), M_CLIENT_W - 1), M_CLIENT_W),
                    style::dim(),
                ),
                Span::styled(escape(&e.command_line()), style::fg()),
            ],
            w,
        ))
    }));
    let selected = (!m.lines.is_empty()).then(|| m.sel.saturating_sub(off) + 2);
    if m.lines.is_empty() && (m.running() || m.stopped.is_some()) {
        let t = if m.running() {
            " No lines yet."
        } else {
            " No lines."
        };
        lines.push(text(t.to_string(), style::dim()));
    }
    rows(frame, inner, lines, selected);
    line_panel(frame, detail, m.selected());
}

/// The selected line in full, as far as two rows take it.
fn line_panel(frame: &mut Frame, area: Rect, e: Option<&Entry>) {
    let inner = panel(frame, area, "Line", "");
    let w = usize::from(inner.width);
    let Some(e) = e else {
        return;
    };
    let when = e.unix().map_or_else(|| "—".to_string(), format_utc);
    let first = Line::from(fit(
        vec![
            Span::styled(" ", style::dim()),
            Span::styled(when, style::fg()),
            Span::styled(" · db ", style::dim()),
            Span::styled(escape(&e.line.db), style::fg()),
            Span::styled(" · client ", style::dim()),
            Span::styled(escape(&e.line.client), style::fg()),
        ],
        w,
    ));
    let mut cmd = escape(&e.command_line());
    if e.cut > 0 {
        cmd.push_str(&format!(
            " … {} more arguments not kept",
            format::grouped(e.cut)
        ));
    }
    let room = w.saturating_sub(1).max(1);
    let chars: Vec<char> = cmd.chars().collect();
    let mut lines = vec![first];
    let mut rest: &[char] = &chars;
    for row in 0..2 {
        if rest.is_empty() {
            break;
        }
        let mut take = 0;
        let mut used = 0;
        while take < rest.len() {
            let cw = super::widgets::width(&rest[take].to_string());
            if used + cw > room {
                break;
            }
            used += cw;
            take += 1;
        }
        take = take.max(1);
        let mut part: String = rest[..take].iter().collect();
        rest = &rest[take..];
        if row == 1 && !rest.is_empty() {
            part = trunc(&format!("{part}…"), room);
        }
        lines.push(Line::from(Span::styled(format!(" {part}"), style::fg())));
    }
    rows(frame, inner, lines, None);
}
