//! Draws [`ServerState`]: the Clients, Slow log and Monitor panels, and the figures of
//! its Overview poll that `ui/overview.rs` shows. Owner: wave B, with `views/server.rs`.
//!
//! Every line is cut with `…` to the width of its panel, so nothing wraps at 80 columns.
//! Text from the server (names, keys, addresses) was cleaned of control characters when
//! it was parsed. Commands (Slow log arguments, Monitor lines) are drawn as
//! `live::command::masked_line` writes them, credentials masked, and through
//! `value::escape`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format;
use crate::live::monitor::{Entry, MAX_LINES, MAX_TIME};
use crate::live::server::{ClientConn, SlowEntry};
use crate::snapshot::format_utc;
use crate::tui::app::Screen;
use crate::tui::app::views::server::{
    CLIENTS_EVERY, Clients, Group, IDLE_FLAG_SECS, Metric, Monitor, Overview, Poll, Prompt,
    SLOWLOG_EVERY, ServerState, SlowLog, lib_label, note_of,
};
use crate::tui::app::views::{Ctx, LiveState};
use crate::value::escape;

use super::analysis::{Col, MARK, Tier, head_row_at, notice, paragraph, pick_flex};
use super::widgets::{
    Section, Title, after_gutter, fit, gutter_area, list_and_details, list_below, list_open, pad_l,
    pad_r, rows, scroll_offset, sections, side_split, trunc,
};
use super::{style, utc_time};

/// Draws the screen `ctx.screen` of this view into `area`, the body between the
/// header and the key bar.
pub fn draw(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    match ctx.screen {
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
        let line = Line::from(Span::styled(format!(" ▲ {off}"), style::warning()));
        let right = with_node(ctx.node_title(), "off".to_string());
        notice(frame, area, title, &right, vec![line]);
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
            style::warning(),
        )),
        LiveState::Waiting => Line::from(Span::styled(
            " Waiting for the live connection…",
            style::dim(),
        )),
        LiveState::Ready => match &poll.failed {
            Some(f) => Line::from(Span::styled(format!(" ▲ {f}"), style::warning())),
            None => Line::from(Span::styled(" Waiting for the server…", style::dim())),
        },
    };
    let right = with_node(ctx.node_title(), String::new());
    notice(frame, area, title, &right, vec![line]);
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
        style::warning(),
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
// Overview's figures (drawn by `ui/overview.rs`)
// ---------------------------------------------------------------------------

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
                        style::warning()
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
                style::warning()
            } else {
                style::fgb()
            };
            (format::count(v as u64), st)
        }
        Metric::NetIn | Metric::NetOut => plain(format!("{}/s", format::bytes(v as u64))),
        Metric::Evicted => {
            let st = if v > 0.0 {
                style::warning()
            } else {
                style::fgb()
            };
            (format!("{}/s", per_sec(v)), st)
        }
        Metric::Expired => plain(format!("{}/s", per_sec(v))),
        Metric::CpuUser | Metric::CpuSys => plain(format!("{v:.1}%")),
        Metric::Fragmentation => {
            let st = if v > 1.5 {
                style::warning()
            } else {
                style::fgb()
            };
            (format!("{v:.2}"), st)
        }
    }
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// Rows of the details under a list.
const DETAIL_ROWS: u16 = 5;
/// Lists this wide show their Wide columns.
const WIDE_AT: usize = 90;

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
    if c.drill.is_some() && side_split(area.width).is_some() {
        drilled_side(frame, area, state, ctx);
    } else if c.drill.is_some() {
        connections(frame, area, state, ctx);
    } else {
        groups(frame, area, state, ctx);
    }
}

const G_GROUP: usize = 0;
const G_MARK: usize = 1;
const G_CONNS: usize = 2;
const G_IDLE: usize = 3;
const G_OLDEST: usize = 4;
const G_CMD: usize = 5;
const G_MEM: usize = 6;
const G_BY: usize = 7;

