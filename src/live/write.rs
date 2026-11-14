//! Sends a [`Permit`]'s commands (spec "Write permissions"). This is the only place in
//! `live` that sends a write, and it takes nothing but a permit, which only the gate
//! ([`crate::write::Permit::grant`]) makes. Bulk actions are not run here: they go in
//! batches through [`super::bulk`].
//!
//! Every command takes from the session's [`Budget`]. Writes run one at a time (a
//! process-wide lock): `WATCH` belongs to the connection every view shares, so two
//! checked edits must not overlap.
//!
//! Failures are in redis-lens's own words ([`write_error`]): a `NOPERM` is
//! [`LiveError::Denied`] with the command's name, never the server's text, which names
//! the user.

use std::time::Instant;

use redis::{ErrorKind, RedisError, ServerErrorKind, Value};
use tokio::sync::Mutex;

use super::cmdline::{Output, run_error};
use super::{Conn, LiveError, error_for, is_unknown_command};
use crate::budget::Budget;
use crate::value::{self, Raw};
use crate::write::{NewKey, Permit, WriteOp};

/// One write at a time on the shared connection.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

/// How a write went.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// A status reply (`OK`).
    Ok,
    /// An integer reply: how many were removed, `0` or `1` for `RENAMENX`, `HSETNX`,
    /// `EXPIRE`, `PERSIST`; for `HEXPIRE` and `HPERSIST` the one field's code.
    Int(i64),
    /// A string reply, escaped: the new entry's ID for `XADD`.
    Text(String),
    /// The value changed since it was loaded (`WATCH`), so nothing was written.
    Changed,
    /// `N`: the key exists already, so nothing was written.
    Exists,
    /// `XAUTOCLAIM`: the IDs claimed (escaped), and how many pending entries were gone
    /// from the stream and so deleted from the pending list (Redis 7.0 and later).
    Claimed { ids: Vec<String>, deleted: usize },
    /// A command typed on the command line, drawn like `redis-cli`. Values: on screen
    /// only.
    Ran(Output),
}

/// Runs `permit`'s op.
pub async fn run(conn: &mut Conn, budget: &Budget, permit: Permit) -> Result<Reply, LiveError> {
    let op = permit.op();
    if matches!(op, WriteOp::Bulk { .. }) {
        return Err(LiveError::Failed(
            "a bulk action runs in batches of its own".to_string(),
        ));
    }
    let _one = ONE_AT_A_TIME.lock().await;
    let name = op.command();
    match op {
        WriteOp::SetString { key, old, .. } => {
            let n = old.len();
            let mut check = redis::pipe();
            check
                .cmd("WATCH")
                .arg(key)
                .cmd("STRLEN")
                .arg(key)
                .cmd("GETRANGE")
                .arg(key)
                .arg(0)
                .arg(i64::try_from(n).unwrap_or(i64::MAX) - 1);
            let (_, len, now): (Value, i64, Vec<u8>) = watched(conn, budget, &check).await?;
            if usize::try_from(len).ok() != Some(n) || (n > 0 && now != old.as_bytes()) {
                unwatch(conn, budget).await;
                return Ok(Reply::Changed);
            }
            transaction(conn, budget, &name, op).await
        }
        WriteOp::LSet {
            key, index, old, ..
        }
        | WriteOp::LDel {
            key, index, old, ..
        } => {
            let mut check = redis::pipe();
            check
                .cmd("WATCH")
                .arg(key)
                .cmd("LINDEX")
                .arg(key)
                .arg(*index);
            let (_, now): (Value, Option<Vec<u8>>) = watched(conn, budget, &check).await?;
            if now.as_deref() != Some(old.as_bytes()) {
                unwatch(conn, budget).await;
                return Ok(Reply::Changed);
            }
            transaction(conn, budget, &name, op).await
        }
        WriteOp::NewKey {
            value: NewKey::String(_),
            ..
        } => Ok(set_nx_reply(&send_one(conn, budget, &name, op).await?)),
        WriteOp::NewKey { key, .. } => {
            let mut check = redis::pipe();
            check.cmd("WATCH").arg(key).cmd("EXISTS").arg(key);
            let (_, exists): (Value, i64) = watched(conn, budget, &check).await?;
            if exists != 0 {
                unwatch(conn, budget).await;
                return Ok(Reply::Exists);
            }
            transaction(conn, budget, &name, op).await
        }
        WriteOp::SReplace { .. } => transaction(conn, budget, &name, op).await,
        WriteOp::XAutoClaim { .. } => {
            let v = send_one(conn, budget, &name, op).await?;
            Ok(claimed(&v))
        }
        WriteOp::Command { args, .. } => {
            let mut c = redis::Cmd::new();
            for a in args.as_slice() {
                c.arg(a.as_slice());
            }
            budget.take(1).await;
            let start = Instant::now();
            let r: Result<Value, RedisError> = c.query_async(conn).await;
            let took = start.elapsed();
            match r {
                Ok(v) => Ok(Reply::Ran(Output::of(&v, took))),
                Err(e) => Ok(Reply::Ran(Output::error_line(run_error(&name, &e)?, took))),
            }
        }
        _ => single(conn, budget, &name, op).await,
    }
}

