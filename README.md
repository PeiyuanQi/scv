# SCV

SCV is a small, fast, extensible agent runtime for the terminal. It keeps the
agent loop simple, puts model and tool authority in a separate server process,
and provides a responsive Rust TUI for coding work.

> **Project status:** SCV is an early implementation. Its protocol and
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
- Daemon-supervised chat channels (WeChat, Feishu/Lark) with persistent enablement and live health
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

All packages use version `0.2.1`, with exact `=0.2.1` pins for dependencies
between workspace packages.

```bash
cargo publish --locked -p scv-core
cargo publish --locked -p scv-protocol
cargo publish --locked -p scv-client
cargo publish --locked -p scv-provider-openai
cargo publish --locked -p scv-tools
cargo publish --locked -p scv-channels
cargo publish --locked -p scv-clawbot
cargo publish --locked -p scv-feishu
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

## Daemon and channels (WeChat, Feishu)

SCV has one long-running daemon. Run it attached to the terminal with
`scv run --workspace /path/to/workspace`, or let the user service supervise the
same process:

```bash
scv start --workspace /path/to/workspace
scv status
scv reload
scv restart --workspace /path/to/workspace
scv restart --when-idle   # into a newly installed release, once owner work is done
scv stop
```

`scv restart --when-idle` lets a chat-driven update finish its report before
the daemon restarts, checks the new release, and rolls back to the previous
binary if it does not come up; the new daemon announces the outcome in chat
(see [configuration](docs/configuration.md#daemon-and-component-settings)).

Each SCV instance owns an explicit profile root. Use `--scv-home PATH` (or
`SCV_HOME`) to run independent daemons with separate provider/model settings,
sockets, credentials, skills, channel state, and systemd units:

```bash
scv --scv-home ~/.scv-work --model gpt-4.1-mini start --workspace /path/to/workspace
scv --scv-home ~/.scv-review --model o4-mini start --workspace /path/to/workspace
scv --scv-home ~/.scv-work status
```

`--config PATH` (or `SCV_CONFIG`) selects an additional explicit configuration
file for that instance. The selected home and config are persisted into the
instance's service unit, so a restart does not fall back to the default
`~/.scv` configuration. `scv update` restarts only the selected instance.

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

Chat channels connect chat accounts to the daemon, all through
`scv channels <command> <channel>`. Authenticate the WeChat ClawBot bridge once
with `scv channels login wechat`. The QR login stores the bearer token at
`$SCV_HOME/credentials/wechat/<account>.json` (normally under `~/.scv`)
with mode `0600`; the token is never printed. Saved accounts are enabled by
default and start under the daemon automatically. Login remains explicit and
honors an account's saved opt-out.

Delivery state is bound to the account identity and API origin. Token rotation
for the same known identity preserves state; changing identity or origin
requires explicit logout first, including replacement of legacy credentials
without known identity. Stop any old `0.1.9` standalone ClawBot process before
enabling the supervised account; those processes do not honor the new locks.

`scv channels run wechat --account NAME --workspace /path/to/workspace`
persistently enables the account in the running daemon and returns. WeChat
sessions are tool-free by default; add `--remote-tools owner` to give the bot's
own WeChat account every SCV tool, including delegated Claude Code and Codex,
with approvals granted automatically. That equals shell access from that WeChat
account; `--remote-tools none` revokes it. `scv channels stop wechat
--account NAME` persistently disables it while retaining credentials.
`scv channels status` queries live daemon health of every channel account,
including identity and last successful contact; saved credentials alone do not
mean connected. `scv channels logout wechat --account NAME` requires a live
daemon and joins the component before deleting credentials, delivery state, and
the account's settings table.

Feishu (and Lark, its international edition) works the same way:
`scv channels login feishu` shows a QR code; scanning it with the Feishu app
creates a bot app in your own account, with no developer console or public URL,
and records you as its owner. `scv channels login feishu --app-id cli_...
--owner-open-id ou_...` instead adds an app you already have, reading its
secret from a hidden prompt or stdin. The bot connects over Feishu's event long
connection and, after any disconnection, catches up on messages sent in the
meantime from chat history. In groups it answers only messages that mention
it. `scv channels run feishu --workspace PATH --remote-tools owner` then
grants the owner's direct chats every SCV tool, exactly as for WeChat.

Both channels take pictures, voice messages, videos, files, quoted messages,
and (on Feishu) forwarded bundles. SCV downloads them privately under
`$SCV_HOME/state/media`, shows images to the model when it accepts image input,
and hands the owner's agent the file paths; other senders' files are limited
to pictures. In the owner's chat the agent can send files and pictures back
with its `chat_attach` tool. See [channel media](docs/channels.md#media).


Account settings are `[channels.<channel>.<account>]` tables in
`config.toml`, with
`enabled` defaulting to `true`, `remote_tools` defaulting to `"none"`, and an
optional workspace defaulting to the daemon workspace. `scv channels run` and
`stop` edit only that table and keep your comments. The daemon reconciles accounts and settings every two seconds
or immediately on `scv reload`, so a hand edit takes effect without a restart.
To opt out while offline, set `enabled = false` before daemon startup. See the
[channels contract](docs/channels.md) for permissions and recovery behavior.

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

Everything an SCV instance keeps is under `~/.scv` (or `$SCV_HOME`), and the
one file you edit is `config.toml`:

```text
~/.scv/config.toml     settings: provider and key, limits, agents, chat accounts
~/.scv/credentials/    channel sign-ins SCV writes
~/.scv/agents/<name>/  private homes of delegated agents, with their sign-ins
~/.scv/skills/         your skills
~/.scv/state/          runtime state: socket, delegated runs, delivery state
```

`scv config show` lists every path, the settings in effect with where each
came from, and whether each credential is in place, without showing any
secret; `scv config path` prints the file's path. Copy
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
command_timeout_seconds = 600

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
`~/.scv/skills/<name>/SKILL.md` for the user. Set `SCV_HOME` to relocate the
whole instance. Only skill metadata enters
the initial prompt; the model loads full instructions through the contained
`read_skill` tool when needed. Tool-enabled sessions also list the Codex and
Claude Code skills (`.agents/skills`, `.claude/skills`) of the workspace and its
child projects as `<project>:<name>`, so SCV knows to delegate that work to an
agent running in the project.

`web_fetch` reads public web pages and APIs as text, without approval for
HTTPS documentation hosts in `web.auto_approve_domains` and with approval for
anything else, and never reaches loopback or private addresses by default.
Web search comes from the provider's hosted Responses tool (`web.search =
"provider"`) or a SearXNG or Brave Search backend; see
[configuration](docs/configuration.md#web-tools).

The built-in `agent_claude`, `agent_codex`, `agent_grok`, `agent_dsh`, and
`agent_pi` tools launch Claude Code, Codex, Grok Build, DeepSeek Harness, and pi
directly, without shell interpolation; a session offers only those installed. They are optional,
approval-gated, cancellable subprocess adapters and share the same output and
timeout limits as other process tools. A call may set `cwd` to a project
directory inside the workspace, where the agent loads that project's
`AGENTS.md`/`CLAUDE.md` and skills, and may raise `timeout_seconds` up to
`tools.max_timeout_seconds` (default 14400) for long work such as landing a
change. Each adapter receives an instance-private
`HOME`, `SCV_HOME`, XDG directories, and the agent's own state directory
(`CODEX_HOME`, `GROK_HOME`, `DSH_HOME`, `PI_CODING_AGENT_DIR`) under
`$SCV_HOME/agents/<name>`, and inherits no API-key variables. SCV never
reuses or modifies the user's normal `~/.claude`, `~/.codex`, `~/.grok`,
`~/.dsh`, or `~/.pi` configuration. Sign the agents in for SCV once with
`scv agents login <name>`, copy a custom-provider Codex setup with
`scv agents import codex`, point pi at SCV's own provider with
`scv agents import pi --from-scv-provider` (or any OpenAI-compatible endpoint
with `scv agents login pi --openai-compatible`), and check with
`scv agents status`. `agent_scv` delegates to a nested SCV in its own private
home, kept running for the conversation and driven over the SCV protocol; its
tool approvals come back to the calling session. Give it SCV's own provider
with `scv agents import scv`. A delegated run returns only its final reply, usage, and
status, not the agent's event log. SCV records each run while it lasts: list
them with `scv agents ps`, stop one with `scv agents kill <handle>`, and the
daemon stops runs left behind by a killed SCV process within a minute.
Claude Code, Codex, and pi keep multi-turn conversations: a result's `session`
handle continues the same conversation, and `scv agents gc` clears old
transcripts. Any agent call may set `background: true` to return a job handle
at once while the agent keeps working; `agent_wait` and `agent_status` observe
jobs, `agent_cancel` stops one, and when one finishes SCV reports it in a turn
of its own (over WeChat or Feishu, as an unprompted message to the owner). The
main agent is told to work this way by default: it answers quick questions
itself and hands real work to background agents, so it stays available to
chat. A session runs at most `agent.max_background` (default 4) jobs, and
closing it cancels them. `agent.prefer` and `[agents.<name>] use_for` steer
which agent it picks. Claude Code, Codex, Grok Build, and DeepSeek Harness run over the
Agent Client Protocol when its server is installed (`claude-agent-acp` and
`codex-acp` from npm `@agentclientprotocol/*`, or the built-in `grok agent
stdio` and `dsh --profile acp`): one server per conversation whose permission
requests come back to the calling session for approval and whose progress
streams as it works. `[agents.<name>] transport = "resume"` keeps one process
per turn.

## Architecture

SCV is a Cargo workspace with deliberately narrow packages:

- `scv-core`: loop and extension traits;
- `scv-protocol`: versioned wire types with no runtime policy;
- `scv-client`: shared default socket path and daemon control helper;
- `scv-provider-openai`: streaming provider transport;
- `scv-tools`: filesystem, process, skill, and nested-agent tools;
- `scv-channels`: the chat-channel bridge every channel shares: durable
  claims, delivery state, and remote sessions;
- `scv-clawbot`: the WeChat channel's iLink login, polling, and sending;
- `scv-feishu`: the Feishu/Lark channel's QR app registration, event long
  connection with catch-up, and sending;
- `scv-server`: configuration, sessions, permissions, component supervision,
  and protocol dispatch;
- `scv-tui`: terminal client and headless protocol client.

The TUI connects to the local Unix-socket daemon; `scv-server --stdio` exposes
the same server library to one-shot local clients. Dependencies flow from
server to the channel crates (WeChat's `scv-clawbot`, Feishu's `scv-feishu`)
to the channel core (`scv-channels`) to client to protocol; the TUI depends on client, not server.
All long-running components must be supervised by the server. Start with the
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
