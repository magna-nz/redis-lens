//! 5 Pub/Sub: the channels with their subscriber counts (`PUBSUB CHANNELS`, `NUMSUB`,
//! `NUMPAT` on the picked node; shard channels on every connected node), the live tail
//! (one at a time, on a RESP3 connection of its own: the TUI's tail), and publishing,
//! which is a write: through the write gate.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{App, Body, SHAPE, error, live_error, ok};
use crate::live::pubsub::{Reply, Request, Subscription};
use crate::live::{Dest, LiveReply, LiveRequest};
use crate::value::{Raw, escape};
use crate::write::WriteOp;

fn channels_json(cs: &[crate::live::pubsub::Channel]) -> Vec<serde_json::Value> {
    cs.iter()
        .map(|c| serde_json::json!({ "name": escape(&c.name), "subscribers": c.subscribers }))
        .collect()
}

/// `GET /api/pubsub/channels`.
pub async fn channels(State(app): State<App>) -> Response {
    let Some((conn, picked, nodes, shards_off)) = app
        .hub
        .with(|c| {
            c.live_ready().then(|| {
                let nodes: Vec<usize> = if c.session.is_cluster() {
                    (0..c.session.nodes.len())
                        .filter(|&i| c.session.nodes[i].connected)
                        .collect()
                } else {
                    vec![0]
                };
                let off = c
                    .session
                    .server
                    .as_ref()
                    .and_then(crate::live::pubsub::shards_off);
                (c.id, c.session.picked, nodes, off)
            })
        })
        .flatten()
    else {
        return error(StatusCode::CONFLICT, super::NOT_LIVE);
    };
    let counts = app
        .hub
        .request(
            Some(conn),
            Dest::node(picked),
            LiveRequest::PubSub(Request::Counts),
        )
        .await;
    let counts = match counts {
        Ok(LiveReply::PubSub(Reply::Counts(c))) => c,
        Ok(_) => return error(StatusCode::BAD_GATEWAY, SHAPE),
        Err(e) => return live_error(&e),
    };
    let mut shard_nodes = Vec::new();
    if shards_off.is_none() {
        let asks = nodes.iter().map(|&i| {
            let hub = std::sync::Arc::clone(&app.hub);
            async move {
                let r = hub
                    .request(
                        Some(conn),
                        Dest::node(i),
                        LiveRequest::PubSub(Request::ShardCounts),
                    )
                    .await;
                (i, r)
            }
        });
        for (i, r) in futures::future::join_all(asks).await {
            shard_nodes.push(match r {
                Ok(LiveReply::PubSub(Reply::ShardCounts(s))) => serde_json::json!({
                    "node": i,
                    "channels": channels_json(&s.channels),
                    "total_channels": s.total_channels,
                    "notes": s.notes,
                }),
                Ok(_) => serde_json::json!({ "node": i, "off": SHAPE }),
                Err(e) => serde_json::json!({ "node": i, "off": e.to_string() }),
            });
        }
    }
    ok(serde_json::json!({
        "node": picked,
        "channels": channels_json(&counts.channels),
        "total_channels": counts.total_channels,
        "patterns": counts.patterns,
        "notes": counts.notes,
        "shards": { "off": shards_off, "nodes": shard_nodes },
    }))
}

/// `POST /api/pubsub/subscribe` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscribe {
    pub name: String,
    /// `auto` (the default: a pattern when it has `*`, `?` or `[`), `channel`,
    /// `pattern` or `shard`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// `POST /api/pubsub/subscribe`: starts the live tail, replacing the one that runs. A
/// shard channel is tailed on the node that owns its slot; the rest on the picked node.
pub async fn subscribe(State(app): State<App>, Body(s): Body<Subscribe>) -> Response {
    let to = match s.kind.as_deref().unwrap_or("auto") {
        "auto" => Subscription::parse(&s.name),
        "channel" => (!s.name.is_empty()).then(|| Subscription::Channel(s.name.clone())),
        "pattern" => (!s.name.is_empty()).then(|| Subscription::Pattern(s.name.clone())),
        "shard" => match Subscription::parse_shard(&s.name) {
            Ok(t) => t,
            Err(why) => return error(StatusCode::UNPROCESSABLE_ENTITY, why),
        },
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "kind is auto, channel, pattern or shard",
            );
        }
    };
    let Some(to) = to else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Type the channel or pattern to subscribe to.",
        );
    };
    let node = app.hub.with(|c| match &to {
        Subscription::Shard(ch) if c.session.is_cluster() => c
            .session
            .owner_of(ch.as_bytes())
            .ok_or(crate::live::nodes::NO_OWNER),
        _ => Ok(c.session.picked),
    });
    let node = match node {
        Some(Ok(n)) => n,
        Some(Err(why)) => return error(StatusCode::UNPROCESSABLE_ENTITY, why),
        None => return error(StatusCode::CONFLICT, super::NOT_LIVE),
    };
    let kind = crate::web::session::sub_kind(&to);
    match app.hub.subscribe(to, node) {
        Ok(tail) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({ "tail": tail, "kind": kind, "node": node })),
        )
            .into_response(),
        Err(why) => error(StatusCode::UNPROCESSABLE_ENTITY, &why),
    }
}

/// `DELETE /api/pubsub/subscribe`: ends the tail.
pub async fn unsubscribe(State(app): State<App>) -> Response {
    ok(serde_json::json!({ "stopped": app.hub.unsubscribe() }))
}

/// `POST /api/pubsub/publish` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    pub channel: String,
    pub message: String,
    /// `SPUBLISH` to a shard channel, on the owner of its slot.
    #[serde(default)]
    pub shard: bool,
}

/// The message is a value: `Debug` leaves it out.
impl std::fmt::Debug for Publish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publish")
            .field("shard", &self.shard)
            .finish_non_exhaustive()
    }
}

/// `POST /api/pubsub/publish`: `PUBLISH` (or `SPUBLISH`) through the write gate: never
/// on a read-only profile, asked first on `prod` (409).
pub async fn publish(State(app): State<App>, Body(p): Body<Publish>) -> Response {
    if p.channel.is_empty() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Type the channel to publish to.",
        );
    }
    let message = Raw::from(p.message.as_str());
    let op = if p.shard {
        WriteOp::SPublish {
            channel: p.channel,
            message,
        }
    } else {
        WriteOp::Publish {
            channel: p.channel,
            message,
        }
    };
    super::writes::write_flow(&app, op).await
}
