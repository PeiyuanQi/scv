# SCV

SCV is a small, fast, extensible agent runtime for the terminal. It keeps the
agent loop simple, puts model and tool authority in a separate server process,
and provides a responsive Rust TUI for coding work.

> **Project status:** SCV is an early v0.1 implementation. Its protocol and
> configuration may change before 1.0. Run it in version-controlled workspaces
> and review every approval.

## What works

- Streaming OpenAI-compatible model calls and function tools
- Built-in workspace-scoped `read`, atomic `write`, and `bash` tools
- Native tool adapters for installed `claude`, `codex`, and `pi` agents
- Progressive-disclosure Markdown skills from user and project directories
- Configurable, bounded context selection and deterministic compaction
- Server-enforced approvals, timeouts, output caps, and cancellation
- A Unix-socket daemon with a Ratatui client, plus a stdio server for one-shot clients
- Daemon-supervised ClawBot accounts with persistent enablement and live health
- Interactive TUI plus a headless `scv exec` mode
- Linux and macOS support on ARM64 and x86-64

## Install

SCV requires Rust 1.88 or newer and `/bin/bash`.

```bash
cargo install --locked --git https://github.com/PeiyuanQi/scv
```

Until release archives are published, build from source:

```bash
git clone https://github.com/PeiyuanQi/scv.git
cd scv
cargo build --release --locked
```

The package installs two binaries: `scv` and the standalone protocol entry
point `scv-server`.

Update an installed SCV binary from crates.io with:

```bash
scv update
```

The registry index can be selected in `~/.scv/config.toml`:

```toml
[update]
index_url = "https://mirrors.ustc.edu.cn/crates.io-index"
```

`SCV_CARGO_INDEX_URL` or `scv update --index-url URL` can override that value.
SCV passes the URL to Cargo and does not handle registry credentials itself.
When the systemd user daemon is active, the update command restarts it after the
published binary is installed. A foreground `scv run` daemon requires an
explicit restart. Connected TUI clients automatically reconnect and create a
fresh session, without restoring server history or replaying submitted work.

To publish from a clean checkout, authenticate with `cargo login` and publish
the workspace in dependency order (Cargo will refuse a package whose local
dependencies are not already on crates.io):

All packages use version `0.1.10`, with exact `=0.1.10` pins for dependencies
between workspace packages.

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

The Unix-socket daemon is the normal JSONL backend for the TUI. Embedding
clients and headless `scv exec` can use the separate stdio entry point:

```bash
scv server --stdio
```

The stdio protocol is intentionally local and one-session-per-connection.

## Daemon and ClawBot / WeChat iLink

SCV has one long-running daemon. Run it attached to the terminal with
`scv run --workspace /path/to/workspace`, or let the user service supervise the
same process:

```bash
scv start --workspace /path/to/workspace
scv status
scv reload
scv restart --workspace /path/to/workspace
scv stop
```

Daemon starts use the server's `on-risk` approval policy by default. Pass
`--approval-policy always` or `--approval-policy never` before `start` or
`restart` when that invocation needs a different explicit policy. SCV runs the
daemon as the current user. If the user has no currently valid sudo
authorization, an interactive start asks whether to continue with reduced
capability; `--allow-sudo` asks sudo to authenticate the current user's
existing policy first. This flag cannot grant sudoers membership or turn the
daemon into a root service, and non-interactive starts without verified sudo
authorization fail with an actionable error.

With no subcommand, `scv` starts the TUI and connects to the local server
socket. It does not start a private server child. If the daemon is unavailable,
SCV reports the socket path and suggests `scv start` or `scv run`. Model and
provider overrides are sent when the TUI creates its session, so a running
daemon can serve sessions using different models or providers without a daemon
restart:

```bash
scv --model gpt-4.1-mini
scv --provider local --model llama3.1 --base-url http://localhost:11434/v1
```

Authenticate the WeChat ClawBot bridge once with `scv clawbot login`. The QR
login stores the bearer token at `$SCV_HOME/clawbot/accounts/<account>.json`
(normally under `~/.scv`) with mode `0600`; the token is never printed. Saved
accounts are enabled by default and start under the daemon automatically.
Login remains explicit and honors an account's saved opt-out.

Delivery state is bound to the account identity and API origin. Token rotation
for the same known identity preserves state; changing identity or origin
requires explicit logout first, including replacement of legacy credentials
without known identity. Stop any old `0.1.9` standalone ClawBot process before
enabling the supervised account; those processes do not honor the new locks.

`scv clawbot run --account NAME --workspace /path/to/workspace` persistently
enables the account in the running daemon and returns. `scv clawbot stop
--account NAME` persistently disables it while retaining credentials.
`scv clawbot status --account NAME` queries live daemon health, including
identity and last successful contact; saved credentials alone do not mean
connected. `scv clawbot logout --account NAME` requires a live daemon and joins
the component before deleting credentials, delivery state, and settings.

Account settings live at `$SCV_HOME/clawbot/settings/<account>.json`, with
`enabled` defaulting to `true` and an optional workspace defaulting to the
daemon workspace. The daemon reconciles accounts and settings every two seconds
or immediately on `scv reload`. To opt out while offline, set `enabled` to
`false` in the private settings file before daemon startup. See the
[ClawBot contract](docs/clawbot.md) for permissions and recovery behavior.