fn group_columns() -> [Col; 8] {
    [
        Col::new("GROUP", 14, Tier::Base),
        Col::new("", 1, Tier::Base),
        Col::new("CONNS", 6, Tier::Base),
        Col::new("IDLE", 8, Tier::Base),
        Col::new("OLDEST", 8, Tier::Wide),
        Col::new("LAST COMMAND", 14, Tier::Wide),
        Col::new("MEM", 7, Tier::Wide),
        Col::new("BY", 7, Tier::More),
    ]
}

/// The lines above the table's rows: the trouble line when something is wrong, then the
/// head. The selected row is `lines.len()` rows further down.
fn with_head(trouble_line: Option<Line<'static>>, head: Line<'static>) -> Vec<Line<'static>> {
    let mut v: Vec<Line<'static>> = trouble_line.into_iter().collect();
    v.push(head);
    v
}

fn groups(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let c = &state.clients;
    let right = right_title(
        state,
        ctx,
        &secs(CLIENTS_EVERY),
        &format!("{} connections · {} groups", c.conns.len(), c.groups.len()),
    );
    let sel = c.sel.min(c.groups.len().saturating_sub(1));
    let g = c.groups.get(sel);
    let (dt, dr) = match g {
        Some(g) => (
            g.key.label().to_string(),
            format!("{} · {} connections", g.key.kind(), g.connections()),
        ),
        None => ("Details".to_string(), String::new()),
    };
    let (list, details) = list_and_details(
        frame,
        area,
        Section {
            title: Title::new("Clients", &right),
            rows: None,
        },
        Section {
            title: Title::new(&dt, &dr),
            rows: Some(DETAIL_ROWS),
        },
        0,
    );
    let side = side_split(area.width).is_some();
    let (top, pre, selected) = group_table(list, c, trouble(&c.poll, ctx), ctx.more);
    list_below(frame, list, top, pre - 1, selected);

    let dw = usize::from(details.width);
    let mut dl = Vec::new();
    if let Some(g) = g {
        let said = format!(
            " {} connections · oldest {} · longest idle {} · {}",
            g.connections(),
            age(g.oldest_age),
            age(g.longest_idle),
            format::bytes(g.mem)
        );
        if side {
            dl.extend(paragraph(Vec::new(), " ", &said, style::fg(), dw));
        } else {
            dl.push(Line::from(fit(vec![Span::styled(said, style::fg())], dw)));
        }
        if g.flagged() {
            dl.extend(paragraph(
                vec![Span::styled(format!(" {MARK} "), style::warning())],
                "   ",
                &format!(
                    "{} of them {} been idle for {} or more. Idle clients hold memory and a connection each.",
                    g.idle_conns,
                    if g.idle_conns == 1 { "has" } else { "have" },
                    idle_hours()
                ),
                style::warning(),
                dw,
            ));
        }
        dl.push(Line::from(Span::styled(
            " enter lists this group's connections.",
            style::dim(),
        )));
    }
    rows(frame, details, dl, None);
}

/// The groups table for a list `area`: its lines (the trouble line if any, the head,
/// the rows that fit around the selection), how many lines come before the rows, and
/// the selected row's line.
fn group_table(
    area: Rect,
    c: &Clients,
    trouble_line: Option<Line<'static>>,
    more: bool,
) -> (Vec<Line<'static>>, usize, Option<usize>) {
    let sel = c.sel.min(c.groups.len().saturating_sub(1));
    // The columns start after the marker gutter.
    let w = usize::from(gutter_area(area).width);
    let cols = group_columns();
    let (keep, gw) = pick_flex(&cols, G_GROUP, w, after_gutter(WIDE_AT), more);
    let head = head_row_at(
        &cols,
        &keep,
        G_GROUP,
        gw,
        &[G_CONNS, G_IDLE, G_OLDEST, G_MEM],
    );
    let mut top = with_head(trouble_line, head);
    let pre = top.len();
    let body_h = usize::from(area.height).saturating_sub(pre);
    let off = scroll_offset(sel, body_h);
    top.extend(
        c.groups
            .iter()
            .skip(off)
            .take(body_h)
            .map(|g| group_line(g, &cols, &keep, gw)),
    );
    if c.groups.is_empty() {
        top.push(Line::from(Span::styled(" No connections.", style::dim())));
    }
    let selected = (!c.groups.is_empty()).then(|| sel - off + pre);
    (top, pre, selected)
}

