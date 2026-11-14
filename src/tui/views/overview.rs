//! 1 Overview: what `App` keeps for the Overview area, the findings it lists, and
//! "Analyze the keys?" (`r`).
//!
//! The live figures at the top of Overview are the Server view state's Overview poll
//! ([`super::server::ServerState::overview`]): `Screen::Overview` is owned by
//! [`super::Owner::Server`], so that poll and the cluster per-shard poll run while
//! Overview is on screen. Drawing is in `ui/overview.rs`.
//!
//! # What a connect reads
//!
//! A connect reads no key. It reads the server's own figures ([`Figures`]:
//! `scan::figures`, kept in `Session::figures`), and once, where the permission check
//! allows them, `SLOWLOG GET` and `CLIENT LIST` (they fill the Server view state's Slow
//! log and Clients, as if those panels had polled). Overview's "FROM THE SERVER"
//! findings come from these alone ([`server_findings`]): big keys counted by `INFO
//! keysizes`, the slowest slow-log entry, and many idle connections from one address.
//!
//! # Analyzing
//!
//! `r` on Overview, Browse › Keys and the Analyze panels opens [`AnalyzeDialog`]: Quick
//! ([`Depth::Quick`], [`QUICK_SAMPLE`] keys, the default) or Deep ([`Depth::Deep`],
//! [`DEEP_SAMPLE`]), each with the engine's estimate ([`estimate_for`]). `enter` starts it
//! as a new run of the scan machinery with `ScanOptions::sample_limit` set to
//! [`Depth::sample_limit`], keeping the old results on screen meanwhile as a rescan did.
//! `esc` while it runs stops it and keeps what it sampled: the scan returns a partial
//! report (`ScanOptions::stop`), which becomes the current results, marked stopped.
//! After an analysis, "THINGS TO LOOK AT" ([`analysis_findings`]) lists the Big keys
//! recommendations, the TTL risks and growth, and the slow log.

use std::time::Duration;

use crate::analysis::{Recommendation, Severity, recommendations};
use crate::format;
use crate::live::server::{ClientConn, SlowEntry};
use crate::model::{ContextGap, PERM_FREQ, PERM_MEMORY, PermStatus, Profile, ScanReport};
use crate::scan::ScanOptions;
use crate::scan::estimate::{estimate_analysis, estimate_cluster_analysis};
use crate::scan::figures::{NodeFigures, ServerFigures};
use crate::tui::app::{App, Phase, Screen, Session};

pub use crate::scan::estimate::{DEEP_SAMPLE, QUICK_SAMPLE};

use super::server::IDLE_FLAG_SECS;
use super::{PROBE_CLIENT_LIST, PROBE_SLOWLOG};

/// How many keys an analysis samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    /// [`QUICK_SAMPLE`] keys: the default.
    #[default]
    Quick,
    /// [`DEEP_SAMPLE`] keys: finds rare patterns and rare big keys.
    Deep,
}

impl Depth {
    pub const ALL: [Depth; 2] = [Depth::Quick, Depth::Deep];

    /// `ScanOptions::sample_limit` for this depth.
    pub fn sample_limit(self) -> u64 {
        match self {
            Self::Quick => QUICK_SAMPLE,
            Self::Deep => DEEP_SAMPLE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Quick => "Quick",
            Self::Deep => "Deep",
        }
    }
}

/// What the connect read of the server itself, without reading a key: the figures of
/// `scan::figures`. Kept in `Session::figures` for the session; every run reads them
/// again (a database switch changes them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Figures {
    /// The server's figures, summed over the nodes on a cluster. `None` when `INFO` or
    /// `DBSIZE` failed (`failed` says why).
    pub server: Option<ServerFigures>,
    /// On a cluster, one per node in the live node table's order (`Session::nodes`);
    /// empty on a standalone server.
    pub nodes: Vec<NodeFigures>,
    /// Why `server` is `None`: `scan::figures::failure_text`, never the server's text.
    pub failed: Option<&'static str>,
}

