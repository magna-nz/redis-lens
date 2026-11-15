//! Analyze › Hot keys from Monitor (spec "Analyze › Hot keys", "From MONITOR (P2)"): the
//! TUI's second Hot keys ranking, from the last Monitor run of the session.
//!
//! A run is phase 1's Monitor (`POST /api/server/monitor`: it always asks, the profile's
//! name typed on `prod`, it stops after 30 s or 10,000 lines). While it runs, the session
//! keeps its lines as the TUI's Monitor panel does ([`Kept`]): already masked
//! (`live::monitor::read_line`), at most `MAX_LINES`, in memory only. When it ends the
//! keys are placed with the key specs the Slow log uses (one cache per session,
//! `Current::slow_specs`): the commands not in it are asked with `COMMAND INFO` through
//! the command budget, at most 64 names a request and [`MAX_LOOKUPS`] requests, then the
//! TUI's own [`monitor::rank`] ranks them. A run ended by a new run (a database switch,
//! an analysis) or by the live connection closing is ranked with what the cache knows,
//! as the TUI ranks a run its rescan ended.
//!
//! The ranking is never saved: it is not part of the analysis, so no snapshot holds it.
//! `GET /api/analysis/hot_keys/monitor` shows it while the analysis has no hot keys.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::{App, Refused, ok};
use crate::live::monitor::{self, Entry, MAX_LINES, MAX_TIME, Ranking};
use crate::live::server::{Reply as ServerReply, Request as ServerRequest, names_to_learn};
use crate::live::{Dest, LiveError, LiveReply, LiveRequest};
use crate::tui::app::ConnId;
use crate::tui::app::views::server::{MAX_LOOKUPS, MonitorHot};
use crate::web::keyname::Name;
use crate::web::session::{Current, Hub};

/// The lines of the session's last Monitor run, as the TUI's Monitor panel keeps them.
/// Values: in memory only, never logged; `Debug` shows how many there are.
pub struct Kept {
    pub run: u64,
    pub node: usize,
    /// When its connection answered (lines started to come), and in Unix seconds.
    started: Option<Instant>,
    started_unix: Option<i64>,
    lines: Vec<Entry>,
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kept")
            .field("run", &self.run)
            .field("lines", &self.lines.len())
            .finish_non_exhaustive()
    }
}

/// A ranking made, and the node its run read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub hot: MonitorHot,
    pub node: usize,
}

/// Where the ranking of the last ended run is.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Rank {
    #[default]
    None,
    /// The run ended; its keys are being placed (`COMMAND INFO`, when `lookup`).
    Pending {
        lookup: bool,
    },
    Done(Ranked),
}

/// The Monitor ranking of a connection: the run whose lines are kept, and the ranking
/// of the last one that ended.
#[derive(Debug, Default)]
pub struct Hot {
    pub kept: Option<Kept>,
    pub rank: Rank,
    /// The run whose keys are placed, and when it ended.
    ended: Option<(Kept, Instant)>,
    /// A task places the keys now.
    looking: bool,
    /// A task is to be started for this connection, once the session's lock is let go.
    pub due: Option<ConnId>,
}

impl Hot {
    /// Run `run` starts on node `node`: its lines are kept from now on. The last ranking
    /// stays until this run ends, as in the TUI.
    pub fn begin(&mut self, run: u64, node: usize) {
        self.kept = Some(Kept {
            run,
            node,
            started: None,
            started_unix: None,
            lines: Vec::new(),
        });
    }

    /// Run `run`'s connection answered at `at_unix_ms` (the server's clock).
    pub fn ready(&mut self, run: u64, at_unix_ms: i64) {
        if let Some(k) = self.kept.as_mut().filter(|k| k.run == run) {
            k.started = Some(Instant::now());
            k.started_unix = Some(at_unix_ms.div_euclid(1000));
        }
    }

    /// Lines of run `run`, at most [`MAX_LINES`] kept.
    pub fn lines(&mut self, run: u64, lines: &[Entry]) {
        if let Some(k) = self.kept.as_mut().filter(|k| k.run == run) {
            let room = MAX_LINES.saturating_sub(k.lines.len());
            k.lines.extend(lines.iter().take(room).cloned());
        }
    }

    /// Run `run` ended. A run that never started keeps the last ranking. Otherwise the
    /// ranking is pending: true when the keys are to be placed with `COMMAND INFO` first
    /// (`lookup`, and a task is to be started for it), false when it was ranked now or
    /// needs nothing more.
    pub fn end(&mut self, run: u64, lookup: bool, specs: &SpecsNow<'_>) -> bool {
        let Some(k) = self.kept.take_if(|k| k.run == run) else {
            return false;
        };
        if k.started.is_none() {
            return false;
        }
        self.ended = Some((k, Instant::now()));
        self.rank = Rank::Pending { lookup };
        if lookup {
            return !self.looking;
        }
        self.rank_now(specs);
        false
    }

