//! Pure rendering: `draw(frame, &app)` paints the whole screen from the state.
//!
//! The chrome, from the mockups: a one-line header (the connection and its status), the
//! tab row of the areas (`0 Connections │ 1 Overview 2 Browse 3 Analyze 4 Server
//! 5 Pub/Sub`), a sub-tab row on the areas with panels, the body, and a one-line footer
//! with the 3 to 5 keys of the screen and `: Command  ? Help` on the right. Dialogs
//! (the write gate's question, a bulk action, the database list, "Analyze the keys?",
//! `?`) draw over the screen dimmed.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use crate::conn::parse::mask;
use crate::conn::{ScanTarget, provider};
use crate::format;
use crate::model::{NodeReport, NodeStatus, PERM_MEMORY, PermStatus, ServerFlavor, ServerInfo};

use super::app::views::{Ctx, Owner, PROBE_FT_LIST};
use super::app::{App, Area, Panel, Phase, Screen, Session};
use super::keymap;

mod analysis;
mod bulk;
mod cmdline;
mod confirm;
mod connections;
mod dbs;
mod delta;
mod diff;
pub mod help;
mod hot;
mod keys;
mod nodes;
pub mod overview;
mod patterns;
mod pubsub;
mod search;
mod server;
mod slots;
mod streams;
pub mod style;
mod ttl;
mod value;
pub mod widgets;

use widgets::{rows, trunc, width};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

/// The header, the tab row, the sub-tab row (empty without panels), the body and the
/// footer.
fn layout(area: Rect, sub_tabs: bool) -> [Rect; 5] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(u16::from(sub_tabs)),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// The body of a screen of `area`'s size on an area with panels or without.
pub fn body_of(area: Rect, sub_tabs: bool) -> Rect {
    layout(area, sub_tabs)[3]
}

/// Rows of details under the Connections list (`ui/connections.rs`).
const CONNECTIONS_DETAIL: usize = 9;
/// Rows of details under the Nodes list (`ui/nodes.rs`).
const NODES_DETAIL: usize = 8;
/// Rows of details under the Clients list, and under a group's connections
/// (`ui/server.rs`).
const CLIENTS_DETAIL: usize = 5;
/// Rows of the entry under the Slow log list (`ui/server.rs`).
const SLOW_LOG_DETAIL: usize = 6;
/// Rows of the line under the Monitor list (`ui/server.rs`).
const MONITOR_DETAIL: usize = 4;
/// Rows of the selected slot under the hottest slots, and range rows shown at most
/// above them (`ui/slots.rs`).
const SLOTS_DETAIL: usize = 4;
const SLOTS_RANGES: usize = 5;