/// The newest saved snapshot of the session's profile, read from the snapshot
/// directory on connect (never from the server): Overview's "Last analysis" line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastSnapshot {
    pub taken_at_unix: i64,
    pub sampled_keys: u64,
}

/// What an analysis costs, roughly: the engine's [`estimate_analysis`] or
/// [`estimate_cluster_analysis`] at the profile's command budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Keys sampled: the sample, or every key when the database holds fewer.
    pub keys: u64,
    /// Commands sent, about: the figures, the `SCAN`s and 2 to 4 per key.
    pub commands: u64,
    /// The profile's command budget, per second; `None` when it has none (local).
    pub per_sec: Option<u32>,
    /// How long the budget makes it take at least; `None` without a budget.
    pub duration: Option<Duration>,
    /// The shards it samples across; 0 on a standalone server.
    pub shards: usize,
    /// `MEMORY USAGE` per key (the permission check allowed it).
    pub sizes: bool,
    /// `OBJECT FREQ` per key (allowed, and the policy is LFU).
    pub freq: bool,
}

/// The engine's estimate of a `depth` analysis for `profile` alone, when nothing is
/// known of the server: the database is taken to hold at least the sample, sizes on,
/// no LFU counters. Screens that have the session use [`estimate_for`].
pub fn estimate(depth: Depth, profile: &Profile) -> Estimate {
    let per_sec = ScanOptions::for_profile(profile).max_commands_per_sec;
    let e = estimate_analysis(depth.sample_limit(), u64::MAX, true, false, per_sec);
    Estimate {
        keys: e.keys,
        commands: e.commands,
        per_sec,
        duration: e.duration,
        shards: 0,
        sizes: true,
        freq: false,
    }
}

/// The engine's estimate of a `depth` analysis of `app`'s session: the keys from the
/// connect's figures (each node's `DBSIZE` on a cluster), `MEMORY USAGE` and `OBJECT
/// FREQ` as the permission check and the policy allow them (the choices the scan makes),
/// at the profile's command budget. Without a session, [`estimate`] of nothing.
pub fn estimate_for(depth: Depth, app: &App) -> Estimate {
    let Some(s) = app.session.as_ref() else {
        return estimate(depth, &empty_profile());
    };
    let per_sec = ScanOptions::for_profile(&s.profile).max_commands_per_sec;
    let perms = app.entry(s.id).and_then(|e| e.perms.as_ref());
    let server = s
        .figures
        .as_ref()
        .and_then(|f| f.server.as_ref())
        .map(|f| &f.server)
        .or(s.server.as_ref());
    let sizes = perms.is_none_or(|p| p.allowed(PERM_MEMORY));
    let freq =
        perms.is_some_and(|p| p.allowed(PERM_FREQ)) && server.is_some_and(|v| v.lfu_enabled());
    let sample = depth.sample_limit();
    let nodes = s.figures.as_ref().map_or(&[][..], |f| &f.nodes[..]);
    let (e, shards) = if nodes.is_empty() {
        let dbsize = server.map_or(u64::MAX, |v| v.total_keys);
        (estimate_analysis(sample, dbsize, sizes, freq, per_sec), 0)
    } else {
        let dbsizes: Vec<u64> = nodes
            .iter()
            .map(|n| n.figures.as_ref().map_or(0, |f| f.server.total_keys))
            .collect();
        (
            estimate_cluster_analysis(sample, &dbsizes, sizes, freq, per_sec),
            nodes.iter().filter(|n| n.figures.is_some()).count(),
        )
    };
    Estimate {
        keys: e.keys,
        commands: e.commands,
        per_sec,
        duration: e.duration,
        shards,
        sizes,
        freq,
    }
}

/// Whole seconds of `d`, rounded, at least 1.
fn round_secs(d: Duration) -> u64 {
    (d.as_secs_f64().round() as u64).max(1)
}

