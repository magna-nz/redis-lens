//! Pattern inference: turning keys like `user:88213:cart` into `user:{id}:cart`.
//!
//! Owner: scanner. Contract only; bodies are stubs.

use crate::model::{KeySample, PatternStats};

/// The pattern for one key, from its own segments.
///
/// Splits on `:` `/` `|` `.` (keeping the separators) and replaces segments that look
/// like identifiers with a placeholder: `{id}` for integers, `{uuid}`, `{ulid}`,
/// `{hash}` for long hex strings, `{ip}` for IPv4 addresses.
pub fn pattern_for_key(_key: &str) -> String {
    todo!("pattern_for_key")
}

/// Group sampled keys into patterns and compute per-pattern statistics.
///
/// `scale` is `total_keys / sampled_keys` and turns sampled counts and bytes into
/// keyspace-wide estimates. Patterns are sorted by `est_bytes` (or `est_keys` when no
/// sample has a size), largest first.
pub fn aggregate(_samples: &[KeySample], _scale: f64) -> Vec<PatternStats> {
    todo!("aggregate")
}
