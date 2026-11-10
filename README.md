# redis-lens

[![CI](https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml/badge.svg)](https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml)

A terminal UI that shows what's using memory in Redis and Valkey.

- **Patterns.** Groups keys into patterns such as `user:{id}:cart` and shows keys, memory, average size and TTL coverage for each.
- **TTL.** Flags patterns where most keys expire but some don't, and patterns with no TTL that keep growing.
- **Diff.** Save a snapshot, then compare later to see what grew, and whether it was more keys or bigger values.
- **Hot keys.** Ranks keys by LFU frequency and shows which node serves them.
- **Connections.** Saved profiles for production, staging and local servers, plus Redis and Valkey containers found in Docker.

> **Status:** early development. The scaffold is in place; the features above are being built.

## Credentials stay in your OS keychain

Usernames and passwords are stored in the macOS Keychain, Windows Credential Manager, or the Secret Service on Linux. The config file only names the profile, host, port, TLS setting and environment. redis-lens never writes credentials to its own files or logs.

Production profiles are read-only, and scans use `SCAN` (never `KEYS`) with a command budget, so they are safe to run against live servers.

## Install

```sh
cargo install redis-lens
```

Prebuilt binaries for Linux, macOS and Windows are attached to each [GitHub Release](https://github.com/magna-nz/redis-lens/releases).

## Usage

```sh
# Save a connection. You are prompted for the password; it goes to the keychain.
redis-lens conn add prod-cache --url rediss://lens-ro@prod-cache.internal:6380 --env prod --ask-password

# Open the TUI
redis-lens

# Or print a report
redis-lens scan prod-cache
```

redis-lens needs a user that can run `SCAN`, `TYPE`, `PTTL`, `MEMORY USAGE`, `OBJECT FREQ`, `INFO` and `DBSIZE`. A minimal read-only ACL user:

```
ACL SETUSER lens-ro on >… ~* resetchannels -@all +ping +hello +client|setinfo +client|setname +select +info +dbsize +scan +type +ttl +pttl +memory|usage +object|freq
```

Without `MEMORY USAGE` you still get key counts and TTLs, just no sizes.

## Try the demo

Needs Docker.

```sh
make demo
```

This starts a Redis on port 6390 (with ACL users) and a Valkey on port 6391, seeds about 120K keys with a few planted problems, saves both as connections (the demo password goes to your keychain), and opens the TUI. `make demo-down` removes it all.

## Development

```sh
make test   # unit tests
make it     # integration tests against Redis and Valkey containers (needs Docker)
make lint   # rustfmt and clippy
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
