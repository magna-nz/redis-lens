//! Browse › Keys, the pattern tree (`GET /api/browse`, `POST /api/browse/more`), and the
//! database switch (`GET /api/dbs`, `POST /api/db`).
//!
//! The tree is the TUI's: from the analysis's report, from its partial results while it
//! runs, or, before any analysis, from key names alone ([`NamesState`]: `SCAN` pages of
//! 1,000 names through the budget, [`FIRST_PAGES`] of them the first time, one more per
//! `POST /api/browse/more`; on a cluster every primary in turn). Patterns are grouped
//! under their first segment ([`PatternTree`]) and sorted as the TUI sorts them.
//!
//! The TUI has no server-side glob filter on this tree (its `/` filters the rows on
//! screen), so neither has this route: the page filters the rows it has.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::{App, Body, Q, Refused, error, ok};
use crate::conn::ScanTarget;
use crate::diff::ReportDiff;
use crate::live::dbs::{Reply as DbsReply, Request as DbsRequest};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest, ReqId};
use crate::model::{PatternStats, TTL_BUCKET_LABELS};
use crate::tui::app::Phase;
use crate::tui::app::views::LiveState;
use crate::tui::app::views::dbs::{DENIED, DbList, OFF_CLUSTER, UNREAD};
use crate::tui::app::views::names::{FIRST_PAGES, NamesState};
use crate::tui::app::views::slots::SlotsState;
use crate::tui::tree::{PatternTree, Root, SortBy, summarize_group};
use crate::verdict::{PatternVerdict, pattern_verdict, ttl_verdict};
use crate::web::keyname::Name;
use crate::web::session::Current;

/// What Browse and Analyze keep of a session beside the TUI's [`crate::tui::app::Session`].
#[derive(Debug, Default)]
pub struct Browse {
    pub names: NamesState,
    pub lists: super::keys::Lists,
    pub values: super::values::Views,
    /// The databases `GET /api/dbs` read last.
    pub dbs: Option<Vec<u32>>,
    /// Big keys' lengths and encodings, for the report they were read for.
    pub facts: Option<super::analyze::Facts>,
    /// The report whose facts are being read, and when the read ends: a second
    /// request waits for it instead of reading them again.
    pub facts_reading: Option<((i64, u64), tokio::sync::watch::Receiver<bool>)>,
    /// Slots, for the run it was read in.
    pub slots: Option<(u64, SlotsState)>,
}

impl Browse {
    /// A new run started: a page of names in flight is asked again on the new live
    /// connection; a database switch starts everything afresh (another database's keys).
    /// The key lists' and values' numbers go on from where they were, so a number from
    /// before the switch never names a list or a value of the new database.
    pub fn new_run(&mut self, db_switch: bool) {
        self.names.lost();
        self.slots = None;
        if db_switch {
            self.lists.clear();
            self.values.restart();
            *self = Self {
                lists: std::mem::take(&mut self.lists),
                values: std::mem::take(&mut self.values),
                ..Self::default()
            };
        }
    }
}

/// The databases' wording, without the TUI's keys.
pub const DBS_NOT_READY: &str =
    "The connection is not ready yet: wait for it, then pick a database.";
pub const DBS_ANALYZING: &str =
    "An analysis is running: wait for it to finish, then pick a database.";
pub const DBS_CLOSED: &str = "The live connection is closed: analyze again first.";
pub const DB_NOT_LISTED: &str =
    "Pick a database the list shows (read the list first): it shows the databases with keys.";
pub const SWITCHED: &str = "for this session. The saved profile is unchanged.";

/// Why the tree is empty.
pub const NOT_READY: &str = "Key names are read once the live connection is open.";
pub const STARTING: &str = "An analysis is starting: its patterns come once it samples keys.";
pub const FROM_ANALYSIS: &str =
    "These patterns come from the analysis: analyze again to read them anew.";
pub const ANALYSIS_RUNS: &str = "An analysis is running: its patterns replace the key names.";

#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    /// `memory` (the default), `keys`, `avg_size`, `no_ttl` or `name`.
    sort: Option<String>,
    /// A report node (`nodes[i].index`) to narrow the tree to, after a cluster analysis.
    node: Option<usize>,
}

