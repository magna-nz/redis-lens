//! The live connection: after a scan, its connection stays open for the views that poll
//! or look things up (spec "Browsing, server views and writes"). On a cluster, one
//! connection per shard, the one discovery opened (spec "Cluster live views (P4)").
//! Owner: the cluster live core (P4); the per-view submodules belong to their views.
//!
//! Contract: the types and signatures below are fixed. Only [`LiveRequest::Write`] and
//! a batch of [`LiveRequest::Bulk`] send writes, and each carries a
//! [`crate::write::Permit`], which only the write gate makes ([`write`], [`bulk`]).
//! Only [`values`] (key values and stream entries, in pages), [`search`] (query
//! results) and the Pub/Sub tail ([`pubsub`]) read values, held as
//! [`crate::value::Raw`] (spec "Values"); the command line ([`cmdline`]) holds its
//! replies as [`cmdline::Output`], redacted the same way.
//!
//! [`spawn`] starts one task per connected session on a standalone server, and
//! [`spawn_nodes`] one on a cluster, over a [`nodes::Table`]. Requests arrive on a
//! queue, each with a [`Dest`], and each runs in a task of its own on a clone of its
//! node's connection, so a slow lookup never holds up a poll; every command takes from
//! that node's [`Budget`] (one budget per node, never one for the whole cluster).
//! Dropping the [`LiveHandle`] ends the task, aborts the requests in flight and, once
//! the scan has let go of its clones too, closes the connections.
//!
//! **Routing.** [`To::Node`] sends to one node of the table (the picked node, or each
//! primary for the per-shard summary), [`To::Key`] to the node that owns the key's slot
//! ([`crate::conn::slot`]). A standalone table sends everything to its one node.
//!
//! **Redirects.** Each request runs on a [`Conn`], which records the first `MOVED` or
//! `ASK` it sees, as an error or inside a pipeline's replies. With [`Follow::Redirects`]
//! the request is sent once more, to the node named, when the table has a connection
//! to it: after `ASK` with `ASKING` in the same pipeline, after `MOVED` with the slot
//! map updated. [`Follow::MovedOnFirst`] follows only a `MOVED` answering the request's
//! first command (a `WATCH`, before anything was queued). [`Follow::Never`] returns the
//! result as it came (a bulk run counts the keys that moved). A second redirect, or one
//! to an address that is not in the table, fails with [`MOVING`]: no node is connected
//! to after discovery.
//!
//! The per-view `run` functions are generic over [`LiveConn`] (any
//! [`redis::aio::ConnectionLike`], so tests pass a bare `MultiplexedConnection`) and
//! send commands with [`query`] and [`query_pipe`], which take from the budget first and
//! turn a failure into a [`LiveError`] with [`error_for`].

use std::collections::HashMap;
use std::sync::Arc;

use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::{FromRedisValue, ServerErrorKind, Value};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::budget::Budget;

pub mod bulk;
pub mod cmdline;
pub mod command;
pub mod dbs;
pub mod keys;
pub mod monitor;
pub mod nodes;
pub mod pubsub;
pub mod search;
pub mod server;
pub mod slots;
pub mod streams;
pub mod values;
pub mod write;

/// What the per-view `run` functions send on: any connection redis-rs can send
/// commands and pipelines on.
pub trait LiveConn: ConnectionLike + Send {}

impl<T: ConnectionLike + Send> LiveConn for T {}

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum To {
    /// Node `i` of the session's node table (a standalone server has only node 0).
    Node(usize),
    /// The node that owns this key's slot.
    Key(Vec<u8>),
}

/// Which redirects a request follows (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Follow {
    /// `MOVED` and `ASK`, once.
    #[default]
    Redirects,
    /// Only a `MOVED` answering the request's first command: a write sent with `WATCH`
    /// and `MULTI`/`EXEC`, where nothing was queued yet when the `WATCH` was redirected.
    MovedOnFirst,
    /// None: the result is returned as it came.
    Never,
}

/// A request's destination and redirect rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dest {
    pub to: To,
    pub follow: Follow,
}

impl Dest {
    /// Node `i`, following redirects.
    pub fn node(i: usize) -> Self {
        Self {
            to: To::Node(i),
            follow: Follow::Redirects,
        }
    }

    /// The owner of `key`'s slot, following redirects.
    pub fn key(key: impl Into<Vec<u8>>) -> Self {
        Self {
            to: To::Key(key.into()),
            follow: Follow::Redirects,
        }
    }

    #[must_use]
    pub fn follow(mut self, follow: Follow) -> Self {
        self.follow = follow;
        self
    }
}

