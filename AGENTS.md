# Agent Instructions

## Compatibility

- `CLAUDE.md` is the Claude Code bridge to this file.

## Project Context

- This repository contains Peon, a Rust workspace for a small extensible agent
  runtime with a separate stdio server and Ratatui client.
- Read `README.md` for the human development flow.
- Treat `docs/architecture.md` as the dependency-boundary source of truth and
  the other files in `docs/` as the final-state contracts for their subjects.
- Keep documentation and code aligned when introducing or changing behavior,
  APIs, data shapes, dependencies, or development commands.

## Git Workflow

- Follow semantic versioning for releases. Increment the patch component by
  `0.0.1` for features and bug fixes; use minor or major bumps only for the
  corresponding semver-compatible or breaking changes.

- For every feature, first write a plan/spec and update the final-state design
  documents in `docs/`; obtain explicit approval before implementation.
- Implement approved work in a dedicated sibling worktree. At delivery, run the
  required checks, commit, push, merge to `main`, publish affected crates, and
  remove the completed worktree and local task branch.

- The default branch is `main` and the canonical remote is `origin`.
- Treat existing uncommitted changes as user-owned unless told otherwise.
- Prefer git worktrees for parallel or unrelated agent work so multiple agents
  can develop concurrently without colliding. Put worktrees in a sibling
  directory such as `../peon-<task>`; this repository does not define an ignored
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
- Keep protocol types and framing in `scv-protocol`, loop and extension traits
  in `scv-core`, provider transport in provider crates, tool implementations in
  `scv-tools`, policy and session authority in `scv-server`, and terminal
  presentation in `scv-tui`.
- Design cross-cutting changes to the agent loop, protocol, trust boundaries,
  or crate architecture in `docs/` first. Obtain an independent design PASS
  before implementation and an independent completeness/safety review before
  delivery.
- Record only the accepted final state in `docs/`; do not keep planning diaries
  or stale alternatives in the design documents.
- Commit the workspace manifest and lockfile together when dependencies change,
  and document exact setup, run, and verification commands in `README.md` and
  this file.
- Do not add dependencies or tooling solely to manufacture a default workflow.
- Keep changes focused and avoid unrelated cleanup.

## Verification

- Run `git diff --check` for every documentation or code change.
- Setup/build: `cargo build --workspace --locked`.
- Local TUI: set `OPENAI_API_KEY`, then run `cargo run --bin scv`.
- Tests: `cargo test --workspace --locked`.
- Format: `cargo fmt --check`.
- Lint: `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- Dependencies: `cargo deny check advisories bans licenses sources` when
  `cargo-deny` is installed (CI always runs it).
- Release build: `cargo build --release --locked`.
- Benchmarks: `cargo bench --bench runtime` for performance-sensitive changes.
- Before handoff, confirm documentation references resolve and report any
  verification that could not be run.
