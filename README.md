<div align="center">
  <img src="docs/mark.svg" alt="redis-lens mark" width="104">
  <h1>redis-lens</h1>
  <p><strong>A terminal UI and a web dashboard for Redis and Valkey.</strong></p>
  <p>
    <a href="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml"><img src="https://github.com/magna-nz/redis-lens/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
    <a href="https://crates.io/crates/redis-lens"><img src="https://img.shields.io/crates/v/redis-lens" alt="crates.io"></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.89%2B-B7410E" alt="Rust 1.89+"></a>
    <a href="https://valkey.io/"><img src="https://img.shields.io/badge/Redis-6.2%2B%20%7C%20Valkey%208-DC382D" alt="Redis 6.2+ and Valkey 8"></a>
    <a href="#license"><img src="https://img.shields.io/badge/License-MIT-yellow.svg" alt="License MIT"></a>
  </p>
  <p><a href="https://magna-nz.github.io/redis-lens/"><strong>Documentation</strong></a></p>
</div>

See what's in your Redis or Valkey, from your terminal or a web page on your own machine.

- **Browse** key patterns, keys and values side by side
- **Find** what uses the memory and what never expires
- **Watch** live stats, clients, the slow log and Pub/Sub
- **Stay safe:** production is read-only until you turn writes on

<div align="center">
  <img src="docs/demo.gif" alt="redis-lens in the terminal: connecting to the demo Redis, Overview, analyzing the keys, Browse with patterns, a pattern's keys and a value side by side, the Actions menu, Go to opening the Slow log, and the keys sheet" width="900">
  <p><em>In the terminal</em></p>
  <img src="docs/dashboard.gif" alt="The redis-lens dashboard in a browser: connecting from the connection menu, Overview, Browse with a pattern, its keys and a value side by side, Go to opening the Slow log, the command line drawer, the keys sheet and the light theme" width="900">
  <p><em>In the browser</em></p>
</div>

## What you can do

- **Find any key:** `ctrl-f` from any screen (in the browser, `/` in Go to) searches the whole server by name or pattern with `SCAN`.
- **Browse keys and values:** every Redis type, plus JSON, streams and module types, decoded as JSON, MessagePack, hex and more.
- **See what uses memory:** `a` groups the keys into patterns, with big keys, hot keys and keys that never expire. Overview lists what to look at.
- **Watch the server:** live figures, clients, the slow log, `MONITOR` and Pub/Sub.
- **Run commands:** `:` opens a command line with completion and help, and replies as text, a table or JSON.
- **Edit data:** add, edit, expire, rename and delete keys, fields and elements, and expire or delete every key of a pattern after a dry run.
- **Connect to anything:** ACL users, TLS, cluster, Sentinel, SSH tunnels, cloud IAM sign-in and Docker discovery.
  Passwords stay in the OS keychain.
- **Check it in CI:** save snapshots, compare them over time, and fail a build on rules you set.
- **Use it in a browser:** `ctrl-o` opens the dashboard, with every screen the terminal has and the same rules: read-only production, writes through the gate, passwords only in the keychain.
  The connection, its access, the database and the node are switched from the bar at the top.

## Keys

The same letters mean the same thing on every screen, in the terminal and in the browser (where a few are terminal-only, the table says so).
Lower case acts on the selected row, upper case on what it belongs to: with a field selected `d` deletes the field and `D` the key; with a key selected `d` deletes the key and `D` every key of its pattern.

| Key | Does |
|---|---|
| `1`–`5`, `0` | Overview, Browse, Analyze, Server, Pub/Sub, Connections. The current area's digit again goes to its next panel |
| `tab` `shift-tab` | Next or previous panel (terminal; the browser keeps them for focus) |
| `↑↓` `j` `k`, `PgUp` `PgDn`, `Home` `End` | Move |
| `→` `l` `enter` / `←` `h` `esc` | Open, or go back |
| `/` | Filter the list |
| `s`, `m` | Sort, more rows |
| `v` | More columns (terminal) |
| `a` | Analyze the keys: Quick (10,000) or Deep (100,000) |
| `r` | Read again now |
| `space` | Pause or resume what is live |
| `y` / `Y` | Copy the row / its parent (a key name / its pattern) |
| `e` `+` `R` `f` | Edit, add, rename the key, show the value in another format |
| `t` / `T` | Set a TTL on the row / on its parent |
| `d` / `D` | Delete the row / its parent |
| `w` | Writes on or off for the connection |
| `b`, `[` `]` | The database list; the previous or next node on a cluster |
| `.` | Actions of the selected row and the screen, with their keys (terminal; in the browser each button shows its key) |
| `ctrl-k` | Go to a pattern, a loaded key, a screen or an action |
| `ctrl-f` | Find keys on the whole server (terminal) |
| `:` | Command line |
| `?` | The keys of this screen |
| `ctrl-s` | Save a snapshot of the analysis |
| `ctrl-o`, `q` | Open the dashboard, quit (terminal) |

Every key, screen by screen, is in the [key reference](https://magna-nz.github.io/redis-lens/#keys) and in `?`.

## Install

```sh
# Homebrew (macOS, Linux)
brew install magna-nz/tap/redis-lens
# Shell installer (macOS, Linux)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/magna-nz/redis-lens/releases/latest/download/redis-lens-installer.sh | sh
# PowerShell installer (Windows)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/magna-nz/redis-lens/releases/latest/download/redis-lens-installer.ps1 | iex"
# crates.io (Rust 1.89+); add --features entra-id for Microsoft Entra ID sign-in
cargo install redis-lens --locked
```

Or download a binary for macOS, Linux or Windows from [GitHub Releases](https://github.com/magna-nz/redis-lens/releases).

## Get started

```sh
redis-lens conn add local --url redis://localhost:6379
redis-lens              # the terminal UI: enter connects, a analyzes, ? lists the keys, ctrl-o opens the dashboard
redis-lens --dashboard  # only the dashboard, opened in your browser
redis-lens --terminal   # only the terminal UI, no dashboard
```

From a clone, `make demo` starts a seeded Redis and Valkey in Docker to try it on.

## Contributing

```sh
make test   # unit tests
make it     # integration tests against Redis and Valkey (Docker)
make lint   # rustfmt and clippy
```

Behaviour is defined in [`spec.md`](spec.md); a change to behaviour updates it in the same pull request.
Releases: [`docs/releasing.md`](docs/releasing.md).

## License

[MIT](LICENSE)