impl Default for Dest {
    fn default() -> Self {
        Self::node(0)
    }
}

/// The fixed text of a redirect redis-lens does not follow (spec "Cluster live views
/// (P4)", Redirects).
pub const MOVING: &str = "the cluster is moving that slot; try again, or rescan";

/// The fixed text of a request whose keys hash to different slots: refused before
/// anything is sent.
pub const CROSSSLOT: &str =
    "the keys are in different slots (CROSSSLOT), so it can't be sent as one command";

/// A `MOVED` or `ASK` a [`Conn`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// `ASK` (true) or `MOVED`.
    pub ask: bool,
    pub slot: u16,
    /// The host as the server named it (may be empty: the node that answered).
    pub host: String,
    pub port: u16,
    /// It answered the request's first command.
    pub first: bool,
}

impl Redirect {
    fn of_server_error(e: &redis::ServerError, first: bool) -> Option<Self> {
        let ask = match e.kind() {
            Some(ServerErrorKind::Ask) => true,
            Some(ServerErrorKind::Moved) => false,
            _ => return None,
        };
        Self::parse(ask, e.details()?, first)
    }

    fn of_error(e: &redis::RedisError, first: bool) -> Option<Self> {
        if let Some((addr, slot)) = e.redirect_node() {
            let ask = e.kind() == redis::ErrorKind::Server(ServerErrorKind::Ask);
            return Self::parse(ask, &format!("{slot} {addr}"), first);
        }
        e.clone()
            .into_server_errors()?
            .iter()
            .find_map(|(_, se)| Self::of_server_error(se, first))
    }

    /// `"<slot> <host>:<port>"`.
    fn parse(ask: bool, details: &str, first: bool) -> Option<Self> {
        let mut it = details.split_ascii_whitespace();
        let slot: u16 = it.next()?.parse().ok()?;
        let (host, port) = it.next()?.rsplit_once(':')?;
        Some(Self {
            ask,
            slot,
            host: host.to_string(),
            port: port.parse().ok()?,
            first,
        })
    }
}

/// The connection one request runs on: a node's connection that records the first
/// redirect it sees and, with `asking`, sends `ASKING` before each command in the same
/// pipeline (a request sent again after an `ASK`).
pub struct Conn<C = MultiplexedConnection> {
    inner: C,
    asking: bool,
    /// How many sends went through, to tell the first command apart.
    calls: usize,
    seen: Option<Redirect>,
}

impl<C: ConnectionLike + Send> Conn<C> {
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            asking: false,
            calls: 0,
            seen: None,
        }
    }

    /// For a request sent again after an `ASK`.
    pub fn asking(inner: C) -> Self {
        Self {
            asking: true,
            ..Self::new(inner)
        }
    }

    /// The first redirect seen, if any.
    pub fn redirect(&self) -> Option<&Redirect> {
        self.seen.as_ref()
    }

    fn note(&mut self, r: Option<Redirect>) {
        if self.seen.is_none() {
            self.seen = r;
        }
    }

    fn note_values(&mut self, values: &[Value], first: bool) {
        let r = values.iter().find_map(|v| match v {
            Value::ServerError(e) => Redirect::of_server_error(e, first),
            _ => None,
        });
        self.note(r);
    }

    fn note_error(&mut self, e: &redis::RedisError, first: bool) {
        self.note(Redirect::of_error(e, first));
    }

    /// Counts a send; true for the request's first.
    fn start(&mut self) -> bool {
        self.calls += 1;
        self.calls == 1
    }
}

/// The server errors among `values`, first one.
fn first_server_error(values: &[Value]) -> Option<redis::ServerError> {
    values.iter().find_map(|v| match v {
        Value::ServerError(e) => Some(e.clone()),
        _ => None,
    })
}

