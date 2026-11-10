# redis-lens: feature proposals

Research date: 2026-11-11. Research only; no repo files were changed.
Grounding: `spec.md` and `status.md` read in full. I also grepped `src/conn/mod.rs` (the permission probe already separates `NOPERM` from "unknown command") and `Cargo.toml` (redis-rs 1.8.1, `cluster-async` already enabled).

Ranking is value divided by effort, my judgement. Effort: S = days, M = 1-2 weeks, L = more. Items 1-7 are quick wins. Items 8-14 are bigger bets.

## What the field offers that redis-lens does not

Short version, with the sources I opened.

| Tool | Has that redis-lens lacks | Source |
|---|---|---|
| RedisInsight | Database analysis report: type distribution, expiry summary, top keys and namespaces, historical reports. Caps at 10,000 keys, then extrapolates. Bulk delete by pattern. Entra ID discovery for Azure. | https://redis.io/docs/latest/develop/tools/insight/ |
| redis-rdb-tools | Offline RDB analysis: memory CSV with encoding and element counts, `--largest N`, regex and type filters, diff. | https://github.com/sripathikrishnan/redis-rdb-tools |
| hdt3213/rdb | Offline RDB: `bigkey`, `hotkey`, `prefix` (memory by prefix, `-max-depth`), flame graph, JSON/CSV. RDB v1-12. | https://github.com/HDT3213/rdb |
| RMA | Pattern grouping by `SCAN` (like redis-lens) plus encoding and real-vs-expected RAM. Issue tracker shows it never worked on cluster. | https://github.com/gamenet/redis-memory-analyzer , https://github.com/gamenet/redis-memory-analyzer/issues/17 |
| redis-inventory | Hierarchical table and sunburst HTML chart of memory by pattern. | https://github.com/obukhov/redis-inventory |
| redis-cli | `--bigkeys`, `--memkeys`, `--keystats` (adds size distribution), `--hotkeys` (LFU only), `--cursor` to resume a scan, `-i` throttle, `--rdb` to pull a dump. | https://redis.io/docs/latest/develop/tools/cli/ |
| valkey-cli | `--bigkeys`, `--memkeys`, `--hotkeys` (LFU only). `--keystats` is not on the page I read. | https://valkey.io/topics/cli/ |
| redis-keyspace-stats (Rust crate) | Per-pattern memory and TTL percentiles from glob groups. Direct overlap, but alpha: version list ends at 0.5.4 (2023-04-21), table output only. | https://docs.rs/crate/redis-keyspace-stats/0.4.0 |
| redis_exporter | Prometheus metrics per key group: `redis_key_group_memory_usage_bytes`, with a cardinality cap. README warns scans are costly. | https://github.com/oliver006/redis_exporter |
| redis-tui (Go) | General client with memory stats, bulk TTL set, bulk delete, cluster node view, JSON export. No Valkey mention. | https://github.com/davidbudnick/redis-tui |
| memtui (Rust) | Key browser for Redis, Memcached, etcd. No memory analysis on the README. | https://github.com/dmk/memtui |
| Azure PaaS blog scripts | Bash plus Lua scripts that count keys with no TTL and keys over a size threshold. Shows demand for a TTL and size audit on Azure. Warns the Lua scan blocks the server. | https://techcommunity.microsoft.com/blog/azurepaasblog/redis-keys-statistics/4486079 |

Where redis-lens already leads: live `SCAN` plus `MEMORY USAGE` sampling, pattern inference with placeholders, TTL-risk verdicts, snapshot diff with cause attribution, keychain-backed profiles, a command budget, and replica preference. None of the tools above combine these. No standalone Valkey-specific memory profiler turned up, but that search was not exhaustive.

## Gaps in the evidence

- I found no Hacker News or Reddit threads on this topic. Two searches returned only blog tutorials. So "what users ask for" below rests on GitHub issues, tool feature lists and vendor docs, not forum discussion.
- GitHub issue evidence is thin. The useful hits are linked in each proposal.
- I could not confirm whether RedisInsight can export an analysis report. A search found only connection export.
- Google's Memorystore for Redis (non-Valkey) IAM page returned 404; I only verified Memorystore for Valkey.
- I did not verify that `aws-sigv4` supports query-string presigning (the docs.rs landing page did not say), or that any maintained Rust RDB parser exists.
- I did not read the AWS ElastiCache or Azure Managed Redis behaviour for `MEMORY USAGE` on the Azure side.
- The Redis 8 release where `INFO keysizes` first appeared is not stated on the INFO page. A Valkey issue says "new in Redis 8".

