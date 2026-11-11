<div align="center">
  <img src="docs/mark.svg" alt="redis-lens mark" width="104">
  <h1>redis-lens</h1>
  <p><strong>A terminal UI that shows what is using memory in Redis and Valkey, and whether any of it is a mistake.</strong></p>
  <p>
    <a href="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml"><img src="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
    <a href="https://crates.io/crates/redis-lens"><img src="https://img.shields.io/crates/v/redis-lens" alt="crates.io"></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.89%2B-B7410E" alt="Rust 1.89+"></a>
    <a href="https://valkey.io/"><img src="https://img.shields.io/badge/Redis-7%2B%20%7C%20Valkey%208-DC382D" alt="Redis 7+ and Valkey 8"></a>
    <a href="#license"><img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg" alt="License MIT OR Apache-2.0"></a>
  </p>
  <p>
    <a href="#authentication"><img src="https://img.shields.io/badge/auth-ACL%20%C2%B7%20password%20%C2%B7%20TLS%20%C2%B7%20Entra%20ID-DC382D" alt="Auth: ACL, password, TLS and Entra ID"></a>
  </p>
</div>

<br>

You have a Redis or Valkey that is using more memory than you expected. `INFO memory` tells you how much. It does not
tell you which keys. redis-lens samples the keyspace, groups keys into patterns such as `user:{id}:cart`, and shows
what each pattern costs: keys, memory, average size and how many have no TTL.

It answers one question: what is using the memory, and is any of it a mistake? Carts that never expire, a cache
that only grows, an import job that never cleans up, one huge queue. Those show up as rows you can read.

It is built to be safe on production. It is read-only and never sends a write command. It walks the keyspace with
`SCAN` and never `KEYS`. It stays under a command budget. Credentials live in your OS keychain, not in a file.

<div align="center">
  <img src="docs/demo.gif" alt="redis-lens opening the demo Redis, browsing key patterns by memory, then the TTL, Diff and Hot keys views" width="900">
</div>

<br>

## Install

```sh
cargo install redis-lens
```

