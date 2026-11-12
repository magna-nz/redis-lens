//! Live requests for the server view. Owner: wave B. Phase 1: no writes, no values.

use super::{Conn, LiveError};
use crate::budget::Budget;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {}

/// Runs one request. Every command goes through `budget`.
pub async fn run(conn: &mut Conn, budget: &Budget, req: Request) -> Result<Reply, LiveError> {
    let _ = (conn, budget);
    match req {}
}