/// List rows the screen on show draws on a terminal of `area` when its list is longer
/// than the screen: what PgUp and PgDn move by (`App::set_list_rows`; the runtime calls
/// it with this after every frame). 0 when not even one row fits.
///
/// - Expiry, Changes, Hot keys, Big keys and Pub/Sub draw the list and the selection's
///   details side by side in one frame over the whole body: the body less the two
///   borders and the column header.
/// - Browse's lists have a bottom line too ([`browse_rows`]); a key's value lists its
///   items in the top part of the right pane.
/// - The full-width lists (Connections, Nodes, Clients, Slow log, Monitor, Slots) have
///   their details below them in the same frame: their own section of it, less the
///   column header (and the status line above it where there is one).
pub fn list_rows(area: Rect, app: &App) -> usize {
    let screen = app.screen();
    let h = usize::from(body_of(area, screen.area().has_panels()).height);
    // A frame of sections ([`widgets::sections`]): one border per section and the
    // bottom one; the first section never gets more than the body less two borders.
    let first = |rows: usize| rows.min(h.saturating_sub(2));
    let below = |sections: usize, fixed: usize| h.saturating_sub(sections + 1 + fixed);
    match screen {
        Screen::Patterns | Screen::KeyList | Screen::Streams | Screen::Search => browse_rows(area),
        Screen::KeyDetail => value_item_rows(h),
        Screen::Overview
        | Screen::BigKeys
        | Screen::Ttl
        | Screen::Diff
        | Screen::HotKeys
        | Screen::PubSub => h.saturating_sub(3),
        // The list section takes what it needs up to what leaves the details theirs.
        Screen::Connections => first(below(2, CONNECTIONS_DETAIL).max(4)).saturating_sub(1),
        Screen::Nodes => first(below(2, NODES_DETAIL).max(3)).saturating_sub(1),
        // The list takes what the details leave, less its header and, when the poll is
        // in trouble, the line saying so.
        Screen::Clients => {
            let trouble = app
                .ctx()
                .is_some_and(|ctx| server::trouble(&app.views.server.clients.poll, &ctx).is_some());
            below(2, CLIENTS_DETAIL).saturating_sub(1 + usize::from(trouble))
        }
        // A status line, then the header.
        Screen::SlowLog => below(2, SLOW_LOG_DETAIL).saturating_sub(2),
        Screen::Monitor => below(2, MONITOR_DETAIL).saturating_sub(2),
        // The ranges (their header and at most five), the hottest slots, the slot.
        Screen::Slots => {
            let ranges = app.session.as_ref().map_or(0, |s| {
                app.views.slots.range_rows(&s.nodes).len().min(SLOTS_RANGES)
            });
            below(3, ranges + 1 + SLOTS_DETAIL).saturating_sub(1)
        }
    }
}

/// Item rows a key's value lists on a body `h` rows tall: the right pane less the facts
/// line and the blank under it, of which the item list takes two fifths (at least three
/// rows) with its header (`ui/value.rs`).
fn value_item_rows(h: usize) -> usize {
    let pane = h.saturating_sub(2).saturating_sub(2);
    (pane * 2 / 5).max(3).min(pane).saturating_sub(1)
}

/// List rows a Browse list (Patterns, a pattern's keys, Streams, Search) shows on a
/// screen of `area`: the body under the sub-tab row less the frame's two borders, the
/// header row and the bottom line.
pub fn browse_rows(area: Rect) -> usize {
    usize::from(body_of(area, true).height.saturating_sub(4))
}

/// True while a dialog is open over the screen: the screen behind it is dimmed.
pub fn dialog_open(app: &App) -> bool {
    app.help
        || app.analyze.is_some()
        || app.confirm.is_some()
        || app.bulk.is_some()
        || app.dbs.is_some()
}

pub fn draw(frame: &mut Frame, app: &App) {
    let full = frame.area();
    let [head, tab_row, sub_row, body, foot] = layout(full, app.area.has_panels());
    header(frame, head, app);
    tabs(frame, tab_row, app);
    if app.area.has_panels() {
        sub_tabs(frame, sub_row, app);
    }
    let screen = app.screen();
    match screen {
        Screen::Connections => connections::draw(frame, body, app),
        Screen::Overview => overview::draw(frame, body, app),
        Screen::Patterns => patterns::draw(frame, body, app),
        Screen::Ttl => ttl::draw(frame, body, app),
        Screen::Diff => diff::draw(frame, body, app),
        Screen::HotKeys => hot::draw(frame, body, app),
        Screen::Nodes => nodes::draw(frame, body, app),
        _ => view_state(frame, body, app, screen),
    }
    if app.views.cmdline.open
        && let Some(ctx) = app.ctx()
    {
        cmdline::draw(frame, body, &app.views.cmdline, &ctx);
    }
    footer(frame, foot, app);
    if dialog_open(app) {
        // Everything but the footer, which says what the dialog wants.
        widgets::dim(
            frame,
            Rect {
                height: full.height.saturating_sub(1),
                ..full
            },
        );
    }
    if let Some(b) = &app.bulk {
        bulk::draw(frame, body, b, app.tick);
    }
    if let Some(l) = &app.dbs {
        dbs::draw(frame, body, l);
    }
    if let Some(c) = &app.confirm {
        confirm::draw(frame, body, c);
    }
    if let Some(d) = &app.analyze {
        overview::draw_analyze(frame, body, d, app);
    }
    if app.help {
        help::draw(frame, body, app);
    }
}