Or download a binary for Linux, macOS or Windows from [GitHub Releases](https://github.com/magna-nz/redis-lens/releases).
Building from source needs Rust 1.89 or newer.

## Try it in 30 seconds

You need Docker and a Rust toolchain.

```sh
make demo
```

This starts a Redis 7 on port 6390 (ACL users `lens` and `lens-limited`) and a Valkey 8 on port 6391 (no auth), and
seeds them with a keyspace that has planted problems: carts without TTLs, search results without TTLs, an orphaned
import job, a large queue and a few hot keys. It saves both as connections, puts the demo password in your keychain,
and opens the TUI. `make demo-down` stops it and deletes the data.

## Use

### Save a connection

```sh
# Prompts for the password without echoing it. The password goes to the keychain.
redis-lens conn add prod-cache --url rediss://lens-ro@prod-cache.internal:6380 --env prod --ask-password

# Or read it from standard input.
printf '%s' "$REDIS_PASSWORD" | redis-lens conn add staging --url redis://lens-ro@staging:6379 --env stage --password-stdin

redis-lens conn list
redis-lens conn test prod-cache      # connect and run the permission check
redis-lens conn remove prod-cache    # also deletes the keychain entry
```

`--url` takes `redis://[user[:password]@]host[:port][/db]`, `rediss://` for TLS, or a StackExchange.Redis string such
as `host:6380,password=...,ssl=True`. A password typed into the URL works, but it lands in your shell history, so
prefer `--ask-password` or `--password-stdin`. `--env` is `prod`, `stage`, `local` or `other`, and is guessed from
the host if you leave it out. `--force` replaces a profile with the same name.

`--config <path>` or `REDIS_LENS_CONFIG` points at a different config file.

### The TUI

```sh
redis-lens
```

Press `?` on any view for its keys. The main ones:

| Key | Where | Action |
|---|---|---|
| `0` to `4` | Every view | Switch view: Connections, Patterns, TTL, Diff, Hot keys |
| `?` | Every view | Show or hide the key list |
| `a` | Every view | Add a connection: opens the Connections view with a box to paste a connection string into. The password is masked in the preview. |
| `↑` `↓` or `k` `j`, `PgUp` `PgDn`, `Home` `End` or `g` `G` | Every list | Move, by a page, to the first or last row |
| `enter` | Connections | Connect to the selected profile |
| `r` | Connections | Look for Docker containers again |
| `r` | Patterns, TTL, Diff, Hot keys | Scan again; the previous results stay until new ones arrive |
| `→` `←` or `l` `h`, `enter` | Patterns | Expand and collapse a pattern group |
| `/`, `esc` | Patterns | Filter patterns, clear the filter |
| `o` | Patterns | Sort by memory, keys, average size, no-TTL % or name |
| `y`, `Y` | Patterns, TTL, Diff, Hot keys | Copy the pattern (Hot keys: the key), or a `redis-cli --scan` command for it, through your terminal (OSC 52) |
| `s` | Every view but Connections | Save a snapshot of the current scan |
| `q` | Every view | Quit |

In the add box, `enter` saves, `esc` cancels and `ctrl-u` clears the field. `ctrl-c` quits from anywhere.

Narrow terminals never wrap: the Patterns view hides the share bar, then Δ, then the average size, and the selected
row's text that doesn't fit scrolls across.

### Scan from the command line

```sh
redis-lens scan prod-cache
redis-lens scan prod-cache --sample 20000 --match 'user:*'
redis-lens scan prod-cache --json > report.json
redis-lens scan prod-cache --max-rate 1000 --save
```

| Flag | Meaning |
|---|---|
| `--sample N` | Stop after N keys. Default 100,000. |
| `--match GLOB` | Only scan keys matching the glob. |
| `--json` | Print the report as JSON instead of a table. |
| `--max-rate N` | At most N commands per second. `0` means no limit. |
| `--save` | Save the report as a snapshot. |
| `--keep N`, `--keep-days D` | With `--save`, keep only the newest N snapshots, or those from the last D days. |
| `--check RULE` | Fail with exit code 1 when a rule fails, e.g. `no-ttl-pct<20`. Repeatable. See below. |
| `--rules FILE`, `--strict` | Read rules from a TOML file; fail rules that couldn't be judged. |
| `--report md\|html\|csv`, `-o FILE`, `--examples` | Write a shareable report. Example key names only with `--examples`. |

### Gate a deploy with `--check`

```sh
redis-lens scan staging --check 'no-ttl-pct<20' --check 'verdict!=persistent-growing' --check 'total-bytes<2GiB'
```

Each rule is `<metric><op><value>[,min-keys=N]`. The metrics are `no-ttl-pct`, `growth` (against the newest earlier
snapshot), `pattern-bytes`, `verdict` and `total-bytes`; the ops are `< <= > >= = !=`. Per-pattern rules skip patterns
with fewer than 50 sampled keys, so small patterns don't flap. A rule that can't be judged (no `MEMORY USAGE`, no
baseline) is reported as SKIP, and fails only with `--strict`. Exit codes: 0 pass, 1 a rule failed, 2 the scan
itself broke. Rules can live in a file: `--rules lens-rules.toml` with a `[check]` table holding `rules = [...]` and an
optional `min_keys = 100`.

In GitHub Actions:

```yaml
jobs:
  gate:
    runs-on: ubuntu-latest
    steps:
      - name: Install redis-lens
        run: cargo install redis-lens --locked
      - name: Gate the deploy
        # 0 pass, 1 a rule failed, 2 the scan itself broke
        run: |
          redis-lens conn add staging --url "$STAGING_REDIS_URL" --env stage
          redis-lens scan staging --sample 50000 \
            --check 'no-ttl-pct<20' --check 'total-bytes<2GiB' \
            --report md -o redis-report.md
        env:
          STAGING_REDIS_URL: ${{ secrets.STAGING_REDIS_URL }}
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: redis-report
          path: redis-report.md
```

Credentials go to the OS keychain, which a headless runner may not have. On CI prefer a connection without a stored
password (a token command, or a network-restricted user), and check `conn add` works on your runner first. `growth`
rules need a baseline, so keep the snapshot directory between runs (`--save`, and cache it) or they are skipped.

### Reports

```sh
redis-lens scan prod-cache --report md -o report.md
redis-lens scan prod-cache --report html -o report.html
redis-lens scan prod-cache --report csv > patterns.csv
```

Markdown and HTML hold a summary, the top patterns, TTL risks and, when there is an earlier snapshot, what changed.
The HTML is one file with no external requests. Key names are escaped for each format (including spreadsheet formula
injection in CSV), and example key names are left out unless you pass `--examples`, because they can hold user ids
or emails.

### Compare with an earlier scan

```sh
redis-lens diff prod-cache
redis-lens diff prod-cache --against ~/old-snapshot.json --json
```

`diff` scans, compares with the newest earlier snapshot for that profile (or the file given to `--against`), and then
saves the new scan. It takes `--max-rate` and `--json` too. `diff` scans the whole keyspace, so it needs a baseline
taken without `--match`: run `redis-lens scan <profile> --save` (no `--match`) once first. Snapshots taken with
`--match` are only compared with scans that used the same glob.

### Compare two instances

```sh
redis-lens compare staging prod
redis-lens compare staging prod --right-file ~/prod-snapshot.json --json
```

Scans both (each with its own rate limit; production stays read-only) or reads a saved snapshot for a side, and
compares per pattern: share of total memory, key-count and average-size ratios, and the share of keys without a TTL
on each side. "No TTL on one side, set on the other" is flagged first, since it is the likely bug. Shares and ratios
come first because datasets rarely have the same size. Standalone servers only for now.

### Scheduled snapshots and trends

redis-lens has no daemon; let cron, systemd or CI run it. A nightly snapshot that keeps a month of history:

```sh
# m h dom mon dow  command
15 3 * * *  redis-lens scan prod-cache --save --keep 30 >/dev/null
```

`--keep-days 30` keeps by age instead; both together keep a snapshot only while both allow it. The newest snapshot is
never deleted. Then:

```sh
redis-lens trend prod-cache                            # memory per pattern over the last 7 snapshots, growth per day
redis-lens snapshots list prod-cache
redis-lens snapshots prune prod-cache --keep 10        # shows what would go
redis-lens snapshots prune prod-cache --keep 10 --yes  # deletes it
```

The Diff view shows the same trend as a sparkline column once there are two earlier snapshots.

### Find containers

```sh
redis-lens discover
```

Lists running Docker containers that look like Redis or Valkey.

## The five views

**Connections.** Saved profiles and the Redis or Valkey containers found in Docker. Each row shows the name,
environment badge (PROD, STAGE, LOCAL, OTHER), endpoint, auth method, status and memory. The detail panel shows the
server version, topology, where the credentials come from, TLS, and the permission check results.

**Patterns.** Key patterns sorted by estimated memory, with type, estimated keys, share of memory, average size,
the percentage with no TTL, and the change since the last snapshot. Child patterns group under their prefix. The
detail panel shows example keys with sizes and TTLs, p50, p99 and max size, a TTL histogram, a one-line verdict and a
suggested command.

**TTL.** Patterns ranked by TTL risk. *Mixed* means most keys expire but some don't, which is probably a bug.
*Persistent and growing* is a likely leak. *Persistent and stable* is treated as intentional. Each row has a
histogram over none, under 1 minute, 1 hour, 1 day, 7 days, 30 days and over 30 days.

**Diff.** The current scan against a saved snapshot: before, after, change and percentage change for each pattern.
It says whether growth came from more keys or from bigger values, and flags new patterns.

**Hot keys.** Keys ranked by LFU counter (`OBJECT FREQ`), with pattern, type and size. Keys that are both hot and
big are flagged. This needs an LFU eviction policy such as `allkeys-lfu`. Without one the view says so.

Pattern inference splits keys on `:`, `/` and `|`, and replaces UUIDs, ULIDs, numeric ids, long hex hashes and IPv4
addresses with placeholders. For example `user:88213:cart` becomes `user:{id}:cart`.

## Authentication

| Method | How |
|---|---|
| None | No `AUTH` is sent. |
| ACL user and password, or password only | Both are stored in the OS keychain. |
| TLS | `rediss://` or `ssl=True`, verified against the OS trust store (with the Mozilla roots as a fallback). |
| Microsoft Entra ID | Behind a cargo feature, see below. |
| Token command | A command that prints a short-lived token (AWS, Azure and Google Cloud IAM), see below. |
| Docker discovery | Containers are found through the Docker socket. |

**Entra ID.** For Azure Cache for Redis and Azure Managed Redis. Install with the feature enabled:

```sh
cargo install redis-lens --features entra-id
redis-lens conn add azure-cache --url rediss://my-cache.redis.azure.net:10000 --env prod --entra
```

It uses the developer-tools credential, so run `az login` first. Managed identity is not verified yet. Azure hosts
(`*.redis.cache.windows.net`, `*.redis.azure.net`) are recognised and the environment is guessed from the host name.

**Short-lived tokens (IAM).** Managed services sign in with short-lived tokens sent as the password. Give
redis-lens the command that prints one, after `--`. It runs that command at every connect and never stores the token.

```sh
# Azure Managed Redis / Azure Cache for Redis with Microsoft Entra ID. The username is your object ID.
redis-lens conn add azure-prod --url rediss://<object-id>@contoso.eastus.redis.azure.net:10000 -- \
  az account get-access-token --resource https://redis.azure.com --query accessToken -o tsv

# Google Cloud Memorystore with IAM auth. The access token is the password.
redis-lens conn add gcp-prod --url rediss://10.0.0.5:6378 --provider memorystore -- \
  gcloud auth print-access-token

# AWS ElastiCache or MemoryDB with IAM auth. The username is the IAM-enabled user ID; the command is any tool or
# script that prints the SigV4 auth token (valid 15 minutes; see AWS's "Authenticating with IAM" page).
redis-lens conn add aws-prod --url rediss://<iam-user-id>@master.my-cluster.abc123.use1.cache.amazonaws.com:6379 -- \
  /usr/local/bin/elasticache-iam-token my-cluster <iam-user-id> us-east-1
```

`conn add` shows the command and asks before saving it (`--yes` skips the question in scripts). The command is run
directly, not through a shell, with a 10 s limit. Its error output is never shown, because some tools print tokens
there; if it fails, run it yourself to see why. The username goes to the keychain, the command to the config file.

**What works where.** redis-lens recognises ElastiCache, ElastiCache Serverless, MemoryDB and Azure from the host
name (`--provider` sets it by hand, and is the only way for Memorystore) and says before a scan what will be missing.
The permission check (`redis-lens conn test`) is what decides; this table is what to expect.

| Service | Sizes (`MEMORY USAGE`) | Hot keys (`OBJECT FREQ`) | Dataset figures (`MEMORY STATS`) | `INFO keysizes` | Default rate |
|---|---|---|---|---|---|
| Redis 8+ (self-hosted) | yes | with an LFU policy | yes | yes | 5,000/s |
| Redis 6.2–7.x, Valkey 8 | yes | with an LFU policy | yes | no | 5,000/s |
| ElastiCache Serverless | no (restricted) | no (restricted) | no (restricted) | no | 1,000/s |
| ElastiCache (node-based) | yes | with an LFU policy in the parameter group | yes* | engine-dependent | 5,000/s |
| MemoryDB | yes* | with an LFU policy* | yes* | engine-dependent | 5,000/s |
| Azure Cache for Redis | yes* | with an LFU policy* | yes* | no | 5,000/s |
| Azure Managed Redis / Enterprise | yes* | * | no (unsupported) | * | 5,000/s |
| Memorystore | yes* | with an LFU policy* | yes* | engine-dependent | 5,000/s |

\* Not checked against the service; the permission check shows what actually works.

**Docker discovery.** Lists running containers whose image is redis, valkey, redis-stack or bitnami/redis and that
publish port 6379. A password found in `--requirepass` or in `REDIS_PASSWORD` or `VALKEY_PASSWORD` is held in memory
only.

**The keychain.** Usernames and passwords are stored in the macOS Keychain, Windows Credential Manager or the Secret
Service on Linux. There is one entry per profile: service `redis-lens`, account `profile:<name>`. The config file
holds the profile name, host, port, TLS setting and environment, and nothing secret. Credentials never appear in
logs, error text or snapshots.

On macOS, a keychain entry is tied to the binary that wrote it. After an upgrade or a rebuild, macOS asks once
whether redis-lens may use it. Choose **Always Allow**. Until you answer, connecting waits; after two seconds the
TUI tells you it is waiting on the keychain.

## Least-privilege ACL user

redis-lens needs only a handful of read commands. A minimal user:

```
ACL SETUSER lens-ro on >… ~* resetchannels -@all +ping +hello +client|setinfo +client|setname +select +info +dbsize +scan +type +ttl +pttl +memory|usage +object|freq +memory|stats
```

Before scanning, redis-lens tries each command once and turns off what it can't use. It never aborts a scan over a
missing permission.

| Missing | What you lose |
|---|---|
| `MEMORY USAGE` | Sizes. You still get key counts and TTLs. |
| `OBJECT FREQ`, or a non-LFU eviction policy | The Hot keys view. |
| `MEMORY STATS` | The `dataset.bytes` figures in the server context strip. |
| A key ACL on some keys (`NOPERM`) | Those keys' sizes. The scan carries on. |

`conn test <name>` shows the result of the check.

## Safety

- Read-only. redis-lens never sends a write command. Profiles with env `prod` are read-only by default.
- The keyspace is walked with `SCAN cursor COUNT n`. Never `KEYS`. Per-key commands (`TYPE`, `PTTL`, `MEMORY USAGE
  ... SAMPLES 5`, `OBJECT FREQ`) are pipelined in batches.
- A command budget limits load. The default is no limit for `local` profiles, 1,000 commands per second on
  ElastiCache Serverless and 5,000 for everything else. Change it with `--max-rate`.
- With `conn add --prefer-replica`, scans run against an online replica found through `INFO replication`, so the
  primary serves no scan traffic. The replica is verified first: its `INFO replication` must say it is a replica and
  carry the primary's replication ID (`master_replid`). An ordinary unrelated server on the announced address fails
  that check and is not scanned. The replication ID is not secret, though, so a server built to imitate a replica can
  pass it and be scanned (without credentials). A replica that passes without credentials is scanned without them. Credentials go only to an
  announced host that demands auth and is on the primary's host or not loopback (after DNS resolution). If you reach
  the primary through an SSH tunnel on 127.0.0.1, any local service on the announced port counts as "on the primary's
  host". This trusts
  the address the primary announces, the way Cluster and Sentinel clients do: anyone who can make the primary
  announce a host they control that demands auth can collect the credentials, which is why `--prefer-replica` is
  opt-in. If no replica is reachable or verified, redis-lens scans the primary and says why. A replica can lag behind
  its primary, so counts and sizes may trail the primary slightly.
- Scans stop at 100,000 keys by default. Counts are then scaled up to estimates using `DBSIZE`. If the whole
  keyspace was seen, or `--match` is used, no scaling is applied.

## Snapshots and diffs

`s` in the TUI, or `redis-lens scan --save`, writes the scan as JSON to
`<data dir>/redis-lens/snapshots/<name>-<hash>/<YYYYMMDDTHHMMSSZ>.json`. `<name>` is the profile name lower-cased,
with anything other than letters, digits, `.`, `_` and `-` replaced by `_`. `<hash>` is 12 hex digits of a hash of
the exact profile name, so profiles whose names differ only in case or punctuation keep separate histories. On macOS
the data dir is `~/Library/Application Support`. A snapshot holds patterns and example key names. It never holds values or
credentials. Example key names can still be sensitive, so treat snapshot files like the data they describe.

`redis-lens diff <profile>` and the Diff view compare a scan with the newest earlier snapshot taken with the same
`--match` glob. For `diff` that means a snapshot taken without `--match`.

A cron job that runs `redis-lens scan <profile> --save --keep 30` once a day gives you something to diff against
and a month of trend (see [Scheduled snapshots and trends](#scheduled-snapshots-and-trends)).

## Development

```sh
make test       # unit tests, no Docker
make it         # integration tests against Redis and Valkey in Docker
make lint       # rustfmt check and clippy with -D warnings
make demo-up    # start and seed the demo servers without opening the TUI
make demo-down  # stop the demo and delete its data
make gif        # record docs/demo.gif (needs the demo up, python3 and agg)
```

`demo/record.py` drives redis-lens in a pseudo-terminal from a JSON list of keystrokes and writes an asciinema cast.
`make gif` uses its own temporary config file, saves the `demo-cache` and `demo-valkey` connections that
`demo/gif-steps.json` expects, records, renders with `agg`, then removes those connections and their keychain
entries. See `make help` for the full list.

`spec.md` describes the intended behaviour and `status.md` tracks progress.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
