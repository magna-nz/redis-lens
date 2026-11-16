//! Pure rendering: `draw(frame, &app)` paints the whole screen from the state.
//!
//! The chrome, from the mockups: two rows on top, the body, and a one-line footer. Row 1
//! is the context bar (the connection, its environment, ` w ` and its access, ` b ` and
//! its database, the picked node on a cluster, the server's figures, the status on the
//! right). Row 2 is the areas with the current area's panels inline (`1 Overview
//! 2 Browse › Keys · Streams · Search 3 Analyze 4 Server 5 Pub/Sub`, `0 Connections` at
//! the right). The footer has the 3 to 5 keys of the screen and `ctrl-f Find key
//! : Command  ? Help` on the right (the `ctrl-f` prompt takes its place while it is
//! open). Dialogs
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

mod actions;
mod analysis;
mod bulk;
mod cmdline;
mod confirm;
mod connections;
mod dbs;
mod delta;
mod diff;
pub mod help;
pub(crate) mod hot;
mod keys;
mod nodes;
pub mod overview;
mod patterns;
mod pubsub;
pub(crate) mod search;
mod server;
pub mod sheet;
mod slots;
pub(crate) mod streams;
pub mod style;
mod ttl;
mod value;
pub mod widgets;

use widgets::{rows, trunc, width};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

