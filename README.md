# Peon

Peon is a small, fast, extensible agent runtime for the terminal. It keeps the
agent loop simple, puts model and tool authority in a separate server process,
and provides a responsive Rust TUI for coding work.

> **Project status:** Peon is an early v0.1 implementation. Its protocol and
> configuration may change before 1.0. Run it in version-controlled workspaces
> and review every approval.

## What works

- Streaming OpenAI-compatible model calls and function tools
- Built-in workspace-scoped `read`, atomic `write`, and `bash` tools
- Native tool adapters for installed `claude`, `codex`, and `pi` agents
- Progressive-disclosure Markdown skills from user and project directories
- Configurable, bounded context selection and deterministic compaction
- Server-enforced approvals, timeouts, output caps, and cancellation
- Separate stdio server and Ratatui client processes
- Interactive TUI plus a headless `peon exec` mode
- Linux and macOS support on ARM64 and x86-64

## Install

Peon requires Rust 1.88 or newer and `/bin/bash`.

```bash
cargo install --locked --git https://github.com/PeiyuanQi/peon
```

Until release archives are published, build from source:

```bash
git clone https://github.com/PeiyuanQi/peon.git
cd peon
cargo build --release --locked
```

The package installs two binaries: `peon` and the standalone protocol entry
point `peon-server`.

## Quick start

Peon's first provider speaks the OpenAI-compatible Chat Completions API.

```bash
export OPENAI_API_KEY="your-key"
cd /path/to/your/project
peon
```

Use another compatible model or endpoint:

```bash
peon --model gpt-4.1-mini --base-url https://api.openai.com/v1
```

Run one non-interactive prompt. Risky tools are denied unless `--yes` is
present:

```bash
peon exec "Explain this repository"
peon exec --yes "Run the tests and fix the failure"
```

In the TUI, `Enter` sends, `Ctrl+J` inserts a newline, `Esc` cancels, `Ctrl+O`
toggles the latest tool result, and `/help` lists the compact command set.

## Configuration

User configuration lives at `~/.config/peon/config.toml`. A workspace may add
`.peon/config.toml`, but project configuration cannot redirect provider
credentials or replace native-agent executables.

```toml
[provider]
model = "gpt-4.1-mini"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

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

Precedence is CLI, environment, explicit `PEON_CONFIG`, project configuration,
user configuration, then defaults. Unknown keys fail startup. See
[`docs/configuration.md`](docs/configuration.md) for the complete schema and
trust rules.

## Extending Peon

The core exposes small Rust traits for providers, tools, context policies,
approval gates, and event sinks. Registering a new `Tool` does not require a
change to the agent loop or TUI.

Skills use `.peon/skills/<name>/SKILL.md` in a project or
`~/.config/peon/skills/<name>/SKILL.md` for the user. Only skill metadata enters
the initial prompt; the model loads full instructions through the contained
`read_skill` tool when needed.

The built-in `agent_claude`, `agent_codex`, and `agent_pi` tools launch those
installed CLIs directly, without shell interpolation. They are optional,
approval-gated, cancellable subprocess adapters and share the same output and
timeout limits as other process tools.

## Architecture

Peon is a Cargo workspace with deliberately narrow packages:

- `peon-core`: loop and extension traits;
- `peon-protocol`: versioned wire types with no runtime policy;
- `peon-provider-openai`: streaming provider transport;
- `peon-tools`: filesystem, process, skill, and nested-agent tools;
- `peon-server`: configuration, sessions, permissions, and protocol dispatch;
- `peon-tui`: terminal client and headless protocol client.

The TUI spawns `peon server --stdio`; `peon-server --stdio` exposes the same
server library to other local clients. Start with the final v0.1
[`architecture`](docs/architecture.md), then see the
[`protocol`](docs/protocol.md), [`context`](docs/context-management.md),
[`tools`](docs/tools.md), [`TUI`](docs/tui.md), and
[`security model`](docs/security.md).

## Security

Peon is **not an OS sandbox**. `bash` and nested agents run with your user
permissions and inherited environment after approval. File tools reject
absolute paths, parent traversal, and symlink escapes, but an approved process
can access anything your account can access. Use a container or operating-system
sandbox for untrusted repositories. See [`SECURITY.md`](SECURITY.md) and the
full [`security model`](docs/security.md).

## Development

Read [`AGENTS.md`](AGENTS.md) before using a coding agent in this repository.
Use a sibling git worktree for parallel or unrelated work. For cross-cutting
agent-loop, protocol, security, or architecture changes, update the final-state
design in `docs/` and pass an independent design review before implementation.
Keep one coherent requested outcome in one commit by default, and commit or
push only at an explicit delivery boundary.

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
