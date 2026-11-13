//! redis-lens: see what's using memory in Redis and Valkey.
//!
//! Module map (see CLAUDE.md for who owns what):
//! - [`model`]: types shared by every module.
//! - [`pattern`]: turning keys into patterns such as `user:{id}:cart`.
//! - [`scan`]: sampling a live server with `SCAN` and building a [`model::ScanReport`].
//! - [`budget`]: the command budget a scan and the live views share.
//! - [`live`]: the connection kept open after a scan, for the views that poll.
//! - [`config`]: saved connection profiles (no secrets).
//! - [`secrets`]: credentials in the OS keychain.
//! - [`conn`]: parsing connection strings, connecting, permission checks, Docker discovery.
//! - [`check`], [`report`]: `scan --check` rules and `scan --report` files.
//! - [`format`]: human-readable bytes, counts, durations and percentages.
//! - [`snapshot`], [`diff`], [`verdict`]: saved reports, comparing them, TTL risk.
//! - [`cli`]: command-line entry points.
//! - [`tui`]: the terminal UI.
//! - [`value`]: values on screen: decompression, format detection, terminal-safe drawing.

pub mod analysis;
pub mod budget;
pub mod check;
pub mod cli;
pub mod config;
pub mod conn;
pub mod diff;
pub mod format;
pub mod live;
pub mod model;
pub mod pattern;
pub mod report;
pub mod scan;
pub mod secrets;
pub mod snapshot;
pub mod tui;
pub mod value;
pub mod verdict;