/// Rows a group's connections leave at the bottom of the right pane for the selected
/// connection's facts, from [`SIDE_AT`](super::widgets::SIDE_AT) columns: a rule and
/// three lines.
pub(super) const CONN_FACTS_SIDE: u16 = 4;

/// An opened group from [`SIDE_AT`](super::widgets::SIDE_AT) columns: the groups on
/// the left, the opened one marked with a dim `›`; its connections on the right, which
/// has the keys, and the selected connection's facts under them in the same pane.
fn drilled_side(frame: &mut Frame, area: Rect, state: &ServerState, ctx: &Ctx<'_>) {
    let c: &Clients = &state.clients;
    let conns = c.open_conns();
    let key = c.drill.as_ref();
    let name = key.map_or("", |k| k.label());
    let left_right = right_title(
        state,
        ctx,
        &secs(CLIENTS_EVERY),
        &format!("{} groups", c.groups.len()),
    );
    let right_left = format!("Connections of {name}");
    let n = conns.len();
    let right_right = format!("{n} connection{} · esc back", if n == 1 { "" } else { "s" });
    // The opened (focused) connections get the larger share: about 40/60, the
    // reverse of the groups view.
    let lw = u16::try_from(u32::from(area.width) * 40 / 100).unwrap_or(area.width / 2);
    let inner = super::widgets::frame_panes(
        frame,
        area,
        &[lw, 1],
        &[
            Title::new("Clients", &left_right),
            Title::new(&right_left, &right_right),
        ],
        1,
    );
    let (left, right) = (inner[0], inner[1]);
    // The groups, the opened one marked but not selected: the keys are on the right.
    let (mut top, pre, _) = group_table(left, c, None, false);
    // The rows start where `group_table` scrolled them to; the opened group is marked
    // only when it is among them.
    let off = scroll_offset(
        c.sel.min(c.groups.len().saturating_sub(1)),
        usize::from(left.height).saturating_sub(pre),
    );
    let open = c
        .groups
        .iter()
        .position(|g| Some(&g.key) == key)
        .and_then(|i| i.checked_sub(off));
    let rest = top.split_off(pre.min(top.len()));
    rows(frame, gutter_area(left), top, None);
    list_open(
        frame,
        Rect {
            y: left.y + pre as u16,
            height: left.height.saturating_sub(pre as u16),
            ..left
        },
        rest,
        open,
    );
    // The connections, then a rule and the selected one's facts.
    let facts_h = CONN_FACTS_SIDE.min(right.height);
    let list = Rect {
        height: right.height - facts_h,
        ..right
    };
    let facts = Rect {
        y: list.y + list.height,
        height: facts_h,
        ..right
    };
    // AGE and LIBRARY / NAME stay beside the address whenever they fit.
    let (lines, pre, selected, cn) = conn_table(list, c, trouble(&c.poll, ctx), ctx.more, 0);
    list_below(frame, list, lines, pre - 1, selected);
    if facts.height > 0 {
        rows(
            frame,
            Rect { height: 1, ..facts },
            vec![Line::from(Span::styled(
                "─".repeat(usize::from(facts.width)),
                style::border(),
            ))],
            None,
        );
        conn_facts(
            frame,
            Rect {
                y: facts.y + 1,
                height: facts.height - 1,
                ..facts
            },
            cn,
        );
    }
}

