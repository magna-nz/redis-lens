//! Pure rendering: `draw(frame, &app)` paints the whole screen from the state.
//!
//! Layout, from the design mockup: three header lines (brand and connection, the
//! memory gauge, the tabs), a fourth with the panel names on views with panels, the
//! view's panels, and a key bar at the bottom.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use crate::conn::parse::mask;
use crate::conn::{ScanTarget, provider};
use crate::format;
use crate::model::{PERM_MEMORY, ScanReport, ServerFlavor, ServerInfo};

use super::app::views::{Ctx, Owner};
use super::app::{App, Phase, Screen, Session, View};
use super::keymap;

mod analysis;
mod connections;
mod delta;
mod diff;
pub mod help;
mod hot;
mod keys;
mod patterns;
mod pubsub;
mod search;
mod server;
mod streams;
pub mod style;
mod ttl;
pub mod widgets;

use widgets::{bar, panel, rows, trunc, width};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Rows inside a detail panel.
pub const DETAIL_ROWS: u16 = 8;

pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

/// Header rows: three, and a fourth for the panel names on a view with panels.
fn header_rows(view: View) -> u16 {
    if view.panels().is_empty() { 3 } else { 4 }
}

/// The header, the view's body and the footer.
fn screen(area: Rect, view: View) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(header_rows(view)),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// List rows a view without panels shows on a screen of `area`: what PgUp and PgDn
/// move by. Every list view has the same layout: the main panel above the detail
/// panel, its border, one header row, then the rows. On a view with panels the tab
/// bar's second row takes one more, which `App` subtracts.
pub fn list_rows(area: Rect) -> usize {
    let [_, body, _] = screen(area, View::Connections);
    let (main, _) = split_body(body);
    usize::from(Block::bordered().inner(main).height.saturating_sub(1))
}

pub fn draw(frame: &mut Frame, app: &App) {
    let [head, body, foot] = screen(frame.area(), app.view);
    header(frame, head, app);
    let screen = app.screen();
    match screen {
        Screen::Connections => connections::draw(frame, body, app),
        Screen::Patterns => patterns::draw(frame, body, app),
        Screen::Ttl => ttl::draw(frame, body, app),
        Screen::Diff => diff::draw(frame, body, app),
        Screen::HotKeys => hot::draw(frame, body, app),
        _ => view_state(frame, body, app, screen),
    }
    footer(frame, foot, app);
    if app.help {
        help::draw(frame, frame.area(), screen);
    }
}

/// A screen a view state owns: its draw function, or why there is nothing to draw (no
/// session, the view's permission probe failed, the scan has not finished).
fn view_state(frame: &mut Frame, area: Rect, app: &App, screen: Screen) {
    let title = screen.name();
    let Some(ctx) = app.ctx() else {
        let text = " Not connected. Open 0 Connections, pick a connection and press enter.";
        notice_panels(
            frame,
            area,
            title,
            "",
            vec![Line::from(Span::styled(text, style::dim()))],
        );
        return;
    };
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
        Owner::Keys => keys::draw(frame, area, &v.keys, &ctx),
        Owner::Analysis => {
            if report_or_wait(frame, area, app, title).is_some() {
                analysis::draw(frame, area, &v.analysis, &ctx);
            }
        }
        Owner::Server => server::draw(frame, area, &v.server, &ctx),
        Owner::Streams => streams::draw(frame, area, &v.streams, &ctx),
        Owner::PubSub => pubsub::draw(frame, area, &v.pubsub, &ctx),
        Owner::Search => search::draw(frame, area, &v.search, &ctx),
    }
}

/// What a stub view draws until wave B builds it: its title and "Not built yet".
pub fn not_built(frame: &mut Frame, area: Rect, title: &str, ctx: &Ctx<'_>) {
    let lines = vec![Line::from(Span::styled(
        format!(" Not built yet: {} comes in phase 1.", ctx.screen.title()),
        style::dim(),
    ))];
    notice_panels(frame, area, title, "", lines);
}

