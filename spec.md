# redis-lens spec

What redis-lens does and how it should behave. This is the source of truth: when behaviour changes, change this file in the same PR. Progress lives in [status.md](status.md).

## Purpose

A terminal UI that answers "what is using the memory in this Redis or Valkey, and is any of it a mistake?" It has to be safe to point at production and quick to point at a local container.

**Users:** backend developers and SREs who run Redis or Valkey as a cache, queue or session store.

**Non-goals:** a general Redis client or key editor, a monitoring daemon, or anything that writes data. The one exception is a bulk `expire` command, planned for later, which stays opt-in and off on production profiles.

## Supported servers and platforms

- **Servers:** Redis 7.x and Valkey 8.x, standalone, are tested in CI. Redis 6.2 is tested in CI too (a separate integration job). Cluster comes in v0.3.
- **Platforms:** macOS, Linux and Windows on x86_64, plus aarch64 for macOS and Linux.
- **Minimum Rust version:** 1.89.

## Views

The TUI has five views, switched with `0` to `4`. The keys are listed in [Keys](#keys) and in the `?` overlay; both, and the key bar at the bottom of the screen, come from one table in `src/tui/keymap.rs`. Every list view moves with `↑↓` (`j` `k`), `PgUp` `PgDn` (one page: as many rows as the list shows on screen, at least one; it stops at the first and last row) and `Home` `End` (`g` `G`). Narrow terminals never wrap: names are shortened with `…`. On the Patterns view, when the pattern column would get narrower than 12 cells, columns are hidden in this order: the share bar (the share percentage stays), then Δ, then the average size. Only when all three are hidden is a row too wide for the terminal cut at the right edge. Other views cut a row too wide at the right edge. On the selected row, text that doesn't fit its column scrolls across: it rests for 1 s, scrolls about 5 columns a second until its end is visible, holds 1 s and starts again. On the Patterns view only the name scrolls; the tree indent and the `▾`/`▸` mark stay in place. Selecting a different row, switching views or starting a new scan run restarts it; the same row moving to another position does not.

### 0 Connections
- Lists saved profiles and the Redis or Valkey containers found in Docker.
- For each one it shows the name, environment badge (PROD, STAGE, LOCAL, OTHER), endpoint, auth method, status and memory.
- The detail panel shows the endpoint, server version and topology, auth, where the credentials come from, TLS, and the permission check results.
- `a` adds a connection by pasting a connection string (see [Connection strings](#connection-strings)). The parsed result is previewed with the password masked before anything is saved.
- For a Docker container the auth column shows what discovery found out (see [Docker discovery](#docker-discovery)): `password`, `none`, `required` or `unknown`. The detail panel adds where the credentials come from: for `password`, the container, held in memory only; for `required`, not found, so save it as a profile with credentials.
- `enter` connects to the selected profile. When a connect for a keychain profile takes longer than 2 s, the status line and the connecting panel say "Waiting for the OS keychain. If a permission dialog is open, choose Always Allow." After 15 s the keychain read gives up (see [Credential rules](#credential-rules)).
- Each `enter` starts a new scan run, also when the same entry is connected again. Connection results, progress, partial results and results of a replaced or cancelled run are ignored.

### 1 Patterns
- Key patterns, sorted by estimated memory by default. `o` cycles the sort: memory → keys → average size → no-TTL % → name → memory. Name sorts A to Z, the others largest first; ties go by name, A to Z. Without sizes (`MEMORY USAGE` denied) memory and average size are skipped, and the default sorts by keys. Groups sort by their totals (summed keys and memory, the group's average size and no-TTL share, the group label for name) and children sort inside their group by the same key. The header marks the sorted column with `▼` (`▲` for name), the title says `sorted by <key>`, and a short notice confirms the change. The sort lasts while the session's scans run and finish, and is not saved between runs; connecting starts again by memory.
- Each row shows the pattern, type, estimated keys, estimated memory, share bar, average size, percentage with no TTL, and the change since the last snapshot.
- Child patterns group under their shared prefix and can be expanded or collapsed.
- The selection follows its row, not its position, whenever the rows change (new partial results, the finished scan, another sort): a group by its shared prefix, a pattern by its name. A pattern now inside a collapsed group selects the group; a group that became a single pattern selects that pattern; a group or pattern that merged into `{var}` selects what it merged into. When the row is gone the selection stays at the same position, or the last row if there are fewer.
- A collapsed group stays collapsed when its patterns change: it is remembered by its prefix and by its patterns. If its prefix changes because part of it merged into `{var}` (`tenant.a1:*` and `tenant.a2:*` become `tenant.{var}:*`), or splits back out, the new group is collapsed when it holds one of the collapsed group's patterns, equal or up to `{var}`. A collapsed group that disappears or becomes a single pattern is collapsed again if it comes back. New groups start expanded.
- While a scan runs, the view shows partial results aggregated from the keys sampled so far (no extra server commands, at most four updates a second), marked `partial · N / M sampled` in the title and without the Δ column. The finished scan replaces them; a failed or cancelled scan clears them (see [Rescan](#rescan) for the previous results kept during a rescan). The TTL, Diff and Hot keys views wait for the finished scan. `s` never saves partial results: it says to wait for the scan to finish.
- The detail panel shows up to five example keys with their sizes and TTLs, a size distribution (p50, p99, max), a TTL histogram, a one-line verdict, and a suggested command.

### 2 TTL
- Patterns ranked by TTL risk:
  - **mixed** means most keys expire but some don't, which is probably a bug
  - **persistent and growing** is a likely leak
  - **persistent and stable** is treated as intentional
  - **ok**
- Each row shows a TTL histogram over the buckets none, <1m, <1h, <1d, <7d, <30d and >30d.

### 3 Diff
- Compares the current scan with the newest earlier snapshot of the same profile taken with the same `MATCH` glob. Comparing with a chosen snapshot is CLI-only, through `redis-lens diff --against`. For each pattern it shows before, after, the change, and the change as a percentage.
- It also says whether the change came from the key count or the average size: more keys, fewer keys, bigger values or smaller values. Memory is keys × average size, so the cause is whichever of `ln(keys after / keys before)` and `ln(average after / average before)` is larger in magnitude, and its direction always follows the change in memory. A change under 1% or under 64 KiB is "unchanged".
- New and gone patterns are flagged when both scans saw the whole keyspace or used the same `MATCH` glob. Otherwise a pattern found in only one scan is shown as "not in the other scan", and it never counts as growth in the TTL view. When either scan was partial (`complete` false), a `New` pattern counts as growing in the TTL view only if it passes a threshold; `PatternDiff.partial` records this. With sizes that is the usual byte threshold, which for a new pattern is more than 1 MiB. Without sizes there are no keys before, so 5% of them would let any key through: the pattern needs at least 50 sampled keys and at least 1% of the newer scan's sampled keys. Both sides are sampled counts (`PatternDiff.pattern_sampled_keys_after` and `PatternDiff.sampled_keys_after`, not part of the JSON output), because estimated keys are scaled up and a handful of sampled keys could pass a sampled floor.

### 4 Hot keys
- Up to 50 sampled keys with an LFU counter (`OBJECT FREQ`) above 0, ranked by that counter, showing the key, pattern, type, counter and size. A node column comes with cluster support (v0.3).
- Keys with a counter of at least 100 and a size of at least 10 MiB are flagged as hot and big.
- Needs an LFU eviction policy. Without one, the view says so and offers an opt-in, time-limited `MONITOR` sample (planned for v0.2).

### Keys

#### Everywhere

| Keys | Action |
|---|---|
| `0-4` | Switch view, 0 Connections to 4 Hot keys |
| `?` | Show or hide this key list |
| `a` | Add a connection |
| `q` `ctrl-c` | Quit |

#### Connections

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `enter` | Connect and scan |
| `r` | Look for Docker containers again |

#### Patterns

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `→` `←` `l` `h` | Expand or collapse the selected group |
| `enter` | Expand or collapse |
| `/` | Filter patterns by name |
| `esc` | Clear the filter |
| `o` | Sort by memory, keys, avg size, no-TTL % or name |
| `y` | Copy the selected pattern to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `r` | Scan again, keeping old results meanwhile |
| `s` | Save a scan snapshot file |

#### TTL

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `y` | Copy the selected pattern to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `r` | Scan again, keeping old results meanwhile |
| `s` | Save a scan snapshot file |

#### Diff

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `y` | Copy the selected pattern to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `r` | Scan again, keeping old results meanwhile |
| `s` | Save a scan snapshot file |

#### Hot keys

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `y` | Copy the selected key name to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `r` | Scan again, keeping old results meanwhile |
| `s` | Save a scan snapshot file |

The key bar shows the main keys of the current view, from this table: Connections `↑↓ Move · enter Connect · a Add · r Rescan Docker · 0-4 View · ? Help · q Quit`; Patterns `↑↓ Move · →← Fold · / Filter · o Sort · s Snapshot · 0-4 View · ? Help · q Quit`; TTL, Diff and Hot keys with a result `↑↓ Move · r Rescan · s Snapshot · 0-4 View · ? Help · q Quit`, otherwise `0-4 View · a Add · ? Help · q Quit`. Paging, copy and (on Patterns) rescan are only in the `?` overlay, so the bar fits 80 columns. A notice or an input line replaces the bar while shown. A test checks that this table is exactly what `keymap::markdown()` renders.

### Help overlay

`?` opens a centred overlay titled "Keys · <view>" that lists the keys that work everywhere and the current view's keys. `?` or `esc` closes it; `q` and ctrl-c still quit; every other key is ignored while it is open. On small terminals it shrinks to the screen and cuts text with `…`. In the add flow and the filter input every key is text, `?`, `y`, `Y` and `r` included.

### Rescan

`r` on views 1-4 scans the shown connection again as a new run, the same way `enter` connects it (same scan options, rate limit and read-only rules; the connection's current profile and credential source). It works when the scan has finished or failed. While a connect or scan is running it only says "A scan is already running"; with no connection yet it says to connect first. On Connections `r` still looks for Docker containers again.

A rescan keeps the view, the Patterns filter, collapsed groups, sort and selected row, and the TTL, Diff and Hot keys selections (moved to the last row if the new results are shorter). The previous results stay on every view, marked `rescanning · previous results` at the start of the panel title, until the new run's first partial results arrive. From then on Patterns shows the partial results (the selection follows its row) and TTL, Diff and Hot keys wait for the finished scan. If the rescan fails before any results of its own, the error is shown and the previous results stay, marked `rescan failed · previous results`. If it fails after partial results it behaves like any failed scan. While a rescan runs, `s` says to wait for it to finish; it never saves partial results and does not save the previous results again. Results, partials and progress of the replaced run are ignored. The one exception is the replaced run's snapshot lookup, which still fills in the Diff of its own report while that report is on screen. A slow keychain read during a rescan shows the keychain hint in the status line.

### Copy

`y` copies the selected row's pattern: a group row copies its label, such as `user:{id}:*`; on Hot keys it copies the key name. `Y` copies the suggested command `redis-cli --scan --pattern "<glob>"` for that pattern (on Hot keys, the key's pattern), the same text as the `$` line in the detail panel. Key names are chosen by whoever writes keys, and no one quoting style is safe in every shell (POSIX shells, fish, PowerShell, cmd.exe), so the command is only offered when every character of the glob is an ASCII letter or digit or one of `_ : . / * ? { } [ ] @ # + = , ~ - |`, none of which is special inside double quotes in any of them. For any other pattern the detail panel's command line says there is no command (without the `$ ` prompt) and `Y` says why; `y` still copies the pattern. `y` never copies text with control characters (a pasted newline would run what follows it) and says so. Only on Patterns, TTL, Diff and Hot keys; never on Connections or in the add flow or filter input. With nothing selected it says so.

Copying uses the OSC 52 terminal sequence (no clipboard library), written to the terminal between frames, so it also works over SSH. Terminals without OSC 52 (macOS Terminal.app, for one) ignore it, and the terminal never confirms a copy, so the notice says "Copied <pattern|key|command> to the clipboard (needs a terminal with OSC 52)". Text whose sequence would be over 100,000 bytes is not sent ("too long to copy"). A write error is shown as an error notice. Only a pattern name, a key name or that command can be copied (the `CopyText` type has no other constructor), so credentials, connection strings and profiles never reach the clipboard.

## Scanning

1. Read `INFO server`, `INFO memory` and `DBSIZE`.
2. Walk the keyspace with `SCAN cursor COUNT n [MATCH glob]`. **Never `KEYS`.** Deduplicate the keys SCAN returns.
3. For each batch, pipeline `TYPE`, `PTTL`, `MEMORY USAGE key SAMPLES 5` (when allowed) and `OBJECT FREQ` (when allowed and the policy is LFU).
4. Stop at the sample limit (default 100,000) or when the cursor returns to 0.
5. Stay under a command budget: 5,000 commands per second by default, none for `local` profiles. `--max-rate N` overrides it (`0` means no limit).
6. Scale sampled counts to estimates with `DBSIZE / keys present`, where keys present counts every distinct key SCAN returned that still existed when it was looked up: the samples plus keys that exist but could not be read (for example `NOPERM` from a key ACL, or names that are not UTF-8). Keys that vanished between SCAN and the lookup (`TYPE` `none`, `PTTL` -2) are left out, so short-lived keys don't shrink the estimates. The scale is never below 1, and it is 1 when the SCAN cursor returned to 0 (the whole keyspace was seen), when `MATCH` is used, or when no key seen still existed.

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
| TLS | `rediss://` or `ssl=True`. Certificates are checked against the OS trust store (macOS Keychain, Windows certificate store, the system CA bundle on Linux; `SSL_CERT_FILE`/`SSL_CERT_DIR` replace it when set), loaded once per process. Certificates that fail to load are skipped. On macOS and Linux the OS store is used on its own; if it has no usable certificate, the bundled Mozilla roots are used instead, never in addition. The bundled roots are the certificates in `webpki-root-certs` whose trust anchor is in `webpki-roots` with no name constraint: a certificate cannot carry Mozilla's name constraints, so a root limited to some domains (today one, limited to `.tr`) is left out rather than trusted for every host name. On Windows the OS store and the bundled roots are trusted together (each certificate once), because Windows fills its root store lazily through Automatic Root Update and a public root it has not fetched yet would otherwise be refused (`UnknownIssuer`). The trade-off: on Windows a bundled root that an administrator removed from the store is still trusted. Certificate and host name checks are always on. | v0.1 |
| Microsoft Entra ID | redis-rs `EntraIdCredentialsProvider::new_developer_tools()`, the developer-tools credential chain (az login and similar), behind the `entra-id` cargo feature. Managed identity and workload identity are not covered yet. | v0.2 |
| AWS ElastiCache IAM | | later |
| Sentinel, SSH tunnel | | later |

### Credential rules

- Usernames and passwords live **only** in the OS keychain: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
- There is one entry per profile, with service `redis-lens` and account `profile:<name>`.
- The config file never holds a credential, and the `Profile` type has no field for one.
- Credentials never appear in logs, error text, `Debug` output or snapshots.
- Keychain reads, writes and deletes give up after 15 s. On macOS the error says macOS is waiting for permission to use the keychain entry and to click "Always Allow" in the dialog, then try again; elsewhere it says the OS keychain did not answer (it may be locked). The error names the entry's account, never its contents.
- A panic inside a keychain call prints nothing (it would land on the TUI) and is reported as "the keychain call failed", never with the panic's message.
- A password typed into a URL on the command line is accepted, with a warning about shell history. `--password-stdin` and `--ask-password` are the preferred ways in.

### Safety

- Profiles with env `prod` are read-only by default. redis-lens never sends a write command, and the later `expire` command is refused on production profiles.
- Scans can prefer replicas. It is opt-in per profile: `conn add --prefer-replica` sets `prefer_replica = true` in the config (absent when false). `conn::connect_for_scan` resolves the profile's credentials once (one keychain read, or one Entra ID token provider) and uses them for every connection it opens. It connects to the primary, reads `INFO replication` and, on `role:master`, takes the first `slaveN:…state=online` entry with a usable address: an IPv4 or IPv6 address other than `0.0.0.0` or `::`, or a host name, and a port other than 0. It connects to that candidate with the same credentials and TLS settings (the same trust roots, with the certificate checked against the announced host name) and verifies it (below); connecting and verifying share one 2 s timeout, and reading the keychain happens before it starts. On `role:slave` (or `role:replica`) the profile already points at a replica. The result is a `ScanTarget`: `Primary`, `Replica(endpoint)`, or `PrimaryFallback(reason)` when the replica could not be used or verified (the scan then runs on the primary). `scan` and `diff` print a `Target:` line for a replica or a fallback (`Target:  replica 10.0.1.21:6379`, `Target:  primary (replica unreachable: …)`); the TUI shows it in the header and the Connections detail panel, with the reason passed through the same masking as error messages. Endpoints with an IPv6 address are shown in brackets, `[::1]:6379`. A replica can lag, so counts and sizes may trail the primary slightly.
- **Replica verification.** The address in `INFO replication` is whatever the replica announced, and anything may be listening there. redis-lens only scans a candidate it has checked against this primary (the replication ID is not secret, so a server built to imitate a replica can pass the check; it is then scanned without credentials, and its data is filed under the profile): the candidate's `INFO replication` must say `role:slave` (or `role:replica`) and its `master_replid` must equal the primary's. The steps:
  1. Read the primary's `master_replid`. Without one, nothing can be verified: fall back.
  2. Resolve the candidate's host once (`tokio::net::lookup_host`). Every connection to it uses those addresses, so DNS can't change between the check and the connection; TLS still checks the certificate against the announced name.
  3. Ask the candidate for `INFO replication` on a connection that sends no credentials (no `AUTH`, `HELLO` or `CLIENT SETINFO`). If it answers and passes, the scan runs **on that same connection**, without credentials (it then reads as the replica's default user). It is never sent the profile's credentials: `master_replid` is not secret (anyone who can run `INFO`, or `PSYNC`, can learn it), so passing the check proves nothing about who holds the address.
  4. Only a candidate that answers `NOAUTH`, for a profile that has credentials, may get them, and only when it is not on this machine or the primary is on this machine too. A host counts as on this machine when its name is loopback or any address it resolves to is loopback or unspecified; a loopback address announced by a remote primary names the machine running redis-lens, not the replica.
  5. The authenticated connection is verified again, and it is the one scanned.

  Resolving and every connection and check share the 2 s replica timeout. Any failure drops the candidate's connections and gives `PrimaryFallback("replica at <endpoint> is not a replica of this primary")`, or the reason it could not be checked (a refused loopback address, a host that does not resolve, a connection error, a timeout).

  The trust boundary: with `--prefer-replica`, the address the primary announces for its replica is trusted the way Cluster and Sentinel clients trust addresses servers announce. Credentials go only to an announced host that demands auth and is on the primary's host or not loopback (after DNS resolution). Anyone who can make the primary announce a host they control that demands auth can collect the credentials. That is why `--prefer-replica` is opt-in.
- Profile names starting with `docker:`, in any case (`Docker:`, `DOCKER:`), are reserved for discovered containers: `conn add` refuses them and the TUI add flow's guessed name avoids the prefix.

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

Lists running containers whose image is redis, valkey, redis-stack or bitnami/redis and that publish port 6379. A password found in `--requirepass` or in `REDIS_PASSWORD`/`VALKEY_PASSWORD` is held in memory only, and the auth shows as `password`. Without one, discovery sends a single `PING` to the published port on 127.0.0.1 with no credentials (no `AUTH`, `HELLO` or `CLIENT SETINFO`; 1 s timeout) and shows `none` (PONG), `required` (`NOAUTH` or `WRONGPASS`: the server wants credentials discovery cannot see, such as an ACL file) or `unknown` (refused, timed out, or any other answer). Only `PING` is sent. Containers are probed at the same time, each with its own 1 s timeout, so discovery takes about 1 s at worst however many containers don't answer. Results are sorted by container name.

## Command line

```
redis-lens                         open the TUI
redis-lens conn add <name> --url <url> [--env prod|stage|local|other] [--password-stdin | --ask-password] [--entra] [--prefer-replica] [--force]
redis-lens conn list | remove <name> | test <name>
redis-lens scan <name> [--sample N] [--match GLOB] [--max-rate N] [--save] [--json]
redis-lens diff <name> [--against FILE] [--max-rate N] [--json]
redis-lens discover
```

`--config <path>` or `REDIS_LENS_CONFIG` overrides the config location. The default is the platform config directory, for example `~/Library/Application Support/redis-lens/config.toml` on macOS. The file is TOML with `[[profile]]` tables and is written atomically with mode 0600.

## Snapshots (v0.2)

`s` (or `redis-lens scan --save`) writes the current `ScanReport` as JSON to `<data dir>/redis-lens/snapshots/<profile dir>/<YYYYMMDDTHHMMSSZ>.json`. A snapshot holds patterns and example key names, never values or credentials. It also records whether the scan saw the whole keyspace (`complete`: the cursor returned to 0 with no sample limit reached and no `MATCH`) and the `MATCH` glob it used; snapshots written before these fields existed read as incomplete with no glob.

- **Profile directory:** `<name>-<hash>`, where `<name>` is the profile name lower-cased with anything other than `[a-z0-9._-]` replaced by `_` (spaces included), trailing dots removed, and `_` added after Windows device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`). `<hash>` is the first 12 hex digits of the 64-bit FNV-1a hash of the exact profile name, so names that only differ in case, punctuation or non-ASCII characters get their own directories.
- **Docker containers** are snapshotted under `docker:<container>`, so they never share history with a saved profile of the same name.
- **Reading:** a snapshot whose stored `profile` is not exactly the requested one is ignored, and so are files that can't be parsed. The baseline is the newest remaining snapshot taken strictly before the new scan with the same `MATCH` glob.
- **Writing** goes to a uniquely named temp file that is flushed to disk and then renamed into place.

`redis-lens diff <profile>` first checks, before touching the server, that a baseline exists for the profile with no `MATCH` glob (the scan uses none), then scans, compares with that baseline (or with `--against <file>`), then saves the new scan. The Diff view uses the same baseline.

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
