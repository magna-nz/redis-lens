//! View 0: saved profiles, Docker containers, and the add-connection flow.

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

use super::widgets::{
    fit, pad_l, pad_r, pad_r_marquee, panel, rows, scroll_offset, split_cols, trunc, width,
};
use super::{cluster_degraded, server_label, spinner, split_body, style};

struct Cols {
    name: usize,
    env: usize,
    ep: usize,
    auth: usize,
    writes: usize,
    status: usize,
    mem: usize,
}

fn cols(inner: usize) -> Cols {
    let (name, env, auth, writes, status, mem) = if inner >= 100 {
        (22, 9, 14, 11, 15, 7)
    } else {
        (18, 8, 9, 10, 14, 7)
    };
    let ep = inner
        .saturating_sub(name + env + auth + writes + status + mem)
        .max(8);
    Cols {
        name,
        env,
        ep,
        auth,
        writes,
        status,
        mem,
    }
}

/// The WRITES column: `read-only` (amber) or `on` (red).
fn writes_cell(p: &Profile) -> (&'static str, ratatui::style::Style) {
    if p.is_read_only() {
        ("read-only", style::amber())
    } else {
        ("on", style::warn())
    }
}

/// The detail panel's Writes row: off or on, and why.
fn writes_row(p: &Profile) -> Vec<Span<'static>> {
    let (state, why) = match (p.read_only, p.env) {
        (None, Env::Prod) => ("off", "production default"),
        (None, _) => ("on", "default outside production"),
        (Some(true), _) => ("off", "read_only = true"),
        (Some(false), _) => ("on", "read_only = false"),
    };
    let s = if state == "off" {
        style::amber()
    } else {
        style::warn()
    };
    let mut spans = kv("Writes", state.to_string(), s);
    spans.push(Span::styled(format!(" ({why})"), style::dim()));
    spans
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
            .filter(|c| !PERM_LABELS.contains(&c.label))
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

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let (main, detail) = split_body(area);
    table(frame, main, app);
    match (app.add.as_ref(), app.selected_entry()) {
        (None, Some(e)) if app.conn_sel != ConnSel::Add => entry_detail(frame, detail, app, e),
        (add, _) => add_panel(frame, detail, app, add),
    }
}

fn status(e: &ConnEntry) -> (&'static str, ratatui::style::Style) {
    match e.status {
        ConnStatus::NotOpen => ("○ not open", style::dim()),
        ConnStatus::Connecting => ("● connecting…", style::amber()),
        ConnStatus::Connected => ("● connected", style::ok()),
        ConnStatus::Limited => ("● limited", style::amber()),
        ConnStatus::Failed => ("✗ failed", style::warn()),
    }
}

fn auth_label(e: &ConnEntry) -> &'static str {
    match (&e.source, e.profile.auth) {
        (Source::Docker { auth, .. }, _) => auth.label(),
        (_, Auth::None) => "none",
        (_, Auth::Keychain) => "keychain",
        (_, Auth::EntraId) => "Entra ID",
        (_, Auth::Command) => "command",
    }
}

fn entry_line(app: &App, e: &ConnEntry, c: &Cols) -> Line<'static> {
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
    let env = e.profile.env.label();
    let (writes, writes_style) = writes_cell(&e.profile);
    let mem = e
        .server
        .as_ref()
        .map_or("—".to_string(), |s| format::bytes(s.used_memory));
    Line::from(vec![
        Span::styled(if active { " › " } else { "   " }, style::amber()),
        Span::styled(
            cell(&e.profile.name, c.name - 3),
            if active { style::fgb() } else { style::fg() },
        ),
        Span::styled(format!(" {env} "), style::env_badge(e.profile.env)),
        Span::raw(" ".repeat(c.env.saturating_sub(env.len() + 2))),
        Span::styled(cell(&e.profile.endpoint.to_string(), c.ep), style::dim()),
        Span::styled(pad_r(auth_label(e), c.auth), style::fg()),
        Span::styled(pad_r(writes, c.writes), writes_style),
        Span::styled(pad_r(st, c.status), st_style),
        Span::styled(format!("{} ", pad_l(&mem, c.mem - 1)), style::fg()),
    ])
}