    /// Ranks the ended run with what the cache knows now.
    fn rank_now(&mut self, specs: &SpecsNow<'_>) {
        let Some((k, ended)) = self.ended.take() else {
            return;
        };
        let ranking = monitor::rank(&k.lines, specs.specs);
        let keys_note = (ranking.unknown > 0)
            .then(|| {
                specs
                    .keys_off
                    .map(|why| format!("Key names are off: {why}."))
            })
            .flatten();
        let secs = k.started.map_or(0, |s| {
            ended.saturating_duration_since(s).min(MAX_TIME).as_secs()
        });
        self.rank = Rank::Done(Ranked {
            hot: MonitorHot {
                ranking,
                secs,
                at_unix: k.started_unix.unwrap_or(0),
                keys_note,
            },
            node: k.node,
        });
    }

    /// The names the ended run's commands need learned, and the node it read.
    fn to_learn(&self, specs: &SpecsNow<'_>) -> Option<(Vec<String>, usize)> {
        let (k, _) = self.ended.as_ref()?;
        let names = names_to_learn(specs.specs, k.lines.iter().map(|e| e.line.args.as_slice()));
        Some((names, k.node))
    }
}

/// What the session knows about key positions now: the Slow log's cache, and why key
/// names are off when `COMMAND INFO` was refused.
pub struct SpecsNow<'a> {
    pub specs: &'a std::collections::HashMap<String, crate::live::server::CommandKeys>,
    pub keys_off: Option<&'a str>,
}

impl<'a> SpecsNow<'a> {
    pub fn of(c: &'a Current) -> Self {
        Self {
            specs: &c.slow_specs,
            keys_off: c.slow_keys_off.as_deref(),
        }
    }
}

/// Ends run `run` of `c` (the hooks in the session call it): the ranking is made now, or
/// the returned connection is to have its keys placed by [`settle`].
pub fn end_run(c: &mut Current, run: u64, lookup: bool) -> Option<ConnId> {
    let lookup = lookup && c.slow_keys && c.live_ready();
    let now = SpecsNow {
        specs: &c.slow_specs,
        keys_off: c.slow_keys_off.as_deref(),
    };
    let start = c.hot.end(run, lookup, &now);
    if start {
        c.hot.looking = true;
    }
    start.then_some(c.id)
}

/// Places the keys of the ended run of connection `conn` as the TUI does once a run
/// ended (`monitor_lookup`): `COMMAND INFO` for the names the cache lacks, through the
/// budget, until none is missing, key names are off, or [`MAX_LOOKUPS`] were asked;
/// then ranks them.
pub async fn settle(hub: Arc<Hub>, conn: ConnId) {
    for round in 0..=MAX_LOOKUPS {
        let next = hub
            .with_mut(|c| {
                if c.id != conn {
                    return None;
                }
                let Some((names, node)) = c.hot.to_learn(&SpecsNow::of(c)) else {
                    // Ranked meanwhile (a new run ranks at once).
                    c.hot.looking = false;
                    return None;
                };
                if names.is_empty() || !c.slow_keys || round >= MAX_LOOKUPS || !c.live_ready() {
                    rank_now(c);
                    return None;
                }
                Some((names, node))
            })
            .flatten();
        let Some((names, node)) = next else {
            return;
        };
        let req = LiveRequest::Server(ServerRequest::CommandKeys { names });
        let result = hub.request(Some(conn), Dest::node(node), req).await;
        let again = hub
            .with_mut(|c| {
                if c.id != conn {
                    return false;
                }
                match result {
                    Ok(LiveReply::Server(ServerReply::CommandKeys(learned))) => {
                        c.slow_specs.extend(learned);
                        true
                    }
                    Ok(_) => true,
                    // As the TUI: a refusal turns key names off for the session; any
                    // failure ranks with what is known.
                    Err(e) => {
                        if matches!(e, LiveError::Denied(_) | LiveError::Unsupported(_)) {
                            c.slow_keys = false;
                            c.slow_keys_off =
                                Some(crate::web::wording::server_free(&e.to_string()));
                        }
                        rank_now(c);
                        false
                    }
                }
            })
            .unwrap_or(false);
        if !again {
            return;
        }
    }
}

