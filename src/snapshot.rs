//! Saving scan reports to disk and loading them back for the Diff view.
//!
//! Owner: engine (wave 4). Contract only; bodies are stubs.
//!
//! Files are `<dir>/<profile>/<YYYYMMDDTHHMMSSZ>.json`, holding a serialized
//! [`ScanReport`]: patterns and example key names, never values or credentials.

use std::path::{Path, PathBuf};

use crate::model::ScanReport;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid snapshot: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("could not find a data directory for this user")]
    NoDataDir,
}

/// One saved snapshot, newest first when listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub profile: String,
    pub taken_at_unix: i64,
}

/// `<data dir>/redis-lens/snapshots`, e.g. `~/Library/Application Support/redis-lens/snapshots`.
pub fn default_dir() -> Result<PathBuf, SnapshotError> {
    todo!("snapshot::default_dir")
}

/// Write `report` under `dir/<profile>/`, named by its `taken_at_unix` in UTC. Writes
/// atomically (temp file, then rename). Profile names are made filesystem-safe.
pub fn save(_dir: &Path, _report: &ScanReport) -> Result<PathBuf, SnapshotError> {
    todo!("snapshot::save")
}

/// Snapshots for a profile, newest first. A missing directory is an empty list.
pub fn list(_dir: &Path, _profile: &str) -> Result<Vec<SnapshotInfo>, SnapshotError> {
    todo!("snapshot::list")
}

pub fn load(_path: &Path) -> Result<ScanReport, SnapshotError> {
    todo!("snapshot::load")
}

/// The newest snapshot for `profile` taken strictly before `before_unix`, if any.
/// Used to compare a fresh scan with the previous one.
pub fn latest_before(
    _dir: &Path,
    _profile: &str,
    _before_unix: i64,
) -> Result<Option<ScanReport>, SnapshotError> {
    todo!("snapshot::latest_before")
}
