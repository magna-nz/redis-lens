# redis-lens vs Redis Insight: gap analysis

Research date: 2026-11-12. These are proposals; nothing here is built, and `spec.md` is unchanged.
Redis Insight version compared: v3.8.0 (July 2026). The comparison covers features only; no Redis Insight source code was read or copied. Redis Insight is source-available under a restrictive licence.

**Grounding:**
- Redis Insight: its docs on redis.io and the release notes for v2.0 to v3.8.0. Sources are listed at the end.
- redis-lens: `spec.md`, `status.md`, `docs/feature-ideas.md`, plus parts of `src/tui/`, `src/scan/`, `src/cli/` and `src/model.rs`.

**Spot-checked by hand:**
- Production mode with type-to-confirm (3.6), Arrays (3.8), and the 10,000-key cap in Database Analysis.
- The list of commands redis-lens sends. A grep of `cmd("…")` in `src` finds only `DBSIZE`, `INFO`, `MEMORY`, `OBJECT`, `PING`, `PTTL`, `SCAN`, `SELECT` and `TYPE`.

## The decision this needs first

The spec's non-goals rule out most of what makes Redis Insight a one-stop shop. `spec.md:11`:

> **Non-goals:** a general Redis client or key editor, a monitoring daemon, or anything that writes data.

Today redis-lens never reads a value and never writes. `Profile::is_read_only` (`src/model.rs:461`) only shows a badge and a warning, because there is no write path to block yet. The features below fall into three tiers, and each tier past the first needs a change to the stance:

| Tier | What it needs | Decision |
|---|---|---|
| **A: read-only metadata** | New read commands (`SLOWLOG GET`, `CLIENT LIST`, `XINFO`, `FT.INFO` …). No values, no writes. | None. This fits today's spec. |
| **B: reading values** | `HSCAN`/`LRANGE`/`GETRANGE`… to show key contents. | Allow values on screen? Values may hold PII. They would need the same rules as credentials for snapshots, reports and logs, but they could be shown on screen. |
| **C: writes** | Key edits, TTL and delete, a command line, bulk delete and expire, publish, stream ack. | Drop "anything that writes" for non-production profiles? Production profiles stay write-free; that is stronger than Redis Insight's type-to-confirm. |

My recommendation: do A now. Do B behind a per-profile `show_values` setting that is off for `prod`. Do C only for non-production profiles, every write behind type-to-confirm. That keeps the "safe to point at production" promise (`spec.md:7`) and still reaches most of Redis Insight's surface.

**Decided by the owner, 2026-11-12:** all three phases. Values are shown on screen for every profile and never saved. Writes are allowed on any profile with writes turned on, `prod` included, behind the three checks in spec.md "Write permissions": the server ACL, the profile's `read_only` setting (on by default for `prod`; turning it off for `prod` needs the profile name typed), and a confirmation for each action as in Redis Insight 3.6. The spec is updated: see spec.md "Browsing, server views and writes (planned)".

## Gap matrix

Fit: how well it works in a TUI (✓ good, ~ workable, ✗ poor). Effort: S = days, M = 1-2 weeks, L = more. "Have" means redis-lens has it on `main` today.

### Connections

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Standalone, TLS (CA, client cert, SNI), ACL users | **Have** | – | ✓ | – | We also have the OS trust store and the keychain. Redis Insight optionally encrypts its local store. |
| Local auto-discovery | **Have, better** | – | ✓ | – | Docker discovery each run, not just on first launch. |
| Azure discovery with Entra ID | Part | – | ✓ | M | Entra ID is developer-tools only (feature idea 10). Redis Insight discovers across subscriptions. |
| Redis Cloud and Redis Software discovery | Missing | A | ~ | M | Cloud API sign-in. Low priority for our users. |
| AWS ElastiCache discovery | Missing (Redis Insight lacks it too) | A | ✓ | M | A place we could lead, with feature idea 9 (IAM). |
| Cluster, including force-standalone | Missing | A | ✓ | L | Already v0.3 (`spec.md:327`). |
| Sentinel | Missing | A | ✓ | M | Already "later" (`spec.md:328`). |
| SSH tunnel (password or key) | Missing | A | ✓ | M | Already "later". The key passphrase goes in the keychain (rule 1). |
| Switch DB index while connected | Missing in the TUI (profile `db` only) | A | ✓ | S | Indexes from `INFO keyspace`, then `SELECT`. |
| Import and export a connection list | Missing | A | ✓ | S | Profiles only, never credentials. |
| Preset connections from env or a file | Part (`--config`, `REDIS_LENS_CONFIG`) | A | ✓ | S | |
| Dev/Production label | **Have, stricter** | – | ✓ | – | Our `env` plus `read_only`. Redis Insight only asks for confirmation. |
| Tags and aliases on connections | Part (name, env) | A | ✓ | S | |