/// Ranks the ended run of `c` with what its cache knows now; the lookup is over.
fn rank_now(c: &mut Current) {
    c.hot.rank_now(&SpecsNow {
        specs: &c.slow_specs,
        keys_off: c.slow_keys_off.as_deref(),
    });
    c.hot.looking = false;
}

/// `once`, `twice`, `3 times`, as the TUI says it.
pub fn times(n: u64) -> String {
    crate::tui::ui::hot::times(n)
}

/// What the panel says of the ranking's source, as the TUI's right pane does.
pub const SOURCE: &str = "Ranked by how often the last Monitor run named each key, because the analysis has no LFU counters: a short sample, not the server's own counters.";
/// The ranking is not made yet.
pub const PENDING: &str = "Ranking the keys of the last Monitor run…";
/// No Monitor run has ended in this session.
pub const NO_RUN: &str = "Or rank keys by how often Monitor sees them: Server › Monitor.";
/// The run named no key.
pub const NO_KEYS: &str = "The last Monitor run named no keys.";
/// The analysis has LFU counters: the panel ranks by them.
pub const LFU: &str = "The analysis has LFU counters: Hot keys ranks the keys by them.";

/// `3 lines have a command whose keys are unknown.`
pub fn unknown_text(n: u64) -> String {
    let (lines, verb) = if n == 1 {
        ("line", "has")
    } else {
        ("lines", "have")
    };
    format!(
        "{} {lines} {verb} a command whose keys are unknown.",
        crate::format::grouped(n)
    )
}

/// The ranking as the page gets it.
pub fn body(r: &Ranked, endpoint: Option<String>) -> serde_json::Value {
    let h = &r.hot;
    let total = h.ranking.lines.max(1);
    serde_json::json!({
        "title": crate::tui::ui::hot::monitor_source(h),
        "secs": h.secs,
        "lines": h.ranking.lines,
        "at_unix": h.at_unix,
        "unknown": h.ranking.unknown,
        "unknown_text": (h.ranking.unknown > 0).then(|| unknown_text(h.ranking.unknown)),
        "keys_note": h.keys_note,
        "node": r.node,
        "endpoint": endpoint,
        "rows": rows(&h.ranking, total),
        "empty": h.ranking.keys.is_empty().then_some(NO_KEYS),
    })
}

fn rows(r: &Ranking, total: u64) -> Vec<serde_json::Value> {
    r.keys
        .iter()
        .map(|(k, n)| {
            serde_json::json!({
                "key": Name::of(k),
                "count": n,
                "share": *n as f64 / total as f64,
                "named": format!(
                    "{} · named {} in {} lines",
                    crate::value::escape(k),
                    times(*n),
                    crate::format::grouped(r.lines)
                ),
                "pattern": Name::of(&crate::pattern::pattern_for_key(k)),
            })
        })
        .collect()
}