## Quick start

SCV's first provider speaks the OpenAI-compatible Responses API.

```bash
export OPENAI_API_KEY="your-key"
cd /path/to/your/project
# In a separate terminal, start `scv run --workspace /path/to/your/project`.
scv
```

Use another compatible model or endpoint:

```bash
scv --model gpt-4.1-mini --base-url https://api.openai.com/v1
```

Run one non-interactive prompt. Risky tools are denied unless `--yes` is
present:

```bash
scv exec "Explain this repository"
scv exec --yes "Run the tests and fix the failure"
```

In the TUI, `Enter` sends, `Ctrl+J` inserts a newline, `Esc` cancels, `Ctrl+O`
toggles the latest tool result, and `/help` lists the compact command set.

## Configuration

User configuration lives at `~/.scv/config.toml` (or `$SCV_HOME/config.toml`); copy
[`config.example.toml`](config.example.toml) there to get started. A workspace may add
`.scv/config.toml`, but project configuration cannot redirect provider
credentials or replace native-agent executables.

```toml
[provider]
model = "gpt-4.1-mini"
base_url = "https://api.openai.com/v1"
api_key = "sk-your-key"
api_key_env = "OPENAI_API_KEY" # optional fallback

[context]
max_tokens = 128000
reserve_output_tokens = 8192

[tools]
approval_policy = "on-risk"
command_timeout_seconds = 120

[agents.codex]
command = "codex"
args = ["exec"]
```

Precedence is CLI, environment, explicit `SCV_CONFIG`, project configuration,
user configuration, then defaults. Unknown keys fail startup. See
[`docs/configuration.md`](docs/configuration.md) for the complete schema and
trust rules.

## Extending SCV

The built-in provider uses the OpenAI Responses API at `/responses`, with streaming text and function-call events. The core exposes small Rust traits for providers, tools, context policies,
approval gates, and event sinks. Registering a new `Tool` does not require a
change to the agent loop or TUI.

Skills use `.scv/skills/<name>/SKILL.md` in a project or
`~/.scv/skills/<name>/SKILL.md` for the user. Set `SCV_HOME` to relocate all user
configuration and skills. Only skill metadata enters
the initial prompt; the model loads full instructions through the contained
`read_skill` tool when needed.

The built-in `agent_claude`, `agent_codex`, and `agent_pi` tools launch those
installed CLIs directly, without shell interpolation. They are optional,
approval-gated, cancellable subprocess adapters and share the same output and
timeout limits as other process tools.

## Architecture

SCV is a Cargo workspace with deliberately narrow packages:

- `scv-core`: loop and extension traits;
- `scv-protocol`: versioned wire types with no runtime policy;
- `scv-client`: shared default socket path and daemon control helper;
- `scv-provider-openai`: streaming provider transport;
- `scv-tools`: filesystem, process, skill, and nested-agent tools;
- `scv-clawbot`: iLink login, polling, delivery state, and remote sessions;
- `scv-server`: configuration, sessions, permissions, component supervision,
  and protocol dispatch;
- `scv-tui`: terminal client and headless protocol client.

The TUI connects to the local Unix-socket daemon; `scv-server --stdio` exposes
the same server library to one-shot local clients. Dependencies flow from
server to ClawBot to client to protocol; the TUI depends on client, not server.
All long-running components must be supervised by the server. Start with the
final v0.1
[`architecture`](docs/architecture.md), then see the
[`protocol`](docs/protocol.md), [`context`](docs/context-management.md),
[`tools`](docs/tools.md), [`TUI`](docs/tui.md), and
[`security model`](docs/security.md).

## Security

SCV is **not an OS sandbox**. `bash` and nested agents run with your user
permissions and inherited environment after approval. File tools reject
absolute paths, parent traversal, and symlink escapes, but an approved process
can access anything your account can access. Use a container or operating-system
sandbox for untrusted repositories. See [`SECURITY.md`](SECURITY.md) and the
full [`security model`](docs/security.md).

## Development

Read [`AGENTS.md`](AGENTS.md) before using a coding agent in this repository.
Use a sibling git worktree for parallel or unrelated work. Keep final-state
design documents in `docs/` aligned with behavior changes, including
cross-cutting agent-loop, protocol, security, or architecture work. Reasonable
redesign and cleanup are part of feature work when they leave the project
clearer, smaller, safer, or more efficient. Prefer one complete end-to-end
implementation of the requested outcome, including tests and documentation,
over artificial vertical slices. Commit or push only at an explicit delivery
boundary.

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources
cargo build --release --locked
git diff --check
```

Tests use scripted providers and fake executables; they do not require a live
API key. The test and performance contract is in
[`docs/quality.md`](docs/quality.md), with measured results and an honest
feature comparison in the [`v0.1 evaluation`](docs/evaluation.md).
Contributions are welcome—read
[`CONTRIBUTING.md`](CONTRIBUTING.md) first.

## License

Licensed under the [Apache License 2.0](LICENSE).