fn group_line(g: &Group, cols: &[Col], keep: &[usize], gw: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for &i in keep {
        spans.push(Span::raw(" "));
        let w = if i == G_GROUP { gw - 1 } else { cols[i].w };
        spans.push(match i {
            G_GROUP => Span::styled(pad_r(&trunc(g.key.label(), w), w), style::fg()),
            G_MARK => Span::styled(if g.flagged() { MARK } else { " " }, style::warning()),
            G_CONNS => Span::styled(
                pad_l(&format::count(g.connections() as u64), w),
                style::fgb(),
            ),
            G_IDLE => Span::styled(
                pad_l(&age(g.longest_idle), w),
                if g.flagged() {
                    style::warning()
                } else {
                    style::fg()
                },
            ),
            G_OLDEST => Span::styled(pad_l(&age(g.oldest_age), w), style::fg()),
            G_CMD => Span::styled(pad_r(cmd_text(&g.top_cmd), w), style::fg()),
            G_MEM => Span::styled(pad_l(&format::bytes(g.mem), w), style::fg()),
            G_BY => Span::styled(pad_r(g.key.kind(), w), style::dim()),
            _ => Span::raw(""),
        });
    }
    Line::from(spans)
}

/// The hour the flag is set at, in words: `1 hour`.
fn idle_hours() -> String {
    let h = IDLE_FLAG_SECS / 3600;
    format!("{h} hour{}", if h == 1 { "" } else { "s" })
}

const C_ADDR: usize = 0;
const C_ID: usize = 1;
const C_IDLE: usize = 2;
const C_AGE: usize = 3;
const C_LIB: usize = 4;
const C_CMD: usize = 5;
const C_MEM: usize = 6;

fn conn_columns() -> [Col; 7] {
    [
        Col::new("ADDRESS", 16, Tier::Base),
        Col::new("ID", 7, Tier::Base),
        Col::new("IDLE", 8, Tier::Base),
        Col::new("AGE", 8, Tier::Wide),
        Col::new("LIBRARY / NAME", 16, Tier::Wide),
        Col::new("LAST COMMAND", 12, Tier::More),
        Col::new("MEM", 7, Tier::More),
    ]
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
    let sel = c.conn_sel.min(conns.len().saturating_sub(1));
    let cn = conns.get(sel).copied();
    let (dt, dr) = match cn {
        Some(cn) => (cn.addr.clone(), format!("id {}", cn.id)),
        None => ("Details".to_string(), String::new()),
    };
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Clients", &right),
                rows: None,
            },
            Section {
                title: Title::new(&dt, &dr),
                rows: Some(DETAIL_ROWS),
            },
        ],
    );
    let (lines, pre, selected, cn) = conn_table(
        inner[0],
        c,
        trouble(&c.poll, ctx),
        ctx.more,
        after_gutter(WIDE_AT),
    );
    list_below(frame, inner[0], lines, pre - 1, selected);
    conn_facts(frame, inner[1], cn);
}

