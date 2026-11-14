# redis-lens vs Redis Insight: gap analysis

Research date: 2026-11-12. Updated 2026-11-15: phases 1 to 5 are built, and the "redis-lens" column below says what is true now. The Tier, Fit and Effort columns and the notes on Redis Insight are as researched. `spec.md` describes what is built.
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

Fit: how well it works in a TUI (✓ good, ~ workable, ✗ poor). Effort: S = days, M = 1-2 weeks, L = more. "Have" means redis-lens has it, built by phase 5 (on `claude/insight-phase5` until it is merged). "Skipped" is a deliberate choice, "Missing" is not built.

### Connections

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Standalone, TLS (CA, client cert, SNI), ACL users | **Have** | – | ✓ | – | We also have the OS trust store and the keychain. Redis Insight optionally encrypts its local store. |
| Local auto-discovery | **Have, better** | – | ✓ | – | Docker discovery each run, not just on first launch. |
| Azure discovery with Entra ID | Part | – | ✓ | M | Entra ID sign-in is built for developer tools (feature idea 10). Discovery across subscriptions is not built: it needs cloud accounts and can't be tested in CI. |
| Redis Cloud and Redis Software discovery | Missing | A | ~ | M | Not built in phase 5: needs cloud accounts and can't be tested in CI. Low priority for our users. |
| AWS ElastiCache discovery | Missing (Redis Insight lacks it too) | A | ✓ | M | Not built in phase 5, for the same reason. A token command covers ElastiCache IAM today (feature idea 9). |
| Cluster, including force-standalone | **Have**, force-standalone in progress | A | ✓ | L | Cluster since v0.3, live views since Phase 4. Force-standalone (one node as a standalone server) is being built for Phase 5. |
| Sentinel | **Have** | A | ✓ | M | Phase 4: Sentinel credentials kept apart in the keychain; `--prefer-replica` uses `SENTINEL REPLICAS`. |
| SSH tunnel (password or key) | Part | A | ✓ | M | Key login through a bastion since v0.3, the key passphrase in the keychain (rule 1). Password login is not built: a large change on a security-sensitive path. |
| Switch DB index while connected | **Have** | A | ✓ | S | Phase 4: `b` lists the indexes from `INFO keyspace`. |
| Import and export a connection list | **Have** | A | ✓ | S | Phase 4: `conn export` and `conn import`, profiles only, never credentials. |
| Preset connections from env or a file | Part (`--config`, `REDIS_LENS_CONFIG`) | A | ✓ | S | Presets from environment variables are not built: they conflict with rule 1, so that is the owner's call. |
| Dev/Production label | **Have, stricter** | – | ✓ | – | Our `env` plus `read_only`. Redis Insight only asks for confirmation. |
| Tags and aliases on connections | Part (name, env, tags) | A | ✓ | S | Tags are built (Phase 4). Aliases are not: low value. |

### Overview and monitoring

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Live overview: CPU, memory, ops/s, clients, network, keys | **Have, better** | A | ✓ | M | Overview's "Now": six figures with sparklines (twelve with `v`), through the budget, pausable. Adds hit rate, evictions and expired keys, which Redis Insight leaves out. |
| Per-shard overview on cluster | **Have** | A | ✓ | M | Phase 4: one row per shard on Overview, from a per-shard poll. |
| Slow Log viewer | **Have** | A | ✓ | S | Arguments shown masked; `NOPERM` degrades; threshold edit and reset through the write gate; `y` copies the masked command line (Phase 5). |
| `CLIENT LIST` view | **Have** | A | ✓ | S | Grouped by library and by address; `K` kills a connection through the write gate. |
| Profiler (`MONITOR`) | **Have** | A | ~ | M | Server › Monitor, time-limited, asks first, credentials masked as lines arrive; `y` copies a masked line (Phase 5). Server-side `HOTKEYS` is not built: not confirmed on any image. |