/// The body before connecting, on every area but Connections.
pub const CONNECT_FIRST: &str =
    " Connect first: open 0 Connections, pick a connection and press enter.";

/// A screen a view state owns: its draw function, or why there is nothing to draw (no
/// session, the view's permission probe failed, no analysis yet).
fn view_state(frame: &mut Frame, area: Rect, app: &App, screen: Screen) {
    let title = screen.name();
    let Some(ctx) = app.ctx() else {
        notice_panels(
            frame,
            area,
            title,
            "",
            vec![Line::from(Span::styled(CONNECT_FIRST, style::dim()))],
        );
        return;
    };
    // Browse › Streams and Search: dimmed in their two-pane frame, with the reason.
    if let Some(why) = ctx.off_reason().filter(|_| screen.area() == Area::Browse) {
        streams::browse_notice(
            frame,
            area,
            title,
            "off",
            &[(format!(" {why}"), style::dim())],
        );
        return;
    }
    if let Some(why) = ctx.off_reason() {
        let lines = vec![Line::from(Span::styled(
            format!(" ▲ {why}"),
            style::amber(),
        ))];
        notice_panels(frame, area, title, "off", lines);
        return;
    }
    let Some(owner) = screen.owner() else {
        return;
    };
    let v = &app.views;
    match owner {
        // Browse › Keys, a pattern's keys and a key's value: two panes of one frame.
        Owner::Keys => keys::draw(frame, area, app),
        Owner::Value => value::draw(frame, area, app),
        // Big keys: "No analysis yet." (or connecting, analyzing, failed) until there is
        // a report, as the other Analyze panels.
        Owner::Analysis => {
            if !analysis::need_report(frame, area, app, title, "ranks the biggest keys.") {
                analysis::draw(frame, area, &v.analysis, &ctx);
            }
        }
        Owner::Server => server::draw(frame, area, &v.server, &ctx),
        Owner::Streams => streams::draw(frame, area, &v.streams, &ctx),
        Owner::PubSub => pubsub::draw(frame, area, &v.pubsub, &ctx),
        Owner::Search => search::draw(frame, area, &v.search, &ctx),
        Owner::Slots => slots::draw(frame, area, &v.slots, &ctx),
        // Not a screen: drawn over the body by `draw`.
        Owner::Cmdline => {}
    }
}

/// What a stub view draws until it is built: its title and "Not built yet".
pub fn not_built(frame: &mut Frame, area: Rect, title: &str, ctx: &Ctx<'_>) {
    let lines = vec![Line::from(Span::styled(
        format!(" Not built yet: {} comes later.", ctx.screen.title()),
        style::dim(),
    ))];
    notice_panels(frame, area, title, "", lines);
}

/// Marks results kept from the previous run while an analysis runs, or after it failed
/// before any results of its own came.
pub fn previous_marker(app: &App) -> Option<&'static str> {
    let s = app.session.as_ref()?;
    s.previous?;
    Some(match s.phase {
        Phase::Failed(_) => "analysis failed · previous results",
        _ => "analyzing · previous results",
    })
}

/// `right`, a panel's right title, with [`previous_marker`] first when there is one,
/// so a narrow title cuts the rest, not the marker.
pub fn mark_previous(app: &App, right: &str) -> String {
    match (previous_marker(app), right.is_empty()) {
        (Some(m), true) => m.to_string(),
        (Some(m), false) => format!("{m} · {right}"),
        (None, _) => right.to_string(),
    }
}