pub fn sort_of(name: &str) -> Option<SortBy> {
    Some(match name {
        "memory" => SortBy::Memory,
        "keys" => SortBy::Keys,
        "avg_size" => SortBy::AvgSize,
        "no_ttl" => SortBy::NoTtl,
        "name" => SortBy::Name,
        _ => return None,
    })
}

pub fn sort_name(s: SortBy) -> &'static str {
    match s {
        SortBy::Memory => "memory",
        SortBy::Keys => "keys",
        SortBy::AvgSize => "avg_size",
        SortBy::NoTtl => "no_ttl",
        SortBy::Name => "name",
    }
}

pub const BAD_SORT: &str = "the sort must be one of memory, keys, avg_size, no_ttl or name";
pub const BAD_NODE: &str = "That node is not one of this analysis's nodes.";

/// `GET /api/browse[?sort=<s>][&node=<i>]`: the pattern tree. Before an analysis it
/// reads the first pages of key names when none were read yet.
pub async fn tree(State(app): State<App>, Q(q): Q<TreeQuery>) -> Response {
    let sort = match q.sort.as_deref().map(sort_of) {
        Some(None) => return error(StatusCode::BAD_REQUEST, BAD_SORT),
        Some(Some(s)) => s,
        None => SortBy::Memory,
    };
    read_names(&app).await;
    match app.hub.with(|c| body(c, sort, q.node)) {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => r.into_response(),
        Some(Ok(b)) => ok(b),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct MoreBody {
    sort: Option<String>,
}

/// `POST /api/browse/more`: one more page of key names (1,000), as `n` on Browse ›
/// Keys. Refused (409) once an analysis's patterns are shown, or when every name is
/// read.
pub async fn more(State(app): State<App>, Body(b): Body<MoreBody>) -> Response {
    let sort = match b.sort.as_deref().map(sort_of) {
        Some(None) => return error(StatusCode::BAD_REQUEST, BAD_SORT),
        Some(Some(s)) => s,
        None => SortBy::Memory,
    };
    let asked = app.hub.with_mut(|c| {
        if c.session.report.is_some() || c.session.partial.is_some() {
            return Err(if c.analysis_running() {
                ANALYSIS_RUNS.to_string()
            } else {
                FROM_ANALYSIS.to_string()
            });
        }
        if c.analysis_running() {
            return Err(ANALYSIS_RUNS.to_string());
        }
        c.browse.names.more()
    });
    match asked {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(why)) => return error(StatusCode::CONFLICT, &why),
        Some(Ok(())) => {}
    }
    read_names(&app).await;
    match app.hub.with(|c| body(c, sort, None)) {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => r.into_response(),
        Some(Ok(b)) => ok(b),
    }
}

/// Reads the pages of key names that are due, one at a time, as the TUI's
/// `read_names` does: only once the live connection is ready, with no analysis
/// running and no report to show.
///
/// The reading runs in a task of its own, awaited here: when the request that started
/// it goes (the browser closed it), the page in flight is still answered into the
/// state, so the names never stay "reading".
pub async fn read_names(app: &App) {
    let app = app.clone();
    let _ = tokio::spawn(async move { read_names_now(&app).await }).await;
}

async fn read_names_now(app: &App) {
    loop {
        let step = app
            .hub
            .with_mut(|c| {
                let s = &c.session;
                if !c.live_ready()
                    || s.report.is_some()
                    || s.partial.is_some()
                    || matches!(s.phase, Phase::Scanning | Phase::Connecting)
                {
                    return None;
                }
                if !c.browse.names.started() {
                    let dbsize = crate::tui::app::names_dbsize(s);
                    let nodes = s.nodes.clone();
                    c.browse.names.start(&nodes, dbsize);
                }
                let ask = c.browse.names.next_page()?;
                // One page at a time: the id is not used to match the answer.
                c.browse.names.sent(Some(ReqId(0)), ask.clone());
                Some((c.id, c.session.scan, ask))
            })
            .flatten();
        let Some((conn, scan, ask)) = step else {
            return;
        };
        let result = app.hub.request(Some(conn), ask.dest, ask.req).await;
        let taken = app.hub.with_mut(|c| {
            let same = c.id == conn && c.session.scan == scan;
            if same {
                c.browse.names.reply(result);
            }
            same
        });
        if taken != Some(true) {
            return;
        }
    }
}

/// A pattern's change since the last snapshot.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Change {
    pub delta_bytes: Option<i64>,
    pub delta_keys: i64,
    /// `new`, `gone`, `more-keys`, … (`diff::Cause`).
    pub cause: crate::diff::Cause,
    pub cause_words: &'static str,
    /// New, or grown by 300 MiB or more: the TUI shows it in red.
    pub alarming: bool,
}

