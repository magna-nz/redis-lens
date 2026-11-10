//! `redis-lens scan <profile>`.
//!
//! Owner: TUI and CLI wiring. Stub until the scanner and connection layer exist.

use std::path::PathBuf;

use super::ScanArgs;

pub async fn run(_config: Option<PathBuf>, _args: ScanArgs) -> anyhow::Result<()> {
    anyhow::bail!("`scan` is not implemented yet")
}