/// A blank line and the profile's provider notice (see [`provider::notice`]), if any.
fn provider_notice(lines: &mut Vec<Line<'static>>, p: &crate::model::Profile) {
    if let Some(n) = provider::notice(p.provider) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(format!(" ▲ {n}"), style::amber())));
    }
}

/// One frame holding a few lines of explanation under `title` (the frame layout every
/// screen uses: no empty details panel beside or below it).
pub fn notice_panels(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    right: &str,
    lines: Vec<Line<'static>>,
) {
    let inner = widgets::sections(
        frame,
        area,
        &[widgets::Section {
            title: widgets::Title::new(title, right),
            rows: None,
        }],
    )[0];
    let mut all = vec![Line::default()];
    all.extend(lines);
    rows(frame, inner, all, None);
}

fn flavor(f: ServerFlavor) -> &'static str {
    match f {
        ServerFlavor::Redis => "Redis",
        ServerFlavor::Valkey => "Valkey",
        ServerFlavor::Unknown => "Server",
    }
}

pub fn server_label(s: &ServerInfo) -> String {
    format!("{} {}", flavor(s.flavor), s.version)
}

/// Whether a cluster scan lost a shard, to draw its summary in amber: the connect
/// summary ([`ScanTarget::Cluster`]) says a node was unreachable or only the seed could
/// be read, or the finished report has a node that is not `scanned`.
pub fn cluster_degraded(summary: &str, nodes: Option<&[NodeReport]>) -> bool {
    summary.contains("unreachable")
        || summary.contains("seed node only")
        || nodes.is_some_and(|n| n.iter().any(|n| n.status != NodeStatus::Scanned))
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// `left`, then `right` pushed against the right edge when there is room.
fn justify(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let lw = spans_width(&left);
    let rw = spans_width(&right);
    if !right.is_empty() && lw + rw + 2 <= w {
        left.push(Span::raw(" ".repeat(w - lw - rw - 1)));
        left.extend(right);
    }
    Line::from(left)
}

fn utc_time(unix: i64) -> String {
    let s = unix.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn sizes_unavailable(app: &App, s: &Session) -> bool {
    match &s.report {
        Some(r) => !r.sizes_available,
        None => app
            .entry(s.id)
            .and_then(|e| e.perms.as_ref())
            .is_some_and(|p| !p.allowed(PERM_MEMORY)),
    }
}

/// How the header's status shows its time ([`status`]): `14:02 UTC` when it fits, else
/// `14:02`, else not at all, before any other part of the header gives way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    Utc,
    Short,
    Off,
}

/// The status at the right of the header: `● connected 14:02 UTC`, `⠋ analyzing 41%`,
/// `● analyzed 14:02 UTC`. "connected" carries the time the run connected, "analyzed"
/// the time of the report. Times are UTC: the app has no time zone.
fn status(app: &App, s: &Session, clock: Clock) -> Vec<Span<'static>> {
    let spin = spinner(app.tick);
    let at = |unix: Option<i64>| match (unix, clock) {
        (Some(t), Clock::Utc) => format!(" {} UTC", hh_mm(t)),
        (Some(t), Clock::Short) => format!(" {}", hh_mm(t)),
        _ => String::new(),
    };
    match &s.phase {
        // `esc` asked an analysis that is still connecting to stop.
        Phase::Connecting if s.analysis.is_some() && s.stopped => {
            vec![Span::styled(format!("{spin} stopping…"), style::amber())]
        }
        Phase::Connecting if s.analysis.is_some() && s.previous.is_some() => {
            vec![Span::styled(format!("{spin} analyzing"), style::amber())]
        }
        Phase::Connecting => vec![Span::styled(format!("{spin} connecting…"), style::amber())],
        Phase::Connected => vec![
            Span::styled("● ", style::ok()),
            Span::styled(
                format!("connected{}", at(s.connected_at_unix)),
                style::dim(),
            ),
        ],
        // `esc` asked it to stop: it ends after its current batch, keeping what it has.
        Phase::Scanning if s.stopped => {
            vec![Span::styled(format!("{spin} stopping…"), style::amber())]
        }
        Phase::Scanning => {
            let pr = s.progress.unwrap_or_default();
            let pct = (pr.sampled * 100)
                .checked_div(pr.target)
                .map(|p| format!(" {}%", p.min(100)))
                .unwrap_or_default();
            vec![Span::styled(
                format!("{spin} analyzing{pct}"),
                style::amber(),
            )]
        }
        Phase::Done => vec![
            Span::styled("● ", style::ok()),
            Span::styled(
                format!(
                    "analyzed{}{}",
                    at(s.report.as_ref().map(|r| r.taken_at_unix)),
                    // A partial report `esc` kept: said, not only coloured.
                    if super::app::views::overview::stopped_early(s) {
                        " (stopped)"
                    } else {
                        ""
                    }
                ),
                style::dim(),
            ),
        ],
        Phase::Failed(_) => vec![Span::styled("✗ failed", style::warn())],
    }
}

