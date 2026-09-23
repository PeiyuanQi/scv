# Delegation Implementation Plan

Status: approved; in progress

## Objective

Make delegated agents (`agent_*`) and other SCV instances usable for sustained
work: multi-turn conversations, research, coding, and landing a change through
CI and release. SCV must always know what it started and clean it up.

## Decisions

- Delegated agents are trusted and run as the user. Cleanup is cooperative: it
  catches accidental leaks, not a child that deliberately escapes. OS
  sandboxing is out of scope.
- Delegation stays a tool. The core loop gains only progress reporting and
  in-tool approvals.
- The model sees SCV-issued handles such as `codex-2`, never vendor session
  IDs. Handles belong to the parent session.
- Each adapter declares its capabilities in the adapter table: output format,
  resume support, live transport, permission flags, and conversation file
  locations.
- Two transports:
  - **resume**: one process per turn, continued through the agent's own
    session files (`claude --resume`, `codex exec resume`);
  - **live**: one long-running stdio child per conversation, speaking JSON-RPC
    2.0 (ACP) or the SCV protocol.

## Steps

Each step is one PR landed through the feature-flow skill and released with the
next patch version at landing time. Steps 2 and 4 do not depend on the adapter
table and may be developed in parallel with others; landings stay sequential.

| # | Step | Depends on |
|---|---|---|
| 0a | Replay tool calls to the Responses API (done, 0.1.21) | — |
| 0b | Adapter table, grok/dsh/pi, full-work defaults, `permissions` (done, 0.1.22; zcode deferred) | 0a |
| 1 | Delegation foundation: structured results, registry, cleanup (done, 0.1.26) | 0b |
| 2 | ClawBot long-turn resilience (done, 0.1.23) | 0b landed |
| 3 | Multi-turn conversations (resume) (done, 0.1.28) | 1 |
| 4 | SCV web tools: `web_fetch`, `web_search` (done, 0.1.25) | 0b landed |
| 5 | Progress events and protocol v3 (done, 0.1.30) | 3 |
| 6a | Live mode: SCV to SCV (`agent_scv`) (done, 0.1.31) | 5 |
| 6b | Live mode: ACP adapter (done, 0.1.33) | 6a |
| 7 | Background delegations | 6, iLink check |

### 0b. Adapter table and full-work defaults

One data-driven adapter descriptor per agent. New defaults sized for feature
work through release: `tools.agent_timeout_seconds` 3600,
`tools.max_timeout_seconds` 14400, `tools.command_timeout_seconds` 600,
`agent.max_steps` 128. Per-agent `permissions = "default" | "full"`; `full`
adds each CLI's own full-autonomy and web-search flags and is shown in the
approval summary. The built-in default stays `default`.

### 1. Delegation foundation

- Adapters declare an output format; parsers produce one result:
  `{agent, status: completed|failed|timeout|cancelled, reply, usage,
  exit_code, stderr_tail, truncated}`. Claude uses
  `--output-format stream-json --verbose --session-id <uuid>`; Codex uses
  `exec --json` with `-o <file>` as a fallback; pi uses `--mode json`; Grok
  and DeepSeek Harness stay plain text. Full logs never enter the parent
  history.
- A delegation registry per SCV process records handle, agent, parent session,
  `cwd`, pid, pgid, `/proc` start time, owning process, and state. As built,
  the agent tools receive the registry and session ID through
  `ToolsConfig.delegation` when the session's tools are built, rather than
  through `ToolContext`, so the core crate stays unaware of delegation.
- Every child gets `SCV_PARENT=<instance>/<session>/<handle>` and
  `SCV_DELEGATION_DEPTH`. Delegation is refused at
  `agent.max_delegation_depth` (default 2; `[agents]` holds only per-adapter
  tables). `scv run/start/stop/restart/update/clawbot` are refused at
  depth > 0.
- `$SCV_HOME/run/delegations/<handle>.json` (0600, atomic) is written at spawn
  and removed at reap. The daemon reconciles at startup and every 60 seconds,
  killing groups whose owning process died.
- The daemon becomes a child subreaper on Linux. After a delegation exits,
  processes still tagged with its handle get TERM, then KILL after 2 seconds.
- `scv agents ps [--all]`, `scv agents kill <handle>|--orphans`, and active
  and reaped counts in `scv status`.

### 2. ClawBot long-turn resilience

- Keep polling while a sender's turn runs, so a long owner turn neither blocks
  other senders nor the owner's later messages, which queue in the owner's
  session.
- A final reply that iLink rejects as expired is kept and delivered with the
  sender's next message instead of being dropped.