/// The context bar, the area row, the body and the footer.
fn layout(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// The body of a screen of `area`'s size: under the two chrome rows, above the footer.
pub fn body_of(area: Rect) -> Rect {
    layout(area)[2]
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
    let mut body = body_of(area);
    if screen == Screen::Connections {
        // Less the dashboard line, while there is one.
        body = connections::split_dashboard(body, app).0;
    }
    let h = usize::from(body.height);
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
/// screen of `area`: the body less the frame's two borders, the header row and the
/// bottom line.
pub fn browse_rows(area: Rect) -> usize {
    usize::from(body_of(area).height.saturating_sub(4))
}

/// True while a dialog is open over the screen: the screen behind it is dimmed. A
/// question (the confirm sheet) and the Actions menu dim nothing: the row they act on
/// stays readable.
pub fn dialog_open(app: &App) -> bool {
    app.help
        || app.analyze.is_some()
        || app
            .bulk
            .as_ref()
            .is_some_and(|b| !matches!(b.stage, crate::tui::app::bulk::Stage::Asking { .. }))
        || app.dbs.is_some()
}

/// The question open as a sheet, if any: the write gate's, Monitor's or the command
/// line's.
pub fn open_sheet(app: &App) -> Option<sheet::Sheet> {
    let mut sh = open_sheet_of(app)?;
    sh.wait = app.in_grace();
    Some(sh)
}

fn open_sheet_of(app: &App) -> Option<sheet::Sheet> {
    if let Some(c) = &app.confirm {
        return Some(confirm::sheet(c));
    }
    let ctx = app.ctx()?;
    if app.views.cmdline.open {
        return cmdline::confirm_sheet(&app.views.cmdline, &ctx);
    }
    if app.monitor_asking() {
        return server::monitor_sheet(&app.views.server.monitor, &ctx);
    }
    None
}

pub fn draw(frame: &mut Frame, app: &App) {
    let full = frame.area();
    let [head, area_row, body, foot] = layout(full);
    header(frame, head, app);
    areas(frame, area_row, app);
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
    // A dialog, or a question a reply opened a moment ago, dims the rest.
    if dialog_open(app) || app.in_grace() {
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
    if app.menu.is_some() {
        actions::draw(frame, body, app);
    }
    if let Some(d) = &app.analyze {
        overview::draw_analyze(frame, body, d, app);
    }
    if app.help {
        help::draw(frame, body, app);
    }
    // The question takes every key first, so it is drawn last: on top of everything.
    if let Some(sh) = open_sheet(app) {
        sheet::draw(frame, body, &sh);
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

/// The context bar: the connection's name, environment, ` w ` and the write state,
/// ` b ` and the database, the picked node on a cluster, then as much of the address,
/// server version, memory and keys as fits, and the status on the right.
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
        // The badge and the key cap carry their own padding: one space between.
        Span::raw(" "),
        Span::styled(format!(" {} ", p.env.label()), style::env_badge(p.env)),
        Span::raw(" "),
        Span::styled(" w ", style::key()),
        Span::raw(" "),
        write_state(p),
    ];
    let parts = header_parts(app, s);
    // The status's time gives way first: it is cut to `14:02`, then left out, before
    // any part but the address and the version (shown only when they fit) is dropped
    // or cut.
    for clock in [Clock::Utc, Clock::Short, Clock::Off] {
        let right = status(app, s, clock);
        let (keep, whole) = fit_parts(&mut parts.clone(), spans_width(&left), &right, w);
        if whole || clock == Clock::Off {
            let mut after_fact = false;
            for part in keep {
                let sep = if part.chip || !after_fact {
                    "   "
                } else {
                    " · "
                };
                after_fact = !part.chip;
                left.push(Span::styled(sep, style::dim()));
                left.extend(part.spans);
            }
            return justify(left, right, w);
        }
    }
    unreachable!("the last clock always returns")
}

/// One optional part of the context bar, after the name, the badge and the access.
#[derive(Debug, Clone)]
struct Part {
    /// The rank it keeps its place by when the line is short (lowest first).
    rank: u8,
    spans: Vec<Span<'static>>,
    /// A control with its key cap (` b ` db, `[ ]` node): three spaces before it, and
    /// before the first fact after it. Facts are joined by ` · `.
    chip: bool,
    /// Cut rather than dropped when it does not fit whole (the keychain hint, a
    /// cluster's summary): at most one part is.
    cut: bool,
}

impl Part {
    fn chip(rank: u8, spans: Vec<Span<'static>>) -> Self {
        Self {
            rank,
            spans,
            chip: true,
            cut: false,
        }
    }

    fn fact(rank: u8, span: Span<'static>) -> Self {
        Self {
            rank,
            spans: vec![span],
            chip: false,
            cut: false,
        }
    }

    fn cut(mut self) -> Self {
        self.cut = true;
        self
    }

    fn width(&self) -> usize {
        spans_width(&self.spans)
    }
}

/// The context bar's optional parts in the order they show: ` b ` and the database,
/// the picked node on a cluster, then the facts.
fn header_parts(app: &App, s: &Session) -> Vec<Part> {
    let p = &s.profile;
    // Database 0, the default, shows when it fits, as the address does; any other is
    // kept before the figures.
    let db_rank = if p.db == 0 { DB0_RANK } else { 0 };
    let mut parts = vec![Part::chip(
        db_rank,
        vec![
            Span::styled(" b ", style::key()),
            Span::styled(format!(" db {}", p.db), style::fg()),
        ],
    )];
    if let Some(node) = s.node_title() {
        parts.push(Part::chip(
            2,
            vec![
                Span::styled(" [ ] ", style::key()),
                Span::styled(format!(" {node}"), style::fg()),
            ],
        ));
    }
    let cluster = match &s.target {
        Some(ScanTarget::Cluster(summary)) => Some(summary.clone()),
        _ => None,
    };
    // A connect waiting on the OS keychain says so first: during an analysis the old
    // results stay in the body, where the hint would otherwise go.
    if let Some(hint) = app.keychain_hint() {
        parts.push(Part::fact(0, Span::styled(hint, style::amber())).cut());
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
            parts.push(Part::fact(0, Span::styled(summary.clone(), st)).cut());
        }
        None => parts.push(Part::fact(
            ADDRESS_RANK,
            Span::styled(trunc(&s.endpoint().to_string(), 40), style::dim()),
        )),
    }
    if s.read_alone() {
        // A cluster node read alone on purpose: only its keys are seen.
        parts.push(Part::fact(1, Span::styled("standalone", style::amber())));
    }
    match &s.target {
        Some(ScanTarget::Replica(ep)) => parts.push(Part::fact(
            3,
            Span::styled(
                format!("replica {}", trunc(&ep.to_string(), 24)),
                style::dim(),
            ),
        )),
        Some(ScanTarget::PrimaryFallback(_)) => parts.push(Part::fact(
            3,
            Span::styled("primary (replica unreachable)", style::amber()),
        )),
        _ => {}
    }
    if let Some(srv) = &s.server {
        parts.push(Part::fact(
            VERSION_RANK,
            Span::styled(server_label(srv), style::dim()),
        ));
        let mem = if srv.maxmemory > 0 {
            format!(
                "{} / {}",
                format::bytes(srv.used_memory),
                format::bytes(srv.maxmemory)
            )
        } else {
            format!("{} used", format::bytes(srv.used_memory))
        };
        parts.push(Part::fact(0, Span::styled(mem, style::dim())));
        parts.push(Part::fact(
            0,
            Span::styled(
                format!("{} keys", format::count(srv.total_keys)),
                style::dim(),
            ),
        ));
    }
    if sizes_unavailable(app, s) {
        parts.push(Part::fact(4, Span::styled("▲ no sizes", style::warn())));
    }
    parts
}

/// Parts of the context bar ranked at or above this are shown only when they fit, in
/// rank order and only while each one before them did: the address, then database 0,
/// then the server version. The status's time gives way before the others.
const WHEN_IT_FITS: u8 = 5;
/// The ranks of the parts shown only when they fit: see [`WHEN_IT_FITS`].
const ADDRESS_RANK: u8 = WHEN_IT_FITS;
const DB0_RANK: u8 = WHEN_IT_FITS + 1;
const VERSION_RANK: u8 = WHEN_IT_FITS + 2;

/// The context bar's `parts` (by rank, see [`header_parts`]) that fit on a line `w`
/// wide after `left_w` cells and before `right`, in their order. True when every part
/// ranked below [`WHEN_IT_FITS`] is there whole.
fn fit_parts(
    parts: &mut [Part],
    left_w: usize,
    right: &[Span<'static>],
    w: usize,
) -> (Vec<Part>, bool) {
    // Keep the parts by rank while the whole line, the status, a gap of two and
    // [`justify`]'s margin fit.
    let fixed = left_w + spans_width(right) + 3;
    let mut keep = vec![false; parts.len()];
    let mut whole = true;
    let mut used = 0;
    let mut cut_one = false;
    // Once one part shown only when it fits does not, none ranked after it shows: the
    // version never stands in for a missing address.
    let mut optional_full = false;
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| parts[i].rank);
    for i in order {
        // Each part comes after a three-cell separator.
        let add = parts[i].width() + 3;
        let optional = parts[i].rank >= WHEN_IT_FITS;
        if optional && optional_full {
            continue;
        }
        if fixed + used + add <= w {
            keep[i] = true;
            used += add;
            continue;
        }
        if optional {
            optional_full = true;
        } else {
            whole = false;
        }
        // The keychain hint or a cluster's summary is cut rather than dropped, once.
        let room = w.saturating_sub(fixed + used + 3);
        if parts[i].cut && !cut_one && room >= 12 {
            cut_one = true;
            let span = &parts[i].spans[0];
            parts[i].spans = vec![Span::styled(trunc(&span.content, room), span.style)];
            keep[i] = true;
            used += room + 3;
        }
    }
    let kept = parts
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(part, _)| part.clone())
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

/// Why Browse › Search is dimmed in the area row: its probe (`FT._LIST`) was refused
/// or skipped, so there is no search module to use.
fn search_off(app: &App) -> bool {
    app.session
        .as_ref()
        .and_then(|s| app.entry(s.id))
        .and_then(|e| e.perms.as_ref())
        .and_then(|p| p.checks.iter().find(|c| c.label == PROBE_FT_LIST))
        .is_some_and(|c| !matches!(c.status, PermStatus::Allowed))
}

/// How much of the area row is spelled out: `0 Connections` or only `0`, the other
/// areas' names or only their digits, the current area's panels all named or only the
/// current one with where it is (`Keys · 1 of 3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AreaFit {
    connections: bool,
    names: bool,
    panels: bool,
}

/// The fits [`area_line`] tries, widest first.
const AREA_FITS: [AreaFit; 4] = [
    AreaFit {
        connections: true,
        names: true,
        panels: true,
    },
    AreaFit {
        connections: false,
        names: true,
        panels: true,
    },
    AreaFit {
        connections: false,
        names: false,
        panels: true,
    },
    AreaFit {
        connections: false,
        names: false,
        panels: false,
    },
];

/// The left of the area row: `1 Overview   2 Browse › Keys · Streams · Search
/// 3 Analyze …`. The current area is bold with its digit as a key cap, its current
/// panel bold in the focus colour, the rest dim; before connecting areas 1 to 5 are
/// faint.
fn area_spans(app: &App, fit: AreaFit) -> Vec<Span<'static>> {
    let connected = app.session.is_some();
    let other = if connected {
        style::tab()
    } else {
        style::track()
    };
    let mut out = vec![Span::raw(" ")];
    for a in Area::ALL.into_iter().filter(|a| *a != Area::Connections) {
        if out.len() > 1 {
            out.push(Span::raw("   "));
        }
        if a != app.area {
            let text = if fit.names {
                format!("{} {}", a.digit(), a.title())
            } else {
                a.digit().to_string()
            };
            out.push(Span::styled(text, other));
            continue;
        }
        out.push(Span::styled(format!(" {} ", a.digit()), style::key()));
        out.push(Span::styled(format!(" {}", a.title()), style::fgb()));
        out.extend(panel_spans(app, fit.panels));
    }
    out
}

