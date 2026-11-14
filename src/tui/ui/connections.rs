//! Connections, the home screen: saved profiles and Docker containers in one list, what
//! the selected one means in words below it, and the add-connection flow. One frame: the
//! list on top, its details under it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use secrecy::ExposeSecret;

use crate::conn::docker::DiscoveredAuth;
use crate::conn::parse::{AzureKind, ConnFormat, ParsedConnection, mask};
use crate::conn::{ScanTarget, provider};
use crate::format;
use crate::model::{
    Auth, Env, PERM_LABELS, PERM_MEMORY, PERM_MEMORY_STATS, PermCheck, PermStatus,
    PermissionReport, Profile,
};
use crate::tui::app::{AddState, App, ConnEntry, ConnSel, ConnStatus, DockerState, Source};

use super::analysis::{Col, Tier, head_row_at, paragraph, pick_flex};
use super::widgets::{
    Section, Title, fit, pad_l, pad_r, pad_r_marquee, rows, scroll_offset, sections, split_cols,
    trunc, width,
};
use super::{cluster_degraded, server_label, spinner, style};

const ENV: usize = 1;
const ADDRESS: usize = 2;
const ACCESS: usize = 3;
const STATUS: usize = 4;
const SIGN_IN: usize = 5;
const MEMORY: usize = 6;

/// The list's columns: five by default, the sign-in and the memory at wide widths and
/// with `v`.
fn columns(wide: bool) -> [Col; 7] {
    [
        // The marker `›` and a space come first in the cell, so the heading is indented.
        Col::new("  NAME", if wide { 24 } else { 15 }, Tier::Base),
        Col::new("ENV", 7, Tier::Base),
        // Takes what the others leave, at least this much.
        Col::new("ADDRESS", 14, Tier::Base),
        Col::new("ACCESS", 10, Tier::Base),
        Col::new("STATUS", 13, Tier::Base),
        Col::new("SIGN-IN", 9, Tier::Wide),
        Col::new("MEMORY", 7, Tier::Wide),
    ]
}

/// The list shows the Wide columns from this many cells.
const WIDE_AT: usize = 100;
/// Rows the details keep at least, when the list is long.
const DETAIL_ROWS: u16 = 9;
/// The width of a detail row's label, and a space.
const LABEL: usize = 11;

/// The ACCESS column: `read-only` (amber) or `writes on` (red).
fn access_cell(p: &Profile) -> (&'static str, ratatui::style::Style) {
    if p.is_read_only() {
        ("read-only", style::amber())
    } else {
        ("writes on", style::warn())
    }
}

/// The detail's Access row in words: what the setting is, why, and which key changes
/// it. With writes on and an `ACL DRYRUN` answer in `perms`, it counts the writes the
/// server refuses this user.
fn access_sentence(p: &Profile, perms: Option<&PermissionReport>) -> String {
    let refused = perms.map_or(0, |r| {
        r.checks
            .iter()
            .filter(|c| {
                c.label != crate::conn::PERM_ACL_DRYRUN
                    && crate::conn::is_write_probe(c.label)
                    && matches!(c.status, PermStatus::Denied(_))
            })
            .count()
    });
    match (p.read_only, p.env) {
        (None, Env::Prod) => "read-only, the default for production. w turns writes on.".into(),
        (Some(true), _) => "read-only (read_only = true). w turns writes on.".into(),
        (read_only, _) => {
            let why = if refused > 0 {
                format!("{refused} refused by the server")
            } else if read_only.is_none() {
                "the default outside production".to_string()
            } else {
                "read_only = false".to_string()
            };
            format!("writes on ({why}). w turns them off.")
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let n_saved = app.saved_shown().count();
    let n_docker = app.docker_shown().count();
    let filtered = !app.conn_filter.is_empty();
    let first_run = app.add.is_none()
        && !filtered
        && app.saved().next().is_none()
        && app.docker_entries().next().is_none();
    if first_run {
        first_run_panel(frame, area, app);
        return;
    }
    let right = if filtered {
        format!(
            "{n_saved} of {} saved · {n_docker} of {} in Docker",
            app.saved().count(),
            app.docker_entries().count()
        )
    } else {
        format!("{n_saved} saved · {n_docker} in Docker")
    };
    let iw = usize::from(area.width.saturating_sub(2));
    let (lines, sel) = list_lines(app, iw);
    let usable = area.height.saturating_sub(3);
    let list_rows = u16::try_from(lines.len() + 1)
        .unwrap_or(u16::MAX)
        .min(usable.saturating_sub(DETAIL_ROWS).max(4));

    let shown = match (app.add.as_ref(), app.selected_entry()) {
        (None, Some(e)) if app.conn_sel != ConnSel::Add => Some(e),
        _ => None,
    };
    let (dt, dr) = match shown {
        Some(e) => detail_title(e),
        None => add_title(app.add.as_ref()),
    };
    let inner = sections(
        frame,
        area,
        &[
            Section {
                title: Title::new("Connections", &right),
                rows: Some(list_rows),
            },
            Section {
                title: Title::new(&dt, &dr),
                rows: None,
            },
        ],
    );
    list(frame, inner[0], app, lines, sel);
    match shown {
        Some(e) => entry_detail(frame, inner[1], app, e),
        None => add_panel(frame, inner[1], app, app.add.as_ref()),
    }
}

/// First run: the list is empty, so say how to add a connection and that Docker
/// containers appear by themselves.
fn first_run_panel(frame: &mut Frame, area: Rect, app: &App) {
    let inner = sections(
        frame,
        area,
        &[Section {
            title: Title::new("Connections", ""),
            rows: None,
        }],
    )[0];
    let w = usize::from(inner.width);
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(" No saved connections yet.", style::fgb())),
        Line::default(),
    ];
    lines.extend(paragraph(
        vec![Span::styled(" a  ", style::amber())],
        "    ",
        "Add one: paste a redis:// or rediss:// URL, or a StackExchange.Redis string. The password goes to the OS keychain, never to a file.",
        style::fg(),
        w,
    ));
    lines.push(Line::default());
    lines.extend(paragraph(
        vec![Span::raw(" ")],
        " ",
        "Redis and Valkey containers running in Docker show up here by themselves.",
        style::dim(),
        w,
    ));
    lines.push(Line::from(Span::styled(
        match app.docker {
            DockerState::Searching => " DOCKER · searching…",
            DockerState::Unavailable => " Docker: not running",
            DockerState::Found => " DOCKER · no Redis or Valkey containers",
        },
        style::dim(),
    )));
    rows(frame, inner, lines, None);
}

