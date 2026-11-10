# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Snapshots. `s` in the TUI and `redis-lens scan --save` write the scan as JSON to the data directory.
- `redis-lens diff <profile>`: scans, compares with the newest earlier snapshot (or `--against <file>`), then saves
  the new scan. Supports `--json` and `--max-rate`.
- TTL view: patterns ranked by TTL risk (mixed, persistent and growing, persistent and stable, ok) with a TTL
  histogram.
- Diff view: before, after and change per pattern, and whether growth came from more keys or bigger values.
- Hot keys view: keys ranked by LFU counter, with hot and big keys flagged. Hot keys are also in scan reports.
- A change-since-last-snapshot column in the Patterns view.
- `--max-rate` on `scan` and `diff`. `0` means no limit.
- Optional Microsoft Entra ID sign-in, behind the `entra-id` cargo feature (developer-tools credential).
- `make gif`, a Makefile target that records the demo GIF.
- Scan a replica instead of the primary: `conn add --prefer-replica`. redis-lens reads `INFO replication`, uses the
  first online replica, and falls back to the primary (saying why) if it can't reach it.
- Replica verification: before a replica is scanned, its `INFO replication` must show it is a replica with the
  primary's replication ID (`master_replid`). A different server on the announced address is never scanned or
  snapshotted. A replica that passes without credentials is scanned without them. Credentials go only to a
  candidate that demands auth and is on the primary's host or not loopback after DNS resolution; the resolved
  address is the one connected to. Announced addresses of `0.0.0.0`, `::` or empty are skipped;
  IPv6 addresses and host names are accepted.
- The TUI says when it is waiting on the OS keychain, so a hidden macOS permission dialog doesn't look like a hang.

### Changed

- Local profiles have no command budget by default. Other profiles keep 5,000 commands per second.
- Documentation: a full README, a logo, a demo GIF, and a changelog.
- Profile names starting with `docker:`, in any case, are reserved for containers found by discovery.
- A pattern that only appears after a sampled (partial) scan no longer counts as growing unless it passes a
  threshold: more than 1 MiB with sizes, or without sizes at least 50 sampled keys and 1% of the newer scan's
  sampled keys.
- `--prefer-replica` reads the keychain once per scan and uses the same credentials for the primary and the replica.
  The 2 s replica timeout covers only connecting to and verifying the replica.
- `make gif` uses a temporary config with its own demo connections and removes them afterwards.
- IPv6 endpoints are shown in brackets, such as `[::1]:6379`.

## [0.1.0] - unreleased

### Added

- Saved connections in a TOML config file, with usernames and passwords stored only in the OS keychain
  (`conn add`, `list`, `remove`, `test`). `--password-stdin` and `--ask-password` keep passwords out of shell history.
- Connection strings: `redis://`, `rediss://` and the StackExchange.Redis format. Azure hosts are recognised.
- TLS, ACL users and password-only auth.
- Docker discovery of Redis and Valkey containers (`redis-lens discover`).
- Permission check that probes each command redis-lens needs and degrades instead of failing.
- The scanner: `SCAN`-based sampling with pipelined per-key commands and a command budget.
- Pattern inference: UUID, ULID, numeric id, hash and IPv4 placeholders, with high-cardinality segments collapsed
  to `{var}`.
- `redis-lens scan` with table and `--json` output, `--sample` and `--match`.
- The TUI: Connections view (including an add-connection flow with a masked preview) and Patterns view.
- CI for fmt, clippy, tests on three operating systems, integration tests against Redis and Valkey, MSRV and
  packaging, plus a release workflow and a seeded demo environment.

### Security

- Password input is masked as fixed-width bullets, so its length is not revealed.
- A half-typed URL password is masked before the `@` is entered.
- Fixes to credential handling when saving a connection fails part-way (credential rollback).
