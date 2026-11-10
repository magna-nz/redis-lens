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

### Changed

- Local profiles have no command budget by default. Other profiles keep 5,000 commands per second.
- Documentation: a full README, a logo, and a changelog.

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
