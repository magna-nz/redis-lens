//! The terminal UI.
//!
//! Owner: TUI and CLI wiring. Stub until the scanner and connection layer exist.

use std::path::PathBuf;

pub async fn run(_config: Option<PathBuf>) -> anyhow::Result<()> {
    anyhow::bail!("the TUI is not implemented yet")
}
