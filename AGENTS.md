# Agent Instructions

## Compatibility

- `CLAUDE.md` is the Claude Code bridge to this file.

## Project Context

- This repository contains SCV, a Rust workspace for a small extensible agent
  runtime with a Unix-socket daemon, supervised components, a Ratatui client,
  and a stdio endpoint for headless clients.
- Read `README.md` for the human development flow.
- Treat `docs/architecture.md` as the dependency-boundary source of truth and
  the other files in `docs/` as the final-state contracts for their subjects.
- Keep documentation and code aligned when introducing or changing behavior,
  APIs, data shapes, dependencies, or development commands.

## Git Workflow

- Follow semantic versioning for releases. Increment the patch component by
  `0.0.1` for features and bug fixes; use minor or major bumps only for the
  corresponding semver-compatible or breaking changes.

- For each feature, define the intended final behavior and keep the relevant
  final-state documents in `docs/` aligned as part of the same change. Do not
  require a separate approval round or split implementation into artificial
  vertical slices when the requested outcome is clear.
- Implement approved work in a dedicated sibling worktree. At delivery, run the
  required checks, commit, push, merge to `main`, publish affected crates, and
  remove the completed worktree and local task branch.

- The default branch is `main` and the canonical remote is `origin`.
- Treat existing uncommitted changes as user-owned unless told otherwise.
- Prefer git worktrees for parallel or unrelated agent work so multiple agents
  can develop concurrently without colliding. Put worktrees in a sibling
  directory such as `../scv-<task>`; this repository does not define an ignored
  project-local worktree directory.
- Treat commits as explicit delivery boundaries, not progress checkpoints. Do
  not commit after every file, subtask, test, or agent turn.
- Default to one cohesive commit for one requested outcome. Split only when the
  parts are independently reviewable and revertible.
- Ordinary task completion does not authorize commits, pushes, integration,
  worktree removal, or branch deletion.
- Do not infer a review, release, or direct-integration action from repository
  visibility, ownership, or the absence of branch protection.

## Coding and Documentation Rules

- Use stable Rust 1.88 or newer, Rust 2024 edition, `cargo fmt`, and Clippy with
  warnings denied.
- Keep protocol types and framing in `scv-protocol`, default socket discovery
  and daemon control helpers in `scv-client`, loop and extension traits
  in `scv-core`, provider transport in provider crates, tool implementations in
  `scv-tools`, policy and session authority in `scv-server`, and terminal
  presentation in `scv-tui`. Keep the shared chat-channel bridge in
  `scv-channels` and each platform's transport in its own crate (WeChat in
  `scv-clawbot`). Preserve `server -> clawbot -> channels -> client -> protocol`;
  TUI and channel crates must not depend on server.
- All current and future long-running components must implement the server's
  `Component::run(cancel, HealthReporter)` contract and run under its
  `Supervisor`. Keep starts idempotent per account, retries bounded, and
  cancellation/shutdown joined; join the old instance before starting a
  replacement after credential or settings changes. Track daemon session tasks
  through shutdown.
- Keep all crate versions aligned and internal workspace dependency versions
  exactly pinned. Publish in dependency order: core, protocol, client,
  provider-openai, tools, channels, clawbot, server, tui, cli.
- Treat `SCV_HOME` or `--scv-home` as the instance ownership boundary. Separate
  profiles must not share sockets, service units, credentials, adapter state,
  or provider/model configuration. SCV-created native-agent subprocesses must
  receive instance-private `HOME`/XDG state and Codex state, and must not
  silently reuse the user's normal Codex configuration.
- Design cross-cutting changes to the agent loop, protocol, trust boundaries,
  or crate architecture in `docs/` as needed, and review the design while
  implementing it. A reasonable redesign, refactor, or cleanup is encouraged
  when it keeps the project smaller, clearer, safer, or more efficient and is
  necessary for the requested feature. Reserve extra review gates for changes
  with material compatibility, security, or operational risk.
- Record only the accepted final state in `docs/`; do not keep planning diaries
  or stale alternatives in the design documents.
- Commit the workspace manifest and lockfile together when dependencies change,
  and document exact setup, run, and verification commands in `README.md` and
  this file.
- Do not add dependencies or tooling solely to manufacture a default workflow.
- Keep changes focused on the requested outcome, but include the cleanup and
  refactoring needed to leave that outcome complete and maintainable. Prefer
  one end-to-end implementation that includes its API, behavior, tests,
  documentation, and integration work over a sequence of incomplete vertical
  slices. Split work only when a boundary is independently useful or required
  by a real dependency, release, or safety constraint.

## Verification

- Run `git diff --check` for every documentation or code change.
- Setup/build: `cargo build --workspace --locked`.
- Local daemon: set `OPENAI_API_KEY`, then run
  `cargo run --bin scv -- run --workspace /absolute/path/to/workspace`.
- Local TUI: run `cargo run --bin scv` in another terminal.
- Tests: `cargo test --workspace --locked`.
- Format: `cargo fmt --check`.
- Lint: `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- Dependencies: `cargo deny check advisories bans licenses sources` when
  `cargo-deny` is installed (CI always runs it).
- Release build: `cargo build --release --locked`.
- Benchmarks: `cargo bench --bench runtime` for performance-sensitive changes.
- Before handoff, confirm documentation references resolve and report any
  verification that could not be run.