fn table(frame: &mut Frame, area: Rect, app: &App) {
    let n_saved = app.saved().count();
    let n_docker = app.docker_entries().count();
    let right = format!("{n_saved} saved · {n_docker} found in Docker");
    let inner = panel(frame, area, "Connections", &right);
    let c = cols(usize::from(inner.width));
    let head = Line::from(vec![
        Span::styled(pad_r("   NAME", c.name), style::hd()),
        Span::styled(pad_r("ENV", c.env), style::hd()),
        Span::styled(pad_r("ENDPOINT", c.ep), style::hd()),
        Span::styled(pad_r("AUTH", c.auth), style::hd()),
        Span::styled(pad_r("WRITES", c.writes), style::hd()),
        Span::styled(pad_r("STATUS", c.status), style::hd()),
        Span::styled(format!("{} ", pad_l("MEM", c.mem - 1)), style::hd()),
    ]);

    let mut lines: Vec<(Line<'static>, Option<ConnSel>)> = Vec::new();
    let group = |t: String| (Line::from(Span::styled(t, style::dim())), None);
    lines.push(group(" SAVED".into()));
    if n_saved == 0 {
        lines.push(group(
            "   No saved connections yet. Press a to add one.".into(),
        ));
    }
    for e in app.saved() {
        lines.push((entry_line(app, e, &c), Some(ConnSel::Entry(e.id))));
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
    for e in app.docker_entries() {
        lines.push((entry_line(app, e, &c), Some(ConnSel::Entry(e.id))));
    }
    lines.push((
        Line::from(vec![
            Span::styled(" + Add connection", style::amber()),
            Span::styled(
                "    paste a redis:// URI or a StackExchange.Redis string",
                style::dim(),
            ),
        ]),
        Some(ConnSel::Add),
    ));

    rows(frame, inner, vec![head], None);
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let sel = lines
        .iter()
        .position(|(_, s)| *s == Some(app.conn_sel))
        .unwrap_or(0);
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

fn entry_detail(frame: &mut Frame, area: Rect, app: &App, e: &ConnEntry) {
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
    let mut right = format!(
        "{} · {}",
        p.env.label(),
        if docker.is_some() {
            "found in Docker"
        } else {
            "saved profile"
        }
    );
    if let Some(pv) = p.provider {
        right.push_str(&format!(" · {}", provider::label(pv)));
    }
    let inner = panel(frame, area, &p.name, &right);
    let (lw, rw) = split_cols(usize::from(inner.width));

    let mut endpoint = p.endpoint.to_string();
    if p.db != 0 {
        endpoint.push_str(&format!(" · db {}", p.db));
    }
    match &e.target {
        // A cluster's summary goes on the Server line, which has the room for it.
        None | Some(ScanTarget::Primary | ScanTarget::Cluster(_)) => {}
        Some(t) => endpoint.push_str(&format!(" · scanning {t}")),
    }
    let server = match (&e.server, &e.target) {
        (Some(s), Some(ScanTarget::Cluster(summary))) => {
            // The finished report of this entry, when it is the one on screen.
            let nodes = app
                .session
                .as_ref()
                .filter(|s| s.id == e.id)
                .and_then(|s| s.report.as_ref())
                .and_then(|r| r.nodes.as_deref());
            (
                format!("{} · {summary}", server_label(s)),
                if cluster_degraded(summary, nodes) {
                    style::amber()
                } else {
                    style::fg()
                },
            )
        }
        (Some(s), _) => (
            format!("{} · {}", server_label(s), mode(s.mode)),
            style::fg(),
        ),
        (None, _) => ("not connected yet".to_string(), style::dim()),
    };
    let (auth, cred) = match (docker.map(|(_, _, auth)| auth), p.auth) {
        (Some(DiscoveredAuth::Password), _) => {
            ("password (default user)", "from the container, memory only")
        }
        (Some(DiscoveredAuth::None), _) | (None, Auth::None) => ("none", "none"),
        (Some(DiscoveredAuth::Required), _) => (
            "required",
            "not found: save it as a profile with credentials",
        ),
        (Some(DiscoveredAuth::Unknown), _) => ("unknown", "could not tell"),
        (None, Auth::Keychain) => ("password or ACL user", "OS keychain"),
        (None, Auth::EntraId) => ("Entra ID", "developer tools (az login)"),
        (None, Auth::Command) => (
            "token command",
            "token from the command, username in the OS keychain",
        ),
    };
    let left: Vec<Vec<Span<'static>>> = vec![
        vec![Span::styled(" CONNECTION", style::hd())],
        kv("Endpoint", endpoint, style::fg()),
        kv("Server", server.0, server.1),
        kv("Auth", auth.into(), style::fg()),
        kv("Credential", cred.into(), style::fg()),
        kv(
            "TLS",
            if p.endpoint.tls { "on" } else { "off" }.into(),
            style::fg(),
        ),
        writes_row(p),
    ];

    let mut right: Vec<Vec<Span<'static>>> =
        vec![vec![Span::styled("PERMISSION CHECK", style::hd())]];
    match &e.perms {
        Some(r) => right.extend(perm_lines(r, rw, 6)),
        // Not connected yet: the scan's checks, unrun.
        None => {
            for (i, label) in PERM_LABELS.iter().enumerate() {
                right.push(vec![
                    Span::styled("– ", style::dim()),
                    Span::styled(pad_r(label, 14), style::fg()),
                    Span::styled(if i == 0 { "runs on connect" } else { "" }, style::dim()),
                ]);
            }
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for i in 0..7 {
        let mut spans = fit(left.get(i).cloned().unwrap_or_default(), lw);
        spans.extend(fit(right.get(i).cloned().unwrap_or_default(), rw));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(note(app, e, docker.map(|(c, i, _)| (c, i)))));
    rows(frame, inner, lines, None);
}

fn mode(m: crate::model::ServerMode) -> &'static str {
    match m {
        crate::model::ServerMode::Standalone => "standalone",
        crate::model::ServerMode::Cluster => "cluster",
        crate::model::ServerMode::Sentinel => "sentinel",
    }
}

fn note(app: &App, e: &ConnEntry, docker: Option<(&String, &String)>) -> Span<'static> {
    if let Some(err) = &e.error {
        return Span::styled(format!(" ✗ {err}"), style::warn());
    }
    match e.status {
        ConnStatus::Connecting => {
            let hint = app
                .keychain_hint()
                .filter(|_| app.session.as_ref().is_some_and(|s| s.id == e.id));
            if let Some(hint) = hint {
                return Span::styled(format!(" {} {hint}", spinner(app.tick)), style::amber());
            }
            return Span::styled(
                format!(
                    " {} Connecting and checking permissions…",
                    spinner(app.tick)
                ),
                style::amber(),
            );
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
            return Span::styled(
                if memory {
                    " ▲ This user can't run MEMORY USAGE, so Patterns shows counts but no sizes."
                } else if only_stats {
                    " ▲ MEMORY STATS is denied, so Patterns can't say how much of dataset.bytes the sample explains."
                } else {
                    " ▲ Some checks were denied, so the views that need them are turned off."
                },
                style::amber(),
            );
        }
        ConnStatus::Connected => {
            // A view probe that failed turns its view off but leaves the scan whole.
            let views_off = e.perms.as_ref().is_some_and(|p| {
                p.checks
                    .iter()
                    .any(|c| !PERM_LABELS.contains(&c.label) && c.status != PermStatus::Allowed)
            });
            if views_off {
                return Span::styled(
                    " ✓ The scan has every permission it needs. Views whose check failed are off.",
                    style::ok(),
                );
            }
            return Span::styled(" ✓ Every permission check passed.", style::ok());
        }
        _ => {}
    }
    if let Some((container, image)) = docker {
        return Span::styled(
            format!(" Found in Docker: container {container} ({image}). Press enter to connect."),
            style::dim(),
        );
    }
    // Before connecting, say what the provider takes away; once connected the
    // permission check above is what counts.
    if let Some(n) = provider::notice(e.profile.provider) {
        return Span::styled(format!(" ▲ {n}"), style::amber());
    }
    let lead = if e.just_saved { " Saved. " } else { " " };
    Span::styled(
        format!("{lead}Press enter to connect. redis-lens checks permissions before it scans."),
        style::dim(),
    )
}

fn add_panel(frame: &mut Frame, area: Rect, app: &App, add: Option<&AddState>) {
    let parsed = add.and_then(|a| a.parsed());
    match (add, parsed) {
        (Some(add), Some(Ok(p))) => parsed_panel(frame, area, app, add, &p),
        (Some(add), Some(Err(e))) => {
            let inner = panel(frame, area, "Add connection", "could not parse");
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
            let inner = panel(
                frame,
                area,
                "Add connection",
                "redis:// · rediss:// · StackExchange.Redis · Azure",
            );
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

fn parsed_panel(frame: &mut Frame, area: Rect, app: &App, add: &AddState, p: &ParsedConnection) {
    let fmt = format_label(p);
    let inner = panel(frame, area, "Add connection", &fmt);
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