/// The current area's panels after its name: ` › Keys · Streams · Search`, or
/// ` › Keys · 1 of 3`. Nothing on an area without panels.
fn panel_spans(app: &App, all: bool) -> Vec<Span<'static>> {
    let panels = app.panels_of(app.area);
    if panels.is_empty() {
        return Vec::new();
    }
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
    let mut out = vec![Span::styled(" › ", style::dim())];
    if all {
        for (i, p) in panels.iter().enumerate() {
            if i > 0 {
                out.push(Span::styled(" · ", style::dim()));
            }
            out.push(Span::styled(p.name(), style_of(*p)));
        }
    } else {
        let at = current
            .and_then(|c| panels.iter().position(|p| *p == c))
            .unwrap_or(0);
        let name = current.map_or("", Panel::name);
        out.push(Span::styled(name, style::title()));
        out.push(Span::styled(
            format!(" · {} of {}", at + 1, panels.len()),
            style::dim(),
        ));
    }
    out
}

/// `0 Connections` at the right of the area row (only `0` when the row is short), a key
/// cap and bold, and always named, while it is the area on screen.
fn connections_spans(app: &App, names: bool) -> Vec<Span<'static>> {
    if app.area == Area::Connections {
        vec![
            Span::styled(" 0 ", style::key()),
            Span::styled(" Connections", style::fgb()),
        ]
    } else {
        let title = if names { " Connections" } else { "" };
        vec![Span::styled(format!("0{title}"), style::tab())]
    }
}