impl<C: ConnectionLike + Send> ConnectionLike for Conn<C> {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a redis::Cmd) -> redis::RedisFuture<'a, Value> {
        Box::pin(async move {
            let first = self.start();
            let r = if self.asking {
                let mut p = redis::pipe();
                p.cmd("ASKING").add_command(cmd.clone());
                self.inner
                    .req_packed_commands(&p, 1, 1)
                    .await
                    .map(|mut v| v.pop().unwrap_or(Value::Nil))
            } else {
                self.inner.req_packed_command(cmd).await
            };
            match &r {
                Ok(v) => self.note_values(std::slice::from_ref(v), first),
                Err(e) => self.note_error(e, first),
            }
            r
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipe: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let first = self.start();
            if !self.asking {
                let r = self.inner.req_packed_commands(pipe, offset, count).await;
                match &r {
                    Ok(v) => self.note_values(v, first),
                    Err(e) => self.note_error(e, first),
                }
                return r;
            }
            let mut p = redis::pipe();
            if pipe.is_transaction() {
                // `ASKING` once, in front of `MULTI` (the queued commands share a slot),
                // sent as a plain pipeline so every reply is seen.
                p.cmd("ASKING").cmd("MULTI");
                for c in pipe.cmd_iter() {
                    p.add_command(c.clone());
                }
                p.cmd("EXEC");
                let mut all = match self.inner.req_packed_commands(&p, 0, p.len()).await {
                    Ok(all) => all,
                    Err(e) => {
                        self.note_error(&e, first);
                        return Err(e);
                    }
                };
                self.note_values(&all, first);
                // A command refused while it was queued aborts the transaction.
                let queued = all.get(1..all.len().saturating_sub(1)).unwrap_or(&[]);
                if let Some(e) = first_server_error(queued) {
                    return Err(e.into());
                }
                Ok(all.pop().into_iter().collect())
            } else {
                for c in pipe.cmd_iter() {
                    p.cmd("ASKING").add_command(c.clone());
                }
                let all = match self.inner.req_packed_commands(&p, 0, p.len()).await {
                    Ok(all) => all,
                    Err(e) => {
                        self.note_error(&e, first);
                        return Err(e);
                    }
                };
                let replies: Vec<Value> = all.into_iter().skip(1).step_by(2).collect();
                self.note_values(&replies, first);
                Ok(replies.into_iter().skip(offset).take(count).collect())
            }
        })
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

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
    /// A page of a key's value, for the key detail screen.
    Value(values::Request),
    /// The command line panel: a command's flags and docs, and the typed command.
    Command(cmdline::Request),
    /// A write the gate let through (spec "Write permissions").
    Write(crate::write::Permit),
    /// A step of a bulk action (spec "Bulk actions"): the dry run, or a batch of a
    /// bulk op the gate let through.
    Bulk(bulk::Request),
    /// The Slots panel: `CLUSTER SLOT-STATS` of a node's slot ranges.
    Slots(slots::Request),
    /// `INFO keyspace`, for the database list.
    Dbs(dbs::Request),
}

/// The answer to a [`LiveRequest`], from the same group.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveReply {
    Server(server::Reply),
    Keys(keys::Reply),
    Streams(streams::Reply),
    PubSub(pubsub::Reply),
    Search(search::Reply),
    Value(values::Reply),
    Command(cmdline::Reply),
    Write(write::Reply),
    Bulk(bulk::Reply),
    Slots(slots::Reply),
    Dbs(dbs::Reply),
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

/// What a write's view hears when its question was answered no
/// ([`LiveError::Failed`] with this text): nothing was sent.
pub const CANCELLED: &str = "Cancelled: nothing was sent.";

/// Runs one request: the dispatcher to the per-view `run` functions. Every command goes
/// through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: LiveRequest,
) -> Result<LiveReply, LiveError> {
    match req {
        LiveRequest::Server(r) => server::run(conn, budget, r).await.map(LiveReply::Server),
        LiveRequest::Keys(r) => keys::run(conn, budget, r).await.map(LiveReply::Keys),
        LiveRequest::Streams(r) => streams::run(conn, budget, r).await.map(LiveReply::Streams),
        LiveRequest::PubSub(r) => pubsub::run(conn, budget, r).await.map(LiveReply::PubSub),
        LiveRequest::Search(r) => search::run(conn, budget, r).await.map(LiveReply::Search),
        LiveRequest::Value(r) => values::run(conn, budget, r).await.map(LiveReply::Value),
        LiveRequest::Command(r) => cmdline::run(conn, budget, r).await.map(LiveReply::Command),
        LiveRequest::Write(p) => write::run(conn, budget, p).await.map(LiveReply::Write),
        LiveRequest::Bulk(r) => bulk::run(conn, budget, r).await.map(LiveReply::Bulk),
        LiveRequest::Slots(r) => slots::run(conn, budget, r).await.map(LiveReply::Slots),
        LiveRequest::Dbs(r) => dbs::run(conn, budget, r).await.map(LiveReply::Dbs),
    }
}

/// What [`LiveClosed`](crate::tui::app::Msg::LiveClosed) says when the connection went
/// away without being closed on purpose.
pub const CONNECTION_LOST: &str = "the connection to the server was lost";

