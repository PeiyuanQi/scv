# Release and Compatibility

Status: final design for v0.1

The current workspace release is `0.1.19`. All crates share that version, and
dependencies between workspace packages use exact `=0.1.19` pins.

SCV v0.1 supports the latest patch release of stable Rust 1.88 or newer on:

- macOS 13 or newer on Apple Silicon and x86-64;
- glibc-based Linux on x86-64 and ARM64.

The release workflow builds and tests four target archives:

- `scv-aarch64-apple-darwin.tar.gz`;
- `scv-x86_64-apple-darwin.tar.gz`;
- `scv-aarch64-unknown-linux-gnu.tar.gz`;
- `scv-x86_64-unknown-linux-gnu.tar.gz`.

Each archive contains `scv`, `scv-server`, `README.md`, `LICENSE`, and
`NOTICE`. Checksums are published beside the archives. Release builds use Cargo
locked mode. The project does not ship a curl-to-shell installer in v0.1.

SCV is licensed under the Apache License 2.0. The root `LICENSE` contains the
unmodified Apache 2.0 license text, `NOTICE` identifies SCV and any required
third-party notices, and the workspace and every published Cargo package set
`license = "Apache-2.0"`. Dependency license checks reject packages whose terms
are incompatible with Apache-2.0 distribution.

The root `README.md` is the installation and quick-start contract. It includes
prerequisites, provider configuration, source build, binary usage, safety
limits, extension entry points, development checks, architecture links,
contribution guidance, and license information.

Installed clients can update with `scv update`. The command uses crates.io by
default, accepts a Cargo index override through `[update].index_url`,
`SCV_CARGO_INDEX_URL`, or `--index-url`, and installs the published `scv-cli`
binary through Cargo. It restarts an active systemd user daemon after
installation. A foreground `scv run` daemon requires an explicit restart.
Existing TUI clients reconnect and start fresh sessions without restoring
server history or automatically replaying submitted or queued work.

Multiple SCV profiles may run concurrently. Select one with `--scv-home` or
`SCV_HOME`; each profile has an independent socket, systemd user unit, provider
configuration, model selection, credentials, ClawBot state, and nested-agent
state. Custom profile selectors are persisted by `scv start`/`restart`, and
`scv update` restarts only the selected daemon.

Before enabling supervised ClawBot accounts, stop any `0.1.9` standalone bridge
processes manually: they do not honor the new account locks. Legacy credentials
and unbound delivery state are loaded conservatively; changing an account's
identity or API origin requires explicit logout before login. See
[ClawBot identity and durable state](clawbot.md#identity-and-durable-state).
Account settings saved with `remote_tools` by `0.1.16` or newer fail closed
under older releases, which reject unknown settings keys; remove the key
before downgrading.

## Publication and checks

The end-to-end landing flow for this repository is the `feature-flow` agent
skill at `.agents/skills/feature-flow/SKILL.md`. Codex reads it from
`.agents/skills`, and Claude Code from the `.claude/skills` symlink to the same
directory. The flow develops in a sibling worktree and passes the checks
below. It rebases onto `origin/main`, through a `gh` pull request when `gh` is
signed in or a fast-forward push otherwise, then publishes, installs the
release, and restarts the local daemon. Its scripts cover the steps that are
easy to get wrong:

- `publish.sh`: a resumable publish in dependency order;
- `deploy.sh`: install, restart, and verify;
- `host.sh`: run landing commands from agents that SCV started with a private
  home.

Publish from a clean verified checkout after authenticating with `cargo login`.
Wait for each dependency version to become available before publishing its
dependents:

```bash
cargo publish --locked -p scv-core
cargo publish --locked -p scv-protocol
cargo publish --locked -p scv-client
cargo publish --locked -p scv-provider-openai
cargo publish --locked -p scv-tools
cargo publish --locked -p scv-clawbot
cargo publish --locked -p scv-server
cargo publish --locked -p scv-tui
cargo publish --locked -p scv-cli
```

Required checks remain:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources
cargo build --release --locked
git diff --check
```

`cargo-deny` is required in CI and runs locally when installed. These commands
are release requirements, not a record of a successful run.

## Compatibility policy

- The JSONL protocol used by both Unix sockets and stdio is versioned
  independently from the crate version; version 2 includes daemon management.
- Additive object fields do not change the protocol version.
- Removing a field, changing its meaning, or changing message ordering requires
  a protocol version increase.
- Configuration rejects unknown keys in v0.x so misspellings do not silently
  weaken behavior.
- Rust traits are extension seams but do not promise a stable third-party ABI
  before 1.0.
- Linux and macOS are release-gated; other platforms are best effort until they
  join the CI matrix.