fn status(e: &ConnEntry) -> (&'static str, ratatui::style::Style) {
    match e.status {
        ConnStatus::NotOpen => ("○ not open", style::dim()),
        ConnStatus::Connecting => ("● connecting…", style::amber()),
        ConnStatus::Connected => ("● connected", style::ok()),
        ConnStatus::Limited => ("▲ limited", style::amber()),
        ConnStatus::Failed => ("✗ failed", style::warn()),
    }
}

/// The sign-in method in a word, for the SIGN-IN column.
fn auth_label(e: &ConnEntry) -> &'static str {
    match (&e.source, e.profile.auth) {
        (Source::Docker { auth, .. }, _) => auth.label(),
        (_, Auth::None) => "none",
        (_, Auth::Keychain) => "keychain",
        (_, Auth::EntraId) => "Entra ID",
        (_, Auth::Command) => "command",
    }
}

fn entry_line(
    app: &App,
    e: &ConnEntry,
    cols: &[Col; 7],
    keep: &[usize],
    address: usize,
) -> Line<'static> {
    let active = app.session.as_ref().is_some_and(|s| s.id == e.id);
    let (st, st_style) = status(e);
    let selected = app.conn_sel == ConnSel::Entry(e.id);
    let ticks = app.sel_ticks();
    let cell = |s: &str, w: usize| {
        if selected {
            pad_r_marquee(s, w, ticks)
        } else {
            pad_r(s, w)
        }
    };
    let mut spans = Vec::new();
    for &i in keep {
        spans.push(Span::raw(" "));
        let w = cols[i].w;
        match i {
            0 => {
                spans.push(Span::styled(
                    if active { "› " } else { "  " },
                    style::amber(),
                ));
                spans.push(Span::styled(
                    cell(&e.profile.name, w.saturating_sub(2)),
                    if active { style::fgb() } else { style::fg() },
                ));
            }
            ENV => {
                let env = e.profile.env.label();
                spans.push(Span::styled(
                    format!(" {env} "),
                    style::env_badge(e.profile.env),
                ));
                spans.push(Span::raw(" ".repeat(w.saturating_sub(env.len() + 2))));
            }
            ADDRESS => spans.push(Span::styled(
                cell(&e.endpoint().to_string(), address.saturating_sub(1)),
                style::dim(),
            )),
            ACCESS => {
                let (text, st) = access_cell(&e.profile);
                spans.push(Span::styled(pad_r(text, w), st));
            }
            STATUS => spans.push(Span::styled(pad_r(st, w), st_style)),
            SIGN_IN => spans.push(Span::styled(pad_r(auth_label(e), w), style::fg())),
            MEMORY => {
                let mem = e
                    .server
                    .as_ref()
                    .map_or("—".to_string(), |s| format::bytes(s.used_memory));
                spans.push(Span::styled(pad_l(&mem, w), style::fg()));
            }
            _ => {}
        }
    }
    Line::from(spans)
}

