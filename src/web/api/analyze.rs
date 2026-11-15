//! Analyze: starting, stopping and following an analysis (`/api/analysis`), and its
//! panels (Big keys, Expiry, Changes, Hot keys, and on a cluster Nodes and Slots), plus
//! Overview's analysis part ([`overview_block`]).
//!
//! An analysis is the TUI's (`r`, then Quick or Deep): a new run of the session with
//! the depth's sample ([`Hub::new_run`](crate::web::session::Hub::new_run)), the same
//! `ScanOptions` and budget, shard by shard on a cluster. Its progress, its end and a
//! failure come as `progress` events; `GET /api/state` says whether a report exists and
//! how old it is. Every panel is computed from the report as the TUI computes it; Big
//! keys reads the lengths and encodings of its keys once per report, and Slots reads
//! `CLUSTER SLOT-STATS` once per run (again with `refresh=1`).

use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::browse::{Change, Sample};
use super::{App, Body, Q, Refused, error, off, ok};
use crate::analysis::{BIG_KEY_BYTES, KeyFacts, Severity, keys_to_check, length_unit, pattern_of};
use crate::diff::ReportDiff;
use crate::live::keys::{FactKey, Reply as KeysReply, Request as KeysRequest};
use crate::live::slots::{Reply as SlotsReply, Request as SlotsRequest};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest};
use crate::model::{PERM_FREQ, PermStatus, ScanReport, ServerFlavor};
use crate::tui::app::views::overview::{
    Depth, Estimate, about_time, analysis_findings_of, estimate_of,
};
use crate::tui::app::views::slots::{
    AT_ONCE, NEEDS_ENABLED, NOT_CLUSTER, NodeState, Off, Order, Outcome, SlotsState, keys_in_slot,
};
use crate::verdict::ttl_verdict;
use crate::web::dto;
use crate::web::keyname::Name;
use crate::web::session::Current;

pub fn depth_name(d: Depth) -> &'static str {
    match d {
        Depth::Quick => "quick",
        Depth::Deep => "deep",
    }
}

pub fn depth_of(name: &str) -> Option<Depth> {
    match name {
        "quick" => Some(Depth::Quick),
        "deep" => Some(Depth::Deep),
        _ => None,
    }
}

/// Big keys' lengths and encodings, read for one report.
#[derive(Debug, Default)]
pub struct Facts {
    report: (i64, u64),
    pub facts: HashMap<String, KeyFacts>,
    pub refused: Vec<LiveError>,
    pub error: Option<String>,
}

fn report_id(r: &ScanReport) -> (i64, u64) {
    (r.taken_at_unix, r.sampled_keys)
}

/// The facts of the session's report, when read.
pub fn facts_of(c: &Current) -> Option<&Facts> {
    let r = c.session.report.as_ref()?;
    c.browse.facts.as_ref().filter(|f| f.report == report_id(r))
}

pub const NO_REPORT: &str = "No analysis yet: analyze the keys first.";
pub const BAD_DEPTH: &str = "the depth must be quick or deep";
pub const STOPPING: &str = "Stopping the analysis. What it sampled so far is kept.";
pub const STOPPING_AGAIN: &str = "Stopping: the analysis ends after its current batch.";

#[derive(Debug, Serialize, PartialEq)]
pub struct EstimateDto {
    pub depth: &'static str,
    pub sample_limit: u64,
    /// Keys it samples: the sample, or every key when the database holds fewer.
    pub keys: u64,
    /// Read-only commands it sends, about.
    pub commands: u64,
    /// The profile's command budget; `null` without one.
    pub per_sec: Option<u32>,
    pub duration_secs: Option<u64>,
    /// `about 8 s`, `about 2 min`, or `as fast as the server answers`.
    pub about: String,
    /// Shards it samples across; 0 on a standalone server.
    pub shards: usize,
    /// `MEMORY USAGE` per key.
    pub sizes: bool,
    /// `OBJECT FREQ` per key.
    pub freq: bool,
}

impl EstimateDto {
    pub fn of(depth: Depth, e: &Estimate) -> Self {
        Self {
            depth: depth_name(depth),
            sample_limit: depth.sample_limit(),
            keys: e.keys,
            commands: e.commands,
            per_sec: e.per_sec,
            duration_secs: e.duration.map(|d| d.as_secs_f64().round() as u64),
            about: about_time(e),
            shards: e.shards,
            sizes: e.sizes,
            freq: e.freq,
        }
    }
}

fn estimates(c: &Current) -> Vec<EstimateDto> {
    Depth::ALL
        .into_iter()
        .map(|d| EstimateDto::of(d, &estimate_of(d, &c.session, c.perms.as_ref())))
        .collect()
}