### Overview and monitoring

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Live overview: CPU, memory, ops/s, clients, network, keys | Part (one-off context strip) | A | ✓ | M | A new **Server** view polling `INFO` every few seconds, with sparklines. It counts against the command budget, and polling can be stopped. Add hit ratio, evictions and expired keys, which Redis Insight leaves out. |
| Per-shard overview on cluster | Missing | A | ✓ | M | After v0.3. |
| Slow Log viewer | Missing | A | ✓ | S | `SLOWLOG GET`. Needs `@admin`, so degrade on `NOPERM`. The arguments in an entry can hold values, so redact them unless values are allowed (tier B). Redis Insight also edits the threshold with `CONFIG SET`; that is tier C, and ElastiCache blocks it. |
| `CLIENT LIST` view | Missing | A | ✓ | S | Idle, age, command, memory per client, grouped by `lib-name` and address. Good for finding leaks of connections. |
| Profiler (`MONITOR`) | Missing | A | ~ | M | Read-only but heavy, and the spec already lists it as a hot-keys fallback (`spec.md:59`). Time-limited, off for prod, or prefer server-side `HOTKEYS` (feature idea 12). |

### Browser (key level)

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Tree view by namespace | **Have, better** | – | ✓ | – | Our pattern inference finds `{uuid}`, `{id}` and the rest, not just a delimiter. |
| Filter by name and type | Part (`/` filter on patterns) | A | ✓ | S | Add a type filter. |
| Key list for a pattern, with size and TTL | Missing (examples only) | A | ✓ | M | `enter` on a pattern runs `SCAN MATCH`, budgeted and paged, showing type, TTL, size, encoding and length. Metadata only, so it stays tier A. Needs a scan `Effect`/`Msg` for TUI-driven work and the limiter applied to it. |
| Key details: type, TTL, encoding, length | Missing | A | ✓ | S | `OBJECT ENCODING` plus `STRLEN`/`HLEN`/`LLEN`/`SCARD`/`ZCARD`/`XLEN`, along with the encoding thresholds. |
| View values for each type | Missing | **B** | ✓ | M | `GETRANGE`, `HSCAN`, `SSCAN`, `ZSCAN`, `LRANGE`, `XRANGE`, `JSON.GET`, always paged and capped in size. |
| Formatters: UTF-8, hex, JSON, MessagePack, Protobuf, Pickle, PHP, Java | Missing | B | ✓ | M | Start with UTF-8, hex, JSON and MessagePack. Decompression (gzip, LZ4, zstd, Snappy) is cheap to add. |
| Copy a value | Missing | B | ✓ | S | OSC 52 is already in place. The `CopyText` limits would need to grow. |
| Add, edit and delete keys of each type, TTL edit, rename | Missing | **C** | ~ | L | Editing a field at a time works in a TUI. Large JSON editing does not; hand off to `$EDITOR` instead. |
| Hash field TTL | Missing | A (read), C (set) | ✓ | M | Reading it is feature idea 8 (`HEXPIRE`, `HTTL`). |
| JSON, Vector Sets, Arrays (Redis 8.8) | Missing | B, C | ~ | M each | View them first. Vector similarity search is niche in a TUI. |
| Sample data loader | Missing | C | ✓ | S | Already done better by our `make demo-up`. |