/// An opened group's connections for a list `area`, as [`group_table`], and the
/// selected connection.
fn conn_table<'c>(
    area: Rect,
    c: &'c Clients,
    trouble_line: Option<Line<'static>>,
    more: bool,
    wide_at: usize,
) -> (
    Vec<Line<'static>>,
    usize,
    Option<usize>,
    Option<&'c ClientConn>,
) {
    let conns = c.open_conns();
    let sel = c.conn_sel.min(conns.len().saturating_sub(1));
    let cn = conns.get(sel).copied();
    // The columns start after the marker gutter.
    let w = usize::from(gutter_area(area).width);
    let cols = conn_columns();
    let (keep, aw) = pick_flex(&cols, C_ADDR, w, wide_at, more);
    let head = head_row_at(&cols, &keep, C_ADDR, aw, &[C_ID, C_IDLE, C_AGE, C_MEM]);
    let mut lines = with_head(trouble_line, head);
    let pre = lines.len();
    let body_h = usize::from(area.height).saturating_sub(pre);
    let off = scroll_offset(sel, body_h);
    lines.extend(conns.iter().skip(off).take(body_h).map(|cn| {
        let idle_long = cn.idle >= IDLE_FLAG_SECS;
        let mut spans = Vec::new();
        for &i in &keep {
            spans.push(Span::raw(" "));
            let cw = if i == C_ADDR { aw - 1 } else { cols[i].w };
            spans.push(match i {
                C_ADDR => Span::styled(pad_r(&trunc(&cn.addr, cw), cw), style::fg()),
                C_ID => Span::styled(pad_l(&cn.id.to_string(), cw), style::dim()),
                C_IDLE => Span::styled(
                    pad_l(
                        &if idle_long {
                            format!("{MARK} {}", age(cn.idle))
                        } else {
                            age(cn.idle)
                        },
                        cw,
                    ),
                    if idle_long {
                        style::warning()
                    } else {
                        style::fg()
                    },
                ),
                C_AGE => Span::styled(pad_l(&age(cn.age), cw), style::fg()),
                C_LIB => Span::styled(pad_r(&trunc(&lib_label(cn), cw), cw), style::fg()),
                C_CMD => Span::styled(pad_r(cmd_text(&cn.cmd), cw), style::fg()),
                _ => Span::styled(pad_l(&format::bytes(cn.mem), cw), style::fg()),
            });
        }
        Line::from(spans)
    }));
    if conns.is_empty() {
        lines.push(Line::from(Span::styled(
            " No connections in this group any more. esc goes back.",
            style::dim(),
        )));
    }
    let selected = (!conns.is_empty()).then(|| sel - off + pre);
    (lines, pre, selected, cn)
}

/// The selected connection's facts: its client, age and idle time, last command and
/// memory.
fn conn_facts(frame: &mut Frame, inner: Rect, cn: Option<&ClientConn>) {
    let dw = usize::from(inner.width);
    let mut dl = Vec::new();
    if let Some(cn) = cn {
        let label = |t: &str| Span::styled(format!(" {}", pad_r(t, 9)), style::dim());
        dl.push(Line::from(fit(
            vec![label("Client"), Span::styled(lib_label(cn), style::fg())],
            dw,
        )));
        dl.push(Line::from(fit(
            vec![
                label("Age"),
                Span::styled(age(cn.age), style::fg()),
                Span::styled(" · idle ", style::dim()),
                Span::styled(
                    age(cn.idle),
                    if cn.idle >= IDLE_FLAG_SECS {
                        style::warning()
                    } else {
                        style::fg()
                    },
                ),
            ],
            dw,
        )));
        dl.push(Line::from(fit(
            vec![
                label("Last"),
                Span::styled(cmd_text(&cn.cmd).to_string(), style::cmd()),
                Span::styled(" · memory ", style::dim()),
                Span::styled(format::bytes(cn.mem), style::fg()),
            ],
            dw,
        )));
    }
    rows(frame, inner, dl, None);
}

// ---------------------------------------------------------------------------
// Slow log
// ---------------------------------------------------------------------------

const L_MARK: usize = 0;
const L_TIME: usize = 1;
const L_DUR: usize = 2;
const L_CMD: usize = 3;
const L_KEYS: usize = 4;
const L_CLIENT: usize = 5;

