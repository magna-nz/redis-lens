# redis-lens status

Where the build stands. Update this file in the same PR as the work it describes. What the tool should do is in [spec.md](spec.md).

**Current milestone:** v0.2 done on main; wave 6 merged (PR #10); wave 7 (four bugs and the test gaps) in progress; then usability, then v0.3 (cluster) or the feature ideas below
**Last updated:** 2026-11-11 (wave 7 merged on `wave7/fixes`; verifier next)

## Overnight run (2026-11-10): approvals and plan

The owner approved waves 3, 4 and 5 to run unattended, with these rules:

- **Merging:** merge each wave's PR into `main` with the admin bypass (`gh pr merge --merge --admin`), but only when all 8 required checks are green **and** the wave's verifier has signed off. Use a merge commit, never squash. If CI fails, fix it (Auto-fix is on) and merge once it is green.
- **Never** create a tag, a GitHub release, or run `cargo publish`.
- If a wave can't be completed, stop it, then record why here and in the PR.
- **After wave 5**, research new features and write proposals under [Feature ideas](#feature-ideas). Don't build them.
- **Final check:** the TUI must run properly against the demo Docker containers. Record it with `vhs`.
- Keep the orchestrating session's context under about 60%. This file is the restart point.

| Wave | Content | Agents | Branch → PR | State |
|---|---|---|---|---|
| 2 | Scanner and connections | implementer ×2 [sonnet] | `wave2/*` → `v0.1-core` | **done**, merged into `v0.1-core` |
| 3 | TUI (Connections and Patterns views), `scan` command, verifier fixes | | `v0.1-core` → PR #6 | **done**: verified, CI green, merged into main (6dc45c6) |
| 4 | Snapshots and diff engine; TTL, Diff and Hot keys views | | `v0.2-views` → PR #7 | **done**: verified, CI green, merged into main |
| 5 | Docs, logo, GIF, CHANGELOG, Dependabot ignore; replica preference with verification; keychain-wait hint; v0.2 follow-ups | implementer [sonnet] ×2, implementer [opus], verifier [opus] | `wave5/polish` → PR | **done**: verified after three rounds (the first two BLOCKED on replica trust, both fixed), CI green, merged into main as PR #8 (0f981e9) |

## Wave 6 (2026-11-11): bugs and rough edges

Approved by the owner: "Approve, merge when green". Same merge rules as the overnight run: all required checks green and verifier sign-off, `gh pr merge --merge --admin`, no tag, release or publish.

| Part | Content | Agent | Branch | State |
|---|---|---|---|---|
| A | Scale estimate ignores keys that vanish between `SCAN` and the per-key lookups (counts come out low); live partial results in the Patterns view | implementer [opus] | `wave6/scan` (worktree `.claude/worktrees/w6-scan`) | **done** (5eea369, 008d6a8): denominator is keys present (seen minus vanished; unreadable keys stay in); partials on a second channel via `scan_with_partial`; 247 unit tests; merged into `wave6/fixes` |
| B | Redis 6.2 in the integration job, as a new CI job (existing required check names unchanged) | implementer [sonnet] | `wave6/tests` (worktree `.claude/worktrees/w6-tests`) | **done** (89f0274): all integration tests pass on 6.2 with no test changes; merged into `wave6/fixes` |
| C | OS trust store for TLS (load native certs, skip ones that fail to load, fall back to bundled roots when the store is empty); TLS replica test on Redis and Valkey via a throwaway CA and `SSL_CERT_FILE` | implementer [opus] | `wave6/tls` from `wave6/fixes` after B | **done** (9291fba, 3a54fdc): `src/conn/tls.rs`; `tests/tls_it.rs` (6 tests, Redis + Valkey, also on 6.2); merged into `wave6/fixes` |
| V | Verifier on the merged `wave6/fixes` | verifier [opus] | pinned detached worktree at 09343e2 | **APPROVE**, no blockers. Its ready-made test for the partial denominator (D1) is added. Follow-ups below |

## Wave 7 (2026-11-11): four bugs and the test gaps

Approved by the owner ("Approve all"): the split below; on Windows the OS store plus the bundled roots; a throwaway CA installed in each CI runner's OS store; merge with `gh pr merge --merge --admin` when all checks are green and the verifier approves. No tag, release or publish. Later waves, not started: rescan key, sorting, `?` help, clipboard, selection following the pattern (N2, N3), Page Up/Down, `--check`, report export, v0.3 cluster.

Integration branch `wave7/fixes` (worktree `.claude/worktrees/wave7`, from main 92adb64). Contract commit e8973be: `conn::docker::DiscoveredAuth` (`model::Auth` unchanged: it is written to the config file, and "unknown" is not a way to authenticate).

| Part | Content | Agent | Branch | State |
|---|---|---|---|---|
| A | Per-scan id (`ScanId`) on `Effect::Connect`, `LoadHistory` and every `Msg::Connected/ConnectFailed/Scan*/History`; forwarder aborted with its scan (`AbortOnDrop`) through the `scan_and_report` seam; D5 and D6 tests; discovered auth in the Connections view | implementer [opus] | `wave7/tui` | **done** (9ebc063, bc26d61), merged |
| B | Keychain calls give up after 15 s (`SecretError::Timeout`, "Always Allow" on macOS); `discover` probes with a credential-less `PING` (none / required / unknown); tests: URL-password warning, `--ask-password`, `--config` and `REDIS_LENS_CONFIG` (`tests/cli_config.rs`), `conn test`, the spec's `lens-ro` ACL line (Redis 6.2, 7.4, Valkey 8.1) | implementer [sonnet] | `wave7/conn` | **done** (ff681cd), merged |
| C | Bundled roots as an explicit store from `webpki-root-certs` 1.0.9; Windows trusts the OS store plus the bundle (N4); D2 (exact store sizes, Cargo.lock pin on redis 1.8.1), D3 (bundle non-empty, holds ISRG Root X1), D4 reduced to one fallback; `tests/os_trust.rs` plus a CI step that installs a throwaway CA in the OS store on macOS, Windows and Ubuntu | implementer [opus] | `wave7/tls` | **done** (2a4196c), merged. The CI trust-store step runs for the first time on the PR |
| D | Limiter pacing (virtual clock, plus a rate lower bound over the fake), `--max-rate` into `ScanOptions` for `scan` and `diff`, SCAN deduplication | implementer [sonnet] | `wave7/scan` | **done** (21e8dbb), merged |
| V | Verifier on the merged `wave7/fixes` | verifier [opus] | pinned detached worktree | pending |

**Test gaps from wave 6, and where each is covered now:**
- D1 partial denominator: `scan::tests::partial_denominator_leaves_out_pending_and_vanished_keys` (wave 6).
- D2: `conn::tls::tests::a_template_holds_exactly_the_given_roots`, `redis_is_still_the_version_whose_store_handling_was_checked`.
- D3: `conn::tls::tests::the_bundle_is_the_crate_set_and_holds_isrg_root_x1`, `bundled_roots_are_an_explicit_store_of_exactly_the_crate_certs`.
- D4: one shared fallback, unreachable (comment in `TlsRoots::from_choice`).
- D5: `tui::tests::progress_and_partials_all_arrive_before_scan_done` / `_before_scan_failed`, `aborting_the_scan_task_stops_its_forwarder`.
- D6: `tui::app::tests::replaced_run_of_the_same_entry_is_ignored`, `replaced_run_of_the_same_entry_does_not_mask_the_new_runs_failures`.
- D7 (`make coverage`) and D8 (throttle wiring): unchanged; dev target and low value.
- Limiter and `--max-rate`: `scan::tests::limiter_*`, `scan_with_a_rate_takes_at_least_commands_over_rate`, `cli::scan::tests::max_rate_flag_reaches_the_scan_options`, `cli::diff::tests::diff_max_rate_flag_reaches_the_scan_options`.
- SCAN dedup: `scan::tests::repeated_scan_keys_are_looked_up_once`, `repeated_scan_keys_do_not_skew_the_scale_estimate`.
- URL-password warning: `cli::conn::tests::url_password_warns_without_echoing_it`. `--ask-password`: `ask_password_goes_to_store_only`. `REDIS_LENS_CONFIG` / `--config`: `tests/cli_config.rs`.
- `conn test`: `conn_it::conn_test_*`. `lens-ro` line: `conn_it::spec_minimal_read_only_user_can_connect_check_and_scan`.
- Per-OS trust store: `tests/os_trust.rs` (runs in CI's unit-test job on three OSes; skipped locally).
- Real keychain: `secrets::tests::keyring_store_round_trip` stays `#[ignore]`, because it needs a person to answer the keychain prompts. The timeout around it is tested with a blocking fake (`secrets::tests::with_timeout_*`).
- Horizontal scrolling: not implemented (see Known issues); waiting for the owner.

**Wave 6 follow-ups (from the verifier; N1, N4 and the test gaps are done in wave 7; N2 and N3 are later):**
- N1: after reconnecting the same entry, a stale partial (or `ScanDone`) from the aborted scan can show for up to about 250 ms. The forwarder outlives the aborted scan and sends with the same `ConnId`. Fix: a per-scan id, or abort the forwarder from a drop guard (`tui/app.rs` ~1065, `tui/mod.rs` ~177/378).
- N2 and N3 (UX): the Patterns selection is a row index, so it moves as partials re-sort and resets to 0 on `ScanDone`. A collapse is lost when a group's key changes.
- N4 (Windows): the Windows store fills public roots lazily, so a root that hasn't been fetched yet now gives `UnknownIssuer` where bundled-only used to connect.
- Test gaps in the diff:
  - D2: nothing catches a redis-rs upgrade that merges stores.
  - D3: the empty-store TLS test would also pass with zero roots.
  - D5: the ordering inside `connect_and_scan`.
  - D6: the N1 window.
- Spec behaviours with no test:
  - limiter pacing and `--max-rate`
  - SCAN deduplication
  - the shell-history warning for a URL password
  - `--ask-password` and `REDIS_LENS_CONFIG`
  - `conn test`
  - horizontal scrolling
  - the spec's `lens-ro` ACL line
  - per-OS trust-store connections
  - the real keychain (an ignored test)

**TLS replica test (resolved: the owner chose the OS trust store, done in part C).** Original blocker: `src/conn/mod.rs:119` builds TLS connections with `tls_params: None` on redis-rs's `tls-rustls-webpki-roots`, so only public CAs are trusted and a test with a throwaway CA can't connect. This is also a product gap: a server behind a private or corporate CA can't be reached. It needs a decision (OS trust store, or a per-profile CA file) before a test can be written.

**Owner instruction (2026-11-11): "make sure we have tests for everything".** Every behaviour change in this wave needs a test. Each agent reports a test map (change → test), and the verifier audits it, plus a repo-wide check that each behaviour in `spec.md` has a test.

**Coverage (`make coverage`, unit + integration, 2026-11-11 at 09343e2):** 92.6% of lines. All tests pass: 260 unit, conn 14, diff 4, replica 11, scan_cli 5, scan 15, smoke 3, tls 6. Lowest: `main.rs` 0% (entry point only), `cli/mod.rs` 24% (clap dispatch), `tui/mod.rs` 52% (terminal event loop), `secrets.rs` 70% (the real keychain, an ignored test), `tui/ui/connections.rs` 77%.

Integration branch: `wave6/fixes` (worktree `.claude/worktrees/wave6`). Out of scope: Entra ID against real Azure and managed identity (needs Azure; feature idea 10), and the `--prefer-replica` trust boundary (a documented design trade-off).

### How to resume after a restart or compaction

1. Read this file, `spec.md` and `CLAUDE.md`.
2. Run `git worktree list` and `git branch -a`, and check open PRs with `gh pr list`. Agent worktrees live in `.claude/worktrees/<name>`, made by hand from the integration branch, because the harness's own worktrees branch from `main`.
3. Find the first wave in the table above that isn't **done**. Check its branches for commits before redispatching agents.
4. Each wave ends like this:
   1. Read the diffs.
   2. Merge with `--no-ff`.
   3. Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -D warnings`, `cargo test`, and `cargo test --features integration-tests --tests`.
   4. Dispatch the verifier.
   5. Update this file.
   6. Push and open the PR.
   7. Merge when it's green.

## Done

- 2026-11-10: Public repo, with `main` protected (code-owner review, required CI checks with admin bypass "always", no force-push) and CODEOWNERS.
- 2026-11-10: Scaffold, CI, the release workflow and the demo environment ([PR #1](https://github.com/magna-nz/redis-lens/pull/1), merged).
- 2026-11-10: Connections layer, on `v0.1-core`:
  - keychain storage (one JSON entry per profile)
  - TOML config, written atomically with mode 0600
  - parsing and masking of redis://, rediss:// and StackExchange.Redis strings
  - connecting with ACL users and TLS
  - permission probes
  - Docker discovery
  - `conn add/list/remove/test` and `discover`

  Checked by hand against the real macOS Keychain and the demo servers: no credentials in the config file, and the keychain entry is removed on `conn remove`.
- 2026-11-10: Scanner, on `v0.1-core`:
  - pattern inference (`{id}` `{uuid}` `{ulid}` `{hash}` `{ip}`, plus collapsing to `{var}`)
  - per-pattern stats
  - INFO parsing for Redis and Valkey
  - rate-limited `SCAN` sampling that tolerates `NOPERM`
- 2026-11-10: `src/format.rs`: shared byte, count, duration, percentage and glob formatting.
- Tests on `v0.1-core`: 71 unit tests (including proptests), plus 28 integration tests against Redis 7.4 and Valkey 8.1: 14 connection, 11 scanner, 3 smoke.

- 2026-11-10: TUI with the Connections and Patterns views, merged into `v0.1-core`. Checked in a real terminal against the demo containers using `demo/record.py`, which drives the TUI in a pseudo-terminal and writes an asciinema cast; `agg` turns that into a GIF. The tree, the planted-problem flags, the detail panel and the add flow with the password masked all work.

## In progress

See the overnight table above.

## Needs a decision or action from the owner

- **Dependabot PRs #2 to #5** are left for review. Don't merge #2: it bumps the MSRV job's pinned `dtolnay/rust-toolchain@1.89` to 1.120. Wave 5 adds a Dependabot ignore rule for that pin.

- Add a `CARGO_REGISTRY_TOKEN` repository secret before the first release.
- Decide whether to publish a placeholder 0.0.1 to reserve the crate name early.

## Known issues and notes

- **Replica scans (`--prefer-replica`) trust the announced address**, like Cluster and Sentinel clients do. A server that imitates a replica, since the replication ID isn't secret, can be scanned without credentials, and its data is filed under the profile. A profile that reaches prod through an SSH tunnel on 127.0.0.1 treats local services on the announced port as on the primary's host. A replica scanned without credentials runs as its default user. TLS replica scans are tested (`tests/tls_it.rs`).

- **macOS keychain prompt after rebuilds.** Keychain entries are tied to the binary that wrote them. A rebuilt or upgraded binary has to ask macOS for access, and until someone answers the dialog, connecting hangs. Mitigated in wave 5: the README says to click "Always Allow", and the TUI shows a hint when a keychain read takes more than 2 s. Since wave 7 a keychain call gives up after 15 s with an error saying to click "Always Allow" and try again, so nothing hangs. A write that timed out can still finish once the dialog is answered, leaving an entry no profile points to (still only in the keychain).

- **Lesson:** never switch the main checkout's branch while an agent is reading it. Do all work in worktrees.

- **`vhs` doesn't work here.** It drives headless Chromium, which produces no output, and turning off its browser sandbox was refused. Use `demo/record.py` with `agg` for GIFs instead.

- The demo's `rl:*` and `lock:*` keys expire after up to 30 minutes, so re-run `make demo-up` before recording a GIF. (Local profiles scan with no rate limit since wave 4; `--max-rate` overrides it.)

- Agent worktrees made by the harness branch from `main`, not the current branch, so create them by hand.
- **Entra ID** is implemented behind `--features entra-id`, using redis-rs's `EntraIdCredentialsProvider::new_developer_tools()` (az login and other developer tools). It is untested against real Azure, and managed identity isn't covered yet.
- `discover` shows the demo Redis as auth "required" (since wave 7; it was "none"), because its passwords live in an ACL file discovery can't see. Save it as a profile instead (`make demo-profiles`).
- **Horizontal scrolling is not implemented**, although the spec's Views section says narrow terminals scroll horizontally. Rows are drawn at fixed column widths and cut off at the right edge; nothing wraps, but there is no scroll offset or key. Found in wave 7.
- For Redis ACL rules passed as server arguments, use one argument per word (`tests/common/mod.rs`).
- `ParsedConnection.credentials` is `Some` with an empty password when a URL has a username but no password, so `conn add` can ask for `--password-stdin` or `--ask-password`.

## Feature ideas

Researched on 2026-11-11. Proposals only; nothing here is built. The full write-up, with evidence, scope, effort, risks and sources, is in [docs/feature-ideas.md](docs/feature-ideas.md).

**Quick wins** (ranked by value against effort):
1. `--check` thresholds with exit codes for CI and pre-deploy gates (S)
2. Report export as Markdown, HTML or CSV (S)
3. Detect ElastiCache, MemoryDB and Azure, and say plainly what degrades (S)
4. Server-wide numbers from `INFO keysizes`, `INFO keyspace` and `MEMORY STATS` (S)
5. An external token command for passwordless auth on GCP, AWS and Azure (S)
6. Snapshot retention (`--keep`, `prune`) and a trend view (S–M)
7. Compare two instances, for example staging against prod (S–M)

**Bigger bets:**
8. Hash field TTL awareness (`HEXPIRE`), to avoid false leak verdicts (M)
9. Native AWS IAM auth for ElastiCache and MemoryDB (M)
10. Azure managed identity and service principal, plus Azure Managed Redis cluster-policy handling (M)
11. Prometheus textfile export (M)
12. Server-side `HOTKEYS` (Redis 8.6+) in place of a `MONITOR` fallback (M)
13. A per-slot view from `CLUSTER SLOT-STATS`, after v0.3 (M)
14. Offline RDB analysis (L)

**Verified facts that affect the roadmap:**
- ElastiCache Serverless blocks `MEMORY USAGE` and `OBJECT FREQ`.
- The OSS clustering policy on Azure Managed Redis needs v0.3 cluster support.
- `HEXPIRE` exists in Redis from 7.4 and in Valkey only from 9.0, so CI would need a newer Valkey to test it.