---

# Quick wins

## 1. `--check` thresholds and exit codes for CI and pre-deploy gates

**Pitch:** `redis-lens scan staging --check 'no-ttl-pct<20' --check 'growth<25%'` exits non-zero when a rule fails, so a pipeline can block a deploy.

**Why:**
- People already script this by hand. The Azure post above ships Bash and Lua to count keys with no TTL and keys over a size threshold. https://techcommunity.microsoft.com/blog/azurepaasblog/redis-keys-statistics/4486079
- A rdb-tools issue from 2013 is still open asking for expiry in the memory report so permanent keys can be told apart from expiring ones. https://github.com/sripathikrishnan/redis-rdb-tools/issues/27
- redis-lens already has the inputs: TTL verdicts, snapshot diff, and `--json`. Only the rule layer and exit codes are missing.

**What:**
- Rules over the existing `ScanReport` and `Diff`: pattern verdict equals `mixed` or `persistent-growing`, a pattern's no-TTL percentage, a pattern's memory growth against the baseline, total estimated memory, any pattern above X bytes.
- Rules come from repeated `--check` flags or a `[check]` table in a file (`--rules lens-rules.toml`). Print which rule failed and the pattern. Exit codes: 0 pass, 1 rule failed, 2 scan error. A partial or degraded scan (for example no `MEMORY USAGE`) should say so and fail only rules it could evaluate, with `--strict` to fail on unevaluable rules.
- Commands: none new. Same `SCAN`, `TYPE`, `PTTL`, `MEMORY USAGE`. Fully read-only.
- Follow-on, not in the first cut: `--webhook URL` that posts the pass/fail summary (Slack-compatible JSON). That sends data off the machine, so it should be explicit and print only pattern names and numbers, never key examples. I found no user request for Slack alerts specifically; treat it as speculative.
- Ship a GitHub Actions example in the README.

**Effort:** S.

**Risks:**
- Sampled estimates are noisy, so thresholds on small patterns will flap. Mitigate with a minimum-key-count floor per rule and by reporting the sample fraction.
- A CI job pointed at a production profile runs a real scan. Keep the command budget on and warn when the profile env is `prod`.

## 2. Report export: Markdown, HTML and CSV

**Pitch:** `redis-lens scan prod --report md|html|csv -o report.md` produces a shareable file for a PR comment, ticket or review.

**Why:**
- Peers export: rdb-tools writes a memory CSV, hdt3213/rdb writes CSV for memory, bigkey, prefix and hotkey reports, redis-inventory renders HTML sunbursts. Sources in the table above.
- RMA users asked for JSON output (issue closed). https://github.com/gamenet/redis-memory-analyzer/issues/28
- redis-lens has `--json` and the TUI only. A teammate without the tool cannot read the result.

**What:**
- Pure rendering over `ScanReport` and the diff. Markdown: summary, top patterns table, TTL risks, diff section when a baseline exists. HTML: single self-contained file with a sortable table and a share bar, no external requests. CSV: one row per pattern.
- No new server commands. No credentials in output (already true of snapshots); example key names are included by default in snapshots, so add `--no-examples` for reports that leave the machine.

**Effort:** S.

**Risks:** Example key names can contain user ids or emails. Default to examples off in reports, on in the TUI.

## 3. Provider detection and honest degradation on managed services

**Pitch:** Recognise ElastiCache, MemoryDB, Memorystore and Azure by host name, and explain which views will be empty and why before the scan runs.

**Why:**
- ElastiCache Serverless blocks the commands redis-lens depends on. The restricted list includes `memory`, `memory usage`, `object`, `object freq`, `client list`, `monitor`, `slowlog`, `latency` and `client tracking`. https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/SupportedCommands.html
- Real users hit this: a RedisInsight issue reports the analysis report shows key counts but no sizes on ElastiCache Serverless. https://github.com/redis/RedisInsight/issues/4051
- The same AWS page warns that frequent or large `SCAN` on serverless caches can raise latency and slow scaling. redis-lens should lower its default budget there.
- redis-lens already parses Azure host names (spec) and the probe already separates `NOPERM` from "unknown command" (`src/conn/mod.rs:106-132`), so the plumbing exists.

