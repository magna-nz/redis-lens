//! The live connection: after a scan, its connection stays open for the views that poll
//! or look things up (spec "Browsing, server views and writes"). Owner: wave A1; the
//! per-view submodules belong to wave B.
//!
//! Contract: the types and signatures below are fixed. Phase 1 sends no write command
//! and reads no value (spec "Values").

use std::sync::Arc;

use redis::aio::MultiplexedConnection;

use crate::budget::Budget;

pub mod keys;
pub mod pubsub;
pub mod search;
pub mod server;
pub mod streams;

/// The connection type the live task uses. It is cheap to clone.
pub type Conn = MultiplexedConnection;

/// Ties a reply to the request that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReqId(pub u64);

/// One piece of work for the live connection, grouped by the view that asks.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveRequest {
    Server(server::Request),
    Keys(keys::Request),
    Streams(streams::Request),
    PubSub(pubsub::Request),
    Search(search::Request),
}

/// The answer to a [`LiveRequest`], from the same group.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveReply {
    Server(server::Reply),
    Keys(keys::Reply),
    Streams(streams::Reply),
    PubSub(pubsub::Reply),
    Search(search::Reply),
}

/// Why a request failed. The text is always redis-lens's own fixed wording, never the
/// server's error text (it can name the ACL user) and never a credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LiveError {
    /// `NOPERM`: the server refused the command for this user. Holds the command name.
    #[error("the server refused {0} for this user")]
    Denied(String),
    /// Unknown command, or a server too old for it. Holds what is missing.
    #[error("the server does not support {0}")]
    Unsupported(String),
    /// Any other failure, in redis-lens's own words.
    #[error("{0}")]
    Failed(String),
    /// The live connection is gone.
    #[error("the live connection is closed")]
    Closed,
}

/// Runs one request: the dispatcher to the per-view `run` functions. Every command goes
/// through `budget`.
pub async fn run(
    conn: &mut Conn,
    budget: &Budget,
    req: LiveRequest,
) -> Result<LiveReply, LiveError> {
    let _ = (conn, budget, req);
    todo!("wave A1")
}

/// The runner's handle on one session's live task. Dropping it stops the task and closes
/// the connection.
#[derive(Debug)]
pub struct LiveHandle {
    _todo: (),
}

impl LiveHandle {
    /// Queues a request. False when the task has stopped.
    pub fn send(&self, id: ReqId, req: LiveRequest) -> bool {
        let _ = (id, req);
        todo!("wave A1")
    }
}

/// Starts the live task on `conn`. Each finished request is passed to `reply`, in any
/// order; requests may run concurrently, each still taking from `budget`.
pub fn spawn(
    conn: Conn,
    budget: Arc<Budget>,
    reply: impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync + 'static,
) -> LiveHandle {
    let _ = (conn, budget, reply);
    todo!("wave A1")
}