/// The header row, the group lines, the connection rows and the add row of a list
/// `iw` cells wide, and the index of the selected one among the body rows. The header
/// is the first line.
fn list_lines(app: &App, iw: usize) -> (Vec<(Line<'static>, Option<ConnSel>)>, usize) {
    let cols = columns(iw >= WIDE_AT);
    let (keep, address) = pick_flex(&cols, ADDRESS, iw, WIDE_AT, app.more_here());
    let n_saved = app.saved_shown().count();
    let n_docker = app.docker_shown().count();
    let filtered = !app.conn_filter.is_empty();

    let mut lines: Vec<(Line<'static>, Option<ConnSel>)> = Vec::new();
    lines.push((head_row_at(&cols, &keep, ADDRESS, address, &[MEMORY]), None));
    let group = |t: String| (Line::from(Span::styled(t, style::dim())), None);
    lines.push(group(" SAVED".into()));
    if n_saved == 0 && filtered && app.saved().next().is_some() {
        lines.push(group(
            "   No saved connection matches the filter. Press esc to clear it.".into(),
        ));
    } else if n_saved == 0 {
        lines.push(group(
            "   No saved connections yet. Press a to add one.".into(),
        ));
    }
    for e in app.saved_shown() {
        lines.push((
            entry_line(app, e, &cols, &keep, address),
            Some(ConnSel::Entry(e.id)),
        ));
    }
    lines.push(group(match app.docker {
        DockerState::Searching => " DOCKER · searching…".into(),
        DockerState::Unavailable => " Docker: not running".into(),
        DockerState::Found if n_docker == 0 => " DOCKER · no Redis or Valkey containers".into(),
        DockerState::Found => format!(
            " DOCKER · {n_docker} Redis-compatible container{}",
            if n_docker == 1 { "" } else { "s" }
        ),
    }));
    for e in app.docker_shown() {
        lines.push((
            entry_line(app, e, &cols, &keep, address),
            Some(ConnSel::Entry(e.id)),
        ));
    }
    lines.push((
        Line::from(vec![
            Span::styled(" + Add a connection", style::amber()),
            Span::styled("  press a, then paste a redis:// URL", style::dim()),
        ]),
        Some(ConnSel::Add),
    ));
    let sel = lines
        .iter()
        .position(|(_, s)| *s == Some(app.conn_sel))
        .unwrap_or(1);
    (lines, sel)
}

/// Draws the list into `inner`: the header stays, the rest scrolls to keep the
/// selection on screen.
fn list(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    mut lines: Vec<(Line<'static>, Option<ConnSel>)>,
    sel: usize,
) {
    let _ = app;
    let head = lines.remove(0).0;
    let sel = sel.saturating_sub(1);
    rows(frame, inner, vec![head], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let off = scroll_offset(sel, usize::from(body.height));
    let visible: Vec<Line<'static>> = lines.into_iter().skip(off).map(|(l, _)| l).collect();
    rows(frame, body, visible, Some(sel - off));
}

fn kv(label: &str, value: String, value_style: ratatui::style::Style) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!(" {}", pad_r(label, 12)), style::dim()),
        Span::styled(value, value_style),
    ]
}

/// The detail frame's title: the connection and what it is.
fn detail_title(e: &ConnEntry) -> (String, String) {
    let p = &e.profile;
    let mut right = format!(
        "{} · {}",
        p.env.label(),
        if e.is_docker() {
            "found in Docker"
        } else {
            "saved"
        }
    );
    if let Some(pv) = p.provider {
        right.push_str(&format!(" · {}", provider::label(pv)));
    }
    (p.name.clone(), right)
}

/// How the connection signs in, in a sentence.
fn sign_in_sentence(docker: Option<DiscoveredAuth>, auth: Auth) -> &'static str {
    match (docker, auth) {
        (Some(DiscoveredAuth::Password), _) => {
            "password for the default user, from the container, kept in memory only"
        }
        (Some(DiscoveredAuth::None), _) | (None, Auth::None) => "none",
        (Some(DiscoveredAuth::Required), _) => {
            "a password is required and was not found: save it as a profile with credentials"
        }
        (Some(DiscoveredAuth::Unknown), _) => "could not tell",
        (None, Auth::Keychain) => "password or ACL user, kept in the OS keychain",
        (None, Auth::EntraId) => "Entra ID, from your developer tools (az login)",
        (None, Auth::Command) => "a token from a command; the username is kept in the OS keychain",
    }
}

