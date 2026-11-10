//! redis-lens: see what's using memory in Redis and Valkey.
//!
//! Module map (see CLAUDE.md for who owns what):
//! - [`model`]: types shared by every module.
//! - [`pattern`]: turning keys into patterns such as `user:{id}:cart`.
//! - [`scan`]: sampling a live server with `SCAN` and building a [`model::ScanReport`].
//! - [`config`]: saved connection profiles (no secrets).
//! - [`secrets`]: credentials in the OS keychain.
//! - [`conn`]: parsing connection strings, connecting, permission checks, Docker discovery.
//! - [`format`]: human-readable bytes, counts, durations and percentages.
//! - [`cli`]: command-line entry points.
//! - [`tui`]: the terminal UI.

pub mod cli;
pub mod config;
pub mod conn;
pub mod format;
pub mod model;
pub mod pattern;
pub mod scan;
pub mod secrets;
pub mod tui;
