# redis-lens spec

What redis-lens does and how it should behave. This is the source of truth: when behaviour changes, change this file in the same PR. Progress lives in [status.md](status.md).

## Purpose

A terminal UI that answers "what is using the memory in this Redis or Valkey, and is any of it a mistake?" It has to be safe to point at production and quick to point at a local container.

Planned (see [Browsing, server views and writes](#browsing-server-views-and-writes)): a one-stop shop for Redis and Valkey in the terminal, like Redis Insight. Browse keys and values, watch the server, streams, Pub/Sub and Search indexes, run commands, and change data, but only with your permission ([Write permissions](#write-permissions)).

**Users:** backend developers and SREs who run Redis or Valkey as a cache, queue or session store.

**Non-goals:**
- A monitoring daemon: nothing keeps running after you quit.
- An AI assistant, or anything else that sends your data to a third party.
- Managing Redis Data Integration pipelines.
- A GUI, a web UI or a REST API.

redis-lens writes to a server only through the one write gate ([Write permissions](#write-permissions)): never on a read-only profile, which is the default for `prod`.

## Supported servers and platforms

- **Servers:** Redis 7.x and Valkey 8.x, standalone and cluster, are tested in CI. Redis 6.2 is tested in CI too (a separate integration job), standalone and cluster. A cluster is found from its seed node and scanned shard by shard (see [Cluster](#cluster)); redis-rs's cluster client is not used. Sentinel is planned for P4 ([Sentinel](#sentinel-p4)).
- **Platforms:** macOS, Linux and Windows on x86_64, plus aarch64 for macOS and Linux.
- **Minimum Rust version:** 1.89.
- **Install:** Homebrew (`brew install magna-nz/tap/redis-lens`), the shell and PowerShell installers attached to each GitHub Release (they install into `$CARGO_HOME/bin`), `cargo install redis-lens` from crates.io (`--features entra-id` for Entra ID), or a binary archive from GitHub Releases.
- **Licence:** MIT ([LICENSE](LICENSE)).

## Views

The TUI has a home screen and five areas, switched with `0` to `5` ([Navigation](#navigation)): 0 Connections (the home screen), 1 Overview, 2 Browse, 3 Analyze, 4 Server and 5 Pub/Sub. `6` does nothing. Three areas have panels, shown after the area's name in the area row, switched with `tab` and `shift-tab`:
- 2 Browse: Keys · Streams · Search;
- 3 Analyze: Big keys · Expiry · Changes · Hot keys, plus Nodes · Slots only on a cluster;
- 4 Server: Clients · Slow log · Monitor.

Connections, Overview and Pub/Sub have no panels. The sections below describe Connections, Overview, Browse › Keys and its two deeper levels, and the Analyze panels; the other panels are in [Browsing, server views and writes](#browsing-server-views-and-writes). The keys are listed in [Keys](#keys) and in the `?` help; both, and the footer at the bottom of the screen, come from one table in `src/tui/keymap.rs`.

Every list moves with `↑↓` (`j` `k`), `PgUp` `PgDn` (one page: as many rows as the list shows on screen, at least one; it stops at the first and last row) and `Home` `End` (`g` `G`). Each list has columns it always shows, columns that join in on a wider terminal, and columns only `v` adds ([Navigation](#navigation)). Narrow terminals never wrap: names are shortened with `…`, and when the name column would get narrower than its minimum, the optional columns are hidden first; only then is a row too wide for the terminal cut at the right edge. On the selected row, text that doesn't fit its column scrolls across: it rests for 1 s, scrolls about 5 columns a second until its end is visible, holds 1 s and starts again. On Browse › Keys only the name scrolls; the tree indent and the `▾`/`▸` mark stay in place. Selecting a different row, switching screens or starting a new run restarts it; the same row moving to another position does not.

### 0 Connections
- Lists saved profiles and the Redis or Valkey containers found in Docker, in one frame: the list on top, the selected entry's details below it. On first run, with nothing saved and no container found, the panel says how to add a connection and that Docker containers appear on their own.
- The list has five columns at 80 columns: name, environment badge (PROD, STAGE, LOCAL, OTHER), address, access (`read-only` or `writes on`) and status (`○ not open`, `● connecting…`, `● connected`, `▲ limited`, `✗ failed`: a glyph and a word, never colour alone). When the list is 100 cells wide or more, SIGN-IN and MEMORY join them. At 80 columns `v` adds SIGN-IN only; MEMORY does not fit beside the address.
- The details say what the entry means in words, one labelled row each: Address (with TLS on or off, the database when it is not 0, and once connected the server, its version and its topology), Docker (for a container: its name and image), Sign-in (where the password lives, such as "password or ACL user, kept in the OS keychain"), Access (why writes are on or off, such as "read-only, the default for production. w turns writes on."), Tags and Sentinel when the profile has them, otherwise Values ("shown on screen, never saved"), and Checks: the permission check in one sentence. `v` lists each check that ran.
- `+` adds a connection by pasting a connection string (see [Connection strings](#connection-strings)). The parsed result is previewed with the password masked before anything is saved. `+` adds a connection on Connections only.
- For a Docker container the SIGN-IN column shows what discovery found out (see [Docker discovery](#docker-discovery)): `password`, `none`, `required` or `unknown`. The Sign-in row says where the credentials come from: for `password`, the container, held in memory only; for `required`, not found, so save it as a profile with credentials.
- `enter` connects to the selected profile and lands on [1 Overview](#1-overview). A connect reads no key: it reads the server's own figures and nothing else ([What a connect reads](#what-a-connect-reads)). When a connect for a keychain profile takes longer than 2 s, the header and Overview's connecting line say "Waiting for the OS keychain. If a permission dialog is open, choose Always Allow." After 15 s the keychain read gives up (see [Credential rules](#credential-rules)).
- Each `enter` starts a new run, also when the same entry is connected again. Connection results, figures, progress, partial results and results of a replaced or stopped-and-replaced run are ignored.

### 1 Overview

The first screen after a connect. One frame of two sections: "Now", live, above "Keys", what the server says about its keys and, after an analysis, what the analysis found.

#### What a connect reads

A connect reads no key. In order:
1. The [Permission check](#permission-check) and the view probes ([Permission check, additions](#permission-check-additions)). The key-command probes send `SCAN 0 COUNT 10`, `TYPE`, `PTTL`, `MEMORY USAGE` and (under an LFU policy) `OBJECT FREQ` once each, on the first key `SCAN` returns or on `redis-lens:probe`; that is all a connect sends that names a key.
2. The server's figures (`scan::figures`), each taken from the command budget before it is sent: `INFO server`, `INFO memory`, `DBSIZE`, `INFO keyspace`, `INFO keysizes` and `MEMORY STATS` (not sent when the permission check denied it). These are the same six commands, through the same function, that a scan reads first ([Scanning](#scanning), step 1).
3. The live connection opens ([Shared command budget](#shared-command-budget-p1)).
4. Once, through the live requests of Server › Slow log and Server › Clients and the budget, each only where the permission check allowed it: `SLOWLOG GET 128` (with `CONFIG GET slowlog-*` where allowed and `COMMAND INFO` for the key specs of the commands it names, as the Slow log panel asks) and `CLIENT LIST`. Their answers fill those two panels as if they had polled; each panel still reads again when it is first shown.
5. The newest saved snapshot of the profile, from the snapshot directory only, never the server, for the "Last analysis" line.

On a cluster, step 1 runs on every connected node through that node's budget, step 2 reads each node's figures through its own budget (up to 4 nodes at once) and sums them as a cluster scan does ([Cluster](#cluster), step 7), and step 4 goes to the first primary in slot order. When `[` `]` pick another node, the slow-log and idle-client findings of the last node go, and the new node's `SLOWLOG GET` and `CLIENT LIST` are read once, the same way (through that node's budget, each only where the permission check allowed it), while Overview is on screen: at once on Overview, or when Overview is next shown. Nothing polls them there; the Slow log and Clients panels poll on their own. A step-4 answer that arrives after the panel already has a newer one (its own poll answered, or one in flight) is dropped. A connect never sends `KEYS`, `GET` or any value read.

#### Now

- Six figures from the Overview `INFO` poll, two to a row, each with a sparkline: ops/s, clients, memory (of the limit when `maxmemory` is set), hit rate, evicted/s and expired/s. Below them one line: server and version, uptime, role and replicas, persistence (RDB and AOF, with a failed save or write in red) and `maxmemory-policy`.
- The poll sends `INFO` every 2 s, through the budget, only while Overview is on screen. `space` pauses and resumes it ("Live figures paused. space resumes."), and `>` `<` step the interval through 1, 2, 3, 5, 10, 15, 30 and 60 s (`>` polls more often, `<` less). The section's right title says how often it polls, or that it is paused or off. Rates come from the difference between two polls and are left out after a restart, a counter reset or a long gap, never shown as a spike.
- `v` shows every figure the poll reads (twelve: also blocked clients, network in and out, CPU user and sys, and fragmentation).
- **On a cluster** the section is titled with the picked node (`node 127.0.0.1:7001 · 1 of 3`) and its right title says `[ ] picks a node`; `[` `]` pick another ([Cluster live views](#cluster-live-views-p4)). Below the figures, one row per shard in slot order: its slots, node, role, ops/s, memory and keys, the picked one marked `›`, a node without a connection `✗ not connected` and one that did not answer `✗ no answer`. The rows come from the per-shard poll (one `INFO` per shard's node every 5 s, or at the Overview's interval if that is longer, each from its node's budget, only while Overview is on screen and not paused); until it answers they show the connect's figures.
- A refused `INFO` turns "Now" off with a one-line reason; the rest of Overview stays.

#### Keys

- **Headline**, from the connect's figures: `7.92M keys · 31% have a TTL · 13G of data, 2.1G overhead` (`DBSIZE`; `expires` over keys of `INFO keyspace`; `dataset.bytes` and `overhead.total` of `MEMORY STATS`). A part whose figure is missing is left out.
- **Before an analysis**, "FROM THE SERVER  no keys read": what the server says on its own, each finding with a WHERE column naming the panel `enter` goes to (`↑↓` pick one):
  - big keys counted by `INFO keysizes` (strings of 1 MiB or more, collections of 65,536 items or more), when there are any: Analyze › Big keys;
  - the slowest slow-log entry, by its command name and duration only ("KEYS took 812 ms at 14:01 UTC."), never its arguments or keys: Server › Slow log;
  - many idle connections from one address: at least 10 connections from one IP address, each idle for an hour or more ("212 connections from 10.0.9.17 idle for over an hour."): Server › Clients.

  With none, it says "Nothing stands out in what the server reports." Under it, "WHAT USES THE MEMORY  needs a sample of the keys" and the line `r  Analyze: 10,000 keys, about 30K read-only commands, about 6 s.` from the [estimate](#analysis), then, when the profile has a saved snapshot, "Last analysis: today 09:12 UTC, 100,000 keys (a saved snapshot)." (the date instead of "today" on another day). The section's right title is "from INFO, MEMORY STATS, SLOWLOG · no keys read" (on a cluster "summed over N shards · no keys read", N the shards whose figures came in and are in the sum).
- **While an analysis runs**, "ANALYZING  sampling 4,100 of 10,000 keys", a bar and the time left at the budget (the keys still to sample, their `SCAN`s and per-key commands; the figures are read by then), then which commands it sends per key and the budget ("TYPE, PTTL and MEMORY USAGE for each sampled key, at most 5,000 commands/s."), and "Browse and Analyze fill in as it goes.  esc stops it and keeps what it has." The server's findings stay below. The right title gives the analysis's estimated commands in all.
- **After an analysis**, "THINGS TO LOOK AT", each with its WHERE: the [recommendations](#3-analyze) (big keys and encodings go to Big keys; keys or memory that never expire, and lock keys without a TTL, go to Expiry; a pattern that never expires and grows goes to Changes when there is a snapshot to compare with, else Expiry; the eviction findings go to Expiry, Big keys or Hot keys, see [Recommendations](#3-analyze)), then the slowest slow-log entry. "Nothing stands out in the sample." when there are none. Then "WHERE THE MEMORY IS": the three patterns with the most memory, each with a share bar, and the rest on one row ("N other patterns"). From 100 cells wide, one more line, "FREED BY TTL  within 1 h … · 1 d … · 7 d … · never …": the memory the TTLs free within 1 h, 1 d and 7 d, and the memory that never expires (the 30 d figure and the bars are on [Analyze › Expiry](#analyze--expiry)); without sizes it says it needs them. The right title is "10,000 sampled keys at 14:02 UTC", with "· stopped early" after a stopped analysis and, from 100 cells, "· a analyzes again".
- **Degrading** (rule 3): each part the server refused or lacks is left out with one line, in redis-lens's words: "Big keys from the server need INFO keysizes (Redis 8 and later).", "Data and overhead are missing: MEMORY STATS is denied for this user." (or "this server has no MEMORY STATS."), "The share with a TTL is missing: INFO keyspace could not be read.", "Slow log is off: …", "Idle clients are off: …". When `INFO server`, `INFO memory` or `DBSIZE` fails, the figures are missing with a fixed sentence (such as "INFO or DBSIZE is denied for this user (ACL +info +dbsize), so the server's figures are missing"), never the server's text, which can name the ACL user. None of this aborts the connect.
- While connecting, the section says "Connecting to <name> (<address>) and checking permissions…", with the keychain hint when it applies. A provider's notice ([Providers](#providers)) shows at the bottom in every state.
- When the analysis's report arrives, a notice says so: "✓ Analysis done: 10,000 keys in 7s. 5 things to look at on 1 Overview." (on Browse: "Overview lists 5 things to look at: press 1."); for a stopped one, "Analysis stopped: kept 4,100 sampled keys. The results are partial."

The keys: `a` asks "Analyze the keys?" ([Analysis](#analysis)), `enter` goes to the selected finding's panel, `esc` stops a running analysis, `space` `<` `>` and `v` as above.

### 2 Browse › Keys

The pattern tree on the left and the selected row's details on the right, in one frame with a shared divider. `→` on a pattern lists its keys on the left ([the key list](#2-browse--keys-additions)), with that pattern's summary on the right; `→` on a key shows its value on the right while the key list stays on the left ([the value pane](#browse--keys--a-keys-value)). `←` goes back one step each time. The left pane takes 41 of 80 columns and 74 of 120; with `v` it takes 70%.

#### Before an analysis: from key names

- **Reading names.** The first time Browse › Keys shows with the live connection open, redis-lens reads key names with the names pager (`live::names`): one `SCAN cursor COUNT 1000` per page, each taken from the command budget, one page in flight at a time. It reads 8 pages on its own (about 8,000 names), then stops; `m` reads one page more. Nothing else about a key is read: no `TYPE`, `PTTL`, `MEMORY USAGE`, `OBJECT`, length or value until the key is opened. Names that are not UTF-8 are counted as names read but get no pattern. A name `SCAN` returns twice is counted once.
- **On a cluster** there is one cursor per shard's node (the primary, or the verified replica with `prefer_replica`), each page sent to that node only, with redirects never followed. The nodes take turns, one page each, in slot order, so the names read so far come from every shard alike. A node discovery could not connect to is left out, and the right pane names it ("▲ 10.0.1.13:6379: not connected, so its key names are not read."). The bottom line says "2 of 3 nodes done" while the walk goes on.
- **Patterns** are inferred from the names exactly as a scan infers them ([Pattern inference](#pattern-inference), the `{var}` collapse included).
- **Estimated counts.** A pattern's keys are `names_seen × max(1, DBSIZE / names_read)`, rounded, where `DBSIZE` is the connect's figure (summed over the nodes on a cluster) and `names_read` counts every distinct name read. Once every node's cursor has returned to 0 with none left out, the scale is 1 and the counts are exact. The KEYS column shows an estimate as `~1.9M` and an exact count without `~`. A pattern that names one key (its only name read is the pattern itself, so nothing in it varies; a name with a hash tag such as `{user1}.following` keeps its braces and is one key too) shows `1 seen` while the walk goes on and is never scaled.
- **Columns:** PATTERN and KEYS only. The rows carry no type, size or TTL, so no TTL figure, no `▲` and no sort but keys and name (`s` toggles the two; the default is keys). The area row says "names only, no analysis yet".
- **Right pane:** the selected row's estimate ("~390K keys (estimated from names)", or "N keys (every name read)", or "One key: its name has no part that varies"), how many of the names read are its own, that Browse reads key names with SCAN, 1,000 a call, and reads nothing else until a key is opened, that sizes, TTLs and types come from an analysis (`a`), and "→ lists its keys".
- **Bottom line:** "8,000 names read · m reads more"; "All 2,701 names read" once complete; "some nodes left out" when the walk ended with nodes missing; "reading more…" while a page is in flight.
- **Degrading:** a refused `SCAN` leaves that node out, is never asked again, and says "The server refused SCAN for this user, so no key names can be read." Any other failure stops the reading with the reason, and `m` tries the same page again. On a cluster, a node whose page fails again when `m` tries it once more is left out ("10.0.1.12:6379: did not answer SCAN twice, so its key names are left out and the other nodes go on." in the right pane) and the other nodes' walk goes on; the walk is then never complete. A closed live connection never leaves a node out. A page in flight when its connection is replaced is asked again on the next one.
- While an analysis runs no names are read (the budget is the analysis's); the names tree stays until the analysis's first partial results replace it ("names only until the analysis has results"). `m` then says the analysis's patterns replace the key names. A run that reads no key (a [database switch](#database-switch-p4)) starts the names afresh.

#### After an analysis

- Key patterns, sorted by estimated memory by default. `s` cycles the sort: memory → keys → average size → no-TTL % → name → memory. Name sorts A to Z, the others largest first; ties go by name, A to Z. Without sizes (`MEMORY USAGE` denied) memory and average size are skipped, and the default sorts by keys. Groups sort by their totals (summed keys and memory, the group's average size and no-TTL share, the group label for name) and children sort inside their group by the same key. The header marks the sorted column with `▼` (`▲` for name), the title says `by <key>`, and a short notice confirms the change. The sort lasts while the session's analyses run and finish, and is not saved between runs; connecting starts again by memory.
- **Columns.** At 80 columns: PATTERN, KEYS, MEMORY and the `▲` mark. When the list's inside is 60 cells or more (a terminal of about 110 columns), TYPE and a share bar with its percentage join in. `v` adds the average size, the share with no TTL and Δ, the change since the last snapshot (when there is one to compare with). When the pattern column would get narrower than 20 cells, the optional columns go in this order: the share bar, Δ, the average size, TYPE, NO TTL.
- **`▲`** marks a row with something to look at: a lock pattern whose keys have no TTL, a sampled key of 64 MiB or more, a growing pattern that never expires, or a pattern where most keys expire but some don't. A pattern whose keys never expire and that does not grow looks intentional and is not marked. A group is marked when one of its patterns is. The right pane says why in a sentence ("▲ Lock keys that never expire: a holder that crashed never lets go of them.").
- Child patterns group under their shared prefix and can be expanded or collapsed.
- The selection follows its row, not its position, whenever the rows change (new partial results, the finished scan, another sort): a group by its shared prefix, a pattern by its name. A pattern now inside a collapsed group selects the group; a group that became a single pattern selects that pattern; a group or pattern that merged into `{var}` selects what it merged into. When the row is gone the selection stays at the same position, or the last row if there are fewer.
- A collapsed group stays collapsed when its patterns change: it is remembered by its prefix and by its patterns. If its prefix changes because part of it merged into `{var}` (`tenant.a1:*` and `tenant.a2:*` become `tenant.{var}:*`), or splits back out, the new group is collapsed when it holds one of the collapsed group's patterns, equal or up to `{var}`. A collapsed group that disappears or becomes a single pattern is collapsed again if it comes back. New groups start expanded.
- While an analysis runs, the tree shows partial results aggregated from the keys sampled so far (no extra server commands, at most four updates a second), marked `partial · N / M sampled` in the title and without the Δ column. When the pane has no room for that beside "Patterns", it takes the title's place. The finished analysis replaces them; a failed one clears them (see [Analyzing again](#analyzing-again) for the previous results kept meanwhile). Expiry, Changes and Hot keys wait for the finished analysis. `ctrl-s` never saves partial results: it says to wait for the analysis to finish.
- The right pane shows the row's keys, memory and share of memory; the typical key size (the median) and the largest, and on a wide pane the size 99% of keys stay under; why it is marked, or for a row that is not what its TTLs say; an EXPIRES IN histogram over the buckets none, <1m, <1h, <1d, <7d, <30d and >30d; up to five example keys with their sizes and expiry ("never" for none); the suggested command `Y` copies, after `$`, broken at spaces rather than cut; and "→ lists its keys". Without sizes it says "no sizes (MEMORY USAGE not allowed)".
- The bottom line says what the rows come from: "from 10,000 sampled keys · v more" at 80 columns, a longer sentence on wider terminals.
- **Narrowed to a node.** On a cluster, `enter` on [Analyze › Nodes](#analyze--nodes) shows that node's own patterns (estimated for that node alone) instead of the merged ones, from the top row. The title reads `Patterns · node <address>` and the right side starts with `esc for all nodes`. Sort, filter, folding, copy and the right pane work as usual, except that there is no Δ column and no context strip (the snapshot diff and the server figures are of the whole cluster), and the right pane has no example keys (they are kept on the merged rows) and says so. `esc` clears a filter first; with no filter it goes back to the merged patterns, keeping the selected pattern selected, and back to the Nodes panel. The narrowing lasts across screens and across a new analysis whose report has the same node (same node ID and address); it ends when partial results of the new analysis come in (they are merged), when the new report lacks the node, and on a new connect. `ctrl-s` saves the whole report. Bulk actions (`t` and `d` here, `T` and `D` on the key list) still run on every primary; their question's first line says so: "On every node, not only <address>: the pattern's keys on every primary."
- **Context strip.** For a finished analysis, the list's bottom border shows the server's exact figures beside the estimates: how much of `MEMORY STATS` `dataset.bytes` the sample's estimated memory explains; the exact key count and the share with a TTL (`INFO keyspace`); the number of big keys from `INFO keysizes` (strings of 1 MiB or more, collections of 65,536 items or more); "no MEMORY STATS" with the reason when it is missing; and "▲ possible sampling bias (keysizes)" when the sample disagrees sharply with `INFO keysizes`. The warning and a missing `MEMORY STATS` come first, so they survive when the strip is cut with `…` on a narrow list; it is absent while partial results show. The sampling-bias warning has two triggers, and neither applies with `MATCH`: the histogram counts strings of 1 MiB or more but no sampled string or mixed-type pattern is that large (`MEMORY USAGE` is never below a string's length); or a type's share of keys differs by 20 points or more between the histogram and the sample (at least 100 keys on each side, mixed-type patterns at most 10% of the sample). `redis-lens scan` prints the same figures as text lines.

### Browse › Keys › a key's value

`→` (or `l`, `enter`) on a key of the [key list](#2-browse--keys-additions) opens its value in the right pane, one level below the key list; the key list stays on the left, narrower (28 of 80 columns, 40 of 120), the open key bright and the others dim, with SIZE and EXPIRES when there is room. With no key selected it says so. `←` (or `h`, `esc`) on the items goes back to the key list. Switching screens and coming back to Browse › Keys shows it again. Opening the same key again keeps what it loaded; another key starts afresh. A new connect closes it. On a cluster it reads the key's slot owner and names that node ([Cluster live views](#cluster-live-views-p4)).

- **Facts** in one line above the value come from the key list's row, so they cost no command: its length (bytes for a string; fields, items, members or entries for the others), size and expiry ("never expires" or "expires in 6d 11h"); then, as room allows, the node on a cluster ("on node 127.0.0.1:7001"), field TTLs (for a hash on a server with `HTTL`: "1 of 3 fields expire" of the loaded fields), the encoding ("stored as listpack") and the LFU counter (under an LFU policy). The title is the key's name, with its type on the right, how many items are loaded of its length while not all are, and with `v` the format and the commands it reads.
- **The value** is read in pages through the [command budget](#shared-command-budget-p1). The first page loads when the screen shows; `m` loads the next. At most 10,000 items are held; past that `m` says so. These are the only commands sent:
  - every first page: `TYPE`, pipelined with that page's commands for the type the key list saw. `none` means the key is gone: "The key no longer exists." The type `TYPE` returns is the one read; it may differ from the key list's;
  - string: `STRLEN` and `GETRANGE key 0 65535`. A longer string shows its first 64 KiB, and the title says of how much;
  - hash: `HSCAN key cursor COUNT 100`, then `HTTL key FIELDS n …` for that page's fields;
  - list: `LRANGE key i i+99`, with each element's index;
  - set: `SSCAN key cursor COUNT 100`;
  - sorted set: `ZRANGE key i i+99 WITHSCORES`, members and scores;
  - stream: `XREVRANGE key + - COUNT 100`, newest first, then `XREVRANGE key (<last id> - COUNT 100`;
  - JSON (`ReJSON-RL`): `JSON.GET key`, only when the key list's size (`MEMORY USAGE`) is known and at most 256 KiB. The document is shown up to 64 KiB, and the panel says when it is cut. Without a known size, or above 256 KiB, it is not read and the panel says why;
  - vector set: `VRANDMEMBER key 100`, 100 random elements. A vector set can't be read in pages, and the panel says so;
  - array (Redis 8.8 and later, `TYPE` `array`): `ARSCAN key i 18446744073709551614 LIMIT 100`, each element with its index; the next page starts after the last index read;
  - the module types below, each read once, in one pipeline after `TYPE`, through the budget;
  - any other type: only the metadata, and "redis-lens does not read values of type <type>".

  | Module type | `TYPE` reply | Commands | Shown |
  |---|---|---|---|
  | time series | `TSDB-TYPE` | `TS.INFO`, `TS.REVRANGE key - + COUNT 500` | the info (retention, labels, rules, total samples, first and last timestamp, also in UTC), a sparkline of the samples from oldest to newest, and the samples newest first in UTC |
  | Bloom | `MBbloom--` (Redis), `bloomfltr` (Valkey bundle) | `BF.INFO` | the info |
  | Cuckoo | `MBbloomCF` | `CF.INFO` | the info |
  | Count-Min Sketch | `CMSk-TYPE` | `CMS.INFO` | the info |
  | Top-K | `TopK-TYPE` | `TOPK.INFO`, `TOPK.LIST key WITHCOUNT` | the items with their counts |
  | t-digest | `TDIS-TYPE` | `TDIGEST.INFO`, `TDIGEST.QUANTILE key 0.5 0.9 0.99`, `TDIGEST.MIN`, `TDIGEST.MAX` | p50, p90, p99, min and max |

  A module type's summary scrolls, and `f` and `y` say why they do nothing there. When its `*.INFO` is refused or unknown, the panel is one line ("Time series not read: the server refused TS.INFO for this user."), never an error screen; any of its other commands that is refused, unknown or fails leaves its part out, with a ▲ note. Edits and deletes are refused there with a reason. `KeyType` is unchanged: the key list shows these keys as `other`, and `TYPE` picks the reader.

  A field or member that `HSCAN` or `SSCAN` returns twice is listed once.
- **Field TTLs** are read only on Redis 7.4 and later and Valkey 9.0 and later, from the version read on connect. On older servers the TTL column and the Field TTL row are hidden and `HTTL` is never sent. When `HTTL` is refused or unknown, field TTLs are off for the rest of the session, with a note.
- **Layout:** a collection lists its items (field, value and TTL for a hash; index and element; member; member and score; entry ID and its fields), each value shortened to one line. Below a divider, the selected item's value is shown in full. Its header gives the name, the format, the size and "auto-detected" or "shown as <format>", and "decompressed from gzip" (or zlib, LZ4, zstd), "truncated at 1 MiB" or the renderer's own note when they apply. A stream entry lists each field name with its value. A string or a JSON document shows just its value.
- **Focus:** `→` (`l`) moves from the items into the value below the divider, and `←` (`h`, `esc`) back to the items; `tab` only switches panels. The move keys move the selection, or scroll the value when it has the focus. A string or a document always scrolls.
- **Formats:** `f` shows the value in the next format (text, hex, JSON, MessagePack, PHP, Pickle, Protobuf, Java, then text again), starting from the detected one, and says so ("Shown as hex."). A format that doesn't fit the bytes falls back, with a note. It holds for every item of the key until another key is opened. Detection is in [Values](#values).
- **Copy:** `y` copies the selected value in the format shown. Text with control characters, a newline included, is not copied: "The value has control characters, so it is not copied: press f for hex, then y." A stream entry has several fields, so it is not copied.
- **Refusals** (rule 3): a read the server refuses is turned off, and the panel says so in redis-lens's own words ("Can't read the value: the server refused HSCAN for this user."), never with the server's text or the username. `m` does not send it again. A refused `STRLEN` only leaves the length out. Any other failure shows its reason, and `m` tries again. `r` does nothing here.
- **Terminal safety:** everything from the server drawn here (values, field names, members, entry IDs, the type name, the key name in the title) shows control characters and escape sequences as escapes (`\x1b`), never raw.

### The Analyze panels

Big keys, Expiry, Changes and Hot keys (and Nodes on a cluster) show the last analysis. Before one they say "No analysis yet.", what the panel needs a sample for, the `a` line with the same [estimate](#analysis) Overview shows (and the Deep figures, for rare patterns and big keys), and that what the server reports without a sample is on Overview. While an analysis runs they show the previous report, marked, or wait for the first one. Each panel is one frame: the list on the left and the selected row's details on the right with a shared divider, except Nodes, whose list takes the full width with the details below it. A `▲` marks a row worth a look, and the details say why in a sentence. Every panel has base columns at 80 columns, more at wider widths, and the rest with `v`.

### Analyze › Expiry

One panel: the patterns ranked by TTL risk on the left, and on the right the selected pattern and the memory the TTLs free across all patterns. It has no deeper level; `→` does nothing here.
- The ranking, riskiest first, says each pattern's status in words:
  - **some never expire**: most keys expire but some don't, which is probably a bug (`▲`, amber);
  - **never expires, growing**: a likely leak (`▲`, red);
  - **never expires, stable**: treated as intentional;
  - **short-lived** and **ok**.
- Columns: PATTERN and NO TTL (the share of keys without a TTL, with the `▲` when STATUS is hidden) at 80 columns; KEYS and STATUS when the list is 56 cells or more; MEMORY with `v`.
- The right pane: the pattern's keys, memory and status; its sentence with the `▲`; EXPIRES IN, its TTL histogram over the buckets none, <1m, <1h, <1d, <7d, <30d and >30d; with `v` the typical TTL and the keys sampled; on a wide pane (44 cells or more) or with `v` the command `Y` copies, after `$`; then "MEMORY FREED BY TTL  all patterns" ("FREED BY TTL, ALL" on a narrow pane): bars of the memory freed within 1 h, 1 d, 7 d and 30 d, cumulative, and the memory that never expires (red when more than 25% of it), each with its size and share. The bars are computed from the sampled `PTTL` and size, scaled like the estimates, and need no command. Without sizes the pane says there are no memory figures instead.
- Overview shows the forecast in one line, "FREED BY TTL" (within 1 h, 1 d and 7 d, and never), after an analysis when its frame is 100 cells wide or more ([1 Overview](#1-overview)). There is no other forecast screen.

### Analyze › Changes
- Compares the current analysis with the newest earlier snapshot of the same profile taken with the same `MATCH` glob. Comparing with a chosen snapshot is CLI-only, through `redis-lens diff --against`. For each pattern it shows the change and the change as a percentage at 80 columns; NOW (after), WHY (the cause, below) and TREND join on wider terminals; BEFORE and a bar with `v` where they fit. A `▲` marks a new pattern or growth of at least 300M; the right pane says why in a sentence.
- It also says whether the change came from the key count or the average size: more keys, fewer keys, bigger values or smaller values. Memory is keys × average size, so the cause is whichever of `ln(keys after / keys before)` and `ln(average after / average before)` is larger in magnitude, and its direction always follows the change in memory. A change under 1% or under 64 KiB is "unchanged".
- New and gone patterns are flagged when both scans saw the whole keyspace or used the same `MATCH` glob. Otherwise a pattern found in only one scan is shown as "not in the other scan", and it never counts as growth on Analyze › Expiry. When either scan was partial (`complete` false), a `New` pattern counts as growing on Analyze › Expiry only if it passes a threshold; `PatternDiff.partial` records this. With sizes that is the usual byte threshold, which for a new pattern is more than 1 MiB. Without sizes there are no keys before, so 5% of them would let any key through: the pattern needs at least 50 sampled keys and at least 1% of the newer scan's sampled keys. Both sides are sampled counts (`PatternDiff.pattern_sampled_keys_after` and `PatternDiff.sampled_keys_after`, not part of the JSON output), because estimated keys are scaled up and a handful of sampled keys could pass a sampled floor.
- When at least two earlier snapshots with the same `MATCH` glob exist, a TREND column shows each pattern's memory (keys without sizes) as a sparkline over up to 7 earlier snapshots and the current scan, oldest first; `·` marks a snapshot without the pattern. The right pane adds the least-squares growth per day, as a percentage of the mean and absolute, and how many of the scans were partial. With one earlier snapshot there is no trend column. Trend results of a replaced run are ignored exactly like its history.

### Analyze › Hot keys
- Up to 50 sampled keys with an LFU counter (`OBJECT FREQ`) above 0, ranked by that counter. KEY, FREQ and SIZE at 80 columns; HEAT and TYPE on wider terminals; PATTERN with `v`. On a cluster they are the top 50 across the cluster, and on wider terminals (or with `v`) a NODE column shows the address of the node each key was sampled on (cut with `…` to 15 cells); the right pane names it in full. The right pane gives "Reads N of 255", the size, the expiry ("never expires"), the pattern and the node.
- Keys with a counter of at least 100 and a size of at least 10 MiB are hot and big: their size carries a `▲` (`▲ 612M`).
- Needs an LFU eviction policy. Without one, the panel says so, and points to 4 Server › Monitor.
- **From MONITOR (P2):** when the analysis has no hot keys (no LFU counters, or `OBJECT FREQ` refused) and a Monitor run has ended in this session, the panel ranks up to 50 keys by how often the last run's lines name them: KEY and COUNT, LINES (the share of the run's lines) on wider terminals, PATTERN with `v`. The title says where the ranking came from, such as `from MONITOR, 30 s, 8,412 lines, at 13:04 UTC`. Key positions come from the key specs the Slow log uses (one cache per session); the commands not in it are asked with `COMMAND INFO` through the command budget when the run ends, at most 64 names a request. Lines whose command's keys can't be placed (`COMMAND INFO` refused, a command the server does not know) are counted and named in the right pane. `y` and `Y` copy as on the LFU ranking. The ranking is never saved: it is not part of the analysis, so neither `ctrl-s` nor a report holds it.

### Analyze › Nodes

A panel of 3 Analyze, after Hot keys, shown only on a cluster.

- One frame: the list on top, the selected node's details below. One row per shard in slot order, for the node analyzed for it: SLOTS (`0-5460`, or `0-99,200,300-5460` for split ranges), NODE (the address it was analyzed at, as the cluster announced it), ROLE (`primary` or `replica`), STATE (`✓ analyzed`, `▲ partial` or `✗ failed`: a glyph and a word), KEYS (its `DBSIZE`) and MEMORY (its `used_memory`); `v` adds SAMPLED, the keys sampled on it. Figures a failed node doesn't have show `—`.
- The title gives the number of shards, how many were read on a replica, and how many failed or are partial (`3 shards · 1 read on a replica · 1 partial`).
- The details show the selected node: its ID, its primary's ID for a replica, every slot range with the slot count, its server and version, keys and memory (used and limit), the sample ("4,000 of the 10,000 analyzed keys, 9 patterns", and whether every key was seen, or that sizes were unavailable), what `enter` does, and its whole note, wrapped. Notes are fixed texts written by redis-lens (why a node failed, was stopped early, or why its shard's replica was not used).
- `enter` narrows Browse › Keys to the selected node (see [2 Browse › Keys](#2-browse--keys)). A failed node has no results: `enter` says so and stays.
- Like Expiry, Changes and Hot keys it waits for the finished analysis, and while another runs it shows the previous report, marked.
- **Header and Connections.** On a cluster the header shows the summary from connecting (`cluster · 3 shards`, `cluster · 3 shards · replicas 2/3`, `cluster · 3 shards · 1 unreachable`, `cluster · seed node only (shards unknown)`) where the seed's address would be. It comes first among the header's optional parts and is cut with `…` rather than dropped. The header's memory and key count are the cluster's merged figures (memory, limit and keys summed over the nodes, from the connect's figures and then the finished report). On the Connections detail the Address row ends `<server> · <summary>`. Both are amber when a node was unreachable, only the seed could be read, or the finished report has a node that is not analyzed in full.

### Keys

#### Everywhere

| Keys | Action |
|---|---|
| `0` | Connections, the home screen |
| `1` `2` `3` `4` `5` | Overview, Browse, Analyze, Server, Pub/Sub; the current area's digit again: its next panel |
| `v` | Show more columns and details, or fewer again |
| `?` | Every key here; type to search for an action |
| `.` | Actions of the selected row, its parent and the screen, with their keys |
| `:` | Open the command line |
| `ctrl-f` | Find keys by name or pattern on the whole server |
| `b` | Switch the database (once connected) |
| `w` | Turn writes on or off: selected or connected profile |
| `ctrl-o` | Open the web dashboard in your browser |
| `q` `ctrl-c` | Quit |

#### Connections

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `enter` | Connect: reads the server's own figures, no keys |
| `+` | Add a connection |
| `/` | Filter the profiles by name or tag |
| `esc` | Clear the filter |
| `r` | Look for Docker containers again |

#### Overview

| Keys | Action |
|---|---|
| `a` | Analyze the keys: Quick (10,000) or Deep (100,000) |
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `enter` | Go to the selected finding's panel |
| `esc` | Stop the analysis and keep what it has sampled |
| `space` | Pause or resume the live figures |
| `<` `>` | Poll the live figures less or more often, 1 to 60 s |
| `[` `]` | Pick the previous or next node (cluster only) |

#### Browse › Keys

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `→` `l` `enter` | Open a pattern's keys, or expand a group and go in |
| `←` `h` | Collapse a group, or go up to it |
| `/` | Filter patterns by name |
| `m` | Before an analysis: read 1,000 more key names |
| `esc` | Clear the filter, or show all nodes again |
| `s` | Sort by memory, keys, avg size, no-TTL % or name |
| `y` | Copy the selected pattern to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `a` | Analyze again (Quick or Deep); old results stay |
| `ctrl-s` | Save a snapshot of the analysis |
| `t` | Set a TTL on every key of the pattern (writes) |
| `d` | Delete every key of the pattern (writes) |

#### Browse › Keys › a pattern's keys

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `→` `l` `enter` | Show the selected key's value |
| `←` `h` `esc` | Clear the filters first, then go back |
| `/` | Filter the loaded keys by name |
| `s` | Sort the loaded keys |
| `ctrl-t` | Show one type of the loaded keys |
| `m` | Load 1,000 more keys |
| `y` | Copy the selected key name to the clipboard |
| `Y` | Copy the pattern of the selected key |
| `+` | Add a key (writes) |
| `t` | Set or remove the selected key's TTL (writes) |
| `d` | Delete the selected key (writes) |
| `R` | Rename the selected key, never over another (writes) |
| `T` | Set a TTL on every key of the pattern (writes) |
| `D` | Delete every key of the pattern (writes) |

#### Browse › Keys › a key's value

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection, or scroll the value |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `→` `l` | Move into the value pane |
| `←` `h` `esc` | Back to the items, then to the key list |
| `f` | Show the value as text, hex, JSON, MessagePack, PHP, Pickle, Protobuf or Java |
| `m` | Load the next 100 items |
| `y` | Copy the selected value as shown to the clipboard |
| `Y` | Copy the key name to the clipboard |
| `e` | Edit the selected field, element or value (writes) |
| `+` | Add a field, element or member (writes) |
| `t` | Set or remove the selected hash field's TTL (writes) |
| `T` | Set or remove the key's TTL (writes) |
| `R` | Rename the key, never over another (writes) |
| `d` | Delete the selected field, element or member (writes) |
| `D` | Delete the key (writes) |

#### Browse › Streams

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection, or scroll the value |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `→` `l` | Into the right pane: the details or an entry's value |
| `←` `h` | Back to the left pane, or close the entries or result |
| `enter` | List the selected stream's entries |
| `m` | Load 100 more entries |
| `f` | Show the value as text, hex, JSON, MessagePack, PHP, Pickle, Protobuf or Java |
| `y` | Copy the selected value as shown to the clipboard |
| `esc` | Close the entries, or back to the list |
| `space` | Pause or resume polling |
| `r` | Refresh this panel or the entries |
| `a` | Before an analysis: analyze the keys; the list of streams comes from it |
| `c` | Claim pending entries for another consumer (writes) |
| `A` | Acknowledge the selected pending entry (writes) |
| `-` | Trim the stream (writes) |

#### Browse › Search

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection, or scroll the text |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `→` `l` | Into the right pane: the details or a result's value |
| `←` `h` | Back to the left pane, or close the entries or result |
| `/` | Run a query on the selected index |
| `enter` | Open the selected result |
| `x` | Explain the query |
| `X` | Profile the query |
| `f` | Show the value as text, hex, JSON, MessagePack, PHP, Pickle, Protobuf or Java |
| `y` | Copy the selected value as shown to the clipboard |
| `esc` | Back to the indexes and results |
| `r` | Refresh this panel now |
| `+` | Create an index (writes) |
| `d` | Drop the selected index, its documents stay (writes) |
| `[` `]` | Pick the previous or next node (cluster only) |

#### Analyze › Big keys

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `y` | Copy the selected key name to the clipboard |
| `a` | Analyze again (Quick or Deep); old results stay |
| `ctrl-s` | Save a snapshot of the analysis |

#### Analyze › Expiry

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `y` | Copy the selected pattern to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `a` | Analyze again (Quick or Deep); old results stay |
| `ctrl-s` | Save a snapshot of the analysis |

#### Analyze › Changes

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `y` | Copy the selected pattern to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `a` | Analyze again (Quick or Deep); old results stay |
| `ctrl-s` | Save a snapshot of the analysis |

#### Analyze › Hot keys

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `y` | Copy the selected key name to the clipboard |
| `Y` | Copy a redis-cli --scan command for the pattern |
| `a` | Analyze again (Quick or Deep); old results stay |
| `ctrl-s` | Save a snapshot of the analysis |

#### Analyze › Nodes

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `enter` | Only this node's patterns in Browse › Keys |
| `a` | Analyze again (Quick or Deep); old results stay |
| `ctrl-s` | Save a snapshot of the analysis |

#### Analyze › Slots

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move through the hottest slots |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `s` | Sort the hottest slots by keys, CPU, net in or out |
| `r` | Read the slot stats again on every node |

#### Server › Clients

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `enter` | List the selected group's connections |
| `esc` | Back to the groups |
| `space` | Pause or resume polling |
| `r` | Refresh this panel now |
| `d` | End the selected connection (writes) |
| `[` `]` | Pick the previous or next node (cluster only) |

#### Server › Slow log

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `space` | Pause or resume polling |
| `r` | Refresh this panel now |
| `y` | Copy the selected command line, masked |
| `e` | Change the slow log threshold or length (writes) |
| `D` | Reset the slow log (writes) |
| `[` `]` | Pick the previous or next node (cluster only) |

#### Server › Monitor

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Scroll the lines |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `tab` `shift-tab` | Next or previous panel, nothing else |
| `enter` | Start MONITOR (asks; stops at 30 s or 10,000 lines) |
| `esc` | Stop MONITOR now |
| `y` | Copy the selected command line, masked |
| `[` `]` | Pick the previous or next node (cluster only) |

#### Pub/Sub

| Keys | Action |
|---|---|
| `↑↓` `j` `k` | Move the selection |
| `PgUp` `PgDn` | Move one page |
| `Home` `End` `g` `G` | Jump to the first or last row |
| `enter` | Subscribe to the selected channel |
| `+` | Subscribe to a channel or pattern |
| `S` | Subscribe to a shard channel (SSUBSCRIBE) |
| `space` | Pause or resume the tail; without a tail, the counts |
| `ctrl-l` | Clear the tail |
| `/` | Filter the tail |
| `esc` | Clear the filter, or stop the tail |
| `r` | Refresh this panel now |
| `P` | Publish; SPUBLISH on shard channels (writes) |
| `[` `]` | Pick the previous or next node (cluster only) |

#### Command line panel · keys

| Keys | Action |
|---|---|
| `enter` | Run the command (writes go through the write gate) |
| `↑↓` | Step through this session's commands |
| `tab` | Complete a command, subcommand or loaded key name |
| `PgUp` `PgDn` | Scroll the output |
| `ctrl-l` | Clear the output |
| `ctrl-u` | Clear the input line |
| `ctrl-t` | Draw replies as text, a table, JSON or raw |
| `ctrl-p` | Pipeline mode: a repeat goes in pipelines |
| `esc` | Close the command line, stop a repeat, or cancel a question |

The footer shows 3 to 5 of these keys for the screen on show, chosen by its state (before and after an analysis, while one runs, with writes on), then `. Actions   ctrl-f Find key   ? Keys` against the right edge (`. Actions   ? Keys` before connecting). `:` is not in the footer; `?` finds it. When they do not all fit, `ctrl-f Find key` is left out first, then `. Actions`, then the screen's keys from the end, but never the way back (`esc`): it goes last of all; `? Keys` stays. At 80 columns that leaves out at most one key of a screen; from 100 columns nothing is left out. The footer shows only keys that do something: no write key on a read-only profile, nor one the gate would refuse because the server already refused its command (`d End` on Clients after a refused `CLIENT KILL`); no `m More rows` once a list is read to the end (the key list shows `y Copy` there instead); on an empty list only the keys that need no row (the key list: `m`, `+` and `esc`; Streams after an analysis that found none: `a` and `tab`; Search: `+`, `r` and `tab`; Pub/Sub: `+`, `space` and `r`; Clients and Slow log: `tab`, `space` and `r`). A view that is off shows only `tab` and `0` (`0` alone on a screen without panels); a drill-down shows `esc Back`. On the value of a key redis-lens does not read, or a module type's summary, the footer is `↑↓ Move` and `esc Back`, with `D Delete key` between them when writes are on: there is nothing to format or copy there, and `d` deletes no item. Every other key is in the `?` help ([Help](#help)). A notice or an input line replaces the footer while shown; while the command line is open, its input line does; while a dialog is open the footer says what it wants; while the [confirm sheet](#the-confirm-sheet) is open it shows the sheet's answer keys, and while the [Actions menu](#actions-menu) is open `↑↓ Pick   enter Run   esc Close` and, dim, "or press the action's own key". A test checks that this table is exactly what `keymap::markdown()` renders.

### Help

`?` opens the help over the dimmed screen, titled "Help · <screen>", with a search line at the top ("type to find any action or screen…").
- With nothing typed it lists HERE, the keys of the screen it was opened on, then EVERYWHERE, the keys that work on every screen.
- Typing searches every screen and every action of every screen, ignoring case; every typed word must match. Screens come first ("Go to Server › Slow log"), then actions in screen order, each with where it is on the right (`e on Slow log`, `v everywhere`), then the keys that work everywhere. An action shared by several screens is listed once. The move keys are left out, and Nodes and Slots only show on a cluster.
- `↑↓` pick a line, Backspace edits, `esc` clears the search and, with none, closes; `?` with nothing typed closes too. Every other key is typed into the search, `q` included; ctrl-c still quits.
- `enter` runs the picked line as if its key had been pressed on its screen: it goes to that screen first (to the key list or a key's value only when one is open), then presses the key there, through every check that key meets. A write still goes through the [write gate](#write-permissions) and its question. When the screen can't be reached (no key open, or Nodes off a cluster) nothing is pressed. A screen's line just goes there.
- On small terminals the help shrinks to the screen and cuts text with `…`. In the add flow, the filter inputs, the Search query input and the Pub/Sub inputs (`+`, `S` and `/`) every key is text, `?`, `y`, `Y`, `a` and `r` included.

### Actions menu

`.` opens the actions of the focused pane, on every screen, built from the key table above (the table is its only source: each line is a row of it). The menu reads nothing from the server.
- **Groups**, in this order, each under a heading naming what it acts on: the selected row's actions (CONNECTION, FINDING, PATTERN, KEY, FIELD, ELEMENT, MEMBER, STREAM, GROUP, CONSUMER, ENTRY, INDEX, RESULT, NODE, CLIENT, LINE, CHANNEL…, with the row's name), then its parent's (the key of a selected field, the pattern of a key list, the stream of a selected group or entry, the index of an open result), then the screen's actions that are not navigation (`a`, `r`, `ctrl-s`, `v` and the like). Each screen's `enter` (connect, open, list, go there) is the selected row's first action. On a value `R` renames the key, so it is under KEY; on a key list it is the key's own. Search's `f` and `y` act on an open result's fields: under RESULT while one is open, off and under the screen otherwise. Streams' `c` and `A` act on the selected group, consumer or pending entry, which heads the group; `A` reads "Pending entries" on a group or consumer and "Ack" on a pending entry; with the entries open the heading is the selected entry and the stream's writes sit under STREAM. On Connections the selected connection's group holds `enter Connect` and `w Writes`, and the screen's `+ Add`, `/ Filter` and `r`. Moving, panels, going back and the keys that work everywhere (`0`–`5`, `?`, `:`, `ctrl-f`, `b`, `q`, and `w` but on Connections) are not listed. An empty group is left out; a row heading with no selection says "nothing selected".
- **Each line:** the `›` gutter, the key as a key cap, the action's name (`v` reads "More columns"), and at the right one of: the name of the command a write sends now without asking (`HDEL`); what a form sends when its key opens one (`HSET`, `EXPIRE / PERSIST`, `HEXPIRE / HPERSIST`; "opens a form" for a new key, whose command depends on the type picked); `asks first` (amber) when the [write gate](#write-permissions), a bulk action or Monitor asks first (a form whose submit asks shows it too); for `f` the format it switches to (`→ MessagePack`); or `off: <reason>` when pressing the key would do nothing. The reason is the one the key's own handler gives, shortened (`read-only`, `the server refused HDEL`, `nothing selected`, `no analysis yet`…; a command or key name keeps its case). An off line is dim and is not run: `enter` on it shows its full reason as a notice. When a line is short the action's name is cut first; the reason is cut with `…` only when it alone is wider than the line. A test checks every line against what running it does: a command shown is the command sent, with no question; `asks first` opens a question and sends nothing; a form sends nothing; `off:` sends, opens and changes nothing.
- **Keys:** `↑↓` `j` `k` (`Home` `End`) pick a line, `enter` runs it, the line's own key runs it, `esc` or `.` close. Running a line closes the menu and presses its key on the screen behind, through the same handler and checks: a write still meets the write gate and its [confirm sheet](#the-confirm-sheet). Every other key is ignored while it is open.
- **What closes it:** besides `esc`, `.` and running a line, the menu closes when what it was opened on changes under it: the screen (a reply that goes back to the key list after a delete), the focused pane or level (a drill-down, a stream's entries or a Search result opening or closing), or the selected row of the focused list (a poll or reload that removes it or selects another; one that only moves it keeps the menu). It then says "The screen changed, so the Actions menu closed." after what the change itself said. A line is never run on anything but what the menu was opened on. A question opening closes it too (see [The confirm sheet](#the-confirm-sheet)).
- **Box:** at most 58 columns wide, its border in the focus colour, titled `Actions` with `<profile> · ENV · read-only` (or `writes on`) at the right. The screen is not dimmed. It goes to the bottom-right of the body when that leaves the selected rows (and the start of their names) in view, else to the top right, bottom left or top left; when no corner does, it sits below (or above) the selected rows, shorter, and scrolls; when there is no room for that either it stays at the bottom right and its headings name the rows. It fits 80×24; when its lines do not fit, it scrolls with the selection and its last row says `↓ N more`.
- In the add flow, a filter or query input, a form, the command line, the Find key prompt and the `?` help, `.` is text.

### Analyzing again

`a` on Overview, Browse › Keys and every Analyze panel but Slots asks "Analyze the keys?" ([Analysis](#analysis)), also when an analysis has finished, been stopped or failed; `enter` there starts a new run on the shown connection, the same way `enter` connects it (same rate limit and read-only rules; the connection's current profile, database and credential source), then analyzes. `a` on Browse › Streams before any analysis asks it too. While a connect is running `a` says "Still connecting. Press a again once connected."; while an analysis is running "An analysis is already running. esc on 1 Overview stops it and keeps what it has."; with no connection yet it says to connect first. `r` only refreshes, and does nothing elsewhere: on Connections it looks for Docker containers again; on Server, Pub/Sub, Search and Slots, and on Streams once an analysis has found streams, it fetches the panel on screen again; with a stream's entries open it reads them again from the newest, and on Search it also asks again for query commands that were refused. A pattern's keys and a key's value have no `a`: go back to the patterns with `←` to analyze again.

A new analysis keeps the area and panel on screen, the Browse › Keys filter, collapsed groups, sort, selected row and node narrowing (see [2 Browse › Keys](#2-browse--keys)), and the Expiry, Changes, Hot keys and Nodes selections (moved to the last row if the new results are shorter). It connects again (on a cluster it discovers its shards again), which re-reads the permission check and the six figures once ([What a connect reads](#what-a-connect-reads)): the scan starts from those figures and does not send them again. It does not read the slow log and client list. The previous results stay on every screen, marked `analyzing · previous results` at the start of the panel title, until the new run's first partial results arrive. From then on Browse › Keys shows the partial results (the selection follows its row) and Expiry, Changes and Hot keys wait for the finished analysis. If the analysis fails before any results of its own, the error is shown and the previous results stay, marked `analysis failed · previous results`. If it fails after partial results it behaves like any failed analysis. While it runs, `ctrl-s` says to wait for it to finish; it never saves partial results and does not save the previous results again. Results, partials and progress of the replaced run are ignored. The one exception is the replaced run's snapshot lookup, which still fills in Changes for its own report while that report is on screen. A slow keychain read meanwhile shows the keychain hint in the header.

### Copy

`y` copies the selected row's pattern: a group row copies its label, such as `user:{id}:*`; on Hot keys it copies the key name. `Y` copies the suggested command `redis-cli --scan --pattern "<glob>"` for that pattern (on Hot keys, the key's pattern), the same text as the `$` line in the right pane. Key names are chosen by whoever writes keys, and no one quoting style is safe in every shell (POSIX shells, fish, PowerShell, cmd.exe), so the command is only offered when every character of the glob is an ASCII letter or digit or one of `_ : . / * ? { } [ ] @ # + = , ~ - |`, none of which is special inside double quotes in any of them. For any other pattern the right pane's command line says there is no command (without the `$ ` prompt) and `Y` says why; `y` still copies the pattern. `y` never copies text with control characters (a pasted newline would run what follows it) and says so. Only on Browse › Keys, Expiry, Changes and Hot keys, on Big keys and a pattern's keys, where `y` copies the key name, and on a key's value, a stream's entries and a Search result, where it copies the selected value ([Values](#values)); never on Connections, Overview or Nodes, or in the add flow or filter input. With nothing selected it says so. On a pattern's keys `Y` copies the pattern instead (on Find results, the pattern the selected key belongs to), and on a key's value it copies the key name.

On Server › Slow log and Server › Monitor, `y` copies the selected entry's or line's command line, credentials masked, exactly as the panel shows it, never the raw arguments. The "… N more arguments" note is not copied. A line with a control character is not copied, as above. `Y` does nothing there. The notice names a "command line".

Copying uses the OSC 52 terminal sequence (no clipboard library), written to the terminal between frames, so it also works over SSH. Terminals without OSC 52 (macOS Terminal.app, for one) ignore it, and the terminal never confirms a copy, so the notice says "Copied <pattern|key|command> to the clipboard (needs a terminal with OSC 52)". Text whose sequence would be over 100,000 bytes is not sent ("too long to copy"). A write error is shown as an error notice. Only a pattern name, a key name, that command, a masked command line from Slow log or Monitor, or a value as shown in a key's value pane can be copied (the `CopyText` type has no other constructor), so credentials, connection strings and profiles never reach the clipboard.

### Values

- **One renderer** draws every value. Each value's format is detected, in this order: Java serialization (`AC ED 00 05`), Pickle (protocol 2 to 5, exactly one complete pickle), JSON, PHP `serialize()` (exactly one array or object), MessagePack, UTF-8 text, otherwise hex. Protobuf and Pickle protocols 0 and 1 are never auto-detected: pick them with `f`.
- **Decompression:** gzip, zlib, LZ4 frame, zstd and Snappy are decompressed first, up to 1 MiB, and the panel says so. Framed Snappy is found by its stream identifier and its CRC-32C checksums are checked. Raw Snappy has no magic, so it is tried only on bytes that would show as hex, and kept only when it decodes validly throughout and the result is longer than the input or a recognised format.
- **Structured formats:** PHP, Pickle, Protobuf and Java draw as an indented tree, read by bounded readers made for untrusted bytes. The caps are 10,000 lines, 1 MiB of text, 1 KiB for any one string and 32 levels of nesting; a cut tree says so on its last line.
  - Pickle is only read, never run. The stack and the memo hold at most 65,536 entries each, and a pickle holds at most 200,000 objects. A global, a call (`REDUCE`, `NEWOBJ`, `INST`, `OBJ`) and `BUILD` show as `module.name(args)` text.
  - Protobuf shows field numbers and wire types. A length-delimited field shows as a string when it is printable UTF-8, else as a nested message when it parses fully, else as a string or bytes.
  - Java shows class names, field names and primitive values. What is not understood shows as a hex dump from there.
  - Copy takes the tree on one line, children in `{…}`, control characters escaped.
- **On screen only:** key values, stream entries on [Browse › Streams](#browse--streams), Search results ([Browse › Search](#browse--search)) and Pub/Sub messages ([5 Pub/Sub](#5-pubsub)) are shown on screen. They are never written to snapshots, reports, logs, error text or `Debug` output: a value is held in a type whose `Debug` shows only its length.
- **Clipboard:** a value reaches the clipboard only through `y` on that value in [a key's value](#browse--keys--a-keys-value), on a stream's entries or on a Search result, in the format shown. The [Copy](#copy) rule on control characters applies; switch to hex (`f`) to copy such a value.
- **Code:** `CopyText` has a value constructor, `CopyText::value`, and a crate-only `CopyText::masked_line` for Slow log and Monitor lines, which masks nothing itself: its callers pass text already masked by `command::masked_line`.
- **Terminal safety:** control characters and escape sequences in a value are drawn as escapes (`\x1b`), never sent to the terminal raw. On Browse › Streams and Browse › Search so is every other text from the server: keys, index, group and consumer names, entry IDs, and the explain and profile text. On 5 Pub/Sub so are channel names and patterns. On Browse › Keys so are pattern names, which come from key names.

## Scanning

A scan is what `redis-lens scan` (and `diff`, `compare`, `--check`) runs, and what the TUI runs when you analyze ([Analysis](#analysis)). The TUI never scans on connect.

1. Read the server's figures through the same function a TUI connect uses (`scan::figures::server_figures_with_budget`), each command taken from the command budget before it is sent: `INFO server`, `INFO memory` and `DBSIZE`, then the server context, three read-only commands:
   - `INFO keyspace`: the scanned database's `dbN` line (keys, expires, avg_ttl, and `subexpiry` where the server reports it: keys with field-level TTLs, Redis 7.4 and later);
   - `INFO keysizes`: used when the reply has the section, detected by its presence, never by version (Redis 8 and later have it; Valkey and older Redis answer with nothing);
   - `MEMORY STATS`: unless the permission check denied it (`dataset.bytes`, `overhead.total`, `keys.count`, `keys.bytes-per-key`, `fragmentation`, read tolerantly).

   A part that fails, is denied or is missing is left out with a fixed reason, never the server's text, and never fails the scan. A failed `INFO server`, `INFO memory` or `DBSIZE` fails it.
2. Walk the keyspace with `SCAN cursor COUNT n [MATCH glob]`. **Never `KEYS`.** Deduplicate the keys SCAN returns.
3. For each batch, pipeline `TYPE`, `PTTL`, `MEMORY USAGE key SAMPLES 5` (when allowed) and `OBJECT FREQ` (when allowed and the policy is LFU).
4. Stop at the sample limit (100,000 for `scan` by default; the TUI's analyses use 10,000 or 100,000) or when the cursor returns to 0.
5. Stay under a command budget: 5,000 commands per second by default, 1,000 on ElastiCache Serverless profiles (AWS warns that frequent or large `SCAN`s there raise latency), none for `local` profiles. `--max-rate N` overrides it (`0` means no limit).
6. Scale sampled counts to estimates with `DBSIZE / keys present`, where keys present counts every distinct key SCAN returned that still existed when it was looked up: the samples plus keys that exist but could not be read (for example `NOPERM` from a key ACL, or names that are not UTF-8). Keys that vanished between SCAN and the lookup (`TYPE` `none`, `PTTL` -2) are left out, so short-lived keys don't shrink the estimates. The scale is never below 1, and it is 1 when the SCAN cursor returned to 0 (the whole keyspace was seen), when `MATCH` is used, or when no key seen still existed.

A per-key error such as `NOPERM` drops that field (sizes, frequency) and never aborts the scan.

A scan can be **stopped early** (`ScanOptions::stop`, a shared flag; the TUI's `esc`). It looks at the flag before each `SCAN` and before each pipelined batch, so it stops within one batch (a batch already sent is read to the end), and returns the report built from what it sampled so far: `complete` false, estimates scaled as usual by `DBSIZE / keys present`, the server context kept. On a cluster a node that had finished stays `scanned`, and a node cut short or not started yet is `partial` with the note "the analysis was stopped before this node was done; its results are partial". A flag set before the scan starts still reads the figures, then samples nothing. The CLI never sets it, so `scan` and its output are unchanged.

### Analysis

The TUI reads keys only on request: `a` on Overview, Browse › Keys and the Analyze panels opens "Analyze the keys?" over the dimmed screen, titled with the profile and its environment. It says sampling reads a few commands per key and writes nothing, and offers two depths, each with its keys, commands and time:
- **Quick:** 10,000 keys, the default, selected when the dialog opens;
- **Deep:** 100,000 keys, which finds rare patterns and rare big keys.

`↑↓` (`j` `k`) pick, `enter` starts, `esc` cancels; every other key is ignored while it is open. The dialog ends by saying both stay under the profile's limit of N commands/s (or that the profile has none) and that `esc` stops either one and keeps what it has. There is no "whole keyspace" depth in the TUI.

- **The estimate** (`scan::estimate`), shown in the dialog, on Overview and on every "No analysis yet" panel: keys = min(sample, `DBSIZE`); commands = the 6 figure commands + one `SCAN` per 1,000 keys (at least one) + 2 per key (`TYPE`, `PTTL`), plus 1 with `MEMORY USAGE` allowed and 1 with `OBJECT FREQ` allowed under an LFU policy (the choices the scan makes); time = commands / the profile's `max_commands_per_sec`, rounded to whole seconds ("about 6 s", "about 2 min"), or "as fast as the server answers" without a limit. The time is a lower bound: round trips come on top. On a cluster the sample is split across the shards by `DBSIZE` as the scan splits it (a node whose figures did not come in is sent nothing and costs nothing), each node has the full budget and up to 4 run at once, so the time is when the last node would finish, and the dialog says "across N shards". For example 7.92M keys with sizes, no LFU, at 5,000 commands/s: Quick is 30,016 commands, about 6 s.
- **Running it.** `enter` starts a new run of the session's connection (it connects again and re-reads the permission check and the figures, once, see [Analyzing again](#analyzing-again)), then runs the scan above from those figures with `ScanOptions::sample_limit` set to 10,000 or 100,000, through the same command budget, which the live views share. The header shows `⠋ analyzing 41%`, Overview shows its progress, and Browse and Analyze fill in as it goes.
- **Stopping it.** `esc` stops a running analysis on any screen where `esc` has nothing else to do: Overview, Big keys, Expiry, Changes, Hot keys, Nodes, Slots and Slow log; Browse › Keys with no filter and no node narrowing; Clients with no group open. Elsewhere `esc` keeps its own meaning. The header says `stopping…` until the scan answers. Its partial report becomes the current results, marked: the header says `● analyzed 14:02 UTC (stopped)`, Overview's right title adds "· stopped early", and THINGS TO LOOK AT says the findings come from that many keys only. One that had already sampled its whole target is not marked. One stopped before its first batch keeps the results on screen as they were: "Analysis stopped before it sampled any key. Nothing changed." A stopped, partial report is not saved: `ctrl-s` says "This analysis was stopped early, so its results are partial and are not saved. Press a to analyze again."
- **The CLI** keeps its own default: `redis-lens scan` samples 100,000 keys unless `--sample` says otherwise, and its output is unchanged.

### Cluster

**Force standalone (P5).** A profile with `force_standalone = true` reads the server at its endpoint alone, even when it is a cluster node, as Redis Insight's "force standalone" does. Nothing below applies to it:
- no `CLUSTER SHARDS` or `CLUSTER SLOTS`, no replica step (even with `prefer_replica`), no slot routing, and no Nodes or Slots panels. Keys on other nodes are not seen.
- A `MOVED` or `ASK` is never followed, and no other node is connected to. The key read, the command line and writes answer "this key lives on another node; the profile is set to standalone". A bulk run still counts the keys that moved.
- A cluster-mode server read this way shows a `standalone` mark in the header and "cluster node read as standalone" in the Connections details.
- Its scan report has no nodes table and carries `note`: "only one node of a cluster was read (the profile is set to standalone); keys on other nodes are not counted". The note is in the text table, the Markdown and HTML reports and saved snapshots. The snapshot field is optional, absent when there is none, so older files load.
- The write gate and `read_only` are unchanged. `redis-lens expire` still needs a discovered cluster, so it does not run on such a profile when the server is a cluster node.

When the profile's server is a cluster node (`INFO server` says `redis_mode:cluster`, or `server_mode:cluster` on Valkey 8; `INFO cluster` saying `cluster_enabled:1` when neither field is there), redis-lens scans every shard and merges the results.

1. **Discovery.**
   - The seed (the profile's endpoint) is connected to once, whatever it turns out to be. Telling a cluster apart costs no extra command: the `INFO server` reply read for it answers the scan's own first `INFO server` on that connection, if the scan starts within 10 s. A standalone server with `prefer_replica` runs its replica step on that same connection.
   - redis-lens reads `CLUSTER SHARDS` on the seed. If that fails (Redis 6.2, an unknown command, `NOPERM`) or can't be read, it reads `CLUSTER SLOTS`. If neither can be read, only the seed is scanned, with a note saying the shards could not be read; that scan is never complete.
   - A shard with no slots, or no primary, is skipped. Shards are listed in slot order.
   - A cluster only has database 0: a profile with another database is refused with an error before any other node is connected to.
2. **Addresses.**
   - Each node is reached at its announced `endpoint`, else its `ip`; a `hostname` is used only where `CLUSTER SHARDS` reports it as the endpoint.
   - The port is `tls-port` for TLS profiles (or the only port listed) and `port` otherwise.
   - `?`, an empty or unspecified address (`0.0.0.0`, `::`), a host whose last label is all digits or `0x` hex (shorthand address forms such as `127.1`, `0`, `2130706433` or `0x7f000001`, which a resolver may read as an address) and port 0 are refused with a note.
   - Each node's address is resolved once (or, behind SSH, forwarded once) and every connection to it uses those addresses; TLS checks the announced name.
3. **Connecting.**
   - Up to 4 nodes are connected at once, each within 5 s. Behind SSH, the forward is started first, outside that timeout.
   - The seed's connection is reused for its own shard.
   - Where `CLUSTER MYID` is allowed, a node must report the ID the cluster announced; one that reports another is not scanned.
   - A node that is refused, unreachable or times out is left out with a fixed note, and the other shards are still scanned.
4. **Replicas.** With `prefer_replica`, each shard tries its online replicas: `health: online` in `CLUSTER SHARDS`, or every replica in `CLUSTER SLOTS`.
   - Each one is verified as for a standalone server (see [Safety](#safety)): `role:slave` and `master_replid` equal to its shard primary's, within the 2 s replica timeout.
   - The connection is then put in `READONLY` mode (a cluster replica otherwise redirects every key command to its primary).
   - A shard whose replicas can't be used is scanned on its primary, with a note saying why.
   - The `Target:` line and the TUI header give a summary such as `cluster · 3 shards`, `cluster · 3 shards · replicas 2/3` or `cluster · 3 shards · 1 unreachable`. The TUI's header and Overview show the figures the connect read on every connected node ([What a connect reads](#what-a-connect-reads)), summed as in step 7; the finished report's merged figures replace them.
5. **Sampling.**
   - Each node reads its own figures first (`INFO server`, `INFO memory`, `DBSIZE` and the server context, the six commands of [Scanning](#scanning) step 1), through that node's own budget, before any node is walked. A node that fails them is `failed` with a fixed note.
   - The sample limit is split across the shards in proportion to their `DBSIZE` (largest remainder; a shard with keys gets at least one sample when the limit allows; the total never exceeds the limit).
   - Up to 4 nodes are sampled at once. The command budget applies **per node**: each node is its own server and gets the full budget.
   - `MATCH` applies on every node. A node whose `DBSIZE` is 0 is not walked.
6. **Estimates.**
   - Each node's estimates scale by its own `DBSIZE / keys present`, and the cluster's estimates are their sums.
   - Patterns are inferred once over the keys of every node, so the `{var}` collapse sees them all and a node's rows always add up to the merged rows.
   - The merged rows take their sampled counts, TTL histogram, percentiles and examples from the combined samples.
   - Hot keys are the top 50 across the cluster, each with the node it was found on.
7. **Merged figures.**
   - The report's server line is the first scanned shard's flavor, version and policy, with `used_memory`, `maxmemory` and `DBSIZE` summed over the nodes scanned.
   - The scan is complete only when every shard was scanned completely and the scanned shards cover all 16384 slots.
   - Sizes are available only when every scanned node allowed `MEMORY USAGE`.
   - The server context sums what adds up: keyspace keys, expires and subexpiry; `dataset.bytes`, `overhead.total` and `keys.count`; `INFO keysizes` bins of the same type and unit. `avg_ttl` is weighted by expires, and `keys.bytes-per-key` by `keys.count`. `fragmentation` is left out. A part is merged only when every scanned node has it.
8. **Failures.** A node that fails before sampling is `failed`, and one that fails mid-scan is `partial` and keeps what it sampled; both carry a fixed note. The scan fails only when no node at all could be scanned.

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
| Token command | `conn add <name> --url <url> -- <program> [args…]` saves `auth = "command"` and `token_command = [<program>, <args>…]`. At every connect (each TUI analysis connects again) redis-lens runs the program directly, never through a shell, with standard input closed and standard error discarded, and kills it after 10 s. Once it has exited, its output gets 250 ms to close (a background process it started can keep it open); what it printed by then is the output. Its standard output (at most 64 KiB, minus one trailing newline) is sent as the password, with the username from the keychain when there is one. Covers the IAM tokens of AWS (ElastiCache, MemoryDB), Azure (Entra ID) and Google Cloud (Memorystore) through their CLIs. See [Token command](#token-command). | quick wins |
| AWS ElastiCache IAM, native | Without an external CLI. Until then, use a token command. | later |
| SSH tunnel | `conn add --ssh [USER@]HOST[:PORT]` (an IPv6 bastion as `[ADDRESS]:PORT`), optionally with `--ssh-identity FILE` and `--ssh-known-hosts FILE`, saves `[profile.ssh]` with `host` and, when given, `port`, `identity_file` and `known_hosts_file`. The server, and every replica or cluster node it announces, is reached through its own forward over the bastion with the system `ssh`, never through a shell. Host keys are always checked strictly, key passphrases are never handled, and the SSH username lives in the OS keychain. See [SSH tunnels](#ssh-tunnels). | v0.3 |
| Sentinel | `conn add --sentinel HOST:PORT[,…] --sentinel-master NAME [--sentinel-tls] [--sentinel-user USER] [--sentinel-password-stdin \| --sentinel-ask-password]` saves `[profile.sentinel]`: the primary is asked from the Sentinels at every connect, checked (`role:master`, not a cluster node) and pinned. The Sentinels' own username and password live in the keychain (account `sentinel:profile:<name>`) apart from the data node's, and each set goes only to its own servers. See [Sentinel](#sentinel-p4). | P4 |

### Token command

- `conn add … -- <program> [args…]`: everything after `--` is the command, stored as an array of arguments in the config file. A username in the URL goes to the keychain; without one there is no keychain entry. A password in the URL, `--password-stdin`, `--ask-password` or `--entra` together with a token command is an error.
- **Confirmation:** `conn add` prints the command and asks before saving it, because config files get copied around and the command runs at every connect. `--yes` skips the question. Without a terminal and without `--yes`, it refuses.
- **At connect:** the command runs again for every connect, `conn test` and analysis, so no token outlives its scan, and the TUI needs no refresh. A non-zero exit, a timeout, empty output, output over 64 KiB or output that is not UTF-8 fails the connect with an error that names the program and, for a failed command, its exit status. The error never includes what the command printed: neither its standard output, nor its standard error, which is discarded.

### SSH tunnels

- **Adding:** `conn add <name> --url <url> --ssh [USER@]HOST[:PORT]`. The user is everything before the last `@`. An IPv6 bastion takes brackets to carry a port (`[2001:db8::1]:2222`). `HOST` may be a `Host` alias from `~/.ssh/config`, whose settings (ProxyJump, User, IdentityFile and so on) apply. The host must have no spaces or control characters, no `@` and no leading `-`, and be at most 253 bytes.
  - The user goes to the OS keychain (account `ssh:profile:<name>`), never to the config file, and the output says so. Without a user there is no entry, and ssh picks the user from its own config or the local login.
  - `--ssh-identity` (ssh `-i`, with `IdentitiesOnly=yes`) and `--ssh-known-hosts` (`UserKnownHostsFile`) must name existing files. They are stored as given, and may not contain `%`, `"`, `${` or control characters, which ssh would expand or misread.
  - With `--ssh` and no `--env`, a loopback URL host is guessed as `OTHER`, not `LOCAL`: it is the bastion's loopback.
  - `--force` replaces the SSH entry, or deletes it when the new profile has no SSH user. `conn remove` deletes it too.
  - `conn list` adds an `SSH` column (the bastion's host and port, never the user) when any profile uses one. `conn test` prints `Through SSH bastion <host[:port]>.` On a cluster, `conn test` connects to and checks the seed node only; `scan` checks every node.
- **How:** each endpoint (host and port, as configured or announced) gets one forward, started the first time a connection needs it and reused after that. A forward whose ssh process has died is started again on next use.
  - The command is `ssh -N -o BatchMode=yes -o StrictHostKeyChecking=yes -o ExitOnForwardFailure=yes -o ConnectTimeout=8 -o ServerAliveInterval=15 -o ControlMaster=no -o ControlPath=none -L 127.0.0.1:<local>:<host>:<port> [-p PORT] [-i FILE -o IdentitiesOnly=yes] [-o UserKnownHostsFile=FILE] [-l USER] HOST`. An IPv6 target goes in brackets.
  - ssh runs directly, with standard input and output closed. On Windows it runs without a console window. Connection multiplexing is off, so no master process outlives redis-lens.
  - The local port is picked by binding `127.0.0.1:0`. If ssh can't listen on it, two more ports are tried.
  - A forward is ready when `127.0.0.1:<local>` accepts a TCP connection (ssh opens it only after logging in). If ssh exits first, or the port isn't ready within 10 s, the connect fails and ssh is killed.
  - Connections are pinned to the forward's address. TLS still checks the certificate against the server's own name.
  - Every ssh process is killed when the last connection using it is dropped (`conn test` returns, a new analysis replaces its connection, the TUI disconnects).
  - A cluster behind a bastion works the same way. The seed and every announced node or replica get their own forward, started outside the node and replica timeouts. Node addresses in the report are the announced ones, never a forward's 127.0.0.1. A node the bastion can't connect to is a failed node.
- **Errors:** ssh's standard error is read only to pick one fixed message, and is never shown, logged or kept:
  - the host key is not in known_hosts or does not match ("connect once with ssh to check and add it");
  - the bastion refused the login (check the user and key; an encrypted key must be loaded in ssh-agent);
  - the bastion could not be reached (the name did not resolve, or the connection was refused or timed out);
  - ssh could not listen on the local end;
  - no ssh program;
  - not ready within 10 s;
  - ssh exited for another reason ("run ssh to the bastion yourself to see why").
- **Not covered:**
  - Docker discovery always connects directly.
  - Behind a bastion, loopback is judged by name only (see the trust boundary).
  - A bastion that refuses forwarding (`AllowTcpForwarding no`), or that cannot reach the server, shows up as the server connection closing, not as an SSH error.
  - If redis-lens is killed with no chance to clean up (SIGKILL), its ssh processes are left running.
- **Known exposure:** with `-l <user>`, the SSH username is in ssh's arguments, so other users of the machine can see it in the process list while a tunnel runs. Setting `User` for the bastion in `~/.ssh/config`, and leaving the user out of `--ssh`, avoids it.
- **Known exposure: the local forward port.** The local port is picked free and released just before ssh binds it, after logging in. A local process that binds that port in between receives the readiness connection, and then the server's `AUTH` (username and password). While a tunnel runs, any local user can also connect to its `127.0.0.1` port, as with any `ssh -L`; for a server or replica without auth, that is a way in for them.

### Providers

A profile can name the managed service it is on (`provider` in the config file). `conn add` sets it from `--provider elasticache|elasticache-serverless|memorydb|azure|memorystore`, or from the host name:
- `*.serverless.<region>.cache.amazonaws.com` is ElastiCache Serverless; other `*.cache.amazonaws.com` hosts are ElastiCache;
- `*.memorydb.<region>.amazonaws.com` is MemoryDB (the AWS patterns also match `.amazonaws.com.cn`);
- `*.redis.cache.windows.net` (and its `chinacloudapi.cn` and `usgovcloudapi.net` forms), `*.redisenterprise.cache.azure.net` and `*.redis.azure.net` are Azure.

Memorystore uses private addresses, so only `--provider` sets it. `conn list` shows the provider. It is a hint: the permission check stays authoritative. It adds a one-line notice: in the Connections details before connecting (once connected, the permission check counts), at the bottom of Overview's Keys section in every state, and in `conn add`, `conn test` and `scan` output:
- ElastiCache Serverless: "ElastiCache Serverless restricts MEMORY and OBJECT, so sizes and hot keys are missing; counts and TTLs still work."
- Azure: "Azure Managed Redis and Enterprise tiers don't support MEMORY STATS, so dataset.bytes may be missing."

ElastiCache Serverless profiles also get the lower default command budget (see [Scanning](#scanning)).

### Credential rules

- Usernames and passwords live **only** in the OS keychain: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
- There is one entry per profile, with service `redis-lens` and account `profile:<name>`, plus one more (`ssh:profile:<name>`) for a profile with an SSH user, and one more (`sentinel:profile:<name>`) for a profile whose Sentinels have their own username or password ([Sentinel](#sentinel-p4)).
- The config file never holds a credential, and the `Profile` type has no field for one.
- Credentials never appear in logs, error text, `Debug` output or snapshots.
- A token command's output is a credential. It is held in memory (`SecretString`) for the connect only, and never written to the config file, logs, error text, `Debug` output, snapshots or the screen. For `auth = "command"` the config file holds the command and the keychain holds only the username, when there is one.
- The SSH username of a profile with a bastion is a credential too. It lives only in the keychain (account `ssh:profile:<name>`, as the username with an empty password), and is never written to the config file, logs, error text, `Debug` output, snapshots or the screen. The places it appears are ssh's own argument list while a tunnel runs (see [SSH tunnels](#ssh-tunnels)), and the `conn add --ssh USER@HOST` command line itself, which the shell may keep in its history; `User` in `~/.ssh/config` avoids both. ssh's output is never shown, logged or kept, and key passphrases are never asked for or stored: ssh runs with `BatchMode=yes`, so an encrypted key must be in the SSH agent.
- Keychain reads, writes and deletes give up after 15 s. On macOS the error says macOS is waiting for permission to use the keychain entry and to click "Always Allow" in the dialog, then try again; elsewhere it says the OS keychain did not answer (it may be locked). The error names the entry's account, never its contents.
- A panic inside a keychain call prints nothing (it would land on the TUI) and is reported as "the keychain call failed", never with the panic's message.
- A password typed into a URL on the command line is accepted, with a warning about shell history. `--password-stdin` and `--ask-password` are the preferred ways in.

### Safety

- Profiles with env `prod` are read-only by default (`read_only` unset; `Profile::is_read_only`). Today redis-lens never sends a write command. From the write phase on, a write is sent only when all three [Write permissions](#write-permissions) checks allow it, and never on a read-only profile.
- Scans can prefer replicas. It is opt-in per profile: `conn add --prefer-replica` sets `prefer_replica = true` in the config (absent when false). `conn::connect_for_scan` resolves the profile's credentials once (one keychain read, or one Entra ID token provider) and uses them for every connection it opens. It connects to the primary, reads `INFO replication` and, on `role:master`, takes the first `slaveN:…state=online` entry with a usable address: an IPv4 or IPv6 address other than `0.0.0.0` or `::`, or a host name whose last label is not all digits or `0x` hex (so shorthand address forms such as `127.1` are refused), and a port other than 0. It connects to that candidate with the same credentials and TLS settings (the same trust roots, with the certificate checked against the announced host name) and verifies it (below); connecting and verifying share one 2 s timeout, and reading the keychain happens before it starts. On `role:slave` (or `role:replica`) the profile already points at a replica. The result is a `ScanTarget`: `Primary`, `Replica(endpoint)`, or `PrimaryFallback(reason)` when the replica could not be used or verified (the scan then runs on the primary). `scan` and `diff` print a `Target:` line for a replica or a fallback (`Target:  replica 10.0.1.21:6379`, `Target:  primary (replica unreachable: …)`); the TUI shows it in the header and the Connections detail panel, with the reason passed through the same masking as error messages. Endpoints with an IPv6 address are shown in brackets, `[::1]:6379`. A replica can lag, so counts and sizes may trail the primary slightly.
- **Replica verification.** The address in `INFO replication` is whatever the replica announced, and anything may be listening there. redis-lens only scans a candidate it has checked against this primary (the replication ID is not secret, so a server built to imitate a replica can pass the check; it is then scanned without credentials, and its data is filed under the profile): the candidate's `INFO replication` must say `role:slave` (or `role:replica`) and its `master_replid` must equal the primary's. The steps:
  1. Read the primary's `master_replid`. Without one, nothing can be verified: fall back.
  2. Resolve the candidate's host once (`tokio::net::lookup_host`). Every connection to it uses those addresses, so DNS can't change between the check and the connection; TLS still checks the certificate against the announced name. Behind an SSH bastion the candidate's name is resolved on the bastion: this step starts the candidate's own forward instead, and every connection to it uses that forward.
  3. Ask the candidate for `INFO replication` on a connection that sends no credentials (no `AUTH`, `HELLO` or `CLIENT SETINFO`). If it answers and passes, the scan runs **on that same connection**, without credentials (it then reads as the replica's default user). It is never sent the profile's credentials: `master_replid` is not secret (anyone who can run `INFO`, or `PSYNC`, can learn it), so passing the check proves nothing about who holds the address.
  4. Only a candidate that answers `NOAUTH`, for a profile that has credentials, may get them, and only when it is not on the same machine as whatever reaches it, or the primary is on that machine too. Direct, a host counts as on this machine when its name is loopback or any address it resolves to is loopback or unspecified; a loopback address announced by a remote primary names the machine running redis-lens, not the replica. Behind an SSH bastion, a host counts as on the bastion when its name is loopback (`localhost`, `*.localhost`, `127.0.0.0/8`, `::1`, `::ffff:127.0.0.0/104`, in any case) or unspecified (`0.0.0.0`, `::`); the forward's own 127.0.0.1 never counts. Shorthand address forms that ssh would read as loopback there (`127.1`, `0x7f000001`) never get this far: they are refused as announced addresses.
  5. The authenticated connection is verified again, and it is the one scanned.

  Resolving and every connection and check share the 2 s replica timeout. Behind a bastion, starting the candidate's forward comes first, within ssh's 10 s readiness limit, and the 2 s covers the rest. Any failure drops the candidate's connections and gives `PrimaryFallback("replica at <endpoint> is not a replica of this primary")`, or the reason it could not be checked (a refused loopback address, a host that does not resolve, a connection error, a timeout). A connection or server error is never shown with the server's text, which can name the ACL user: it reads "NOPERM INFO replication is not allowed for this user", "the replica refused the credentials", "the replica refused the connection", "timed out" or "the replica's reply could not be read", after `<endpoint>: `. A failure that never reached the server keeps its own text, which comes from the operating system or the certificate check and says why: a refused connection, an unknown issuer, a certificate for another name. When the profile's own server fails `INFO replication` with anything but a `NOPERM`, the reason is `INFO replication failed (<code>)`, the error's code word only.

  The trust boundary: with `--prefer-replica`, the address the primary announces for its replica is trusted the way Cluster and Sentinel clients trust addresses servers announce. Credentials go only to an announced host that demands auth and is either not loopback or on the same machine as the primary. Direct, loopback is judged on this machine, after DNS resolution. Behind an SSH bastion, it is judged from the bastion by name, since names resolve there. An announced `127.0.0.1` behind a bastion is the bastion itself, so it gets credentials only when the profile's own host is loopback on the bastion too. A name that resolves to loopback only on the bastion is not recognised, and counts as remote. A profile that reaches a server through a tunnel opened by hand on 127.0.0.1 still counts as on this machine; use `--ssh` for a bastion instead. Anyone who can make the primary announce a host they control that demands auth can collect the credentials. That is why `--prefer-replica` is opt-in.
- **Announced cluster nodes.**
  - The addresses in `CLUSTER SHARDS`/`CLUSTER SLOTS` are whatever the nodes announced. They are trusted the way cluster clients trust them: every shard's primary (or verified replica) is connected to with the profile's credentials.
  - The same loopback rule as for replicas applies, judged against the seed: credentials go to an announced node only when it is not on this machine, or the seed is on this machine too. Behind SSH, "on this machine" is judged from the bastion: a cluster on the bastion's own loopback, reached as `127.0.0.1` through the bastion, gets credentials on every node; reached at the bastion's network address, a node announced as `127.0.0.1` gets none, and its shard says why.
  - Each address is resolved (or forwarded) once and pinned, and TLS checks the announced name.
  - Where `CLUSTER MYID` is allowed, a node must report the ID the seed announced for it.
  - Anyone who can make the seed announce a host they control (that is not refused by the loopback rule) can collect the credentials. Pointing redis-lens at a cluster trusts that cluster's view of its own members.
  - Notes about refused or failed nodes are fixed texts and never include a server's error text.
- Profile names starting with `docker:`, in any case (`Docker:`, `DOCKER:`), are reserved for discovered containers: `conn add` refuses them and the TUI add flow's guessed name avoids the prefix.

### Permission check

On every connect (in the TUI, each analysis connects again), before the figures or any key is read, redis-lens probes each command it needs once: `SCAN`, `TYPE`/`PTTL`, `MEMORY USAGE`, `INFO`, `OBJECT FREQ` and `MEMORY STATS`. The views degrade according to the result:
- no `MEMORY USAGE` means counts and TTLs but no sizes
- no LFU policy or no `OBJECT FREQ` means no hot keys
- no `MEMORY STATS` means the server context has no `dataset.bytes` figures, and Overview and the Browse › Keys strip say so
- no `SCAN` means Browse › Keys reads no key names, and says so

The key commands are probed on the first key `SCAN` returns, or on `redis-lens:probe` when the database is empty; for that missing key, `ERR no such key` and a cluster redirect (`MOVED`, `ASK`, from a shard that doesn't own the key's slot) count as allowed, since Redis and Valkey check ACLs before they redirect. `TYPE` and `PTTL` are judged together, and `PTTL` is still probed after a redirected `TYPE`.
A refused check reads "NOPERM <command> is not allowed for this user", never the server's own text, which names the ACL user. A connect checks again and forgets the last connect's results.

A minimal read-only ACL user:

```
ACL SETUSER lens-ro on >… ~* resetchannels -@all +ping +hello +client|setinfo +client|setname +select +info +dbsize +scan +type +ttl +pttl +memory|usage +object|freq +memory|stats +cluster|shards +cluster|slots +cluster|myid +readonly
```

On a cluster, `+cluster|shards` (or `+cluster|slots` on Redis 6.2) is needed to find the shards; without both, only the seed is scanned. `+cluster|myid` lets redis-lens check each node's identity, and `+readonly` is needed to scan cluster replicas with `prefer_replica`. The permission check runs on every connected node, and a command counts as allowed only when every node allows it.

### Connection strings

- `redis://[user[:password]@]host[:port][/db]`, and `rediss://` for TLS. `valkey://` and `valkeys://` are the same URLs under Valkey's name.
- StackExchange.Redis format: `host:port,password=…,user=…,ssl=True,defaultDatabase=N`. Unknown options are ignored.
- Any other URL scheme is refused ("only redis, rediss, valkey and valkeys URLs are supported"), and so is a host that holds a user name, a password or other URL parts (`@`, `/`, `?` and the like): such a string would otherwise be taken as a host and its password shown and saved with it. The refusal never repeats what was typed.
- Azure hosts are recognised: `*.redis.cache.windows.net` is Azure Cache for Redis and `*.redis.azure.net` is Azure Managed Redis. The environment is guessed from the host name.

### Docker discovery

Lists running containers whose image is redis, valkey, redis-stack or bitnami/redis and that publish port 6379. A password found in `--requirepass` or in `REDIS_PASSWORD`/`VALKEY_PASSWORD` is held in memory only, and the auth shows as `password`. Without one, discovery sends a single `PING` to the published port on 127.0.0.1 with no credentials (no `AUTH`, `HELLO` or `CLIENT SETINFO`; 1 s timeout) and shows `none` (PONG), `required` (`NOAUTH` or `WRONGPASS`: the server wants credentials discovery cannot see, such as an ACL file) or `unknown` (refused, timed out, or any other answer). Only `PING` is sent. Containers are probed at the same time, each with its own 1 s timeout, so discovery takes about 1 s at worst however many containers don't answer. Results are sorted by container name.

## Command line

```
redis-lens                         open the TUI
redis-lens conn add <name> --url <url> [--env prod|stage|local|other] [--password-stdin | --ask-password] [--entra] [--prefer-replica] [--provider P] [--ssh [USER@]HOST[:PORT] [--ssh-identity FILE] [--ssh-known-hosts FILE]] [--force] [--yes] [--writes on|off [--confirm NAME]] [--tag TAG]... [--sentinel HOST:PORT[,…] --sentinel-master NAME [--sentinel-tls] [--sentinel-user USER] [--sentinel-password-stdin | --sentinel-ask-password]] [-- TOKEN_COMMAND...]
redis-lens conn set <name> [--writes on|off [--confirm NAME] [--yes]] [--tag TAG]... [--untag TAG]...   (at least one of --writes, --tag, --untag)
redis-lens conn list | remove <name> | test <name>
redis-lens conn export [FILE] [--force]
redis-lens conn import FILE [--force] [--yes]
redis-lens scan <name> [--sample N] [--match GLOB] [--max-rate N] [--save [--keep N] [--keep-days D]] [--json]
                [--check RULE]... [--rules FILE] [--strict]
                [--report md|html|csv [-o FILE] [--examples]]
redis-lens diff <name> [--against FILE] [--max-rate N] [--json]
redis-lens snapshots list <name> [--json]
redis-lens snapshots prune <name> (--keep N | --keep-days D) [--yes]
redis-lens trend <name> [--pattern P] [--last N] [--json]
redis-lens compare <left> <right> [--left-file FILE] [--right-file FILE] [--sample N] [--max-rate N] [--json]
redis-lens expire <name> <pattern> --ttl DURATION [--apply] [--yes | --confirm PATTERN] [--max-rate N]
redis-lens discover
```

**`scan` on a cluster** reads one node per shard (see [Cluster](#cluster)) and prints `Target:  cluster · N shards` (with `· M unreachable` when a shard could not be reached), followed by a Nodes table: one row per shard with its slot ranges (`0-5460`; several ranges comma-joined, after three the rest counted as `+N more`), the node's address, role, status (`scanned`, `partial` or `failed`), its key count (`-` when unreadable), the keys sampled and its memory, and under the table the note of any node that has one. The pattern table below is the merge over all shards. `--json` includes the nodes. Markdown and HTML reports have the same table as a Nodes section; CSV stays one row per pattern. A standalone scan prints no Nodes table or section. On a cluster, `--check` rules judge the merged patterns; when any node was not scanned to the end (status `partial` or `failed`), a line `NOTE not fully scanned: <address> (<status>), ...; rules judge the merged patterns, which may be incomplete` follows the rule lines. It changes no exit code, the same as a partial scan of a standalone server.

**`scan --check`** gates a pipeline on the scan. Each `--check RULE` (and each entry of `rules` in the `[check]` table of a `--rules` TOML file) is `<metric><op><value>[,min-keys=N]` with ops `< <= > >= = !=` and metrics `no-ttl-pct`, `growth`, `pattern-bytes`, `verdict` and `total-bytes`. A rule states what must hold. Per-pattern rules fail every pattern that breaks them and skip patterns with fewer than `min-keys` sampled keys (default 50; `min_keys` in the file changes the default). `growth` compares with the newest earlier snapshot of the profile with the same `MATCH` glob; `verdict` uses the verdicts of the TUI's Analyze › Expiry, and `persistent-growing` and `persistent-stable` need that baseline too, since only history tells them apart. A rule that cannot be judged (no `MEMORY USAGE`, no baseline) is reported as SKIP and does not fail, unless `--strict` is given. The scan itself is a normal one with the profile's command budget; a warning is printed on stderr for production profiles. Exit codes: 0 all rules passed, 1 a rule failed, 2 the scan or a rule could not be run (usage, connection or scan error). `--check` combines with `--json` and `--report`; the rule lines then go to stderr if stdout is taken.

**`scan --report md|html|csv`** writes a shareable report instead of the table, to `-o FILE` or stdout. Markdown and HTML hold the summary (with the server context lines), top patterns, TTL risks and, when a baseline snapshot exists, the changes since it; HTML is one self-contained file with no external requests. CSV has one row per pattern. Example key names are left out unless `--examples` is given. Names come from whoever writes keys, so they are escaped for each format: HTML entities; inline code spans in Markdown, so no link, emphasis or HTML can form; and a leading `'` on CSV cells that start with `= + - @`, tab or CR. Reports contain no credentials or connection string.

**`redis-lens compare <left> <right>`** scans both profiles one after the other, each with its own rate limit and read-only rule, or reads a saved snapshot for a side with `--left-file`/`--right-file`. It writes nothing to either server and saves no snapshot. Per pattern it shows, first, the share of each side's total estimated memory (of keys when either side has no sizes), the key-count ratio and the average-size ratio (right / left), and the share of keys with no TTL on each side; then the absolute keys, memory and average sizes. It shows each side's sample fraction (sampled / total keys) and flags patterns found on one side only. "No TTL on one side, set on the other" (more than half the sampled keys without a TTL on one side, fewer than half on the other) is flagged as a likely bug and listed first. When the sides used different `MATCH` globs, or either side was sampled, it says that a pattern on one side only may not be missing from the other. A side may be a standalone server or a cluster; a cluster is compared on its merged patterns, and its header line shows `(cluster · N shards)`, plus `· M not fully scanned` when a shard was partial or failed. `--json` adds `shards` (and `shards_unfinished` when above 0) to that side's summary.

`--config <path>` or `REDIS_LENS_CONFIG` overrides the config location. The default is the platform config directory, for example `~/Library/Application Support/redis-lens/config.toml` on macOS. The file is TOML with `[[profile]]` tables and is written atomically with mode 0600.

## Snapshots (v0.2)

`ctrl-s` (or `redis-lens scan --save`) writes the current `ScanReport` as JSON to `<data dir>/redis-lens/snapshots/<profile dir>/<YYYYMMDDTHHMMSSZ>.json`. In the TUI `ctrl-s` works on Browse › Keys and the Analyze panels but Slots, and saves only a finished analysis: never partial results, and not the report of an analysis `esc` stopped early ([Analysis](#analysis)). A snapshot holds patterns and example key names, never values or credentials. Overview's "Last analysis" line reads the newest snapshot of the profile from this directory. It also records whether the scan saw the whole keyspace (`complete`: the cursor returned to 0 with no sample limit reached and no `MATCH`) and the `MATCH` glob it used; snapshots written before these fields existed read as incomplete with no glob.

- **Profile directory:** `<name>-<hash>`, where `<name>` is the profile name lower-cased with anything other than `[a-z0-9._-]` replaced by `_` (spaces included), trailing dots removed, and `_` added after Windows device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`). `<hash>` is the first 12 hex digits of the 64-bit FNV-1a hash of the exact profile name, so names that only differ in case, punctuation or non-ASCII characters get their own directories.
- **Docker containers** are snapshotted under `docker:<container>`, so they never share history with a saved profile of the same name.
- **Reading:** a snapshot whose stored `profile` is not exactly the requested one is ignored, and so are files that can't be parsed. The baseline is the newest remaining snapshot taken strictly before the new scan with the same `MATCH` glob.
- **Writing** goes to a uniquely named temp file that is flushed to disk and then renamed into place.

`redis-lens diff <profile>` first checks, before touching the server, that a baseline exists for the profile with no `MATCH` glob (the scan uses none), then scans, compares with that baseline (or with `--against <file>`), then saves the new scan. The TUI's Analyze › Changes uses the same baseline.

Snapshots also hold the server context (`server_context`); snapshots written before it existed load without it. A scan with sizes also writes `key_sizes` there and in `scan --json`: the 50 sampled keys with the largest `MEMORY USAGE` (key name, type, size, TTL; never values), and the sampled bytes per TTL bucket. They feed Big keys and Expiry. A scan without sizes writes no `key_sizes`, so its output is as before, and snapshots written before the field existed load without it.

A snapshot of a cluster scan (`ctrl-s` in the TUI or `scan --save`) also holds the nodes, each with its node, status, note and own pattern rows, and each hot key's node index. Saving from Browse › Keys narrowed to one node still saves the whole report. Snapshots written before v0.3 have no nodes and still load. When the two scans `diff` compares did not read the same nodes (one standalone and one cluster, a different number of shards, or different node IDs or addresses), it prints `Note:    the two scans read different nodes (standalone or cluster, shard count, or node ids or addresses); patterns are compared on the merged rows` before the pattern table; the `--json` output does not carry the note.

**Retention.** `scan --save --keep N` keeps the newest N snapshots of the profile, and `--keep-days D` deletes those older than D days. `N` is at least 1. Given both, a snapshot stays only when both allow it. The newest snapshot is never deleted. Snapshots dated after the current time (a clock that was ahead, or a file copied from another machine) are never deleted and take no keep slot, so the snapshot just saved always stays. Retention only touches files it would have written for that profile: regular files directly in the profile's directory, named exactly `YYYYMMDDTHHMMSSZ.json`, whose stored `profile` is the profile. Other files, corrupt files, other profiles' snapshots and symbolic links are left alone, and a profile directory that is a symbolic link is refused. Deleting snapshots touches local files only, never a server.

`redis-lens snapshots list <profile> [--json]` lists a profile's snapshots, newest first, with the time (UTC), file name, number of patterns, sampled keys, whether the scan was complete, and its `MATCH` glob. `redis-lens snapshots prune <profile> (--keep N | --keep-days D) [--yes]` applies the same rule; it needs at least one of them. Without `--yes` it only lists what it would delete; with `--yes` it deletes them and lists what it deleted.

**Trend.** `redis-lens trend <profile> [--pattern P] [--last N] [--json]` takes the newest N snapshots (default 7, at least 2) that share the newest snapshot's `MATCH` glob, and says how many with another glob were left out. For each pattern (only those containing P with `--pattern`) it shows estimated memory over time as a sparkline (`▁` to `█`, scaled from the series' smallest to its largest value; keys instead of memory unless every snapshot has sizes), the first and last value, and a least-squares growth rate per day, as a share of the pattern's mean and in bytes (or keys). A pattern missing from a snapshot is a gap (`·`), not a zero, and is left out of the fit. Partial scans are included and marked. Rows are sorted by growth per day, largest first. Reads local files only.

## Browsing, server views and writes

Phase 1 is built: [Navigation](#navigation), the [Shared command budget](#shared-command-budget-p1), the probes in [Permission check, additions](#permission-check-additions), and every item tagged P1 under [Views](#views-1). Of phase 2, [a key's value](#browse--keys--a-keys-value) and the value renderer ([Values](#values)) are built, with the Values row on Connections, and so are the stream entries on [Browse › Streams](#browse--streams), the live tail on [5 Pub/Sub](#5-pubsub), the queries on [Browse › Search](#browse--search), the Slow log arguments and the Monitor panel on [4 Server](#4-server), with the Hot keys ranking from Monitor, and the [Command line panel](#command-line-panel) for readonly commands. Phase 3 is built: every item tagged P3, with [Write permissions](#write-permissions), [Bulk actions](#bulk-actions), the `ACL DRYRUN` probes and [Server versions](#server-versions). The other items tagged P2 are not built. Phase 4 ([Topology and connections](#topology-and-connections-p4)) is built: every item tagged P4. Each phase lands with its own spec change: that change moves its text into the sections above, and updates the key table and `src/tui/keymap.rs` together. The phases are listed under [Milestones](#milestones); the tag after each heading or item says which phase it belongs to. The screens are drawn in the TUI design artifact linked there. The TUI redesign (five areas, Overview, analysis on request, Browse from key names) changed where each of these lives, not what it reads, except where the text says so.

### Navigation

- **Areas:** `0` Connections (the home screen), `1` Overview, `2` Browse, `3` Analyze, `4` Server, `5` Pub/Sub. `6` and any other digit do nothing. Before connecting, `1` to `5` still switch, and the body says "Connect first: open 0 Connections, pick a connection and press enter." An area keeps the panel and the level it was left at.
- **Panels:** `tab` and `shift-tab` switch to the next or previous panel of the area on screen, at the level it was left at, and do nothing else: they never move between panes. Browse has Keys · Streams · Search; Analyze has Big keys · Expiry · Changes · Hot keys, and Nodes · Slots only on a cluster (off a cluster they are not in the cycle, the area row or the `?` search); Server has Clients · Slow log · Monitor.
- **Deeper and back:** `→` `l` `enter` go one level deeper or into the right pane: on Browse › Keys a group expands, a pattern lists its keys, a key shows its value, and on a value `→` moves into the value; on Streams and Search `→` moves into the right pane (`enter` lists a stream's entries or opens a result). `←` `h` go back: collapse a group or go up to it, from a key's value to the key list, from the key list to the patterns, from a right pane to the left one, and on a stream's entries or a Search result back to the list. `esc` clears a filter first, then goes back as `←` does (on Browse › Keys it leaves a node narrowing instead of collapsing). On a screen where `esc` has nothing else to do it stops a running analysis and keeps what it sampled ([Analysis](#analysis)).
- **`v`** shows more columns and details on the area on screen, or fewer again: one setting per area, off by default, kept while the app runs, across connects too. What it adds is said with each screen.
- **Footer and `?`:** the footer shows the 3 to 5 keys that matter on the screen and `. Actions   ctrl-f Find key   ? Keys` at the right ([Keys](#keys)); `.` lists the actions of the selection and the screen with their keys and state ([Actions menu](#actions-menu)); `?` lists the rest and finds any action ([Help](#help)). Command names are left out of titles; `v` and `?` show them.
- **Clusters:** a cluster connect keeps a live connection to each shard's node and runs the view probes on every node ([Cluster live views](#cluster-live-views-p4)). Overview's live figures, 4 Server (Clients, Slow log, Monitor) and Browse › Search work on the picked node, and Overview adds a row per shard. The key names and a pattern's keys walk every shard (or the node Browse › Keys is narrowed to), a key's value and its edits go to the key's slot owner, Browse › Streams reads each stream on its owner, 5 Pub/Sub counts classic channels on the picked node and shard channels on every primary, and the bulk actions and the command line work per node. Browse › Keys after an analysis and every Analyze panel work on the merged results; Big keys and Expiry cover every node's samples, each big key carries its node, and Big keys reads lengths and encodings on each key's owner.
- **Command line (P2):** `:` opens it from any screen except the add flow, a filter or query input and an open dialog (the `?` help), where it is text. It is off before the live connection is ready, and says why. On a cluster it routes each command by its keys ([Cluster live views](#cluster-live-views-p4), Command line). See [Command line panel](#command-line-panel).
- **Screen layout:** from the top, two chrome rows on every screen, the context bar (the header, one line) and the area row (one line), then the body, and the footer (one line). 80×24 and 120×36 both work; nothing wraps, text is cut with `…`.
  - **The body** is one frame. A list sits on the left and the selected row's details on the right, with a shared divider (`┬` `┴`). Where a list needs the full width (Connections, Nodes, Clients, Slow log, Monitor, Slots), the details sit below it in the same frame (`├` `┤`).
  - **Focus:** the focused pane's border and title are drawn in the focus colour, the title bold; the other panes keep the normal border and a dim title. The shared divider belongs to the focused side. In a frame split top to bottom, the section with the selection is the focused one (Overview's Keys section, the Slots' hottest slots, the top list elsewhere).
  - **Selection:** every list keeps a two-cell gutter at its left; the selected row has `›` in it, in the focus colour, and a highlighted background, so the row reads without colour too. The column header lines up with the rows after the gutter.
  - **Dialogs** (`?`, "Analyze the keys?", a bulk action's dry run, the database list) draw centred over the screen, which is dimmed behind them, all but the footer, which says what the dialog wants.
  - **The confirm sheet** is the one question every write and costly read asks: see [The confirm sheet](#the-confirm-sheet). The [Actions menu](#actions-menu) (`.`) sits at the bottom-right of the body.
  - Every state keeps a word or a glyph (`✓` `▲` `✗` `●` `○`), never colour alone, so the light theme and `NO_COLOR` still read.
- **Area row:** `1 Overview   2 Browse › Keys · Streams · Search   3 Analyze   4 Server   5 Pub/Sub` with `0 Connections` at the right. The area on screen is bold with its digit as a key cap, and its panels follow its name after `›`: the current panel bold in the focus colour, the others dim. The other areas are dim; before connecting areas 1 to 5 are faint. Search is dimmed when its probe (`FT._LIST`) was refused or found no module. When the row is short, `0 Connections` shows only `0` first, then the other areas show only their digit, then the panels show only the current one and its place, such as `Expiry · 2 of 4`; the area on screen keeps its name (Connections too, while it is on screen). On Browse a short note sits at the right, before `0 Connections`: "names only, no analysis yet", "names only until the analysis has results", or "Search is off: no search module"; it is left out when it does not fit beside the panels.
- **Header (context bar):** one line. Before connecting: `redis-lens <version>   not connected`, and "pick a connection, then enter" at the right. Once connected, the profile's name (never a username or password), its environment badge, the ` w ` key cap and the write state; then ` b ` and the database (`db 3`), on a cluster the `[ ]` key cap and the picked node (`node 10.0.1.11:6379 · 1 of 3`), and the facts, dim and joined by ` · `: the keychain hint while a keychain read is slow, the cluster summary or the address, `standalone`, the replica the session reads, the server and version, memory used against the limit (`15G / 24G`, or `15G used`), the key count and `▲ no sizes` when `MEMORY USAGE` is denied. As many as fit show, kept in this order of importance when the line is short: the database when it is not 0, the keychain hint and the cluster summary (both cut with `…` rather than dropped), memory, the key count, `standalone`, the node, the replica, then `▲ no sizes`. After those, three parts show only when they fit, in this order, each only while the ones before it did: the address, database 0, then the server and version. So the version goes first as the line narrows, then database 0, then the address; the version or database 0 never shows without the address. Between the name, the badge and the ` w ` key cap there is one space. The status sits at the right:
  - `⠋ connecting…`;
  - `● connected 14:02 UTC`: connected, no analysis yet;
  - `⠋ analyzing 41%`: how much of the sample is read (`⠋ analyzing` while an analysis connects with earlier results on screen);
  - `⠋ stopping…`: `esc` asked it to stop;
  - `● analyzed 14:02 UTC`, the time of the report, with `(stopped)` after an analysis `esc` stopped early;
  - `✗ failed`.

  Times are UTC: the TUI has no time zone. When the line is short, the status's time gives way first (`14:02 UTC`, then `14:02`, then no time), before any other part but the address, database 0 and the version, which show only when they fit. `w` toggles the write state from any screen ([Keys](#keys)). The write state is:
  - `read-only` (amber) on a read-only profile;
  - `writes on` (red text) on another profile with writes on;
  - `WRITES ON` (red badge) on a `prod` profile with writes on.

### Shared command budget (P1)

- **One limiter for everything:** every command a view sends goes through the profile's command budget ([Scanning](#scanning), rule 2), not only scans. That includes polls, key lists, values, the command line and writes. There is one limiter per connected profile. Each command, or batch, books the next slot of the profile's time line before it is sent; after an idle gap a slot may start in the past by at most its own length (at least 10 ms), so idle time never builds a burst bigger than one batch, and a scan's batch takes the longer of its slot and its round trip, not both.
- **Polling views** poll only while they are on screen, and `space` pauses them:
  - Overview: `INFO` every 2 s (`>` and `<` change it, 1 to 60 s), and on a cluster each shard's row every 5 s ([1 Overview](#1-overview))
  - Clients: every 5 s
  - Slow log: every 10 s
  - Streams: `XINFO` every 5 s
  - Pub/Sub counts: every 5 s
- **Big keys are always read in pages:**
  - `HSCAN`, `SSCAN` and `ZSCAN` with `COUNT 100`;
  - `LRANGE`, `ZRANGE` and `XRANGE` 100 items at a time;
  - `GETRANGE` for the first 64 KiB of a string, and `JSON.GET` only for a document of at most 256 KiB, shown up to 64 KiB;
  - `VRANDMEMBER` for 100 random elements of a vector set, which has no pages.

  redis-lens never sends `KEYS`, `HGETALL`, `SMEMBERS`, `LRANGE 0 -1` or another whole-key read. A user can still type them on the command line.

### Views

#### 0 Connections, additions

- **ACCESS column (P1):** `read-only` (amber) or `writes on` (red). The details add two rows:
  - **Access:** the setting in words, why, and the key that changes it: "read-only, the default for production. w turns writes on.", "read-only (read_only = true). w turns writes on.", or "writes on (the default outside production). w turns them off." (`read_only = false`, or how many writes the server refuses this user, in the brackets instead);
  - **Values:** "shown on screen, never saved" (built with [a key's value](#browse--keys--a-keys-value)), on a profile with no tags and no Sentinel.
- **`w` (P3)** turns writes on or off for the selected profile and saves the change to the config file (`read_only = false` or `true`). See [Write permissions](#write-permissions).
  - Turning writes on asks first, as in [Write permissions](#write-permissions) check 2: `y`/`n`, or type the profile name on `prod`. `skip_non_prod_confirm` does not apply to it. The change applies once the config file has it.
  - Turning writes off never asks and applies at once, before the config file is saved; a failed save says so.
  - Docker containers have no saved profile, so `w` affects only the current session for them.
  - It is the only setting the TUI writes to the config file outside the add flow.

#### 2 Browse › Keys, additions

- **The `▲` mark (P1):** each pattern has a verdict, the most worrying first: a lock pattern with keys that have no TTL, `growing` and `mixed` (the TTL verdicts), a sampled key of 64 MiB or more, `persistent`, `ok`. The list shows it as the `▲` mark, which is always shown, and the right pane says it in a sentence ([2 Browse › Keys](#2-browse--keys), After an analysis). On narrow terminals the other columns hide first, so at 80 columns the share bar is gone and long pattern names are cut with `…`.
- **`→` on a pattern (P1)** opens its key list; on a group it expands the group, and `←` collapses it.
- **`t` and `d` (P3):** bulk expire and bulk delete for every key of the pattern (`T` and `D` on the pattern's key list). See [Bulk actions](#bulk-actions).

**A pattern's keys (P1).**
- It sends `SCAN MATCH <glob>`, where the glob is the pattern with each placeholder replaced by `*` and glob characters in the literal parts escaped. Opening the list loads the first 1,000 keys of the pattern (at most 500 `SCAN` calls); `m` loads 1,000 more. Only the first page loads on its own: when a page finds no key of the pattern, the list says so and waits for `m`.
- A glob can match keys that infer to other patterns (`user:*:cart` matches `user:abc:cart`), so only keys that infer to the selected pattern are listed, and the right pane counts the ones left out. The bottom line shows how many are loaded and the pattern's estimated key count (`N of ~M loaded. m loads 1,000 more.`, the count exact when it is known). The title says the sort, the filter and the type filter; on a cluster it starts with how many nodes are done (`2 of 3 nodes`), and with `v` it shows the `SCAN MATCH` glob.
- The keys are on the left and the selected key's facts on the right: its length, size and expiry, its node on a cluster, a `▲` sentence when it is stored past its compact encoding, and "→ shows the value"; then any column that is off and why.
- Each key shows:
  - its type, TTL and size (`MEMORY USAGE`), and its encoding (`OBJECT ENCODING`);
  - its length: `STRLEN`, `HLEN`, `LLEN`, `SCARD`, `ZCARD` or `XLEN`;
  - its LFU counter (`OBJECT FREQ`), when the policy is LFU.

  These are pipelined like a scan, so values aren't read.
- `/` filters the loaded keys by name and `s` sorts them: by size, length, TTL, LFU counter (under an LFU policy) or name. `y` copies the key name and `Y` the pattern. `esc` clears the filter first, then goes back to the patterns, as `←` does. `→` or `enter` opens the selected [key's value](#browse--keys--a-keys-value). `+` adds a key (P3): its name (prefilled with the pattern's text before the first `{`), its type (string, hash, list, set, sorted set, stream) and a first value, sent with `SET … NX`, or after `WATCH` and `EXISTS` for the other types, so it never overwrites a key; a key that exists says so. The new key is added to the list and its value opens.

**Edits on [a key's value](#browse--keys--a-keys-value) (P3), each through [Write permissions](#write-permissions):**
- `e`: edit the selected field, element or value: `SET … KEEPTTL` for a string, `HSET`, `LSET`, a set member replaced (`SREM` and `SADD` in `MULTI`/`EXEC`), a sorted set member's score (`ZADD`), or `JSON.SET`. A value that is UTF-8, at most 4 KiB and has no control or bidi character is edited in a prompt on screen; any other opens `$VISUAL`, then `$EDITOR`, then `vi`, on a temporary file (mode 0600) in a new directory (mode 0700), which is deleted afterwards whatever happens. A newline the editor adds at the end is dropped when the value had none; an editor that exits with an error, or an unchanged value, sends nothing. A string is edited only when its whole value is loaded, and a JSON document only when it wasn't cut; a stream entry and a vector set are not edited, and say why.
- `+`: add a field, element or member: `HSETNX` (never overwriting a field), `RPUSH`, `SADD`, `ZADD` with a score, `XADD` with one `field value` pair.
- `T`: set the key's TTL, with `EXPIRE` or `PERSIST`. `t`: set the selected hash field's TTL, with `HEXPIRE` or `HPERSIST` (Redis 7.4 and Valkey 9.0, where field TTLs are read; otherwise it says why, and on any other type that `T` sets the key's). It takes `90`, `90s`, `10m`, `24h`, `7d`, `1w`, combinations such as `1h30m`, and `-`, `persist` or `none`; `0` is refused (`D` deletes) and the longest is 100 years.
- `R`: rename, with `RENAMENX`. It never overwrites an existing key, and says so.
- `d`: delete the selected field, element or member, with `HDEL`, `SREM`, `ZREM`, `XDEL`, or `LSET` to a unique marker then `LREM` for a list element.
- `D`: delete the key, with `UNLINK`. The key list drops it and the value closes.
- On the key list itself, `t`, `d` and `R` do the same for the selected key (`EXPIRE` or `PERSIST`, `UNLINK`, `RENAMENX`), with the same prompts and questions: the list updates the row (its TTL, its name, or drops it) and an open value of that key follows. When `t` or `R` finds the key gone (EXPIRE answers 0, RENAMENX says there is no such key) the row goes as after `d`, with "The key no longer exists." `T` and `D` there are the pattern's [bulk actions](#bulk-actions); a Find result list has no pattern, so they say so and do nothing.

A string edit, and a list element's edit or delete, is sent with `WATCH` and `MULTI`/`EXEC` after checking the value it was loaded with, so it fails and says so instead of overwriting a value that changed since then; the value is read again. After each write, what changed is read again: the value from its first page, with the selection kept, and the key's row in the key list. One write at a time per view, and one write at a time over the session's connection.

#### Find key (`ctrl-f`)

Finds keys by name on the whole server, from any screen once connected, before an analysis too. It only reads, so read-only profiles find keys as well.

- **Opening it:** `ctrl-f` opens a one-line prompt in place of the footer: `Find key: <text>` and, dim, "a name, or a pattern with * ? [ ]". It starts with the text last searched. Before connecting it says to connect first. While the write gate's question (or any [confirm sheet](#the-confirm-sheet): writes on, a bulk action's, Monitor's, the command line's), a bulk action's dialog, the database list, "Analyze the keys?", the [Actions menu](#actions-menu), the command line, the add flow or the `?` help is open, `ctrl-f` does nothing: those take every key. The filter inputs of Connections, Patterns and a key list close, keeping their text; any other text input of a view (a form, the Search query, the Pub/Sub prompt) stays as it is under the prompt and is back after `esc`.
- **The prompt:** typing edits the text (at most 512 bytes; control characters are left out, typed or pasted), `backspace` deletes, `ctrl-u` clears, `esc` closes it and searches nothing, `enter` searches. Spaces around the text are left out, and on empty text `enter` does nothing. Until the live connection is open `enter` searches nothing and says "Still connecting. Press ctrl-f, then enter, once connected." (the cluster's nodes may not be known yet); once it has closed it says "The live connection is closed, so keys can't be searched. Connect again on 0 Connections." `ctrl-f` brings the text back either way. The text is drawn with control characters and escape sequences as escapes.
- **The glob:** text with `*`, `?` or `[` in it is a `SCAN MATCH` glob and is sent as typed (`\` escapes the next character, as in Redis). Any other text finds every name that contains it: `*`, the text with each `\` doubled, `*`. So `28648` finds `user:28648:feed`, and `user:*:cart` finds the carts. A typed glob may hold at most 8 `*` wildcards (a run of `*` counts once): one with more is slow to match on every key `SCAN` looks at, and on servers without the CVE-2022-36021 fix can take exponential time, so it is not sent and the notice says "A pattern may have at most 8 * wildcards, so matching it stays cheap for the server. This one has N."
- **The results** show on 2 Browse › Keys as a key list titled `Find: <glob>`, in place of a pattern's keys: the same columns, sort, `/` filter, `ctrl-t` type filter, `y` copy, `t` `d` `R` and `+`, the same `SCAN MATCH <glob> COUNT 1000` pages with at most 500 `SCAN` calls each, from the [command budget](#shared-command-budget-p1), and the same pipelined facts (type, TTL, size, encoding, length, LFU counter), never a value. Every key `SCAN` returns is listed, since there is no pattern to infer to. Only the first page loads on its own; `m` loads 1,000 more. The bottom line says `N loaded. m loads 1,000 more.` while the walk goes on, then `N keys match.`, `1 key matches.` or `No key matches <glob>.`; a first page that matched nothing yet says `No key matched yet. m scans further.` `→` or `enter` opens a key's value, and `←` there comes back to the results. `esc` or `←` on the results (with no filter to clear) goes back to the screen `ctrl-f` was pressed on, or to Patterns when that was a pattern's keys or a key's value, whose keys the find replaced. A new find replaces the results, also from the results themselves (it keeps where to go back to), and opening a pattern's keys replaces a find. A form open on a key's value (`e`, `+`, `t`, `T`, `R`) is dropped by a find, since the list it was opened from is replaced.
- **On a cluster** the walk reads every connected node in slot order, whatever node Patterns is narrowed to, as a pattern's keys do ([Cluster live views](#cluster-live-views-p4)): each node's own `SCAN`, redirects never followed, a node without a connection left out and named.
- **Degrading:** a refused `SCAN` says "The server refused SCAN for this user, so keys can't be searched."; other failures say what a pattern's keys say.

#### 3 Analyze

Expiry, Changes, Hot keys and Nodes are described under [The Analyze panels](#the-analyze-panels); they are the earlier TTL, Diff, Hot keys and Nodes views.

- **Big keys (P1):** the 50 biggest sampled keys across all patterns (from the report's `key_sizes`). KEY and SIZE at 80 columns; TYPE and TTL when the list is 44 cells or more; PATTERN and LENGTH with `v`. Length and encoding are read live for those 50 only, pipelined. `y` copies the key name. The right pane shows the selected key (size, length, TTL, pattern, and its encoding once read), then "THINGS TO LOOK AT", the recommendations below. With no sized key it says why: "No sizes: MEMORY USAGE is not allowed for this user, so keys can't be ranked by size.", or that no sampled key had a size.
- **Expiry (P1):** memory that will be freed by TTL within 1 h, 1 d, 7 d and 30 d, and memory that never expires. It is computed from the sampled `PTTL` and size, scaled like the estimates, and needs no new commands. It is drawn as the "MEMORY FREED BY TTL" bars in [Analyze › Expiry](#analyze--expiry)'s right pane, and as the "FREED BY TTL" line on [1 Overview](#1-overview); it has no screen of its own.
- **Recommendations (P1):** "THINGS TO LOOK AT", on Overview after an analysis (each with the panel `enter` goes to) and in Big keys' right pane. It uses only data the analysis already has, plus `OBJECT ENCODING`, never `CONFIG GET` (ElastiCache blocks it). The rules:
  - a key of 64 MiB or more;
  - a collection of 1,000,000 items or more;
  - in a pattern, 5% or more of its hashes, sets or sorted sets past their compact encoding (`hashtable`, `skiplist`). The analysis reads no encodings, so this checks the pattern's example keys (its largest), for the 100 biggest such patterns: it can overstate a pattern whose big keys are the exception;
  - a pattern whose segment is `lock`, `locks`, `mutex` or `lease` with keys that have no TTL;
  - the share of memory that never expires, when it is over 25%;
  - every TTL verdict that isn't `ok`;
  - the eviction policy against the sample and the server's memory (phase 5). These send no command, use `maxmemory-policy` from the connect's `INFO`, and say nothing when the policy is unknown (empty):
    - a `volatile-*` policy while the sample holds at least 10 keys and fewer than 5% of them have a TTL (Warn, opens Expiry): the policy only evicts keys with a TTL, so it can't free memory and writes fail at `maxmemory`. The hint says to set TTLs or use an `allkeys-*` policy;
    - the `noeviction` policy with `maxmemory` set and used memory at 90% of it or more (Warn, opens Big keys): the server refuses writes at the limit. The hint says to raise `maxmemory`, free memory or use an eviction policy;
    - a policy that is not LFU (Info, opens Hot keys): the server keeps no access counters, so Hot keys can't rank by `OBJECT FREQ`. The hint suggests `maxmemory-policy allkeys-lfu` (or `volatile-lfu`), or Server › Monitor.

  The 64 MiB rule is about one key. Browse › Keys' "Large" verdict (largest key 50 MiB, or average 10 MiB, which draws the AVG cell in amber) is about a pattern and stays as it was.

#### 4 Server

Overview's live figures moved to [1 Overview](#1-overview) ("Now"). Each Server panel is one frame: the list takes the full width and the selected row's details sit below it.

- **Clients (P1):**
  - `CLIENT LIST`, grouped by library (`lib-name` and `lib-ver`, Redis 7.2 and later; otherwise the client name, or "(no name)") and by address (/24 for IPv4, /64 for IPv6). Both kinds of group are in one list, library groups first, with a BY column. Each group shows connections, the oldest age, the longest idle time, the most common last command, and memory. The `user=` field is dropped when the reply is read and never shown.
  - A group with connections idle for an hour or more is flagged. `enter` lists a group's connections.
  - `d` ends the selected connection with `CLIENT KILL ID` (P3, `@dangerous`), on a group's list of connections. It refuses the connection redis-lens read the list with (the one whose last command is `CLIENT LIST`, idle 0). The list is read again afterwards.
- **Slow log:**
  - `SLOWLOG GET 128` (P1): time (UTC), duration, command name, key names and client. Key names come from the command's key specs in `COMMAND INFO`, asked once per command name and kept for the session (before Redis 7.0, the first/last/step key positions, so commands with moving keys show fewer). `COMMAND GETKEYS` is never used: it would send the logged arguments back to the server. The arguments (P2) are kept only as one line with the credentials masked (`AUTH`, `HELLO … AUTH`, `ACL SETUSER` rules starting `>`, `<`, `#` or `!`, `CONFIG SET requirepass`/`masterauth`/`masteruser`, `MIGRATE … AUTH`/`AUTH2`; the server may already have written some as `(redacted)`), drawn escaped on the `args` line of the selected entry's panel, on screen only. When the server cut a long command to 32 arguments, its marker is not shown as an argument or a key: the line ends with `… N more arguments`.
  - Notes on known problems: `KEYS`; `FLUSHALL` and `FLUSHDB`; whole-key reads (`HGETALL`, `SMEMBERS`, `LRANGE 0 -1` and the like). The slow log has no sizes, so a whole-key read is known by its shape; that it was slow is the hint that the key is big.
  - The threshold and length come from `CONFIG GET slowlog-*` where it is allowed; otherwise the panel says they are unknown. When `COMMAND INFO` is refused the entries show without key names and say why. Neither is asked again in that session.
  - `e` sets the threshold and length with `CONFIG SET slowlog-log-slower-than` and `slowlog-max-len` (P3, `@dangerous`), from a form filled with the current values where they are known; only changed values are sent, one `CONFIG SET` each. `D` sends `SLOWLOG RESET` (P3, `@dangerous`).
- **Monitor (P2):**
  - `MONITOR` on a connection of its own, through the profile's route and credentials (TLS, token command and SSH tunnels). redis-rs reads `MONITOR` lines only on a connection that resolves the host itself and has no credentials provider, so the panel is off, with the reason, through an SSH tunnel with TLS and with Entra ID. redis-rs also swallows the server's answer to `MONITOR`, so the permission is checked first with one `MONITOR` on an ordinary connection that is closed at once; each of the two takes one command from the budget. A user without `+monitor` gets "the server refused MONITOR for this user", never the username or the server's text.
  - It stops after 30 s or 10,000 lines, whichever comes first, on `esc`, and with the session: a new analysis or connect, the live connection closing, connecting elsewhere or quitting. The lines stay in memory only, for the session.
  - It always asks first, on the [confirm sheet](#the-confirm-sheet) ("Start MONITOR?"): `y`/`n`, or on a `prod` profile you type the profile name. Read-only profiles too, because `MONITOR` writes nothing but slows the server. The sheet names the server and says why it asks.
  - Each line shows its time (UTC, from the line's own timestamp), database, client and the command as one line, credentials masked as on the Slow log, escaped; newest at the bottom. The selection follows the newest line until you move it, and `End` follows again. The title says whether it runs, how many lines it kept and the time left. A line longer than 1 KiB keeps its arguments up to there and says how many it left out.
  - Without an LFU policy, Analyze › Hot keys can rank keys by how often they appear in the last Monitor run ([Analyze › Hot keys](#analyze--hot-keys)).

#### Browse › Streams

A panel of 2 Browse: the streams on the left, the selected one's groups and consumers on the right, in one frame. Streams are found by an analysis: before one the panel says "Streams are found by an analysis: it samples the keys and lists the streams among them." and "a analyzes.", and `a` opens "Analyze the keys?" ([Analysis](#analysis)). After one that found none it says so, and that a Deep analysis samples more.

- **Stream list (P1):** the streams the last analysis found (type `stream`; a sample may miss some, and the title says so). For each:
  - from `XINFO STREAM`: length, the age of the oldest entry (from its ID, against the server's `TIME`; this computer's clock when `TIME` is refused), and the last ID. Only entry IDs are kept: the first and last entries' fields are dropped when the reply is read. Size comes from `MEMORY USAGE` (`XINFO STREAM` has none) and is left out when that is refused;
  - from `XINFO GROUPS`: groups, lag (Redis 7.0 and later), pending entries and the last delivered ID;
  - from `XINFO CONSUMERS`: consumers, with pending entries, idle time, and inactive time (Redis 7.2 and later).
- **Verdicts:**
  - `stuck`: a consumer that holds pending entries has been idle 30 min or more (idle time; a consumer that keeps polling an empty stream resets it);
  - `lagging`: a group's lag is 1,000 or more;
  - `no readers`: no consumer groups;
  - `unbounded`: the oldest entry is over 30 days old and the stream holds 100,000 entries or more;
  - `ok`.

  All that apply are computed; the list shows the first, in the order above, with `+` when there are more, and the right pane lists them all. `→` moves from the list into the right pane (groups and consumers), and `←` or `esc` back. At most 50 streams are read per poll. An optional command refused for this user (`TIME`, `XINFO GROUPS`, `XINFO CONSUMERS`, `MEMORY USAGE`) is left out with a note and not asked again until `r`. An `r` pressed while a poll is in flight reads once more as soon as that poll answers, asking again for what was refused.
- **Entries (P2):** `enter` lists the selected stream's entries, one level below the list: they take the right pane, and the list narrows to the stream keys, as the key list does beside a key's value. `←` or `esc` goes back to the list.
  - They are read like a stream in [a key's value](#browse--keys--a-keys-value), through the [command budget](#shared-command-budget-p1): the first page `TYPE` and `XREVRANGE key + - COUNT 100`, newest first; `m` the next 100 with `XREVRANGE key (<last id> - COUNT 100`. At most 10,000 are held. The title says how many are loaded of the stream's length.
  - They are drawn and work as a key's value: the entries above a divider, each entry's fields shown one line each, and the selected entry's fields in full below it. `→` moves from the entries into the value and `←` back, `f` changes the format, and `y` does not copy an entry, since it has several fields. A refused `XREVRANGE` says so in redis-lens's own words and `m` does not send it again.
  - The `XINFO` poll stops while the entries are shown; back on the list it reads at once when a poll is due. `space` still pauses or resumes it. `r` reads the entries again from the newest.
  - Switching screens and coming back shows the entries again. `enter` on the same stream again keeps what it loaded; another stream starts afresh. A stream gone since the analysis has no entries to list, and `enter` says so.
- **Writes (P3):**
  - `A` on a group or consumer lists its pending entries under it (`XPENDING`, the first 100; a read, so read-only profiles too); on a pending entry it acknowledges it (`XACK`);
  - `c` claims the group's pending entries idle at least N ms for another consumer (`XAUTOCLAIM … COUNT 100`): it takes any consumer's idle entries, not only the selected one's, and the form says so;
  - `-` trims the stream with `XTRIM MAXLEN ~ N` or `MINID ~ id` (destructive).

  `a` analyzes; acknowledging is `A` so the two never meet.

#### 5 Pub/Sub

- **Counts (P1):** `PUBSUB CHANNELS` (the first 1,000, most subscribers first; the title says how many there are), `PUBSUB NUMSUB` for each channel in batches of 100, and `PUBSUB NUMPAT`. Payloads aren't read. A refused `NUMSUB` or `NUMPAT` leaves its column out with a note.
- **Live tail (P2):**
  - **Subscribing:** `enter` subscribes to the selected channel. `+` asks for a channel or pattern: text with `*`, `?` or `[` in it is sent with `PSUBSCRIBE`, any other with `SUBSCRIBE`; `enter` subscribes, `esc` cancels. Subscribing is not a write, so read-only profiles subscribe too.
  - **Its own connection:** opened like the session's (same route, SSH tunnel, TLS, token command or Entra ID, and credentials from the same place), but speaking RESP3, and always to the profile's own endpoint, not the replica a `prefer_replica` scan reads. The `SUBSCRIBE` or `PSUBSCRIBE` takes one command from the session's [command budget](#shared-command-budget-p1).
  - **One tail at a time:** a new subscription replaces the last one, drops its connection and starts with an empty list. The tail keeps running while another view is shown. It ends with `esc` (when no filter is set), a new subscription, a new connect or a new analysis (which connects again), and quitting. Messages of a tail that ended, or of a replaced run, are ignored.
  - **Messages** reach the view in batches at most every 100 ms, each stamped with the time redis-lens received it. A batch holds at most 2,000; when more arrived, the oldest are skipped and the footnote counts them. The last 2,000 stay in memory, newest at the bottom, as TIME (`HH:MM:SS.mmm`, UTC, when received), CHANNEL and MESSAGE (the value on one line; see [Values](#values)).
  - **Title:** `Subscribed: <channel or pattern> · <rate> msg/s · <kept> kept (last 2,000)`, with `pattern` before a pattern. When it doesn't fit, `(last 2,000)` goes first, then the parts from the right. The rate is messages per second over the last 5 s while the view is on screen (over at least 1 s); messages that arrived while another view was shown are kept but not counted. The footnote says "Messages are shown, never saved."
  - **Keys:** `space` pauses: the list shown stays as it was while messages still arrive and are kept, and the title says `paused · N new`; `space` again shows them. `ctrl-l` clears the list (the tail keeps running). `/` filters what is shown by channel or message (case-insensitive, on the message's one-line form); `esc` clears the filter. Without a tail, `space` pauses polling the counts.
  - **Layout:** one frame. Before a tail, the channels are on the left and the selected channel's details (or the totals) on the right. Once a tail started, the channels are on the left, narrower and without the bars, and the tail on the right. The channel list is 22 columns wide at 80 columns and 35 from 120.
  - **Refusals** (rule 3), in redis-lens's own words, never the server's text or the username: "the server refused SUBSCRIBE on that channel for this user" (no `+subscribe`, or channel rules that leave the channel out, such as `resetchannels`), "the server refused PSUBSCRIBE on that pattern for this user", "the server refused RESP3 (HELLO 3), which the live tail needs to receive messages" and "the server refused HELLO for this user". A connection that can't be opened says why as a connect error does, and a lost one says "the connection to the server was lost". The tail stops and says so; `enter` or `+` subscribes again.
  - **Terminal safety:** channel names, patterns and messages are drawn with control characters as escapes, in the channel list too.
- **Publish (P3):** `P` publishes a message with `PUBLISH`, to the selected channel or another one typed in the form, and says how many subscribers got it. It is not destructive.

#### Browse › Search

A panel of 2 Browse: the indexes on the left, the selected index's schema or its results on the right, in one frame; one level deeper (a result, the explain or the profile) stays in the right pane.

- **Indexes (P1):** shown when `FT._LIST` works (Redis Search or valkey-search). Otherwise the panel says the module is missing ("The Search module is not loaded on this server."), and when the permission check found `FT._LIST` not allowed, Search is dimmed in the area row and the row's note says "Search is off: no search module".
  - Each index, from `FT.INFO`: what it indexes, its prefixes, documents, size, indexing progress and indexing failures.
  - The schema of the selected index.
  - Features the module lacks are turned off with a reason. valkey-search reports no index size, so Size is left out on Valkey; it lists fields by name, Redis Search in creation order.
  - The list is read when the view is shown and with `r`; it is not polled. A failed read is not retried on its own: before any list has been read the view shows the failure and `r`; after one, it keeps the last list and says the new read failed. `→` moves from the indexes into the schema, and `←` or `esc` back.
- **Queries (P2):** on the selected index, through the [command budget](#shared-command-budget-p1), one command at a time:
  - `/` opens a query input, which starts from the query last run. `enter` sends `FT.SEARCH <index> <query> LIMIT 0 50`, the query as one argument, as typed; an empty query is `*`. `esc` closes the input without sending. Every key is text in it; a pasted newline becomes a space. valkey-search does not take `*`: it needs a field filter, such as `@price:[-inf +inf]`, and says so in its message.
  - The results replace the schema in the right pane while their index is selected, and get the focus (`←` goes back to the indexes). The pane says the query, how many documents match (`3 of 1,204 results`) and the time the command took. Each of the up to 50 results shows its key and its first fields as `name=value`, each value shortened by the value renderer. A JSON index returns the document as one field, `$`.
  - `enter` opens the selected result one level deeper, in the right pane, drawn and working as a hash in [a key's value](#browse--keys--a-keys-value): its fields above, the selected one's value below, `→` `←`, `f` and `y` as there. `esc` (or `←` from the fields) goes back to the results.
  - `x` shows `FT.EXPLAIN <index> <query>`, the plan one line per line, and `X` runs `FT.PROFILE <index> SEARCH QUERY <query> LIMIT 0 50`. Both use the query last run, on the selected index; with none run yet they say to press `/` first. They open one level deeper, scroll with the move keys, and `esc` goes back. The profile is drawn as an indented tree of the reply (`name: value` per field, `- ` before each item of a list), and its results replace the results in the right pane.
  - Errors: a refused command says "The server refused FT.SEARCH for this user." (or `FT.EXPLAIN`, `FT.PROFILE`) and an unknown one "The server does not support FT.EXPLAIN." valkey-search has neither `FT.EXPLAIN` nor `FT.PROFILE`. Either way the command is not sent again until `r`; `x` or `X` says why, and the results pane leaves it out of its keys. Any other error from the server about the query (a syntax error, an unknown index) shows the server's message, because it describes the query and not the user: without its leading upper-case code, escaped and cut at 200 characters. This is the one place server error text is shown, and only for these three commands.
- **Writes (P3):** `+` creates an index from a form (name, `ON HASH` or `JSON`, prefixes, and fields as `name TYPE` or, on JSON, `$.path AS alias TYPE`, with types `TEXT`, `TAG`, `NUMERIC` and `GEO`) with `FT.CREATE`. `d` drops one with `FT.DROPINDEX`, never with `DD`, so documents are kept. The list is read again afterwards. Both act on the index list only: with a result, an explain or a profile open, `d` says "d drops an index: esc back to the index list first." and `+` "+ creates an index: esc back to the index list first.", and nothing is sent or asked, whatever `skip_non_prod_confirm` says.

### Command line panel

- **The panel (P2):** `:` opens a panel at the bottom of the screen, over the view, which keeps running (its polls go on). It is off, and `:` says why, before the live connection is ready and after it closed. On a cluster it sends each command to its keys' slot owner, or to the picked node ([Cluster live views](#cluster-live-views-p4)). While it is open every key is text for it (digits, `q` and `?` included) and its input line replaces the footer; `esc` closes it, keeping the line, the output and the history. Its keys are under [Command line panel · keys](#command-line-panel--keys) in the key table.
  - One command at a time, through the [command budget](#shared-command-budget-p1); `enter` while one runs says to wait. The line is split like `redis-cli` (quotes and `\xHH` escapes); a line that does not split says why.
  - Replies are drawn like `redis-cli`: `(integer) 5`, `(nil)`, strings quoted (`"a\nb"`), status replies bare (`OK`), arrays numbered `1) …` with nested ones indented under their number, and over RESP3 sets `1~ …`, maps `1# key => value`, `(double) 1.5`, `(true)`, `(big number) …`. Every string is escaped as in [Values](#values). Each reply shows the time the command took (the wait for the budget not included), as `(0.42 ms)`. A reply over 1 MiB is cut on screen, and its last line says so.
  - Errors: a refused command says "the server refused GET for this user", never the server's text, which names the user (a `NOPERM` inside a reply reads "NOPERM the server refused this for this user"); an unknown one "the server does not support …". Any other error from the server is about the command typed and is shown as `redis-cli` does, `(error) ERR syntax error`, escaped and cut at 200 characters.
  - `↑↓` steps through the history, `tab` completes, `PgUp` and `PgDn` scroll the output, `ctrl-l` clears it, `ctrl-u` clears the line and `esc` closes. The cursor stays at the end of the line: typing adds there and backspace takes the last character.
  - The output keeps the last 100 commands and at most 4 MiB of replies, each reply counted by the largest of its result views, so the cap holds whichever view is shown; the oldest go first.
  - **Result views:** `ctrl-t` cycles a reply's view: text, table, JSON, raw. The title adds `· table view` (and so on) when the view isn't text. All four keep the same escaping, the same 1 MiB cut with its last line, and the fixed NOPERM text.
    - Table: a map, an array of 2-item arrays, or the flat array of a pairs command (`HGETALL`, `CONFIG GET`, `MEMORY STATS`, `XINFO STREAM`, the module `*.INFO` commands, `… WITHSCORES` and `… WITHVALUES`) shows as `#`, field and value columns; an array of arrays shows as rows; any other array as `#` and value. A nested aggregate in a cell reads `(array of 3)`. Every column but the last is cut at 60 characters.
    - JSON: 2-space indent. A map is an object when every key is a string, otherwise an array of `[key, value]` pairs. Sets are arrays, nil is `null`, `nan` and `±inf` and big numbers are strings, and an error is `{"error": …}`. Control and bidi characters are written `\uXXXX`, invalid UTF-8 `\xNN`.
    - Raw: one value per line with its type first (`[bulk string] $5 "hello"`, `[integer] :5`, `[map] %2`), indented under aggregates.
  - **Repeat, `N cmd`:** a count in front of a command sends it that many times, at most 100. A count of 0, over 100, or signed is refused with one line, and nothing is sent, not even `COMMAND INFO`.
    - Commands go one at a time. Each is checked as a single command is, right before it is sent, takes one from the budget, and goes through the write gate every time it is a write.
    - A `@dangerous` readonly command asks once, naming the count ("Send it 3 times to …?").
    - A write that would ask (destructive, `@dangerous`, or any write on `prod`) is not repeated: "Not sent: this write asks first, and the write gate's question covers one command, so it is not repeated. Send it without the count." On a read-only profile a repeated write is refused before anything is sent.
    - Replies are numbered `#1` onward, and the run ends with `(N replies in N round trips · time)`. `esc` stops a run (the command in flight is answered and kept); a second `esc` closes the panel. A long run drops its oldest replies to stay under the output cap, and says so.
    - Credentials after the count are masked, and the history keeps the count.
  - **Pipeline mode:** `ctrl-p` toggles it; it is off by default and the title shows `· pipeline`. A readonly repeat then goes in pipelines of at most `max_commands_per_sec / 10` commands (at least 1; all of them when the profile has no limit). Every command is checked before its pipeline is sent, and the whole pipeline is taken from the budget first. Writes still go one at a time.
  - Uploading a commands file is not built.
- **Helper (P2):** from `COMMAND DOCS <name>` (Redis 7.0 and later, and Valkey), read once a name when its first word is typed out: the command's arguments and complexity on one line, its summary, whether it is sent (readonly, asks first, or not sent here) and its ACL categories on the next. On older servers the helper is off and says so once in the output. `tab` completes command names (from `COMMAND LIST`, asked once a session when the panel first opens; on Redis 6.2, which lacks it, only the names already looked up), subcommands (from `COMMAND INFO`) and the key names the key list has loaded, quoted when they need it. Pressing `tab` again goes to the next match.
- **Readonly commands are sent; writes go through the write gate:** before sending a command, redis-lens reads its flags and ACL categories with `COMMAND INFO <name>` (once a name a session; for a container command such as `CONFIG`, the subcommand's). A command without the `readonly` flag is a write: it goes through [Write permissions](#write-permissions), so on a read-only profile it is not sent and the panel says why. So `INFO`, `PING`, `CLIENT LIST` and `CONFIG GET`, which lack the flag, are sent only where writes are on. A command that blocks (the `blocking` flag, or `@blocking` before Redis 7.0: `BLPOP`, `XREAD … BLOCK`) is not sent, since it would hold the connection every view shares; `XREAD` without `BLOCK` is. An unknown command says so. When `COMMAND INFO` itself is refused, nothing is sent. The check runs again where the command is sent.
- **Not sent whatever their flags:** scripts and functions (`@scripting`, and `EVAL`, `EVAL_RO`, `EVALSHA`, `EVALSHA_RO`, `FCALL`, `FCALL_RO`, `SCRIPT`, `FUNCTION` by name): `EVAL_RO` is flagged readonly, but its script can run commands the panel never sees (`SLOWLOG RESET`, `KEYS` without the question) or loop until someone sends `SCRIPT KILL`. Module internals (names starting `_`, such as `_FT.DEBUG`). Module commands (a dotted name), which are often flagged readonly though they change state (`FT.CONFIG SET`, `FT.ALIASADD`, `FT.DICTADD`, `FT.DEBUG`, `GRAPH.CONFIG SET`, `CF.COMPACT`, `search.*` and `timeseries.*` cluster commands): only those known to read are sent, the RedisJSON, Search, TimeSeries and probabilistic reads (`JSON.GET`, `FT.SEARCH`, `FT.AGGREGATE`, `FT.INFO`, `TS.RANGE`, `BF.EXISTS` and the like) and `GRAPH.RO_QUERY`, `GRAPH.EXPLAIN`, `GRAPH.LIST`; the list is `MODULE_READS` in `src/live/cmdline.rs`. Before Redis 7.0, `COMMAND INFO` lists no subcommands and a container command's flags cover them all (6.2 flags `MEMORY` readonly, `MEMORY PURGE` included), so there only the subcommands known to read are sent: `MEMORY USAGE|STATS|DOCTOR|MALLOC-STATS`, `OBJECT ENCODING|FREQ|IDLETIME|REFCOUNT`, `XINFO STREAM|GROUPS|CONSUMERS` (and `HELP`); other container commands are not sent there.
- **`@dangerous` commands** ask first on every profile, readonly ones (`KEYS`) included, and `KEYS`, `SORT_RO` and `FT.TAGVALS` ask where the server gives no ACL categories (before 6.0, and module commands on Redis Stack 6.2 and 7.2) too: the panel shows the command and the profile, and sends it on `y` (`n` or `esc` cancel), or on a `prod` profile once the profile name is typed exactly and `enter` pressed. See [Write permissions](#write-permissions).
- **Refused commands:** commands that would change the panel's own connection are refused with a pointer to the right place, without reading their flags:
  - `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE` and the unsubscribes: use Pub/Sub;
  - `MONITOR`: use the Monitor panel;
  - `SELECT`: it would switch the database under every view; the reason points to `b`, the database list;
  - `AUTH`, `HELLO`, `RESET`, `QUIT`, `CLIENT REPLY`, `SETNAME`, `SETINFO`, `TRACKING`, `CACHING`, `NO-TOUCH` and `NO-EVICT`: refused;
  - `MULTI`, `EXEC`, `DISCARD`, `WATCH` and `UNWATCH`: a transaction or a watch left open would queue or abort every view's commands and redis-lens's own edits, so they are refused, writes on included;
  - `SYNC`, `PSYNC`, `READONLY`, `READWRITE` and `ASKING`: they would turn the connection into a replica link, switch its cluster mode, or set a one-shot flag that the next command of any view would meet;
  - `POST` and `HOST:`, which Redis flags readonly: they are its guard against HTTP sent to it, and close the connection.
- **Credentials (rule 1):** on screen and in the history, the panel masks, while the line is typed too:
  - the password arguments of `ACL SETUSER` (`>`, `<`, `#` and `!` rules);
  - `CONFIG SET requirepass`, `masterauth`, `masteruser`, Valkey's `primaryauth` and `primaryuser`, `tls-key-file-pass`, `tls-client-key-file-pass`, `sentinel-pass` and `sentinel-user`, and `SENTINEL SET … auth-pass|auth-user` and `SENTINEL CONFIG SET sentinel-pass|sentinel-user`;
  - `AUTH`, `HELLO … AUTH`, `MIGRATE … AUTH` and `AUTH2`.

  The history keeps the masked text, in memory only, for the session; `↑` brings back the masked text. Command lines and replies never go to snapshots, reports, logs, error text or `Debug` output.
- **Writes (P3):** write commands follow [Write permissions](#write-permissions): the app's question names the profile and shows the masked command, and the reply is drawn like a readonly command's; a cancel says "Cancelled: nothing was sent." Everything under "Not sent whatever their flags" and "Refused commands" stays refused with writes on: the write gate does not make a script safe.

### Values (P2)

- **Other values:** Monitor lines and Slow log arguments follow the rules for key values in [Values](#values): one renderer, on screen only, never in snapshots, reports, logs, error text or `Debug` output, and the clipboard only through `y` in the format shown. Built: both are drawn as one line written by the command masker (credentials as `***`, control and bidi characters as `\xHH`) and escaped; a Monitor line has its credentials masked as it arrives, so the password is never held. `y` on either copies that masked line ([Copy](#copy)).

### Write permissions

A write is sent only when all three checks allow it.

1. **The server.** The connected user can only run what its ACL allows.
   - A `NOPERM` turns that action off for the session, with the reason ("the server refused HSET for this user"), never the username.
   - The permission check uses `ACL DRYRUN` where it is allowed (Redis 7.0 and later, `@admin`) to show which writes the user may send. Otherwise writes show as "checked by the server on use".
   - A write `ACL DRYRUN` says is refused is off from the connect on, with the same reason. The Writes row on 0 Connections counts them ("2 refused by the server"). A write refused only for the probe's key or channel reads as "checked by the server on use", since the real key may be allowed.
2. **The profile.** `read_only` in the config file. Unset means read-only exactly when env is `prod`.
   - Change it with `w` on Connections, `redis-lens conn set <name> --writes on|off`, or `conn add --writes on|off`.
   - Turning writes on asks first. On a `prod` profile you must type the profile name: `conn set` reads it from the terminal, or `--confirm <name>` gives it, and `--yes` never replaces it. On other profiles it is `y`/`n`, and `--yes` (or `--confirm` with the name) skips the question. With no terminal and no answer given, it refuses and says which flag to pass.
   - It asks only when the profile is read-only now: `conn set --writes on` on a profile with writes on, and `conn add --writes on` on a profile that isn't `prod`, don't ask.
   - Turning writes off never asks.
   - Only `read_only` changes in the config file; nothing about credentials is touched.
3. **The action.** Each write asks according to the table. A setting in the config file, `[settings] skip_non_prod_confirm = true`, turns off the `y`/`n` prompts on profiles that aren't `prod`. It never applies to `prod`, to bulk actions, or to a prompt where you type a name.

| Action | Read-only profile | Writes on, not `prod` | Writes on, `prod` |
|---|---|---|---|
| Browse, view values, readonly commands | runs | runs | runs |
| Edit, add, set a TTL, rename, publish, ack, claim, create an index | not sent | runs | asks `y`/`n` |
| Delete a key, field or element, trim a stream, drop an index | not sent | asks `y`/`n` | type the key or index name (a destructive command typed on the command line: its key when it names exactly one, otherwise the profile name) |
| Bulk expire or delete by pattern | not sent | dry run, then `y`/`n` | dry run, then type the pattern |
| `@dangerous` commands (`FLUSHDB`, `CONFIG SET`, `CLIENT KILL`, `SLOWLOG RESET`, `KEYS` …) | readonly ones (`KEYS`) ask as in the next two columns; the rest are not sent | asks `y`/`n` | type the profile name |
| `MONITOR` | asks (`prod`: type the name) | asks `y`/`n` | type the profile name |

- **What counts as destructive or dangerous:** a write is destructive when it deletes or overwrites data:
  - `DEL`, `UNLINK`, `HDEL`, `HGETDEL`, `SREM`, `ZREM`, `LREM`, `XDEL`, `XTRIM`, `GETDEL`, `LTRIM`;
  - the pops, `LPOP`, `RPOP`, `LMPOP`, `SPOP`, `ZPOPMIN`, `ZPOPMAX`, `ZMPOP`, and `ZREMRANGEBYRANK|SCORE|LEX`;
  - commands that move a value or overwrite a destination: `RPOPLPUSH`, `LMOVE`, `SMOVE`, `MOVE`, `COPY`, `RESTORE`, `BITOP`, the `*STORE` commands (`SINTERSTORE`, `ZRANGESTORE`, `GEOSEARCHSTORE` …), and `SORT`, `GEORADIUS` and `GEORADIUSBYMEMBER` with `STORE`;
  - `EXPIRE`, `PEXPIRE`, `HEXPIRE` and `HPEXPIRE` with a time that is not positive (or not a number), and `GETEX` or `SET` with `EX`/`PX` not positive, which delete the key or field; `EXPIREAT`, `PEXPIREAT`, `HEXPIREAT`, `HPEXPIREAT`, and `GETEX` or `SET` with `EXAT`/`PXAT`, always (the time may be past);
  - `XGROUP DESTROY` and `XGROUP DELCONSUMER`, and the newer deletes `XDELEX`, `XACKDEL`, `VREM`, `DELIFEQ` and `DELEX`;
  - `FLUSHDB`, `FLUSHALL`;
  - `RENAME` onto an existing key;
  - `FT.DROPINDEX`;
  - and every command with the `@dangerous` ACL category, read from `COMMAND INFO`.
- **Each prompt** names the profile and shows the exact command it will send, as one line with credentials masked and control characters escaped. It says when a write can't be undone. `y` sends, `n` or `esc` cancels; a typed name is sent with `enter`, and a name that doesn't match sends nothing. Values in a prompt stay on screen only, like every value.

#### The confirm sheet

Every question that guards a write is one sheet, docked to the bottom of the body, full width, above the footer; the screen behind it is not dimmed, so the row acted on stays in view. It is drawn for the write gate's question (every op), turning writes on (`w`), a bulk action's final question, starting Monitor, and the command line's question (`@dangerous` or a write).
- **Lines:** the top border holds the question ("Delete this key?") and, at the right, `<profile> · ENV` (when the border is short the profile name is cut first, never the environment); then what it acts on; `Sends   <command>` (the command the write gate shows, credentials masked; on the command line, the line as the command line masks it, with the count when it repeats); whether it can be undone; then the answer line: ` y  <Verb>    n  Cancel`, or on `prod` "Type <name> and press enter" and the input below it. Its height is what it needs. The answer and the name to type are always whole: a name wider than the sheet wraps over as many rows as it needs. On a short body the lines above the answer are cut to one row each with `…` first, then left out from the end.
- **Keys:** `y` and `n`/`esc`, or the typed name, `enter`, `ctrl-u` (clear) and `esc`, on every sheet, Monitor's included. `enter` never answers a `y`/`n` question. Every key goes to the sheet first; no other key reaches the screen behind it, `ctrl-f`, `:`, `?` and `.` included. The footer shows the sheet's keys.
- **On top:** opening any question (the write gate's, writes on, a bulk action's, Monitor's, the command line's) closes the Actions menu and the `?` help, and the sheet is drawn last, over every other overlay: what takes the keys is what is on top.
- **A question a reply opened:** a question opened by something other than the key being pressed (a server reply, such as the `COMMAND INFO` a command line write waits for, or a bulk action's dry run) takes no answer for half a second (5 ticks): its answer line says `wait…` in place of its keys, the footer offers only `esc Cancel`, and the screen behind it is dimmed as for a centred dialog, so a key typed for something else cannot answer it. `n` (on a `y`/`n` question) and `esc` cancel at once. A question opened by the user's own key takes its answer at once.
- The sheet only draws: each question keeps its own state and keys. "Analyze the keys?" and the database list stay centred dialogs.
- **One gate:** every write goes through one write gate, which applies checks 2 and 3. Tests check that no write command reaches a read-only profile. In the code, a write is sent only with a `Permit`, which only the gate gives (`src/write.rs`), in the TUI and from the command line (`redis-lens expire`). Writes run one at a time over the session's connection, since `WATCH` holds for the whole connection.
- **Clusters (P4):** every write on a cluster goes through the same gate and is sent to its key's slot owner: key edits, `SPUBLISH` (to the shard channel's owner) and the command line's writes (by the keys their key specs find). Keyless writes (Clients' `d`, the Slow log's `e` and `D`, Search's index writes, `PUBLISH`) go to the picked node. Keys in different slots, and a write to a shard read on a replica, are refused before asking. Bulk actions and `redis-lens expire` run per primary. See [Cluster live views](#cluster-live-views-p4). The tests that no write reaches a read-only profile cover every node.

### Bulk actions

- **What they are (P3):** `t` sets a TTL on every key of a pattern, and `d` deletes them (on Browse › Keys; `T` and `D` on the pattern's key list). `redis-lens expire` does the first from the command line.
- **Dry run:** a `SCAN MATCH` counts the matching keys that infer to the pattern, and how many already have a TTL. Nothing is written until you confirm, and when nothing would change it says so without asking. The run walks the keyspace again with the same `SCAN MATCH`, so keys created or removed since the dry run are handled as they are then: the dry run's counts are a forecast.
- **Where:** per primary on a cluster ([Cluster live views](#cluster-live-views-p4)). Off on a replica target, on a cluster read on replicas, and on a cluster with a primary that discovery could not connect to, with the reason, in the TUI and from the command line.
- **Expire:**
  - It sends `EXPIRE key seconds NX` (Redis 7.0 and later; on Redis 6.2, bulk expire is off with a reason).
  - Only keys without a TTL change.
  - The default TTL is 24 h, and the prompt can change it.
- **Delete:** it sends `UNLINK`.
- **Running it:**
  - In pipelines of 500, within the command budget, with progress shown. `esc` stops after the current batch. Turning writes off with `w` stops it before the next batch, and so does a refused command.
  - Each batch's key names are appended to `<data dir>/redis-lens/bulk/<profile dir>/<YYYYMMDDTHHMMSSZ>-<expire|delete>.txt` and flushed to disk before the batch is sent. The file is created by the first batch with keys (directories 0700, the file 0600, never over an existing file: `-2`, `-3` … are added to the name). For an expire it lists only the keys sent `EXPIRE … NX`, and `PERSIST` on those keys undoes it.
  - The file holds key names only, one a line, escaped: `\` as `\\`, newline, CR and tab as `\n`, `\r` and `\t`, and any other control character and U+2028/U+2029 as `\u{XXXX}`, so a name can't start a new line.
- **From the command line:**
  ```
  redis-lens expire <name> <pattern> --ttl DURATION [--apply] [--yes | --confirm PATTERN] [--max-rate N]
  ```
  Without `--apply` it is a dry run. With `--apply`, a `prod` profile needs `--confirm` with the exact pattern (it is never asked on the terminal, and `--yes` never replaces it), and other profiles ask `y`/`n`, or need `--yes` when there is no terminal. `--yes` and `--confirm` need `--apply`. A read-only profile refuses with exit code 2, dry run included. It connects to the profile's own endpoint, never a replica.

### Topology and connections (P4)

Phase 4 closes the topology and connection gaps against Redis Insight: the live views on a cluster, Sentinel, a database switch, a per-slot view, and a few small items. Each part lands with its key table rows and `src/tui/keymap.rs` together; until a part is built, the text below is a plan.

**New keys (P4):**
- `b` everywhere (once connected): the database list ([Database switch](#database-switch-p4)). In the add flow and every text input it is text.
- `[` `]` on 1 Overview, the Server panels (Clients, Slow log, Monitor), 5 Pub/Sub and Browse › Search: pick the previous or next node (cluster only). They are only in the `?` help, so the footers still fit 80 columns.
- `S` on 5 Pub/Sub: subscribe to a shard channel.
- `ctrl-t` on the key list: filter by type.
- `/` and `esc` on 0 Connections: filter by name or tag, and clear it.
- Analyze › Slots: the move keys, `tab` `shift-tab`, `s` (order the hottest slots) and `r` (read again). Its footer: the move keys, `tab`, `s` and `r`, then `. Actions   ctrl-f Find key   ? Keys`.

#### Cluster live views (P4)

Built. On a cluster, the views that read one server work node by node. Nothing is merged on the server side: redis-lens keeps its own connection to each node, routes each command itself, and never uses redis-rs's cluster client.

- **Connections kept.** A cluster connect keeps the connection discovery opened to each shard (the primary, or the verified replica with `prefer_replica`) for the live views, as a standalone connect keeps its scan connection. Each node keeps the command budget the scan gives it ([Cluster](#cluster), step 5), shared by the scan and that node's live requests: the budget applies **per node**, never one budget for the whole cluster. A node that failed discovery has no connection, and every view says so when a request would go to it ("127.0.0.1:7002 is not connected (discovery could not use it), so nothing is sent to it"). That covers the two things that open a connection of their own, Monitor and the Pub/Sub tail (classic, on the picked node; sharded, on the channel's slot owner): they are refused with the same sentence and connect to nothing, so the profile's credentials never go to a node discovery refused (a loopback address announced by a remote cluster) or could not reach. The check is made twice, when the key is pressed and again where the connection would be opened, and discovery marks such a node as one credentials never go to. A node whose connection drops answers every later request with "the connection to 127.0.0.1:7002 was lost; rescan to connect again" (`a` analyzes again, which connects again) while the other nodes go on; the live connection only ends when every node is lost. `Msg::Nodes`, sent right after `Msg::Connected`, carries the node list (ID, address as announced, role, slot ranges), so the views know the nodes before the scan finishes.
- **Probes per node.** The view probes and the `ACL DRYRUN` probes ([Permission check, additions](#permission-check-additions)) run on every connected node, each through that node's budget. A view or a write counts as allowed only when every node allows it; a refusal names no node and no user.
- **Routing.**
  - A command with keys goes to the node that owns the key's slot, from the slot map discovery read. The slot is computed locally (CRC16 of the key, or of its `{hash tag}`, modulo 16384), never asked with `CLUSTER KEYSLOT`.
  - A keyless command goes to the **picked node** (below).
  - A request whose keys hash to different slots is split by node where each part stands alone (key metadata on the key list and Big keys, stream infos); otherwise it is refused before anything is sent ("the keys are in different slots (CROSSSLOT), so it can't be sent as one command").
  - **Redirects:** a `MOVED` reply to a node redis-lens holds a connection to is followed once: the request is sent again on that node and the slot map remembers the move for the session. `ASK` is followed once with `ASKING` sent in the same pipeline as the command. Every command sent to a node takes from that node's budget first, an `ASKING` included. A redirect to an address redis-lens has no connection to is not followed: no new node is ever connected to after discovery, so credentials only go where discovery already sent them. A second redirect, or one to an unknown address, fails with a fixed reason ("the cluster is moving that slot; try again, or rescan"). A write sent with `WATCH` and `MULTI`/`EXEC` is never followed on `ASK`, and on `MOVED` only when the `WATCH` itself was redirected (nothing was queued yet).
  - **Replicas:** on a shard scanned on its replica (`prefer_replica`), reads go to the replica (it is in `READONLY` mode). Writes, the bulk actions and the command line's writes are off for that shard, with the reason "this shard was read on a replica; connect without prefer_replica to write". No primary connection is opened for them.
- **The picked node.** Overview's live figures, Clients, Slow log, Monitor, Search, classic Pub/Sub and the command line's keyless commands read one node at a time: the picked node, by default the first primary in slot order. `[` and `]` pick the previous and next node, from any of those views; the pick is one per session and lasts across screens and a new analysis whose node list still has that node (same ID and address), else it goes back to the first primary. Each of these views shows it first at the right of its title, notices included: `node 127.0.0.1:7001 · 1 of 3`. A new pick restarts the shown view's polling on the new node, clears Overview's live figures and Clients, empties the Slow log (its key specs kept) and resets Browse › Search to its index list. With fewer than two nodes, and on a standalone server, `[` and `]` do nothing and the title is as before.
- **A pattern's keys.** `SCAN MATCH` on every shard's node in slot order, or only on the node Browse › Keys is narrowed to ([2 Browse › Keys](#2-browse--keys)). The nodes are walked one at a time: a page of 1,000 keys reads the current node until its cursor returns to 0, then goes on to the next, so `m` never sends to more than one node at once. The title starts with the nodes done (`2 of 3 nodes`), so it is still on screen when the title is cut short at 80 columns. When the list is 60 cells wide or more, or with `v`, a NODE column after the key shows the node each key came from (cut with `…` to 15 cells), as on Hot keys; the right pane and a key's value name it in full. The metadata pipeline runs on the same node as the `SCAN` that found the keys. A node that discovery could not connect to is left out of the walk, and the right pane says so ("10.0.1.13:6379 is not connected (discovery could not use it), so its keys are not listed").
- **A key's value and its edits.** Sent to the slot owner of the key, with the redirect rule above. Writes still go through the one gate in `src/write.rs`. `R` (rename, `RENAMENX`) needs both names in one slot; across slots it is refused before asking, with the CROSSSLOT reason. `+` (add a key) sends to the new name's slot owner.
- **Big keys.** Length and encoding are read on each key's slot owner (they were left out on a cluster).
- **1 Overview and 4 Server.**
  - Overview's live figures ("Now"), Clients, Slow log and Monitor read the picked node. Overview's sparklines start again when the pick changes. Clients' `d` ends a connection on the picked node, and the Monitor run stays on the node it started on (its title names it) until it ends.
  - **Per-shard rows (Overview):** below the figures in "Now", one row per shard in slot order for the node the session reads there (the primary, or the verified replica with `prefer_replica`), the picked one marked `›`: its slots, node address, role, ops/s (`instantaneous_ops_per_sec`), used memory and keys (`INFO keyspace` db0), and "✗ not connected" or "✗ no answer" for a node without a connection or one that did not answer. It is the one place a poll goes to every node: one `INFO` per shard node, every 5 s or at the Overview's interval if that is longer, only while the Overview is on screen and polling is not paused, each from its own node's budget. It is never shown on a standalone server.
- **Browse › Streams.** The streams the analysis found, each read on its slot owner (`XINFO`, `XPENDING`, the entries, ack, claim and trim). One poll sends one request per node that owns a listed stream, each from that node's budget, with the 50-stream cap over the whole list. A NODE column shows each stream's node. A node whose answer fails keeps its streams' last rows; a request for one stream follows a redirect, one for several does not.
- **5 Pub/Sub.**
  - **Classic channels** are read from the picked node: on a cluster a message published on any node reaches every node's subscribers, but `PUBSUB CHANNELS` and `NUMSUB` count only that node's own subscribers, and the title says so (`subscribers on 127.0.0.1:7001`). The tail (`SUBSCRIBE`, `PSUBSCRIBE`) runs on the picked node.
  - **Shard channels** (Redis 7.0 and later, and Valkey; on a standalone server too): a second list under the classic one, from `PUBSUB SHARDCHANNELS` (the first 1,000) and `PUBSUB SHARDNUMSUB` in batches of 100. On a cluster they are read from every primary on each counts poll (every 5 s, only while Pub/Sub is on screen), each from its own node's budget, and each row shows its node. `S` subscribes to the selected shard channel, or asks for one, with `SSUBSCRIBE` on its slot owner; a pattern can't be shard-subscribed, and the prompt says so and stays open. `enter` on a shard channel row starts a shard tail too. `P` on a shard channel publishes with `SPUBLISH` to its slot owner, through the write gate like `PUBLISH`, and says how many subscribers of that node got it. On Redis 6.2 the shard list is left out with a reason.
- **Browse › Search** reads the picked node: an index on a cluster without the Search coordinator only covers that node's keys, and the title names it as above (`node 127.0.0.1:7001 · 1 of 3`).
- **Bulk actions and `redis-lens expire`.** Per primary, in slot order, one node at a time: the dry run counts on every primary (and says how many keys each holds), the run walks each primary's own `SCAN MATCH` with that node's budget. Off on a cluster read on replicas (`prefer_replica`), with the reason, as on a standalone replica target, and when a primary has no connection (its keys could not be walked). The confirm sheet lists each primary's count. Redirects are not followed in a bulk run: a key that moved during the run is left alone and counted ("12 keys moved to another node during the run and were left alone"). On a cluster each line of the key file is the node's address as announced, a tab, then the key name escaped as before (a tab in a name is already `\t`, so the line splits at the first tab). `redis-lens expire` on a cluster discovers the shards and runs on the primaries, never on replicas. Both are refused, with a fixed reason and before anything is sent, when the cluster's slot map could not be read in full (`CLUSTER SHARDS` and `CLUSTER SLOTS` both refused, so only the seed is known; or the connected primaries together cover fewer than all 16,384 slots): the run would skip the keys on the other primaries. `redis-lens expire` also stops when `INFO cluster` can't be read on a server that `INFO server` does not call a cluster, rather than treating it as one server.
- **Command line.**
  - A command's keys are found from the key specs in its `COMMAND INFO` reply (before Redis 7.0, its first/last/step key positions), the same as the Slow log. A command with keys goes to its slot's owner; keyless commands go to the picked node. The reply is headed by the node it was sent to (`127.0.0.1:7002>`); a `MOVED` followed on the way is not shown there.
  - A command whose keys hash to different slots is refused with the CROSSSLOT reason, without sending it or asking first. Where the key specs find only some keys (Redis 6.2 for `ZUNIONSTORE`, `ZINTERSTORE`, `ZDIFFSTORE` and `GEORADIUS`/`GEORADIUSBYMEMBER ... STORE`; `SORT ... STORE` on every version), redis-lens also looks for the rest where the command puts them (the keys after `numkeys`, and the `STORE`/`STOREDIST` destination) before checking the slots. Any other command with keys the specs may miss still goes to the picked node, with the note. A command whose keys can't be found (moving keys that need `COMMAND GETKEYS`, which is never sent) goes to the picked node and the panel says so.
  - Every Phase 2 and 3 refusal holds on every node: scripts and functions, unlisted module commands, `POST`/`HOST:`, the connection-changing commands (`READONLY`, `READWRITE` and `ASKING` included), `SELECT`, and blocking commands.
  - `CLUSTER` subcommands that read (`CLUSTER INFO`, `NODES`, `SHARDS`, `SLOTS`, `MYID`, `KEYSLOT`, `COUNTKEYSINSLOT`, `GETKEYSINSLOT`) are judged by their flags as any other command; the rest are writes or `@dangerous` and go through the gate, on the picked node.
- **Still off on a cluster:** nothing in the views above. The database switch is off, since a cluster only has database 0 ([Database switch](#database-switch-p4)).

#### Sentinel (P4)

Built. A profile can point at Sentinels instead of a server. redis-lens asks a Sentinel where the primary is, then connects to it as it would to any server, with the profile's TLS, SSH route, credentials and database.

- **Config:** a `[profile.sentinel]` table: `master` (the name the Sentinels monitor), `nodes` (one or more `host:port`, IPv6 in brackets; port 26379 when left out) and `tls` (whether the Sentinels themselves speak TLS; absent when false). The profile's `endpoint` holds the primary's address as given to `conn add` and is only shown until the first connect; redis-lens never connects to it without asking a Sentinel first, so a failover can't send it to an old primary (a database other than 0 included: the cluster check on database 0 is never made for a Sentinel profile). A node that can't be read fails loading the config file.
- **Credentials:** a Sentinel's own username and password (Sentinel's `requirepass` or ACL users) are kept apart from the data node's, in the OS keychain only, account `sentinel:profile:<name>` (rule 1). Never in the config file, logs, error text, `Debug` output, snapshots, export files or on screen. Without that entry the Sentinels are asked without credentials. The Sentinels never get the data node's credentials, and the data node never gets the Sentinels'. A Sentinel connection sends no `CLIENT SETINFO` and no `SELECT`. `conn remove` deletes the entry as it does the other entries; `conn add` (with or without `--force`, whether or not the old profile had a Sentinel) deletes it whenever the saved profile has no Sentinel credentials, so a leftover entry from an earlier profile of the same name is never used.
- **Resolving the primary:** at every connect (each analysis connects again), `conn test`, Pub/Sub tail and Monitor run, the Sentinels are asked in the configured order, each within 2 s for connecting and the answer (behind SSH, each Sentinel's forward is started first, outside the 2 s): `SENTINEL GET-MASTER-ADDR-BY-NAME <master>`. The first Sentinel that gives an address wins; one that is unreachable, refuses, or doesn't know the name is skipped and the next is asked. When none gives an address the connect fails with a fixed reason: "no Sentinel knew the primary <master>" when any Sentinel answered that it doesn't know the name; otherwise "every Sentinel refused redis-lens (check the Sentinel credentials)" when any refused (`WRONGPASS`, `NOAUTH`, `NOPERM`); otherwise "no Sentinel could be reached". The answer's address follows the rules for announced addresses ([Cluster](#cluster), step 2: no `?`, unspecified, numeric-last-label hosts or port 0). The first answer is used even when it breaks them: the connect fails with "the address Sentinel gave can't be used: <reason>" (the host, or the port), and the next Sentinel is not asked. A usable address is resolved once (or forwarded once through SSH) and pinned.
- **Checking the answer:** the primary is connected to with the profile's credentials and must say `role:master` in `INFO replication`; otherwise the connect fails: "the address Sentinel gave is not a primary (a failover may be running); try again". A primary that refuses `INFO replication` fails it too, closed: "the primary refused INFO replication, so redis-lens can't check it is a primary". The loopback rule for announced addresses applies, judged against the Sentinel that answered: the data node's credentials go to the resolved primary only when it is not on this machine, or that Sentinel is on this machine too (behind SSH, judged from the bastion as for replicas); otherwise the primary is not connected to: "the primary Sentinel gave (<address>) is on this machine and the Sentinel that answered is not, so the profile's credentials are not sent to it". A primary that refuses the credentials is a fixed error: "the primary Sentinel gave (<address>) refused the profile's credentials". Any other failure to connect to it, or a server error other than a refusal, is a fixed error too: "the primary Sentinel gave (<address>) could not be connected to or answered an error" (never the server's text, which can name the ACL user). The `INFO server` read to tell a cluster apart answers the scan's own first `INFO server`, as for a cluster seed.
- **`prefer_replica`:** `SENTINEL REPLICAS <master>`, asked on the Sentinel that answered (within 2 s), gives the candidates, in the order the Sentinel lists them; a replica whose `flags` hold `s_down`, `o_down` or `disconnected`, or whose `master-link-status` is not `ok`, or whose address breaks the announced-address rules, is skipped. The first usable one is verified exactly like a standalone replica ([Safety](#safety): `role:slave` and the primary's `master_replid`, no credentials first, the loopback rule judged against the resolved primary), with the same 2 s timeout and the same trust note: the Sentinel's view of its replicas is trusted the way `INFO replication` is. Otherwise the scan runs on the primary with `PrimaryFallback` and the reason: "NOPERM SENTINEL REPLICAS is not allowed for this user", "the Sentinel did not list the replicas", "the Sentinel lists no replica that is up", or the standalone replica's reasons.
- **A Sentinel profile whose primary is a cluster node** is refused ("Sentinel and cluster can't be combined").
- **Monitor:** reached directly, the monitor connection goes to the address checked; when Sentinel names a TLS primary by host name, Monitor is off for that run ("MONITOR is not available yet when Sentinel names a TLS primary by host name."), since redis-rs would look the name up again; this is decided before any `MONITOR` is sent or budget is taken. The monitor connection goes to the address the Sentinel named (never the profile's configured endpoint).
- **Adding:** `conn add <name> --url <url> --sentinel HOST:PORT[,HOST:PORT…] --sentinel-master NAME [--sentinel-tls] [--sentinel-user USER] [--sentinel-password-stdin | --sentinel-ask-password]`. The URL gives the data node's username, password, TLS and database as usual. `--sentinel` and `--sentinel-master` need each other, and the other `--sentinel-*` flags need `--sentinel`. `--sentinel-password-stdin` and `--password-stdin` can't both read standard input. A Sentinel password given without `--sentinel-user` is a password-only Sentinel (`requirepass`); `--sentinel-user` without a Sentinel password is an error. `--sentinel-ask-password` first prints a line saying it asks for the Sentinels' own password. A Sentinel listed twice is kept once. The output says `Asks Sentinel for <master> via <first host:port> (+N) [(TLS)]: the primary is found at every connect.`, and that the Sentinel credentials are in the OS keychain when there are some. The keychain entry is written before the config file and put back as it was if the save fails.
- **Showing it:** `conn list` adds a `SENTINEL` column (`<master> via <first host:port> (+N)`) when any profile has one. `conn test` says `Connected to <name> (<resolved primary>, <ENV>).` and prints `Sentinel: <host:port> gave <master> at <address>.` before the checks. The Connections details have a Sentinel row (and then no Values row): `<master> · asks <first host:port> (+N)` until the first connect, then `<master> · named by <host:port>` (the Sentinel that answered). The Address row and the connection list show the resolved primary (the configured address until then). The header shows the resolved primary.
- **ACL:** a minimal Sentinel user needs `+ping +hello +auth +sentinel|get-master-addr-by-name`, and `+sentinel|replicas` for `prefer_replica`.

#### Database switch (P4)

**Built.**

- `b` (on every view where the connection is ready, and a text input takes it as text) opens a list of databases: the current one, and every `dbN` line of `INFO keyspace`, read fresh when the list opens (one command from the budget, nothing polls it), with its keys and expires; the current database is always listed, with zeros when it holds no key, and the selection starts on it. `↑↓` move, `enter` switches (on the current database it only closes the list), `s` switches and saves the database as the profile's `db` in the config file (saved profiles only; it asks `y`/`n` first, `n` or `esc` going back to the list with nothing done, and a failed save says so), `esc` closes. If `INFO` is refused (`NOPERM`) or fails, the list shows the current database only, with a fixed reason ("The server refused INFO for this user: only the current database is listed."), never the server's text. Without a saved profile (a Docker container) `s` says so ("A container has no saved profile, so nothing can be saved.") and does nothing else; `enter` still switches, for the session. `s` on the current database saves it without connecting again.
- **Switching** connects the session again on the other database and reads its figures, as a connect does ([What a connect reads](#what-a-connect-reads)), but never samples keys, even when the session had an analysis: keys are sampled only on `a`. The earlier database's results are not shown for the new one; Overview offers `a` ("No analysis yet" on the Analyze panels), and Browse › Keys reads the new database's key names. A later `a` stays on that database. Every live connection moves with it: the views' live connection is opened again on the new index, a running Pub/Sub tail subscribes again to the same channel or pattern once the new run's live connection is open (Pub/Sub is not per database, so no message is lost to the switch other than those that arrive while it reconnects), and a running Monitor stops, since starting it again asks first. The header shows `db N` whenever the database is not 0. With `s`, the switch is not held back by the save: a failed save says so ("Could not save db N for <profile>: …. The switch still applies to this session.") and the switch stands.
- The switch lasts for the session: the session keeps its own copy of the profile with the new `db`, a new connect uses the profile's saved `db` again, and the profile is never changed unless `s` is used and answered `y`; only `db` is written, and no credential.
- **Off:** on a cluster ("A cluster only has database 0."), before the connection is ready ("The connection is not ready yet…", also when nothing is connected, or the live connection is closed), and while an analysis runs ("An analysis is running: wait for it to finish, then press b."). Each says so in the status line and sends nothing. `SELECT` stays refused on the command line, and its reason points to `b`.

#### Analyze › Slots (P4)

Built. A panel of 3 Analyze, after Nodes, shown only on a cluster. One frame of three sections: the slot ranges, the hottest slots, and the selected slot's details. It shows what each slot range holds, from `CLUSTER SLOT-STATS` (Redis 8.2 and later, Valkey 8.0 and later; each node's version from its `INFO`).

- **Commands:** when the panel is first shown and on `r`, never polled: on every primary, one `CLUSTER SLOT-STATS SLOTSRANGE <start> <end>` per slot range it owns, each from that node's budget, at most 4 nodes at once. Each node's request starts with one `INFO server` (the version; nothing in the session knows it before the scan ends, and the panel does not wait for the scan), and a node whose version has no `CLUSTER SLOT-STATS` is not sent it. A primary scanned on a replica is asked on that replica. A node discovery could not connect to is not asked and shows `✗ not connected`. A new analysis or connect drops what was read; the panel reads again when it is shown. Nothing else is sent; the per-slot CPU and network figures need `cluster-slot-stats-enabled yes`, which redis-lens never sets.
- **Ranges:** one row per node's slot range, in slot order: range, node and keys; CPU (µs), network in and out, and memory (Redis only, `memory-bytes`) when the section is 90 cells wide or more, or with `v`. A metric the reply doesn't carry shows `—`; when every CPU and network figure is 0 the title says "CPU and network need cluster-slot-stats-enabled yes".
- **Hottest slots:** below, the 20 slots with the most keys, or by CPU, network in or network out (a slot whose figure is 0 or missing is left out): `s` cycles the order (keys → CPU → net in → net out → keys), and the move keys move through this list. Each row: slot, node, keys, CPU, network in and out. The detail panel lists the scan's example, big and hot keys that hash to the selected slot, with their patterns (computed locally, no command).
- **Off** with a reason: "Not a cluster: this is one standalone server."; "CLUSTER SLOT-STATS needs Redis 8.2 or Valkey 8.0; <node> runs <Redis|Valkey> <version>" (the whole panel is off when any primary is older; nodes not yet asked are then not asked); "The server refused CLUSTER SLOT-STATS for this user." (any node's `NOPERM`); a node that fails shows `✗` on its ranges and the others still show. It waits for the connection, not for the scan.
- Nothing is copied on this panel (`y` does nothing).

#### Small items (P4)

Built.

- **Type filter on the key list:** `ctrl-t` cycles the types shown: all → string → hash → list → set → zset → stream → other → all, over the loaded keys, with the name filter. The title says `type <t>` while one is set, and a type with no loaded key says so. It sends nothing. `esc` clears the name filter and the type together, and only then goes back to the patterns. The key is in the `?` help, not on the footer.
- **Connection export and import:**
  - `redis-lens conn export [FILE]` writes the saved profiles as TOML `[[profile]]` tables (standard output without `FILE`), exactly the config file's profile fields. It never reads the keychain, and never writes a credential: there is none in a profile, and the Sentinel and SSH usernames stay in the keychain. A file is written atomically (a temporary file in the same directory, then a rename) with mode 0600, and never over an existing file without `--force`. With no saved profile it fails ("no saved connections to export").
  - `redis-lens conn import FILE [--force] [--yes]` adds every profile of the file. A name that exists is refused unless `--force` replaces it; names with the `docker:` prefix are refused. Nothing is imported if any profile is refused: the error lists every refusal. Also refused: a name twice in the file or with control characters, a tag that is not valid, an SSH bastion host that is not valid, a token command with no program. Also refused: a Sentinel table with no node, or a master name that is empty or has spaces. `[settings]` and any other table in the file are ignored. Nothing is stored in the keychain. But an imported profile must not pick up credentials stored under its name for another place (a file could point `prod` at another host and be sent prod's password at the next connect): the keychain entries under an imported name (the server's, the bastion's, the Sentinels') are deleted, unless the profile replaces one with the same address and TLS, kind of auth, token command, bastion and Sentinels. This covers a name new to the config file whose entries were left behind. The output names each profile whose stored credentials were removed; a refused import or a failed save leaves the keychain as it was. To keep an import from turning writes on, `read_only = false` is dropped (the env's default applies) and the output says so; `read_only = true` is kept. A profile with a token command shows its command and asks first, as `conn add` does (`--yes` skips; with no terminal it refuses). Imported profiles have no credentials: the output lists the profiles that need `conn add <name> --force …` to store them, those with auth `keychain` or `command`, those behind an SSH bastion (the username may be needed) and those that ask Sentinels (the Sentinels' password may be needed). Names from the file are escaped in everything printed.
- **Tags on connections:** `tags = ["team-a", "eu"]` in a profile (absent when empty). `conn add --tag TAG` (repeatable) and `conn set <name> --tag TAG | --untag TAG` change them; a tag is 1 to 32 characters of letters, digits, `-`, `_`, `.` and `/`, matched as typed (case counts), and a tag named twice is kept once. `conn set` with none of `--writes`, `--tag`, `--untag`, `--standalone` and `--no-standalone` is a usage error; removing a tag the profile lacks, or adding and removing one in the same call, is refused with nothing changed; `--writes` and the tags are saved together or not at all. `conn list` adds a TAGS column when any profile has one. On 0 Connections the details show them in a Tags row (a profile with tags has no Values row), and `/` filters the list by name or tag (a part of either, ignoring case; Docker containers by name), `esc` clears. Inside the filter input every key is text, `enter` applies it, the input shows in the footer, and the panel title counts what it shows (`2 of 5 saved · 0 of 1 in Docker`). `/` is on the Connections footer.
- **Force standalone (P5):** `force_standalone = true` in a profile (absent when off). `conn add --standalone` and `conn set <name> --standalone | --no-standalone` change it. It is refused for a Sentinel profile ("--standalone conflicts with a Sentinel profile: Sentinel already names one primary"), in `conn import` too, and a hand-edited file that has both still loads (so `conn set --no-standalone` and `conn remove` can fix it) but is refused with the same line at every connect, before anything is resolved or sent. `conn list` adds a MODE column when any profile has it, and `conn export` and `conn import` carry it. What it changes is under [Cluster](#cluster).
- **Theme and `NO_COLOR`:** `[settings] theme = "dark"` (the default) or `"light"`, a palette for light terminals with the same roles (dim, warn, error, ok, selection, focus, placeholder). Focus (`#6cb6ff` dark, `#0b5cad` light) marks where the keys go: the focused pane's border and title, the current panel, the `›` on the selected row, the input cursor, the key hints in help and dialogs, and the command line's border. Placeholder (`#b79cf5` dark, `#6f42c1` light) colours the `{placeholders}` in inferred patterns. Amber is kept for warnings, `▲`, `read-only`, notices and progress. When the `NO_COLOR` environment variable is set and not empty, no colour is drawn at all: selection, badges and the active tab use reverse video and bold instead, focus is bold only, and every state keeps its words (`read-only`, `✗ failed`), so nothing is told by colour alone. `NO_COLOR` wins over `theme`. The mode is read once when the TUI starts (an unknown `theme` value is a config error); it applies to the TUI.

### Permission check, additions

- **P1 probes:** `SLOWLOG LEN`, `CLIENT LIST ID 9223372036854775807` (an ID no client has, so it lists nothing), `XINFO HELP`, `PUBSUB NUMPAT`, `FT._LIST`. A refused probe turns its view off, never the connection: only the checks in [Permission check](#permission-check) make it limited. A missing Search module (an unknown `FT._LIST`) reads as skipped, not refused.
- **P2 probes:** `COMMAND DOCS GET`.
- **P3 probes:** `ACL DRYRUN`, where it is allowed.
- **P4 probes:** on a cluster, all of the above on every connected node, each through its node's budget; `PUBSUB SHARDNUMSUB` (no channel) on Redis 7.0 and later and Valkey for the shard channel list; no probe for the Slots panel: its refusal (`NOPERM`) is read from the panel's first request, which is read-only and sent only when the panel is shown.
- **Turning off:** each missing permission turns off its view or panel, with the reason (rule 3).
- **ACL user:** the minimal read-only user gains `+slowlog|get +slowlog|len +client|list +xinfo +pubsub|channels +pubsub|numsub +pubsub|numpat +command|docs +command|info +time +object|encoding +strlen +hlen +llen +scard +zcard +xlen`. Stream entries need `+type +xrevrange`, both in the `+@read` below. Give `+ft._list +ft.info`, and for queries `+ft.search +ft.explain +ft.profile`, only on a server with the Search module: a server without it refuses the whole rule, and an ACL file with it does not load. `+config|get` shows the slow log's threshold, but it also lets the user read every setting, `requirepass` and `masterauth` included, so it is left out: without it the panel says the threshold is unknown. For P2 it also needs the read commands for values: `+@read`, and `-keys` to block `KEYS`. The Pub/Sub live tail needs `+subscribe +psubscribe` and channels it may subscribe to: `&*` or `allchannels` in place of `resetchannels`, or `&<pattern>` for some (a `PSUBSCRIBE` pattern is checked against them as it is written). Module values (arrays and the time series, Bloom, Cuckoo, Count-Min Sketch, Top-K and t-digest keys) need no rule of their own: every command they read is in `+@read` on Redis 8 (checked on 8.10.1), which P2 already has. They are not named one by one, because a server without the module refuses the rule. The Monitor panel needs `+monitor`; it is left out of the minimal user, because every command the server runs, other users' values included, goes to whoever runs `MONITOR`.

### Server versions

Features that need a newer server are turned off with a reason on older ones:
- Redis 7.0: `EXPIRE NX`, `COMMAND DOCS`, consumer group lag, `ACL DRYRUN`
- Redis 7.2: `lib-name` in `CLIENT LIST`, consumer inactive time
- Redis 7.4 and Valkey 9.0: `HTTL`
- Redis 8.8: arrays (`ARSCAN`)
- P4: Redis 7.0 and Valkey: sharded Pub/Sub (`SSUBSCRIBE`, `PUBSUB SHARDCHANNELS|SHARDNUMSUB`, `SPUBLISH`); Redis 8.2 and Valkey 8.0: `CLUSTER SLOT-STATS` (`memory-bytes` on Redis only)

Redis 6.2 stays tested: it runs everything else.

## Web dashboard

An alternative to the TUI in a web browser, served by the same redis-lens process, with the same features and the same rules. It is being built in phases: phase 1 is Connections, Overview, Server, Pub/Sub and the command line; phase 2 adds Browse, Analyze, snapshots and the database switch; phase 3 adds the edits and bulk actions; phase 4 adds Browse › Streams, Browse › Search and Hot keys from Monitor, so the dashboard now has every screen of the TUI. The routes and their replies are in [docs/dashboard-api.md](docs/dashboard-api.md).

### Starting it

- `redis-lens` (or `redis-lens tui`) starts the TUI and serves the dashboard beside it on `127.0.0.1:7379`. The Connections screen shows `Dashboard  http://127.0.0.1:7379  ·  ctrl-o opens it` under the frame, and `ctrl-o` opens it in the browser, signed in. Its notices: "Opened the dashboard in your browser.", "Could not open a browser for the dashboard.", "The dashboard is off (--terminal).", "The dashboard is off (dashboard = false in the config file)." or why it did not start. Quitting the TUI stops the dashboard.
- `redis-lens --dashboard` serves only the dashboard: it prints `Dashboard: <sign-in URL>` and `Press ctrl-c to stop.`, opens the browser unless `--no-open` is given, runs until ctrl-c and exits 0.
- `redis-lens --terminal` runs only the TUI; nothing is served.
- `--port N` (1 to 65535) picks the port. `--terminal` conflicts with `--dashboard` and `--port`, and `--no-open` needs `--dashboard`. With any subcommand but `tui` these flags are a usage error.
- `[settings]` in the config file: `dashboard = false` never starts it beside the TUI (`--dashboard` still does), and `dashboard_port = N` sets the port. Flags win. Each key is absent from the file while at its default. A config file that cannot be read does not start it beside the TUI either, since the file may say not to: "The dashboard is off: the config file could not be read, so it may say not to start it. Fix the file, then start redis-lens again."
- A port in use: with `--dashboard` it is an error, "Port N on 127.0.0.1 is in use, so the dashboard did not start. Pick another with --port."; beside the TUI the TUI carries on and says once "The dashboard is off: port N is in use. --port picks another."
- While the TUI runs the dashboard never writes to the terminal.

### Who can open it

- It listens on `127.0.0.1` only. There is no option to listen anywhere else.
- Each run makes a session token: 32 random bytes, in hex. It is shown in one place, the `Dashboard:` line of `--dashboard`; never in the TUI, a notice, a log, an error or `Debug` output.
- The sign-in URL is `http://127.0.0.1:<port>/#token=<token>`. The token is in the fragment, which a browser never sends to a server or in a `Referer`. The page takes it from there, removes it from the address bar, keeps it for that tab, and sends it as `Authorization: Bearer <token>` with every `/api/*` request. There is no cookie, so the browser never sends the token to another port or site by itself.
- Opening the browser (`ctrl-o`, `--dashboard`) never passes the URL as a program argument, where other users of the computer could read it: redis-lens writes a small page that redirects to it into a directory only the user can read (0700, the file 0600, random names), opens that file, and deletes it after 30 seconds and when it stops.
- `/` and the page's own files hold no data and need no token. Every `/api/*` route, the event stream included, answers 401 without the token, compared in constant time; the page then says to open the dashboard from redis-lens.
- Every request's `Host` must be `127.0.0.1:<port>` or `localhost:<port>`, so a name that resolves to this computer is refused (403), and so is a request line that names another host. Every request that is not a `GET` or `HEAD` must also carry an `Origin` of the dashboard itself, the header `X-Redis-Lens: 1` and a JSON body, so a page on another site cannot send one.
- Every response carries `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY` and a Content-Security-Policy that allows scripts, styles and requests from the dashboard itself only.
- The page is plain HTML, CSS and JavaScript inside the binary. It asks no other host for anything, fonts included, has no inline script or style, and draws everything it gets from a server as text, never as markup.
- The server logs no request, body or reply. A request's headers must arrive within 10 seconds, a body holds at most 2 MiB, and the number of open connections is capped.

### The same rules as the TUI

- **Credentials (rule 1).** Passwords and usernames never reach the browser. Adding a connection sends the password once, in the request's body, straight to the OS keychain through `SecretStore`; it is never sent back, logged or written to the config file. Addresses are shown masked, and slow log, Monitor and command-line text is masked as the TUI masks it. A password waiting on a question (a token command, writes on `prod`) is held in memory for at most two minutes.
- **Values** are shown, never saved or logged.
- **Never hurting the server (rule 2).** The dashboard has its own connection, opened the way the TUI opens one, and sends what the TUI sends, through the same code: it builds no commands of its own. The TUI and the dashboard take from one [command budget](#shared-command-budget-p1) per connection and node, so together they stay within the rate. The live figures are polled only while a page is open, every 2 s to start with (1 to 60 s), and not at all while paused.
- **Writes** go through the one write gate ([Write permissions](#write-permissions)). A read-only profile refuses, in the dashboard too, and the saved profile is read again for every write, so one made read-only in the TUI or with `conn set` stops writing in the dashboard at once. A write the gate asks about comes back to the page as a question, yes or no, or the name typed on `prod`; the server keeps the write under a random id that works once and for two minutes, and checks the gate again when it is answered.
- **Degrading (rule 3).** A missing permission turns one panel off and says why. No reply carries a server's error text, only redis-lens's own fixed wording (any text that starts with a server's error code is replaced); the one exception is the reply to a command typed on the command line, as in the TUI. A connect that opens but cannot sign in is a failed connect with a fixed reason.
- The dashboard words things in its own terms: no screen numbers or CLI flags, and only its own keys (see Keys).

### Chrome

- **Context bar.** One line at the top, wrapping on a narrow window: the brand, the connection button, the access button, the database control, the node select on a cluster, Go to, Live with its interval, the keys sheet button (`?`), and the theme. The connection button shows a status dot, the name and the environment badge, and opens a menu of the saved connections and the Docker containers found (name, environment and masked address, as on their cards; never a user name, password or URL). A click connects; "Manage connections" opens Connections. Docker discovery is shared with Connections and is not run again for the menu; when it is asked again, only the newest answer is kept. The access button reads Read-only or Writes on and switches the writes of the connection the session is on, as the writes switch on Connections does, through the write gate and its question. The database control shows `db N` from the state and reads the database list only when it is opened (and when Browse opens, as before); the focus starts on the current database, and a list read for an earlier session or run is never shown. On a cluster it says "A cluster only has database 0." The node select replaces the node pickers that were on Overview, Server, Pub/Sub and Search; Browse keeps its "Patterns of" filter and Overview its per-shard Show buttons. A menu closes on Escape, a click outside it or the focus leaving it, and gives the focus back to its button.
- **Left rail.** Overview, Browse, Analyze, Server, Pub/Sub, a rule, Connections and Command line. Where the page is served from is said in a line on Connections.
- **Command line.** A drawer docked to the bottom of the window over the main column, on every screen, with a focus-coloured top edge. Opening it neither scrolls the page nor changes the address; the end of the view scrolls up above it. Escape closes it from anywhere inside it.
- **Colour roles** (both themes, every text pair WCAG AA): focus and selection (links, focus rings, the current area, tab and row, the primary button); warning (▲, read-only); danger (the PROD badge, errors, writes on, and destructive buttons: delete a key, a field, element, member or entry, delete every key, remove a connection, drop an index, trim a stream); `{id} {uuid} {ulid} {hash} {ip} {var}` in pattern names (not in a row that is a single key); bars and meters.
- **Density.** 28px rows and table cells, 30px buttons, fields and pills; on a window under 780px wide every target is 44px. The main column uses the window's width.

### Keys

One key table (`keys.js`) drives the key handler, the key hints on the buttons, the `?` sheet and Go to's actions. A key either presses the control that carries its action (`data-act`), so it does exactly what a click does, or runs one of the page's own actions. A test fails when a key has no handler, a handler or control has no key, or two entries claim one key in the same place.

- **When keys act.** Never while the focus is in an input, textarea, select or editable text, and never while a dialog (a confirm included), a menu or the command-line drawer is open. Tab and Shift-Tab are the browser's. Enter and Space on a button, a link or a row do what the browser does with them; Space with the focus on none of those pauses or resumes what is live, and when nothing is live the browser keeps it (the page scrolls) and nothing is said. A held key acts once: only moving in a list repeats, so holding `d` never deletes one item after another.
- **Everywhere.** `1`–`5` and `0`: Overview, Browse, Analyze, Server, Pub/Sub, Connections; the current area's digit again goes to its next panel (Browse: Keys, Streams, Search; Analyze: its panels, Nodes and Slots on a cluster only; Server: Clients, Slow log, Monitor), wrapping. `/` filters the focused list (on Search, the query), else opens Go to. ctrl-k (cmd-k on a Mac) Go to. `?` the keys sheet. `:` the command line. `w` writes on or off (the access button, with its question). `b` the database list. `[` `]` the previous and next node on a cluster. Space pauses the tail, else the live figures. ctrl-s saves a snapshot where a Save button is shown.
- **Lists.** In a list: `j` `↓` / `k` `↑` the next and previous row, PgUp / PgDn a page, Home `g` / End `G` the ends; Enter `→` `l` open the row or, on the open row, go to the pane on the right. A row is opened only through itself (a row that is a button or link) or the one control on it marked as its open control (a key or pattern name, Connect, a consumer's Pending, a shard's Show, a snapshot's Compare), never through a write: a row with nothing to open (a pending entry, a client, a slow log entry) does nothing on Enter, and `→` `l` only move to the pane on the right if there is one; `←` `h` Esc go back or to the pane on the left. One row per list is in the tab order (a roving tabindex), and the selected row is kept across redraws. Moving the selection reads nothing and opens nothing; only Enter, `→`, `l` or a click opens a row. The focused pane has a focus-coloured border.
- **Screen verbs.** Lower case acts on the selected row, upper case on what it belongs to. Browse › Keys: in the value `y` copy the value, `Y` copy the key name, `e` edit the item or the value, `+` add an item, `t` the item's TTL, `T` the key's expiry, `d` delete the item, `D` delete the key, `R` rename, `f` format; in the key list `+` add a key, `t` the key's expiry, `T` a TTL on every key of the pattern, `d` delete the key, `D` delete every key of the pattern, `R` rename; in the patterns `y` copy the pattern, `Y` copy its scan command, `+` add a key, `t` a TTL on every key, `d` delete every key; `m` load more and `s` sort. Streams: `c` claim, `A` acknowledge or list the pending entries, `-` trim, `m` more, `r` read again, `a` analyze. Search: `x` explain, `X` profile, `+` create an index, `d` drop it, `f` format, `r` read again. Pub/Sub: `+` subscribe, `S` shard subscribe, `P` publish. Connections: `+` add, `e` edit, `d` remove, `r` look again. Overview and Analyze: `a` analyze; Slots: `s` order, `r` read again. A key that acts on the open row (a delete, a TTL, a rename, an edit, `y` on an item) acts only when the list's selected row is the open one, also when the focus is on a pane's own button such as Load more; otherwise it says to open the selected row first. On a group of patterns, `t` and `d` say that a group has no keys of its own.
- **Writes.** A write key presses the write's own button: it goes through the write gate and the same question, and when the button is off it only says why.
- **Key hints.** Each button that has a key shows it (drawn from an attribute, hidden from screen readers) and names it in `aria-keyshortcuts`. When a bar's buttons are off, the reasons are shown as text under the bar, one line each; a read-only connection says "Edits are off: this connection is read-only. Turn writes on (w)."
- **The `?` sheet.** A dialog (also the `?` button in the top bar) listing the keys for this pane, this screen and everywhere, with what a write sends, and a filter box (one character finds that key; more finds names).
- **Go to.** The top bar's Go to box, ctrl-k, cmd-k, or `/` when no list has focus opens a centred dialog. Results come from the patterns and keys already on the page, the screens and this screen's actions, in that order: Patterns, Keys with a last row "Find every key matching *text*", Screens, Actions (an action that is off is shown dimmed with its reason). Typing reads nothing from the server; Find runs only when its row is chosen. The row selected before any arrow is pressed is, in order: a Screen or Action (not off) whose name or one of its words starts with the text; else the first Pattern; else the first loaded Key; else any other Screen or Action; and Find only when nothing else matched, so Enter never starts a read the user did not point at ("slow" opens the Slow log). Text starting with `:` offers "Run in the command line", which opens the drawer with the rest typed in, not run; the row shows that text with credentials masked as the command line masks its echo (`AUTH *** ***`). Text starting with `/` finds keys. ↑↓ move, Enter goes, Tab and Shift-Tab jump between groups, Esc closes and gives the focus back; while an input method is composing, its Enter and arrows are its own. No result holds a user name, password or address.
- **Not bound.** ctrl-t and ctrl-l (the browser's), and the TUI keys with no dashboard action (ending a client, the Slow log threshold and reset).

### Browse › Keys: three panes, and its state in the address

- **Panes.** Patterns, the open pattern's keys and the open key's value sit side by side, about 26% / 32% / 42% of the width, and fill the height between the top bar and the bottom of the window (above the command drawer when it is open, so its last rows are never under it). Each pane scrolls inside itself, with its head and column headings staying put; the page does not scroll. Where the three no longer fit (a main column under about 1,060px) two show, the pane the focus was last taken to and the one to its right; under 780px, one at a time. No pane drops under another.
- **Nothing is read as the selection moves.** A pane fills only when its parent row is opened (Enter, `→`, `l` or a click), as before; until then it shows its prompt or what was opened last. Filtering a loaded list and scrolling read nothing either.
- **Patterns:** each row has the name (placeholders in their colour), the memory (from names only, the keys), a share bar, the share and ▲; the pane's head holds Sort and, on a cluster with an analysis, "Patterns of" (a node); the filter is under it, and where the names come from and how far the reading got are at its foot.
- **Keys:** the head names the pattern, or `Find: <glob>`, with its counts, Add key, Clear search under a Find, and "About", a panel that opens over the panes so the rows do not move: the pattern's figures, Copy pattern, the bulk actions (with their dry run, as before), typical and largest sizes, the TTL histogram, example keys and the scan command. A marked pattern's reason is one ▲ line above the rows. The columns are Key, Type, Size, Length and Expires (`never` in the warning colour); a narrow pane drops them from the right. The name takes the width the fixed, narrow Type, Size, Length and Expires columns leave. The open key's row has no actions of its own: Set expiry, Rename and Delete key are on the value pane's bar, and `t`, `R` and `d` on the open row press them there. Load more is at the foot. A key or action reached from the keyboard inside the closed panel (`t`, `d`, `T`, `D`, `y`, `Y`) is pressed as a click would. The panel closes on Esc (the focus goes back to its button) and on a click outside it.
- **Bulk actions only for a listed pattern.** "Set a TTL on every key" and "Delete every key" (buttons and keys) are offered only for a pattern that is a row of the patterns the server gave for this session. A pattern known only from an address lists its keys and nothing more, and its About panel says why the bulk actions are not offered (the server turns placeholders into `*`, so an address could otherwise name the whole keyspace). The check is made again as a bulk action starts.
- **Value:** the name and type, the facts line, the actions with their keys and the reasons the off ones are off, then the items, "Show as" with the format pills and the value, which takes the height left.
- **Read-only, said once.** On a read-only connection the reason is one line on the screen: under the value pane's action bar when a key is open and that pane shows, otherwise in the key list's pane; never two at once (the selected item's bar and the About panel do not repeat it). The line ends with "Turn writes on (w)", which does what the top bar's Read-only button does (the server asks first).
- **Key hints, once each.** Every control's key is drawn the one way (`data-hint`), and not on a control whose text is its key already (`?`). The format pills share one key, `f`, shown once on their "Show as" label.
- **Breadcrumb.** Above the panes: `Patterns › user:{id}:cart › user:10293:cart`, or `Patterns › Find: *cart* › key`. Each part moves the focus to its pane, and shows it when it is hidden; with fewer than three panes shown, `←` on the leftmost pane goes to the part on its left and `→` on the open row of the rightmost pane to the part on its right.
- **The address.** `#/browse?p=<pattern id>&k=<key id>&find=<text>&node=<n>&sort=<s>`: the pattern and key as the hex of their names' bytes (the ids the API takes, so a binary name travels whole), Find's text percent-encoded, the "Patterns of" node's index and the sort. With `find`, the list is Find's and `p` is the pattern Clear search goes back to. It holds nothing else: no connection, credential or value. A part that is unknown, repeated, not even-length hex or longer than the server takes is ignored; nothing in it is ever an error. `#/browse/keys` is the same screen, written `#/browse` in place. A node the analysis does not have is left out (in place) before any key is read; while the patterns are not read yet, the address waits for them.
- **History.** Opening a pattern or a key, Find, Clear search and the jumps from Overview, Analyze, Streams, Search and Go to are each a step in the browser's history, so Back undoes a drill-down and Forward redoes it; moving the selection, filtering and scrolling are not. A sort or node picked, and a key renamed or deleted, change the address in place. Back and Forward read only what changed: a new key reads only its value, a new pattern or Find only its key list, a sort or node only the patterns; going back up reads nothing. Browse is not drawn again from scratch for a change inside it. The rail's Browse link and the Keys tab, clicked on Browse › Keys, do nothing (no second entry for the same screen). A jump from another area that knows the key's type hands the type over in the history entry, never in the address.
- **Opening again.** A click or Enter on the row that is open already, and Find with the text shown, add no step. They read again a pane that shows an error, a gone key or no key (the key list, the value), and nothing else; a pane that shows what it read is not read again.
- **Late replies.** A reply that arrives after something else was opened (Load more on a pattern, then Back to another; more names, then another sort or node; more items of a value, then another key) is dropped, never drawn under the other title.
- **Restoring.** A reload, a copied link or Back from another area shows what the address names with exactly the reads a click would make: the pattern's first page of keys, then the key's value. A pattern or key that no longer exists says so in its pane, as before, and is not read again unless it is opened again. The address names no connection, so it is never used to connect: with no connection Browse asks to connect first and reads nothing; a link opened on whatever connection is live is read there, and a key or pattern it lacks is "not found here". The Browse link in the rail, from another area, shows what Browse held, without reading it again.
- **Another connection or database.** Each history entry Browse shows is marked, in this tab's history only (not in the address), with the connection, which connect of the page it was (a counter that grows each time the page sees a new connect, so an entry from before a reconnect never matches after it) and the database. Back or Forward to an entry with the same mark is applied, also after a new analysis: its pattern and key are looked up again, and say "not found" if they are gone. An entry with another mark (another connection, another database, before a reconnect) is passed over: it is not applied and not rewritten, Browse keeps what it shows, and a line above the panes says "That address was for another connection or database". A database switch, a new connect or a new analysis while Browse shows a pattern or key leaves that entry as it is and adds a step for what is shown now, so Back can return to it.

### Overview, Server and the other screens: layouts

These rearrange what each screen already holds: no layout reads anything new from the server, and picking a row reads nothing.

- **Overview.** One facts line (version, topology, uptime, role and replicas, persistence, policy, then the masked address, TLS and `reading <target>`, which are the first to go on a window under 1,280px and stay under "More figures" as "Server at"), the analysis's age and one Analyze button (`a`). Analyze asks first, with Quick and Deep to choose between (Quick picked) and each one's estimate in the question. The question opens with the focus on Cancel; Enter starts the analysis only with the focus on the Analyze button, cancels on Cancel and does nothing on a choice. Six tiles: Memory (of the limit and its share, or "no limit set"), Commands/sec, Clients (blocked under it, and "paused" while the live figures are), Hit rate, Evicted / Expired per second and Fragmentation; Commands/sec and Clients draw the series the page keeps, the others a meter or nothing. A figure not known yet shows `—`. Under them, two columns. On the left, "Things to look at": the server's findings and the analysis's, each said once, warnings (▲, in the warning colour) first; each row ends in a link to the screen it is about ("Area › Panel"), and the selected one is shown in a box under the list with only what the server sent for it (a finding no longer reported stays selected and the box says so; a reading that fails clears the list and the box). On the right, "Where the memory is" (each pattern opens Browse, and "Every panel of the analysis is on Analyze." under it) after an analysis, the prompt to analyze before one, its progress during one; then "Freed by TTL" when the expiry forecast is held. The rest of the old figures (keys, data and overhead, network, CPU, the TTL share, Sentinel, where the server is) are under "More figures", closed to start with. The cluster's shard table stays. "Turned off, and why" is one line per reason, "Off: …" for a feature turned off; the provider's notice and "Refused by the server this session" are said as they are. "Every permission check (N)" is behind a disclosure. At 1,440 × 900 the page does not scroll.
- **Lists with their detail.** Server › Slow log, Server › Clients and Analyze › Nodes show the list and the picked row's detail side by side from a 1,100px window (Clients 2 : 3, the others 3 : 2), each pane filling the height and scrolling inside itself; on a narrower window the detail goes under the list. Moving in the list picks the row and draws its detail, as it is. Slow log: Time, Took, Command, Keys and Client, ▲ on a row with a note; the entry pane has when, how long, the full arguments (masked), the keys as links to Browse (a key shown with escapes is not a link), the client and the note. Clients: one list in two headed sections, By library and By address, each row with its count, a meter and ▲ for idle connections; the group's connections on the right. Nodes: the shards (▲ partial in the warning colour, ✗ failed in the danger colour), and the picked one's facts with "Show its patterns". The pick is kept by its key across a poll that reorders the list; a reading without it keeps it picked and says it is gone, and no other row is selected or opened in its place. The same holds for Streams (a stream that left the list closes its pending list, entries and forms, and the detail says so), Search's indexes and Overview's findings. Server's panels read every 5 s, one read at a time: a tick while a read is still out is skipped, so a slow server's reply is still drawn; a reply is dropped only when the connection, the node or the panel changed while it was out. A failed Slow log read clears its settings line. Monitor's log takes the full height. Server's two charts are one compact strip.
- **The same parts everywhere.** Analyze, Streams, Search, Pub/Sub and Connections use the same panes, selection, open control and focus border as Browse, and the colour roles above. On a read-only connection each screen says so once, in one place (Streams: the open stream, else the list; Search: the open index, else the list; Pub/Sub: the publish box), with a "Turn writes on (w)" button that does what the top bar's access button does (Streams and Search say it in the server's words, which can say why the profile is read-only); the command line has the same button, named without the key, which does not act while the focus is in the drawer. A reason is said once per pane (the selected item's bar on Browse leaves out a reason the value's notes already give, such as field TTLs). Streams' Claim… and Trim… and Search's New index… are buttons with their keys in the pane head; they open the same forms. Search's formats are pills under one "Show as" label, as on Browse. Long explanations are behind "About this", unless they state a limit being hit now.
- **A question belongs to its session.** A dialog that sends something when it is answered is marked, when it opens, with the connection, which connect of the page it is and the database (the mark Browse puts on its history entries). It closes by itself when that changes while it is open, an answer given after a change sends nothing, and the page says "The connection changed: nothing was sent." ("…nothing was started." for Analyze). This holds for the ask to analyze (Overview's and Analyze's), every write form of Browse (edit, add an item, a TTL or expiry, rename, add a key, a TTL on every key of a pattern) and the questions the server asks while a connection is open. Streams' claim and trim forms and Search's create form are drawn on the screen, not in a dialog: they go as soon as the connection, the run or the node changes.

### Screens (phase 1)

One connection at a time, shared by every tab. Dark by default, with a light theme; the choice is remembered in the browser.

- **Connections:** the saved connections (name, environment, masked address, tags, read-only or writes on, how it signs in) and the Docker containers found. Connect, disconnect, test (each permission check and why a feature is off), edit (tags, the writes switch, the options `conn set` has), remove, and add with the options of `conn add`: address, name, environment, password, tags, writes, provider, SSH tunnel, Sentinel, token command (asked about first, since it runs at every connect), Entra ID where it is compiled in, prefer replica and force standalone. The environment of a saved connection cannot be changed, as in the TUI and `conn set`: add it again and replace it. The writes switch of the connected profile is the TUI's `w`, with its questions; for a container it lasts for the session.
- **Overview:** what the TUI's Overview shows before an analysis: the live figures, the server and its topology, a row per shard on a cluster (the node is picked in the top bar), the provider notice, the permission checks, and "Things to look at" from the server's own figures, by the TUI's rules. The analysis arrives with Browse.
- **Server:** the live charts, Clients (grouped as the TUI groups them, a group opens to its connections), Slow log (arguments masked) and Monitor. Monitor always asks first, with the profile name typed on `prod`, and its limits (30 s, 10,000 lines) are said before it starts.
- **Pub/Sub:** channels with their subscribers, subscribing to a channel, a pattern or a shard channel, the live tail (pause, clear, filter, rate; "Messages are shown, never saved"), and Publish through the write gate.
- **Command line:** a drawer at the bottom of the window (see Chrome), with the TUI's panel's rules: what is sent and refused, writes through the gate, repeats of at most 100, pipeline mode, the reply as text, a table, JSON or raw, completion and the command's help. A line holds at most 8,192 characters, a reply at most 4 MiB (the rest is dropped and said so), and one line runs at a time.

### Screens (phase 2)

- **Browse:** the pattern tree, from key names before an analysis (pages of 1,000, more on request) and from the analysis after one, narrowed to a shard on a cluster; a pattern's keys; Find key (text means "contains", a glob is taken as typed, with the TUI's limits: at most 8 `*`); a key's value of every type, paged as in the TUI (at most 10,000 items), each item whole in any of the TUI's formats. Key names are shown escaped, and the page names a key back to the server by the hex of its bytes, never by the shown text. Copy takes a name or a value to the browser's clipboard; text with control characters is copied only as hex. A key list stops reading pages once it holds 50,000 keys. A request the browser drops while a page is read still adds that page, so nothing stays loading.
- **Database switch:** picked in the top bar; for the dashboard's session only; the saved profile is unchanged. Off on a cluster. A switch drops the key names, key lists and open values; an analysis keeps the lists and values open.
- **Analyze:** Quick or Deep with the TUI's estimate first, progress on the event stream, Stop (what was sampled is kept, marked stopped and not saved; a stop before any key changes nothing) and Analyze again; then Big keys, Expiry, Changes, Hot keys (off without an LFU policy) and, on a cluster only, Nodes and Slots. Overview's "Things to look at" include the analysis. Big keys reads its lengths and encodings once per report. An analysis that fails says so in redis-lens's words, never as a refused connection ("The analysis failed: the server refused a command it needs.", "The analysis failed: the connection to the server broke.", the password sentences for a refused login), and the session stays connected with any earlier report.
- **Snapshots:** save with the TUI's rules (not partial, not stopped, not while running), list, and compare two, or one with the current analysis, in the TUI's snapshot directory and file format. Snapshots of two databases of one connection share that directory, as in the TUI.
- **Questions and new runs:** a question belongs to the run it was asked in. When the session starts a new run before it is answered (a database switch or an analysis), the answer is refused with "Not sent: the connection changed since the question was asked." and nothing is sent, as the TUI drops its question on a new run. This holds for write questions, a `@dangerous` command line's question, and a repeated write that a new run interrupts.
- **Requests:** a query string that lacks a parameter, or has one of the wrong type, is answered 400 with `{"error": "the query parameters are not the ones this route takes"}`, as a body that does not fit is; the input is never echoed.

### Screens (phase 3)

- **A key's value:** the edits the TUI offers for its type: a string as text or hex (sent as `SET … KEEPTTL` inside `MULTI`/`EXEC` after `WATCH`, only if unchanged, so it needs `WATCH`, `MULTI` and `EXEC` too); hash fields added, changed, deleted and given a field TTL where the server has one; list elements pushed, changed and removed; set members and sorted set members added, changed (a score) and removed; stream entries added and deleted. A value saved compressed is not edited (saving it would store it uncompressed), and says so. An edit made on a value that changed since it was read is refused, and the value is read again.
- **Keys:** add a key of any of the six types with its first content (an existing name is never overwritten), set or remove its expiry, rename it (never onto an existing name) and delete it, from the value and from the key list.
- **Bulk actions:** expire or delete a pattern's keys, as the TUI does: a dry run counts them first (reads only), then the question (the pattern typed on `prod`), then batches of at most 500 through the command budget, per node on a cluster, with progress on the event stream and a summary. Stop ends the run before the next batch: a batch already sent finishes, no further one is sent ("Stopped before the first batch: nothing was sent." when none was). While a bulk action counts or runs, a database switch, an analysis, connect, disconnect, the value and key edits, the stream and Search writes and publishing are refused, whether they were asked before it started or not ("A bulk action is running: wait for it to end, or stop it first."); a typed command on the command line is not.
- Every write goes through the one write gate. A read-only profile sends nothing, the page shows each write off with the reason, and a refused command (`NOPERM`) turns that action off for the session in the TUI's words. A question answered after a new run, on another connection, or, for a write sent to the picked node (`FT.CREATE`, `FT.DROPINDEX`, `PUBLISH` on a cluster), after another node was picked is refused and nothing is sent. Delete questions end "This can't be undone.", as in the TUI.

### Screens (phase 4)

- **Browse panels:** Browse has the TUI's three panels, Keys · Streams · Search, each at its own address.
- **Streams:** the streams the analysis found (at most 50), each read on the node that owns it, with the TUI's verdicts (lagging at a lag of 1,000 or more, stuck when a consumer has been idle 30 minutes with entries pending); a stream's groups with lag, pending and last delivered ID, their consumers with pending and idle, and a group's or a consumer's pending entries (at most 100). The entries are the stream's value: newest first, 100 at a time, read again on request, and opened whole on Browse › Keys; the list is not polled while they are open. After a write it reads the pending list shown again, or the list at once, never asking again for what was refused. Before an analysis it says streams are listed after one. Its writes are exactly the TUI's: acknowledge a pending entry (`XACK`), claim a group's pending entries for a consumer (`XAUTOCLAIM`, filled in as the TUI fills it: the most recently active consumer, 60,000 ms, 100; from a consumer or pending row, that consumer's idle time, and that consumer is left out of the target) and trim (`XTRIM MAXLEN ~` or `MINID ~`, which asks first).
- **Search:** the picked node's indexes (`FT._LIST`, at most 50) and each one's `FT.INFO`; off in the TUI's words when the module is missing or the user is refused. `FT.SEARCH`, `FT.EXPLAIN` and `FT.PROFILE` as the TUI sends them (`LIMIT 0 50`, one at a time, a query of one line and at most 1,000 characters, an empty query is `*`, a command the server refused stays off until a refresh); no `FT.AGGREGATE`, as in the TUI. The results are paged ten at a time, and a field opens whole in a format. `FT.CREATE` from the TUI's form, and `FT.DROPINDEX` without `DD` (the documents stay; it asks first). A query's own error keeps the server's message, escaped and cut, as in the TUI.
- **Hot keys from Monitor:** when the analysis has no hot keys, Analyze › Hot keys shows the TUI's second ranking, from the session's last Monitor run (Server › Monitor). Keys are placed with the Slow log's key specs, and commands it doesn't know are asked about with `COMMAND INFO` through the command budget. The ranking is kept in memory only and never saved.
- Every write goes through the one write gate and is refused while a bulk action counts or runs, whether it was asked before the bulk action started or not. A read-only profile sends nothing. Streams write on the key's owner. Search reads and writes on the picked node; picking another node, a database switch or a new run drops what it found.

## Quality bar

- **Unit tests** sit next to the code. Parsers and pattern inference also get proptest.
- **TUI screens** get insta snapshots rendered with ratatui's `TestBackend`.
- **Integration tests** run against real Redis and Valkey containers via testcontainers, behind the `integration-tests` feature.
- **CI** runs on every PR: fmt, clippy with `-D warnings`, unit tests on three operating systems, integration tests, the MSRV build, `cargo package`, and a demo seeding check. All are required on `main`.
- **Releases:** built by [dist](https://axodotdev.github.io/cargo-dist/) 0.32.0 (`dist-workspace.toml`, `.github/workflows/release.yml`). A `vX.Y.Z` tag must match the version in `Cargo.toml`. The release gate (`release-gate.yml`: fmt, clippy with `-D warnings`, unit tests on three operating systems, integration tests on Redis 7.4, Redis 6.2 and Valkey) must pass before anything is built or published. Then dist builds archives for the five targets, the shell and PowerShell installers and a Homebrew formula, creates the GitHub Release with the matching `CHANGELOG.md` section as its notes, pushes the formula to `magna-nz/homebrew-tap` and publishes the crate to crates.io (skipped if that version is already there). Prereleases skip Homebrew and crates.io. Pull requests run `dist plan` only. How to cut one: [docs/releasing.md](docs/releasing.md).

## Demo

`make demo` starts a seeded Redis (port 6390, ACL users `lens` and `lens-limited`) and a Valkey (port 6391, no auth). It saves them as connections, with the demo password going to the keychain, and opens the TUI. The seed plants carts without TTLs, search results without TTLs, an orphaned import job, a large queue, and hot keys.

## Milestones

- **v0.1:** Connections (none, password, ACL, TLS, Docker), Patterns, the TTL column, the `scan` command, the permission check.
- **v0.2:** the TTL view, snapshots and the Diff view, the Hot keys view, Entra ID, preferring replicas.
- **Quick wins (after v0.2):** `scan --check` with exit codes, Markdown, HTML and CSV reports, provider detection, the server context strip, token-command auth, snapshot retention, `snapshots`, `trend` and the Diff trend column, and `compare` for two standalone instances.
- **v0.3:** cluster (scan each primary, or a verified replica per shard), the Nodes view (now Analyze › Nodes), cluster-aware reports, snapshots, `diff` and `compare`, and SSH tunnels through a bastion.
- **Phase 1, read-only views (built):** seven views and panels, the shared command budget, the key list of a pattern, Analysis (Big keys, Expiry, recommendations), Server (Overview, Clients, Slow log without arguments), Streams, Pub/Sub counts, Search indexes, the new permission probes. On a cluster the views that read one server (the key list, Server, Streams, Pub/Sub and Search) are off and say why.
- **Phase 2, values on screen:** key values and formats (built: the key detail and the value renderer), stream entries, the Pub/Sub live tail (built), Search queries, Slow log arguments and the Monitor panel (built), the command line for readonly commands (built).
- **Phase 3, writes (built):** the write permission checks, `w` and `conn set --writes`, key edits, bulk expire and delete, `redis-lens expire`, stream claim, ack and trim, publish, index create and drop, kill client, slow log settings, write commands on the command line.
- **Phase 4, topology and connections (built):** the live views on a cluster (routing by slot, a picked node, a per-shard summary, sharded Pub/Sub, bulk actions and the command line per node), Sentinel connections, the database switch, the Slots panel from `CLUSTER SLOT-STATS`, and the small items: a type filter on the key list, `conn export` and `conn import`, tags on connections, a light theme and `NO_COLOR`.
- **TUI redesign (built):** five areas and a home screen (0 Connections, 1 Overview, 2 Browse, 3 Analyze, 4 Server, 5 Pub/Sub), `tab` only for panels and `→` `←` for depth, `v` for more columns, a one-line header with its status, a footer of 3 to 5 keys and a `?` help that finds and runs any action; a connect that reads no key and Overview with the server's own figures and findings; an analysis only on `a`, Quick (10,000 keys) or Deep (100,000), with its estimate and `esc` to stop it; Browse › Keys from key names before an analysis; Analyze › Expiry as one panel, with the forecast of memory freed by TTL in its right pane and on Overview.
- **Phase 5, the rest of Redis Insight parity (built):** findings for a `volatile-*` policy with few TTLs, a full `noeviction` server and LFU being off; `y` on Server › Slow log and Monitor; command line result views (text, table, JSON, raw), `N cmd` repeats and pipeline mode; decompression of Snappy; detection and trees for PHP `serialize()`, Pickle, Protobuf and Java serialization; and the values of arrays and of the time series, Bloom, Cuckoo, Count-Min Sketch, Top-K and t-digest module types; and force standalone for a cluster node ([Cluster](#cluster)).
- **Left out of phase 5, with reasons:** cloud discovery (Redis Cloud, AWS, Azure across subscriptions), which needs cloud accounts and can't be tested in CI; SSH password login, a large change on a security-sensitive path; connection presets from environment variables, which conflict with rule 1 and are the owner's call; aliases on connections, low value; server-side `HOTKEYS`, not confirmed on any image; uploading a commands file, low value; Vector Set similarity search; creating JSON keys. `expire --dry-run` on read-only profiles stays refused, as in [Bulk actions](#bulk-actions): changing a safety rule is the owner's call.
- **Later:** native ElastiCache IAM (a token command covers it today), Entra managed identity, `expire --dry-run` for non-production profiles, and server-side `HOTKEYS`.

Phases 1 to 5 come from [docs/redisinsight-parity.md](docs/redisinsight-parity.md). The mockups are in the TUI design artifact (https://claude.ai/artifact/2RzWTZFKwJASgtX3phxKfG).