/// Turns a failure of `command` into a [`LiveError`] in redis-lens's own words. The
/// server's text is only looked at to tell an unknown command apart, never kept: it can
/// name the ACL user. `command` is redis-lens's own name for what it sent, such as
/// `SLOWLOG GET`.
pub fn error_for(command: &str, e: &redis::RedisError) -> LiveError {
    if e.kind() == redis::ErrorKind::Server(ServerErrorKind::NoPerm) {
        LiveError::Denied(command.to_string())
    } else if matches!(
        e.kind(),
        redis::ErrorKind::Server(ServerErrorKind::Moved | ServerErrorKind::Ask)
    ) {
        LiveError::Failed(MOVING.to_string())
    } else if is_unknown_command(e) {
        LiveError::Unsupported(command.to_string())
    } else if e.is_timeout() {
        LiveError::Failed(format!("the server did not answer {command} in time"))
    } else if e.is_connection_dropped() {
        LiveError::Closed
    } else {
        LiveError::Failed(format!("{command} failed"))
    }
}

/// `ERR unknown command …` or `ERR unknown subcommand …`: the server (or a module it
/// lacks) has no such command. Also how a missing Search module answers `FT._LIST`.
pub(crate) fn is_unknown_command(e: &redis::RedisError) -> bool {
    if e.kind() != redis::ErrorKind::Server(ServerErrorKind::ResponseError) {
        return false;
    }
    let text = format!("{} {}", e.code().unwrap_or(""), e.detail().unwrap_or("")).to_lowercase();
    text.contains("unknown command") || text.contains("unknown subcommand")
}

/// Takes one command from `budget`, then sends `cmd`. `name` is what a failure is
/// called (see [`error_for`]).
pub async fn query<T: FromRedisValue>(
    conn: &mut impl LiveConn,
    budget: &Budget,
    name: &str,
    cmd: &redis::Cmd,
) -> Result<T, LiveError> {
    budget.take(1).await;
    cmd.query_async(conn).await.map_err(|e| error_for(name, &e))
}

/// Takes the pipeline's length from `budget`, then sends `pipe`. `name` is what a failure
/// is called (see [`error_for`]).
pub async fn query_pipe<T: FromRedisValue>(
    conn: &mut impl LiveConn,
    budget: &Budget,
    name: &str,
    pipe: &redis::Pipeline,
) -> Result<T, LiveError> {
    budget
        .take(u32::try_from(pipe.len()).unwrap_or(u32::MAX))
        .await;
    pipe.query_async(conn)
        .await
        .map_err(|e| error_for(name, &e))
}

type Queued = (ReqId, Dest, LiveRequest);

/// The runner's handle on one session's live task. Dropping it stops the task and closes
/// the connections.
#[derive(Debug)]
pub struct LiveHandle {
    queue: mpsc::UnboundedSender<Queued>,
}

impl LiveHandle {
    /// Queues a request for node 0 (a standalone server's only node). False when the
    /// task has stopped.
    pub fn send(&self, id: ReqId, req: LiveRequest) -> bool {
        self.send_to(id, Dest::default(), req)
    }

    /// Queues a request for `dest`. False when the task has stopped.
    pub fn send_to(&self, id: ReqId, dest: Dest, req: LiveRequest) -> bool {
        self.queue.send((id, dest, req)).is_ok()
    }

    /// A handle with no task behind it, for tests: what it is sent arrives on the
    /// receiver, and dropping the handle closes it.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, mpsc::UnboundedReceiver<Queued>) {
        let (queue, rx) = mpsc::unbounded_channel();
        (Self { queue }, rx)
    }
}

/// Starts the live task on `conn`, a standalone server. Each finished request is passed
/// to `reply`, in any order; requests may run concurrently, each still taking from
/// `budget`.
pub fn spawn(
    conn: MultiplexedConnection,
    budget: Arc<Budget>,
    reply: impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync + 'static,
) -> LiveHandle {
    spawn_with_close(conn, budget, reply, |_| {})
}

/// [`spawn`], calling `on_close` once when the task ends: with `None` when it was closed
/// on purpose (the handle was dropped), otherwise with why, in redis-lens's own words
/// ([`CONNECTION_LOST`]).
///
/// When the connection is lost, every request still in flight or queued is answered
/// with [`LiveError::Closed`] before `on_close` runs. When the handle is dropped,
/// requests in flight are aborted and get no answer.
pub fn spawn_with_close(
    conn: MultiplexedConnection,
    budget: Arc<Budget>,
    reply: impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync + 'static,
    on_close: impl FnOnce(Option<String>) + Send + 'static,
) -> LiveHandle {
    spawn_nodes(nodes::Table::single(conn, budget), reply, on_close)
}