fn incomplete() -> LiveError {
    LiveError::Failed("the server's answer could not be read".to_string())
}

/// A failure of the write `name` in redis-lens's own words. Two server errors are
/// about the key, not the user, and are said plainly; the rest go through
/// [`error_for`].
pub fn write_error(name: &str, e: &RedisError) -> LiveError {
    if e.kind() == ErrorKind::Server(ServerErrorKind::NoPerm) || is_unknown_command(e) {
        return error_for(name, e);
    }
    let detail = e.detail().unwrap_or("").to_ascii_lowercase();
    if e.code() == Some("WRONGTYPE") {
        return LiveError::Failed(format!("{name} failed: the key holds another type now"));
    }
    match e.kind() {
        ErrorKind::Server(_) if detail.contains("no such key") => {
            LiveError::Failed(format!("{name} failed: the key no longer exists"))
        }
        ErrorKind::Server(_) if detail.contains("index out of range") => {
            LiveError::Failed(format!("{name} failed: the list is shorter now"))
        }
        ErrorKind::Server(_)
            if detail.contains("unknown index") || detail.contains("not found") =>
        {
            LiveError::Failed(format!("{name} failed: no such index"))
        }
        ErrorKind::Server(_) if detail.contains("already exists") => {
            LiveError::Failed(format!("{name} failed: it already exists"))
        }
        ErrorKind::Server(_) if detail.contains("no such client") => {
            LiveError::Failed(format!("{name} failed: no such client"))
        }
        ErrorKind::Server(_) if detail.contains("nogroup") || detail.contains("no such group") => {
            LiveError::Failed(format!("{name} failed: no such consumer group"))
        }
        _ => error_for(name, e),
    }
}

fn cmd_of(words: &[Vec<u8>]) -> redis::Cmd {
    let mut c = redis::Cmd::new();
    for w in words {
        c.arg(w.as_slice());
    }
    c
}

async fn send_pipe(
    conn: &mut Conn,
    budget: &Budget,
    name: &str,
    pipe: &redis::Pipeline,
) -> Result<Value, LiveError> {
    budget
        .take(u32::try_from(pipe.len()).unwrap_or(u32::MAX))
        .await;
    pipe.query_async(conn)
        .await
        .map_err(|e| write_error(name, &e))
}

/// Sends `WATCH key` and the check read after it, and reads them. When that fails
/// (the key holds another type now, say) the connection stops watching.
async fn watched<T: redis::FromRedisValue>(
    conn: &mut Conn,
    budget: &Budget,
    check: &redis::Pipeline,
) -> Result<T, LiveError> {
    let r = send_pipe(conn, budget, "WATCH", check)
        .await
        .and_then(|v: Value| redis::from_redis_value(v).map_err(|_| incomplete()));
    if r.is_err() {
        unwatch(conn, budget).await;
    }
    r
}

/// Sends `UNWATCH`, after a check found the value changed. A failure is ignored: the
/// next `EXEC` or the connection's next `WATCH` clears it anyway.
async fn unwatch(conn: &mut Conn, budget: &Budget) {
    budget.take(1).await;
    let _: Result<Value, _> = redis::cmd("UNWATCH").query_async(conn).await;
}

/// Sends the op's one command and reads its reply.
async fn send_one(
    conn: &mut Conn,
    budget: &Budget,
    name: &str,
    op: &WriteOp,
) -> Result<Value, LiveError> {
    let cmds = op.commands();
    let [words] = cmds.as_slice() else {
        return Err(LiveError::Failed(format!("{name} is not one command")));
    };
    budget.take(1).await;
    cmd_of(words)
        .query_async(conn)
        .await
        .map_err(|e| write_error(name, &e))
}

async fn single(
    conn: &mut Conn,
    budget: &Budget,
    name: &str,
    op: &WriteOp,
) -> Result<Reply, LiveError> {
    let v = send_one(conn, budget, name, op).await?;
    Ok(reply_of(&v))
}

/// The op's commands in `MULTI`/`EXEC`, after a `WATCH` (or on their own for a set
/// member's replacement). An aborted `EXEC` (the watched key changed) is
/// [`Reply::Changed`].
async fn transaction(
    conn: &mut Conn,
    budget: &Budget,
    name: &str,
    op: &WriteOp,
) -> Result<Reply, LiveError> {
    let mut pipe = redis::pipe();
    pipe.atomic();
    for words in op.commands() {
        pipe.add_command(cmd_of(&words));
    }
    // MULTI and EXEC count too.
    budget
        .take(u32::try_from(pipe.len() + 2).unwrap_or(u32::MAX))
        .await;
    let v: Value = pipe
        .query_async(conn)
        .await
        .map_err(|e| write_error(name, &e))?;
    match v {
        Value::Nil => Ok(Reply::Changed),
        Value::Array(items) => match items.last() {
            Some(Value::ServerError(_)) => Err(LiveError::Failed(format!("{name} failed"))),
            Some(last) => Ok(reply_of(last)),
            None => Ok(Reply::Ok),
        },
        other => Ok(reply_of(&other)),
    }
}

