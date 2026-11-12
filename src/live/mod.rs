//! The live connection: after a scan, its connection stays open for the views that poll
//! or look things up (spec "Browsing, server views and writes"). Owner: wave A1; the
//! per-view submodules belong to wave B.
//!
//! Contract: the types and signatures below are fixed. No request sends a write
//! command. Only [`values`] (key values and stream entries, in pages), [`search`]
//! (query results) and the Pub/Sub tail ([`pubsub`]) read values, held as
//! [`crate::value::Raw`] (spec "Values").
//!
//! [`spawn`] starts one task per connected session. Requests arrive on a queue and each
//! runs in a task of its own on a clone of the connection, so a slow lookup never holds
//! up a poll; every command still takes from the session's shared [`Budget`]. Dropping
//! the [`LiveHandle`] ends the task, aborts the requests in flight and, once the scan
//! has let go of its clone too, closes the connection.
//!
//! The per-view `run` functions send commands with [`query`] and [`query_pipe`], which
//! take from the budget first and turn a failure into a [`LiveError`] with
//! [`error_for`].

use std::collections::HashMap;
use std::sync::Arc;

use redis::aio::MultiplexedConnection;
use redis::{FromRedisValue, ServerErrorKind};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::budget::Budget;

pub mod command;
pub mod keys;
pub mod pubsub;
pub mod search;
pub mod server;
pub mod streams;
pub mod values;

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
    /// A page of a key's value, for the key detail screen.
    Value(values::Request),
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
    match req {
        LiveRequest::Server(r) => server::run(conn, budget, r).await.map(LiveReply::Server),
        LiveRequest::Keys(r) => keys::run(conn, budget, r).await.map(LiveReply::Keys),
        LiveRequest::Streams(r) => streams::run(conn, budget, r).await.map(LiveReply::Streams),
        LiveRequest::PubSub(r) => pubsub::run(conn, budget, r).await.map(LiveReply::PubSub),
        LiveRequest::Search(r) => search::run(conn, budget, r).await.map(LiveReply::Search),
        LiveRequest::Value(r) => values::run(conn, budget, r).await.map(LiveReply::Value),
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
    conn: &mut Conn,
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
    conn: &mut Conn,
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

/// The runner's handle on one session's live task. Dropping it stops the task and closes
/// the connection.
#[derive(Debug)]
pub struct LiveHandle {
    queue: mpsc::UnboundedSender<(ReqId, LiveRequest)>,
}

impl LiveHandle {
    /// Queues a request. False when the task has stopped.
    pub fn send(&self, id: ReqId, req: LiveRequest) -> bool {
        self.queue.send((id, req)).is_ok()
    }

    /// A handle with no task behind it, for tests: what it is sent arrives on the
    /// receiver, and dropping the handle closes it.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, mpsc::UnboundedReceiver<(ReqId, LiveRequest)>) {
        let (queue, rx) = mpsc::unbounded_channel();
        (Self { queue }, rx)
    }
}

/// Starts the live task on `conn`. Each finished request is passed to `reply`, in any
/// order; requests may run concurrently, each still taking from `budget`.
pub fn spawn(
    conn: Conn,
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
    conn: Conn,
    budget: Arc<Budget>,
    reply: impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync + 'static,
    on_close: impl FnOnce(Option<String>) + Send + 'static,
) -> LiveHandle {
    let (queue, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let why = serve(conn, budget, rx, &reply).await;
        on_close(why);
    });
    LiveHandle { queue }
}

/// The task's loop. Returns `None` when the handle was dropped, the reason otherwise.
async fn serve(
    conn: Conn,
    budget: Arc<Budget>,
    mut rx: mpsc::UnboundedReceiver<(ReqId, LiveRequest)>,
    reply: &(impl Fn(ReqId, Result<LiveReply, LiveError>) + Send + Sync),
) -> Option<String> {
    // Dropped on return: that aborts whatever is still running.
    let mut tasks: JoinSet<Result<LiveReply, LiveError>> = JoinSet::new();
    let mut in_flight: HashMap<tokio::task::Id, ReqId> = HashMap::new();
    loop {
        tokio::select! {
            next = rx.recv() => {
                // The handle was dropped: closed on purpose.
                let (id, req) = next?;
                let mut conn = conn.clone();
                let budget = Arc::clone(&budget);
                let task = tasks.spawn(async move { run(&mut conn, &budget, req).await });
                in_flight.insert(task.id(), id);
            }
            Some(done) = tasks.join_next_with_id() => {
                let (task, result) = match done {
                    Ok((task, result)) => (task, result),
                    Err(e) => (
                        e.id(),
                        Err(LiveError::Failed("the request stopped unexpectedly".to_string())),
                    ),
                };
                let Some(id) = in_flight.remove(&task) else {
                    continue;
                };
                let lost = result == Err(LiveError::Closed);
                reply(id, result);
                if lost {
                    tasks.abort_all();
                    for (_, id) in in_flight.drain() {
                        reply(id, Err(LiveError::Closed));
                    }
                    rx.close();
                    while let Ok((id, _)) = rx.try_recv() {
                        reply(id, Err(LiveError::Closed));
                    }
                    return Some(CONNECTION_LOST.to_string());
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
    }
}
