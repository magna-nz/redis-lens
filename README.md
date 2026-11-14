<div align="center">
  <img src="docs/mark.svg" alt="redis-lens mark" width="104">
  <h1>redis-lens</h1>
  <p><strong>A terminal UI that shows what is using memory in Redis and Valkey, and whether any of it is a mistake.</strong></p>
  <p>
    <a href="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml"><img src="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
    <a href="https://crates.io/crates/redis-lens"><img src="https://img.shields.io/crates/v/redis-lens" alt="crates.io"></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.89%2B-B7410E" alt="Rust 1.89+"></a>
    <a href="https://valkey.io/"><img src="https://img.shields.io/badge/Redis-6.2%2B%20%7C%20Valkey%208-DC382D" alt="Redis 6.2+ and Valkey 8"></a>
    <a href="#license"><img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg" alt="License MIT OR Apache-2.0"></a>
  </p>
  <p><a href="https://magna-nz.github.io/redis-lens/"><strong>Documentation</strong></a></p>
</div>

`INFO memory` tells you how much memory your Redis or Valkey uses. It does not tell you which keys. redis-lens
samples the keyspace under a command budget, groups keys into patterns such as `user:{id}:cart`, and shows what each
pattern costs and which ones look like mistakes: carts that never expire, a cache that only grows, an import job that
never cleans up, one huge queue. Around that it browses keys and values, watches the server, streams, Pub/Sub and
Search, and runs commands, like Redis Insight in the terminal, and it is built to be pointed at production.

<div align="center">
  <img src="docs/demo.gif" alt="redis-lens opening the demo Redis, analyzing the keys, browsing the key patterns and a value, then the Analyze and Server panels" width="900">
</div>

## Install

```sh
cargo install redis-lens                       # Rust 1.89 or newer
cargo install redis-lens --features entra-id   # with Microsoft Entra ID sign-in
```

Or download a binary for Linux, macOS or Windows from [GitHub Releases](https://github.com/magna-nz/redis-lens/releases).

## Quickstart

```sh
redis-lens conn add staging --url redis://lens-ro@staging.internal:6379 --env stage --ask-password
redis-lens conn test staging        # connect and run the permission check
redis-lens                          # pick staging, enter, then r to analyze; ? lists every key
redis-lens scan staging --save      # the same analysis from the command line, kept as a snapshot
make demo                           # or try a seeded Redis and Valkey in Docker (from a clone)
```

## Features

- **Memory by pattern:** keys grouped into patterns with keys, memory, average size and the share without a TTL,
  each marked pattern explained in a sentence.
- **Analysis:** big keys, hot keys by LFU counter, an expiry forecast of the memory TTLs free within 1 h to 30 d,
  and recommendations, from a Quick (10,000 keys) or Deep (100,000 keys) sample whose cost is shown first.
- **Browse:** pattern tree, key lists and paged values with format detection (JSON, MessagePack, hex and more,
  decompressing gzip, zlib, LZ4 and zstd), streams and consumer groups, Search indexes and queries.
- **Server:** live figures with sparklines, clients grouped by library and address, the slow log, and a time-boxed
  `MONITOR`.
- **Pub/Sub and a `:` command line** with `redis-cli`-style replies, completion and a command helper.
- **Connections:** ACL users, TLS, IAM token commands (AWS, Azure, Google Cloud), Entra ID, SSH bastions,
  Sentinel, cluster and Docker discovery.
- **History and CI:** snapshots, `diff`, `trend` and `compare`, `scan --check` rules with exit codes, and
  Markdown, HTML and CSV reports.
- **Writes when you want them:** key edits, bulk expire and delete, publish and more, behind a write gate.

## Safety

- **Credentials live only in the OS keychain**, never in the config file, logs, error messages, snapshots or on
  screen.
- **It never hurts the server:** `SCAN`, never `KEYS`; per-key commands pipelined; every view under one command
  budget; nothing about a key is read until you analyze or open it.
- **Production is read-only by default.** A write is sent only when the server's ACL, the profile and a
  confirmation all allow it.
- **A missing permission turns one feature off** and says why. It never aborts a scan.

## Documentation

The full guide is at **https://magna-nz.github.io/redis-lens/**: every screen, the key reference, connections and
sign-in, the safety model and a minimal ACL user, how the analysis works, snapshots and CI, the command-line
reference, and a comparison with Redis Insight. [`spec.md`](spec.md) is the source of truth for behaviour and
[`status.md`](status.md) tracks progress.

## Contributing

Issues and pull requests are welcome. Before you open a pull request:

```sh
make test   # unit tests, no Docker
make it     # integration tests against Redis and Valkey in Docker
make lint   # rustfmt check and clippy with -D warnings
```

A change that alters behaviour updates [`spec.md`](spec.md) in the same pull request. TUI screens are covered by
insta snapshots, and anything that talks to a server is an integration test run on both Redis and Valkey.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