/// `GET /api/analysis/hot_keys/monitor`: Hot keys ranked from the last Monitor run, for
/// when the analysis has no hot keys (no LFU counters, or `OBJECT FREQ` refused), as the
/// TUI shows it. `status` is `lfu` (the analysis has hot keys: the panel ranks by them),
/// `none` (no run has ended in this session), `pending` (its keys are being placed) or
/// `ready` (with `ranking`).
pub async fn monitor_ranking(State(app): State<App>) -> Response {
    let body = app.hub.with(|c| {
        let Some(r) = &c.session.report else {
            return Err(Refused::new(
                StatusCode::CONFLICT,
                super::analyze::NO_REPORT,
            ));
        };
        if !r.hot_keys.is_empty() {
            return Ok(serde_json::json!({ "status": "lfu", "text": LFU, "ranking": null }));
        }
        Ok(match &c.hot.rank {
            Rank::None => serde_json::json!({ "status": "none", "text": NO_RUN, "ranking": null }),
            Rank::Pending { .. } => {
                serde_json::json!({ "status": "pending", "text": PENDING, "ranking": null })
            }
            Rank::Done(d) => {
                let endpoint = c
                    .session
                    .nodes
                    .get(d.node)
                    .map(|n| crate::value::escape(&n.endpoint.to_string()));
                serde_json::json!({ "status": "ready", "text": SOURCE, "ranking": body(d, endpoint) })
            }
        })
    });
    match body {
        None => {
            Refused::new(StatusCode::CONFLICT, crate::web::session::NOT_CONNECTED).into_response()
        }
        Some(Err(r)) => r.into_response(),
        Some(Ok(b)) => ok(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::server::{Begin, CommandKeys, Find, KeySpec};
    use std::collections::HashMap;

    fn line(s: &str) -> Entry {
        monitor::read_line(s).unwrap()
    }

    fn get_spec() -> CommandKeys {
        CommandKeys::Specs(vec![KeySpec {
            begin: Begin::Index(1),
            find: Find::Range {
                last: 0,
                step: 1,
                limit: 0,
            },
        }])
    }

    #[test]
    fn a_run_is_kept_then_ranked_with_the_tuis_ranking() {
        let mut h = Hot::default();
        h.begin(4, 0);
        h.ready(4, 1_700_000_000_123);
        h.lines(
            4,
            &[
                line(r#"1.0 [0 a:1] "GET" "user:1""#),
                line(r#"1.0 [0 a:1] "GET" "user:1""#),
                line(r#"1.0 [0 a:1] "GET" "user:2""#),
                line(r#"1.0 [0 a:1] "WHAT" "x""#),
            ],
        );
        // Lines of another run are not kept.
        h.lines(5, &[line(r#"1.0 [0 a:1] "GET" "other""#)]);
        let mut specs = HashMap::new();
        specs.insert("get".to_string(), get_spec());
        let now = SpecsNow {
            specs: &specs,
            keys_off: Some("the server refused COMMAND INFO for this user"),
        };
        assert!(!h.end(4, false, &now));
        let Rank::Done(r) = &h.rank else {
            panic!("ranked: {:?}", h.rank);
        };
        assert_eq!(
            r.hot.ranking.keys,
            [("user:1".to_string(), 2), ("user:2".to_string(), 1)]
        );
        assert_eq!((r.hot.ranking.lines, r.hot.ranking.unknown), (4, 1));
        assert_eq!(r.hot.at_unix, 1_700_000_000);
        assert_eq!(
            r.hot.keys_note.as_deref(),
            Some("Key names are off: the server refused COMMAND INFO for this user.")
        );
        let b = body(r, Some("127.0.0.1:6379".into()));
        assert_eq!(b["rows"][0]["key"]["name"], "user:1");
        assert_eq!(b["rows"][0]["named"], "user:1 · named twice in 4 lines");
        assert_eq!(b["rows"][0]["pattern"]["name"], "user:{id}");
        assert_eq!(
            b["unknown_text"],
            "1 line has a command whose keys are unknown."
        );
        assert!(
            b["title"]
                .as_str()
                .unwrap()
                .starts_with("from Monitor, 0 s, 4 lines, at ")
        );
    }

    #[test]
    fn a_run_that_never_started_keeps_the_last_ranking() {
        let specs = HashMap::new();
        let now = SpecsNow {
            specs: &specs,
            keys_off: None,
        };
        let mut h = Hot::default();
        h.begin(1, 0);
        h.ready(1, 0);
        h.lines(1, &[line(r#"1.0 [0 a:1] "GET" "k""#)]);
        h.end(1, false, &now);
        let before = h.rank.clone();
        h.begin(2, 0);
        assert!(!h.end(2, true, &now));
        assert_eq!(h.rank, before);
        // A run asked to look its commands up waits for the lookup.
        h.begin(3, 0);
        h.ready(3, 0);
        assert!(h.end(3, true, &now));
        assert_eq!(h.rank, Rank::Pending { lookup: true });
        assert_eq!(
            h.to_learn(&now).map(|(n, _)| n.is_empty()),
            Some(true),
            "a run without lines has nothing to learn"
        );
    }

    #[test]
    fn the_lines_kept_stop_at_the_runs_cap_and_debug_shows_no_value() {
        const MARK: &str = "s3cr3t-m0n";
        let mut h = Hot::default();
        h.begin(1, 0);
        h.ready(1, 0);
        let l = line(&format!(r#"1.0 [0 a:1] "SET" "k" "{MARK}""#));
        let many: Vec<Entry> = std::iter::repeat_n(l, MAX_LINES + 5).collect();
        h.lines(1, &many);
        h.lines(1, &many[..3]);
        assert_eq!(h.kept.as_ref().unwrap().lines.len(), MAX_LINES);
        assert!(!format!("{h:?}").contains(MARK));
    }

    #[test]
    fn the_words_are_the_dashboards_own() {
        for t in [SOURCE, PENDING, NO_RUN, NO_KEYS, LFU] {
            assert_eq!(crate::web::wording::banned(t), None, "{t}");
            assert_eq!(crate::web::wording::server_free(t), t, "{t}");
        }
        assert_eq!(times(1), "once");
        assert_eq!(times(2), "twice");
        assert_eq!(times(1200), "1,200 times");
        assert_eq!(
            unknown_text(3),
            "3 lines have a command whose keys are unknown."
        );
    }
}
