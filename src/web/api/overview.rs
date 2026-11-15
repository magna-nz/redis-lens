//! 1 Overview before an analysis: what the connect read of the server (its figures, the
//! permission checks, the provider notice, the per-shard rows of a cluster), and the
//! live figures, polled about once a second only while an event stream is open
//! ([`poll`]), as the TUI's Overview polls `INFO` on the picked node and, on a cluster,
//! each shard every [`SHARDS_EVERY`].

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use tokio::sync::watch;

use super::{App, error, ok};
use crate::live::server::{Reply, Request};
use crate::live::{Dest, LiveReply, LiveRequest};
use crate::tui::app::views::server::{SHARDS_EVERY, point};
use crate::web::dto;
use crate::web::session::{Event, Hub};

/// How often the live figures are polled while a page listens.
pub const EVERY: Duration = Duration::from_secs(1);
/// Rates over a longer gap than this between two polls are not told (the page was away).
const MAX_GAP: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Provider {
    label: &'static str,
    notice: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Last {
    taken_at_unix: i64,
    sampled_keys: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Live {
    node: usize,
    info: dto::Info,
    rates: Option<dto::Rates>,
}

/// `GET /api/overview`.
pub async fn get(State(app): State<App>) -> Response {
    let body = app.hub.with(|c| {
        let s = &c.session;
        let figures = s.figures.as_ref();
        let shards: Vec<dto::Shard> = s
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let live = c.live.shards.get(i);
                dto::Shard::of(
                    dto::Node::of(i, n),
                    figures.and_then(|f| f.nodes.get(i)),
                    live.and_then(|(s, _)| s.as_ref()),
                    live.is_some_and(|(_, failed)| *failed),
                )
            })
            .collect();
        serde_json::json!({
            "status": match &s.phase {
                crate::tui::app::Phase::Connecting => "connecting",
                crate::tui::app::Phase::Failed(_) => "failed",
                _ => "connected",
            },
            "figures": figures.and_then(|f| f.server.as_ref()).map(dto::Figures::of),
            "figures_failed": figures.and_then(|f| f.failed),
            "shards": shards,
            "live": c.live.last.as_ref().map(|(_, sample)| Live {
                node: c.live.node,
                info: dto::Info::of(sample),
                rates: c.live.point.as_ref().map(dto::Rates::of),
            }),
            "perms": c.perms.as_ref().map(dto::perms).unwrap_or_default(),
            "provider": s.profile.provider.map(|p| Provider {
                label: crate::conn::provider::label(p),
                notice: crate::conn::provider::notice(Some(p)),
            }),
            "last_analysis": s.last_snapshot.map(|l| Last {
                taken_at_unix: l.taken_at_unix,
                sampled_keys: l.sampled_keys,
            }),
            "server": s.server.as_ref().map(dto::Server::of),
            "target": s.target.as_ref().map(|t| crate::value::escape(&t.to_string())),
        })
    });
    match body {
        Some(b) => ok(b),
        None => error(StatusCode::CONFLICT, super::NOT_LIVE),
    }
}

/// Polls the live figures while at least one event stream is open, until `shutdown`.
/// Every `INFO` goes through the node's budget, like every other live request.
pub async fn poll(hub: Arc<Hub>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.wait_for(|v| *v) => return,
        }
        if hub.listeners() == 0 {
            continue;
        }
        let Some((conn, node, nodes, shards_due)) = hub
            .with(|c| {
                c.live_ready().then(|| {
                    let n = c.session.nodes.len();
                    let due = n > 0
                        && c.live
                            .shards_at
                            .is_none_or(|at| at.elapsed() >= SHARDS_EVERY);
                    (c.id, c.session.picked, n, due)
                })
            })
            .flatten()
        else {
            continue;
        };
        let info = LiveRequest::Server(Request::Info);
        let r = hub
            .request(Some(conn), Dest::node(node), info.clone())
            .await;
        let live = match r {
            Ok(LiveReply::Server(Reply::Info { at, sample })) => hub
                .with_mut(|c| {
                    if c.id != conn || c.live.node != node {
                        return None;
                    }
                    let prev = c.live.last.as_ref().map(|(t, s)| (*t, s));
                    let p = point(prev, at, &sample, MAX_GAP);
                    c.live.point = Some(p);
                    c.live.last = Some((at, (*sample).clone()));
                    Some(serde_json::json!({
                        "node": node,
                        "info": dto::Info::of(&sample),
                        "rates": dto::Rates::of(&p),
                    }))
                })
                .flatten(),
            Ok(_) => None,
            Err(e) => Some(serde_json::json!({ "node": node, "error": e.to_string() })),
        };
        let mut shards = None;
        if shards_due {
            hub.with_mut(|c| c.live.shards_at = Some(std::time::Instant::now()));
            let asks = (0..nodes).map(|i| {
                let hub = Arc::clone(&hub);
                let info = info.clone();
                async move { hub.request(Some(conn), Dest::node(i), info).await }
            });
            let results = futures::future::join_all(asks).await;
            shards = hub
                .with_mut(|c| {
                    if c.id != conn {
                        return None;
                    }
                    c.live.shards.resize(nodes, (None, false));
                    for (i, r) in results.into_iter().enumerate() {
                        match r {
                            Ok(LiveReply::Server(Reply::Info { sample, .. })) => {
                                c.live.shards[i] = (Some(*sample), false);
                            }
                            _ => c.live.shards[i].1 = true,
                        }
                    }
                    Some(
                        c.live
                            .shards
                            .iter()
                            .enumerate()
                            .map(|(i, (s, failed))| {
                                serde_json::json!({
                                    "node": i,
                                    "info": s.as_ref().map(dto::Info::of),
                                    "failed": failed,
                                })
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .flatten();
        }
        if live.is_some() || shards.is_some() {
            let mut data = live.unwrap_or_else(|| serde_json::json!({ "node": node }));
            if let Some(s) = shards {
                data["shards"] = serde_json::Value::Array(s);
            }
            hub.emit(Event {
                name: "figures",
                data,
            });
        }
    }
}