**What:**
- Host-pattern classifier: `*.cache.amazonaws.com`, `*.serverless.*.cache.amazonaws.com`, `*.memorydb.*.amazonaws.com`, Azure (already), Memorystore (private IP, so rely on the user setting a profile tag).
- A `provider` field on the profile, set by `conn add` and overridable. Used for: a pre-scan notice ("ElastiCache Serverless: MEMORY USAGE is restricted, so sizes will be missing; counts and TTLs still work"), a lower default rate, and a README table of what works where.
- Commands: none new. Read-only.

**Effort:** S.

**Risks:** Host patterns drift. Treat as a hint only; the probe result stays authoritative.

## 4. Server context strip: `INFO keysizes`, `INFO keyspace`, `MEMORY STATS`

**Pitch:** Show exact server-side facts next to the sampled estimates, so users can see how much of used memory the sample explains.

**Why:**
- Redis documents an `INFO keysizes` section: a base-2 log histogram of key sizes per database and per type (sample output has `db0_distrib_strings_sizes`, `db0_distrib_lists_items`, `..._hashes_items`). It is server-computed, so it is exact, not sampled. https://redis.io/docs/latest/commands/info/
- The `dbN` line in `INFO keyspace` includes `subexpiry` beside `keys`, `expires`, `avg_ttl` (same page). The page does not define it; I assume it counts keys with field-level expiry, which needs checking against a live 7.4 server.
- `MEMORY STATS` returns `overhead.total`, `dataset.bytes`, `keys.bytes-per-key`, `fragmentation`, and per-db `overhead.hashtable.main`. https://redis.io/docs/latest/commands/memory-stats/ . The page marks it unsupported on Redis Software and Cloud, so it must be optional.
- A Valkey feature request for `keysizes` is open, with a maintainer asking what the point is, and no sign it shipped. https://github.com/valkey-io/valkey/issues/1880 . Gate on the section being present.

**What:**
- Read `INFO keysizes` if present, `INFO keyspace` (already read for `DBSIZE` context), and `MEMORY STATS` if permitted. Add a small header in Patterns: "sample explains N% of dataset.bytes", plus the exact big-key histogram where available.
- Use the histogram as a cross-check on the sampled size distribution, and warn when they disagree sharply (sampling bias).
- ACL needs: `+info`; `+memory|stats` for the optional part. Add to the permission probe and the sample ACL line in the spec.
- Read-only.

**Effort:** S.

**Risks:** The `keysizes` version boundary is not verified, so feature-detect by section presence, not version. `MEMORY STATS` fields vary by version (some are 7.4+), so parse tolerantly.

## 5. External token command for passwordless auth (covers GCP, AWS and Azure today)

**Pitch:** A profile can say `auth = command` with a shell command that prints a short-lived token; redis-lens runs it at connect time and never stores the token.

**Why:**
- Every cloud's IAM auth is "a short-lived token sent as the password":
  - AWS: a SigV4 presigned token, valid 15 minutes. https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/auth-iam.html
  - Azure Managed Redis: an Entra token for scope `https://redis.azure.com/.default`, with the user set to the object ID. https://learn.microsoft.com/en-us/azure/redis/entra-for-authentication
  - Memorystore for Valkey: an IAM access token, one hour by default and up to 12 hours. https://docs.cloud.google.com/memorystore/docs/valkey/about-iam-auth
- AWS documents a CLI that prints the token (`developer-toolkit-elasticache generate_iam_auth_token`) and a `VALKEYCLI_AUTH` pattern that feeds it to valkey-cli (same AWS page). redis-cli has the same pattern with `REDISCLI_AUTH`. Users already do this; redis-lens cannot.
- RedisInsight users are asking for managed IAM and are told only a static password works: https://github.com/redis/RedisInsight/issues/5693

