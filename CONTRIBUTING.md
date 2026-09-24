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

## Local development

Run a development daemon in an instance of its own, so it never touches your
real `~/.scv` (its config, credentials, socket, and running daemon):

```bash
home="$(mktemp -d)"
cargo run --bin scv -- --scv-home "$home" config init    # starter config.toml
OPENAI_API_KEY=... RUST_LOG=scv_server=debug,scv_channels=debug \
  cargo run --bin scv -- --scv-home "$home" run --workspace "$PWD"
```

In another terminal, pass the same `--scv-home` to reach that daemon:
`cargo run --bin scv -- --scv-home "$home"` opens the TUI, `... status`
shows its components, and `... config show` lists every path and setting it
uses, with secrets hidden.

Logs go to the process's stderr. Without `RUST_LOG` the daemon logs warnings
and errors; the systemd unit that `scv start` writes sets `RUST_LOG=info` and
sends stderr to the journal: `journalctl --user -u scv.service`, or
`scv-<hash>.service` for an instance started with `--scv-home` (`systemctl
--user list-units 'scv*'` lists them).

Run one crate's tests with `cargo test -p <crate> [<name filter>]`, and the
black-box tests of one area from the root `tests/it/` with
`cargo test -p scv-cli --test it <module>::` (such as `daemon::`). [`docs/quality.md`](docs/quality.md)
says where new tests go, and [`docs/architecture.md`](docs/architecture.md)
says where to start reading the code.

## How changes are made

Read `AGENTS.md` before using a coding agent. Prefer a sibling git worktree for
parallel or unrelated work so concurrent changes do not collide. Treat commits
as delivery boundaries and prefer one complete end-to-end implementation of a
requested outcome, rather than artificial vertical slices. Split only when a
boundary is independently useful or required by a real dependency, release,
or safety constraint.

Keep changes focused on the requested outcome, while including reasonable
redesign, cleanup, and refactoring needed to keep the project minimal, clear,
safe, and efficient. Update the final-state documents in `docs/` whenever a
change affects behavior, protocol messages, configuration, security boundaries,
dependencies, installation, or development commands. Review cross-cutting
design and completeness during implementation; reserve extra review gates for
material compatibility, security, or operational risk. Record only the accepted
final state, not planning history or discarded alternatives.

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