/// `SET key value NX`: `OK` when it was set, nil when the key exists.
fn set_nx_reply(v: &Value) -> Reply {
    match v {
        Value::Nil => Reply::Exists,
        _ => Reply::Ok,
    }
}

fn reply_of(v: &Value) -> Reply {
    match v {
        Value::Int(n) => Reply::Int(*n),
        Value::Okay | Value::SimpleString(_) => Reply::Ok,
        Value::BulkString(b) => Reply::Text(value::escape(&String::from_utf8_lossy(b))),
        Value::Array(items) if items.len() == 1 => reply_of(&items[0]),
        Value::Nil => Reply::Text(String::new()),
        _ => Reply::Ok,
    }
}

/// `XAUTOCLAIM`'s reply: `[next-cursor, [[id, fields] …], [deleted id …]]` (the third
/// part since Redis 7.0). The IDs are kept; the fields, values, are dropped here.
fn claimed(v: &Value) -> Reply {
    let Value::Array(parts) = v else {
        return Reply::Claimed {
            ids: Vec::new(),
            deleted: 0,
        };
    };
    let ids = match parts.get(1) {
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(|e| match e {
                Value::Array(pair) => match pair.first() {
                    Some(Value::BulkString(id)) => {
                        Some(value::escape(&String::from_utf8_lossy(id)))
                    }
                    _ => None,
                },
                // A claimed ID whose entry was deleted comes back as nil before 7.0.
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let deleted = match parts.get(2) {
        Some(Value::Array(d)) => d.len(),
        _ => 0,
    };
    Reply::Claimed { ids, deleted }
}

/// A unique marker for [`WriteOp::LDel`]: set over the element, then removed with
/// `LREM key 1 <marker>`, so equal elements elsewhere in the list stay.
pub fn list_marker(nonce: u64) -> String {
    format!("__redis-lens:deleted:{}:{nonce:016x}__", std::process::id())
}

/// The raw value the op writes, for a test or a view that wants to show it again.
pub fn written_value(op: &WriteOp) -> Option<&Raw> {
    match op {
        WriteOp::SetString { value, .. }
        | WriteOp::HSet { value, .. }
        | WriteOp::HSetNx { value, .. }
        | WriteOp::LSet { value, .. }
        | WriteOp::RPush { value, .. }
        | WriteOp::SReplace { value, .. }
        | WriteOp::JsonSet { value, .. } => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn replies_are_read_without_values() {
        assert_eq!(reply_of(&Value::Int(1)), Reply::Int(1));
        assert_eq!(reply_of(&Value::Okay), Reply::Ok);
        assert_eq!(reply_of(&Value::Array(vec![Value::Int(2)])), Reply::Int(2));
        assert_eq!(reply_of(&bulk("1-0\x1b")), Reply::Text("1-0\\x1b".into()));
        let v = Value::Array(vec![
            bulk("0-0"),
            Value::Array(vec![
                Value::Array(vec![
                    bulk("1-0"),
                    Value::Array(vec![bulk("f"), bulk("secret")]),
                ]),
                Value::Array(vec![bulk("2-0"), Value::Array(vec![])]),
            ]),
            Value::Array(vec![bulk("3-0")]),
        ]);
        let r = claimed(&v);
        assert_eq!(
            r,
            Reply::Claimed {
                ids: vec!["1-0".into(), "2-0".into()],
                deleted: 1
            }
        );
        assert!(!format!("{r:?}").contains("secret"));
    }

    #[test]
    fn set_nx_says_when_the_key_exists() {
        assert_eq!(set_nx_reply(&Value::Nil), Reply::Exists);
        assert_eq!(set_nx_reply(&Value::Okay), Reply::Ok);
    }

    #[test]
    fn markers_are_unique() {
        assert_ne!(list_marker(1), list_marker(2));
    }

    #[test]
    fn write_errors_are_in_redis_lens_words() {
        let e = |kind, code: &'static str, detail: &str| {
            RedisError::from((ErrorKind::Server(kind), code, detail.to_string()))
        };
        assert_eq!(
            write_error(
                "HSET",
                &e(
                    ServerErrorKind::NoPerm,
                    "NOPERM",
                    "User lens-rw has no permissions to run the 'hset' command"
                )
            ),
            LiveError::Denied("HSET".into())
        );
        assert_eq!(
            write_error(
                "HSET",
                &redis::make_extension_error(
                    "WRONGTYPE".to_string(),
                    Some("Operation against a key holding the wrong kind of value".to_string())
                )
            ),
            LiveError::Failed("HSET failed: the key holds another type now".into())
        );
        assert_eq!(
            write_error(
                "RENAMENX",
                &e(ServerErrorKind::ResponseError, "ERR", "no such key")
            ),
            LiveError::Failed("RENAMENX failed: the key no longer exists".into())
        );
        let other = write_error(
            "SET",
            &e(
                ServerErrorKind::ResponseError,
                "ERR",
                "user lens-rw password hunter2",
            ),
        );
        assert!(!other.to_string().contains("hunter2"));
    }
}