### 3. Multi-turn conversations

- `agent_* {prompt, cwd?, session?, timeout_seconds?, model?, effort?}`.
  Omitting `session` starts a conversation and returns its handle; passing it
  continues the conversation and returns `turn`.
- A conversation keeps its agent and `cwd`. One turn at a time; a busy
  conversation returns `session busy`. A timed-out turn stays resumable.
- `agent.max_conversations` (default 8 per session) and
  `agent.conversation_idle_seconds` (default 86400), under `[agent]` like
  `agent.max_delegation_depth`. Handles end with the parent session.
- Claude Code, Codex, and pi resume; Grok and DeepSeek Harness start fresh on
  every call until their resume options can be verified headless.
- `scv agents gc --older-than 30d` removes conversation files in the private
  agent homes, skipping live ones (marked in `$SCV_HOME/run/conversations`)
  and anything written in the last hour.

### 4. SCV web tools

- `web_fetch`: bounded HTTP(S) GET with HTML-to-text conversion, redirect and
  size limits, and loopback, link-local, and private addresses refused by
  default. HTTPS hosts in `web.auto_approve_domains` run without approval;
  other URLs have `network` risk and need approval.
- `web_search`: the provider's hosted Responses `web_search` tool
  (`web.search = "provider"`), or a `web_search` tool backed by SearXNG or
  Brave Search. Without a configured source there is no search.
- Tool-free sessions never get them.

### 5. Progress events and protocol v3

- `CoreEvent::ToolProgress { call_id, text }` and a `progress` sink in
  `ToolContext`.
- Protocol v3 adds `tool.progress` (at most two per second per call, 512 bytes
  each) and an optional `session.start.delegation_depth`.
- Adapter parsers report commands run, files changed, and tool use. The TUI
  shows the latest line under the running tool; ClawBot ignores progress.
- As built: the core loop paces events (one per 500 ms per call, dropping lines
  reported within 500 ms of completion) so every server gets the same limit,
  and the server bounds the text again. Parsers redact credential-like values.
  A session's declared `delegation_depth` raises the depth its runs count from.

### 6. Live mode

6a (done, 0.1.31):

- A live child per conversation, registered like any delegation. Closing stdin
  is followed by a 2-second grace period and a group kill.
  `scv_tools::live::LiveChild` is protocol-neutral, and the conversation
  store keeps it as the conversation's attachment, so 6b reuses both.
- `ToolContext.approvals` gives tools the session's approval gate.
- `agent_scv` runs `scv server --stdio` with its own home
  (`~/.scv/adapters/scv`), configured by `scv agents import scv`. The child's
  `approval.requested` goes through the parent's approval gate; cancellation
  and timeouts become `turn.cancel`.
- Future work: `[agents.scv] socket`, attaching to an existing daemon instead
  of starting a child.

6b (done, 0.1.33):

- An ACP adapter maps `session/new`, `session/prompt`, `session/update`,
  `session/request_permission`, and `session/cancel`, and offers no client
  file or terminal capabilities. Adapters may prefer ACP and fall back to
  resume.
- ACP v1 servers: Claude Code and Codex through the ACP organisation's
  official adapters (`claude-agent-acp`, `codex-acp`), Grok Build and
  DeepSeek Harness natively (`grok agent stdio`, `dsh --profile acp`). pi has
  only a community adapter and stays on resume; `agent_scv` keeps the SCV
  protocol.
- `[agents.<name>] transport = "auto" | "acp" | "resume"`; `auto` prefers an
  installed ACP server unless `command` is customised. `permissions = "full"`
  maps onto each agent's own session mode or flag.

### 7. Background delegations

- First, with the owner present, check whether iLink accepts a message that is
  not a reply, or a reply with an old `context_token`.
- `agent_* {background: true}` returns a running handle. `agent_wait` and
  `agent_status` observe it. Completion queues a turn in the parent session.
- WeChat sends the result directly when iLink allows it, otherwise with the
  owner's next message. `agents.max_background` defaults to 2 per session.

## Testing

CI uses fake CLIs, fake ACP agents, and fake providers. After each deploy the
implementing agent runs live checks against the real Claude, Codex, and SCV
and reports them.

## Risks

- Vendor JSON formats change. Parsers tolerate unknown fields, fall back to
  text, and keep recorded-sample tests.
- Protocol v3 makes TUI processes from older versions reconnect with
  `version_mismatch`; they must be restarted.
- ACP adapter packages may be unavailable; step 6 then falls back to resume.
- Conversation files grow in the private agent homes until `gc` runs.