/// What the permission check came to, in words and a glyph, for the Checks row.
fn checks_sentence(app: &App, e: &ConnEntry) -> (String, ratatui::style::Style) {
    if let Some(err) = &e.error {
        return (format!("✗ {err}"), style::warn());
    }
    match e.status {
        ConnStatus::Connecting => {
            let hint = app
                .keychain_hint()
                .filter(|_| app.session.as_ref().is_some_and(|s| s.id == e.id));
            let text = match hint {
                Some(h) => format!("{} {h}", spinner(app.tick)),
                None => format!(
                    "{} Connecting and checking what this user may do…",
                    spinner(app.tick)
                ),
            };
            return (text, style::amber());
        }
        ConnStatus::Limited => {
            let memory = e.perms.as_ref().is_some_and(|p| !p.allowed(PERM_MEMORY));
            // MEMORY STATS is the only denied check: only the dataset figures go.
            let only_stats = e.perms.as_ref().is_some_and(|p| {
                p.checks
                    .iter()
                    .filter(|c| {
                        PERM_LABELS.contains(&c.label) && matches!(c.status, PermStatus::Denied(_))
                    })
                    .all(|c| c.label == PERM_MEMORY_STATS)
            });
            let text = if memory {
                "▲ Limited: the server refused MEMORY USAGE, so there are no sizes. Everything else redis-lens needs is allowed. v lists each check."
            } else if only_stats {
                "▲ Limited: the server refused MEMORY STATS, so the share of memory the sample explains is missing. Everything else redis-lens needs is allowed. v lists each check."
            } else {
                "▲ Limited: some checks were refused, so the panels that need them are off. v lists each check."
            };
            return (text.to_string(), style::amber());
        }
        ConnStatus::Connected => {
            // A view probe that failed turns its view off but leaves the analysis whole.
            let views_off = e.perms.as_ref().is_some_and(|p| {
                p.checks
                    .iter()
                    .any(|c| !PERM_LABELS.contains(&c.label) && c.status != PermStatus::Allowed)
            });
            let text = if views_off {
                "✓ The analysis has every permission it needs. Panels whose check failed are off. v lists each check."
            } else {
                "✓ Every check passed. v lists each check."
            };
            return (text.to_string(), style::ok());
        }
        _ => {}
    }
    // Before connecting, say what the provider takes away; once connected the
    // permission check is what counts.
    if let Some(n) = provider::notice(e.profile.provider) {
        return (format!("▲ {n}"), style::amber());
    }
    let text = if e.just_saved {
        "Saved. They run when you connect: redis-lens asks what this user may do before it reads anything."
    } else {
        "run when you connect: redis-lens asks what this user may do before it reads anything."
    };
    (text.to_string(), style::dim())
}

fn entry_detail(frame: &mut Frame, inner: Rect, app: &App, e: &ConnEntry) {
    let p = &e.profile;
    let docker = match &e.source {
        Source::Docker {
            container,
            image,
            auth,
            ..
        } => Some((container, image, *auth)),
        Source::Saved => None,
    };
    let w = usize::from(inner.width);
    let lead = |label: &str| {
        vec![Span::styled(
            format!(" {}", pad_r(label, LABEL - 1)),
            style::dim(),
        )]
    };
    let rest = " ".repeat(LABEL);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut put = |label: &str, text: &str, st: ratatui::style::Style| {
        lines.extend(paragraph(lead(label), &rest, text, st, w));
    };

    // Address: where, TLS, and once connected which server it is. A Sentinel profile
    // shows the primary the Sentinels named, once they have.
    let mut address = e.endpoint().to_string();
    if p.db != 0 {
        address.push_str(&format!(" · db {}", p.db));
    }
    address.push_str(if p.endpoint.tls {
        " · TLS on"
    } else {
        " · TLS off"
    });
    let mut address_style = style::fg();
    match &e.target {
        None | Some(ScanTarget::Primary | ScanTarget::Cluster(_)) => {}
        Some(t) => address.push_str(&format!(" · reading {t}")),
    }
    match (&e.server, &e.target) {
        (Some(s), Some(ScanTarget::Cluster(summary))) => {
            // The finished report of this entry, when it is the one on screen.
            let nodes = app
                .session
                .as_ref()
                .filter(|s| s.id == e.id)
                .and_then(|s| s.report.as_ref())
                .and_then(|r| r.nodes.as_deref());
            address.push_str(&format!(" · {} · {summary}", server_label(s)));
            if cluster_degraded(summary, nodes) {
                address_style = style::amber();
            }
        }
        (Some(s), _) => address.push_str(&format!(" · {}, {}", server_label(s), mode(s.mode))),
        (None, _) => {}
    }
    put("Address", &address, address_style);
    if let Some((container, image, _)) = docker {
        put(
            "Docker",
            &format!("container {container}, image {image}"),
            style::fg(),
        );
    }
    put(
        "Sign-in",
        sign_in_sentence(docker.map(|(_, _, a)| a), p.auth),
        style::fg(),
    );
    put(
        "Access",
        &access_sentence(p, e.perms.as_ref()),
        if p.is_read_only() {
            style::amber()
        } else {
            style::warn()
        },
    );
    if !p.tags.is_empty() {
        put("Tags", &p.tags.join(", "), style::fg());
    }
    if let Some(s) = &p.sentinel {
        put(
            "Sentinel",
            &sentinel_row(s, e.sentinel.as_ref()),
            style::fg(),
        );
    }
    if p.tags.is_empty() && p.sentinel.is_none() {
        put("Values", "shown on screen, never saved", style::fg());
    }
    let (checks, checks_style) = checks_sentence(app, e);
    put("Checks", &checks, checks_style);

    // `v`: every check that ran, as today.
    if app.more_here()
        && let Some(r) = &e.perms
    {
        let room = usize::from(inner.height).saturating_sub(lines.len());
        for spans in perm_lines(r, w.saturating_sub(LABEL + 2), room) {
            let mut row = vec![Span::raw(" ".repeat(LABEL + 1))];
            row.extend(spans);
            lines.push(Line::from(fit(row, w)));
        }
    }
    rows(frame, inner, lines, None);
}

