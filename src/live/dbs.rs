//! The database list's request (spec "Database switch (P4)"): `INFO keyspace`, one
//! command from the budget each time the list opens. Nothing polls it.

use redis::Value;

use crate::budget::Budget;
use crate::model::KeyspaceInfo;
use crate::scan::context::parse_keyspace_all;

use super::{LiveConn, LiveError, query};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `INFO keyspace`: every database that holds keys.
    Keyspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The `dbN` lines, in database order. A database without keys has no line.
    Keyspace(Vec<KeyspaceInfo>),
}

pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Keyspace => {
            let mut c = redis::cmd("INFO");
            c.arg("keyspace");
            let v: Value = query(conn, budget, "INFO keyspace", &c).await?;
            let text = match v {
                Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                Value::SimpleString(s) => s,
                Value::VerbatimString { text, .. } => text,
                _ => String::new(),
            };
            Ok(Reply::Keyspace(parse_keyspace_all(&text)))
        }
    }
}