/// The note the area row carries on Browse, at its right before `0 Connections`:
/// names only, or Search off.
fn browse_note(app: &App) -> Option<&'static str> {
    if app.area != Area::Browse {
        return None;
    }
    let current = app.screen().panel();
    // Old results kept during an analysis are marked in the panels' own titles
    // ([`mark_previous`]), not here.
    let names = current == Some(Panel::Keys) && app.pattern_data().is_some_and(|d| d.names);
    if names && app.analyzing() {
        Some("names only until the analysis has results")
    } else if names {
        Some("names only, no analysis yet")
    } else if search_off(app) {
        Some("Search is off: no search module")
    } else {
        None
    }
}

/// The area row: the areas with the current area's panels inline, `0 Connections` at
/// the right, and on Browse a short note before it. When it does not fit, `0
/// Connections` becomes `0`, then the other areas show only their digits, then the
/// panels only the current one (`Keys · 1 of 3`); the note is kept while any of these
/// fits with it, and dropped only when none does.
fn area_line(app: &App, w: usize) -> Line<'static> {
    let note = browse_note(app);
    let build = |fit: AreaFit, with_note: bool| {
        let left = area_spans(app, fit);
        let mut right = Vec::new();
        if let Some(n) = note.filter(|_| with_note) {
            right.push(Span::styled(n, style::dim()));
            right.push(Span::raw("   "));
        }
        right.extend(connections_spans(app, fit.connections));
        right.push(Span::raw(" "));
        let fits = spans_width(&left) + spans_width(&right) + 2 <= w;
        (left, right, fits)
    };
    let mut tries: Vec<(AreaFit, bool)> = Vec::new();
    if note.is_some() {
        tries.extend(AREA_FITS.iter().map(|f| (*f, true)));
    }
    tries.extend(AREA_FITS.iter().map(|f| (*f, false)));
    for (fit, with_note) in tries {
        let (left, right, fits) = build(fit, with_note);
        if fits {
            return justify_flush(left, right, w);
        }
    }
    // Not even the shortest fits: its left part, cut by the frame.
    Line::from(area_spans(app, AREA_FITS[3]))
}