/// The add flow's title and right title, by what has been typed.
fn add_title(add: Option<&AddState>) -> (String, String) {
    let right = match add.and_then(|a| a.parsed()) {
        Some(Ok(p)) => format_label(&p),
        Some(Err(_)) => "could not parse".to_string(),
        None => "redis:// · rediss:// · StackExchange.Redis · Azure".to_string(),
    };
    ("Add a connection".to_string(), right)
}

fn add_panel(frame: &mut Frame, inner: Rect, app: &App, add: Option<&AddState>) {
    let parsed = add.and_then(|a| a.parsed());
    match (add, parsed) {
        (Some(add), Some(Ok(p))) => parsed_panel(frame, inner, app, add, &p),
        (Some(add), Some(Err(e))) => {
            let err = add.error.clone().unwrap_or(e);
            let lines = vec![
                Line::default(),
                Line::from(Span::styled(format!(" ✗ {err}"), style::warn())),
                Line::from(Span::styled(
                    " Accepted: redis://user:pass@host:port, rediss://… for TLS, or host:port,password=…,ssl=True",
                    style::dim(),
                )),
            ];
            rows(frame, inner, lines, None);
        }
        (add, _) => {
            let ex = |s: &str| Line::from(Span::styled(format!("   {s}"), style::dim()));
            let mut lines = vec![
                Line::from(Span::styled(" PASTE A CONNECTION STRING", style::hd())),
                Line::from(Span::styled(
                    if add.is_some() {
                        " Paste or type one, then press enter."
                    } else {
                        " Press a (or enter), then paste or type one."
                    },
                    style::fg(),
                )),
                ex("redis://[user[:password]@]host[:port][/db]"),
                ex("rediss://…   for TLS"),
                ex("host:port,password=…,user=…,ssl=True,defaultDatabase=N"),
                Line::default(),
                Line::from(Span::styled(
                    " Passwords go to the OS keychain; the config file keeps only a reference.",
                    style::dim(),
                )),
            ];
            if let Some(err) = add.and_then(|a| a.error.clone()) {
                lines[5] = Line::from(Span::styled(format!(" ✗ {err}"), style::warn()));
            }
            rows(frame, inner, lines, None);
        }
    }
}

