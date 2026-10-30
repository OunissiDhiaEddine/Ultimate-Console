# Contributing

The work is split into work packages (WP-00 … WP-11, see `docs/PLAN.md` §4)
that are built in parallel. These rules keep the parallel branches from
stepping on each other.

## Branches and PRs

- One work package per branch and per PR. Title: `WP-xx: <short summary>`.
- Branch from the latest `main`; rebase (don't merge `main` in) before review.
- Open PRs as drafts until CI is green and the manual checklist exists.
- A PR description says what was done, what was verified where (CI, Linux
  type-check, real PC), and what still needs Dhia to test on the PC.

## Where code goes

- Each WP owns its crate (`crates/uc-<name>`) and its `uc-probe` subcommand
  group. Touch other crates only when the change is needed for yours, and say
  so in the PR.
- `uc-core` holds the shared contracts: `Config`, `RuntimeState`, the IPC
  messages, paths and logging. Changing them affects everyone:
  - **Adding** a config field: give it a default; existing files must keep
    loading. Adding an IPC request/response/event variant: fine.
  - **Renaming/removing/changing meaning**: bump `config::SCHEMA_VERSION`
    (with a migration) or `ipc::PROTOCOL_VERSION`, and call it out in the PR.
- Crates expose plain Rust APIs and never depend on `uc-agent`. Policy
  (e.g. "only in gaming mode") lives in the agent, not in the crates.
- Shared dependencies go in `[workspace.dependencies]` in the root
  `Cargo.toml`; crates use `foo.workspace = true`. Use the workspace's
  `windows` crate version and enable only the features you need.

## Code rules

- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` must pass (CI runs them on `windows-latest`).
- Every `unsafe` block gets a `// SAFETY:` comment.
- Pure logic gets unit tests that run anywhere. Hardware behaviour gets a
  `uc-probe` subcommand plus a checklist in `docs/tests/WP-xx.md` (about five
  minutes of steps for the real PC).
- Log with `tracing`, never `println!` in library crates.
- Nothing may need admin rights unless the plan says so (§2.2).

## Commits

Small, focused commits with an imperative subject line, e.g.
`uc-display: restore saved topology`.