/// `GET /api/analysis`: the analysis's state (as in `GET /api/state`) and what a Quick
/// and a Deep one would cost.
pub async fn get(State(app): State<App>) -> Response {
    let state = app.hub.state();
    match app.hub.with(|c| {
        serde_json::json!({
            "analysis": state.analysis,
            "estimates": estimates(c),
            "running": c.analysis_running(),
        })
    }) {
        Some(b) => ok(b),
        None => error(StatusCode::CONFLICT, crate::web::session::NOT_CONNECTED),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct StartBody {
    depth: Option<String>,
}

/// `POST /api/analysis {"depth": "quick"|"deep"}`: starts an analysis (202). Its
/// progress comes as `progress` events.
pub async fn start(State(app): State<App>, Body(b): Body<StartBody>) -> Response {
    let depth = match b.depth.as_deref().map(depth_of) {
        None => Depth::Quick,
        Some(Some(d)) => d,
        Some(None) => return error(StatusCode::BAD_REQUEST, BAD_DEPTH),
    };
    let estimate = app
        .hub
        .with(|c| EstimateDto::of(depth, &estimate_of(depth, &c.session, c.perms.as_ref())));
    match app.hub.new_run(Some(depth), None) {
        Ok(run) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({
                "run": run,
                "depth": depth_name(depth),
                "sample_limit": depth.sample_limit(),
                "estimate": estimate,
            })),
        )
            .into_response(),
        Err(r) => r.into_response(),
    }
}

/// `DELETE /api/analysis`: stops the running analysis, as `esc` does: it stops within
/// one batch, and what it sampled becomes the report, marked stopped.
pub async fn stop(State(app): State<App>) -> Response {
    match app.hub.stop_analysis() {
        Ok(first) => ok(serde_json::json!({
            "stopping": true,
            "text": if first { STOPPING } else { STOPPING_AGAIN },
        })),
        Err(r) => r.into_response(),
    }
}

/// Runs `f` on the session and its report, or says why there is none.
fn with_report<T>(app: &App, f: impl FnOnce(&Current, &ScanReport) -> T) -> Result<T, Refused> {
    app.hub
        .with(|c| match &c.session.report {
            Some(r) => Ok(f(c, r)),
            None => Err(Refused::new(StatusCode::CONFLICT, NO_REPORT)),
        })
        .unwrap_or_else(|| {
            Err(Refused::new(
                StatusCode::CONFLICT,
                crate::web::session::NOT_CONNECTED,
            ))
        })
}

/// What every panel says of the report it shows.
fn about(c: &Current, r: &ScanReport) -> serde_json::Value {
    serde_json::json!({
        "taken_at_unix": r.taken_at_unix,
        "sampled_keys": r.sampled_keys,
        "sizes_available": r.sizes_available,
        "complete": r.complete,
        "stopped": crate::tui::app::views::overview::stopped_early(&c.session),
        "previous": c.session.previous.is_some(),
    })
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Rec {
    pub severity: &'static str,
    pub text: String,
    pub hint: String,
}

pub fn severity_name(s: Severity) -> &'static str {
    match s {
        Severity::Warn => "warn",
        Severity::Note => "note",
        Severity::Info => "info",
    }
}

/// What [`read_facts`] has to do.
enum FactsTodo {
    /// Read them: the connection, the report, the requests by node, and the sender that
    /// says when the read ended.
    Read(
        crate::tui::app::ConnId,
        (i64, u64),
        Vec<(Dest, Vec<FactKey>)>,
        tokio::sync::watch::Sender<bool>,
    ),
    /// Another request reads them now: wait for it.
    Wait(tokio::sync::watch::Receiver<bool>),
}

