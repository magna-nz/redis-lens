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
use crate::live::server::SlowEntry;
use crate::snapshot::format_utc;
use crate::tui::app::Screen;
use crate::tui::app::views::server::{
    CLIENTS_EVERY, Clients, Group, IDLE_FLAG_SECS, Metric, Monitor, Overview, Poll, Prompt,
    SLOWLOG_EVERY, ServerState, SlowLog, lib_label, note_of,
};
use crate::tui::app::views::{Ctx, LiveState};
use crate::value::escape;

use super::analysis::{Col, MARK, Tier, head_row_at, notice, paragraph, pick_flex};
use super::widgets::{Section, Title, fit, pad_l, pad_r, rows, scroll_offset, sections, trunc};
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
        let line = Line::from(Span::styled(format!(" ▲ {off}"), style::amber()));
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
    if c.drill.is_some() {
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
    let w = usize::from(inner[0].width);
    let cols = group_columns();
    let (keep, gw) = pick_flex(&cols, G_GROUP, w, WIDE_AT, ctx.more);
    let head = head_row_at(
        &cols,
        &keep,
        G_GROUP,
        gw,
        &[G_CONNS, G_IDLE, G_OLDEST, G_MEM],
    );
    let mut top = with_head(trouble(&c.poll, ctx), head);
    let pre = top.len();
    let body_h = usize::from(inner[0].height).saturating_sub(pre);
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
    rows(frame, inner[0], top, selected);

    let dw = usize::from(inner[1].width);
    let mut dl = Vec::new();
    if let Some(g) = g {
        dl.push(Line::from(fit(
            vec![Span::styled(
                format!(
                    " {} connections · oldest {} · longest idle {} · {}",
                    g.connections(),
                    age(g.oldest_age),
                    age(g.longest_idle),
                    format::bytes(g.mem)
                ),
                style::fg(),
            )],
            dw,
        )));
        if g.flagged() {
            dl.extend(paragraph(
                vec![Span::styled(format!(" {MARK} "), style::amber())],
                "   ",
                &format!(
                    "{} of them {} been idle for {} or more. Idle clients hold memory and a connection each.",
                    g.idle_conns,
                    if g.idle_conns == 1 { "has" } else { "have" },
                    idle_hours()
                ),
                style::amber(),
                dw,
            ));
        }
        dl.push(Line::from(Span::styled(
            " enter lists this group's connections.",
            style::dim(),
        )));
    }
    rows(frame, inner[1], dl, None);
}

fn group_line(g: &Group, cols: &[Col], keep: &[usize], gw: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for &i in keep {
        spans.push(Span::raw(" "));
        let w = if i == G_GROUP { gw - 1 } else { cols[i].w };
        spans.push(match i {
            G_GROUP => Span::styled(pad_r(&trunc(g.key.label(), w), w), style::fg()),
            G_MARK => Span::styled(if g.flagged() { MARK } else { " " }, style::amber()),
            G_CONNS => Span::styled(
                pad_l(&format::count(g.connections() as u64), w),
                style::fgb(),
            ),
            G_IDLE => Span::styled(
                pad_l(&age(g.longest_idle), w),
                if g.flagged() {
                    style::amber()
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
    let w = usize::from(inner[0].width);
    let cols = conn_columns();
    let (keep, aw) = pick_flex(&cols, C_ADDR, w, WIDE_AT, ctx.more);
    let head = head_row_at(&cols, &keep, C_ADDR, aw, &[C_ID, C_IDLE, C_AGE, C_MEM]);
    let mut lines = with_head(trouble(&c.poll, ctx), head);
    let pre = lines.len();
    let body_h = usize::from(inner[0].height).saturating_sub(pre);
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
                        style::amber()
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
    rows(frame, inner[0], lines, selected);

    let dw = usize::from(inner[1].width);
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
                        style::amber()
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
    rows(frame, inner[1], dl, None);
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
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Slow log", &right),
                rows: None,
            },
            Section {
                title: Title::new(
                    "Entry",
                    &s.selected().map_or_else(String::new, |e| micros(e.micros)),
                ),
                rows: Some(6),
            },
        ],
    );
    let w = usize::from(inner[0].width);
    let cols = slow_columns();
    let (keep, kw) = pick_flex(&cols, L_KEYS, w, WIDE_AT, ctx.more);

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
                L_MARK => Span::styled(if note { MARK } else { " " }, style::amber()),
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
    rows(frame, inner[0], lines, selected);
    entry_lines(frame, inner[1], s);
}

fn entry_lines(frame: &mut Frame, inner: Rect, s: &SlowLog) {
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
        lines.extend(paragraph(
            vec![Span::styled(format!(" {MARK} "), style::amber())],
            "   ",
            n.text(),
            style::amber(),
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

fn monitor(frame: &mut Frame, area: Rect, m: &Monitor, ctx: &Ctx<'_>) {
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Monitor", &monitor_right(m, ctx)),
                rows: None,
            },
            Section {
                title: Title::new("Line", ""),
                rows: Some(4),
            },
        ],
    );
    let w = usize::from(inner[0].width);
    let text = |t: String, st: Style| Line::from(fit(vec![Span::styled(t, st)], w));
    if let Some(p) = &m.prompt {
        let profile = &ctx.profile().name;
        let mut lines = vec![
            Line::default(),
            text(format!(" Send MONITOR to {profile}?"), style::fgb()),
            Line::default(),
        ];
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            "MONITOR runs on a connection of its own, and the server copies every command it runs to it. That slows a busy server.",
            style::fg(),
            w,
        ));
        lines.extend(paragraph(
            vec![Span::raw(" ")],
            " ",
            &format!(
                "It stops after {} s or {} lines, or with esc. The lines stay in memory.",
                MAX_TIME.as_secs(),
                format::grouped(MAX_LINES as u64)
            ),
            style::dim(),
            w,
        ));
        lines.push(Line::default());
        lines.push(match p {
            Prompt::YesNo => text(" y starts it, n cancels.".to_string(), style::amber()),
            Prompt::TypeName(_) => text(
                format!(" A production profile: type {profile}, then enter. esc cancels."),
                style::amber(),
            ),
        });
        rows(frame, inner[0], lines, None);
        return;
    }
    let status = if let Some(why) = crate::conn::monitor_unsupported(ctx.profile()) {
        text(format!(" {MARK} {why}"), style::amber())
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
    let (keep, cw) = pick_flex(&cols, M_COMMAND, w, WIDE_AT, ctx.more);
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
        Line::from(fit(spans, usize::from(inner[0].width)))
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
    rows(frame, inner[0], lines, selected);
    line_lines(frame, inner[1], m.selected());
}

/// The selected line in full, as far as two rows take it.
fn line_lines(frame: &mut Frame, inner: Rect, e: Option<&Entry>) {
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