/// `about 8 s`, `about 2 min`, or how fast without a budget.
pub fn about_time(e: &Estimate) -> String {
    match e.duration.map(round_secs) {
        Some(s) if s < 120 => format!("about {s} s"),
        Some(s) => format!("about {} min", s.div_ceil(60)),
        None => "as fast as the server answers".to_string(),
    }
}

/// `about 4 s left` while an analysis runs: the keys still to sample at the budget, or
/// `None` without a budget or a target.
pub fn time_left(e: &Estimate, sampled: u64, target: u64) -> Option<String> {
    let left = target.checked_sub(sampled)?;
    let rest = estimate_analysis(left, left, e.sizes, e.freq, e.per_sec);
    let secs = round_secs(rest.duration?);
    Some(if secs < 120 {
        format!("about {secs} s left")
    } else {
        format!("about {} min left", secs.div_ceil(60))
    })
}

/// "Analyze the keys?": open while `App::analyze` is `Some`. Every key goes to it:
/// `↑↓` (and `j` `k`) pick, `enter` starts, `esc` cancels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalyzeDialog {
    pub pick: Depth,
}

/// Overview's own state; reset on every connect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewState {
    /// The selected finding under "Keys" (`↑↓`; `enter` goes there): an index into
    /// [`findings`].
    pub sel: usize,
    /// When the running analysis was started (`App::now`), for the done notice's time.
    pub started: Option<std::time::Instant>,
    /// The running analysis was started with `r` (not by a database switch, which
    /// says so itself): its report announces itself in the notice line.
    pub announce: bool,
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// "Many idle clients from one address": at least this many connections from one IP
/// address, each idle for [`IDLE_FLAG_SECS`] (an hour) or more. Ten idle connections
/// from one host is more than a pool usually keeps open for nothing.
pub const IDLE_CLIENTS_MIN: usize = 10;

/// One line of "FROM THE SERVER" or "THINGS TO LOOK AT": what was found, how much it
/// matters, and the screen `enter` goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
    /// Where `enter` goes: Analyze › Big keys, Expiry or Changes, Server › Slow log or
    /// Clients.
    pub to: Screen,
}

impl Finding {
    /// The WHERE column: the panel's name.
    pub fn place(&self) -> &'static str {
        self.to.panel().map_or("", crate::tui::app::Panel::name)
    }
}

/// True when Overview shows an analysis's findings: a report is on screen and no
/// analysis runs (or connects to run). Otherwise it shows what the server says on its
/// own.
pub fn shows_analysis(app: &App) -> bool {
    app.report().is_some() && !app.analysis_running()
}

/// The findings Overview lists now, in order: [`analysis_findings`] after an
/// analysis, else [`server_findings`]. `enter` goes to `findings(app)[sel].to`.
pub fn findings(app: &App) -> Vec<Finding> {
    match app.report() {
        Some(r) if !app.analysis_running() => analysis_findings(app, r),
        _ => server_findings(app),
    }
}

/// `812 ms`, `1.2 s`, `640 µs`.
pub fn micros_text(us: u64) -> String {
    if us < 1_000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{} ms", (us as f64 / 1_000.0).round() as u64)
    } else {
        format!("{:.1} s", us as f64 / 1_000_000.0)
    }
}

/// `14:02 UTC`: every time on Overview, as the header shows its own.
pub fn at_utc(unix: i64) -> String {
    format!("{} UTC", crate::tui::ui::hh_mm(unix))
}

/// The slowest entry of the slow log the connect (or the Slow log panel) read: its
/// command name and how long it took, never its arguments (they can hold values).
fn slow_finding(slow: &[SlowEntry]) -> Option<Finding> {
    let e = slow.iter().max_by_key(|e| (e.micros, e.unix))?;
    let name = if e.command.is_empty() {
        "A command"
    } else {
        e.command.as_str()
    };
    Some(Finding {
        severity: Severity::Note,
        text: format!(
            "{name} took {} at {}.",
            micros_text(e.micros),
            at_utc(e.unix)
        ),
        to: Screen::SlowLog,
    })
}