### Analysis

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Database Analysis: memory by type and namespace, top keys | **Have, better** | – | ✓ | – | Redis Insight stops at 10,000 keys. We default to 100,000, with budget, scaling and verdicts. |
| "Memory freed by TTL over time" | Part (TTL histogram) | A | ✓ | S | Project expiring bytes from the PTTL already sampled. No new commands. |
| Biggest keys list | Part (max per pattern) | A | ✓ | S | A top-N list across all patterns, from data already sampled. |
| Historical reports | **Have, better** | – | ✓ | – | Snapshots, diff, trend and compare. |
| Recommendations | Part (TTL verdicts, `--check`) | A | ✓ | M | Add rules for encoding thresholds, big keys, oversized hashes, `maxmemory-policy` with no TTLs, and LFU being off. Use `INFO` and `OBJECT ENCODING`, not `CONFIG GET`, which is blocked on ElastiCache. |
| Hot keys | **Have (Redis Insight has none)** | – | ✓ | – | |

### Workbench and CLI

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Command line | Missing | C | ✓ | M | A `:` prompt. On read-only profiles, refuse any command whose `COMMAND INFO` flags aren't `readonly`, which makes even this safe on prod. Type-to-confirm for `@dangerous`. History lives in memory only, since commands can hold values or `AUTH`. |
| Command helper and autocomplete | Missing | A | ✓ | S | From `COMMAND DOCS`, so it works offline from the server. |
| Result views: text, table, JSON, raw | Missing | A | ✓ | S | Comes with the command line. |
| Pipeline mode, `N cmd` repeat | Missing | C | ~ | S | |
| Graphical plugins: Search table, TimeSeries chart, Graph, Geo map | Missing | A | ~ / ✗ | M | TimeSeries as a sparkline and Search as a table work. Graph is retired upstream; Geo maps don't fit a TUI. |
| Bulk upload of a commands file | Missing | C | ✓ | S | `redis-cli < file` already does this. Low value. |

### Bulk actions

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Bulk delete by filter, with a count preview and a file of deleted keys | Missing | C | ✓ | M | Rejected before (`docs/feature-ideas.md`). If tier C goes ahead: `--dry-run` by default, `UNLINK` in budgeted batches, type-to-confirm, never on prod. |
| Bulk expire | Missing (planned exception) | C | ✓ | M | `spec.md:11` and `:328` already allow this one. It fits naturally with "set a TTL on this leaking pattern". |

### Pub/Sub and Streams

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Subscribe and pattern-subscribe, live message list | Missing | B | ✓ | M | Payloads are values (tier B). `PUBSUB CHANNELS` and `NUMSUB` without payloads are tier A. |
| Publish | Missing | C | ✓ | S | |
| Stream consumer groups: lag, pending, consumers | Missing | A | ✓ | M | `XINFO GROUPS`, `XINFO CONSUMERS`, `XPENDING` summary. Fits "is any of it a mistake": stuck consumers, a growing PEL, and old streams that are never trimmed (a stream does not store its `MAXLEN`; the age of the oldest entry stands in). A strong addition. |
| Acknowledge and claim | Missing | C | ~ | S | |

### Search, AI and the rest

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Search index list and details | Missing | A | ✓ | S | `FT._LIST` and `FT.INFO`: docs, size, indexing failures, memory. Hidden when the module is missing. |
| Query editor, Explain and Profile | Missing | B | ~ | M | `FT.SEARCH` returns values. |
| Create and drop an index | Missing | C | ~ | M | |
| Redis Copilot (AI) | Missing | – | ✗ | – | **Skip.** It sends schemas and keys to Redis and OpenAI and needs a Redis Cloud sign-in. That conflicts with rule 1 and "safe on prod". |
| RDI pipeline management | Missing | – | ✗ | – | **Skip.** A separate product's management API. |
| Tutorials, guides, What's New | Missing | – | ✗ | – | **Skip.** The `?` help overlay covers us. |
| Triggers and Functions | Missing | – | ✗ | – | **Skip.** Deprecated upstream. |
| Themes, keyboard shortcuts | Part | A | ✓ | S | A light theme and a `NO_COLOR` setting. |
| Telemetry | None | – | – | – | Keep it that way. |
| REST API, Docker image, VS Code extension | Missing | – | ✗ | – | Out of scope for a TUI. |

## Proposed roadmap

Effort is a rough total for each phase.

**Phase 1: tier A, no change to the stance (about 4-6 weeks)**