**What:**
- Profile fields: `auth = "command"`, `token_command = ["az", "account", "get-access-token", "--resource", "https://redis.azure.com", "--query", "accessToken", "-o", "tsv"]`, `username = ...`. Store the argv array in the config file (not a shell string, to avoid injection), username too since it is not secret.
- Run the command on connect and on reconnect; never write the output to disk, logs, `Debug` or snapshots (the existing credential rules apply). Hold it in a `secrecy` type. Time out after about 10 s.
- The TUI needs a refresh path for long sessions: re-run the command and send `AUTH user token` before expiry. Scans are short, so the CLI is simpler than the TUI here.
- Keep a clear note: the command comes from the user's own config, but config files can be copied around, so `conn add` should print the command and ask before saving it, and refuse a command argument that came from a URL.
- Commands sent: `AUTH`/`HELLO` only. No effect on read-only behaviour.

**Effort:** S.

**Risks:** Running a configured command is a new trust surface; document it. It does not replace first-class IAM (items 9 and 10) because it needs external CLIs installed.

## 6. Snapshot retention and trend view

**Pitch:** Make "scheduled snapshots" work without a daemon: `scan --save --keep 30`, `snapshots list|prune`, and a trend column in the Diff view.

**Why:**
- RedisInsight lets users "capture and track the changes in your database by viewing historical analysis reports", so history is expected. https://redis.io/docs/latest/develop/tools/insight/
- redis-lens already writes timestamped snapshots, but only compares against the newest one. A leak that grows 2% a day shows as noise in a day-over-day diff. A seven-point trend shows it.
- The spec rules out a monitoring daemon, so the schedule belongs to cron, systemd or CI. The tool only needs to be cron-friendly: quiet, deterministic exit codes, bounded disk use.

**What:**
- `--keep N` and `--keep-days D` on `scan --save`, plus `redis-lens snapshots list <profile>` and `prune`.
- `redis-lens trend <profile> [--pattern P] [--last N]`: per-pattern memory over the last N snapshots with a unicode sparkline and a least-squares growth rate. Surface it as a column in the Diff view.
- Respect the existing rule that snapshots with different `MATCH` globs or partial scans are not compared.
- Cron and GitHub Actions examples in the README. Local file reads and deletes only; the deletes touch redis-lens's own snapshot directory, not the server.

**Effort:** S to M.

**Risks:** Deleting snapshots is the first destructive action in the tool; confine it to the snapshot directory and require `--yes` on `prune`.

## 7. Compare two instances (staging vs prod)

**Pitch:** `redis-lens compare staging prod` diffs patterns across two profiles, so "does staging look like prod?" has an answer.

**Why:**
- It is on your list of workflow requests, and the spec places it in v0.3. I found no external issue asking for it, so I rate the evidence as "plausible, not demonstrated".
- It reuses the diff engine. `Diff` already takes two `ScanReport`s and attributes the change to key count or average size. Cross-instance comparison needs only the same logic with different labels and a policy for scale (absolute memory differs across environments).

**What:**
- Scan both (or load a saved snapshot for either side with `--left-file`/`--right-file`), then compare per pattern: present in one only, key-count ratio, average-size ratio, TTL distribution difference. Prefer ratios and shares of total over absolute bytes.
- Flag "TTL missing in prod but set in staging" style differences, which is the likely bug class.
- Works on standalone today. Cluster comes with v0.3 and needs no change here.
- Read-only on both servers; each scan keeps its own budget.

**Effort:** S to M.

**Risks:** Datasets are rarely the same size, so naive absolute diffs mislead. Lead with shares and ratios. Sampling differences between environments can look like real differences; show sample fractions.

---

# Bigger bets

## 8. Hash field TTL awareness (`HEXPIRE` and friends)

**Pitch:** Stop reporting hashes as "persistent" when their fields expire, and show field-level TTL where it exists.

