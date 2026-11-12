# redis-lens

A Rust TUI (ratatui + crossterm, tokio) that profiles Redis and Valkey keyspaces.

## Spec and status

- `spec.md` is the source of truth for behaviour. When a change alters behaviour, update `spec.md` in the same change.
- `status.md` tracks what is done, in progress and next. Update it whenever work lands, starts or gets blocked, and change its "Last updated" date.
- Agents working on a single module report back instead of editing these two files; whoever merges their work updates them.

## Commands

- `cargo build` / `cargo test`: unit tests, no Docker needed.
- `cargo test --features integration-tests --tests`: integration tests. Each test starts its own container through `tests/common/mod.rs` (`common::redis()`, `common::valkey()`, `common::redis_with(&[...])`). Needs Docker running.
- `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all --check`: CI fails on any warning.
- `make demo-up` / `make demo-down`: seeded demo servers on 127.0.0.1:6390 (Redis, ACL users in `demo/users.acl`) and 127.0.0.1:6391 (Valkey, no auth).

## Module map and owners

| Path | Owns |
|---|---|
| `src/model.rs` | Types shared by every module. Changing it is a cross-module change: coordinate first. |
| `src/pattern/` | Key → pattern inference and per-pattern stats (scanner). |
| `src/scan/` | `INFO`/`DBSIZE`, `SCAN` sampling, building `ScanReport` (scanner). |
| `src/config.rs` | Profiles in TOML (connections). |
| `src/secrets.rs` | `SecretStore`, `KeyringStore`, `MemoryStore` (connections). |
| `src/conn/` | Connection strings, connecting, permission checks, Docker discovery (connections). |
| `src/cli/conn.rs` | `conn add/list/remove/test`, `discover` (connections). |
| `src/cli/mod.rs`, `src/cli/scan.rs`, `src/tui/` | CLI definition, `scan` command, TUI (wiring). |
| `tests/common/` | Container helpers shared by all integration tests. |

Function signatures in the stub modules are the contract between owners. Keep them unless the change is agreed.

## Rules

1. **Credentials only in the OS keychain.** Usernames and passwords are stored through `SecretStore` and nowhere else: never in the config file, logs, error messages, `Debug` output or scan snapshot files (`scan --save`, `s` in the TUI). Passwords are never shown anywhere, on screen included, and never appear in insta test snapshots. The one exception for usernames: they may be shown on screen while you add a connection (the add preview), so insta snapshots of that screen may contain made-up fixture usernames. They are still never written to the config file, logs or scan snapshot files. `Profile` has no credential fields on purpose. Use `secrecy::SecretString` and expose it only at the point of use.
2. **Never hurt the server.** Use `SCAN`, never `KEYS`. Pipeline per-key commands in batches. Respect `ScanOptions::max_commands_per_sec`. Read-only profiles (`Profile::is_read_only`, the default for `prod`) never send write commands. Every write goes through the one write gate in spec.md "Write permissions".
3. **Degrade instead of failing.** A missing permission (`NOPERM`) or non-LFU policy turns one feature off and says why; it never aborts a scan.
4. Unit tests sit next to the code (`#[cfg(test)] mod tests`). Anything that talks to a server is an integration test under `tests/`, gated with `#![cfg(feature = "integration-tests")]`. Use `insta` for TUI and report snapshots and `proptest` for parsers and inference.
5. Integration tests run on Redis and Valkey both.
