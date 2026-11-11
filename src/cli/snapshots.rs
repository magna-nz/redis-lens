//! `redis-lens snapshots`. Contract stub for the quick-wins wave.

use std::path::PathBuf;

pub async fn run(_config: Option<PathBuf>, _cmd: super::SnapshotsCommand) -> anyhow::Result<()> {
    anyhow::bail!("`redis-lens snapshots` is not built yet")
}