/// The checks in display order with their status: every scan check (`PERM_LABELS`,
/// `None` when it did not run), then the rest of the report (the view probes) in the
/// order they ran.
fn ordered_checks(r: &PermissionReport) -> Vec<(&'static str, Option<&PermStatus>)> {
    let mut out: Vec<(&'static str, Option<&PermStatus>)> = PERM_LABELS
        .iter()
        .map(|l| {
            (
                *l,
                r.checks.iter().find(|c| c.label == *l).map(|c| &c.status),
            )
        })
        .collect();
    out.extend(
        r.checks
            .iter()
            .filter(|c| !PERM_LABELS.contains(&c.label) && !crate::conn::is_write_probe(c.label))
            .map(|c: &PermCheck| (c.label, Some(&c.status))),
    );
    out
}

/// The permission list, at most `max` lines `w` wide. One check per line while they
/// fit (today's six scan checks do); when they don't, allowed checks next to each
/// other share a line (`✓ SCAN · INFO`), and a denied or skipped one keeps its own
/// line with its reason.
fn perm_lines(r: &PermissionReport, w: usize, max: usize) -> Vec<Vec<Span<'static>>> {
    let checks = ordered_checks(r);
    let one = |(label, status): (&'static str, Option<&PermStatus>)| {
        let (mark, mark_style, msg, msg_style) = match status {
            Some(PermStatus::Allowed) => ("✓ ", style::ok(), String::new(), style::dim()),
            // A view probe's NOPERM text names the user: say what it means instead.
            Some(PermStatus::Denied(_)) if !PERM_LABELS.contains(&label) => (
                "✗ ",
                style::warn(),
                "refused, view off".to_string(),
                style::warn(),
            ),
            Some(PermStatus::Denied(m)) => ("✗ ", style::warn(), m.clone(), style::warn()),
            Some(PermStatus::Skipped(m)) => ("– ", style::dim(), m.clone(), style::dim()),
            None => ("– ", style::dim(), String::new(), style::dim()),
        };
        vec![
            Span::styled(mark, mark_style),
            Span::styled(pad_r(label, 14), style::fg()),
            Span::styled(msg, msg_style),
        ]
    };
    if checks.len() <= max {
        return checks.into_iter().map(one).collect();
    }
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    // The allowed labels of the line being filled, and its width so far.
    let mut run: Vec<&str> = Vec::new();
    let flush = |run: &mut Vec<&str>, out: &mut Vec<Vec<Span<'static>>>| {
        if !run.is_empty() {
            out.push(vec![
                Span::styled("✓ ", style::ok()),
                Span::styled(run.join(" · "), style::fg()),
            ]);
            run.clear();
        }
    };
    for (label, status) in checks {
        if status == Some(&PermStatus::Allowed) {
            let joined = width(&run.join(" · ")) + 3 + width(label);
            if !run.is_empty() && 2 + joined > w {
                flush(&mut run, &mut out);
            }
            run.push(label);
        } else {
            flush(&mut run, &mut out);
            out.push(one((label, status)));
        }
    }
    flush(&mut run, &mut out);
    if out.len() > max {
        out.truncate(max);
        if let Some(last) = out.last_mut() {
            last.push(Span::styled(" …", style::dim()));
        }
    }
    out
}

/// The Sentinel row: the master name and the Sentinel that answered the last connect, or
/// before that the Sentinels it will ask. Never a credential.
fn sentinel_row(
    s: &crate::model::Sentinel,
    answer: Option<&crate::conn::sentinel::SentinelAnswer>,
) -> String {
    match answer {
        Some(a) => format!("{} · named by {}", a.master, a.sentinel),
        None => {
            let mut out = s.master.clone();
            if let Some(first) = s.nodes.first() {
                out.push_str(&format!(" · asks {first}"));
            }
            if s.nodes.len() > 1 {
                out.push_str(&format!(" (+{})", s.nodes.len() - 1));
            }
            out
        }
    }
}

fn mode(m: crate::model::ServerMode) -> &'static str {
    match m {
        crate::model::ServerMode::Standalone => "standalone",
        crate::model::ServerMode::Cluster => "cluster",
        crate::model::ServerMode::Sentinel => "sentinel",
    }
}

fn format_label(p: &ParsedConnection) -> String {
    let mut s = match p.format {
        ConnFormat::RedisUri if p.endpoint.tls => "Redis URI (TLS)",
        ConnFormat::RedisUri => "Redis URI",
        ConnFormat::StackExchange => "StackExchange.Redis",
    }
    .to_string();
    match p.azure {
        Some(AzureKind::CacheForRedis) => s.push_str(" · Azure Cache for Redis"),
        Some(AzureKind::ManagedRedis) => s.push_str(" · Azure Managed Redis"),
        None => {}
    }
    s
}