/// Reads Big keys' lengths and encodings for the session's report when they are
/// missing, one request per node (each key on its slot's owner), as the TUI does. Once
/// per report: a request that comes while they are read waits for that read. The read
/// runs in a task of its own, so it ends (and the next request does not wait for
/// nothing) even when the request that started it goes. Overview's findings use the
/// facts: an `overview` event goes out once they are in.
async fn read_facts(app: &App) {
    let todo = app
        .hub
        .with_mut(|c| {
            let r = c.session.report.as_ref()?;
            if facts_of(c).is_some() || !c.live_ready() {
                return None;
            }
            let rid = report_id(r);
            if let Some((at, rx)) = &c.browse.facts_reading
                && *at == rid
                && rx.has_changed().is_ok()
            {
                return Some(FactsTodo::Wait(rx.clone()));
            }
            let big: std::collections::HashSet<&str> =
                r.big_keys().iter().map(|k| k.key.as_str()).collect();
            let keys: Vec<FactKey> = keys_to_check(r)
                .into_iter()
                .map(|(key, key_type)| FactKey {
                    length: big.contains(key.as_str()),
                    key,
                    key_type,
                })
                .collect();
            let groups = crate::tui::app::views::analysis::split_by_node(&c.session, keys);
            let (tx, rx) = tokio::sync::watch::channel(false);
            c.browse.facts_reading = Some((rid, rx));
            Some(FactsTodo::Read(c.id, rid, groups, tx))
        })
        .flatten();
    match todo {
        None => {}
        Some(FactsTodo::Wait(mut rx)) => {
            let _ = rx.wait_for(|done| *done).await;
        }
        Some(FactsTodo::Read(conn, rid, groups, tx)) => {
            let app = app.clone();
            let _ = tokio::spawn(async move {
                read_facts_now(&app, conn, rid, groups).await;
                let _ = tx.send(true);
            })
            .await;
        }
    }
}