fn slow_columns() -> [Col; 6] {
    [
        Col::new("", 1, Tier::Base),
        Col::new("TIME", 9, Tier::Base),
        Col::new("DURATION", 9, Tier::Base),
        Col::new("COMMAND", 13, Tier::Base),
        Col::new("KEYS", 8, Tier::Base),
        Col::new("CLIENT", 21, Tier::Wide),
    ]
}

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
    let right = right_title(
        state,
        ctx,
        &secs(SLOWLOG_EVERY),
        &format!("{} entries · UTC", s.entries.len()),
    );
    let took = s.selected().map_or_else(String::new, |e| micros(e.micros));
    let (list, entry) = list_and_details(
        frame,
        area,
        Section {
            title: Title::new("Slow log", &right),
            rows: None,
        },
        Section {
            title: Title::new("Entry", &took),
            rows: Some(super::SLOW_LOG_DETAIL as u16),
        },
        0,
    );
    let side = side_split(area.width).is_some();
    let inner = [list, entry];
    let w = usize::from(inner[0].width);
    let cols = slow_columns();
    // The columns start after the marker gutter.
    let lw = usize::from(gutter_area(inner[0]).width);
    let (keep, kw) = pick_flex(&cols, L_KEYS, lw, after_gutter(WIDE_AT), ctx.more);

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
        (None, Some(note)) => status.push(Span::styled(note.clone(), style::warning())),
        (None, None) => {}
    }
    let status = trouble(&s.poll, ctx).unwrap_or_else(|| Line::from(fit(status, w)));

    let head = head_row_at(&cols, &keep, L_KEYS, kw, &[L_DUR]);
    let mut lines = vec![status, head];
    let pre = lines.len();
    let body_h = usize::from(inner[0].height).saturating_sub(pre);
    let sel = s.sel.min(s.entries.len().saturating_sub(1));
    let off = scroll_offset(sel, body_h);
    lines.extend(s.entries.iter().skip(off).take(body_h).map(|e| {
        let note = note_of(e).is_some();
        let mut spans = Vec::new();
        for &i in &keep {
            spans.push(Span::raw(" "));
            let cw = if i == L_KEYS { kw - 1 } else { cols[i].w };
            spans.push(match i {
                L_MARK => Span::styled(if note { MARK } else { " " }, style::warning()),
                L_TIME => Span::styled(pad_r(&utc_time(e.unix), cw), style::dim()),
                L_DUR => Span::styled(pad_l(&micros(e.micros), cw), style::fgb()),
                L_CMD => Span::styled(pad_r(&trunc(&e.command, cw), cw), style::cmd()),
                L_KEYS => Span::styled(pad_r(&trunc(&keys_text(e), cw), cw), style::fg()),
                L_CLIENT => Span::styled(pad_r(&trunc(&e.client, cw), cw), style::dim()),
                _ => Span::raw(""),
            });
        }
        Line::from(spans)
    }));
    if s.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            " The slow log is empty.",
            style::dim(),
        )));
    }
    let selected = (!s.entries.is_empty()).then(|| sel - off + pre);
    list_below(frame, inner[0], lines, pre - 1, selected);
    entry_lines(frame, inner[1], s, side);
}