### Browser (key level)

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Tree view by namespace | **Have, better** | – | ✓ | – | Our pattern inference finds `{uuid}`, `{id}` and the rest, not just a delimiter. |
| Filter by name and type | **Have** | A | ✓ | S | `/` filters by name and `t` by type (Phase 4). |
| Key list for a pattern, with size and TTL | **Have** | A | ✓ | M | `→` on a pattern runs a budgeted, paged `SCAN MATCH`, with type, TTL, size, encoding and length. |
| Key details: type, TTL, encoding, length | **Have** | A | ✓ | S | In a key's value: facts line with length, size, expiry, encoding and LFU counter. |
| View values for each type | **Have** | **B** | ✓ | M | Strings, hashes, lists, sets, sorted sets, streams, JSON and vector sets, paged and capped; arrays and the module types in Phase 5. |
| Formatters: UTF-8, hex, JSON, MessagePack, Protobuf, Pickle, PHP, Java | **Have** | B | ✓ | M | All eight, cycled with `f`; Protobuf and Pickle protocols 0 and 1 are picked by hand, never auto-detected. Decompression: gzip, zlib, LZ4, zstd and Snappy (Phase 5). |
| Copy a value | **Have** | B | ✓ | S | `y` on a value, in the format shown, through OSC 52; text with control characters is not copied. |
| Add, edit and delete keys of each type, TTL edit, rename | **Have** | **C** | ~ | L | Field-at-a-time edits, with `$EDITOR` for big values, behind the write gate. Creating JSON keys is not built. |
| Hash field TTL | **Have** (read and set) | A (read), C (set) | ✓ | M | `HTTL` on Redis 7.4 and Valkey 9.0 and later; `HEXPIRE` through the write gate. |
| JSON, Vector Sets, Arrays (Redis 8.8) | **Have** (view) | B, C | ~ | M each | JSON, vector sets (100 random elements) and arrays (`ARSCAN`, Phase 5) are viewed. Vector similarity search is not built: niche in a TUI. |
| Sample data loader | Skipped | C | ✓ | S | Done better by our `make demo-up`. |

### Analysis

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Database Analysis: memory by type and namespace, top keys | **Have, better** | – | ✓ | – | Redis Insight stops at 10,000 keys. We offer Quick (10,000) and Deep (100,000), with budget, scaling and verdicts. |
| "Memory freed by TTL over time" | **Have** | A | ✓ | S | Analyze › Expiry's forecast, also on Overview. |
| Biggest keys list | **Have** | A | ✓ | S | Analyze › Big keys: the 50 biggest sampled keys across all patterns. |
| Historical reports | **Have, better** | – | ✓ | – | Snapshots, diff, trend and compare. |
| Recommendations | **Have** | A | ✓ | M | "Things to look at": big keys, encodings, no-TTL patterns, memory that never expires, and from Phase 5 the eviction policy (volatile with few TTLs, full `noeviction`, LFU off). Uses `INFO` and `OBJECT ENCODING`, not `CONFIG GET`. |
| Hot keys | **Have (Redis Insight has none)** | – | ✓ | – | Under an LFU policy, or from a Monitor run. |

### Workbench and CLI

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Command line | **Have** | C | ✓ | M | `:` panel. Readonly commands run, writes go through the write gate, `@dangerous` asks, scripts and module commands not known to read are not sent. History in memory only, credentials masked. |
| Command helper and autocomplete | **Have** | A | ✓ | S | From `COMMAND DOCS` and `COMMAND LIST`, with `tab`. |
| Result views: text, table, JSON, raw | **Have** | A | ✓ | S | `ctrl-t` cycles them (Phase 5). |
| Pipeline mode, `N cmd` repeat | **Have** | C | ~ | S | `N cmd` (at most 100) and `ctrl-p` pipelines (Phase 5). A write that asks is not repeated. |
| Graphical plugins: Search table, TimeSeries chart, Graph, Geo map | Part | A | ~ / ✗ | M | Search results are listed in Browse › Search, and a time series key shows a sparkline with its samples (Phase 5). Graph is retired upstream and Geo maps don't fit a TUI: skipped. |
| Bulk upload of a commands file | Missing | C | ✓ | S | Not built: `redis-cli < file` already does this. Low value. |

### Bulk actions

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Bulk delete by filter, with a count preview and a file of deleted keys | **Have** | C | ✓ | M | Dry run first, `UNLINK` in pipelines of 500 within the budget, the deleted key names appended to a file, a confirmation each time (the pattern typed on `prod`). |
| Bulk expire | **Have** | C | ✓ | M | `x` on a pattern and `redis-lens expire` (a dry run, then `--apply`). A read-only profile refuses `expire`, dry run included. |

### Pub/Sub and Streams

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Subscribe and pattern-subscribe, live message list | **Have** | B | ✓ | M | The Pub/Sub live tail, with sharded Pub/Sub on a cluster. |
| Publish | **Have** | C | ✓ | S | `P`, through the write gate. |
| Stream consumer groups: lag, pending, consumers | **Have** | A | ✓ | M | Browse › Streams: groups, lag, pending, consumers, and untrimmed streams. |
| Acknowledge and claim | **Have** | C | ~ | S | `A`, `c` and `X` on Streams, through the write gate. |

