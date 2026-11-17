# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/).

`dist` reads this file when it cuts a GitHub Release: the section whose heading names the
tagged version becomes the release notes, and the heading becomes the release title. A
section headed only `## [Unreleased]` is not used. Before you tag, give the changes their
version heading, `## [X.Y.Z]` or `## [X.Y.Z] - YYYY-MM-DD` (see
[docs/releasing.md](docs/releasing.md)).

## [Unreleased]

### Added

- **Go to (`ctrl-k`):** one box on every screen that goes to a pattern, a key already loaded, a screen or an
  action from a few typed letters. Typing reads nothing from the server; `Find every key matching …` runs a
  find only when you pick it, and text that starts with `:` goes to the command line, masked. The dashboard has
  it too (`ctrl-k`, `cmd-k`, or `/` when no list has focus).
- **The Actions menu (`.`):** the actions of the selected row, its parent and the screen, each with its key and
  the command a write sends, `asks first`, or `off:` and the reason.
- **The confirm sheet:** every question that guards a write is one sheet at the bottom of the screen, with the
  exact command it sends, so the row it acts on stays in view. A question opened by a server reply takes no
  answer for half a second.
- **Browse in three panes** from 120 columns: patterns, the open pattern's keys and the open key's value side by
  side. The dashboard shows the same three panes, and its address holds the open pattern, key, find and sort,
  so Back, Forward, reload and a copied link work.
- **Key list actions:** `t`, `d` and `R` on the selected key, `T` and `D` on every key of its pattern, and `Y`
  copies the pattern.
- **Dashboard top bar:** the connection menu, the access button (`w`), the database (`b`), the node on a
  cluster, Go to, Live and its interval, the keys sheet (`?`) and the theme. The command line is a drawer at
  the bottom of the window. The dashboard has the terminal's keys where the browser allows them.

### Changed

- **One key grammar:** a letter means the same thing on every screen; lower case acts on the selected row,
  upper case on what it belongs to. `a` analyzes and `r` only refreshes. `w` works on every screen.
- **The screen's chrome:** a one-line context bar, then an area row that shows the open area's panels after its
  name (`2 Browse › Keys · Streams · Search`). The footer ends in `. Actions   ^K Go to   ? Keys`.
- **Colours mean one thing each** in the terminal: blue for focus, a darker blue for bars, meters and
  sparklines, amber for warnings (`▲` and its sentence, `never` in an expiry column, `read-only`), red for
  danger (the PROD badge, `writes on`, errors, actions that can lose data). Before, bars were amber and
  warnings red.
- **The `?` overlay is called Keys** (`Keys · <screen>`), as the footer calls it.
- **Overview from 120 columns:** "Things to look at" and "Where the memory is" side by side, under the live
  figures.
- Keys that moved:

  | Action | 0.1.0 | Now |
  |---|---|---|
  | Analyze the keys | `r` | `a` (`r` only refreshes) |
  | Pause or resume what is live | `p` | `space` |
  | Sort | `o` | `s` |
  | Load more rows | `n` | `m` |
  | Save a snapshot | `s` | `ctrl-s` |
  | Overview's poll interval | `+` `-` | `<` `>` |
  | Key list: show one type | `t` | `ctrl-t` |
  | Key list: add a key | `N` | `+` |
  | Pattern: TTL on every key, delete every key | `x`, `D` | `t`, `d` |
  | A value: the key's TTL | `t` | `T` (`t` is the field's TTL) |
  | Clients: end a connection | `K` | `d` |
  | Slow log: threshold, reset | `c`, `R` | `e`, `D` |
  | Streams: trim | `X` | `-` |
  | Streams before an analysis: analyze | `r` | `a` |
  | Pub/Sub: subscribe, clear the tail | `s`, `c` | `+`, `ctrl-l` |
  | Search: explain, profile | `e`, `P` | `x`, `X` |
  | Search: create, drop an index | `C`, `D` | `+`, `d` |
  | Connections: add | `a` | `+` |
  | Turn writes on or off | `w` on Connections | `w` on every screen |

## [0.1.0] - 2026-11-16

The first release.

### Added

- **A terminal UI for Redis and Valkey** with six areas: Connections, Overview, Browse, Analyze, Server and Pub/Sub.
  `?` lists every key and runs any action. A light theme, and `NO_COLOR` is respected.
- **Find any key:** `ctrl-f` on any screen searches the whole server by name or pattern.
- **Browse keys and values:** every Redis type, plus JSON, streams and consumer groups, Search indexes, and the
  time series, Bloom, Cuckoo, Count-Min Sketch, Top-K and t-digest module types. Values are decoded as JSON,
  MessagePack, hex and more, including gzip, zlib, LZ4, zstd and Snappy compression and PHP, Pickle, Protobuf
  and Java serialization.
- **See what uses memory:** an analysis on `r` (Quick or Deep) samples keys with `SCAN`, groups them into
  patterns such as `user:{id}:cart`, and shows each pattern's key count, memory and TTL coverage. Analyze has
  Big keys, Expiry (with a forecast of memory freed by TTLs), Changes since the last snapshot, Hot keys (LFU),
  and on a cluster Nodes and Slots.
- **Watch the server:** Overview with the server's own figures and findings, Clients, the slow log, `MONITOR`,
  and a live Pub/Sub tail, including sharded Pub/Sub.
- **Run commands:** a `:` command line with completion and help, replies as text, a table, JSON or raw, repeats
  and pipeline mode.
- **Edit data:** add, edit, expire and delete keys, bulk expire and delete, stream claim, ack and trim, publish,
  Search index create and drop, kill a client, and slow log settings. Every write goes through one write gate:
  production profiles are read-only unless writes are turned on (`w`, `conn set --writes`).
- **Connect to anything:** saved connections in a TOML config file, with usernames and passwords only in the OS
  keychain (`conn add`, `list`, `remove`, `test`, `export`, `import`). `redis://`, `rediss://` and
  StackExchange.Redis connection strings, TLS, ACL users, cluster, Sentinel, the database switch, SSH tunnels
  through a bastion, a token command for cloud IAM sign-in, Microsoft Entra ID behind the `entra-id` feature,
  scanning a verified replica (`conn add --prefer-replica`), and Docker discovery (`redis-lens discover`).
- **A web dashboard:** the same screens in a browser on your own computer (`ctrl-o` in the TUI, `--dashboard`
  for the dashboard alone). It listens on 127.0.0.1 only, behind a session token, and follows the terminal's
  rules: read-only production profiles, every write asked about first, passwords only in the keychain.
- **A permission check** that probes each command redis-lens needs and turns one feature off, saying why,
  instead of failing.
- **Check it in CI:** `redis-lens scan` (table, `--json`, Markdown, HTML and CSV reports, `--check` rules with
  exit codes), snapshots (`scan --save`, `s` in the TUI), `diff`, `snapshots`, `trend` and `compare`.
- **Gentle on the server:** `SCAN`, never `KEYS`, per-key commands pipelined in batches, and one command budget
  every view shares (`--max-rate`; 5,000 commands a second by default, none for local profiles).
- Supports Redis 6.2 and later and Valkey 8, on macOS, Linux and Windows.
- Releases: prebuilt binaries for five targets, shell and PowerShell installers, a Homebrew formula and
  crates.io, built only after fmt, clippy, the unit tests and the integration tests pass.
