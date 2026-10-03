<div align="center">

<img src="https://raw.githubusercontent.com/PeiyuanQi/scv/main/assets/scv.png" alt="SCV: a blue construction mech at work in a quarry" width="440">

# SCV

**One agent to find them all, one agent to guide them.**

A fast, native agent runtime that lives on your machine,<br>
leads your coding agents, and answers from your terminal, Feishu, Slack, and WeChat.

English · [简体中文](https://github.com/PeiyuanQi/scv/blob/main/README.zh-CN.md)

[![crates.io](https://img.shields.io/crates/v/scv-cli?style=flat-square&logo=rust&color=2f6fd6)](https://crates.io/crates/scv-cli)
[![CI](https://img.shields.io/github/actions/workflow/status/PeiyuanQi/scv/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/PeiyuanQi/scv/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square)](https://github.com/PeiyuanQi/scv/blob/main/LICENSE)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-orange?style=flat-square)](https://github.com/PeiyuanQi/scv/blob/main/docs/release.md)

[**Install**](#install) · [**Quickstart**](#quickstart) · [**Chat channels**](https://github.com/PeiyuanQi/scv/blob/main/docs/channels.md) · [**Agents**](https://github.com/PeiyuanQi/scv/blob/main/docs/tools.md#delegated-agents-agent) · [**Docs**](#documentation)

</div>

---

SCV is a small agent runtime written in Rust. One long-running daemon holds
your sessions, tools, and approvals, and you talk to it from a terminal UI or
from Feishu/Lark, WeChat, or Slack on your phone. It handles quick questions itself,
finds the right coding agent for real work (Claude Code, Codex, and others),
briefs it, runs it as a background job, and messages you when it is done.

## Highlights

### Chat from your phone

Scan a QR code to pair Feishu/Lark or WeChat, or connect a Slack app you set up
by hand. SCV answers only you by default,
reads the photos, files, and videos you send, and sends files back. Long work
runs in the background while you keep chatting; results and yes/no questions
arrive as messages. SCV keeps a log of your chat, so a conversation carries on
across restarts and it can look back when you mention something older; `/new`
starts a fresh one. On Feishu and Slack, a thread is a conversation of its
own: SCV answers inside the thread, on a session of its own.

### One agent, many agents

One `agent` tool delegates to Claude Code, Codex, Grok Build, DeepSeek Harness,
pi, or a nested SCV, over the [Agent Client Protocol](https://agentclientprotocol.com)
where available. Your `prefer` list and `use_for` notes pick who does what,
each agent's default model and effort (and a separate effort for hard tasks)
set how, and `agent_wait`, `agent_status`, and `agent_cancel` manage
background jobs. SCV learns each agent's models and effort levels from the
agent itself and passes them exactly, instead of guessing names that change
with every release. A built-in `delegating` skill teaches it how to brief an
agent and what to do when a call fails, and `scv agents check` shows every
agent working.

### Private homes, private keys

Each agent signs in once with `scv agents login` and runs in its own home under
`~/.scv/agents/<name>`. It never reads your personal `~/.claude` or
`~/.codex`, and never inherits your `*_API_KEY` variables.

### Updates without dropping work

`scv restart --when-idle` waits until the work that asked for it has reported
back, then restarts into the new release. A watchdog checks that the new
version comes up with its chat accounts connected and, where the config layout
allows, rolls back if it does not. That is how SCV ships its own releases from
chat, asking you before it publishes.

### You stay in charge

The server, not the client, enforces approvals: by default reads run, while
writes, shell commands, and agents ask first. File tools stay in the workspace,
every command has a timeout and an output cap, and chat stays tool-free until
you grant tools to your own account.

### Native, small, extensible

A Rust daemon, a terminal UI, headless `scv exec`, and a versioned JSONL
protocol. Deterministic context budgeting, any OpenAI-compatible Responses
endpoint, Markdown skills, and Rust traits for providers, tools, and policies.
Linux and macOS.

## Install

```bash
cargo install scv-cli --locked
```

You need Rust 1.88 or newer and `/bin/bash`. This installs `scv` and
`scv-server`, a standalone protocol entry point. To track `main` instead, run
`cargo install --locked --git https://github.com/PeiyuanQi/scv`.

## Quickstart

**1. Configure a model.** SCV speaks the OpenAI-compatible Responses API.

```bash
scv config init                  # writes a starter ~/.scv/config.toml
$EDITOR "$(scv config path)"     # put your API key and model in it
```

**2. Start the daemon** on the folder that holds your projects, then talk to
it. `scv start` runs the daemon as a systemd user service; without systemd,
such as on macOS, keep `scv run --workspace ~/code` open in a terminal instead.

```bash
scv start --workspace ~/code
scv                              # the terminal UI, in the current directory
scv exec "Explain this repository"   # or one headless prompt
```

**3. Connect your phone.** We recommend Feishu (or Lark, its international
edition). SCV keeps one WebSocket connection open to it, so your messages
arrive at once. WeChat also works, but SCV has to poll its API for new
messages, and in our use it answers noticeably slower.

```bash
scv channels login feishu        # scan the QR code (or: lark, wechat)
scv channels run feishu --workspace ~/code --remote-tools owner
scv channels status              # Channels: 1 of 1 enabled accounts connected
```

Slack works too, through an app you create once from SCV's manifest with Socket
Mode on ([setup](https://github.com/PeiyuanQi/scv/blob/main/docs/channels.md#slack-contract)).
`scv channels login slack --slack-owner-user-id U0123456789` then reads its bot
and app-level tokens at hidden prompts.

> **`--remote-tools owner` equals shell access from your chat account.** It
> gives your own account every SCV tool, with approvals granted automatically.
> Leave it out and SCV chats with you tool-free.

**4. Sign in the agents it delegates to**, then check that each one works as
SCV runs it.

```bash
scv agents login claude          # once per agent, in SCV's private home
scv agents login codex
scv agents status
scv agents check                 # version, models, and one short call each
scv agents doctor --workspace .   # offline project Rust/toolchain diagnostics
```

Delegated coding agents resolve installed Rust tools for the selected project
before they start, even when the daemon's PATH only finds an older system
Rust. Toolchain files and Cargo's Rust requirements are checked; unavailable
or incompatible tools produce a diagnostic. The agents keep private homes
and Cargo state. See [project Rust environments](docs/tools.md#project-rust-environment)
for selection rules and limits.

### Everyday commands

| Command | What it does |
| --- | --- |
| `scv` | Open the terminal UI on the running daemon |
| `scv exec [--yes] "…"` | Run one prompt headless; `--yes` approves risky tools for that run |
| `scv run`, `start`, `stop`, `status`, `reload` | Run or manage the daemon |
| `scv channels login`, `run`, `stop`, `status`, `logout` | Manage chat accounts (`wechat`, `feishu`, `lark`) and read-only mailboxes (`email`) |
| `scv agents login`, `status`, `check`, `ps`, `kill` | Sign agents in, check them, and list or stop their runs |
| `scv confirm "…"` | Ask the owner yes or no in chat; exits 0 only on yes |
| `scv project create`, `status`, `tasks`, `report` | Opt into a durable project ledger and inspect its evidence |
| `scv project add-task`, `update-task`, `run-start`, `run-progress`, `run-finish`, `heartbeat`, `events` | Drive or inspect project tasks and supervised run heartbeats |
| `scv config show` | Every path and setting in effect, secrets hidden |
| `scv update` | Install the latest release and restart the daemon |

Project orchestration is optional and independent of one-off `agent` calls.
Create a project explicitly with an existing absolute workspace; the daemon
then persists an append-only event log under `$SCV_HOME/state/projects/` and
starts its supervised project orchestrator. Add tasks with dependencies, record
the agent run IDs and heartbeats, and use `scv project report` to inspect only
observed state. The daemon compacts the event tail into a private snapshot as
it grows; `project events` reports when older sequences have been compacted.
Missing heartbeats make a run and its task stale after five minutes; the
orchestrator never infers progress from agent prose or starts an agent without
an explicit run command.

## What it looks like

An illustration of a Feishu chat with SCV:

```text
You  The checkout test in shop/ has failed since this morning.
     Find out why and fix it, please.
SCV  Handed it to Codex in shop/ as job-1. I'll message you when it
     reports back.
You  Meanwhile, how much disk is left on this machine?
SCV  412 GB free of 1 TB on /.

                           · · · 14 minutes later · · ·

SCV  Codex finished job-1. The test assumed the machine's clock was on
     UTC; it now pins the time zone, and all 212 tests pass. The fix is
     one line in tests/checkout.rs, not committed yet.
You  Nice. Send me the diff.
SCV  Here it is.
SCV  [file] checkout-timezone.diff
```

SCV answers the quick question itself while Codex works, relays Codex's report
without being asked, and sends the diff as a file. Before a step that cannot be
undone, work can also ask you yes or no in the chat with `scv confirm`; SCV's
own release flow asks this way before it publishes.

## How it works

```text
   Feishu/Lark · WeChat                        Terminal
     (your phone)                              (scv TUI)
          │ bot account, QR sign-in               │ Unix socket
          ▼                                       ▼
 ┌─ scv daemon ──────────────────────────────────────────────────┐
 │  chat bridges ─► sessions ─► agent loop ─► OpenAI-compatible  │
 │                                              model endpoint   │
 │  supervisor · approvals · context budget · planned restarts   │
 └───────────────────────────────┬───────────────────────────────┘
                                 │ tools, gated by approval policy
                                 ▼
     read · write · bash · web_fetch · read_skill · chat_attach
     agent ─► Claude Code · Codex · Grok Build · DeepSeek Harness · pi · SCV
              over ACP, the agent's CLI, or SCV's protocol, in private homes
```

The daemon owns every session, tool, and approval. The TUI and each chat
account speak the same versioned protocol, so a message from your phone runs
exactly like a turn in the terminal. See the
[architecture](https://github.com/PeiyuanQi/scv/blob/main/docs/architecture.md).

## Configuration

Everything SCV keeps lives under `~/.scv` (or `--scv-home`), and the one file
you edit is `config.toml`:

```toml
[provider]
active = "openai"

[providers.openai]
kind = "openai-compatible"
model = "gpt-4.1-mini"
base_url = "https://api.openai.com/v1"
api_key = "sk-your-key"
# reasoning_effort = "high"   # with a reasoning model: how hard SCV's own model thinks

[tools]
approval_policy = "on-risk"   # reads run; writes, shell, and agents ask first

[agent]
prefer = ["codex", "claude"]  # who gets delegated work first

[agents.claude]
use_for = "coding"
model = "opus[1m]"            # a value `scv agents check` lists for claude
effort = "xhigh"

[agents.grok]
use_for = "current events, and anything that needs posts on X"
effort = "medium"             # when a call names no effort
hard_task_effort = "high"     # what the main agent passes for a hard task

[notify]
owner = ["feishu:default"]    # where unprompted notices go
```

`scv config show` prints every path and setting with where it came from,
secrets hidden. The full schema and trust rules are in
[docs/configuration.md](https://github.com/PeiyuanQi/scv/blob/main/docs/configuration.md).

## Safety

SCV is **not a sandbox**: approved commands and delegated agents run with your
user's permissions. Work in version-controlled workspaces, read what you
approve, and use a container for untrusted code. SCV is pre-1.0, so its
protocol and configuration may still change. See the
[security model](https://github.com/PeiyuanQi/scv/blob/main/docs/security.md),
and report vulnerabilities as
[SECURITY.md](https://github.com/PeiyuanQi/scv/blob/main/SECURITY.md) describes.

## Documentation

| Guide | What's inside |
| --- | --- |
| [Architecture](https://github.com/PeiyuanQi/scv/blob/main/docs/architecture.md) | Crates, the agent loop, planned restarts, and where to start reading the code |
| [Channels](https://github.com/PeiyuanQi/scv/blob/main/docs/channels.md) | Feishu/Lark, WeChat, and Slack: sign-in, media, background reports, questions to the owner; read-only mail triage reported to a mail chat |
| [Tools](https://github.com/PeiyuanQi/scv/blob/main/docs/tools.md) | Built-in tools, delegated agents, their models and checks, ACP, background jobs, and agent sign-ins |
| [Configuration](https://github.com/PeiyuanQi/scv/blob/main/docs/configuration.md) | Instance layout, every setting, providers, the daemon, and notices |
| [Security](https://github.com/PeiyuanQi/scv/blob/main/docs/security.md) | Trust boundaries, approvals, remote tools, and delegated runs |
| [Context management](https://github.com/PeiyuanQi/scv/blob/main/docs/context-management.md) | The token budget and deterministic compaction |
| [Protocol](https://github.com/PeiyuanQi/scv/blob/main/docs/protocol.md) | The JSONL protocol for clients |
| [Terminal UI](https://github.com/PeiyuanQi/scv/blob/main/docs/tui.md) | Keys, layout, approvals, and headless `scv exec` |
| [Release](https://github.com/PeiyuanQi/scv/blob/main/docs/release.md) | Platforms, compatibility, upgrade notes, and publishing |
| [Quality](https://github.com/PeiyuanQi/scv/blob/main/docs/quality.md) and [evaluation](https://github.com/PeiyuanQi/scv/blob/main/docs/evaluation.md) | Test and performance contract, and the v0.1 measurements |

## Contributing

Contributions are welcome. Build from source and run the checks:

```bash
git clone https://github.com/PeiyuanQi/scv.git && cd scv
cargo build --workspace --locked
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources  # CI always runs it
cargo build --release --locked
git diff --check
```

Tests use scripted providers and fake agents, so they need no API key.
[CONTRIBUTING.md](https://github.com/PeiyuanQi/scv/blob/main/CONTRIBUTING.md)
covers running a development daemon beside your real one, and
[AGENTS.md](https://github.com/PeiyuanQi/scv/blob/main/AGENTS.md) holds the
rules coding agents follow in this repository.

## License

SCV is licensed under the
[Apache License 2.0](https://github.com/PeiyuanQi/scv/blob/main/LICENSE). See
[NOTICE](https://github.com/PeiyuanQi/scv/blob/main/NOTICE).