**Why:**
- Redis 7.4.0 added per-field expiry: `HEXPIRE`, `HPEXPIRE`, `HEXPIREAT`, `HTTL`, `HPTTL`, `HEXPIRETIME`, `HPERSIST`. Fields are deleted when their TTL expires; `HSET` and `HDEL` clear field TTLs. https://redis.io/docs/latest/commands/hexpire/ , https://redis.io/docs/latest/commands/hpttl/
- Redis 8.0 added `HGETEX`, `HSETEX`, `HGETDEL`. https://redis.io/docs/latest/operate/oss_and_stack/stack-with-enterprise/release-notes/redisce/redisos-8.0-release-notes/
- Valkey has `HEXPIRE` from 9.0.0 (not in 8.x, which is what CI tests). https://valkey.io/commands/hexpire/ . ElastiCache lists these commands as supported. https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/SupportedCommands.html
- Today `PTTL` on such a hash returns -1 (no key-level TTL), so the TTL view would call a session-store hash with per-field expiry a "persistent leak". That is a false positive the owner will meet as people adopt the feature. This is the main reason this item is worth doing.
- Valkey 9.2 rc2 changed `MEMORY USAGE` to count expired-but-unreaped hash fields, so size estimates on such hashes are also in flux. https://github.com/valkey-io/valkey/releases (release candidate; subject to change)

**What:**
- Gate on `INFO keyspace` `subexpiry` greater than 0 (see item 4; meaning to be confirmed on a live server) or version at least 7.4 (Redis) / 9.0 (Valkey). When gated off, nothing changes.
- For sampled hashes with key TTL -1, read the first page of fields with `HSCAN key 0 COUNT 20` and then `HPTTL key FIELDS n f1 ... fn`. Report the share of fields with a TTL, and the min and max field TTL. New verdict: `field-ttl` (fine) versus `persistent` (still a concern).
- Extra commands per sampled hash: two, and only for hashes that need it. Fold into the command budget. `HSCAN` and `HPTTL` are read-only (`@read`), so the ACL needs `+hscan +hpttl`.
- Add the two commands to the permission probe and the spec's sample ACL.

**Effort:** M.

**Risks:**
- A first page of fields is a sample of a sample. Say so in the detail panel.
- Extra round trips on large hash-heavy keyspaces. Keep it behind the gate and cap at N hashes per pattern.
- Behaviour when all fields of a hash expire (key removal) is not something I verified; do not claim it in the UI.

## 9. Native AWS IAM auth for ElastiCache and MemoryDB

**Pitch:** `conn add prod --aws-iam --region us-east-1 --cache my-cache --user lens` with tokens generated from the standard AWS credential chain.

**Why:**
- The token is a SigV4 presigned GET to `http://<cache-name>/` with `Action=connect&User=<user>`, plus `ResourceType=ServerlessCache` for serverless. Service name `elasticache`, expiry 900 s; MemoryDB uses service `memorydb` and the same shape. Both documented with reference code. https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/auth-iam.html , https://docs.aws.amazon.com/memorydb/latest/devguide/auth-iam.html
- Constraints from the same pages: TLS required (ElastiCache), engine Valkey 7.2+ or Redis 7.0+, the user's name and id must match, the connection is dropped after 12 hours unless a fresh token is sent with `AUTH`/`HELLO`, and re-auth does not work inside `MULTI`/Lua. Cache names are lowercased at creation, so the code must lowercase.
- Demand: RedisInsight has an open request for exactly this and the maintainers said they will work on it, starting with ElastiCache and then possibly MemoryDB. https://github.com/redis/RedisInsight/issues/5693
- Item 5 makes it possible by shelling out, but a user on a locked-down laptop with AWS SSO should not have to install a Python toolkit.

**What:**
- New auth kind `aws-iam` (behind an `aws-iam` cargo feature, like `entra-id`). Profile stores region, cache or replication-group id, user id, serverless flag, optional AWS profile name. No secret is stored; the AWS default credential chain (env, SSO, instance role) supplies credentials.
- Build the presigned token in-process. The AWS SDK for Rust or `aws-sigv4` can sign; I did not confirm that `aws-sigv4` supports query-string presigning, so a short spike is needed first. redis-rs also exposes a `token-based-authentication` feature with `StreamingCredentialsProvider` and `TokenRefreshConfig` (names seen on docs.rs, behaviour not read), which may handle refresh. https://docs.rs/redis/latest/redis/
- Re-sign at least 1-2 minutes before expiry in the TUI; re-send `AUTH`. The CLI scan finishes well inside 15 minutes.
- Permission check must also surface IAM failures (no `elasticache:Connect` on the cache and user) with a readable message.
- Pairs with item 3: ElastiCache Serverless will still hide `MEMORY USAGE`.
- Commands: `AUTH`/`HELLO`. Read-only unchanged.

