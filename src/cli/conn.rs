//! `redis-lens conn ...` and `redis-lens discover`.
//!
//! Owner: connections. Stub until implemented.

use std::path::PathBuf;

use super::ConnCommand;

pub async fn run(_config: Option<PathBuf>, _cmd: ConnCommand) -> anyhow::Result<()> {
    anyhow::bail!("`conn` is not implemented yet")
}

pub async fn discover(_config: Option<PathBuf>) -> anyhow::Result<()> {
    anyhow::bail!("`discover` is not implemented yet")
}