/// The IP address of a `CLIENT LIST` address: `10.0.9.17` of `10.0.9.17:52344`, `::1`
/// of `[::1]:6379`; `None` for a socket path.
pub fn host_of(addr: &str) -> Option<&str> {
    if addr.is_empty() || addr.starts_with('/') || addr.starts_with('@') {
        return None;
    }
    let host = match addr.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => addr.rsplit_once(':').map_or(addr, |(h, _)| h),
    };
    (!host.is_empty()).then_some(host)
}

/// [`IDLE_CLIENTS_MIN`] or more connections idle for an hour or more from one address:
/// the address with the most, ties by address.
fn idle_finding(conns: &[ClientConn]) -> Option<Finding> {
    let mut by_host: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for c in conns.iter().filter(|c| c.idle >= IDLE_FLAG_SECS) {
        if let Some(h) = host_of(&c.addr) {
            *by_host.entry(h).or_default() += 1;
        }
    }
    let (host, n) = by_host
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))?;
    (n >= IDLE_CLIENTS_MIN).then(|| Finding {
        severity: Severity::Info,
        text: format!(
            "{} connections from {host} idle for over an hour.",
            format::grouped(n as u64)
        ),
        to: Screen::Clients,
    })
}

/// What the server says on its own, no key read: big keys counted by `INFO keysizes`,
/// the slowest slow-log entry, and many idle connections from one address.
pub fn server_findings(app: &App) -> Vec<Finding> {
    let Some(s) = app.session.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let big = s
        .figures
        .as_ref()
        .and_then(|f| f.server.as_ref())
        .and_then(|f| f.headline().big_keys);
    if let Some(n) = big.filter(|n| *n > 0) {
        let what = if n == 1 { "big key" } else { "big keys" };
        out.push(Finding {
            severity: Severity::Warn,
            text: format!(
                "{} {what}, counted by the server (INFO keysizes).",
                format::grouped(n)
            ),
            to: Screen::BigKeys,
        });
    }
    out.extend(slow_finding(&app.views.server.slow.entries));
    out.extend(idle_finding(&app.views.server.clients.conns));
    out
}

/// Where a recommendation of [`recommendations`] is looked into: Expiry for the TTL
/// rules (a lock without a TTL, memory that never expires, a pattern whose keys never
/// expire), Changes for a pattern that never expires and grows when there is a
/// comparison with a snapshot, Big keys for the rest (big keys, encodings).
fn rec_target(rec: &Recommendation, has_diff: bool) -> Screen {
    if rec.text.ends_with("never expires and is growing.") {
        if has_diff { Screen::Diff } else { Screen::Ttl }
    } else if rec.text.contains("never expire") {
        Screen::Ttl
    } else {
        Screen::BigKeys
    }
}

/// "THINGS TO LOOK AT" after an analysis: today's recommendations (big keys and
/// encodings, the TTL risks, growth from Changes when there is a snapshot to compare
/// with), then the slowest slow-log entry.
pub fn analysis_findings(app: &App, report: &ScanReport) -> Vec<Finding> {
    let Some(s) = app.session.as_ref() else {
        return Vec::new();
    };
    let diff = s.diff();
    let mut out: Vec<Finding> = recommendations(report, diff, &app.views.analysis.facts)
        .into_iter()
        .map(|rec| Finding {
            severity: rec.severity,
            to: rec_target(&rec, diff.is_some()),
            text: rec.text,
        })
        .collect();
    out.extend(slow_finding(&app.views.server.slow.entries));
    out
}

/// Why a check reads as off, in redis-lens's own words.
fn probe_off(label: &str, status: &PermStatus) -> Option<String> {
    match status {
        PermStatus::Allowed => None,
        PermStatus::Denied(_) => Some(format!("the server refused {label} for this user")),
        PermStatus::Skipped(why) => Some(format!("{label} is not available: {why}")),
    }
}