async fn read_facts_now(
    app: &App,
    conn: crate::tui::app::ConnId,
    rid: (i64, u64),
    groups: Vec<(Dest, Vec<FactKey>)>,
) {
    let mut facts = Facts {
        report: rid,
        ..Facts::default()
    };
    let hub = &app.hub;
    let answers: Vec<_> = futures::stream::iter(groups.into_iter().filter(|(_, k)| !k.is_empty()))
        .map(|(dest, keys)| async move {
            hub.request(
                Some(conn),
                dest,
                LiveRequest::Keys(KeysRequest::Facts { keys }),
            )
            .await
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let mut closed = false;
    for a in answers {
        match a {
            Ok(LiveReply::Keys(KeysReply::Facts { facts: f, refused })) => {
                facts.facts.extend(f);
                for e in refused {
                    if !facts.refused.contains(&e) {
                        facts.refused.push(e);
                    }
                }
            }
            Ok(_) => {}
            Err(LiveError::Closed) => closed = true,
            Err(e) => {
                if facts.error.is_none() {
                    facts.error = Some(format!("Could not read the lengths: {e}."));
                }
            }
        }
    }
    let stored = app.hub.with_mut(|c| {
        if c.browse
            .facts_reading
            .as_ref()
            .is_some_and(|(at, _)| *at == rid)
        {
            c.browse.facts_reading = None;
        }
        // A closed connection read nothing: the next request reads them again.
        let same = c.id == conn && c.session.report.as_ref().map(report_id) == Some(rid);
        if same && !closed {
            c.browse.facts = Some(facts);
        }
        same && !closed
    });
    if stored == Some(true) {
        app.hub.emit_overview_if_changed();
    }
}

/// `GET /api/analysis/big_keys`: the report's biggest sampled keys, with their lengths
/// and encodings, and the recommendations ("Things to look at").
pub async fn big_keys(State(app): State<App>) -> Response {
    read_facts(&app).await;
    match with_report(&app, big_keys_body) {
        Ok(b) => ok(b),
        Err(r) => r.into_response(),
    }
}

fn big_keys_body(c: &Current, r: &ScanReport) -> serde_json::Value {
    let facts = facts_of(c);
    let empty = HashMap::new();
    let f = facts.map_or(&empty, |f| &f.facts);
    let rows: Vec<serde_json::Value> = r
        .big_keys()
        .iter()
        .map(|k| {
            let kf = f.get(&k.key);
            serde_json::json!({
                "key": Sample::of(k),
                "big": k.bytes.is_some_and(|b| b >= BIG_KEY_BYTES),
                "pattern": Name::of(&pattern_of(r, &k.key)),
                "length": kf.and_then(|x| x.length),
                "length_unit": length_unit(k.key_type),
                "encoding": kf.and_then(|x| x.encoding.as_deref()).map(crate::value::escape),
                "node": k.node.and_then(|n| node_endpoint(r, n)),
            })
        })
        .collect();
    let recs: Vec<Rec> = crate::analysis::recommendations(r, c.session.diff(), f)
        .into_iter()
        .map(|x| Rec {
            severity: severity_name(x.severity),
            text: crate::web::wording::web(&x.text),
            hint: crate::web::wording::web(&x.hint),
        })
        .collect();
    let mut notes: Vec<String> = Vec::new();
    if let Some(e) = facts.and_then(|f| f.error.as_deref()) {
        notes.push(crate::web::wording::server_free(e));
    }
    for e in facts.map_or(&[][..], |f| &f.refused[..]) {
        notes.push(crate::web::wording::server_free(&format!(
            "Without it, some rules are skipped: {e}."
        )));
    }
    if facts.is_none() && !c.live_ready() {
        notes.push("Lengths and encodings are read once the live connection is open.".into());
    }
    let empty_text = rows.is_empty().then_some(if r.sizes_available {
        "No sampled key had a size, so there are no big keys to rank."
    } else {
        "No sizes: MEMORY USAGE is not allowed for this user, so keys can't be ranked by size."
    });
    serde_json::json!({
        "report": about(c, r),
        "big_key_bytes": BIG_KEY_BYTES,
        "rows": rows,
        "empty": empty_text,
        "facts_read": facts.is_some(),
        "recommendations": recs,
        "notes": notes,
    })
}

fn node_endpoint(r: &ScanReport, n: u16) -> Option<String> {
    r.nodes
        .as_ref()?
        .get(usize::from(n))
        .map(|x| crate::value::escape(&x.endpoint.to_string()))
}

/// `GET /api/analysis/expiry`: patterns by TTL risk (the TUI's order: most worrying
/// verdict first, then memory, then name) and the memory freed by TTL.
pub async fn expiry(State(app): State<App>) -> Response {
    match with_report(&app, expiry_body) {
        Ok(b) => ok(b),
        Err(r) => r.into_response(),
    }
}

fn expiry_body(c: &Current, r: &ScanReport) -> serde_json::Value {
    let diff = c.session.diff();
    let mut idx: Vec<usize> = (0..r.patterns.len()).collect();
    idx.sort_by_cached_key(|&i| {
        let p = &r.patterns[i];
        let v = ttl_verdict(p, diff.and_then(|d| d.pattern(&p.pattern)));
        let w = if r.sizes_available {
            p.est_bytes.unwrap_or(0)
        } else {
            p.est_keys
        };
        (v, std::cmp::Reverse(w), p.pattern.clone())
    });
    let rows: Vec<serde_json::Value> = idx
        .into_iter()
        .map(|i| {
            let p = &r.patterns[i];
            let change = diff.and_then(|d| d.pattern(&p.pattern));
            let v = ttl_verdict(p, change);
            let explain: Vec<String> = crate::verdict::explain(p, change)
                .iter()
                .map(|t| crate::value::escape(t))
                .collect();
            serde_json::json!({
                "pattern": Name::of(&p.pattern),
                "est_keys": p.est_keys,
                "est_bytes": p.est_bytes,
                "sampled_keys": p.sampled_keys,
                "no_ttl": p.no_ttl,
                "no_ttl_ratio": p.no_ttl_ratio(),
                "ttl_hist": p.ttl_hist,
                "status": v.label(),
                "warning": v.is_warning(),
                "explain": explain,
                "command": crate::tui::clipboard::suggested_command(&p.pattern),
            })
        })
        .collect();
    let freed = crate::analysis::expiry(r).map(|e| {
        serde_json::json!({
            "within": crate::analysis::EXPIRY_LABELS
                .iter()
                .zip(e.within)
                .map(|(l, b)| serde_json::json!({ "label": l, "bytes": b }))
                .collect::<Vec<_>>(),
            "later": e.later,
            "never": e.never,
            "total": e.total,
            "never_share": e.never_share(),
        })
    });
    serde_json::json!({
        "report": about(c, r),
        "rows": rows,
        "empty": r.patterns.is_empty().then_some("No keys were sampled."),
        "ttl_labels": crate::model::TTL_BUCKET_LABELS,
        "freed": freed,
        "freed_off": freed.is_none().then_some(
            "No memory figures: MEMORY USAGE is not allowed for this user, or this analysis has none."
        ),
    })
}

/// The Changes body of a diff: the TUI's rows, with the trend when there is one.
pub fn diff_body(
    d: &ReportDiff,
    trend: Option<&crate::snapshot::Trend>,
    note: Option<&str>,
) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = d
        .patterns
        .iter()
        .map(|p| {
            let t = trend.and_then(|t| t.pattern(&p.pattern));
            serde_json::json!({
                "pattern": Name::of(&p.pattern),
                "keys_before": p.keys_before,
                "keys_after": p.keys_after,
                "bytes_before": p.bytes_before,
                "bytes_after": p.bytes_after,
                "change": Change::of(p),
                "partial": p.partial,
                "trend": t.map(|t| serde_json::json!({ "values": t.values, "growth": t.growth })),
            })
        })
        .collect();
    serde_json::json!({
        "before_taken_at_unix": d.before_taken_at_unix,
        "after_taken_at_unix": d.after_taken_at_unix,
        "before_utc": crate::snapshot::format_utc(d.before_taken_at_unix),
        "after_utc": crate::snapshot::format_utc(d.after_taken_at_unix),
        "used_memory_before": d.used_memory_before,
        "used_memory_after": d.used_memory_after,
        "rows": rows,
        "empty": d.patterns.is_empty().then_some("Neither analysis sampled any keys."),
        "note": note,
        "trend_points": trend.map(|t| &t.points),
    })
}

/// `GET /api/analysis/changes`: this analysis against the newest snapshot before it,
/// as the TUI's Changes panel.
pub async fn changes(State(app): State<App>) -> Response {
    let body = with_report(&app, |c, r| {
        use crate::tui::app::History;
        let s = &c.session;
        let name = crate::value::escape(&c.report_name());
        match &s.history {
            History::Pending => serde_json::json!({
                "report": about(c, r), "status": "pending", "text": "Looking for an earlier snapshot…",
            }),
            History::NoSnapshot => serde_json::json!({
                "report": about(c, r),
                "status": "none",
                "text": format!("No earlier snapshot for {name}. Save one; the next analysis compares against it."),
            }),
            History::Failed(e) => serde_json::json!({
                "report": about(c, r),
                "status": "failed",
                "text": format!(
                    "Could not load snapshots: {}. The other panels still work; only the comparison is missing.",
                    crate::web::wording::server_free(e)
                ),
            }),
            History::Diff(d) => {
                let mut b = diff_body(d, s.trend(), None);
                if let Some(o) = b.as_object_mut() {
                    o.insert("report".into(), about(c, r));
                    o.insert("status".into(), "diff".into());
                }
                b
            }
        }
    });
    match body {
        Ok(b) => ok(b),
        Err(r) => r.into_response(),
    }
}

/// Hot keys need a big key at least this size and this LFU counter to count as hot and
/// big (the TUI's `HOT_BIG` and `HOT_FREQ`).
pub const HOT_BIG: u64 = 10 * 1024 * 1024;
pub const HOT_FREQ: u8 = 100;

/// Why Hot keys is empty, in the TUI's words; `off` when LFU counters can't be read.
fn hot_reason(c: &Current, r: &ScanReport) -> Option<(bool, String)> {
    if !r.hot_keys.is_empty() {
        return None;
    }
    let denied = c.perms.as_ref().is_some_and(|p| {
        p.checks
            .iter()
            .any(|x| x.label == PERM_FREQ && matches!(x.status, PermStatus::Denied(_)))
    });
    Some(if denied {
        (true, "OBJECT FREQ was denied for this user.".to_string())
    } else if !r.server.lfu_enabled() {
        (
            true,
            format!(
                "The eviction policy is {}, so Redis keeps no LFU counters. Set \
                 maxmemory-policy to allkeys-lfu or volatile-lfu to see hot keys.",
                crate::value::escape(r.server.policy_label())
            ),
        )
    } else if r.sampled_keys == 0 {
        (false, "The analysis sampled no keys.".to_string())
    } else {
        (
            false,
            "No sampled key has an LFU counter above 0 yet.".to_string(),
        )
    })
}

/// `GET /api/analysis/hot_keys`: the sampled keys with the highest LFU counters. Off
/// (`{"off": ...}`) when the policy keeps no LFU counters or `OBJECT FREQ` is refused.
pub async fn hot_keys(State(app): State<App>) -> Response {
    let body = with_report(&app, |c, r| match hot_reason(c, r) {
        Some((true, why)) => Err(why),
        Some((false, why)) => Ok(serde_json::json!({
            "report": about(c, r), "rows": [], "empty": why,
        })),
        None => Ok(serde_json::json!({
            "report": about(c, r),
            "hot_big_bytes": HOT_BIG,
            "hot_freq": HOT_FREQ,
            "rows": r.hot_keys.iter().map(|k| serde_json::json!({
                "key": Sample::of(k),
                "hot_and_big": k.bytes.is_some_and(|b| b >= HOT_BIG) && k.lfu_freq.is_some_and(|f| f >= HOT_FREQ),
                "pattern": Name::of(&pattern_of(r, &k.key)),
                "node": k.node.and_then(|n| node_endpoint(r, n)),
            })).collect::<Vec<_>>(),
            "empty": null,
        })),
    });
    match body {
        Ok(Ok(b)) => ok(b),
        Ok(Err(why)) => off(&why),
        Err(r) => r.into_response(),
    }
}

/// Not a cluster, as the TUI's Nodes panel says it.
pub const NODES_NOT_CLUSTER: &str = "Not a cluster: this analysis read one standalone server.";

/// `GET /api/analysis/nodes`: on a cluster, each shard and the node read for it. Off on
/// a standalone server.
pub async fn nodes(State(app): State<App>) -> Response {
    let body = with_report(&app, |c, r| {
        let ns = r.nodes.as_ref()?;
        Some(serde_json::json!({
            "report": about(c, r),
            "rows": ns.iter().enumerate().map(|(i, n)| serde_json::json!({
                "index": i,
                "id": crate::value::escape(&n.id),
                "endpoint": crate::value::escape(&n.endpoint.to_string()),
                "role": dto::role_name(n.role),
                "primary_id": crate::value::escape(&n.primary_id),
                "slots": n.slots,
                "slot_count": n.slots.iter().map(|(a, b)| u32::from(b - a) + 1).sum::<u32>(),
                "status": match n.status {
                    crate::model::NodeStatus::Scanned => "scanned",
                    crate::model::NodeStatus::Partial => "partial",
                    crate::model::NodeStatus::Failed => "failed",
                },
                "note": n.note.as_deref().map(crate::web::wording::server_free),
                "keys": n.server.as_ref().map(|s| s.total_keys),
                "memory": n.server.as_ref().map(|s| s.used_memory),
                "version": n.server.as_ref().map(|s| crate::value::escape(&s.version)),
                "sampled_keys": n.sampled_keys,
                "complete": n.complete,
                "sizes_available": n.sizes_available,
                "patterns": n.patterns.len(),
            })).collect::<Vec<_>>(),
            "empty": ns.is_empty().then_some("The cluster analysis has no shards to show."),
        }))
    });
    match body {
        Ok(Some(b)) => ok(b),
        Ok(None) => off(NODES_NOT_CLUSTER),
        Err(r) => r.into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SlotsQuery {
    /// `keys` (the default), `cpu`, `net_in` or `net_out`.
    order: Option<String>,
    /// Read again.
    #[serde(default)]
    refresh: Option<u8>,
}

pub fn order_of(name: &str) -> Option<Order> {
    Some(match name {
        "keys" => Order::Keys,
        "cpu" => Order::Cpu,
        "net_in" => Order::NetIn,
        "net_out" => Order::NetOut,
        _ => return None,
    })
}

pub const BAD_ORDER: &str = "the order must be one of keys, cpu, net_in or net_out";

/// `GET /api/analysis/slots[?order=<o>][&refresh=1]`: `CLUSTER SLOT-STATS` of every
/// node's slot ranges (at most four nodes at once), the hottest slots by `order`, and
/// the sampled keys in each. Read once per run. Off on a standalone server, on a
/// version without the command, or when it is refused.
pub async fn slots(State(app): State<App>, Q(q): Q<SlotsQuery>) -> Response {
    let order = match q.order.as_deref().map(order_of) {
        None => Order::Keys,
        Some(Some(o)) => o,
        Some(None) => return error(StatusCode::BAD_REQUEST, BAD_ORDER),
    };
    let todo = app.hub.with(|c| {
        if c.session.nodes.is_empty() {
            return Err(None);
        }
        if !c.live_ready() {
            return Err(Some(Refused::new(StatusCode::CONFLICT, super::NOT_LIVE)));
        }
        let fresh = q.refresh.is_none()
            && c.browse
                .slots
                .as_ref()
                .is_some_and(|(run, _)| *run == c.session.scan);
        Ok((!fresh).then(|| (c.id, c.session.scan, c.session.nodes.clone())))
    });
    let todo = match todo {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(None)) => return off(NOT_CLUSTER),
        Some(Err(Some(r))) => return r.into_response(),
        Some(Ok(t)) => t,
    };
    if let Some((conn, run, nodes)) = todo {
        let mut st = SlotsState::default();
        st.nodes = vec![NodeState::Idle; nodes.len()];
        let mut asks = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.slots.is_empty() {
                continue;
            }
            if n.connected {
                st.nodes[i] = NodeState::Waiting;
                asks.push((i, n.slots.clone()));
            } else {
                st.nodes[i] = NodeState::Failed("not connected".to_string());
            }
        }
        let hub = &app.hub;
        let answers: Vec<_> = futures::stream::iter(asks)
            .map(|(i, ranges)| async move {
                let req = LiveRequest::Slots(SlotsRequest::Stats { ranges });
                (i, hub.request(Some(conn), Dest::node(i), req).await)
            })
            .buffer_unordered(AT_ONCE)
            .collect()
            .await;
        for (i, a) in answers {
            match a {
                Ok(LiveReply::Slots(SlotsReply::Stats(stats))) if !stats.supported => {
                    let runs = format!(
                        "{} {}",
                        match stats.flavor {
                            ServerFlavor::Redis => "Redis",
                            ServerFlavor::Valkey => "Valkey",
                            ServerFlavor::Unknown => "a server",
                        },
                        stats.version
                    );
                    st.off.get_or_insert(Off::TooOld {
                        node: nodes[i].endpoint.to_string(),
                        runs,
                    });
                }
                Ok(LiveReply::Slots(SlotsReply::Stats(stats))) => {
                    st.nodes[i] = NodeState::Loaded(stats);
                }
                Ok(_) => st.nodes[i] = NodeState::Failed("unexpected reply".to_string()),
                Err(LiveError::Denied(_)) => {
                    st.off = Some(Off::Refused);
                }
                Err(e) => st.nodes[i] = NodeState::Failed(e.to_string()),
            }
        }
        if st.off.is_some() {
            for n in &mut st.nodes {
                if *n == NodeState::Waiting {
                    *n = NodeState::Idle;
                }
            }
        }
        app.hub.with_mut(|c| {
            if c.id == conn && c.session.scan == run {
                c.browse.slots = Some((run, st));
            }
        });
    }
    let body = app.hub.with_mut(|c| {
        let nodes = c.session.nodes.clone();
        let report = c.session.report.clone();
        let (_, st) = c.browse.slots.as_mut()?;
        if let Some(o) = &st.off {
            return Some(Err(crate::value::escape(&o.text())));
        }
        st.order = order;
        st.rank();
        Some(Ok(slots_body(st, &nodes, report.as_ref())))
    });
    match body.flatten() {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(why)) => off(&why),
        Some(Ok(b)) => ok(b),
    }
}