/// The selected entry. `wrap`: its arguments, keys and client broken over lines (the
/// details to the right of the list, at full height) rather than cut at the width.
fn entry_lines(frame: &mut Frame, inner: Rect, s: &SlowLog, wrap: bool) {
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
    let when = vec![
        label("when"),
        Span::styled(format_utc(e.unix), style::fg()),
        Span::styled(" · ", style::dim()),
        Span::styled(micros(e.micros), style::fgb()),
        Span::styled(" · ", style::dim()),
        Span::styled(e.command.clone(), style::cmd()),
    ];
    let mut lines = if wrap {
        super::widgets::wrap_spans(when, w, 8)
    } else {
        vec![Line::from(fit(when, w))]
    };
    if wrap {
        let indent = " ".repeat(8);
        let args = escape(&String::from_utf8_lossy(e.args.as_bytes()));
        lines.extend(paragraph(
            vec![label("args")],
            &indent,
            &args,
            style::fg(),
            w,
        ));
        if e.args_cut > 0 {
            lines.push(Line::from(Span::styled(
                trunc(
                    &format!("{indent}… {} more arguments", format::grouped(e.args_cut)),
                    w,
                ),
                style::dim(),
            )));
        }
        lines.extend(paragraph(
            vec![label("keys")],
            &indent,
            &keys,
            style::fg(),
            w,
        ));
        lines.extend(paragraph(
            vec![label("client")],
            &indent,
            &client,
            style::fg(),
            w,
        ));
    } else {
        lines.push(Line::from(fit(args_spans(label("args"), e), w)));
        lines.push(Line::from(fit(
            vec![label("keys"), Span::styled(keys, style::fg())],
            w,
        )));
        lines.push(Line::from(fit(
            vec![label("client"), Span::styled(client, style::fg())],
            w,
        )));
    }
    if let Some(n) = note_of(e) {
        lines.extend(paragraph(
            vec![Span::styled(format!(" {MARK} "), style::warning())],
            "   ",
            n.text(),
            style::warning(),
            w,
        ));
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

const M_TIME: usize = 0;
const M_DB: usize = 1;
const M_CLIENT: usize = 2;
const M_COMMAND: usize = 3;

fn monitor_columns() -> [Col; 4] {
    [
        Col::new("TIME", 13, Tier::Base),
        Col::new("DB", 4, Tier::More),
        Col::new("CLIENT", 22, Tier::Wide),
        Col::new("COMMAND", 12, Tier::Base),
    ]
}

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

/// Monitor's question, while it is open: the one confirm sheet ([`super::sheet`]),
/// docked under the panel. `y`/`n`, or on a production profile its name typed.
pub fn monitor_sheet(m: &Monitor, ctx: &Ctx<'_>) -> Option<super::sheet::Sheet> {
    use super::sheet::{Answer, Sheet, right_of};
    let p = m.prompt.as_ref()?;
    let profile = ctx.profile();
    let node = ctx
        .session
        .picked_node()
        .map(|n| n.endpoint.to_string())
        .unwrap_or_else(|| ctx.session.endpoint().to_string());
    Some(Sheet {
        title: "Start MONITOR?".into(),
        right: right_of(&profile.name, profile.env),
        lines: vec![
            ("", format!("Server {}", crate::value::escape(&node)), style::fg()),
            ("Sends", "MONITOR".into(), style::fgb()),
            (
                "",
                "MONITOR runs on a connection of its own, and the server copies every command it runs to it. That slows a busy server.".into(),
                style::fg(),
            ),
            (
                "",
                format!(
                    "It stops after {} s or {} lines, or with esc. The lines stay in memory.",
                    MAX_TIME.as_secs(),
                    format::grouped(MAX_LINES as u64)
                ),
                style::dim(),
            ),
        ],
        answer: match p {
            Prompt::YesNo => Answer::YesNo("Start"),
            Prompt::TypeName(t) => Answer::Type {
                name: profile.name.clone(),
                typed: t.clone(),
                verb: "Start",
            },
        },
        wait: false,
    })
}

/// True when Monitor draws the selected line to the right of the log, at full height,
/// on a body `area`: from [`SIDE_AT`](super::widgets::SIDE_AT) columns, once a run has
/// started, for as long as it runs or its lines are held. Decided by the run, never by
/// a line, so a long line arriving mid-run does not flip the layout or the page size.
/// Before a run the Line section stays under the log, as below 120 columns.
pub(super) fn monitor_side(area: Rect, m: &Monitor, _ctx: &Ctx<'_>) -> bool {
    side_split(area.width).is_some() && (m.running() || !m.lines.is_empty())
}

fn monitor(frame: &mut Frame, area: Rect, m: &Monitor, ctx: &Ctx<'_>) {
    let right = monitor_right(m, ctx);
    let side = monitor_side(area, m, ctx);
    let inner = if side {
        let lw = side_split(area.width).unwrap_or(area.width);
        super::widgets::frame_panes(
            frame,
            area,
            &[lw, 1],
            &[Title::new("Monitor", &right), Title::new("Line", "")],
            0,
        )
    } else {
        sections(
            frame,
            area,
            &[
                Section {
                    title: Title::new("Monitor", &right),
                    rows: None,
                },
                Section {
                    title: Title::new("Line", ""),
                    rows: Some(super::MONITOR_DETAIL as u16),
                },
            ],
        )
    };
    let w = usize::from(inner[0].width);
    let text = |t: String, st: Style| Line::from(fit(vec![Span::styled(t, st)], w));
    let status = if let Some(why) = crate::conn::monitor_unsupported(ctx.profile()) {
        text(format!(" {MARK} {why}"), style::warning())
    } else if let Some(stop) = &m.stopped {
        let st = if matches!(
            stop,
            crate::tui::app::views::server::Stop::Failed(_)
                | crate::tui::app::views::server::Stop::Lost
        ) {
            style::danger()
        } else {
            style::dim()
        };
        let mut t = format!(" {}", stop.text());
        if m.ranking().is_some() && ctx.report().is_some_and(|r| r.hot_keys.is_empty()) {
            t.push_str(" Analyze › Hot keys ranks its keys.");
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
    let cols = monitor_columns();
    // The columns start after the marker gutter.
    let lw = usize::from(gutter_area(inner[0]).width);
    let (keep, cw) = pick_flex(&cols, M_COMMAND, lw, after_gutter(WIDE_AT), ctx.more);
    let head = head_row_at(&cols, &keep, M_COMMAND, cw, &[]);
    let mut lines = vec![status, head];
    let pre = lines.len();
    let body_h = usize::from(inner[0].height).saturating_sub(pre);
    let sel = m.sel.min(m.lines.len().saturating_sub(1));
    let off = scroll_offset(sel, body_h);
    lines.extend(m.lines.iter().skip(off).take(body_h).map(|e| {
        let mut spans = Vec::new();
        for &i in &keep {
            spans.push(Span::raw(" "));
            let w = if i == M_COMMAND { cw - 1 } else { cols[i].w };
            spans.push(match i {
                M_TIME => Span::styled(pad_r(&line_time(e), w), style::dim()),
                M_DB => Span::styled(pad_r(&escape(&e.line.db), w), style::dim()),
                M_CLIENT => {
                    Span::styled(pad_r(&trunc(&escape(&e.line.client), w), w), style::dim())
                }
                _ => Span::styled(escape(&e.command_line()), style::fg()),
            });
        }
        Line::from(fit(spans, lw))
    }));
    let selected = (!m.lines.is_empty()).then(|| sel - off + pre);
    if m.lines.is_empty() && (m.running() || m.stopped.is_some()) {
        let t = if m.running() {
            " No lines yet."
        } else {
            " No lines."
        };
        lines.push(text(t.to_string(), style::dim()));
    }
    list_below(frame, inner[0], lines, pre - 1, selected);
    let cmd_rows = if side {
        usize::from(inner[1].height).saturating_sub(1)
    } else {
        2
    };
    line_lines(frame, inner[1], m.selected(), cmd_rows);
}

/// The selected line in full, as far as `cmd_rows` rows under its first take it.
fn line_lines(frame: &mut Frame, inner: Rect, e: Option<&Entry>, cmd_rows: usize) {
    let w = usize::from(inner.width);
    let Some(e) = e else {
        return;
    };
    let when = e.unix().map_or_else(|| "—".to_string(), format_utc);
    let first = vec![
        Span::styled(" ", style::dim()),
        Span::styled(when, style::fg()),
        Span::styled(" · db ", style::dim()),
        Span::styled(escape(&e.line.db), style::fg()),
        Span::styled(" · client ", style::dim()),
        Span::styled(escape(&e.line.client), style::fg()),
    ];
    // To the right of the log (more than two rows for the command) the first line
    // wraps too.
    let mut lines = if cmd_rows > 2 {
        super::widgets::wrap_spans(first, w, 1)
    } else {
        vec![Line::from(fit(first, w))]
    };
    let cmd_rows = cmd_rows.saturating_sub(lines.len() - 1);
    let mut cmd = escape(&e.command_line());
    if e.cut > 0 {
        cmd.push_str(&format!(
            " … {} more arguments not kept",
            format::grouped(e.cut)
        ));
    }
    let room = w.saturating_sub(1).max(1);
    let chars: Vec<char> = cmd.chars().collect();
    let mut rest: &[char] = &chars;
    for row in 0..cmd_rows {
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
        if row + 1 == cmd_rows && !rest.is_empty() {
            part = trunc(&format!("{part}…"), room);
        }
        lines.push(Line::from(Span::styled(format!(" {part}"), style::fg())));
    }
    rows(frame, inner, lines, None);
}
