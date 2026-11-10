//! Sampling a live server and building a [`ScanReport`].
//!
//! Owner: scanner. Contract only; bodies are stubs.

use redis::aio::ConnectionLike;
use tokio::sync::watch;

use crate::model::{KeySample, PermissionReport, ScanReport, ServerInfo};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Stop after sampling this many keys.
    pub sample_limit: u64,
    /// `COUNT` hint passed to `SCAN`.
    pub scan_count: u32,
    /// Keys per pipelined batch of `TYPE` / `PTTL` / `MEMORY USAGE` / `OBJECT FREQ`.
    pub batch_size: usize,
    /// Upper bound on commands per second sent to the server. `None` means no limit.
    pub max_commands_per_sec: Option<u32>,
    /// Only scan keys matching this glob (`SCAN ... MATCH`).
    pub match_glob: Option<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sample_limit: 100_000,
            scan_count: 1_000,
            batch_size: 500,
            max_commands_per_sec: Some(5_000),
            match_glob: None,
        }
    }
}

/// Progress published while a scan runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub sampled: u64,
    /// The number of keys the scan expects to sample: `min(sample_limit, DBSIZE)`.
    pub target: u64,
    pub done: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error("unexpected reply from the server: {0}")]
    Protocol(String),
}

/// Read `INFO server`, `INFO memory` and `DBSIZE`.
pub async fn server_info<C: ConnectionLike + Send>(_conn: &mut C) -> Result<ServerInfo, ScanError> {
    todo!("server_info")
}

/// Walk the keyspace with `SCAN` and collect up to `opts.sample_limit` samples.
///
/// `with_sizes` and `with_freq` say whether `MEMORY USAGE` and `OBJECT FREQ` may be
/// sent; when false the matching [`KeySample`] fields are `None`. Keys that disappear
/// between `SCAN` and the batch are skipped.
pub async fn sample_keys<C: ConnectionLike + Send>(
    _conn: &mut C,
    _opts: &ScanOptions,
    _with_sizes: bool,
    _with_freq: bool,
    _progress: Option<&watch::Sender<ScanProgress>>,
) -> Result<Vec<KeySample>, ScanError> {
    todo!("sample_keys")
}

/// Run a full scan: server info, sampling, then [`crate::pattern::aggregate`].
///
/// Uses `perms` to decide whether to ask for sizes and LFU counters.
pub async fn scan<C: ConnectionLike + Send>(
    _profile_name: &str,
    _conn: &mut C,
    _opts: &ScanOptions,
    _perms: &PermissionReport,
    _progress: Option<&watch::Sender<ScanProgress>>,
) -> Result<ScanReport, ScanError> {
    todo!("scan")
}