/// One line for each part of "FROM THE SERVER" that is off, and why (rule 3): a
/// missing `INFO keysizes`, `MEMORY STATS` or `INFO keyspace`, and a slow log or client
/// list the permission check (or the server) refused. Fixed text, never the server's.
pub fn off_reasons(app: &App) -> Vec<String> {
    let Some(s) = app.session.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(f) = s.figures.as_ref().and_then(|f| f.server.as_ref()) {
        for gap in &f.context.missing {
            let line = match gap {
                ContextGap::NoKeysizes => {
                    "Big keys from the server need INFO keysizes (Redis 8 and later)."
                }
                ContextGap::MemoryStatsDenied => {
                    "Data and overhead are missing: MEMORY STATS is denied for this user."
                }
                ContextGap::MemoryStatsUnavailable => {
                    "Data and overhead are missing: this server has no MEMORY STATS."
                }
                ContextGap::KeyspaceUnavailable => {
                    "The share with a TTL is missing: INFO keyspace could not be read."
                }
                ContextGap::Other => continue,
            };
            out.push(line.to_string());
        }
    }
    let perms = app.entry(s.id).and_then(|e| e.perms.as_ref());
    let check = |label: &str| {
        perms
            .and_then(|p| p.checks.iter().find(|c| c.label == label))
            .and_then(|c| probe_off(label, &c.status))
    };
    let server = &app.views.server;
    let slow = check(PROBE_SLOWLOG).or_else(|| server.slow.poll.off.clone());
    if let Some(why) = slow {
        out.push(format!("Slow log is off: {}.", trim_period(&why)));
    }
    let clients = check(PROBE_CLIENT_LIST).or_else(|| server.clients.poll.off.clone());
    if let Some(why) = clients {
        out.push(format!("Idle clients are off: {}.", trim_period(&why)));
    }
    out
}

fn trim_period(s: &str) -> String {
    let s = s.trim_end_matches('.');
    // A panel's own off text ends "…, so this panel is off": the panel is not the point here.
    let s = s.strip_suffix(", so this panel is off").unwrap_or(s);
    // It follows "…is off: ", so a sentence's capital goes; a command name's stays.
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) if a.is_uppercase() && b.is_lowercase() => {
            a.to_lowercase().chain(s[a.len_utf8()..].chars()).collect()
        }
        _ => s.to_string(),
    }
}

/// True when the results on screen are those of an analysis `esc` stopped early: a
/// partial report, kept as the current results (the header says `(stopped)`).
pub fn stopped_early(s: &Session) -> bool {
    s.stopped && s.report.is_some() && s.phase == Phase::Done
}