**Effort:** M. Larger than it looks because of the dependency weight of the AWS crates; keep it feature-gated.

**Risks:** New heavy dependencies and compile time. Cannot be tested in CI without an AWS account; needs a signing unit test against AWS's published examples and a manual check.

## 10. Azure managed identity, service principal and Azure Managed Redis

**Pitch:** Finish Entra support: managed identity (system or user assigned), service principal, and honest handling of Azure Managed Redis cluster policies.

**Why:**
- Today only `EntraIdCredentialsProvider::new_developer_tools()` is wired up (status.md), so it works on a laptop with `az login` but not on an Azure VM, AKS pod or CI runner. The spec text says `DefaultAzureCredential` with managed identity; the implementation has not caught up.
- redis-rs 1.8.1 exports `ManagedIdentityCredentialOptions`, `UserAssignedId`, `ClientSecretCredentialOptions`, `ClientCertificate` and `DeveloperToolsCredentialOptions` under `entra-id` (docs.rs lists the names; their behaviour is not described there, so read the source before relying on them). https://docs.rs/redis/latest/redis/
- Azure rules: scope `https://redis.azure.com/.default`; username is the object ID of the identity; Entra needs TLS; the client must send a fresh token via `AUTH` at least three minutes before expiry; no Entra groups; some commands are blocked. https://learn.microsoft.com/en-us/azure/redis/entra-for-authentication
- Azure Managed Redis runs on the Redis Enterprise stack and is always clustered internally, with three policies: OSS, Enterprise, Non-clustered (up to 25 GB). Enterprise and Non-clustered present one endpoint and work with non-cluster clients. OSS policy needs a Redis Cluster client. https://learn.microsoft.com/en-us/azure/redis/architecture . So standalone redis-lens will work against Enterprise and Non-clustered policy instances now, and OSS policy only after v0.3.
- Redis docs mark `MEMORY STATS` and `MEMORY DOCTOR` as unsupported on Redis Software and Redis Cloud, which share this lineage. I did not verify `MEMORY USAGE` or `OBJECT FREQ` on Azure Managed Redis. https://redis.io/docs/latest/commands/memory-stats/
- Redis Insight already does Azure discovery and Entra login, so Azure users will compare us with it. https://redis.io/docs/latest/develop/tools/insight/