| # | Feature | Effort | Why first |
|---|---|---|---|
| 1 | **Server** view: live `INFO` with sparklines, plus `CLIENT LIST` | M | The biggest visible gap from Redis Insight, and all read-only. |
| 2 | **Slow Log** view, arguments redacted | S | Cheap, often asked for, degrades cleanly on `NOPERM`. |
| 3 | **Key list** for a pattern, with metadata (type, TTL, size, encoding, length) | M | Turns Patterns into a browser without reading values. Builds the TUI-driven, budgeted command path every later view needs. |
| 4 | **Streams** view: groups, lag, pending, untrimmed streams | M | Finds real mistakes, which is the tool's purpose. |
| 5 | Analysis extras: biggest keys, memory freed by TTL, more recommendation rules | S-M | No new commands for the first two. |
| 6 | DB index switch, Search index inspector, command helper | S each | |

**Phase 2: tier B, values on screen (about 3-4 weeks; needs decision B)**
- Key value viewer for each type, paged and capped. Formatters: UTF-8, hex, JSON, MessagePack, decompression.
- Pub/Sub live tail; Search query (read-only).
- New rule: values are never written to snapshots, reports, logs or the clipboard unless the user asks (`y` on a value). Off by default on `prod`.

**Phase 3: tier C, writes on non-production profiles (about 4-6 weeks; needs decision C)**
- A `:` command line. Read-only profiles allow only `readonly` commands; elsewhere `@dangerous` asks for type-to-confirm.
- Edit a key: TTL, rename, delete, field edits, with `$EDITOR` for big values.
- Bulk expire and delete by pattern: dry run first, budgeted `UNLINK`, never on prod.
- Publish, stream ack and claim.

**Already planned, which Redis Insight also has:** cluster (v0.3), Sentinel, SSH tunnels.

**Where we already lead, and should keep leading:** pattern inference, sampling budget, TTL verdicts, snapshots, diff, trend and compare, `--check` for CI, hot keys, a strict prod mode with no writes at all, keychain-only credentials.

## Each new view and the existing rules

- **Rule 2 (never hurt the server):** today only scans go through the limiter (`src/scan/mod.rs:153`). Every view from Phase 1 that sends commands from the TUI must go through a shared limiter too. That is the first piece of work in Phase 1, item 3.
- **Rule 3 (degrade):** `SLOWLOG`, `CLIENT LIST`, `MONITOR` and `CONFIG` need `@admin` or `@dangerous`. They join the permission probe (`src/conn/mod.rs:579`), and each view says why it's off.
- **Rule 1 (credentials):** a Slow Log entry, a `MONITOR` line or a command line's history can contain `AUTH` arguments. Redact them before display, and never keep command history on disk.
- **Spec:** each phase updates `spec.md`, starting with the non-goals line, then the views, keys and permission check.

## Follow-ups found on the way

- `spec.md:59` still says the `MONITOR` hot-keys sample is "planned for v0.2". It was never built (`status.md:165`), and `spec.md:328` lists it under Later.

## Sources

Redis Insight (all opened or searched 2026-11-12):
- Overview: https://redis.io/docs/latest/develop/tools/insight/
- Release notes index: https://redis.io/docs/latest/develop/tools/insight/release-notes/
- v3.8.0 (Arrays, Markdown format, IPv4/IPv6): https://redis.io/docs/latest/develop/tools/insight/release-notes/v.3.8.0/
- v3.6.0 (Dev/Production mode, type-to-confirm, Vector Sets): https://redis.io/docs/latest/develop/tools/insight/release-notes/v.3.6.0/
- v2.12.0 (Database Analysis): https://redis.io/docs/latest/develop/tools/insight/release-notes/v2.12.0/
- Search workspace: https://redis.io/docs/latest/develop/tools/insight/search-workspace/
- RDI connector: https://redis.io/docs/latest/develop/tools/insight/rdi-connector/
- Copilot FAQ: https://redis.io/docs/latest/develop/tools/insight/copilot-faq/
- Configuration: https://redis.io/docs/latest/operate/redisinsight/configuration/
- GitHub releases: https://github.com/redis/RedisInsight/releases

Not confirmed from a primary source:
- the exact commands behind Browser listing, bulk delete, Pub/Sub and Slow Log config;
- key rename and TTL editing;
- whether TimeSeries and probabilistic keys can be viewed in Browser;
- the list of recommendation types;
- the keyboard shortcut list.