/// `14:02`, UTC.
pub fn hh_mm(unix: i64) -> String {
    let s = unix.rem_euclid(86_400);
    format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
}

/// The one header line: the connection's name, environment, write state, then as much
/// of address, server version, memory and keys as fits, and the status on the right.
fn header(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let line = match &app.session {
        None => justify(
            vec![
                Span::styled(
                    format!(" redis-lens {}", env!("CARGO_PKG_VERSION")),
                    style::fgb(),
                ),
                Span::styled("   not connected", style::dim()),
            ],
            vec![Span::styled("pick a connection, then enter", style::dim())],
            w,
        ),
        Some(s) => connected_header(app, s, w),
    };
    rows(frame, area, vec![line], None);
}

fn connected_header(app: &App, s: &Session, w: usize) -> Line<'static> {
    let p = &s.profile;
    let mut left = vec![
        Span::styled(format!(" {}", trunc(&p.name, 24)), style::fgb()),
        Span::raw("  "),
        Span::styled(format!(" {} ", p.env.label()), style::env_badge(p.env)),
        Span::raw("  "),
        write_state(p),
    ];
    // The optional parts, in the order they show, each with the rank it keeps its place
    // by when the line is short (lowest first).
    let cluster = match &s.target {
        Some(ScanTarget::Cluster(summary)) => Some(summary.clone()),
        _ => None,
    };
    let mut parts: Vec<(u8, Span<'static>)> = Vec::new();
    // A connect waiting on the OS keychain says so first: during an analysis the old
    // results stay in the body, where the hint would otherwise go.
    if let Some(hint) = app.keychain_hint() {
        parts.push((0, Span::styled(hint, style::amber())));
    }
    match &cluster {
        Some(summary) => {
            let nodes = s.report.as_ref().and_then(|r| r.nodes.as_deref());
            let st = if cluster_degraded(summary, nodes) {
                style::amber()
            } else {
                style::dim()
            };
            // On a cluster the summary says the most: it comes first, and is cut
            // rather than dropped.
            parts.push((0, Span::styled(summary.clone(), st)));
        }
        None => parts.push((
            5,
            Span::styled(trunc(&s.endpoint().to_string(), 40), style::dim()),
        )),
    }
    match &s.target {
        Some(ScanTarget::Replica(ep)) => parts.push((
            3,
            Span::styled(
                format!("replica {}", trunc(&ep.to_string(), 24)),
                style::dim(),
            ),
        )),
        Some(ScanTarget::PrimaryFallback(_)) => parts.push((
            3,
            Span::styled("primary (replica unreachable)", style::amber()),
        )),
        _ => {}
    }
    if p.db != 0 {
        // Not database 0: kept before the figures.
        parts.push((0, Span::styled(format!("db {}", p.db), style::amber())));
    }
    if let Some(srv) = &s.server {
        parts.push((6, Span::styled(server_label(srv), style::dim())));
        let mem = if srv.maxmemory > 0 {
            format!(
                "{} / {}",
                format::bytes(srv.used_memory),
                format::bytes(srv.maxmemory)
            )
        } else {
            format!("{} used", format::bytes(srv.used_memory))
        };
        parts.push((0, Span::styled(mem, style::fg())));
        parts.push((
            0,
            Span::styled(
                format!("{} keys", format::count(srv.total_keys)),
                style::fg(),
            ),
        ));
    }
    if sizes_unavailable(app, s) {
        parts.push((4, Span::styled("▲ no sizes", style::warn())));
    }
    // The status's time gives way first: it is cut to `14:02`, then left out, before
    // any part but the address and the version (shown only when they fit) is dropped
    // or cut.
    for clock in [Clock::Utc, Clock::Short, Clock::Off] {
        let right = status(app, s, clock);
        let (keep, whole) = fit_parts(&mut parts.clone(), spans_width(&left), &right, w);
        if whole || clock == Clock::Off {
            let mut first = true;
            for span in keep {
                let sep = if first { "   " } else { " · " };
                first = false;
                left.push(Span::styled(sep, style::dim()));
                left.push(span);
            }
            return justify(left, right, w);
        }
    }
    unreachable!("the last clock always returns")
}