/// Marks results kept from the previous run while a rescan runs, or after it failed
/// before any results of its own came.
pub fn previous_marker(app: &App) -> Option<&'static str> {
    let s = app.session.as_ref()?;
    s.previous?;
    Some(match s.phase {
        Phase::Failed(_) => "rescan failed · previous results",
        _ => "rescanning · previous results",
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

/// The main table above, the 8-row detail panel below.
pub fn split_body(area: Rect) -> (Rect, Rect) {
    let [main, detail] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(DETAIL_ROWS + 2)]).areas(area);
    (main, detail)
}

/// The session and its finished report. Without one, draws a message (no connection
/// yet, connecting, scanning, or failed) into the `title` panel and returns `None`.
pub fn report_or_wait<'a>(
    frame: &mut Frame,
    area: Rect,
    app: &'a App,
    title: &str,
) -> Option<(&'a Session, &'a ScanReport)> {
    let (main, detail) = split_body(area);
    let Some(session) = &app.session else {
        let inner = panel(frame, main, title, "");
        let text = Span::styled(
            " No scan yet. Open 0 Connections, pick a connection and press enter.",
            style::dim(),
        );
        rows(frame, inner, vec![Line::default(), Line::from(text)], None);
        panel(frame, detail, "Details", "");
        return None;
    };
    if let Some(report) = &session.report {
        return Some((session, report));
    }
    waiting(frame, main, detail, app, session, title);
    None
}

fn waiting(frame: &mut Frame, main: Rect, detail: Rect, app: &App, s: &Session, title: &str) {
    let spin = spinner(app.tick);
    let p = &s.profile;
    let inner = panel(frame, main, title, &p.name);
    let mut lines = vec![Line::default()];
    match &s.phase {
        Phase::Failed(e) => {
            lines.push(Line::from(Span::styled(format!(" ✗ {e}"), style::warn())));
            lines.push(Line::from(Span::styled(
                " Press 0 to go back to Connections and try again.",
                style::dim(),
            )));
        }
        Phase::Connecting => {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spin, style::amber()),
                Span::styled(
                    format!(
                        " Connecting to {} ({}) and checking permissions…",
                        p.name, p.endpoint
                    ),
                    style::dim(),
                ),
            ]));
            if let Some(hint) = app.keychain_hint() {
                lines.push(Line::from(Span::styled(format!(" {hint}"), style::amber())));
            }
            provider_notice(&mut lines, p);
        }
        Phase::Scanning | Phase::Done => {
            let pr = s.progress.unwrap_or_default();
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(spin, style::amber()),
                Span::styled(" Sampled ", style::dim()),
                Span::styled(
                    format!(
                        "{} / {}",
                        format::grouped(pr.sampled),
                        format::grouped(pr.target)
                    ),
                    style::fgb(),
                ),
                Span::styled(" keys", style::dim()),
            ]));
            let ratio = if pr.target == 0 {
                0.0
            } else {
                pr.sampled as f64 / pr.target as f64
            };
            let w = usize::from(inner.width).saturating_sub(4).min(40);
            let (fill, track) = bar(ratio, w);
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(fill, style::amber()),
                Span::styled(track, style::track()),
            ]));
            provider_notice(&mut lines, p);
        }
    }
    rows(frame, inner, lines, None);
    panel(frame, detail, "Details", "");
}

/// A blank line and the profile's provider notice (see [`provider::notice`]), if any.
fn provider_notice(lines: &mut Vec<Line<'static>>, p: &crate::model::Profile) {
    if let Some(n) = provider::notice(p.provider) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(format!(" ▲ {n}"), style::amber())));
    }
}

