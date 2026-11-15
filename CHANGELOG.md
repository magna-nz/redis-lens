# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/).

`dist` reads this file when it cuts a GitHub Release: the section whose heading names the
tagged version becomes the release notes, and the heading becomes the release title. A
section headed only `## [Unreleased]` is not used. Before you tag, give the changes their
version heading, `## [X.Y.Z]` or `## [X.Y.Z] - YYYY-MM-DD` (see
[docs/releasing.md](docs/releasing.md)).

## [0.1.0]

The first release.

### Added

- **A terminal UI for Redis and Valkey** with six areas: Connections, Overview, Browse, Analyze, Server and Pub/Sub.
  `?` lists every key and runs any action. A light theme, and `NO_COLOR` is respected.
- **Find any key:** `ctrl-f` on any screen searches the whole server by name or pattern.
- **Browse keys and values:** every Redis type, plus JSON, streams and consumer groups, Search indexes, and the
  time series, Bloom, Cuckoo, Count-Min Sketch, Top-K and t-digest module types. Values are decoded as JSON,
  MessagePack, hex and more, including gzip, zlib, LZ4, zstd and Snappy compression and PHP, Pickle, Protobuf
  and Java serialization.
- **See what uses memory:** an analysis on `r` (Quick or Deep) samples keys with `SCAN`, groups them into
  patterns such as `user:{id}:cart`, and shows each pattern's key count, memory and TTL coverage. Analyze has
  Big keys, Expiry (with a forecast of memory freed by TTLs), Changes since the last snapshot, Hot keys (LFU),
  and on a cluster Nodes and Slots.
- **Watch the server:** Overview with the server's own figures and findings, Clients, the slow log, `MONITOR`,
  and a live Pub/Sub tail, including sharded Pub/Sub.
- **Run commands:** a `:` command line with completion and help, replies as text, a table, JSON or raw, repeats
  and pipeline mode.
- **Edit data:** add, edit, expire and delete keys, bulk expire and delete, stream claim, ack and trim, publish,
  Search index create and drop, kill a client, and slow log settings. Every write goes through one write gate:
  production profiles are read-only unless writes are turned on (`w`, `conn set --writes`).
- **Connect to anything:** saved connections in a TOML config file, with usernames and passwords only in the OS
  keychain (`conn add`, `list`, `remove`, `test`, `export`, `import`). `redis://`, `rediss://` and
  StackExchange.Redis connection strings, TLS, ACL users, cluster, Sentinel, the database switch, SSH tunnels
  through a bastion, a token command for cloud IAM sign-in, Microsoft Entra ID behind the `entra-id` feature,
  scanning a verified replica (`conn add --prefer-replica`), and Docker discovery (`redis-lens discover`).
- **A permission check** that probes each command redis-lens needs and turns one feature off, saying why,
  instead of failing.
- **Check it in CI:** `redis-lens scan` (table, `--json`, Markdown, HTML and CSV reports, `--check` rules with
  exit codes), snapshots (`scan --save`, `s` in the TUI), `diff`, `snapshots`, `trend` and `compare`.
- **Gentle on the server:** `SCAN`, never `KEYS`, per-key commands pipelined in batches, and one command budget
  every view shares (`--max-rate`; 5,000 commands a second by default, none for local profiles).
- Supports Redis 6.2 and later and Valkey 8, on macOS, Linux and Windows.
- Releases: prebuilt binaries for five targets, shell and PowerShell installers, a Homebrew formula and
  crates.io, built only after fmt, clippy, the unit tests and the integration tests pass.