/// Parts of the header ranked at or above this are shown only when they fit (the
/// address and the server version); the status's time gives way before the others.
const WHEN_IT_FITS: u8 = 5;

/// The header's `parts` (by rank, see [`connected_header`]) that fit on a line `w` wide
/// after `left_w` cells and before `right`, in their order. True when every part ranked
/// below [`WHEN_IT_FITS`] is there whole.
fn fit_parts(
    parts: &mut [(u8, Span<'static>)],
    left_w: usize,
    right: &[Span<'static>],
    w: usize,
) -> (Vec<Span<'static>>, bool) {
    // Keep the parts by rank while the whole line, the status, a gap of two and
    // [`justify`]'s margin fit.
    let fixed = left_w + spans_width(right) + 3;
    let mut keep = vec![false; parts.len()];
    let mut whole = true;
    let mut used = 0;
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| parts[i].0);
    for i in order {
        // Each part comes after a three-cell separator.
        let add = width(&parts[i].1.content) + 3;
        if fixed + used + add <= w {
            keep[i] = true;
            used += add;
            continue;
        }
        if parts[i].0 < WHEN_IT_FITS {
            whole = false;
        }
        // The keychain hint and a cluster's summary (rank 0, first) are cut rather
        // than dropped.
        let room = w.saturating_sub(fixed + used + 3);
        if parts[i].0 == 0 && used == 0 && room >= 12 {
            let cut = trunc(&parts[i].1.content, room);
            parts[i].1 = Span::styled(cut, parts[i].1.style);
            keep[i] = true;
            used += room + 3;
        }
    }
    let kept = parts
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|((_, span), _)| span.clone())
        .collect();
    (kept, whole)
}

/// The profile's write state: `read-only` (amber), `writes on` (red), or a red
/// `WRITES ON` badge on a `prod` profile with writes on.
fn write_state(p: &crate::model::Profile) -> Span<'static> {
    if p.is_read_only() {
        Span::styled("read-only", style::amber())
    } else if p.env == crate::model::Env::Prod {
        Span::styled(" WRITES ON ", style::env_badge(crate::model::Env::Prod))
    } else {
        Span::styled("writes on", style::warn())
    }
}