/// A panel holding a few lines of explanation, with an empty detail panel below.
pub fn notice_panels(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    right: &str,
    lines: Vec<Line<'static>>,
) {
    let (main, detail) = split_body(area);
    let inner = panel(frame, main, title, right);
    let mut all = vec![Line::default()];
    all.extend(lines);
    rows(frame, inner, all, None);
    panel(frame, detail, "Details", "");
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

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let spin = spinner(app.tick);
    let brand = Span::styled(
        format!(" redis-lens {} ", env!("CARGO_PKG_VERSION")),
        style::brand(),
    );
    let (l0, l1) = match &app.session {
        None => (
            Line::from(vec![brand, Span::styled("  not connected", style::dim())]),
            Line::from(Span::styled(
                " Pick a connection and press enter to scan it.",
                style::dim(),
            )),
        ),
        Some(s) => {
            let p = &s.profile;
            let mut left = vec![
                brand,
                Span::raw("  "),
                Span::styled(format!(" {} ", p.env.label()), style::env_badge(p.env)),
                Span::styled(format!("  {}", p.name), style::fgb()),
                Span::styled(
                    format!(
                        "  {}",
                        trunc(&p.endpoint.to_string(), if w >= 100 { 30 } else { 18 })
                    ),
                    style::dim(),
                ),
            ];
            match &s.target {
                Some(ScanTarget::Replica(ep)) => left.push(Span::styled(
                    format!("  replica {}", trunc(&ep.to_string(), 24)),
                    style::dim(),
                )),
                Some(ScanTarget::PrimaryFallback(_)) => left.push(Span::styled(
                    "  primary (replica unreachable)",
                    style::amber(),
                )),
                _ => {}
            }
            left.push(Span::raw("  "));
            left.push(write_state(p));
            let right = match &s.phase {
                Phase::Connecting => {
                    vec![Span::styled(format!("{spin} connecting…"), style::amber())]
                }
                Phase::Scanning => vec![
                    Span::styled(spin, style::amber()),
                    Span::styled(" scanning", style::dim()),
                ],
                Phase::Done => vec![
                    Span::styled("● ", style::ok()),
                    Span::styled(
                        format!(
                            "scan complete {} UTC",
                            utc_time(s.report.as_ref().map_or(0, |r| r.taken_at_unix))
                        ),
                        style::dim(),
                    ),
                ],
                Phase::Failed(_) => vec![Span::styled("✗ failed", style::warn())],
            };
            (justify(left, right, w), server_line(app, s, w))
        }
    };
    let mut lines = vec![l0, l1, tabs(app, w)];
    if !app.view.panels().is_empty() {
        lines.push(panel_tabs(app, w));
    }
    rows(frame, area, lines, None);
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

fn server_line(app: &App, s: &Session, w: usize) -> Line<'static> {
    let spin = spinner(app.tick);
    // A rescan keeps the old report in the body, where the hint would otherwise go.
    if s.report.is_some()
        && let Some(hint) = app.keychain_hint()
    {
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(spin, style::amber()),
            Span::styled(format!(" {hint}"), style::amber()),
        ]);
    }
    let Some(srv) = &s.server else {
        return Line::from(match &s.phase {
            Phase::Failed(e) => vec![Span::styled(format!(" ✗ {e}"), style::warn())],
            _ => match app.keychain_hint() {
                Some(hint) => vec![
                    Span::raw(" "),
                    Span::styled(spin, style::amber()),
                    Span::styled(format!(" {hint}"), style::amber()),
                ],
                None => vec![
                    Span::raw(" "),
                    Span::styled(spin, style::amber()),
                    Span::styled(
                        format!(" Connecting to {}…", s.profile.endpoint),
                        style::dim(),
                    ),
                ],
            },
        });
    };
    let mut spans = vec![
        Span::styled(format!(" {}", server_label(srv)), style::fgb()),
        Span::styled("   MEM ", style::dim()),
    ];
    if srv.maxmemory > 0 {
        let ratio = srv.used_memory as f64 / srv.maxmemory as f64;
        let (fill, track) = bar(ratio, if w >= 130 { 20 } else { 10 });
        spans.extend([
            Span::styled(fill, style::amber()),
            Span::styled(track, style::track()),
            Span::styled(format!(" {}", format::bytes(srv.used_memory)), style::fgb()),
            Span::styled(
                format!(
                    " / {}  {}",
                    format::bytes(srv.maxmemory),
                    format::percent(ratio)
                ),
                style::dim(),
            ),
        ]);
    } else {
        spans.extend([
            Span::styled(format::bytes(srv.used_memory), style::fgb()),
            Span::styled(" · no limit", style::dim()),
        ]);
    }
    spans.extend([
        Span::styled("   KEYS ", style::dim()),
        Span::styled(format::count(srv.total_keys), style::fgb()),
    ]);
    match (&s.phase, &s.report) {
        (Phase::Scanning | Phase::Connecting, _) => {
            let p = s.progress.unwrap_or_default();
            spans.extend([
                Span::raw("   "),
                Span::styled(spin, style::amber()),
                Span::styled(" Sampled ", style::dim()),
                Span::styled(
                    format!(
                        "{} / {}",
                        format::grouped(p.sampled),
                        format::grouped(p.target)
                    ),
                    style::fgb(),
                ),
            ]);
        }
        (_, Some(r)) => {
            let ratio = if srv.total_keys == 0 {
                1.0
            } else {
                r.sampled_keys as f64 / srv.total_keys as f64
            };
            spans.extend([
                Span::styled("   SAMPLED ", style::dim()),
                Span::styled(format::count(r.sampled_keys), style::fgb()),
                Span::styled(format!(" ({})", format::percent(ratio)), style::dim()),
            ]);
        }
        _ => {}
    }
    if sizes_unavailable(app, s) {
        spans.push(Span::styled(
            "  ▲ sizes unavailable: MEMORY USAGE denied",
            style::warn(),
        ));
    }
    Line::from(spans)
}

