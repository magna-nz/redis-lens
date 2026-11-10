//! `redis-lens diff <profile>`: scan, then compare with the previous snapshot.
//!
//! Owner: engine (wave 4). Stub until implemented.

use std::path::PathBuf;

use super::DiffArgs;

pub async fn run(_config: Option<PathBuf>, _args: DiffArgs) -> anyhow::Result<()> {
    anyhow::bail!("`diff` is not implemented yet")
}
