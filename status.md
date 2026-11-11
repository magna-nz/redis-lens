# redis-lens status

Where the build stands. Update this file in the same PR as the work it describes. What the tool should do is in [spec.md](spec.md).

**Current milestone:** v0.2 done on main; waves 6 and 7 merged (PRs #10, #11); wave 8 (usability) in progress; then v0.3 (cluster) or the feature ideas below
**Last updated:** 2026-11-11 (wave 8 verified; PR #12)

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
- Narrow terminals: rows are drawn at fixed column widths and cut off at the right edge (the spec said "scroll horizontally" until wave 7; the owner chose truncation plus scrolling text on the selected row). Hiding low-priority columns at narrow widths is a candidate for the usability wave.
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