/// The tab row: `0 Connections │ 1 Overview 2 Browse 3 Analyze 4 Server 5 Pub/Sub`, the
/// area on screen highlighted. Before connecting, areas 1 to 5 are faint. When the row
/// does not fit, the other areas show only their digit.
fn tabs(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let connected = app.session.is_some();
    let build = |names: bool| {
        let mut spans = vec![Span::raw(" ")];
        for a in Area::ALL {
            let on = a == app.area;
            let text = if on || names {
                format!(" {} {} ", a.digit(), a.title())
            } else {
                format!(" {} ", a.digit())
            };
            let st = if on {
                style::tab_on()
            } else if connected || a == Area::Connections {
                style::tab()
            } else {
                style::track()
            };
            spans.push(Span::styled(text, st));
            spans.push(Span::styled(
                if a == Area::Connections { " │ " } else { " " },
                style::border(),
            ));
        }
        spans.pop();
        spans
    };
    let mut spans = build(true);
    if spans_width(&spans) > w {
        spans = build(false);
    }
    rows(frame, area, vec![Line::from(spans)], None);
}

/// Why Browse › Search is dimmed in the sub-tab row: its probe (`FT._LIST`) was refused
/// or skipped, so there is no search module to use.
fn search_off(app: &App) -> bool {
    app.session
        .as_ref()
        .and_then(|s| app.entry(s.id))
        .and_then(|e| e.perms.as_ref())
        .and_then(|p| p.checks.iter().find(|c| c.label == PROBE_FT_LIST))
        .is_some_and(|c| !matches!(c.status, PermStatus::Allowed))
}

/// The sub-tab row on an area with panels: `Keys · Streams · Search`, the current one
/// highlighted, and a short note on the right.
fn sub_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let panels = app.panels_of(app.area);
    let current = app.screen().panel();
    let style_of = |p: Panel| {
        if Some(p) == current {
            style::title()
        } else if p == Panel::Search && search_off(app) {
            style::track()
        } else {
            style::dim()
        }
    };
    // Spaced out when it fits, closer together when not; when even that is too wide,
    // only the current panel and where it is in the cycle.
    let names = |indent: &'static str, sep: &'static str| {
        let mut out = vec![Span::raw(indent)];
        for (i, p) in panels.iter().enumerate() {
            if i > 0 {
                out.push(Span::styled(sep, style::border()));
            }
            out.push(Span::styled(p.name(), style_of(*p)));
        }
        out
    };
    let mut left = names("    ", "  ·  ");
    if spans_width(&left) > w {
        left = names("  ", " · ");
    }
    if spans_width(&left) > w {
        let at = current
            .and_then(|c| panels.iter().position(|p| *p == c))
            .unwrap_or(0);
        let name = current.map_or("", Panel::name);
        left = vec![
            Span::raw("  "),
            Span::styled(name, style::title()),
            Span::styled(format!(" · {} of {}", at + 1, panels.len()), style::dim()),
        ];
    }
    // Old results kept during an analysis are marked in the panels' own titles
    // ([`mark_previous`]), not here.
    let names = current == Some(Panel::Keys) && app.pattern_data().is_some_and(|d| d.names);
    let note = if app.area != Area::Browse {
        String::new()
    } else if names && app.analyzing() {
        "names only until the analysis has results".to_string()
    } else if names {
        "names only, no analysis yet".to_string()
    } else if search_off(app) {
        "Search is off: no search module".to_string()
    } else {
        String::new()
    };
    let right = if note.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(note, style::dim())]
    };
    rows(frame, area, vec![justify(left, right, w)], None);
}

/// The last `w` cells of `s`, starting with `…` when cut.
fn tail(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let keep = w.saturating_sub(1);
    let start = chars.len().saturating_sub(keep);
    let mut out = String::from("…");
    out.extend(&chars[start..]);
    out
}

fn input_line(label: &str, text: String, hint: &str, w: usize) -> Line<'static> {
    let label = format!(" {label} ");
    let room = w.saturating_sub(width(&label) + 2 + width(hint)).max(8);
    Line::from(vec![
        Span::styled(label, style::key()),
        Span::styled(format!(" {}", tail(&text, room)), style::fgb()),
        Span::styled("▏", style::amber()),
        Span::styled(hint.to_string(), style::dim()),
    ])
}