/// [`spawn_with_close`] over a node table: a cluster's shards, or one standalone server.
/// On a cluster a node whose connection is lost fails the requests sent to it (with
/// [`nodes::lost`]) and the task goes on; it ends, as on a standalone server, once no
/// node is left.
pub fn spawn_nodes<C>(
    table: nodes::Table<C>,
    reply: impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync + 'static,
    on_close: impl FnOnce(Option<String>) + Send + 'static,
) -> LiveHandle
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    let (queue, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let why = serve(Arc::new(table), rx, &reply).await;
        on_close(why);
    });
    LiveHandle { queue }
}

/// What one request's task gives back: its result, and the node whose connection it
/// found closed, if any.
type Done = (Result<LiveReply, LiveError>, Option<usize>);

/// Runs `req` on the node `dest` picks and follows a redirect as `dest` allows (see the
/// module documentation).
async fn route<C>(table: &nodes::Table<C>, dest: Dest, req: LiveRequest) -> Done
where
    C: ConnectionLike + Clone + Send + Sync,
{
    let moving = || Err(LiveError::Failed(MOVING.to_string()));
    let i = match table.pick(&dest.to) {
        Ok(i) => i,
        Err(e) => return (Err(e), None),
    };
    let (c, budget) = match table.conn(i) {
        Ok(c) => c,
        Err(e) => return (Err(e), None),
    };
    let mut conn = Conn::new(c);
    let first = run(&mut conn, &budget, req.clone()).await;
    if first == Err(LiveError::Closed) {
        return (first, Some(i));
    }
    let Some(r) = conn.redirect().cloned() else {
        return (first, None);
    };
    match dest.follow {
        Follow::Never => return (first, None),
        Follow::MovedOnFirst if r.ask || !r.first => return (moving(), None),
        Follow::MovedOnFirst | Follow::Redirects => {}
    }
    let Some(j) = table.find(&r.host, r.port, i) else {
        return (moving(), None);
    };
    let (c, budget) = match table.conn(j) {
        Ok(c) => c,
        Err(_) => return (moving(), None),
    };
    if !r.ask {
        table.moved(r.slot, j);
    }
    let mut conn = if r.ask { Conn::asking(c) } else { Conn::new(c) };
    let second = run(&mut conn, &budget, req).await;
    if second == Err(LiveError::Closed) {
        return (second, Some(j));
    }
    if conn.redirect().is_some() {
        return (moving(), None);
    }
    (second, None)
}

