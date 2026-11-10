# redis-lens status

Where the build stands. Update this file in the same PR as the work it describes. What the tool should do is in [spec.md](spec.md).

**Current milestone:** v0.1
**Last updated:** 2026-11-10

## Done

- 2026-11-10: Public repo created, with `main` protected (code-owner review, required CI checks, no force-push) and CODEOWNERS added.
- 2026-11-10: Crate scaffold: shared types (`src/model.rs`), module contracts, the CLI shell, and container test helpers ([PR #1](https://github.com/magna-nz/redis-lens/pull/1)).
- 2026-11-10: CI (fmt, clippy, tests on 3 operating systems, integration, MSRV 1.89, package, demo) and the release workflow (5 targets, GitHub Release, crates.io).
- 2026-11-10: Demo environment: an ACL Redis on 6390 and a Valkey on 6391, seeded with planted problems.

## In progress

| Work | Branch | Owner |
|---|---|---|
| Scanner: pattern inference, aggregation, INFO parsing, SCAN sampling | `wave2/scanner` | implementer agent |
| Connections: keychain, config, connection strings, connect, permission check, Docker discovery, `conn` commands | `wave2/conn` | implementer agent |

## Next

1. Review and merge the two wave-2 branches into `scaffold`, then run all tests and the demo.
2. The TUI: Connections and Patterns views, plus `redis-lens scan`.
3. An independent verification pass over the combined diff.
4. Merge PR #1 once CI is green.
5. Tag v0.1.0.

## Needs a decision or action from the owner

- Add a `CARGO_REGISTRY_TOKEN` repository secret before the first release.
- Decide whether to publish a placeholder 0.0.1 to reserve the crate name early.

## Known issues and notes

- Agent worktrees made by the harness branch from `main`, not the current branch. Until `scaffold` is merged, create worktrees by hand from `scaffold`.
- Entra ID: redis-rs 1.8 has an `entra-id` feature, and a build with it enabled compiles. Wiring it up is not done yet.
- For Redis ACL rules passed as server arguments, use one argument per word. See `tests/common/mod.rs`.