/// The footer's key hints: the screen's 3 to 5 keys, then `: Command  ? Help` against
/// the right edge. Keys that do not fit are left out from the end.
fn key_line(app: &App, w: usize) -> Line<'static> {
    let screen = app.screen();
    let c = keymap::FooterCtx {
        ready: app.session.is_some(),
        analyzed: app.report().is_some(),
        analyzing: app.analyzing(),
        writes: app
            .session
            .as_ref()
            .is_some_and(|s| !s.profile.is_read_only()),
    };
    let mut left = keymap::footer(screen, c);
    let mut right = keymap::footer_right(c.ready);
    let fits = |l: &[(&str, &str)], r: &[(&str, &str)]| {
        let rw = keymap::footer_right_width(r);
        keymap::footer_width(l) + if rw == 0 { 0 } else { 4 + rw } <= w
    };
    while !left.is_empty() && !fits(&left, &right) {
        left.pop();
    }
    if !fits(&left, &right) {
        right.clear();
    }
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, l)) in left.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(*k, style::key()));
        spans.push(Span::styled(format!(" {l}"), style::dim()));
    }
    let mut r = Vec::new();
    for (i, (k, l)) in right.iter().enumerate() {
        if i > 0 {
            r.push(Span::raw("  "));
        }
        r.push(Span::styled(*k, style::key()));
        r.push(Span::styled(format!(" {l}"), style::dim()));
    }
    if r.is_empty() {
        return Line::from(spans);
    }
    let lw = spans_width(&spans);
    let rw = spans_width(&r);
    spans.push(Span::raw(" ".repeat(w.saturating_sub(lw + rw))));
    spans.extend(r);
    Line::from(spans)
}

/// The footer of the `?` help.
fn help_footer(app: &App) -> Line<'static> {
    if app.help_find.query.is_empty() {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("esc", style::key()),
            Span::styled(" Close  or start typing to search", style::dim()),
        ])
    } else {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("↑↓", style::key()),
            Span::styled(" Pick  ", style::dim()),
            Span::styled("enter", style::key()),
            Span::styled(" Go  ", style::dim()),
            Span::styled("esc", style::key()),
            Span::styled(" Clear", style::dim()),
        ])
    }
}

/// What a dialog that asks a question puts in the footer.
pub const QUESTION_OPEN: &str = " A question is open: answer it, or esc to cancel.";

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let line = if app.help {
        help_footer(app)
    } else if let Some(c) = &app.confirm {
        confirm::footer(c)
    } else if let Some(b) = &app.bulk {
        bulk::footer(b)
    } else if let Some(l) = &app.dbs {
        dbs::footer(l)
    } else if app.analyze.is_some() {
        Line::from(Span::styled(QUESTION_OPEN, style::dim()))
    } else if let Some(add) = &app.add {
        match &add.password {
            Some(_) => input_line(
                "password",
                "••••••".to_string(),
                "  enter save · esc cancel",
                w,
            ),
            None => input_line(
                "add",
                mask(&add.input),
                "  enter save · esc cancel · ctrl-u clear",
                w,
            ),
        }
    } else if app.screen() == Screen::Patterns && app.patterns.filtering {
        input_line(
            "/",
            app.patterns.filter.clone(),
            "   type to filter patterns · enter to apply · esc to clear",
            w,
        )
    } else if app.screen() == Screen::Connections && app.conn_filtering {
        input_line(
            "/",
            app.conn_filter.clone(),
            "   type a name or tag · enter to apply · esc to clear",
            w,
        )
    } else if let Some(input) = app.cmdline_input() {
        input_line(input.label, input.text, input.hint, w)
    } else if let Some(input) = app.view_input() {
        input_line(input.label, input.text, input.hint, w)
    } else if let Some(n) = &app.notice {
        let s = if n.error {
            style::warn()
        } else {
            style::amber()
        };
        Line::from(Span::styled(format!(" {}", n.text), s))
    } else {
        key_line(app, w)
    };
    rows(frame, area, vec![line], None);
}