/// A profile with nothing set, for an estimate without a session.
fn empty_profile() -> Profile {
    Profile {
        name: String::new(),
        env: crate::model::Env::Other,
        endpoint: crate::model::Endpoint {
            host: String::new(),
            port: 6379,
            tls: false,
        },
        db: 0,
        auth: crate::model::Auth::None,
        read_only: None,
        prefer_replica: false,
        provider: None,
        token_command: None,
        ssh: None,
        sentinel: None,
        tags: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint, Env, KeysizeHistogram, KeyspaceInfo, MemoryStats};

    fn profile(env: Env) -> Profile {
        Profile {
            name: "p".into(),
            env,
            endpoint: Endpoint {
                host: "h".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        }
    }

    /// The fixture server's figures: 7.92M keys, 31% with a TTL, 13G of data and 2.1G
    /// overhead, and `big` big keys by `INFO keysizes` (`None`: no such section).
    pub(crate) fn figures(big: Option<u64>) -> ServerFigures {
        let server = crate::tui::fixtures::server();
        let keys = server.total_keys;
        let keysizes = big.map(|n| {
            vec![KeysizeHistogram {
                key_type: "strings".into(),
                unit: "sizes".into(),
                bins: vec![(1024, 1_000), (2 * 1024 * 1024, n)],
            }]
        });
        let mut missing = Vec::new();
        if big.is_none() {
            missing.push(ContextGap::NoKeysizes);
        }
        ServerFigures {
            server,
            context: crate::model::ServerContextParts {
                keyspace: Some(KeyspaceInfo {
                    db: 0,
                    keys,
                    expires: keys * 31 / 100,
                    avg_ttl_ms: 60_000,
                    subexpiry: None,
                }),
                memory: Some(MemoryStats {
                    dataset_bytes: Some(13 * 1024 * 1024 * 1024),
                    overhead_total: Some(2_254_857_830),
                    ..MemoryStats::default()
                }),
                keysizes,
                missing,
            }
            .into(),
        }
    }

    /// A slow-log entry of `command` taking `micros` at 14:01 UTC, with a secret in its
    /// arguments and key that must never reach Overview.
    pub(crate) fn slow_entry(id: u64, micros: u64, command: &str) -> SlowEntry {
        SlowEntry {
            id,
            unix: 50_460,
            micros,
            command: command.into(),
            keys: vec!["session:secret-value".into()],
            more_keys: 0,
            client: "10.0.9.17:5000".into(),
            client_name: String::new(),
            whole_key_read: false,
            args: crate::value::Raw::from("KEYS session:secret-value"),
            args_cut: 0,
        }
    }

    /// The connect's `SLOWLOG GET` answer: a 20 ms `GET` and an 812 ms `KEYS`.
    pub(crate) fn slow_reply() -> crate::live::LiveReply {
        crate::live::LiveReply::Server(crate::live::server::Reply::SlowLog(Box::new(
            crate::live::server::SlowLogReply {
                entries: vec![slow_entry(2, 812_000, "KEYS"), slow_entry(1, 20_000, "GET")],
                config: None,
                keys_off: None,
                learned: std::collections::HashMap::new(),
            },
        )))
    }

    /// A connection from `addr`, idle `idle` seconds.
    pub(crate) fn client(id: u64, addr: &str, idle: u64) -> ClientConn {
        ClientConn {
            id,
            addr: addr.into(),
            name: String::new(),
            lib_name: String::new(),
            lib_ver: String::new(),
            age: idle + 10,
            idle,
            cmd: "get".into(),
            mem: 0,
            db: Some(0),
        }
    }

    /// The connect's `CLIENT LIST` answer: 212 connections from 10.0.9.17 idle for two
    /// hours, and one busy one.
    pub(crate) fn clients_reply() -> crate::live::LiveReply {
        let mut list: Vec<ClientConn> = (0..212)
            .map(|i| client(i, &format!("10.0.9.17:{}", 40_000 + i), 7_200))
            .collect();
        list.push(client(999, "10.0.1.5:6000", 0));
        crate::live::LiveReply::Server(crate::live::server::Reply::Clients(list))
    }

    #[test]
    fn quick_is_the_default_and_samples_the_engine_presets() {
        assert_eq!(AnalyzeDialog::default().pick, Depth::Quick);
        assert_eq!(Depth::Quick.sample_limit(), 10_000);
        assert_eq!(Depth::Deep.sample_limit(), 100_000);
    }

    #[test]
    fn the_profile_only_estimate_is_the_engines_with_sizes_and_no_lfu() {
        let e = estimate(Depth::Quick, &profile(Env::Prod));
        // 6 figures, 10 SCANs, TYPE + PTTL + MEMORY USAGE per key.
        assert_eq!(e.commands, 30_016);
        assert_eq!(e.per_sec, Some(5_000));
        assert_eq!(about_time(&e), "about 6 s");
        let e = estimate(Depth::Deep, &profile(Env::Prod));
        assert_eq!(e.commands, 300_106);
        assert_eq!(about_time(&e), "about 60 s");
        // Local profiles have no budget.
        let e = estimate(Depth::Deep, &profile(Env::Local));
        assert_eq!((e.per_sec, e.duration), (None, None));
        assert_eq!(about_time(&e), "as fast as the server answers");
    }

    #[test]
    fn time_left_counts_the_keys_still_to_sample_at_the_budget() {
        let e = estimate(Depth::Quick, &profile(Env::Prod));
        // 5,900 keys × 3 + 6 SCANs + 6 figures at 5,000/s.
        assert_eq!(
            time_left(&e, 4_100, 10_000).as_deref(),
            Some("about 4 s left")
        );
        assert_eq!(time_left(&e, 10_000, 1_000), None);
        let local = estimate(Depth::Quick, &profile(Env::Local));
        assert_eq!(time_left(&local, 0, 10_000), None);
    }

    #[test]
    fn slow_and_idle_findings_name_no_argument_and_use_a_threshold() {
        let f =
            slow_finding(&[slow_entry(1, 20_000, "GET"), slow_entry(2, 812_000, "KEYS")]).unwrap();
        assert_eq!(f.text, "KEYS took 812 ms at 14:01 UTC.");
        assert!(!f.text.contains("secret"));
        assert_eq!((f.to, f.place()), (Screen::SlowLog, "Slow log"));
        assert_eq!(slow_finding(&[]), None);

        let conn = client;
        let mut conns: Vec<ClientConn> = (0..9)
            .map(|i| conn(i, &format!("10.0.9.17:{}", 5000 + i), 4_000))
            .collect();
        // Busy ones and other hosts do not count.
        conns.push(conn(100, "10.0.9.17:6000", 10));
        conns.push(conn(101, "10.0.9.18:6000", 9_000));
        assert_eq!(idle_finding(&conns), None, "nine is under the threshold");
        conns.push(conn(102, "10.0.9.17:6001", IDLE_FLAG_SECS));
        let f = idle_finding(&conns).unwrap();
        assert_eq!(
            f.text,
            "10 connections from 10.0.9.17 idle for over an hour."
        );
        assert_eq!((f.to, f.place()), (Screen::Clients, "Clients"));
        assert_eq!(host_of("[::1]:6379"), Some("::1"));
        assert_eq!(host_of("/tmp/redis.sock"), None);
    }

    #[test]
    fn recommendations_go_to_the_panel_that_explains_them() {
        let rec = |text: &str| Recommendation {
            severity: Severity::Note,
            text: text.into(),
            hint: String::new(),
        };
        let cases = [
            (
                "queue:emails is a single 612M list.",
                false,
                Screen::BigKeys,
            ),
            (
                "3 of the 5 largest sampled h:{id} hashes are hashtable.",
                false,
                Screen::BigKeys,
            ),
            (
                "lock:{id}: 40% of its keys never expire.",
                false,
                Screen::Ttl,
            ),
            (
                "62% of memory never expires: 9G of 15G.",
                false,
                Screen::Ttl,
            ),
            (
                "31% of user:{id}:cart keys never expire while the rest do.",
                false,
                Screen::Ttl,
            ),
            ("user:{id}:profile never expires.", true, Screen::Ttl),
            (
                "user:{id}:profile never expires and is growing.",
                true,
                Screen::Diff,
            ),
            (
                "user:{id}:profile never expires and is growing.",
                false,
                Screen::Ttl,
            ),
        ];
        for (text, diff, want) in cases {
            assert_eq!(rec_target(&rec(text), diff), want, "{text}");
        }
    }

    #[test]
    fn micros_read_in_the_unit_that_fits() {
        assert_eq!(micros_text(640), "640 µs");
        assert_eq!(micros_text(812_000), "812 ms");
        assert_eq!(micros_text(1_250_000), "1.2 s");
        assert_eq!(at_utc(86_400 + 3_600 * 9 + 12 * 60), "09:12 UTC");
    }

    #[test]
    fn the_headline_of_the_fixture_figures() {
        let h = figures(Some(3)).headline();
        assert_eq!(h.big_keys, Some(3));
        assert!(h.ttl_share.is_some_and(|s| (s - 0.31).abs() < 0.01));
        assert_eq!(figures(None).headline().big_keys, None);
    }
}