### Search, AI and the rest

| Redis Insight feature | redis-lens | Tier | Fit | Effort | Notes |
|---|---|---|---|---|---|
| Search index list and details | **Have** | A | ✓ | S | Hidden when the module is missing. |
| Query editor, Explain and Profile | **Have** | B | ~ | M | Browse › Search. |
| Create and drop an index | **Have** | C | ~ | M | `C` and `D` on Search, through the write gate. |
| Redis Copilot (AI) | Missing | – | ✗ | – | **Skip.** It sends schemas and keys to Redis and OpenAI and needs a Redis Cloud sign-in. That conflicts with rule 1 and "safe on prod". |
| RDI pipeline management | Missing | – | ✗ | – | **Skip.** A separate product's management API. |
| Tutorials, guides, What's New | Missing | – | ✗ | – | **Skip.** The `?` help overlay covers us. |
| Triggers and Functions | Missing | – | ✗ | – | **Skip.** Deprecated upstream. |
| Themes, keyboard shortcuts | **Have** | A | ✓ | S | A light theme and `NO_COLOR` (Phase 4); the `?` help lists and runs every action. |
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

**Phase 4: topology and connections (built 2026-11-14; spec.md "Topology and connections (P4)")**

Phases 1 to 3 cover the roadmap above. Cluster (v0.3) and SSH tunnels have landed since. Phase 4 takes the next gaps from the matrix: the "Cluster, including force-standalone", "Per-shard overview on cluster", "Sentinel", "Switch DB index while connected", "Import and export a connection list", "Tags and aliases on connections", "Filter by name and type" and "Themes" rows, plus feature idea 13.

| # | Feature | Effort | Notes |
|---|---|---|---|
| 1 | **Live views on a cluster**: routing by slot, a picked node for keyless reads, a per-shard summary on Server, sharded Pub/Sub, bulk actions and the command line per node | L | The largest gap left. Each node keeps its own command budget; no new node is connected to after discovery. |
| 2 | **Sentinel** connections, with Sentinel credentials kept apart in the keychain | M | Redis Insight has it. `--prefer-replica` uses `SENTINEL REPLICAS`. |
| 3 | **DB index switch** in the TUI | S | A rescan on the other database; the profile's saved `db` changes only when asked. |
| 4 | **Slots panel** from `CLUSTER SLOT-STATS` (Redis 8.2+, Valkey 8.0+) | M | Feature idea 13. Redis Insight has no per-slot view. |
| 5 | Type filter on the key list, `conn export`/`import`, tags, a light theme and `NO_COLOR` | S each | Export and import carry profiles only, never credentials. |

**Phase 5: the rest of the matrix (built 2026-11-15; spec.md "Milestones")**

Phases 1 to 4 left the gaps below. Phase 5 closes the ones that fit a TUI and are safe to test.

| # | Feature | Effort | Notes |
|---|---|---|---|
| 1 | **Eviction findings**: a `volatile-*` policy with few TTLs, a full `noeviction` server, LFU off | S | The "Recommendations" row. No new commands. |
| 2 | **`y` on Slow log and Monitor** | S | Copies the masked command line, never the raw arguments. |
| 3 | **Command line**: result views (text, table, JSON, raw), `N cmd` repeats, pipeline mode | M | The "Result views" and "Pipeline mode" rows. A repeat of a write that asks is refused, not asked once. |
| 4 | **Values**: Snappy, PHP, Pickle, Protobuf and Java formats, arrays and the TimeSeries, Bloom, Cuckoo, Count-Min Sketch, Top-K and t-digest types | M | The "Formatters" and "JSON, Vector Sets, Arrays" rows. The readers are bounded, and Pickle is read, never run. |
| 5 | **Force standalone** | M | In progress. |
| 6 | A documentation site and a shorter README | S | Not a matrix row. |

Not built, with reasons: cloud discovery (needs cloud accounts, can't be tested in CI), SSH password login (a large change on a security-sensitive path), connection presets from environment variables (conflict with rule 1, the owner's call), aliases (low value), server-side `HOTKEYS` (not confirmed on any image), uploading a commands file (low value), Vector Set similarity search, and creating JSON keys.

**Built, which Redis Insight also has:** cluster (v0.3, live views in Phase 4), Sentinel (Phase 4), SSH tunnels (v0.3, key login).

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
