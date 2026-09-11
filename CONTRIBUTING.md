# Contributing to SCV

Thank you for helping make SCV smaller, safer, and easier to use.

## Development setup

Install stable Rust 1.88 or newer, clone the repository, and run:

```bash
cargo build --workspace --locked
cargo test --workspace --locked
```

Before opening a change, run:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
git diff --check
```

Read `AGENTS.md` before using a coding agent. Prefer a sibling git worktree for
parallel or unrelated work so concurrent changes do not collide. Treat commits
as delivery boundaries: keep one coherent requested outcome in one commit by
default and split only independently reviewable, revertible changes.

Keep changes focused. Update the final-state documents in `docs/` whenever a
change affects behavior, protocol messages, configuration, security boundaries,
dependencies, installation, or development commands. Design cross-cutting
changes to the agent loop, protocol, trust boundaries, or crate architecture in
those documents first and obtain an independent design PASS before
implementation. Record only the accepted design state, not planning history or
discarded alternatives. Complete an independent safety and feature-completeness
review before delivery.

Tests must not require a live provider credential or installed nested-agent
CLI. Run `cargo bench --bench runtime` for performance-sensitive changes and
compare the result with `docs/evaluation.md` on the same machine where possible.

## Design and compatibility

Core changes must preserve the dependency direction in
`docs/architecture.md`. Protocol breaking changes require a protocol version
increase. Security-sensitive tool changes need traversal, symlink, size,
timeout, cancellation, denial, and process-descendant coverage as applicable.

By contributing, you agree that your contribution is licensed under the
Apache License 2.0.
