<div align="center">
  <img src="docs/mark.svg" alt="redis-lens mark" width="104">
  <h1>redis-lens</h1>
  <p><strong>Redis Insight in your terminal, for Redis and Valkey.</strong></p>
  <p>
    <a href="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml"><img src="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.89%2B-B7410E" alt="Rust 1.89+"></a>
    <a href="https://valkey.io/"><img src="https://img.shields.io/badge/Redis-6.2%2B%20%7C%20Valkey%208-DC382D" alt="Redis 6.2+ and Valkey 8"></a>
    <a href="#license"><img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg" alt="License MIT OR Apache-2.0"></a>
  </p>
  <p><a href="https://magna-nz.github.io/redis-lens/"><strong>Documentation</strong></a></p>
</div>

Browse keys, watch the server, run commands, and find out what is using your memory, all from a terminal. It is
safe to point at production: read-only by default, and it never runs `KEYS`.

<div align="center">
  <img src="docs/demo.gif" alt="redis-lens browsing key patterns and a value, then the Server panel and the command line" width="900">
</div>

## What you can do

- **Find any key:** `ctrl-f` from any screen searches the whole server by name or pattern.
- **Browse keys and values:** every Redis type, plus JSON, streams and module types, decoded as JSON, MessagePack, hex and more.
- **See what uses memory:** keys grouped into patterns like `user:{id}:cart`, with big keys, hot keys and keys that never expire.
- **Watch the server:** live stats, clients, the slow log, `MONITOR` and Pub/Sub.
- **Run commands:** a command line with completion and help, and replies as text, a table or JSON.
- **Edit data:** add, edit, expire and delete keys. Production profiles are read-only unless you allow writes.
- **Connect to anything:** ACL users, TLS, cluster, Sentinel, SSH tunnels, cloud IAM sign-in and Docker discovery.
  Passwords stay in the OS keychain.
- **Check it in CI:** save snapshots, compare them over time, and fail a build on rules you set.

## Install

```sh
cargo install --git https://github.com/magna-nz/redis-lens
```

## Get started

```sh
redis-lens conn add local --url redis://localhost:6379
redis-lens            # pick a connection; press ? for every key
```

From a clone, `make demo` starts a seeded Redis and Valkey in Docker to try it on.

## Contributing

```sh
make test   # unit tests
make it     # integration tests against Redis and Valkey (Docker)
make lint   # rustfmt and clippy
```

Behaviour is defined in [`spec.md`](spec.md); a change to behaviour updates it in the same pull request.

## License

[Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