fn slots_body(
    st: &SlotsState,
    nodes: &[crate::live::nodes::NodeInfo],
    report: Option<&ScanReport>,
) -> serde_json::Value {
    let endpoint = |i: usize| {
        nodes
            .get(i)
            .map(|n| crate::value::escape(&n.endpoint.to_string()))
    };
    let ranges: Vec<serde_json::Value> = st
        .range_rows(nodes)
        .into_iter()
        .map(|r| {
            let (status, error, t) = match r.outcome {
                Outcome::Waiting => ("waiting", None, None),
                Outcome::Failed(e) => ("failed", Some(crate::web::wording::server_free(&e)), None),
                Outcome::Done(t) => ("done", None, Some(t)),
            };
            serde_json::json!({
                "node": r.node,
                "endpoint": endpoint(r.node),
                "start": r.start,
                "end": r.end,
                "status": status,
                "error": error,
                "keys": t.and_then(|t| t.keys),
                "cpu_usec": t.and_then(|t| t.cpu_usec),
                "net_in": t.and_then(|t| t.net_in),
                "net_out": t.and_then(|t| t.net_out),
                "memory": t.and_then(|t| t.memory),
            })
        })
        .collect();
    let hottest: Vec<serde_json::Value> = st
        .hottest
        .iter()
        .map(|h| {
            let keys: Vec<serde_json::Value> = report
                .map(|r| keys_in_slot(r, h.stat.slot))
                .unwrap_or_default()
                .into_iter()
                .map(|k| {
                    serde_json::json!({
                        "key": Name::of(&k.key),
                        "pattern": Name::of(&k.pattern),
                        "kinds": k.kinds,
                    })
                })
                .collect();
            serde_json::json!({
                "slot": h.stat.slot,
                "node": h.node,
                "endpoint": endpoint(h.node),
                "keys": h.stat.keys,
                "cpu_usec": h.stat.cpu_usec,
                "net_in": h.stat.net_in,
                "net_out": h.stat.net_out,
                "memory": h.stat.memory,
                "sampled_keys": keys,
            })
        })
        .collect();
    let zero = st.cpu_and_network_zero();
    serde_json::json!({
        "order": match st.order {
            Order::Keys => "keys",
            Order::Cpu => "cpu",
            Order::NetIn => "net_in",
            Order::NetOut => "net_out",
        },
        "ranges": ranges,
        "hottest": hottest,
        "memory_column": st.flavor() == Some(ServerFlavor::Redis),
        "cpu_and_network_zero": zero,
        "note": zero.then_some(NEEDS_ENABLED),
        "empty": st.hottest.is_empty().then(|| format!("No slot has a figure above 0 for {}.", st.order.label())),
    })
}