/// `left`, then `right` flush against the right edge.
fn justify_flush(
    mut left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    w: usize,
) -> Line<'static> {
    let gap = w.saturating_sub(spans_width(&left) + spans_width(&right));
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

fn areas(frame: &mut Frame, area: Rect, app: &App) {
    let line = area_line(app, usize::from(area.width));
    rows(frame, area, vec![line], None);
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
        Span::styled("▏", style::focus()),
        Span::styled(hint.to_string(), style::dim()),
    ])
}

/// The footer's key hints: the screen's 3 to 5 keys, then `ctrl-f Find key  : Command
/// ? Help` against the right edge ([`keymap::footer_fit`]): the screen's keys that do
/// not fit are left out from the end.
fn key_line(app: &App, w: usize) -> Line<'static> {
    let screen = app.screen();
    let c = app.footer_ctx();
    let (left, right) = keymap::footer_fit(screen, c, w);
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
            r.push(Span::raw("   "));
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

/// A footer of `keys` (` y Delete   n Cancel`), then `note` dim.
fn hints_line(keys: &[(&'static str, &'static str)], note: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, l)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(*k, style::key()));
        spans.push(Span::styled(format!(" {l}"), style::dim()));
    }
    if !note.is_empty() {
        spans.push(Span::styled(format!("   {note}"), style::dim()));
    }
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

/// Dim after the `ctrl-f` prompt's text.
pub const FIND_HINT: &str = "  a name, or a pattern with * ? [ ]";