/// Growth of at least this many bytes is alarming (the TUI's `BIG_GROWTH`).
pub const BIG_GROWTH: i64 = 300 * 1024 * 1024;

impl Change {
    pub fn of(p: &crate::diff::PatternDiff) -> Self {
        let bytes = p.delta_bytes;
        Self {
            delta_bytes: bytes,
            delta_keys: p.delta_keys,
            cause: p.cause,
            cause_words: p.cause.words(),
            alarming: p.cause == crate::diff::Cause::New || bytes.is_some_and(|b| b >= BIG_GROWTH),
        }
    }
}

/// A sampled key, as an analysis keeps it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sample {
    #[serde(flatten)]
    pub key: Name,
    #[serde(rename = "type")]
    pub key_type: &'static str,
    pub bytes: Option<u64>,
    pub ttl: String,
    pub ttl_secs: Option<u64>,
    pub freq: Option<u8>,
    /// The report node it was sampled on (a cluster).
    pub node: Option<u16>,
}

impl Sample {
    pub fn of(k: &crate::model::KeySample) -> Self {
        Self {
            key: Name::of(&k.key),
            key_type: k.key_type.as_str(),
            bytes: k.bytes,
            ttl: super::keys::ttl_text(k.ttl),
            ttl_secs: super::keys::ttl_secs(k.ttl),
            freq: k.lfu_freq,
            node: k.node,
        }
    }
}

/// One pattern row.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PatternRow {
    pub kind: &'static str,
    pub pattern: Name,
    /// `None`: the sampled keys' types differ (`mixed`), or unknown (from names).
    #[serde(rename = "type")]
    pub key_type: Option<&'static str>,
    pub est_keys: u64,
    pub est_bytes: Option<u64>,
    pub sampled_keys: u64,
    pub sampled_bytes: Option<u64>,
    /// Of the tree's total: memory with sizes, else keys.
    pub share: f64,
    pub avg_bytes: Option<u64>,
    /// Unknown (`null`) for rows from key names.
    pub no_ttl: Option<u64>,
    pub no_ttl_ratio: Option<f64>,
    pub ttl_hist: Option<[u64; 7]>,
    pub size_p50: Option<u64>,
    pub size_p99: Option<u64>,
    pub size_max: Option<u64>,
    /// `leak?`, `growing`, `mixed`, `big key`, `persistent` or `ok`; `null` from names.
    pub verdict: Option<&'static str>,
    /// The TUI marks the row (`▲`).
    pub marked: bool,
    pub warning: bool,
    pub ttl_verdict: Option<&'static str>,
    /// Why it is marked, or what its TTLs say: the TUI's sentence.
    pub reason: Option<String>,
    pub change: Option<Change>,
    pub examples: Vec<Sample>,
    /// The `redis-cli` command that lists its keys, when its glob is safe to paste.
    pub command: Option<String>,
    /// From names: the pattern names one key (its only name is the pattern itself).
    pub literal: Option<bool>,
    /// From names: the distinct names seen.
    pub names_seen: Option<u64>,
}

/// A group row: patterns that share a first segment.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupRow {
    pub kind: &'static str,
    pub label: String,
    pub key: String,
    #[serde(rename = "type")]
    pub key_type: Option<&'static str>,
    pub est_keys: u64,
    pub est_bytes: Option<u64>,
    pub sampled_keys: u64,
    pub share: f64,
    pub no_ttl: Option<u64>,
    pub no_ttl_ratio: Option<f64>,
    /// The most worrying of its children's verdicts.
    pub verdict: Option<&'static str>,
    pub marked: bool,
    pub reason: String,
    pub children: Vec<PatternRow>,
}

fn marked(v: PatternVerdict) -> bool {
    !matches!(v, PatternVerdict::Ok | PatternVerdict::Persistent)
}