/// Overview's analysis part: shown once an analysis has a report and none runs, as the
/// TUI's Overview shows its "Things to look at" then. `null` otherwise.
pub fn overview_block(c: &Current) -> serde_json::Value {
    let s = &c.session;
    let Some(r) = s.report.as_ref().filter(|_| !c.analysis_running()) else {
        return serde_json::Value::Null;
    };
    let empty = HashMap::new();
    let facts = facts_of(c).map_or(&empty, |f| &f.facts);
    let findings: Vec<dto::Finding> = analysis_findings_of(r, s.diff(), facts, &c.slow_entries)
        .iter()
        .map(dto::Finding::of)
        .collect();
    let total: u64 = r
        .patterns
        .iter()
        .map(|p| {
            if r.sizes_available {
                p.est_bytes.unwrap_or(0)
            } else {
                p.est_keys
            }
        })
        .sum();
    let top: Vec<serde_json::Value> = r
        .patterns
        .iter()
        .take(5)
        .map(|p| {
            let w = if r.sizes_available {
                p.est_bytes.unwrap_or(0)
            } else {
                p.est_keys
            };
            serde_json::json!({
                "pattern": Name::of(&p.pattern),
                "est_keys": p.est_keys,
                "est_bytes": p.est_bytes,
                "share": if total == 0 { 0.0 } else { w as f64 / total as f64 },
            })
        })
        .collect();
    let expiry = crate::analysis::expiry(r);
    serde_json::json!({
        "taken_at_unix": r.taken_at_unix,
        "sampled_keys": r.sampled_keys,
        "patterns": r.patterns.len(),
        "complete": r.complete,
        "stopped": crate::tui::app::views::overview::stopped_early(s),
        "sizes_available": r.sizes_available,
        "findings": findings,
        "top_patterns": top,
        "never_expires_share": expiry.map(|e| e.never_share()),
        "changes": match &s.history {
            crate::tui::app::History::Pending => "pending",
            crate::tui::app::History::NoSnapshot => "none",
            crate::tui::app::History::Failed(_) => "failed",
            crate::tui::app::History::Diff(_) => "diff",
        },
        "note": r.note.as_deref().map(crate::web::wording::server_free),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depths_and_orders_are_named_both_ways() {
        for d in Depth::ALL {
            assert_eq!(depth_of(depth_name(d)), Some(d));
        }
        assert_eq!(depth_of("full"), None);
        for o in [Order::Keys, Order::Cpu, Order::NetIn, Order::NetOut] {
            let name = match o {
                Order::Keys => "keys",
                Order::Cpu => "cpu",
                Order::NetIn => "net_in",
                Order::NetOut => "net_out",
            };
            assert_eq!(order_of(name), Some(o));
        }
    }

    #[test]
    fn the_wording_has_no_tui_keys_and_reads_as_redis_lenss_own() {
        for t in [
            NO_REPORT,
            STOPPING,
            STOPPING_AGAIN,
            NODES_NOT_CLUSTER,
            NOT_CLUSTER,
            crate::web::session::STOPPED_EMPTY,
            crate::web::session::ANALYSIS_RUNS,
            crate::web::session::NO_ANALYSIS,
            crate::web::session::STILL_CONNECTING,
            crate::web::session::CONNECT_FAILED,
            crate::web::session::NOT_CONNECTED,
            "OBJECT FREQ was denied for this user.",
            "No sampled key has an LFU counter above 0 yet.",
            "No memory figures: MEMORY USAGE is not allowed for this user, or this analysis has none.",
            "No sizes: MEMORY USAGE is not allowed for this user, so keys can't be ranked by size.",
        ] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
        // A note, not an error: it goes out as it is.
        assert_eq!(crate::web::wording::banned(NEEDS_ENABLED), None);
        let too_old = Off::TooOld {
            node: "10.0.0.1:7000".into(),
            runs: "Redis 7.4.2".into(),
        };
        assert_eq!(
            crate::web::wording::server_free(&too_old.text()),
            too_old.text()
        );
        assert_eq!(
            crate::web::wording::server_free(&Off::Refused.text()),
            Off::Refused.text()
        );
    }

    #[test]
    fn an_estimate_says_its_time_in_the_tuis_words() {
        let e = Estimate {
            keys: 10_000,
            commands: 30_000,
            per_sec: Some(5_000),
            duration: Some(std::time::Duration::from_secs(6)),
            shards: 0,
            sizes: true,
            freq: false,
        };
        let d = EstimateDto::of(Depth::Quick, &e);
        assert_eq!(d.about, "about 6 s");
        assert_eq!(d.depth, "quick");
        assert_eq!(d.sample_limit, 10_000);
        assert_eq!(d.duration_secs, Some(6));
    }
}
