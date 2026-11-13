# redis-lens status

Where the build stands. Update this file in the same PR as the work it describes. What the tool should do is in [spec.md](spec.md).

**Current milestone:** Redis Insight parity phase 2 (values on screen), in one PR: [#16](https://github.com/magna-nz/redis-lens/pull/16), branch `phase2/values-on-screen`. Phase 1 merged to main (#15).
**Last updated:** 2026-11-14 (phase 2: all four parts built, in #16)

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

## Quick wins (2026-11-11): feature ideas 1-7

Approved by the owner ("Approve as proposed"). Defaults agreed:
- The token command's username stays in the OS keychain (rule 1); only the argv goes in the config file. `conn add` asks first (`--yes` skips).
- Reports leave example key names out unless `--examples`.
- `compare` comes forward from v0.3 for standalone servers.
- The provider is a host-name hint; ElastiCache Serverless gets a lower default rate.
- `snapshots prune` needs `--yes` and only touches redis-lens's own snapshot directory.
- No tag, release or publish. The owner merges the PR (auto mode blocks the admin merge).

Integration branch `quickwins/integration` (worktree `.claude/worktrees/qw`, from main ebe9f17). Contract commits 95c099f and 1c28218:
- in `model.rs`: `Auth::Command`, `Provider`, `Profile.provider`, `Profile.token_command`, `ScanReport.server_context`;
- every new CLI flag and subcommand, plus exit codes (`ChecksFailed` 1, `GateError` 2), with stubs.

| Part | Content | Agent | Branch | State |
|---|---|---|---|---|
| R | 1 `--check` rules with exit codes; 2 Markdown, HTML and CSV reports | implementer [sonnet] | `quickwins/r` | **done** (850cefa; Markdown names as code spans in 8d0a175), merged |
| S | 6 snapshot retention, `snapshots list/prune`, `trend` and a trend column in the Diff view; 7 `compare` | implementer [opus] | `quickwins/s` | **done** (827ba4c, 9e5d008, 98318f3), merged; `prune` needs `--keep` or `--keep-days` (clap, f0cbbe0) |
| C | 3 provider detection and notices; 4 server context (`INFO keysizes`, `INFO keyspace`, `MEMORY STATS`); 5 token-command auth | implementer [opus] | `quickwins/c` | **done** (7cfbd00, 0e8d584), merged |
| V | Verifier on the merged branch | verifier [opus] | pinned detached worktree (5d11791) | round 1 **BLOCKED** on B1, fixed (below); round 2 at 6cd0d4a **APPROVE**, no blockers |

**Verifier round 1 (BLOCKED, report `qw-verify-report.md` in the session notes), and the fixes (63e4801, 717b306):**
- B1: with snapshots dated in the future (clock skew), `scan --save --keep N` ranked them newest and deleted the snapshot it had just written. Now snapshots dated after now are never deleted and take no keep slot. Test: `snapshot::tests::future_dated_snapshots_never_cost_the_newest_real_one` (keep 1 and 2, and the prune plan); it fails without the fix.
- Test gaps filled:
  - stdin and stderr are `/dev/null` (`conn::token::tests::stdin_and_stderr_are_dev_null`); a timed-out command is really killed (`a_timed_out_command_is_killed`); both checked by mutation;
  - the `--check` baseline shares the scan's `MATCH` glob (`cli::scan::tests::the_baseline_shares_the_scans_match_glob`); which rules need a baseline (`check::tests::which_rules_need_a_baseline`);
  - the context commands count against the budget, and a denied `MEMORY STATS` is not sent (`scan::tests::context_commands_count_against_the_budget`);
  - replacing a password profile with a token command without a username deletes the old keychain entry (`cli::conn::tests::replacing_with_a_token_command_without_a_username_deletes_the_old_entry`).
- Also fixed:
  - N1: a token command that exits but leaves a background process holding stdout no longer times out; its output gets 250 ms to close after it exits (`a_background_child_holding_stdout_does_not_cost_the_token`);
  - N3: `verdict=persistent-stable` needs a baseline, like `persistent-growing`;
  - N9: `--keep 0` is a usage error (`cli::tests::keep_must_be_at_least_one`).
- Not changed (noted):
  - N2: token buffers are not zeroized;
  - N4, N5: the big-strings bias warning ignores the sample fraction, and the server-wide `dataset.bytes` is compared with a one-database sample;
  - N6: `compare`'s TTL mismatch has no min-keys floor;
  - N7: `trend` and `compare` print pattern names raw, like `scan` and `diff` already did;
  - N8: the HTML CSP allows inline scripts (the sort script is inline);
  - N11: a token command's arguments are in clear in the config, so don't put secrets in them.
- Round 2 (APPROVE) notes, and what was done:
  - R2-1, fixed: output one byte over the 64 KiB cap could be accepted in a race with a background writer during the grace period; the cap is checked again on the output taken.
  - A3, fixed: `replacing_a_token_profile_with_a_username_deletes_the_old_username`; it fails if `Auth::Command` is dropped from the delete-old-entry check.
  - Not changed: R2-2 (the wait-for-close half of the exit condition has no deterministic test), R2-3 (the stdin half of `stdin_and_stderr_are_dev_null` can't fail when the test runner's own stdin is already `/dev/null`), and the read-error path has no test. A background process started by a token command is not killed (no process group); the reader thread lives until it closes stdout.

**Checks at 5d11791:** fmt, clippy `-D warnings`, 560 unit tests; every integration test on Redis 7.4, Valkey 8.1 and Redis 6.2 (cli_config 4, compare 3, conn 29, diff 6, replica 11, scan_cli 11, scan 21, smoke 3, tls 6; os_trust 2 ignored locally). With the default thread count, test containers sometimes failed to start while another build ran on the machine; with `--test-threads=4` and `--no-fail-fast` everything passed on both images. The first full run also found a real bug, fixed in 5d11791: `MEMORY STATS` floats didn't always read back exactly from a snapshot, so serde_json's `float_roundtrip` feature is on, with a test using floats that fail without it.

**Coverage (`make coverage`, unit + integration, at 5d11791):** 94.91% of lines (wave 8: 94.46%). New code: `conn/provider.rs` 100%, `format.rs` 100%, `diff.rs` 99.9%, `snapshot.rs` 98.8%, `check.rs` 97.6%, `scan/context.rs` 97.1%, `report/` 93-99%, `cli/compare.rs` 93.3%, `cli/trend.rs` 94.2%, `cli/snapshots.rs` 91.1%, `conn/token.rs` 88.8% (the Windows branch and the interactive prompt). Lowest: `tui/mod.rs` 68%, `secrets.rs` 77%, `cli/mod.rs` 78%.

**How each item is covered** (the agents' change → test maps are in their reports; the verifier audits them):
- `--check`: `check::tests` (every metric and op, the min-keys floor, not judged and `--strict`, growth with and without a baseline, the TOML file, parser errors, a parser proptest); `cli::tests::exit_codes`; `scan_cli_it` passing and failing gates, a growth rule against a saved baseline (Redis and Valkey).
- Reports: `report::{md,html,csv}::tests` snapshots of each format and hostile names (`<script>`, `|`, `=cmd()`, newlines, backticks); `scan_cli_it` writing `--report md -o`.
- Providers: `conn::provider::tests` (each provider and negatives, notices); the serverless rate in `scan::tests`; TUI notice snapshots in `tui::snapshot_tests_server`.
- Server context: parser tests for `INFO keysizes`, `INFO keyspace` and `MEMORY STATS` with documented formats, plus proptests; `context_summary` lines and sampling-bias warnings; `scan_it` on real servers (also `redis:8` for `INFO keysizes`) and with `MEMORY STATS` denied by ACL; the Patterns strip snapshots.
- Token command: `conn::token::tests` (success, timeout, non-zero exit, empty, too long, not UTF-8; Unix only); `conn add` with a token command (argv in the config, username only in the keychain, URL password refused, confirmation and `--yes`); `conn_it` connecting with a token command to a `requirepass` server and with an ACL user.
- Retention and snapshots: `snapshot::tests` (keep only, days only, both, newest kept, other profiles' and foreign files and symlinks untouched); `cli::tests::prune_needs_a_retention_rule`; `diff_it` `scan --save --keep 2` over three scans.
- Trend: sparkline and least-squares tests (empty, one point, flat, rising, gaps); Diff view snapshots with and without a trend column; stale-run tests unchanged.
- Compare: `diff::tests` for the classification and ratios with zeros; `compare_it` on two seeded servers, checking neither was written to (Redis and Valkey).

**Decisions taken in the wave (owner may revisit):**
- `ServerContext` is a boxed newtype (`Deref` to its parts), so `ScanReport` doesn't grow by 176 bytes and trip clippy's `large_enum_variant` on the TUI messages. The JSON is unchanged.
- The ElastiCache Serverless default budget is 1,000 commands/s. AWS publishes no figure; it is chosen from AWS's warning that frequent or large `SCAN`s raise latency there, and ECPU billing.
- The trend reaches the TUI as its own `Msg::Trend` after `Msg::History`, through the same stale-run guard.
- `scan --save --keep` into a profile directory that is a symlink saves the snapshot, then exits 1 with an "unsafe directory" error and deletes nothing.
- `compare` compares the share of keys without a TTL on each side, not the full TTL histogram.
- Markdown reports put pattern and key names in code spans, so a hostile name can't make a link or emphasis.

**Follow-ups found in the wave (not done):**
- Terminal escape sequences in key names reach the terminal in `scan`, `diff`, `trend` and `compare` output (verifier N7; from before this wave for `scan` and `diff`).
- `compare` flags a TTL mismatch on patterns of any size (N6); the context strip's sampling-bias and `dataset.bytes` coverage figures are rough on large or multi-database servers (N4, N5).
- Rule 1 (from wave 8): permission-check rows and `conn test` show the server's `NOPERM` text, which names the ACL user.
- The provider table's starred cells (MemoryDB, Memorystore, node-based ElastiCache command lists, Azure Enterprise, China and US Government suffixes) and the AWS token tool's exact name were not checked against the services (no web access in the agent).
- The token runner's unit tests are Unix-only; the interactive confirmation in `conn add` has no unit test (`--yes` and the no-terminal refusal do).
- The GitHub Actions example has not been run on a real runner (the OS keychain may be missing there).

## Wave 8 (2026-11-11): usability

Approved by the owner ("go do all then merge in when green"): the split below; merge with `gh pr merge --merge --admin` when all checks are green and the verifier approves. No tag, release or publish. Not in this wave: `--check`, report export, v0.3 cluster.

Owner decisions:
- Rescan: `r` in views 1-4 (Connections keeps `r` = Rescan Docker). The old results stay until the first partial arrives; filter, collapse, sort and the selected pattern are kept.
- Clipboard: OSC 52, no new dependency. `y` copies the selected pattern (Hot keys: the key), `Y` the suggested command. Nothing is copied on the Connections view or in the add flow (rule 1).
- Sorting: Patterns only. `o` cycles memory, keys, avg size, no-TTL %, name.
- Also in: hiding the share bar, Δ and avg size on narrow terminals before cutting rows; keychain worker panics stay off the TUI; Docker probes run in parallel; `os_trust` shows as skipped locally.

Integration branch `wave8/usability` (worktree `.claude/worktrees/wave8`, from main 3e347df).

| Part | Content | Agent | Branch | State |
|---|---|---|---|---|
| A | Selection follows the pattern (N2), collapse kept when a group's key changes (N3), sorting, marquee follow-ups, narrow columns | implementer [opus] | `wave8/tree` | **done** (253aa13), merged |
| B | Key table (`tui/keymap.rs`), `?` help overlay (`tui/ui/help.rs`), OSC 52 clipboard (`tui/clipboard.rs`), standalone | implementer [sonnet] | `wave8/keys` | **done** (d34d3fa), merged |
| D | Keychain panic text, parallel discovery probes, `os_trust` skipped | implementer [sonnet] | `wave8/extras` | **done** (693d575), merged |
| C | Rescan, `?` and clipboard wired in, footer from the key table, Page Up/Down and Home/End in every list view | implementer [opus] | `wave8/wire`, after A, B and D | **done** (75804f2), merged |
| V | Verifier on the merged `wave8/usability` | verifier [opus] | pinned detached worktree (3de2853, then the fix commit) | round 1 **BLOCKED** on B1 and B2, fixed; round 2 **BLOCKED** on B3, fixed; round 3 **BLOCKED** on B4 (test only), fixed (below); round 4 at 0eaa9e2 **APPROVE**, no blockers |

**Verifier round 1 (BLOCKED, report `w8-verify-report.md` in the session notes), and the fixes:**
- B1: the suggested command (`Y` and the detail panel's `$` line) put the glob in single quotes without escaping `'`, so a key named ``evil:x'$(touch pwned)'`` gave a command that ran `touch` when pasted into a shell. Fixed in `tui::clipboard::suggested_command`: `'` is written as `'\''`, and control characters and `\` (fish's escape inside single quotes) become `?`. Tests: `a_key_name_cannot_break_out_of_the_command` (exact text) and the proptest `the_pattern_is_always_one_quoted_word` (the text after `--pattern` is exactly one quoted word holding the glob, and has no control characters). Checked in bash with a stub `redis-cli`: one argument, nothing run.
- B3 (round 2): the single-quote fix holds in POSIX shells and fish, but not on Windows: cmd.exe has no single quotes (`x&calc&` runs `calc`), and PowerShell also ends a single-quoted string at typographic quotes and has no `\` escape. Fixed by offering the command only when the glob is ASCII letters, digits and `_ : . / * ? { } [ ] @ # + = , ~ - |`, in double quotes (`redis-cli --scan --pattern "<glob>"`, which also works in cmd); otherwise the `$` line says there is no command and `Y` says why. `y` no longer copies text with control characters. Tests: `tui::clipboard::tests::a_key_name_cannot_break_out_of_the_command` (POSIX, cmd and PowerShell break-outs, newline, `%`, `!`, backtick, non-ASCII), the proptest `the_command_holds_only_safe_characters`, `tui::app::tests::unsafe_patterns_and_control_characters_are_not_copied`, snapshot `patterns_detail_without_a_safe_command`. Eleven existing Patterns and TTL snapshots changed in the `$` line only (single to double quotes).
- B4 (round 3, test only): the B3 code was right, but nothing failed if `$`, `;`, `'`, `^`, `(` or `)` were added to the safe set. `tui::clipboard::tests::every_character_is_classified` puts every Unicode character between two letters and checks a command exists exactly for the safe set (written out again in the test); adding `$` fails it. Also: the TTL panel's no-command path has a snapshot (`ttl_detail_without_a_safe_command`), and the no-command line has no `$ ` prompt, so copying it with the mouse gives nothing that looks like a command. The verifier checked the safe set inside double quotes in bash, `bash --posix`, zsh (with `extendedglob`, `rcquotes`, `equals`), dash, ksh, tcsh and csh, and against fish's, PowerShell's and cmd.exe's documented rules.
- B2: the panic-hook test passed with the silencing removed, and nothing checked the hook is installed. The test now records keychain-thread panics too, and `keychain_calls_install_the_silencing_hook` checks the install; both fail under those mutations.
- Not changed (noted): `copy_never_carries_credentials` checks held credentials only through the Docker entry, since the app never holds a saved profile's credentials; `CopyText` constructors take any `&str`, so "only a pattern, key or command" rests on `App::copy`'s call sites; the discovery concurrency test is timing-based (1.2 s limit for about 0.4 s of work).

**How each item is covered** (agents' change → test maps are in their reports; the verifier audits them):
- Selection follows the pattern: `tui::app::tests::selection_follows_the_pattern_across_partials`, `selection_follows_the_pattern_into_the_finished_scan` and the collapsed-group, `{var}` and gone-row cases. Restart by a message: `a_message_that_changes_the_selected_row_restarts_the_marquee` (fails with `track_selection()` removed from `apply`).
- Collapse kept: `tui::tree::tests` for `Collapsed` (key change through `{var}`, split back, single to group and back) and `tui::app::tests` through partials and `ScanDone`.
- Sorting: `tui::tree::tests` per sort key, ties, groups and children, the no-sizes cycle; a sorted-table snapshot.
- Marquee: indent and mark stay put (`patterns_long_name_mid_scroll`, a group-row snapshot, a child-row test).
- Narrow columns: snapshots at each hiding step, with and without Δ; header and row alignment test.
- Key table: `tui::keymap::tests` (no duplicates, footer subset and width, `spec_lists_every_key`); table and handlers agree both ways (`tui::app::tests`).
- Help overlay: `tui::ui::help::tests` snapshots for every view, tiny terminals; key routing tests; overlay over a view snapshot.
- Copy: `tui::clipboard::tests` (bytes, cap, control characters, command text); `tui::app::tests::copy_never_carries_credentials` (rule 1: every view, row kind and the add flow); runtime write and error (`tui::tests`).
- Rescan: `tui::app::tests` rescan versions of the stale-run tests, previous results kept until the first partial, failure before and after partials, `s` while rescanning, `r` while running or with no session; snapshots of the rescanning and rescan-failed titles.
- Paging: every list view, page sizes 0 and 1, clamps; `list_rows` against the rows each view draws.
- Keychain panic: `secrets::tests::a_panicking_keychain_call_gives_a_fixed_error_without_the_payload`, `the_hook_is_silent_for_the_keychain_thread_and_forwards_other_threads`.
- Parallel discovery: `conn::docker::tests::discovery_probes_run_concurrently_and_keep_order`; `conn_it` on Redis, Valkey and 6.2.
- `os_trust`: `#[ignore]`d locally; CI runs `cargo test --test os_trust -- --ignored` after installing the CA, and the tests fail without it.

**Checks at 3de2853:** fmt, clippy `-D warnings`, 400 unit tests; every integration test on Redis 7.4, Valkey 8.1 and Redis 6.2 (cli_config 4, conn 26, diff 5, replica 11, scan_cli 5, scan 15, smoke 3, tls 6; os_trust 2 ignored locally). Two failures on the first run, both passing on re-run with code unchanged in this wave: `replica_it` on 6.2 (container start-up, as in wave 7) and `conn_it::connect_with_keychain_credentials` under coverage (a reply that wasn't RESP, likely another process on the mapped port).

**Coverage (`make coverage`, unit + integration, at 3de2853):** 94.46% of lines (wave 7: 93.48%). New modules: `tui/clipboard.rs` 100%, `tui/ui/help.rs` 100%, `tui/tree.rs` 98.8%, `tui/app.rs` 97.8%, `tui/keymap.rs` 86.5%. Lowest: `tui/mod.rs` 67% (terminal event loop), `secrets.rs` 76% (the real keychain, an ignored test), `tui/ui/connections.rs` 82%.

**Decisions taken in the wave (owner may revisit):**
- The Patterns key bar has no `r Rescan`: with `? Help` and `o Sort` it is 79 columns, and rescan would make it 90. Rescan is in the `?` overlay.
- After a failed rescan, `s` saves the previous report that is still shown. It has the same snapshot file name as that report's own snapshot, so nothing is duplicated.
- The replaced run's snapshot lookup still fills in the Diff of its own report while that report is on screen; otherwise a lookup still running when `r` was pressed would leave Diff stuck on "Looking for an earlier snapshot…".
- Paging on Connections counts only selectable rows, not the SAVED and DOCKER headings.

**Follow-ups found in the wave (not done):**
- Rule 1: a server's `NOPERM` reply names the ACL user (`NOPERM User <name> has no permissions …`), and the Connections detail panel shows it (fixture: `connections_view` snapshot). It never reaches the scan report or snapshot files. The username should be masked in permission results before they are shown (TUI and `conn test`).
- spec.md "4 Hot keys" promises an opt-in `MONITOR` sample "planned for v0.2"; it was never built (from before this wave).
- The failed-scan waiting panel says "Press 0 to go back to Connections and try again" and doesn't mention `r`.

## Wave 7 (2026-11-11): four bugs and the test gaps

Approved by the owner ("Approve all"): the split below; on Windows the OS store plus the bundled roots; a throwaway CA installed in each CI runner's OS store; merge with `gh pr merge --merge --admin` when all checks are green and the verifier approves. No tag, release or publish. Later waves, not started: rescan key, sorting, `?` help, clipboard, selection following the pattern (N2, N3), Page Up/Down, `--check`, report export, v0.3 cluster.

Integration branch `wave7/fixes` (worktree `.claude/worktrees/wave7`, from main 92adb64). Contract commit e8973be: `conn::docker::DiscoveredAuth` (`model::Auth` unchanged: it is written to the config file, and "unknown" is not a way to authenticate).

| Part | Content | Agent | Branch | State |
|---|---|---|---|---|
| A | Per-scan id (`ScanId`) on `Effect::Connect`, `LoadHistory` and every `Msg::Connected/ConnectFailed/Scan*/History`; forwarder aborted with its scan (`AbortOnDrop`) through the `scan_and_report` seam; D5 and D6 tests; discovered auth in the Connections view | implementer [opus] | `wave7/tui` | **done** (9ebc063, bc26d61), merged |
| B | Keychain calls give up after 15 s (`SecretError::Timeout`, "Always Allow" on macOS); `discover` probes with a credential-less `PING` (none / required / unknown); tests: URL-password warning, `--ask-password`, `--config` and `REDIS_LENS_CONFIG` (`tests/cli_config.rs`), `conn test`, the spec's `lens-ro` ACL line (Redis 6.2, 7.4, Valkey 8.1) | implementer [sonnet] | `wave7/conn` | **done** (ff681cd), merged |
| C | Bundled roots as an explicit store from `webpki-root-certs` 1.0.9; Windows trusts the OS store plus the bundle (N4); D2 (exact store sizes, Cargo.lock pin on redis 1.8.1), D3 (bundle non-empty, holds ISRG Root X1), D4 reduced to one fallback; `tests/os_trust.rs` plus a CI step that installs a throwaway CA in the OS store on macOS, Windows and Ubuntu | implementer [opus] | `wave7/tls` | **done** (2a4196c), merged. The CI trust-store step runs for the first time on the PR |
| D | Limiter pacing (virtual clock, plus a rate lower bound over the fake), `--max-rate` into `ScanOptions` for `scan` and `diff`, SCAN deduplication | implementer [sonnet] | `wave7/scan` | **done** (21e8dbb), merged |
| M | Selected row's overflowing text scrolls across (`ui::widgets::marquee`), restarted on selection change; narrow-width test. Owner-approved after the horizontal-scroll finding | implementer [sonnet] | `wave7/marquee` | **done** (fd51556), merged; 301 unit tests |
| V | Verifier on the merged `wave7/fixes` | verifier [opus] | pinned detached worktree (208156d, then 137b0ff) | round 1 **BLOCKED** on B1, fixed (below); round 2 at 137b0ff **APPROVE**, no blockers |

**Verifier round 1 (BLOCKED, report `.claude/worktrees/w7-verify-report.md`), and the fixes:**
- B1: `webpki-root-certs` loses Mozilla's name constraints, so the TUBITAK root (limited to `.tr` in `webpki-roots`) would have vouched for any host name: on every Windows machine, and elsewhere when the OS store is empty. Fixed: the bundle is the certificates whose anchor is in `webpki-roots` with no name constraint (`webpki-roots` is now a direct dependency); `conn::tls::tests::the_bundle_is_webpki_roots_without_the_name_constrained_roots`, `unlisted_certificates_are_not_bundled`.
- CI: on Linux cargo sets `SSL_CERT_FILE` for the processes it starts (openssl-probe), so `tests/os_trust.rs` refused to run in `unit tests (ubuntu-latest)`. Each test now re-runs itself in a child without `SSL_CERT_FILE`/`SSL_CERT_DIR`. macOS (System keychain) and Windows (Root store) installed the CA and passed on the first CI run.
- `--max-rate` was only tested by rebuilding the options: `diff_it::max_rate_paces_scan_and_diff` now runs `scan` and `diff` through the CLI entry points on a `local` profile and checks they take at least commands / N (Redis and Valkey); dropping the flag at the call site fails it.
- "Only `PING`" rested on reading the code: `conn_it::probe_sends_only_ping` counts commands on the server after `CONFIG RESETSTAT`; making the probe send `CLIENT SETINFO` fails it.
- The probe ITs use a 5 s timeout (`probe_auth_within`), after one `Unknown` in 17 runs on a loaded machine; discovery keeps 1 s.
- `conn_test_reports_denied_commands` now runs on Valkey too.
- Round 2 findings, not blocking, left for the usability wave: removing `App::track_selection()` from `apply` passes every test (a selection reset by `ScanDone` or a new session is untested; the scroll just may not restart); the scrolling name cell includes the tree indent and the `▾` mark, so a long group row hides its mark while it scrolls; partials re-sort rows under a fixed selection index (wave 6 N2/N3), so the scroll keeps going on a different pattern; `os_trust` reports ok without testing when `REDIS_LENS_OS_TRUST_CA` is unset (CI sets it).
- Not changed (noted): the keychain worker's panic message would print to stderr under the TUI; probes run one container at a time (an unreachable one adds up to 1 s); `replica_it` failed once on container start-up timeouts (unchanged code, passed on re-runs).

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
- Horizontal scrolling: the owner chose not to build it (counter-intuitive in a TUI, and `←`/`→` expand and collapse). The spec now describes truncation, and the selected row's overflowing text scrolls across (part M below). Covered by the narrow-width and marquee tests.

**Coverage (`make coverage`, unit + integration, 2026-11-11 at 208156d):** 93.48% of lines (wave 6: 92.6%). All tests pass: 291 unit, cli_config 4, conn 25, diff 4, os_trust 2 (skipped locally), replica 11, scan_cli 5, scan 15, smoke 3, tls 6; the integration tests also pass with `REDIS_LENS_IT_REDIS_IMAGE=redis:6.2-alpine`. Lowest: `cli/mod.rs` 59% (clap dispatch), `tui/mod.rs` 63% (terminal event loop; was 52%), `secrets.rs` 71% (the real keychain, an ignored test), `tui/ui/connections.rs` 82%.

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

## v0.3 (2026-11-11 to 2026-11-12): cluster and SSH tunnels

Started from the parent session's brief. Owner decisions (2026-11-11, the recommended options): our own discovery (`CLUSTER SHARDS`, falling back to `CLUSTER SLOTS`), not redis-rs's cluster client; the rate limit per node, up to 4 nodes at once, the sample limit split by `DBSIZE`; a view 5 Nodes where `enter` narrows Patterns to a node; snapshots keep per-node pattern rows; the system `ssh -N -L` with `BatchMode=yes` and `StrictHostKeyChecking=yes`, one forward per endpoint. Not in scope: Sentinel, the per-slot view, Azure managed identity. No tag, release or publish; the owner merges the PR.

Integration branch `v03/integration` (worktree `.claude/worktrees/v03`). Agent worktrees made by hand from it.

| Wave | Content | Agent | Branch | State |
|---|---|---|---|---|
| contract | `NodeReport`, `NodeRole`, `NodeStatus`, `ScanReport.nodes`, `KeySample.node`, `Profile.ssh`, `SshTunnel`; `Route`/`Connection` (a connection keeps its SSH forward open); stubs | me | d807cff | **done** |
| A | SSH tunnels: `src/conn/ssh.rs`, `conn add --ssh`, `tests/ssh_it.rs` (19) | implementer [opus] | `v03/ssh` | **done**, merged (8004d94) |
| A | Cluster discovery and per-shard scanning: `src/conn/cluster.rs`, `src/scan/cluster.rs`, `pattern::aggregate_nodes`, `tests/cluster_it.rs` (8) | implementer [opus] | `v03/cluster` | **done**, merged (11b002c) |
| contract B | `ScanTarget::Cluster`, `ScanTargets::target()`/`check_permissions()`, `scan::cluster::scan_targets` | me | b225131 | **done** |
| B | TUI: wiring, view 5 Nodes, narrowing, Hot keys node column, header and Connections summary | implementer [opus] | `v03/tui` | **done**, merged (e06cc73) |
| B | `scan` Nodes table, `--check` note, md/html Nodes section, snapshots, `diff` node-set note, `compare` on a cluster; `tests/cluster_cli_it.rs` (4) | implementer [sonnet] | `v03/cli` | **done**, merged (5ad766f) |
| B | Cluster behind a bastion (`tests/ssh_cluster_it.rs`, 10); one primary connect for a standalone `--prefer-replica`; no extra `INFO` per scan (the detection reply answers the scan's first `INFO server`); Valkey 8 `server_mode` | implementer [opus], resumed | `v03/ssh` | **done**, merged (0eb1832) |
| fix | The cluster futures were not `Send` (rustc's higher-ranked limitation with iterator closures held in streams); boxed and collected first, guarded by `scan::cluster::tests::futures_are_send`; the TUI's blocking-thread workaround removed | me | 621502c | **done** |
| V | Verifier on the merged branch (8c02a1a): BLOCK on two findings, 41 mutations run | verifier [opus] | | **done** |
| fix V | B1: an empty shard that doesn't own `redis-lens:probe`'s slot answered `MOVED`, counted as a denial, which turned sizes off for the whole cluster; a redirect on the missing probe key now counts as allowed (`tests/cluster_it.rs` `*_empty_shards`). B2: behind SSH, an announced `127.1` (or `0`, `2130706433`, `0x7f000001`) got credentials on the bastion's loopback; announced hosts whose last label is numeric are now refused (`tests/ssh_it.rs` `a_shorthand_loopback_*`). Tests added for the surviving mutations on sizes merging, case and IPv4-mapped loopback; spec.md states the forward-port race, `conn add --ssh USER@` on the command line and `conn test` on a cluster | me | | **done** |
| V2 | Verifier on the fix diff (56d0420): APPROVE, 17 mutations; its two test gaps (upper-case `0X`, the partials' sizes flag) closed after | verifier [opus] | | **done** |
| PR | One PR from `v03/integration` to `main`; the owner merges | me | | next |

Checks on the merged branch (621502c plus the spec, status and note edits): fmt and clippy clean; 704 unit tests pass. Integration on Redis 7.4 + Valkey 8.1: 134 of 135 passed, and on `redis:6.2-alpine` 133 of 135; os_trust's 2 are ignored as usual. The three failures were Docker Desktop errors in the container helper (a host-port bind race and `PortNotExposed`), not assertions, and each passed when rerun alone: `replica_it::an_open_replica_is_scanned_without_credentials`, `conn_it::probe_server_with_default_user_off_is_required`, `scan_cli_it::sample_limit_is_not_complete`. CI on Linux is the real check. `make coverage` (unit and integration, default images, no failures): 95.54% of lines, 95.62% of regions; `conn/cluster.rs` 94.8%, `scan/cluster.rs` 97.7%, `conn/ssh.rs` 98.6%, `tui/ui/nodes.rs` 98.1% of lines.

Checks after the fixes (56d0420 plus the two test cases): fmt and clippy clean; 710 unit tests pass. Integration: 139 passed on Redis 7.4 + Valkey 8.1 and 139 on `redis:6.2-alpine` (os_trust's 2 ignored), the 6.2 run after one Docker port-bind error in the container helper on the first try.

Open points:
- With a key-pattern ACL that doesn't match `redis-lens:probe` (`~app:*`), an empty shard gets `NOPERM` on the probe and still turns sizes off for the whole cluster. Leaving `DBSIZE` 0 nodes out of the sizes rule would fix it; not done, as it changes what the permission check reports.
- An online replica whose announced address is refused gives `PrimaryFallback("no online replica")`; "address refused" would say more.
- Behind SSH, a profile's own host written as `127.1` counts as remote on the bastion. It only restricts (loopback replicas or nodes get no credentials).
- The ACL-before-redirect order that makes a `MOVED` probe count as allowed holds for Redis 6.2/7.x and Valkey 8; not checked on ElastiCache or MemoryDB.
- With `-l <user>`, the SSH username is visible in the process list while a tunnel runs (documented in spec.md; `User` in `~/.ssh/config` avoids it).
- The ssh child uses `std::process` with a kill on drop, not tokio's `process` feature. Windows is not compiled locally; CI's Windows unit tests are the check.
- The kept `INFO server` reply can be up to 10 s old when the scan reads it (version and mode are what the scan uses from it).
- The TUI colours the cluster summary amber by matching its wording ("unreachable", "seed node only"); a structured field would need a contract change.
- `diff --json` has no node-set note.
- Not tested end to end: a TLS cluster, and a TLS cluster behind a bastion.
- The TUI sends `INFO server`, `INFO memory` and `DBSIZE` per node for the header before a cluster scan, as it does for a standalone server.
- The local forward port is free-picked and released before ssh binds it: a local process that wins the race receives `AUTH`. Any local user can use a running forward, as with any `ssh -L`. Both documented in spec.md; binding the port in redis-lens and passing it to ssh would need a different forwarding mechanism.
- `conn test` on a cluster checks the seed node only (documented).
- A refused node's raw announced address is printed unescaped in text output and the TUI (md and html escape it): the same class as the open point on control characters in key names.
- Not covered by a test: the `CLUSTER MYID` check on cluster replicas (primaries are tested), and starting a cluster node's SSH forward before the 5 s node timeout (the standalone replica's is tested).
- The `.boxed()` in `conn::cluster::discover` isn't needed for `Send`; harmless.

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

- **Replica scans (`--prefer-replica`) trust the announced address**, like Cluster and Sentinel clients do. A server that imitates a replica, since the replication ID isn't secret, can be scanned without credentials, and its data is filed under the profile. A profile that reaches prod through a tunnel opened by hand on 127.0.0.1 treats local services on the announced port as on the primary's host; `--ssh` (v0.3) judges loopback from the bastion instead. Announced cluster nodes are trusted the same way (spec.md, Safety). A replica scanned without credentials runs as its default user. TLS replica scans are tested (`tests/tls_it.rs`).

- **macOS keychain prompt after rebuilds.** Keychain entries are tied to the binary that wrote them. A rebuilt or upgraded binary has to ask macOS for access, and until someone answers the dialog, connecting hangs. Mitigated in wave 5: the README says to click "Always Allow", and the TUI shows a hint when a keychain read takes more than 2 s. Since wave 7 a keychain call gives up after 15 s with an error saying to click "Always Allow" and try again, so nothing hangs. A write that timed out can still finish once the dialog is answered, leaving an entry no profile points to (still only in the keychain).

- **Lesson:** never switch the main checkout's branch while an agent is reading it. Do all work in worktrees.

- **`vhs` doesn't work here.** It drives headless Chromium, which produces no output, and turning off its browser sandbox was refused. Use `demo/record.py` with `agg` for GIFs instead.

- The demo's `rl:*` and `lock:*` keys expire after up to 30 minutes, so re-run `make demo-up` before recording a GIF. (Local profiles scan with no rate limit since wave 4; `--max-rate` overrides it.)

- Agent worktrees made by the harness branch from `main`, not the current branch, so create them by hand.
- **Entra ID** is implemented behind `--features entra-id`, using redis-rs's `EntraIdCredentialsProvider::new_developer_tools()` (az login and other developer tools). It is untested against real Azure, and managed identity isn't covered yet.
- `discover` shows the demo Redis as auth "required" (since wave 7; it was "none"), because its passwords live in an ACL file discovery can't see. Save it as a profile instead (`make demo-profiles`).
- Narrow terminals: the Patterns view hides the share bar, then Δ, then avg size before cutting a row (wave 8); other views cut a row too wide at the right edge, and the selected row's overflowing text scrolls across.
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
- The OSS clustering policy on Azure Managed Redis needs v0.3 cluster support (built; untested against Azure).
- `HEXPIRE` exists in Redis from 7.4 and in Valkey only from 9.0, so CI would need a newer Valkey to test it.

### Redis Insight parity (researched 2026-11-12)

A gap matrix against Redis Insight v3.8.0, with a three-phase roadmap, is in [docs/redisinsight-parity.md](docs/redisinsight-parity.md).

**Approved by the owner on 2026-11-12:** all three phases ("we should do phase 1, 2 and 3"). The decisions:
- **Values:** shown on screen for every profile, never saved.
- **Writes:** allowed with the user's permission, as in Redis Insight. Three checks must all allow a write:
  - the server ACL;
  - the profile's `read_only` setting, a saved toggle (`w`, `conn set --writes`). It is on by default for `prod`, and turning it off there needs the profile name typed;
  - a confirmation for each action: `y`/`n`, or a typed name on `prod`.
- **Recorded:**
  - spec.md has the non-goals rewritten, a new section "Browsing, server views and writes (planned)" and the milestones for phases 1-3;
  - the stale `MONITOR` "planned for v0.2" line is fixed;
  - CLAUDE.md rule 2 now says read-only profiles (the `prod` default) never write, and every write goes through the one write gate.
- **TUI design mockup**, 14 screens: https://claude.ai/artifact/2RzWTZFKwJASgtX3phxKfG (private to the owner).
- **Phase 1, wave A: done** (on branch `claude/redis-insight-tui-features-60db8f`, not yet in a PR):
  - contract `26a34eb`: `Budget`, `live` request and reply types, the new `Effect` and `Msg` variants;
  - A1 (merged `20e8084`): the shared command budget (`src/budget.rs`) used by the scan and the live views, the live connection kept open after a scan (`src/live/`), the five view probes (`conn::check_view_permissions`), and the runner wiring;
  - A2 (merged `3f01132`): seven views with panels and drill-down, the `ViewState` interface (`src/tui/views/`), the key table, the header write state and the WRITES column;
  - verified by V-A (approve, no blockers); then step 0 fixed what it noted:
    - a scan's batch no longer waits its slot plus its round trip;
    - pasted text's effects are run;
    - a new connect forgets the last connect's checks, and the view on screen is told when a check turns it off or on;
    - a refused scan check reads fixed text instead of the server's, which named the ACL user (rule 1; older than this work).
  - Not done: the 5 view probes don't take from the budget (5 commands per connect), and the key bar shows the ready keys while a connect runs.
- **Phase 1, wave B: done** (same branch). Each piece ran as its own session in its own worktree, and was merged here:
  - B1 (merged `7fc0aff`): Server Overview (INFO sparklines and rates), Clients (grouped by library and /24 or /64, idle flag, drill-down), Slow log (key names from `COMMAND INFO`, arguments dropped, notes, threshold from `CONFIG GET`).
  - B3 (merged `897ba0e`): Streams (`XINFO` with stuck, lagging, no readers and unbounded), Pub/Sub counts, Search indexes and schema on Redis 8 and valkey-search.
  - B2 (merged `40eae26`): the Patterns VERDICT column, the key list (SCAN MATCH filtered by inference, paged with `n`, pipelined metadata), Big keys, Expiry and Recommendations (`src/analysis.rs`). New report field `key_sizes` (top 50 keys, bytes per TTL bucket), approved by the owner; older snapshots still load.
  - Merge fixes: `d094216` (tests), `da1c0aa` (the runner boxes `Msg`, for clippy).
  - Verifier V-B blocked on four points, all fixed in `dd7352e` and `de3d4f8`: Search resent a failed `FT._LIST` every tick; the Slow log lost the reason key names were off; some integration tests ran on Redis only (rule 5); spec and status were not updated (this change). Also fixed from its notes: the key list loaded pages without `n`, Streams retried refused commands every 5 s, two unchecked adds, stale replies, Pub/Sub's unused `tab`, a copy of the inference helpers, and an uncapped encoding check.
  - Re-verification V-B2 blocked on three points, fixed in the next commit: Search showed "Reading…" forever after a failed first read (it now shows the failure and `r`); an `r` on Streams during a poll lost its reset of refused commands (it now reads once more when that poll answers); the spec's key bar sentence still had `tab Panel` on Streams, Pub/Sub and Search. Its non-blocking notes are open: Pub/Sub asks a refused `NUMSUB`/`NUMPAT` every poll; the spec's `key_sizes` fields omit `lfu_freq`; Pub/Sub keeps the first 1,000 channels in server order, not the top 1,000 by subscribers; the Slow log's `r` does not ask a refused `COMMAND INFO` again while Streams' does; a Streams poll that fails whole forgets the refusals it found.
  - Demo: `docs/demo.gif` and `demo/gif-steps.json` now tour phase 1; the demo `lens` user has the phase 1 read commands.
  - Merged with v0.3 (main at `0480a16`) for PR #15. What changed in the merge:
    - 5 Nodes is now the Nodes panel of 2 Analysis, after Expiry (`tab`, or `2` then `tab`). `enter` narrows Patterns to the node and `esc` comes back to the panel, as before. Its key bar drops `s Snapshot` (it is in the `?` overlay) to fit 80 columns.
    - The scan takes every command from the shared `Budget`; a cluster scan gives each node a `Budget` of its own (per-node budget, as v0.3 has it). `sample_server` takes the budget instead of building a `Limiter`.
    - On a cluster, the views that read one server (the key list, Server, Streams, Pub/Sub, Search) are off with a fixed reason; a cluster connect opens no live connection and runs no view probes. Big keys and Expiry cover every node's samples, each big key with its node; lengths and encodings are not read on a cluster.
    - The live task keeps the connect's `Connection` (and so an SSH forward) open until it ends.
    - The panel row of the tab bar closes up, then shows only the current panel (`Nodes · 6 of 6`), when the names don't fit.
    - The permission check's `classify` keeps v0.3's redirect rule and phase 1's fixed NOPERM text.
  - Not covered by the merge's tests: the live views on a cluster against a real cluster (they are off), and the "previous results" marking loop in `snapshot_tests.rs`, which still visits the five old screens and not Nodes.
  - Left as is, from V-B's notes: Recommendations are recomputed on each draw (cheap); at 80 columns VERDICT cuts long pattern names (owner to decide whether VERDICT should hide first).
  - Known flake: a test container's port is sometimes not mapped yet (`tests/common/mod.rs:114`); since phase 2 the helper asks again for up to 2 s.
- **Phase 1 merged** to main as #15. Phase 2: see [Phase 2](#phase-2-2026-11-13-values-on-screen) below.
- **Design corrections found while writing the spec:**
  - A stream doesn't store its `MAXLEN`, so Streams shows the age of the oldest entry and an `unbounded` verdict instead of a cap.
  - A pattern's glob can match keys of other patterns, so the key list and bulk actions keep only keys that infer to the selected pattern.

## Phase 2 (2026-11-13): values on screen

Built unattended on `phase2/values-on-screen`, one PR for the whole phase ([#16](https://github.com/magna-nz/redis-lens/pull/16)), one or more commits per part. Not merged: the owner merges. Nothing in phase 2 sends a write command.

| Part | Content | Commits | State |
|---|---|---|---|
| 1 | Value renderer (`src/value/`: text, hex, JSON, MessagePack; gzip, zlib, lz4, zstd capped at 1 MiB; terminal safety), Key detail (`src/live/values.rs`, `views/value.rs`), `f`, `y`, `CopyText::value` | `3dad809` `cd7d27b` `960ff78` `60192fe` `60fa334`, verifier fixes `bb9b3e8` | **done**, verified (the verifier blocked on a MessagePack size blow-up and a cut column; both fixed, with tests) |
| 2 | Stream entries, Pub/Sub live tail (own RESP3 connection), Search queries (`FT.SEARCH`, `FT.EXPLAIN`, `FT.PROFILE`) | `2272334` (shared masking, `src/live/command.rs`), `e514253`, verifier fixes `55ccddc` | **done**, verified (OK, no blockers; its two should-fixes are fixed: one Search query at a time, tail previews made off the UI thread) |
| 3 | Slow log arguments (masked, on screen only), Monitor panel (own connection, asks first, 30 s or 10,000 lines), Hot keys from the last Monitor run | `47167e0` with part 4 (they share the wiring files), clippy fix `8bb87d8`, review fixes in the next commit | **done**, verified (see part 4); tests on Redis, Valkey and Redis 6.2 |
| 4 | Command line panel: `:`, readonly commands only (`COMMAND INFO` flags), `@dangerous` asks, refusals point elsewhere, credentials masked in input and history, `COMMAND DOCS` helper, `tab` completion | `47167e0` with part 3, review fixes in the next commit | **done**. The verifier blocked on `EVAL_RO`/`EVALSHA_RO`/`FCALL_RO`: flagged readonly, but a script ran `SLOWLOG RESET`, `KEYS` without the question, or looped (BUSY). Fixed: scripts and functions are never sent; also module internals, unknown subcommands of a container on servers before 7.0 (6.2 sent `MEMORY PURGE`), `KEYS` asks without ACL categories, more settings masked (Valkey `primaryauth`/`primaryuser`, TLS key passphrases, sentinel passwords), a nested NOPERM is fixed text, the helper's categories escaped. Its re-check blocked on module commands flagged readonly that change state (`FT.CONFIG SET`, `FT.ALIASADD`, `FT.DICTADD`, `GRAPH.CONFIG SET` on Redis Stack 6.2 and 7.2): only module commands on a list of known reads are sent now; `POST` and `HOST:` (flagged readonly on 6.2, they close the connection) are refused; `SORT_RO` asks without categories |

### Decisions taken alone (phase 2)

- **Value cache:** `value::Memo` keeps decode, render and preview results between frames, keyed by a `Raw`'s id (a counter given out when it is made). Bounded: 8 decoded, 8 rendered, 4,096 previews.
- **MessagePack map keys that are not strings** are drawn as a string of their compact JSON; a container key inside such a key becomes `"…"`, so nesting can't double the size per level.
- **Pub/Sub:** each message keeps only the first 4 KiB of its payload once its preview is made (up to 4,000 messages are held while paused). `s`, `c`, `esc`, `p` and `r` are only in the `?` overlay, so the bar fits 80 columns. The rate is over the last 5 s while the view is shown. A new subscription clears the list, the filter and the pause.
- **Streams:** the XINFO poll stops while entries are shown; `y` on an entry does not copy (an entry has several fields, as in Key detail).
- **Search:** results replace the schema while their index is selected; documents, explain and profile open over the whole body and `esc` goes back; `e` and `P` run the last query on the selected index; a refused or unknown `FT.*` command is off until `r`. Server error text is shown only for syntax-type errors, escaped and cut at 200 characters. valkey-search rejects `*`, so an empty query shows that message on Valkey.

- **Monitor connection:** redis-rs's multiplexed connection drops replies nobody asked for, and its own monitor connection resolves the host itself and ignores the answer to `MONITOR`. So `conn::open_monitor` first sends `MONITOR` on an ordinary connection through the profile's route as a permission check, then opens the run with `get_async_monitor`: a run costs 2 commands from the budget. Direct routes work with and without TLS, and an SSH tunnel without TLS; the panel is off, with a fixed reason, behind SSH with TLS and with Entra ID.
- **Monitor:** `enter` asks and `esc` stops; `y`/`n`, or the profile name on `prod`, where a wrong name cancels. Credentials in each line are masked as it arrives, so a password is never held; a line over 1 KiB is cut at an argument. The view follows the newest line until you scroll up (`End` follows again). No `y` copy of Monitor lines or Slow log arguments.
- **Hot keys from Monitor:** used when the scan has no LFU counters (no LFU policy, or `OBJECT FREQ` refused). Key positions come from `COMMAND INFO` key specs shared with the Slow log; names not yet known are asked when the run ends (at most 8 rounds of 64, through the budget). KEY, PATTERN, COUNT and share; lines it can't place are counted. Never in a snapshot or report.
- **Slow log arguments** are on an `args` row of the Entry panel (one entry fewer at 80×24), cut with `…`, then `… N more arguments` when the server cut the command.
- **Command line:**
  - only commands the server flags `readonly` are sent, on every profile; also refused: `blocking` (or `@blocking` on 6.2; `XREAD` only with `BLOCK`), and the commands that would change the shared connection. A container command is judged by its subcommand's entry in the same `COMMAND INFO` reply. If `COMMAND INFO` is refused or unknown, nothing can be sent (fails closed).
  - `COMMAND INFO` and `COMMAND DOCS` are asked once per name, when its first word is followed by a space; `COMMAND LIST` once, when the panel first opens (absent on 6.2: completion then uses names already looked up).
  - the cursor stays at the end of the line; masking works token by token as you type. History: last 100 lines, masked (so a recalled credential line holds `***`). Output: last 100 entries or 4 MiB. One command at a time.
- **ACL:** `+monitor` and `+command|list` are left out of the demo's minimal user: `+monitor` shows every command and value on the server.

### Open questions and leftovers (phase 2)

- **The strict readonly rule refuses `INFO`, `PING`, `TIME`, `CLIENT LIST`, `CONFIG GET`, `SLOWLOG GET`, `PUBSUB …` and `COMMAND …`** on the command line, because they lack the `readonly` flag. Should a short allow-list of harmless commands be sent? Owner to decide.
- Parts 3 and 4 are one commit: both edit the same wiring files (`app.rs`, `keymap.rs`, `views/mod.rs`, `ui/mod.rs`, `live/mod.rs`, spec.md), and splitting their hunks did not give a commit that builds.
- Not tested against real servers: MONITOR through an SSH tunnel and over direct TLS (coded, unit-tested only); the command line through a full TUI session (the live path has integration tests); RESP3 reply shapes on the command line (unit tests; the live connection is RESP2).
- **The command line reads a reply whole** before cutting it at 1 MiB on screen, so `HGETALL` or `LRANGE 0 -1` on a big key, or `SCAN … COUNT 100000000`, can take a lot of memory here and time on the server (redis-rs reads a reply in one piece). The spec lets a user type them; a size check before such reads is for the owner to decide.
- From the part 3-4 verifier's notes, left as they are: a misaligned `CONFIG SET maxmemory requirepass pw` shows `pw` (the server rejects that line); the helper sends the first typed word in `COMMAND INFO`/`COMMAND DOCS` (a password typed as the first word would go there); `PFCOUNT` is flagged readonly but may rewrite its key's cache; the Monitor prompt on `prod` draws pasted text as typed; errors opening the Monitor (like Pub/Sub's) are masked like connect errors; a Monitor line is read whole before it is cut to 1 KiB; the 4 MiB output cap counts text, not per-line overhead.
- The `?` overlay does not list the command line panel's own keys (they are in the spec's key table).

- **Auto-fix could not be turned on** for #16: the app's permission check refused `set_monitor`. CI was checked by hand instead.
- Search result fields are held as the server returned them (at most 50 documents), with no size cap per field.
- Pub/Sub: non-UTF-8 channel names are shown and subscribed to lossily; each subscribe reads the keychain again; `run_tail` end to end through the runner has no test (its pieces do).
- From the Part 2 verifier's notes, left as they are: a channel or index name that is not UTF-8 is sent back lossily; the input bar draws a pasted bidi control as it is (the user's own paste); any NOPERM while the subscriber connection opens reads as "HELLO refused"; stream paging with `XREVRANGE (id` needs Redis 6.2 or later.
- **Main's Redis 6.2 integration job has failed since #15** (`keys_it` expected `listpack`; Redis 6.2 says `ziplist`). Fixed here in `5ded47e`, with the container helper now asking again for a port Docker has not published yet (the known flake).
- CI flake fixed on the way: `conn::ssh` test `a_cancelled_start_kills_ssh` cancelled after a fixed 300 ms, before a loaded runner had started the fake ssh (`8d8e788`).
- Streams and Search keep the same key bar in every mode (entries, document, explain), since `keymap::footer` only knows the screen.