/// The TUI's reason for a pattern row (`ui/patterns.rs`), without its `▲`.
pub fn reason(p: &PatternStats, v: PatternVerdict, diff: Option<&ReportDiff>) -> String {
    let change = diff.and_then(|d| d.pattern(&p.pattern));
    let said = crate::verdict::explain(p, change).join(" ");
    match v {
        PatternVerdict::Leak => {
            "Lock keys that never expire: a holder that crashed never lets go of them.".to_string()
        }
        PatternVerdict::BigKey => format!(
            "A sampled key is {}. A big key blocks the server while it is read, copied or deleted.",
            p.size_max
                .map_or_else(|| "very large".to_string(), crate::format::bytes)
        ),
        _ => said,
    }
}

struct Ctx<'a> {
    patterns: &'a [PatternStats],
    sizes: bool,
    names: Option<&'a NamesState>,
    diff: Option<&'a ReportDiff>,
    total: u64,
}

fn weight(p: &PatternStats, sizes: bool) -> u64 {
    if sizes {
        p.est_bytes.unwrap_or(0)
    } else {
        p.est_keys
    }
}

fn share(w: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        w as f64 / total as f64
    }
}

fn pattern_row(x: &Ctx<'_>, i: usize) -> PatternRow {
    let p = &x.patterns[i];
    let change = x.diff.and_then(|d| d.pattern(&p.pattern));
    let named = x.names.is_some();
    let v = (!named).then(|| pattern_verdict(p, change));
    let avg = match p.sampled_bytes {
        Some(b) if p.sampled_keys > 0 => Some(b / p.sampled_keys),
        _ => None,
    };
    PatternRow {
        kind: "pattern",
        pattern: Name::of(&p.pattern),
        key_type: if named {
            None
        } else {
            p.key_type.map(|t| t.as_str())
        },
        est_keys: p.est_keys,
        est_bytes: p.est_bytes,
        sampled_keys: p.sampled_keys,
        sampled_bytes: p.sampled_bytes,
        share: share(weight(p, x.sizes), x.total),
        avg_bytes: avg,
        no_ttl: (!named).then_some(p.no_ttl),
        no_ttl_ratio: (!named).then(|| p.no_ttl_ratio()),
        ttl_hist: (!named).then_some(p.ttl_hist),
        size_p50: p.size_p50,
        size_p99: p.size_p99,
        size_max: p.size_max,
        verdict: v.map(PatternVerdict::label),
        marked: v.is_some_and(marked),
        warning: v.is_some_and(PatternVerdict::is_warning),
        ttl_verdict: (!named).then(|| ttl_verdict(p, change).label()),
        reason: v.map(|v| crate::value::escape(&reason(p, v, x.diff))),
        change: change.map(Change::of),
        examples: p.examples.iter().map(Sample::of).collect(),
        command: crate::tui::clipboard::suggested_command(&p.pattern),
        literal: x.names.map(|n| n.is_literal(&p.pattern)),
        names_seen: x.names.map(|n| n.seen_of(&p.pattern)),
    }
}

fn rows(x: &Ctx<'_>, sort: SortBy) -> Vec<serde_json::Value> {
    let tree = PatternTree::build_sorted(x.patterns, x.sizes, sort);
    tree.roots
        .iter()
        .map(|root| match root {
            Root::Single(i) => serde_json::to_value(pattern_row(x, *i)),
            Root::Group(g) => {
                let s = summarize_group(g, x.patterns);
                let children: Vec<PatternRow> =
                    g.children.iter().map(|&i| pattern_row(x, i)).collect();
                let v = (x.names.is_none()).then(|| {
                    g.children
                        .iter()
                        .map(|&i| {
                            let p = &x.patterns[i];
                            pattern_verdict(p, x.diff.and_then(|d| d.pattern(&p.pattern)))
                        })
                        .min()
                        .unwrap_or(PatternVerdict::Ok)
                });
                let n = g.children.len();
                let flagged = v.is_some_and(marked);
                let w: u64 = g
                    .children
                    .iter()
                    .map(|&i| weight(&x.patterns[i], x.sizes))
                    .sum();
                serde_json::to_value(GroupRow {
                    kind: "group",
                    label: crate::value::escape(&g.label),
                    key: crate::value::escape(&g.key),
                    key_type: if x.names.is_some() {
                        None
                    } else {
                        s.key_type.map(|t| t.as_str())
                    },
                    est_keys: s.est_keys,
                    est_bytes: s.est_bytes,
                    sampled_keys: s.sampled_keys,
                    share: share(w, x.total),
                    no_ttl: x.names.is_none().then_some(s.no_ttl),
                    no_ttl_ratio: x.names.is_none().then(|| s.no_ttl_ratio()),
                    verdict: v.map(PatternVerdict::label),
                    marked: flagged,
                    reason: if flagged {
                        format!("Some of its {n} patterns have something to look at.")
                    } else {
                        format!("{n} patterns, nothing marked.")
                    },
                    children,
                })
            }
        })
        .map(|v| v.unwrap_or(serde_json::Value::Null))
        .collect()
}