/// The task's loop. Returns `None` when the handle was dropped, the reason otherwise.
async fn serve<C>(
    table: Arc<nodes::Table<C>>,
    mut rx: mpsc::UnboundedReceiver<Queued>,
    reply: &(impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync),
) -> Option<String>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    // Dropped on return: that aborts whatever is still running.
    let mut tasks: JoinSet<Done> = JoinSet::new();
    let mut in_flight: HashMap<tokio::task::Id, ReqId> = HashMap::new();
    loop {
        tokio::select! {
            next = rx.recv() => {
                // The handle was dropped: closed on purpose.
                let (id, dest, req) = next?;
                let table = Arc::clone(&table);
                let task = tasks.spawn(async move { route(&table, dest, req).await });
                in_flight.insert(task.id(), id);
            }
            Some(done) = tasks.join_next_with_id() => {
                let (task, (result, lost_node)) = match done {
                    Ok((task, done)) => (task, done),
                    Err(e) => (
                        e.id(),
                        (
                            Err(LiveError::Failed("the request stopped unexpectedly".to_string())),
                            None,
                        ),
                    ),
                };
                let Some(id) = in_flight.remove(&task) else {
                    continue;
                };
                let all_lost = lost_node.map(|i| (i, table.mark_lost(i)));
                match all_lost {
                    // A cluster with other nodes left: only that node is gone.
                    Some((i, false)) if table.is_cluster() => {
                        let why = table
                            .endpoint(i)
                            .map_or_else(|| CONNECTION_LOST.to_string(), nodes::lost);
                        reply(id, Err(LiveError::Failed(why)));
                    }
                    Some(_) => {
                        reply(id, result);
                        tasks.abort_all();
                        for (_, id) in in_flight.drain() {
                            reply(id, Err(LiveError::Closed));
                        }
                        rx.close();
                        while let Ok((id, _, _)) = rx.try_recv() {
                            reply(id, Err(LiveError::Closed));
                        }
                        return Some(CONNECTION_LOST.to_string());
                    }
                    None => reply(id, result),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::ErrorKind;

    fn server_error(kind: ServerErrorKind, code: &'static str, detail: &str) -> redis::RedisError {
        redis::RedisError::from((ErrorKind::Server(kind), code, detail.to_string()))
    }

    #[test]
    fn errors_are_in_redis_lens_words() {
        let noperm = server_error(
            ServerErrorKind::NoPerm,
            "NOPERM",
            "User lens-ro has no permissions to run the 'slowlog|get' command",
        );
        assert_eq!(
            error_for("SLOWLOG GET", &noperm),
            LiveError::Denied("SLOWLOG GET".into())
        );
        for detail in [
            "unknown command 'FT._LIST', with args beginning with: ",
            "unknown command `FT._LIST`, with args beginning with: ",
            "Unknown subcommand or wrong number of arguments for 'NUMPAT'",
        ] {
            let e = server_error(ServerErrorKind::ResponseError, "ERR", detail);
            assert_eq!(
                error_for("FT._LIST", &e),
                LiveError::Unsupported("FT._LIST".into()),
                "{detail}"
            );
        }
        let other = server_error(
            ServerErrorKind::ResponseError,
            "ERR",
            "Error running script for user lens-ro with password hunter2",
        );
        let e = error_for("XINFO STREAM", &other);
        assert_eq!(e, LiveError::Failed("XINFO STREAM failed".into()));
        let text = e.to_string();
        assert!(
            !text.contains("lens-ro") && !text.contains("hunter2"),
            "{text}"
        );

        let dropped = redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert_eq!(error_for("PING", &dropped), LiveError::Closed);
        let reset =
            redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert_eq!(error_for("PING", &reset), LiveError::Closed);
        let slow = redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::TimedOut));
        assert_eq!(
            error_for("INFO", &slow),
            LiveError::Failed("the server did not answer INFO in time".into())
        );
        let moved = server_error(ServerErrorKind::Moved, "MOVED", "12182 10.0.0.2:7001");
        assert_eq!(error_for("GET", &moved), LiveError::Failed(MOVING.into()));
    }

    #[test]
    fn error_texts_are_fixed() {
        assert_eq!(
            LiveError::Denied("SLOWLOG GET".into()).to_string(),
            "the server refused SLOWLOG GET for this user"
        );
        assert_eq!(
            LiveError::Unsupported("FT._LIST".into()).to_string(),
            "the server does not support FT._LIST"
        );
        assert_eq!(
            LiveError::Closed.to_string(),
            "the live connection is closed"
        );
        assert_eq!(
            MOVING,
            "the cluster is moving that slot; try again, or rescan"
        );
    }

    // -- routing and redirects, on scripted nodes ---------------------------------

    use std::sync::Mutex;

    use crate::model::{Endpoint, NodeRole};
    use nodes::{Node, NodeInfo, Table};

    /// What every fake node received: (node, the command's words).
    type Log = Arc<Mutex<Vec<(usize, Vec<String>)>>>;
    /// The reply of node `n` to a command, by its words.
    type Script = Arc<dyn Fn(usize, &[String]) -> Value + Send + Sync>;

    #[derive(Clone)]
    struct Fake {
        node: usize,
        log: Log,
        script: Script,
    }

    fn words(cmd: &redis::Cmd) -> Vec<String> {
        cmd.args_iter()
            .map(|a| match a {
                redis::Arg::Simple(b) => String::from_utf8_lossy(b).into_owned(),
                _ => "0".into(),
            })
            .collect()
    }

    impl Fake {
        fn answer(&self, cmd: &redis::Cmd) -> Value {
            let w = words(cmd);
            self.log.lock().unwrap().push((self.node, w.clone()));
            (self.script)(self.node, &w)
        }
    }

    impl ConnectionLike for Fake {
        fn req_packed_command<'a>(
            &'a mut self,
            cmd: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, Value> {
            let v = self.answer(cmd);
            Box::pin(async move { Ok(v) })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            pipe: &'a redis::Pipeline,
            offset: usize,
            count: usize,
        ) -> redis::RedisFuture<'a, Vec<Value>> {
            let all: Vec<Value> = pipe.cmd_iter().map(|c| self.answer(c)).collect();
            Box::pin(async move { Ok(all.into_iter().skip(offset).take(count).collect()) })
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    fn redirect(kind: &str, slot: u16, port: u16) -> Value {
        let wire = format!("-{kind} {slot} 10.0.0.1:{port}\r\n");
        let v = redis::parse_redis_value(wire.as_bytes()).unwrap();
        assert!(matches!(v, Value::ServerError(_)), "{v:?}");
        v
    }

    fn pong() -> Value {
        Value::SimpleString("PONG".into())
    }

    /// Three nodes on ports 7000-7002 in slot order, node 2 not connected.
    fn table(script: Script) -> (Table<Fake>, Log) {
        let log: Log = Arc::default();
        let ranges = [(0, 5460), (5461, 10922), (10923, 16383)];
        let nodes = (0..3)
            .map(|i| Node {
                info: NodeInfo {
                    id: format!("n{i}"),
                    endpoint: Endpoint {
                        host: "10.0.0.1".into(),
                        port: 7000 + i as u16,
                        tls: false,
                    },
                    role: NodeRole::Primary,
                    slots: vec![ranges[i]],
                    connected: i != 2,
                },
                conn: (i != 2).then(|| Fake {
                    node: i,
                    log: Arc::clone(&log),
                    script: Arc::clone(&script),
                }),
                budget: Budget::new(None),
            })
            .collect();
        (Table::cluster(nodes), log)
    }

    fn ping() -> LiveRequest {
        LiveRequest::Server(server::Request::Ping)
    }

    fn sent(log: &Log) -> Vec<(usize, String)> {
        log.lock()
            .unwrap()
            .iter()
            .map(|(n, w)| (*n, w.join(" ")))
            .collect()
    }

    #[tokio::test]
    async fn moved_is_followed_once_and_remembered() {
        // "bar" (slot 5061) belongs to node 0 in the map, but node 0 says node 1.
        let (t, log) = table(Arc::new(|n, _| {
            if n == 0 {
                redirect("MOVED", 5061, 7001)
            } else {
                pong()
            }
        }));
        let (r, lost) = route(&t, Dest::key("bar"), ping()).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(lost, None);
        assert_eq!(sent(&log), [(0, "PING".into()), (1, "PING".into())]);
        // Remembered: the next one goes straight to node 1.
        log.lock().unwrap().clear();
        let (r, _) = route(&t, Dest::key("bar"), ping()).await;
        assert!(r.is_ok());
        assert_eq!(sent(&log), [(1, "PING".into())]);
    }

    #[tokio::test]
    async fn ask_is_followed_with_asking_and_not_remembered() {
        let (t, log) = table(Arc::new(|n, w| {
            if n == 0 {
                redirect("ASK", 5061, 7001)
            } else if w[0] == "ASKING" {
                Value::Okay
            } else {
                pong()
            }
        }));
        let (r, _) = route(&t, Dest::key("bar"), ping()).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(
            sent(&log),
            [(0, "PING".into()), (1, "ASKING".into()), (1, "PING".into())]
        );
        assert_eq!(t.owner_of(5061), Ok(0), "an ASK changes no slot");
    }

    #[tokio::test]
    async fn a_second_redirect_fails() {
        let (t, log) = table(Arc::new(|n, _| {
            redirect("MOVED", 5061, if n == 0 { 7001 } else { 7000 })
        }));
        let (r, _) = route(&t, Dest::key("bar"), ping()).await;
        assert_eq!(r, Err(LiveError::Failed(MOVING.into())));
        assert_eq!(sent(&log).len(), 2, "sent once more, never twice");
    }

    #[tokio::test]
    async fn redirects_to_unknown_or_unconnected_nodes_are_not_followed() {
        for port in [7002, 7009] {
            let (t, log) = table(Arc::new(move |_, _| redirect("MOVED", 5061, port)));
            let (r, _) = route(&t, Dest::key("bar"), ping()).await;
            assert_eq!(r, Err(LiveError::Failed(MOVING.into())), "{port}");
            assert_eq!(sent(&log), [(0, "PING".into())]);
            assert_eq!(t.owner_of(5061), Ok(0));
        }
    }

    #[tokio::test]
    async fn never_returns_the_result_as_it_came() {
        let (t, log) = table(Arc::new(|_, _| redirect("MOVED", 5061, 7001)));
        let (r, _) = route(&t, Dest::key("bar").follow(Follow::Never), ping()).await;
        assert_eq!(r, Err(LiveError::Failed(MOVING.into())));
        assert_eq!(sent(&log).len(), 1);
    }

    #[tokio::test]
    async fn moved_on_first_follows_only_a_first_moved() {
        // A request of two commands: the second one is redirected.
        let (t, log) = table(Arc::new(|n, w| {
            if n == 0 && w[0] == "EXISTS" {
                redirect("MOVED", 5061, 7001)
            } else {
                Value::Int(1)
            }
        }));
        let mut conn = Conn::new(t.conn(0).unwrap().0);
        let _: Value = redis::cmd("PING").query_async(&mut conn).await.unwrap();
        let _: Result<Value, _> = redis::cmd("EXISTS").arg("bar").query_async(&mut conn).await;
        assert_eq!(conn.redirect().map(|r| r.first), Some(false));
        // ASK is never followed with MovedOnFirst.
        let (t2, log2) = table(Arc::new(|n, _| {
            if n == 0 {
                redirect("ASK", 5061, 7001)
            } else {
                pong()
            }
        }));
        let (r, _) = route(&t2, Dest::key("bar").follow(Follow::MovedOnFirst), ping()).await;
        assert_eq!(r, Err(LiveError::Failed(MOVING.into())));
        assert_eq!(sent(&log2).len(), 1);
        // A MOVED on the first command is.
        drop(log);
        let (t3, log3) = table(Arc::new(|n, _| {
            if n == 0 {
                redirect("MOVED", 5061, 7001)
            } else {
                pong()
            }
        }));
        let (r, _) = route(&t3, Dest::key("bar").follow(Follow::MovedOnFirst), ping()).await;
        assert!(r.is_ok());
        assert_eq!(sent(&log3).len(), 2);
    }

    #[tokio::test]
    async fn asking_goes_before_each_command_of_a_pipeline() {
        let (t, log) = table(Arc::new(|_, w| {
            if w[0] == "ASKING" {
                Value::Okay
            } else {
                Value::Int(w.len() as i64)
            }
        }));
        let mut conn = Conn::asking(t.conn(1).unwrap().0);
        let mut p = redis::pipe();
        p.cmd("TYPE").arg("a").cmd("TTL").arg("a");
        let got: (i64, i64) = p.query_async(&mut conn).await.unwrap();
        assert_eq!(got, (2, 2));
        assert_eq!(
            sent(&log),
            [
                (1, "ASKING".into()),
                (1, "TYPE a".into()),
                (1, "ASKING".into()),
                (1, "TTL a".into())
            ]
        );
        // A transaction: ASKING once, in front of MULTI.
        log.lock().unwrap().clear();
        let (t, log) = table(Arc::new(|_, w| match w[0].as_str() {
            "ASKING" | "MULTI" => Value::Okay,
            "EXEC" => Value::Array(vec![Value::Int(1)]),
            _ => Value::SimpleString("QUEUED".into()),
        }));
        let mut conn = Conn::asking(t.conn(1).unwrap().0);
        let mut p = redis::pipe();
        p.atomic().cmd("DEL").arg("a");
        let got: (i64,) = p.query_async(&mut conn).await.unwrap();
        assert_eq!(got, (1,));
        assert_eq!(
            sent(&log),
            [
                (1, "ASKING".into()),
                (1, "MULTI".into()),
                (1, "DEL a".into()),
                (1, "EXEC".into())
            ]
        );
    }

    #[tokio::test]
    async fn a_redirect_inside_a_pipeline_is_seen() {
        let (t, _log) = table(Arc::new(|_, w| {
            if w[0] == "TTL" {
                redirect("MOVED", 5061, 7001)
            } else {
                Value::Int(1)
            }
        }));
        let mut conn = Conn::new(t.conn(0).unwrap().0);
        let mut p = redis::pipe();
        p.cmd("TYPE").arg("a").cmd("TTL").arg("a");
        let r: Result<(i64, i64), _> = p.query_async(&mut conn).await;
        assert!(r.is_err());
        assert_eq!(
            conn.redirect(),
            Some(&Redirect {
                ask: false,
                slot: 5061,
                host: "10.0.0.1".into(),
                port: 7001,
                first: true,
            })
        );
    }

    #[tokio::test]
    async fn requests_to_a_node_without_a_connection_say_so() {
        let (t, log) = table(Arc::new(|_, _| pong()));
        let (r, _) = route(&t, Dest::node(2), ping()).await;
        assert_eq!(
            r,
            Err(LiveError::Failed(nodes::not_connected(
                t.endpoint(2).unwrap()
            )))
        );
        assert!(sent(&log).is_empty());
        // Keys of node 2's slots too ("foo" is slot 12182).
        let (r, _) = route(&t, Dest::key("foo"), ping()).await;
        assert!(r.is_err());
        assert!(sent(&log).is_empty());
    }

    #[tokio::test]
    async fn the_task_routes_each_request() {
        let (t, log) = table(Arc::new(|_, _| pong()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_nodes(
            t,
            move |id, r| {
                let _ = tx.send((id, r));
            },
            |_| {},
        );
        assert!(handle.send_to(ReqId(1), Dest::node(1), ping()));
        assert!(handle.send_to(ReqId(2), Dest::key("bar"), ping()));
        for _ in 0..2 {
            let (_, r) = rx.recv().await.unwrap();
            assert!(r.is_ok());
        }
        let mut got = sent(&log);
        got.sort();
        assert_eq!(got, [(0, "PING".into()), (1, "PING".into())]);
    }
}