/// Below this width the tab bar names only the current view; the others show their
/// digit.
pub const TAB_NAMES_FROM: usize = 100;

/// The tab bar: every view's digit and name, the current one highlighted.
fn tabs(app: &App, w: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for v in View::ALL {
        let text = if v == app.view || w >= TAB_NAMES_FROM {
            format!(" {} {} ", v.digit(), v.title())
        } else {
            format!(" {} ", v.digit())
        };
        let s = if v == app.view {
            style::tab_on()
        } else {
            style::tab()
        };
        spans.push(Span::styled(text, s));
        spans.push(Span::raw(" "));
    }
    let p = &app.patterns;
    if app.screen() == Screen::Patterns && !p.filter.is_empty() && !p.filtering {
        spans.extend([
            Span::styled("   filter: ", style::dim()),
            Span::styled(p.filter.clone(), style::amber()),
            Span::styled("  (esc to clear)", style::dim()),
        ]);
    }
    Line::from(spans)
}

/// The tab bar's second row on a view with panels: the panel names, the current one
/// highlighted, and the `tab` hint on the right.
fn panel_tabs(app: &App, w: usize) -> Line<'static> {
    let current = app.screen();
    let mut left = vec![Span::raw("    ")];
    for (i, p) in app.view.panels().iter().enumerate() {
        if i > 0 {
            left.push(Span::styled("  ·  ", style::track()));
        }
        let s = if *p == current {
            style::title()
        } else {
            style::dim()
        };
        left.push(Span::styled(p.name(), s));
    }
    let right = vec![
        Span::styled("tab", style::key()),
        Span::styled(" next panel", style::dim()),
    ];
    justify(left, right, w)
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

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let w = usize::from(area.width);
    let line = if let Some(add) = &app.add {
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
        let screen = app.screen();
        let ready = if screen.is_live_view() {
            app.session.is_some()
        } else {
            app.report().is_some()
        };
        let keys = keymap::footer(screen, ready);
        let mut spans = vec![Span::raw(" ")];
        for (k, l) in keys {
            spans.push(Span::styled(k, style::key()));
            spans.push(Span::styled(format!(" {l}   "), style::dim()));
        }
        Line::from(spans)
    };
    rows(frame, area, vec![line], None);
}