fn parsed_panel(frame: &mut Frame, inner: Rect, app: &App, add: &AddState, p: &ParsedConnection) {
    let fmt = format_label(p);
    let (lw, rw) = split_cols(usize::from(inner.width));
    let user = p.credentials.as_ref().and_then(|c| c.username.clone());
    let has_password = p
        .credentials
        .as_ref()
        .is_some_and(|c| !c.password.expose_secret().is_empty());
    let needs_password = p.credentials.is_some() && !has_password;
    let typed = add.password.as_ref().is_some_and(|pw| !pw.is_empty());
    let name = app.add_name(p);

    let password = if has_password || typed {
        ("••••••  → OS keychain".to_string(), style::amber())
    } else if needs_password {
        ("needed: press enter to type it".to_string(), style::amber())
    } else {
        ("none".to_string(), style::dim())
    };
    let auth = match (&p.credentials, &user) {
        (None, _) => "none".to_string(),
        (Some(_), Some(u)) => format!("ACL user {u}"),
        (Some(_), None) => "password (default user)".to_string(),
    };
    let env_style = if p.env_guess == Env::Prod {
        style::warn()
    } else {
        style::fg()
    };
    let lv = |label: &str, v: String| kv(label, trunc(&v, 46), style::fg());
    let left = [
        vec![Span::styled(" PARSED", style::hd())],
        lv("Format", fmt.clone()),
        lv("Host", p.endpoint.host.clone()),
        lv("Port", p.endpoint.port.to_string()),
        lv("TLS", if p.endpoint.tls { "on" } else { "off" }.into()),
        lv("User", user.clone().unwrap_or_else(|| "default".into())),
    ];
    let rv = |label: &str, v: String, s| {
        vec![
            Span::styled(pad_r(label, 13), style::dim()),
            Span::styled(v, s),
        ]
    };
    let right = [
        vec![Span::styled("CREDENTIALS", style::hd())],
        rv("Password", password.0, password.1),
        rv("Auth", auth, style::fg()),
        rv(
            "Environment",
            format!("{} (guessed)", p.env_guess.label()),
            env_style,
        ),
        rv("Name", name, style::fg()),
        rv("Database", p.db.to_string(), style::fg()),
    ];
    let mut lines: Vec<Line<'static>> = left
        .into_iter()
        .zip(right)
        .map(|(l, r)| {
            let mut spans = fit(l, lw);
            spans.extend(fit(r, rw));
            Line::from(spans)
        })
        .collect();

    let hint = if let Some(err) = &add.error {
        (format!("✗ {err}"), style::warn())
    } else if add.saving {
        ("Saving…".to_string(), style::dim())
    } else if add.password.is_some() {
        (
            format!(
                "▲ Type the password for {}, then press enter. It goes to the OS keychain.",
                user.as_deref().unwrap_or("the default user")
            ),
            style::amber(),
        )
    } else if needs_password {
        (
            "▲ The URL names a user but no password. Press enter to type it.".to_string(),
            style::amber(),
        )
    } else if p.azure.is_some() && p.credentials.is_none() {
        (
            "▲ Azure cache with no key. Entra ID sign-in comes in v0.2; add password=<access key> for now."
                .to_string(),
            style::amber(),
        )
    } else if p.azure.is_some() {
        (
            "▲ Azure access key found. It goes to the OS keychain, never the config file."
                .to_string(),
            style::amber(),
        )
    } else if p.env_guess == Env::Prod {
        (
            "▲ Looks like production, so this profile will be read-only.".to_string(),
            style::amber(),
        )
    } else if !p.endpoint.tls && p.env_guess != Env::Local {
        (
            "▲ TLS is off for a remote host. Use rediss:// or ssl=True if the server supports it."
                .to_string(),
            style::amber(),
        )
    } else {
        (
            "✓ Looks good. Press enter to save it.".to_string(),
            style::ok(),
        )
    };
    lines.push(Line::from(Span::styled(format!(" {}", hint.0), hint.1)));
    lines.push(Line::from(vec![
        Span::styled(" Input  ", style::dim()),
        Span::styled(mask(add.input.trim()), style::cmd()),
    ]));
    rows(frame, inner, lines, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_access_sentence_says_why_and_which_key_changes_it() {
        let mut p = crate::tui::fixtures::profiles().remove(2);
        p.read_only = None;
        let mut perms = crate::tui::fixtures::perms_with_probes();
        assert_eq!(
            access_sentence(&p, Some(&perms)),
            "writes on (the default outside production). w turns them off."
        );
        for (label, status) in [
            (crate::conn::PERM_ACL_DRYRUN, PermStatus::Allowed),
            ("write HSET", PermStatus::Denied("NOPERM".into())),
            ("write UNLINK", PermStatus::Denied("NOPERM".into())),
            ("write SET", PermStatus::Allowed),
        ] {
            perms.checks.push(PermCheck { label, status });
        }
        assert_eq!(
            access_sentence(&p, Some(&perms)),
            "writes on (2 refused by the server). w turns them off."
        );
        // Off says why it is off, whatever the server allows.
        p.read_only = Some(true);
        assert_eq!(
            access_sentence(&p, Some(&perms)),
            "read-only (read_only = true). w turns writes on."
        );
        p.read_only = None;
        p.env = Env::Prod;
        assert!(access_sentence(&p, None).starts_with("read-only, the default for production"));
        // The write probes stay out of the permission list.
        assert!(
            ordered_checks(&perms)
                .iter()
                .all(|(l, _)| !crate::conn::is_write_probe(l))
        );
    }

    #[test]
    fn five_columns_at_80_and_seven_when_wide_or_with_v() {
        let cols = columns(false);
        let (keep, address) = pick_flex(&cols, ADDRESS, 78, WIDE_AT, false);
        assert_eq!(keep, vec![0, ENV, ADDRESS, ACCESS, STATUS]);
        // Room for `prod-cache-01.internal:6380` and its gap.
        assert!(address >= 28, "{address}");
        let (keep, _) = pick_flex(&columns(true), ADDRESS, 118, WIDE_AT, false);
        assert_eq!(keep.len(), 7);
        // `v` at 80 adds the sign-in where it fits, never at the address's cost.
        let (keep, address) = pick_flex(&cols, ADDRESS, 78, WIDE_AT, true);
        assert!(keep.contains(&SIGN_IN), "{keep:?}");
        assert!(address >= cols[ADDRESS].w, "{keep:?} {address}");
    }

    fn screen(app: &App, w: u16, h: u16) -> String {
        crate::tui::app::views::testing::render(app, w, h)
            .backend()
            .to_string()
    }

    #[test]
    fn connections_at_80x24_has_five_columns_and_says_what_a_row_means() {
        let mut app = App::new(crate::tui::fixtures::profiles());
        app.apply(crate::tui::app::Msg::DockerFound(vec![
            crate::tui::fixtures::docker_instance(),
        ]));
        let s = screen(&app, 80, 24);
        for word in ["NAME", "ENV", "ADDRESS", "ACCESS", "STATUS"] {
            assert!(s.contains(word), "{word}: {s}");
        }
        // The plain words of the mockup: Address, Sign-in, Access; no Endpoint, Auth.
        for word in ["Address ", "Sign-in ", "Access "] {
            assert!(s.contains(word), "{word}: {s}");
        }
        for word in [
            "ENDPOINT",
            "Endpoint",
            "Credential",
            "WRITES",
            "SIGN-IN",
            "MEMORY",
        ] {
            assert!(!s.contains(word), "{word}: {s}");
        }
        insta::assert_snapshot!("connections_80x24", s);
    }

    #[test]
    fn v_on_connections_adds_the_sign_in_and_lists_each_check() {
        let mut app = App::new(crate::tui::fixtures::profiles());
        let before = screen(&app, 120, 36);
        assert!(before.contains("SIGN-IN"), "{before}");
        assert!(!screen(&app, 80, 24).contains("SIGN-IN"));
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('v'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let more = screen(&app, 80, 24);
        assert!(more.contains("SIGN-IN"), "{more}");
        insta::assert_snapshot!("connections_more_80x24", more);
    }

    #[test]
    fn the_first_run_explains_how_to_add_one() {
        let app = App::new(Vec::new());
        let s = screen(&app, 80, 24);
        assert!(s.contains("No saved connections yet."), "{s}");
        assert!(s.contains("paste a redis:// or rediss:// URL"), "{s}");
        assert!(s.contains("never to a file"), "{s}");
        assert!(s.contains("show up here by themselves"), "{s}");
        insta::assert_snapshot!("connections_first_run_80x24", s);
    }

    #[test]
    fn no_password_is_ever_drawn_by_the_list_or_its_details() {
        let app = App::new(crate::tui::fixtures::profiles());
        for (w, h) in [(80, 24), (120, 36)] {
            let s = screen(&app, w, h);
            assert!(!s.to_lowercase().contains("password="), "{s}");
        }
    }

    #[test]
    fn every_status_has_a_word_and_a_glyph() {
        for s in [
            ConnStatus::NotOpen,
            ConnStatus::Connecting,
            ConnStatus::Connected,
            ConnStatus::Limited,
            ConnStatus::Failed,
        ] {
            let app = App::new(crate::tui::fixtures::profiles());
            let mut e = app.saved().next().unwrap().clone();
            e.status = s;
            let (text, _) = status(&e);
            let (glyph, word) = text.split_once(' ').unwrap();
            assert!(["○", "●", "▲", "✗"].contains(&glyph), "{text}");
            assert!(word.len() > 3, "{text}");
        }
    }

    #[test]
    fn sign_in_never_names_a_secret() {
        for a in [Auth::None, Auth::Keychain, Auth::EntraId, Auth::Command] {
            let s = sign_in_sentence(None, a);
            assert!(!s.to_lowercase().contains("secret"), "{s}");
        }
    }
}