**What:**
- `conn add --entra managed-identity [--client-id X]`, `--entra service-principal` (client id, tenant id, and the secret from stdin into the keychain), keep `developer-tools` as the default for local use.
- Show the cluster policy in the detail panel when it can be detected (via `CLUSTER INFO`, which is blocked under Enterprise policy per Azure's blocked list, so detection by failure). When the OSS policy is hit, say it needs v0.3.
- Token refresh loop in the TUI, with jitter (Azure advises it).
- Test plan: unit tests for option mapping, and a manual run on a real Azure identity, since status.md records Entra as untested.

**Effort:** M.

**Risks:** Cannot be exercised in CI. Token-refresh behaviour in redis-rs is unverified. Client secret handling has to follow the existing keychain-only rule.

## 11. Prometheus / OpenMetrics export of pattern statistics

**Pitch:** `redis-lens scan prod --prom out.prom` writes pattern-level gauges for the node_exporter textfile collector or a Pushgateway, no daemon needed.

**Why:**
- redis_exporter already exports per-key-group memory, but each aggregation runs `MEMORY USAGE` on every key through `SCAN`, and its README says so and caps cardinality with `--max-distinct-key-groups`. https://github.com/oliver006/redis_exporter
- Users ask the exporter for per-node big-key and memory attribution and are pointed elsewhere. https://github.com/oliver006/redis_exporter/issues/995
- redis-lens infers patterns automatically, uses a sample and a rate budget, and already has the numbers. A cron job that writes a textfile every 15 minutes gives a trend in Grafana at a fraction of the exporter's scan cost, which is useful when the exporter's per-scrape scan is too heavy.

**What:**
- Metrics per pattern: `redis_lens_pattern_estimated_keys`, `..._estimated_bytes`, `..._no_ttl_ratio`, plus scan metadata (sample fraction, duration, complete flag, timestamp). Cap the number of patterns exported (top N plus `other`), as redis_exporter does, to protect Prometheus cardinality.
- OpenMetrics text only, written to a file or stdout. Optional `--push URL` for a Pushgateway is a later step because it sends data off the box.
- Overlaps with item 6 in cron usage.
- Reading only on the Redis side.

**Effort:** M.

**Risks:**
- Pattern names are labels, so pattern drift (new `{var}` collapses) creates series churn. Cap and normalise.
- It brushes against the "not a monitoring daemon" non-goal; keep it a file writer, not a server.

## 12. Server-side `HOTKEYS` tracking as the opt-in sampler

**Pitch:** Replace the planned `MONITOR` fallback with the server's own hot-key tracker where it exists.

**Why:**
- Redis 8.6.0 added `HOTKEYS START | STOP | GET | RESET`. Hotness is CPU time and network bytes per key over a tracking window, kept in a probabilistic structure, one session at a time. `INFO hotkeys` reports `tracking_active`, `used_memory` and `cpu_time`. https://redis.io/docs/latest/commands/hotkeys/ , https://redis.io/docs/latest/commands/info/
- Valkey 9.2.0 (release candidates only; rc2 on 2026-11-04) has `HOTKEYS GET`, but it reports estimated QPS per key with a `hotkeys-sampling-percentage` default of 1, a different metric from Redis. The page also describes it as `@admin @dangerous @slow`. https://valkey.io/commands/hotkeys-get/ , https://github.com/valkey-io/valkey/releases
- The two server APIs differ in shape and meaning, so a naive wrapper would mislead. A profiler that normalises them is useful.
- The current Hot keys view needs an LFU eviction policy. Most caches run `allkeys-lru` or `noeviction`, so the view is often empty. `redis-cli --hotkeys` and `valkey-cli --hotkeys` have the same limit. https://redis.io/docs/latest/develop/tools/cli/ , https://valkey.io/topics/cli/
- It beats `MONITOR` (the spec's current plan): lower overhead and no stream of every command.

**What:**
- Opt-in only: `redis-lens hot <profile> --track 30s`. The TUI prompts first.
- Redis: `HOTKEYS START METRICS 2 CPU NET COUNT 10`, wait, `HOTKEYS GET`, `HOTKEYS STOP`, `HOTKEYS RESET`. Valkey: its start/stop equivalent (I read only the `GET` page; the `START` syntax is unverified).
- Safety: this starts a tracking session on the server. It writes no data, but it is a server-side state change and an admin-category command, so it breaks the spec's current blanket "never changes anything". Decide explicitly: allow it only on non-prod profiles by default, require `--allow-admin` on prod, and always run `STOP` and `RESET` on exit, including on Ctrl-C. Refuse if a session is already active (check `INFO hotkeys`), so redis-lens never kills someone else's tracking.
- redis-rs async cluster connections do not support `HOTKEYS` per the Redis page, so it is single-node only (fine for standalone).

**Effort:** M.

**Risks:** Valkey 9.2 is not released yet, so syntax may move. Redis 8.6+ and 9.2 only, so a small share of users. Needs the new ACL line in the docs.

## 13. Per-slot view with `CLUSTER SLOT-STATS` (after v0.3)

**Pitch:** In cluster mode, add a "slots" view showing key count and, where enabled, CPU and network per slot, so a hot or oversized slot shows up next to the pattern that causes it.

**Why:**
- Valkey 8.0.0 added `CLUSTER SLOT-STATS` with `SLOTSRANGE` and `ORDERBY` and `LIMIT`. Metrics: `key-count` always, and `cpu-usec`, `network-bytes-in`, `network-bytes-out` only when `cluster-slot-stats-enabled yes`. https://valkey.io/commands/cluster-slot-stats/
- ElastiCache lists it as supported. https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/SupportedCommands.html
- It is a Valkey 8+ feature. I did not find an equivalent in the Redis docs I opened.
- It fits the v0.3 cluster plan and the spec's "node column".

**What:**
- In cluster mode, per primary: `CLUSTER SLOT-STATS ORDERBY key-count LIMIT 20 DESC`, and the same ordered by `cpu-usec` when enabled. Map a pattern's example keys to slots with `CLUSTER KEYSLOT` (a local CRC16 calculation avoids even that call).
- Read-only. The CPU metrics need an operator to enable a config; redis-lens must not set it. Show "not enabled" with the config name.

**Effort:** M, and only after cluster scanning exists.

**Risks:** Tied to the v0.3 schedule. Narrow audience (Valkey cluster users).

## 14. Offline RDB analysis

**Pitch:** `redis-lens rdb analyze dump.rdb` runs the same pattern, TTL and diff pipeline on a dump file, with no server contact.

**Why:**
- It is on your list, and every well-known offline tool has problems with newer servers: redis-rdb-tools fails with "Invalid RDB version number 11" (open since 2025-03). https://github.com/sripathikrishnan/redis-rdb-tools/issues/204 . hdt3213/rdb lists RDB v1-12, covering up to Redis 7.2, and has an open issue asking for Valkey compatibility tests. https://github.com/HDT3213/rdb , https://github.com/HDT3213/rdb/issues/67
- It gives exact sizes and zero load on the server, and it works on a backup, which is the only safe route for a fragile production box and for managed services that restrict `MEMORY USAGE` (see item 3).
- `redis-cli --rdb <file>` can fetch a dump from a remote server, so there is a path to a file. It uses replication sync, which makes the primary produce a dump, so it is not free. https://redis.io/docs/latest/develop/tools/cli/

**What:**
- A parser for RDB (Redis and Valkey formats) and an in-memory size estimator, feeding the existing `ScanReport`. Snapshots saved from RDB input should carry `source: rdb`, so the diff engine does not compare exact and sampled numbers silently.
- Per-key size in an RDB is an estimate of in-memory size, not `MEMORY USAGE`; rdb-tools says "approximate" for the same reason. Document the error.
- I did not verify that a maintained Rust RDB parser crate exists. If none does, this is a large build; if one does, it drops to M.

**Effort:** L.

**Risks:** Format churn (new encodings in each Redis and Valkey release; Redis 8's new types), memory estimates that disagree with live numbers, and a large surface to maintain. Lowest value per effort of the list.

---

# Considered and not proposed

- **Keyspace notifications.** Off by default and enabled with `CONFIG SET notify-keyspace-events` (or in redis.conf), at some CPU cost; events are fire-and-forget and per node. https://redis.io/docs/latest/develop/pubsub/keyspace-notifications/ . `CONFIG SET` breaks the read-only guarantee, and ElastiCache blocks `CONFIG` anyway. One read-only variant exists: if an operator has already enabled `x`/`e` events, subscribe for a minute to count expirations and evictions per pattern. That is a niche, so I left it out.
- **Client-side caching (`CLIENT TRACKING`).** It serves cache clients, not profilers. ElastiCache Serverless restricts it. https://redis.io/docs/latest/commands/client-tracking/ , SupportedCommands page above.
- **`LATENCY DOCTOR`.** Needs `@admin @dangerous`, and latency monitoring is not what redis-lens is for. https://redis.io/docs/latest/commands/latency-doctor/
- **`MEMORY DOCTOR`.** Returns free text and is unsupported on Redis Software and Cloud. It could be a one-line pass-through in item 4 if wanted, but the output is not machine-readable. https://redis.io/docs/latest/commands/memory-doctor/
- **Bulk delete / bulk TTL (RedisInsight, redis-tui).** Conflicts with the spec's read-only stance. The spec's planned `expire --dry-run` is a better fit for the one allowed exception.

# Version notes worth knowing

- Redis Open Source: 8.0.0 (May 2025), 8.2, 8.4, 8.6 (Feb 2026), 8.8, 8.10 (Jul 2026), 8.12-RC1 (Oct 2026). https://redis.io/docs/latest/operate/oss_and_stack/stack-with-enterprise/release-notes/redisce/
- Valkey: 9.1.3 is latest stable as of 2026-10-26; 9.2.0-rc2 on 2026-11-04. https://github.com/valkey-io/valkey/releases
- CI today tests Redis 7.4 and Valkey 8.1. Valkey 8.1 has no `HEXPIRE` (since 9.0.0) and no `HOTKEYS` (9.2 rc). Items 8 and 12 therefore need a newer Valkey in the integration matrix, or they stay untested on Valkey.
- A Redis licence change is documented in the 8.0 notes (RSALv2, SSPLv1 or AGPLv3 choice). Not relevant to the proposals, noted only because it is why Valkey 9.x deserves equal test weight.