/// What a dialog that asks a question puts in the footer.
pub const QUESTION_OPEN: &str = " A question is open: answer it, or esc to cancel.";

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let line = if let Some(sh) = open_sheet(app) {
        hints_line(&sh.keys(), if sh.wait { sheet::WAIT } else { "" })
    } else if app.help {
        help_footer(app)
    } else if app.menu.is_some() {
        hints_line(&actions::footer(), actions::FOOTER_NOTE)
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
    } else if app.find.open {
        input_line(
            "Find key:",
            crate::value::escape(&app.find.text),
            FIND_HINT,
            w,
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Msg;
    use crate::tui::app::views::testing::{render, scanned, show};
    use crate::tui::fixtures;

    fn lines(app: &App, w: u16, h: u16) -> Vec<String> {
        let t = render(app, w, h);
        let b = t.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect())
            .collect()
    }

    /// Two chrome rows on every width: the context bar, the area row, then the body's
    /// frame on the third row, the footer on the last.
    #[test]
    fn two_chrome_rows_at_120_80_and_60_columns() {
        let mut app = scanned();
        for (w, h) in [(120, 36), (80, 24), (60, 20)] {
            assert_eq!(body_of(Rect::new(0, 0, w, h)), Rect::new(0, 2, w, h - 3));
            for screen in [
                Screen::Connections,
                Screen::Overview,
                Screen::Patterns,
                Screen::Ttl,
                Screen::Clients,
                Screen::PubSub,
            ] {
                show(&mut app, screen);
                let l = lines(&app, w, h);
                assert!(l[0].starts_with(" prod-cache-01 "), "{w}: {}", l[0]);
                assert!(l[0].contains("read-only"), "{w}: {}", l[0]);
                assert!(l[1].contains(screen.area().title()), "{w}: {}", l[1]);
                assert!(l[2].starts_with('┌'), "{screen:?} at {w}: {}", l[2]);
                for line in &l {
                    assert_eq!(line.chars().count(), usize::from(w), "{line}");
                }
            }
        }
    }

    /// The area row: the current area's panels inline, the others by name where there
    /// is room, only by digit where there is not, and the panels as `Name · n of N`
    /// last; `0 Connections` at the right.
    #[test]
    fn the_area_row_at_120_80_and_60_columns() {
        let mut app = scanned();
        show(&mut app, Screen::Ttl);
        let row = |app: &App, w| lines(app, w, 24)[1].clone();
        let r = row(&app, 120);
        assert!(
            r.starts_with(
                " 1 Overview   2 Browse    3  Analyze › Big keys · Expiry · Changes · Hot keys   4 Server   5 Pub/Sub"
            ),
            "{r}"
        );
        assert!(r.ends_with("0 Connections "), "{r}");
        let r = row(&app, 80);
        assert!(
            r.starts_with(" 1   2    3  Analyze › Big keys · Expiry · Changes · Hot keys   4   5"),
            "{r}"
        );
        assert!(r.ends_with(" 0 "), "{r}");
        let r = row(&app, 60);
        assert!(
            r.starts_with(" 1   2    3  Analyze › Expiry · 2 of 4   4   5"),
            "{r}"
        );
        // The current area's digit is a key cap, its panel in the focus colour.
        let t = render(&app, 120, 24);
        let b = t.backend().buffer();
        let at = |s: &str| r_find(&row(&app, 120), s);
        assert_eq!(b[(at(" 3 ") as u16, 1)].style().bg, style::key().bg);
        assert_eq!(b[(at("Expiry") as u16, 1)].fg, style::FOCUS);
        assert_eq!(b[(at("Changes") as u16, 1)].fg, style::DIM);
    }

    /// The column (in cells) where `s` starts in `line`.
    fn r_find(line: &str, s: &str) -> usize {
        let byte = line.find(s).unwrap_or_else(|| panic!("{s:?} in {line}"));
        widgets::width(&line[..byte])
    }

    /// The context bar, in order: the name, the badge, ` w ` and the access, ` b ` and
    /// the database, then the figures; the status at the right. Narrower, the parts go
    /// in this order: the version, database 0, the address (each shown only while the
    /// ones before it fit), the status's time, the key count, the memory.
    #[test]
    fn the_context_bar_parts_and_their_drop_order() {
        let mut app = scanned();
        app.set_wall_clock(50_520);
        let bar = |app: &App, w| lines(app, w, 24)[0].clone();
        let wide = bar(&app, 160);
        let order = [
            " prod-cache-01 ",
            " PROD ",
            " w  read-only",
            " b  db 0",
            "prod-cache-01.internal:6380",
            "Redis 7.4.2",
            "15G / 24G",
            "7.92M keys",
            "● analyzed",
        ];
        let at: Vec<usize> = order.iter().map(|s| r_find(&wide, s)).collect();
        assert!(at.windows(2).all(|p| p[0] < p[1]), "{wide}");
        assert!(wide.ends_with("UTC "), "{wide}");
        // The exact parts at each width.
        const DB: &str = "db 0";
        const ADDR: &str = "prod-cache-01.internal:6380";
        const VER: &str = "Redis 7.4.2";
        const MEM: &str = "15G / 24G";
        const KEYS: &str = "7.92M keys";
        const UTC: &str = "14:02 UTC";
        const TIME: &str = "14:02";
        let all = [DB, ADDR, VER, MEM, KEYS, UTC, TIME];
        let cases: [(u16, &[&str]); 9] = [
            (160, &[DB, ADDR, VER, MEM, KEYS, UTC, TIME]),
            (130, &[DB, ADDR, MEM, KEYS, UTC, TIME]),
            // The address without database 0: the version does not stand in for it.
            (120, &[ADDR, MEM, KEYS, UTC, TIME]),
            (110, &[MEM, KEYS, UTC, TIME]),
            (83, &[MEM, KEYS, UTC, TIME]),
            (80, &[MEM, KEYS, TIME]),
            (75, &[MEM, KEYS]),
            (70, &[MEM]),
            (50, &[]),
        ];
        for (w, want) in cases {
            let b = bar(&app, w);
            for part in all {
                assert_eq!(
                    b.contains(part),
                    want.contains(&part),
                    "{part:?} at {w}: {b}"
                );
            }
        }
        // As the bar narrows a part, once gone, stays gone; and the version or database 0
        // never shows without the address.
        let drops = [VER, DB, ADDR, "UTC", TIME, KEYS, MEM];
        let mut gone_at = Vec::new();
        for (i, part) in drops.iter().enumerate() {
            let w = (40..=160u16)
                .rev()
                .find(|&w| !bar(&app, w).contains(part))
                .unwrap_or(0);
            gone_at.push(w);
            if i > 0 {
                assert!(
                    w <= gone_at[i - 1],
                    "{part} goes at {w}, before {} at {}",
                    drops[i - 1],
                    gone_at[i - 1]
                );
            }
        }
        for w in 40..=160u16 {
            let b = bar(&app, w);
            if b.contains(VER) || b.contains(DB) {
                assert!(b.contains(ADDR), "at {w}: {b}");
            }
        }
        // The name, the badge and the access stay at every width that holds them.
        for w in [40, 50, 60, 80] {
            let b = bar(&app, w);
            assert!(b.contains("prod-cache-01") && b.contains("PROD"), "{b}");
            assert!(b.contains("read-only"), "{b}");
        }
        // Another database is kept before the figures.
        let id = app.session.as_ref().unwrap().id;
        app.session.as_mut().unwrap().profile.db = 3;
        let b = bar(&app, 80);
        assert!(b.contains(" b  db 3   15G / 24G"), "{b}");
        // On a cluster with its nodes read, the picked node after the database.
        let scan = app.session.as_ref().unwrap().scan;
        app.apply(Msg::Nodes {
            id,
            scan,
            nodes: fixtures::live_nodes(),
        });
        let b = bar(&app, 160);
        assert!(
            b.contains(" b  db 3    [ ]  node 10.0.1.11:6379 · 1 of 3   "),
            "{b}"
        );
        // The profile's name only: no user name, never a password.
        for w in [40, 80, 120, 160] {
            let b = bar(&app, w);
            assert!(!b.contains("lens") && !b.contains('@'), "{b}");
        }
    }

    /// Before connecting the bar reads as it always has.
    #[test]
    fn the_context_bar_before_connecting() {
        let app = App::new(fixtures::profiles());
        let l = lines(&app, 80, 24);
        assert!(l[0].starts_with(" redis-lens "), "{}", l[0]);
        assert!(l[0].contains("not connected"), "{}", l[0]);
        assert!(l[0].ends_with("pick a connection, then enter "), "{}", l[0]);
        assert!(l[1].ends_with(" 0  Connections "), "{}", l[1]);
    }
}
