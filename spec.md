# redis-lens spec

What redis-lens does and how it should behave. This is the source of truth: when behaviour changes, change this file in the same PR. Progress lives in [status.md](status.md).

## Purpose

A terminal UI that answers "what is using the memory in this Redis or Valkey, and is any of it a mistake?" It has to be safe to point at production and quick to point at a local container.

**Users:** backend developers and SREs who run Redis or Valkey as a cache, queue or session store.

**Non-goals:** a general Redis client or key editor, a monitoring daemon, or anything that writes data. The one exception is a bulk `expire` command, planned for later, which stays opt-in and off on production profiles.

## Supported servers and platforms

- **Servers:** Redis 7.x and Valkey 8.x, standalone, are tested in CI. Redis 6.2 should work (ACLs and `MEMORY USAGE` exist there) but isn't tested yet. Cluster comes in v0.3.
- **Platforms:** macOS, Linux and Windows on x86_64, plus aarch64 for macOS and Linux.
- **Minimum Rust version:** 1.89.

## Views

The TUI has five views, switched with `0` to `4`. The keys are `↑↓` to move, `→←` to expand and collapse, `/` to filter, `s` to snapshot and `q` to quit. Narrow terminals scroll horizontally rather than wrapping.

### 0 Connections
- Lists saved profiles and the Redis or Valkey containers found in Docker.
- For each one it shows the name, environment badge (PROD, STAGE, LOCAL, OTHER), endpoint, auth method, status and memory.
- The detail panel shows the endpoint, server version and topology, auth, where the credentials come from, TLS, and the permission check results.
- `a` adds a connection by pasting a connection string (see [Connection strings](#connection-strings)). The parsed result is previewed with the password masked before anything is saved.
- `enter` connects to the selected profile.

### 1 Patterns
- Key patterns sorted by estimated memory. Each row shows the pattern, type, estimated keys, estimated memory, share bar, average size, percentage with no TTL, and the change since the last snapshot.
- Child patterns group under their shared prefix and can be expanded or collapsed.
- The detail panel shows up to five example keys with their sizes and TTLs, a size distribution (p50, p99, max), a TTL histogram, a one-line verdict, and a suggested command.

### 2 TTL
- Patterns ranked by TTL risk:
  - **mixed** means most keys expire but some don't, which is probably a bug
  - **persistent and growing** is a likely leak
  - **persistent and stable** is treated as intentional
  - **ok**
- Each row shows a TTL histogram over the buckets none, <1m, <1h, <1d, <7d, <30d and >30d.

### 3 Diff
- Compares the current scan with a saved snapshot. For each pattern it shows before, after, the change, and the change as a percentage.
- It also says whether the growth came from more keys or from bigger values.
- New patterns are flagged.

### 4 Hot keys
- Keys ranked by LFU counter (`OBJECT FREQ`), showing pattern, type, size and node.
- Keys that are both hot and big are flagged.
- Needs an LFU eviction policy. Without one, the view says so and offers an opt-in, time-limited `MONITOR` sample (planned for v0.2).

## Scanning

1. Read `INFO server`, `INFO memory` and `DBSIZE`.
2. Walk the keyspace with `SCAN cursor COUNT n [MATCH glob]`. **Never `KEYS`.** Deduplicate the keys SCAN returns.
3. For each batch, pipeline `TYPE`, `PTTL`, `MEMORY USAGE key SAMPLES 5` (when allowed) and `OBJECT FREQ` (when allowed and the policy is LFU).
4. Stop at the sample limit (default 100,000) or when the cursor returns to 0.
5. Stay under a command budget (default 5,000 commands per second).
6. Scale sampled counts to estimates with `DBSIZE / keys seen`, where keys seen counts every distinct key SCAN returned, including keys that vanished or could not be read (for example `NOPERM` from a key ACL). The scale is never below 1, and it is 1 when the SCAN cursor returned to 0 (the whole keyspace was seen) or when `MATCH` is used.

A per-key error such as `NOPERM` drops that field (sizes, frequency) and never aborts the scan.

## Pattern inference

Keys are split on `:` `/` `|`, and on `.` inside segments that aren't IPv4 addresses. Segments are replaced as follows, in this order:

| Segment | Placeholder |
|---|---|
| UUID (8-4-4-4-12 hex) | `{uuid}` |
| ULID (26 Crockford base32 characters) | `{ulid}` |
| only digits | `{id}` |
| hex, at least 16 characters | `{hash}` |
| IPv4 address | `{ip}` |

After that, a literal segment position other than the first with 20 or more distinct values among otherwise identical patterns collapses to `{var}`. The first segment names the namespace, so `user:{id}` and `order:{id}` stay apart, and single-segment keys never collapse.

Examples: `user:88213:cart` → `user:{id}:cart`, `rl:10.0.4.12:29187460` → `rl:{ip}:{id}`.

## Connections and authentication

| Auth | How | Status |
|---|---|---|
| None | No `AUTH`. | v0.1 |
| ACL user and password, or a password only | Both are kept in the OS keychain. | v0.1 |
| TLS | `rediss://` or `ssl=True`, verified against webpki roots. | v0.1 |
| Microsoft Entra ID | `DefaultAzureCredential` (az login, managed identity, workload identity) via redis-rs `entra-id`, behind the `entra-id` cargo feature. | v0.2 |
| AWS ElastiCache IAM | | later |
| Sentinel, SSH tunnel | | later |

### Credential rules

- Usernames and passwords live **only** in the OS keychain: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
- There is one entry per profile, with service `redis-lens` and account `profile:<name>`.
- The config file never holds a credential, and the `Profile` type has no field for one.
- Credentials never appear in logs, error text, `Debug` output or snapshots.
- A password typed into a URL on the command line is accepted, with a warning about shell history. `--password-stdin` and `--ask-password` are the preferred ways in.

### Safety

- Profiles with env `prod` are read-only by default. redis-lens never sends a write command, and the later `expire` command is refused on production profiles.
- Scans can prefer replicas (planned for v0.2).

### Permission check

Before scanning, redis-lens probes each command it needs once: `SCAN`, `TYPE`/`PTTL`, `MEMORY USAGE`, `INFO` and `OBJECT FREQ`. The views degrade according to the result:
- no `MEMORY USAGE` means counts and TTLs but no sizes
- no LFU policy or no `OBJECT FREQ` means no hot keys

A minimal read-only ACL user:

```
ACL SETUSER lens-ro on >… ~* resetchannels -@all +ping +hello +client|setinfo +client|setname +select +info +dbsize +scan +type +ttl +pttl +memory|usage +object|freq
```

### Connection strings

- `redis://[user[:password]@]host[:port][/db]`, and `rediss://` for TLS.
- StackExchange.Redis format: `host:port,password=…,user=…,ssl=True,defaultDatabase=N`. Unknown options are ignored.
- Azure hosts are recognised: `*.redis.cache.windows.net` is Azure Cache for Redis and `*.redis.azure.net` is Azure Managed Redis. The environment is guessed from the host name.

### Docker discovery

Lists running containers whose image is redis, valkey, redis-stack or bitnami/redis and that publish port 6379. A password found in `--requirepass` or in `REDIS_PASSWORD`/`VALKEY_PASSWORD` is held in memory only.

## Command line

```
redis-lens                         open the TUI
redis-lens conn add <name> --url <url> [--env prod|stage|local|other] [--password-stdin | --ask-password] [--entra] [--force]
redis-lens conn list | remove <name> | test <name>
redis-lens scan <name> [--sample N] [--match GLOB] [--json]
redis-lens discover
```

`--config <path>` or `REDIS_LENS_CONFIG` overrides the config location. The default is the platform config directory, for example `~/Library/Application Support/redis-lens/config.toml` on macOS. The file is TOML with `[[profile]]` tables and is written atomically with mode 0600.

## Snapshots (v0.2)

`s` (or `redis-lens scan --save`) writes the current `ScanReport` as JSON to `<data dir>/redis-lens/snapshots/<profile>/<YYYYMMDDTHHMMSSZ>.json`. `redis-lens diff <profile>` scans, compares with the newest earlier snapshot (or `--against <file>`), then saves the new scan. The Diff view compares against the latest snapshot or a chosen one. A snapshot holds patterns and example key names, never values or credentials.

## Quality bar

- **Unit tests** sit next to the code. Parsers and pattern inference also get proptest.
- **TUI screens** get insta snapshots rendered with ratatui's `TestBackend`.
- **Integration tests** run against real Redis and Valkey containers via testcontainers, behind the `integration-tests` feature.
- **CI** runs on every PR: fmt, clippy with `-D warnings`, unit tests on three operating systems, integration tests, the MSRV build, `cargo package`, and a demo seeding check. All are required on `main`.
- **Releases:** a `vX.Y.Z` tag checks the tag matches the version, builds binaries for five targets, creates a GitHub Release and publishes to crates.io.

## Demo

`make demo` starts a seeded Redis (port 6390, ACL users `lens` and `lens-limited`) and a Valkey (port 6391, no auth). It saves them as connections, with the demo password going to the keychain, and opens the TUI. The seed plants carts without TTLs, search results without TTLs, an orphaned import job, a large queue, and hot keys.

## Milestones

- **v0.1:** Connections (none, password, ACL, TLS, Docker), Patterns, the TTL column, the `scan` command, the permission check.
- **v0.2:** the TTL view, snapshots and the Diff view, the Hot keys view, Entra ID, preferring replicas.
- **v0.3:** cluster (scan each primary), a per-node view, comparing two instances.
- **Later:** ElastiCache IAM, Sentinel, SSH tunnels, `expire --dry-run` for non-production profiles, and a MONITOR sampling fallback for hot keys.
