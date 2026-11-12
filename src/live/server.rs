//! Live requests for the server view. Owner: wave B (`Ping` and `Pong`: wave A1).
//! Phase 1: no writes, no values.

use super::{Conn, LiveError, query};
use crate::budget::Budget;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `PING`: checks the live connection end to end.
    Ping,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The server answered `PING`.
    Pong,
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(conn: &mut Conn, budget: &Budget, req: Request) -> Result<Reply, LiveError> {
    match req {
        Request::Ping => {
            let _: String = query(conn, budget, "PING", &redis::cmd("PING")).await?;
            Ok(Reply::Pong)
        }
    }
}