/// The body of `GET /api/browse`.
fn body(c: &Current, sort: SortBy, node: Option<usize>) -> Result<serde_json::Value, Refused> {
    let s = &c.session;
    let diff = s.diff();
    let node_rows: Vec<serde_json::Value> = s
        .report
        .as_ref()
        .and_then(|r| r.nodes.as_ref())
        .map(|ns| {
            ns.iter()
                .enumerate()
                .map(|(i, n)| {
                    serde_json::json!({
                        "index": i,
                        "endpoint": crate::value::escape(&n.endpoint.to_string()),
                        "role": crate::web::dto::role_name(n.role),
                        "status": match n.status {
                            crate::model::NodeStatus::Scanned => "scanned",
                            crate::model::NodeStatus::Partial => "partial",
                            crate::model::NodeStatus::Failed => "failed",
                        },
                        "sampled_keys": n.sampled_keys,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let (source, patterns, sizes, names): (&str, &[PatternStats], bool, Option<&NamesState>) =
        if let Some(r) = &s.report {
            match node {
                Some(i) => {
                    let n =
                        r.nodes.as_ref().and_then(|ns| ns.get(i)).ok_or_else(|| {
                            Refused::new(StatusCode::UNPROCESSABLE_ENTITY, BAD_NODE)
                        })?;
                    ("analysis", &n.patterns, n.sizes_available, None)
                }
                None => ("analysis", &r.patterns, r.sizes_available, None),
            }
        } else if let Some(p) = s.partial.as_deref() {
            ("partial", &p.patterns, p.sizes_available, None)
        } else if c.browse.names.started() {
            ("names", &c.browse.names.stats, false, Some(&c.browse.names))
        } else {
            ("none", &[], false, None)
        };
    let total: u64 = patterns.iter().map(|p| weight(p, sizes)).sum();
    let x = Ctx {
        patterns,
        sizes,
        names,
        diff: if source == "analysis" { diff } else { None },
        total,
    };
    // From names only keys and name sort, as in the TUI.
    let sort = if names.is_some() && !matches!(sort, SortBy::Keys | SortBy::Name) {
        SortBy::Keys
    } else {
        sort.effective(sizes)
    };
    let why = match source {
        "none" if c.analysis_running() => Some(STARTING),
        "none" if !c.live_ready() => Some(NOT_READY),
        _ => None,
    };
    let n = &c.browse.names;
    let endpoint = |i: &usize| {
        s.nodes
            .get(*i)
            .map(|n| crate::value::escape(&n.endpoint.to_string()))
    };
    Ok(serde_json::json!({
        "source": source,
        "why": why,
        "sort": sort_name(sort),
        "sorts": if names.is_some() {
            vec!["keys", "name"]
        } else if sizes {
            vec!["memory", "keys", "avg_size", "no_ttl", "name"]
        } else {
            vec!["keys", "no_ttl", "name"]
        },
        "sizes": sizes,
        // The node the rows are narrowed to: only an analysis's rows can be.
        "node": node.filter(|_| source == "analysis"),
        "nodes": node_rows,
        "previous": s.previous.is_some() && s.report.is_some(),
        "partial": s.partial.as_deref().filter(|_| source == "partial").map(|p| serde_json::json!({
            "sampled": p.sampled,
            "target": p.target,
        })),
        "names": (source == "names").then(|| serde_json::json!({
            "pages": n.pages,
            "first_pages": FIRST_PAGES,
            "names_read": n.names_read(),
            "dbsize": n.dbsize(),
            "complete": n.complete(),
            "done": n.done(),
            "reading": n.is_reading(),
            "error": n.error.as_deref().map(crate::web::wording::server_free),
            "refused": n.refused.iter().filter_map(endpoint).collect::<Vec<_>>(),
            "skipped": n.skipped.iter().filter_map(endpoint).collect::<Vec<_>>(),
            "missing": n.missing().iter().filter_map(endpoint).collect::<Vec<_>>(),
            "nodes_done": n.nodes_done(),
        })),
        "total": total,
        "ttl_labels": TTL_BUCKET_LABELS,
        "rows": rows(&x, sort),
    }))
}

/// `GET /api/dbs`: the databases with their key counts (`INFO keyspace` on the picked
/// node), the current one always listed. Off on a cluster, which has only database 0.
pub async fn dbs(State(app): State<App>) -> Response {
    let ready = app.hub.with(|c| {
        let s = &c.session;
        if s.is_cluster() || matches!(s.target, Some(ScanTarget::Cluster(_))) {
            return Err(None);
        }
        let why = match (&s.phase, &s.live) {
            (Phase::Connecting, _) => Some(if c.analysis_running() {
                DBS_ANALYZING
            } else {
                DBS_NOT_READY
            }),
            (Phase::Scanning, _) => Some(DBS_ANALYZING),
            (Phase::Failed(_), _) => Some(crate::web::session::CONNECT_FAILED),
            (_, LiveState::Waiting) => Some(DBS_NOT_READY),
            (_, LiveState::Closed(_)) => Some(DBS_CLOSED),
            _ => None,
        };
        match why {
            Some(w) => Err(Some(w)),
            None => Ok((
                c.id,
                s.picked,
                s.profile.db,
                (!c.docker).then(|| s.profile.name.clone()),
                s.profile.env == crate::model::Env::Prod,
            )),
        }
    });
    let (conn, picked, current, saved_as, prod) = match ready {
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(None)) => return super::off(OFF_CLUSTER),
        Some(Err(Some(why))) => return error(StatusCode::CONFLICT, why),
        Some(Ok(r)) => r,
    };
    let mut list = DbList::loading(current, saved_as.clone(), prod, ReqId(0));
    let req = LiveRequest::Dbs(DbsRequest::Keyspace);
    match app.hub.request(Some(conn), Dest::node(picked), req).await {
        Ok(LiveReply::Dbs(DbsReply::Keyspace(infos))) => list.fill(&infos),
        Err(LiveError::Denied(_)) => list.unread(DENIED),
        Err(LiveError::Closed) => return error(StatusCode::CONFLICT, super::NOT_LIVE),
        Ok(_) | Err(_) => list.unread(UNREAD),
    }
    let listed: Vec<u32> = list.rows.iter().map(|r| r.db).collect();
    app.hub.with_mut(|c| {
        if c.id == conn {
            c.browse.dbs = Some(listed);
        }
    });
    let unread = match list.stage {
        crate::tui::app::views::dbs::Stage::Unread(why) => Some(why),
        _ => None,
    };
    ok(serde_json::json!({
        "current": current,
        "rows": list.rows.iter().map(|r| serde_json::json!({
            "db": r.db,
            "keys": r.counts.map(|c| c.0),
            "expires": r.counts.map(|c| c.1),
            "current": r.db == current,
        })).collect::<Vec<_>>(),
        "unread": unread,
        "saved_as": saved_as.as_deref().map(crate::value::escape),
        "note": format!("Picking a database is {SWITCHED}"),
    }))
}

#[derive(Debug, Deserialize)]
pub struct DbBody {
    db: u32,
}

/// `POST /api/db {"db": n}`: switches the session to database `n`, as `enter` on the
/// TUI's database list: a new run that reads no key (the earlier results go; Browse
/// reads the new database's names), the live connection opened again on it, a running
/// tail subscribed again, a running Monitor stopped. For this session only: the saved
/// profile is not changed. 202 with the run; 200 when it is the current database.
pub async fn switch(State(app): State<App>, Body(b): Body<DbBody>) -> Response {
    let check = app.hub.with(|c| {
        let s = &c.session;
        if s.is_cluster() || matches!(s.target, Some(ScanTarget::Cluster(_))) {
            return Err(Refused::new(StatusCode::CONFLICT, OFF_CLUSTER));
        }
        if c.analysis_running() {
            return Err(Refused::new(StatusCode::CONFLICT, DBS_ANALYZING));
        }
        if b.db == s.profile.db {
            return Ok(false);
        }
        if !c.browse.dbs.as_ref().is_some_and(|l| l.contains(&b.db)) {
            return Err(Refused::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                DB_NOT_LISTED,
            ));
        }
        Ok(true)
    });
    match check {
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
        Some(Err(r)) => r.into_response(),
        Some(Ok(false)) => {
            ok(serde_json::json!({ "db": b.db, "switched": false, "run": null, "text": null }))
        }
        Some(Ok(true)) => match app.hub.new_run(None, Some(b.db)) {
            Ok(run) => (
                StatusCode::ACCEPTED,
                axum::Json(serde_json::json!({
                    "db": b.db,
                    "switched": true,
                    "run": run,
                    "text": format!("Switched to db {} {SWITCHED}", b.db),
                })),
            )
                .into_response(),
            Err(r) => r.into_response(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{KeySample, KeyType, Ttl};

    fn stats(pattern: &str, keys: u64, no_ttl: u64, bytes: Option<u64>) -> PatternStats {
        PatternStats {
            pattern: pattern.into(),
            key_type: Some(KeyType::String),
            sampled_keys: keys,
            est_keys: keys * 10,
            sampled_bytes: bytes,
            est_bytes: bytes.map(|b| b * 10),
            no_ttl,
            ttl_hist: [no_ttl, 0, 0, keys - no_ttl, 0, 0, 0],
            size_p50: None,
            size_p99: None,
            size_max: bytes,
            examples: vec![KeySample {
                key: format!("{}\x1b", pattern.replace("{id}", "1")),
                key_type: KeyType::String,
                bytes,
                ttl: Ttl::Persistent,
                lfu_freq: None,
                node: None,
            }],
        }
    }

    #[test]
    fn sorts_are_named_both_ways() {
        for s in [
            SortBy::Memory,
            SortBy::Keys,
            SortBy::AvgSize,
            SortBy::NoTtl,
            SortBy::Name,
        ] {
            assert_eq!(sort_of(sort_name(s)), Some(s));
        }
        assert_eq!(sort_of("size"), None);
    }

    #[test]
    fn rows_group_by_first_segment_with_the_tuis_verdicts_and_reasons() {
        let patterns = vec![
            stats("lock:{id}", 10, 10, Some(100)),
            stats("user:{id}:cart", 20, 10, Some(2_000)),
            stats("user:{id}:name", 20, 0, Some(1_000)),
        ];
        let total = patterns.iter().map(|p| weight(p, true)).sum();
        let x = Ctx {
            patterns: &patterns,
            sizes: true,
            names: None,
            diff: None,
            total,
        };
        let rows = rows(&x, SortBy::Memory);
        assert_eq!(rows.len(), 2);
        let group = &rows[0];
        assert_eq!(group["kind"], "group");
        assert_eq!(group["label"], "user:{id}:*");
        assert_eq!(group["verdict"], "mixed");
        assert_eq!(group["marked"], true);
        assert_eq!(group["children"][0]["pattern"]["name"], "user:{id}:cart");
        let lock = &rows[1];
        assert_eq!(lock["verdict"], "leak?");
        assert_eq!(
            lock["reason"],
            "Lock keys that never expire: a holder that crashed never lets go of them."
        );
        // An example key's control character never reaches the page.
        assert_eq!(lock["examples"][0]["name"], "lock:1\\x1b");
        assert_eq!(lock["examples"][0]["copy"], serde_json::Value::Null);
        assert_eq!(lock["command"], "redis-cli --scan --pattern \"lock:*\"");
    }

    #[test]
    fn a_new_pattern_is_alarming() {
        let d = crate::diff::PatternDiff {
            pattern: "p".into(),
            keys_before: None,
            keys_after: Some(3),
            bytes_before: None,
            bytes_after: Some(10),
            delta_bytes: Some(10),
            delta_keys: 3,
            cause: crate::diff::Cause::New,
            partial: false,
            sampled_keys_after: 3,
            pattern_sampled_keys_after: 3,
        };
        let c = Change::of(&d);
        assert!(c.alarming);
        assert_eq!(c.cause_words, "new");
        assert_eq!(serde_json::to_value(&c).unwrap()["cause"], "new");
    }

    #[test]
    fn the_wording_has_no_tui_keys() {
        for t in [
            DBS_NOT_READY,
            DBS_ANALYZING,
            DBS_CLOSED,
            DB_NOT_LISTED,
            SWITCHED,
            NOT_READY,
            STARTING,
            FROM_ANALYSIS,
            ANALYSIS_RUNS,
            OFF_CLUSTER,
            DENIED,
            UNREAD,
            BAD_NODE,
        ] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
    }
}
