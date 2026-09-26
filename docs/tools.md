# Built-in Tools

Status: final design

All tool inputs are validated JSON objects. Tools execute serially and receive a
cancellation token, workspace root, timeout, and output limits from the server.

## `read`

```json
{"path":"src/main.rs","offset":0,"limit":65536}
```

`path` is a required workspace-relative UTF-8 path. `offset` and `limit` are
optional byte values. The result reports the selected content, total byte
length, and whether it was truncated. The selected byte range must be UTF-8 or
the tool returns a clear error. Workspace reads are read-only risk;
lexically secret-like paths are elevated to filesystem risk and require
approval. This name heuristic improves visibility but is not a confidentiality
boundary; an OS sandbox is still required for isolation.

## `write`

```json
{"path":"src/main.rs","content":"fn main() {}\n","mode":"replace","expected_sha256":"..."}
```

`mode` is `create` or `replace`. `create` fails if the path exists. `replace`
fails if it does not exist. `expected_sha256` is optional for the first write
but, when supplied, must match the file observed immediately before the write.
It detects stale input but is not a filesystem lock against an external writer.
The tool writes a capability-contained temporary file in the destination
directory, flushes it, and installs it atomically. All writes have filesystem
risk.

## `bash`

```json
{"command":"cargo test --workspace","timeout_seconds":900}
```

`command` is passed to `/bin/bash -lc` in the workspace. Without
`timeout_seconds` a call gets `tools.command_timeout_seconds` (default 600).
A call may choose any timeout up to `tools.max_timeout_seconds` (default 14400),
which the schema advertises as its `maximum`; a larger request is refused
before approval with an error naming the ceiling. The result contains exit status and bounded
combined stdout/stderr, with truncation metadata. Shell execution has process
risk and is not sandboxed.

SCV creates a new process group for shell and native-agent children. On
cancellation it sends `TERM` to the whole group, allows up to two seconds for
cleanup, then sends `KILL` to any remaining members. Reaching the configured
deadline sends `KILL` immediately. SCV also cleans up descendants after the
group leader exits and bounds output-pipe draining, so a background child cannot
keep a tool call alive indefinitely.

## Sending files to a chat (`chat_attach`)

```json
{"path":"out/chart.png","caption":"Sales by month"}
```

Offered only in a tool-enabled session whose client named a chat `channel`
in `session.start`: an account owner's WeChat or Feishu session. It sends a
file to the user after the reply text: PNG, JPEG, GIF, WebP, and BMP images
arrive as pictures, video as video where the platform has it, and anything
else as a file. `path` is absolute or relative to the workspace; `caption` is
optional, at most 1 KiB, and goes into the reply text as `<name>: <caption>`.
Call it once per file; a reply carries at most 8 files, and the rest are
reported as not sent. The tool has network risk, since the file leaves the
host; a chat owner's session approves it like any other call.

The model's input can carry injected instructions, so the tool refuses, after
resolving symlinks:

- anything that is not a regular, non-empty file of at most 25 MiB;
- the SCV instance directory (`SCV_HOME`: its settings, credentials, agent
  homes, and state), except the media directory holding what chat users sent;
- credential and key locations under the user's home: `.ssh`, `.gnupg`,
  `.aws`, `.azure`, `.kube`, `.docker`, `.netrc`, `.git-credentials`,
  `.npmrc`, `.pypirc`, `.cargo/credentials(.toml)`, `.config/gh`,
  `.config/gcloud`, `.config/hub`, browser profiles, `.password-store`,
  `.local/share/keyrings`, and the `.codex`, `.claude`, `.claude.json`,
  `.grok`, and `.scv` directories;
- host secrets: `/etc/shadow`, `/etc/gshadow`, `/etc/ssh`, `/etc/sudoers`,
  `/root`, `/proc`, `/sys`, and `/dev`;
- any path with a secret-like name: `.env` files, names containing
  `credential` or `private_key`, `.pem`, `.key`, `.p12`, `.pfx`, `.kdbx`, and
  SSH key names such as `id_ed25519`.

It then opens the file without following a final symlink, checks it again
through the open handle, and copies it into the channels' media outbox
(`$SCV_HOME/state/media/outbox`, mode `0600`); the result reports that copy.
The chat bridge sends only regular files inside the outbox, from a durable
delivery record, and deletes each copy once it is sent or refused. This keeps
a prompt from mailing out keys by path; it is not a sandbox, and a model with
`bash` can still copy data elsewhere.

## `web_fetch`

```json
{"url":"https://docs.rs/serde/latest/serde/","offset":0}
```

Fetches one public `http` or `https` URL with a GET request and returns it as
text. HTML becomes readable plain text with numbered link references, JSON and
other text types pass through, and a declared binary type such as an image,
PDF, or archive is refused before it downloads. The result begins with the
final URL, status, content type, and the character range returned. SCV
downloads at most `web.fetch_max_bytes` (default 2 MiB) and returns at most
`tools.output_limit_bytes`; a longer page ends with the `offset` that returns
its next part. An HTTP error status returns the body as a failed result.

Requests carry a `scv/<version>` User-Agent and no cookies, credentials, or
referrer, ignore proxy variables, follow at most `web.max_redirects` (default
5) redirects, and stop after `web.fetch_timeout_seconds` (default 30). URLs
with another scheme or embedded credentials are refused.

Only public addresses are reachable unless `web.allow_private_addresses` is
set. Loopback, private, shared (CGNAT), link-local (including cloud metadata
at `169.254.169.254`), multicast, reserved, and documentation ranges, IPv6
unique-local and link-local addresses, and IPv6 forms that embed a refused
IPv4 address are all refused. A host name is resolved once by a checking
resolver: if any of its addresses is refused the name is refused, and the
connection uses only the checked addresses, so a name cannot be rebound to a
private address between check and connect. IP-literal URLs and every redirect
target are checked the same way.

An HTTPS URL whose host is in `web.auto_approve_domains` has `read_only`
risk, so it runs without approval under `on-risk` and is allowed under
`never`. A redirect from such a fetch may lead only to another listed HTTPS
host; otherwise the call fails and names the target, which the model can then
request on its own. Every other URL has `network` risk and needs approval,
because the URL itself can carry data the model has read to a host that a
prompt-injected page chose. The approval summary shows the full URL.

## `web_search`

With `web.search = "provider"`, each model request also offers the endpoint's
hosted Responses tool `{"type":"web_search"}`. The provider runs the searches
and returns the answer with `url_citation` annotations; SCV appends a
`Sources:` list for any cited URL that the answer does not already link. No
SCV tool call or approval is involved, and the queries reach only the model
provider, which already receives the conversation.

With `web.search = "searxng"` or `"brave"`, SCV registers a `web_search` tool:

```json
{"query":"tokio latest version","count":5}
```

It returns up to `count` (at most `web.max_search_results`, default 8) titles,
URLs, and snippets from SearXNG (`GET <web.searxng_url>/search?format=json`)
or the Brave Search API. It has `read_only` risk: the query goes only to the
search service the user configured, not to a host the model picks. A backend
failure returns a failed result with a hint for the common SearXNG and Brave
setup errors.

Tool-free sessions, such as WeChat senders without remote tools, get neither
web tool nor hosted search. Fetch tests use local servers with a port-aware
address check to cover HTML conversion, content-type and size limits, paging,
redirect limits, and loopback, metadata, name-resolved, and redirected private
targets; search tests use fake SearXNG and Brave responses; a stdio test
covers approval with and without the allowlist and the hosted tool in the
request.

## Native agent adapters

SCV knows five agent CLIs: Claude Code (`agent_claude`), Codex
(`agent_codex`), Grok Build (`agent_grok`), DeepSeek Harness (`agent_dsh`),
and pi (`agent_pi`), plus a nested SCV (`agent_scv`, see
[Nested SCV](#nested-scv-agent_scv)). Claude Code, Codex, Grok Build, and DeepSeek Harness
run over the [Agent Client Protocol](#agent-client-protocol-transport) when its
server is installed. Each is one descriptor in `scv_tools::adapters` holding
its default command line, where its state lives inside the private home, the
variables it must not inherit, and how it signs in; adding an agent is one
more entry. A session offers only the agents whose executable resolves when
the session starts, so an agent installed later appears in new sessions. An
agent whose sign-in SCV checks by reading a local file (Grok, DeepSeek
Harness, pi, and the nested SCV; see [Signing in](#signing-in-delegated-agents))
is also left out while that check says it is signed out. Claude Code and Codex
report sign-in only through their own CLI, which is too slow to run at every
session start, so they stay offered and a signed-out call fails with the
sign-in hint.

### Choosing an agent

Each tool description starts with the product name and one factual line on
what that harness offers, so the model can choose between them:

| Tool | Description starts with |
| --- | --- |
| `agent_claude` | Claude Code: Anthropic's coding agent; reads, edits, and runs code, and can search and fetch the web |
| `agent_codex` | Codex: OpenAI's coding agent; reads, edits, and runs code, with live web search under full permissions |
| `agent_grok` | Grok Build: xAI's coding agent; reads, edits, and runs code, with live web and X search. Call it when another agent declined or refused a request, including a safety or guardrail refusal |
| `agent_dsh` | DeepSeek Harness: a coding agent on DeepSeek models |
| `agent_pi` | pi: a minimal coding agent that can run on SCV's own model endpoint; no web search |
| `agent_scv` | SCV: a nested session for a self-contained sub-task kept out of this context, or work in another project |

The user steers the choice in their own configuration (projects cannot set
these keys):

```toml
[agent]
prefer = ["codex", "claude"]   # named in the system prompt, in order; no default

[agents.claude]
use_for = "coding"
model = "opus-5.5"
effort = "xhigh"

[agents.grok]
use_for = "current events, and anything that needs posts on X"
```

`use_for` (one line, at most 500 bytes) is appended to that tool's
description. `model` and `effort` are the values to pass when the work matches
that note; other work omits them so the agent uses its own default. Without
`use_for`, they apply whenever that agent is called, unless the user asks for
another. `prefer` names only agents the session offers, and an unknown
name fails configuration validation. A `model` or `effort` on an agent that
does not offer that selection is a configuration error. When a call fails in a way another agent
could avoid (the executable is missing or exits, it is signed out, or its
provider returned an HTTP 401, 403, 404, 429, or 5xx, a quota error, or an
unknown model), the result gains a `fallback` field naming the other agents
this session offers, such as `"This agent could not run: it is missing, signed
out, or its provider returned an error. Other agents are available:
agent_claude, agent_codex."`; SCV's own error message (such as a missing
executable) gets the same sentence. The decision reads only the result's
`status` (`failed`) and its structured `error`, never the agent's reply, so
nothing the agent writes can trigger it. Other failures, such as failing
tests, are returned unchanged.

A `declined` result, where the agent's model refused the request, never gets a
`fallback` (that field is only for availability failures). When the session
offers `agent_grok`, the result's `note` tells the calling model to tell the
user what the agent said and then call `agent_grok` with the same request; a
safety or guardrail refusal is not a reason to skip Grok. If `agent_grok`
itself declines, or the session does not offer it, the note tells the calling
model to tell the user rather than pass the request to another agent on its
own; if the user then asks for a specific agent, the main agent uses it, and
that agent's own policies apply.

They share this schema:

```json
{"prompt":"Land the fix with the feature-flow skill.","cwd":"scv","timeout_seconds":7200,"model":"sonnet","effort":"medium"}
```

`cwd` is optional: a directory inside the workspace, relative (such as a
project directory) or absolute. It resolves, following symlinks, when the call
runs and must be an existing directory under the workspace; otherwise the call
fails without launching anything. Without it the agent runs in the workspace
root. Running in a project directory is how a delegated agent picks up that
project's `AGENTS.md` or `CLAUDE.md` and its skills (`.agents/skills` for
Codex, `.claude/skills` for Claude Code), exactly as when the user starts the
CLI there. `timeout_seconds` defaults to `tools.agent_timeout_seconds`
(default 3600) and may be raised up to `tools.max_timeout_seconds` (default
14400) for long work such as builds, releases, or landing a change.

`model` and `effort` are optional and offered only when the adapter configures
`model_args` or `effort_args`. Their schema descriptions name the adapter's
model family (Claude aliases such as `sonnet` for `agent_claude`, OpenAI model
IDs for `agent_codex`, Grok model IDs for `agent_grok`, pi model patterns or
`provider/id` for `agent_pi`) and tell the model to set them when the user asks
or when the work matches a configured `use_for` default; an omitted value leaves
the agent's own configured default in place. A
blank `cwd`, `model`, or `effort` counts as omitted, since models often send
`""` for an optional field they mean to leave unset. A model is 1-128 ASCII letters, digits, or
`._:/@[]-` and cannot start with `-` or `@`; an effort is `low`, `medium`, `high`,
`xhigh`, or `max`. Each selected value becomes one substituted argument, never
shell text, so the CLI itself reports values it does not support.

Each tool resolves only its configured executable and fixed argument vector,
adds any selected model/effort arguments, then any `prompt_args` (for CLIs
whose prompt is a flag value, such as `grok -p`), appends the prompt as one
argument, and starts it directly in the workspace or the selected `cwd`. A
prompt cannot start with `-`, so it is never read as a flag.
The executable is looked up in the user's per-user install directories
(`~/.local/bin`, plus `~/.grok/bin` for Grok) before `PATH`, the order a login
shell uses, because the user service's `PATH` omits them; the daemon therefore
runs the same install the user's shell does.
Native adapters receive an instance-private `HOME`, `SCV_HOME`, and XDG
configuration/data/state directory, plus the agent's own state location inside
it: `CODEX_HOME`, `GROK_HOME` (`.grok`), `DSH_HOME` (`.dsh`), or
`PI_CODING_AGENT_DIR` (`.pi/agent`). Grok also gets
`GROK_DISABLE_AUTOUPDATER=1`. Before those are set, SCV removes every inherited
variable ending in `_API_KEY`, SCV's selector variables, and each adapter's
credential, endpoint, and state variables (`ANTHROPIC_*` keys and tokens,
`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, `OPENAI_BASE_URL`,
`CODEX_BASE_URL`, `GROK_*`, `DSH_*`, `DEEPSEEK_BASE_URL`, `PI_*`, and similar),
so no nested agent reuses the parent's configuration or another agent's
credentials from outside its private home. The `bash` tool retains the normal
inherited environment for compatibility.
The model cannot supply other flags or a different executable. Timeout,
cancellation, and process-group behavior match `bash`; the output is the
structured result described below. Adapter execution has delegate risk because
the child agent may independently read, write, run commands, access inherited
credentials, or ask its own model provider.

The default invocation contracts are:

| Tool | Invocation |
| --- | --- |
| `agent_claude` | `claude -p --output-format stream-json --verbose --session-id <uuid> [--model <model>] [--effort <effort>] <prompt>` |
| `agent_codex` | `codex exec --json -o <file> [-m <model>] [-c model_reasoning_effort="<effort>"] <prompt>` |
| `agent_grok` | `grok [-m <model>] [--reasoning-effort <effort>] -p <prompt>` |
| `agent_dsh` | `dsh --profile headless <prompt>` |
| `agent_pi` | `pi -p --mode json [--model <model>] [--thinking <effort>] <prompt>` |

Grok's `-p` and pi's `-p` run one prompt and exit. DeepSeek Harness takes its
model from its profile, so `agent_dsh` offers no `model` or `effort`.
`permissions = "full"` switches follow the fixed arguments, before the output
format arguments.

### Results

Each adapter declares its output format, and SCV reads the CLI's stdout as it
arrives, keeping only the reply, token usage, and error, so a long run's event
log never reaches the parent model:

- Claude Code's `stream-json` events end with a `result` event carrying the
  reply, `is_error`, and usage; SCV picks the `--session-id` itself when a
  conversation starts.
- Codex's `--json` events give the last `agent_message` item as the reply,
  `turn.completed` usage, and `turn.failed` or `error` messages. `-o` names a
  file in a private `tmp` directory of Codex's agent home holding the final
  message, used if the stream carried none and deleted afterwards.
- pi's `--mode json` gives the last assistant `message_end` text and usage.
- Grok and DeepSeek Harness stay plain text: stdout is the reply. Grok's JSON
  output exists but its success shape could not be verified signed out.

Unknown events and fields are ignored, lines over 4 MiB are skipped, and a
structured stream with no JSON at all falls back to its text. The tool result
is one object:

```json
{"agent":"codex","status":"completed","reply":"…","usage":{"input_tokens":11257,"output_tokens":5},"exit_code":0,"stderr_tail":"…","truncated":false}
```

`status` is `completed`, `failed` (non-zero exit or a reported error),
`declined` (the model stopped with a `refusal` stop reason: Claude Code's
`stop_reason` in stream-json, or ACP's `stopReason`), `timeout`, or `cancelled`
(stopped by `scv agents kill`). `reply` is bounded by
`tools.output_limit_bytes` on a character boundary, `stderr_tail` holds the
last 2 KiB of stderr, and `truncated` says whether anything was cut. A failed
run also carries `error`, what went wrong as the CLI or SCV reported it and
never the model's reply: Claude Code's result of a failed run, Codex's
`turn.failed` or `error` message, pi's `errorMessage`, a failed plain-text
run's closing output, and the stderr tail. A failure whose `error` reads like
a missing sign-in gains a `hint` (see below), and a `declined` result carries
a `note` for the calling model. A turn of a conversation (next section) also
carries `"session"` and `"turn"`.

### Progress

While a structured run lasts, its events also become short status lines that
clients see as `tool.progress` (see [protocol](protocol.md#tool-lifecycle-and-approval)):

- Codex: `$ <command>` when a command starts, `exit <code>: <command>` when one
  fails, `<kind> <file>` for each file change, and `search: <query>`.
- Claude Code: `$ <command>` for Bash, `<tool> <file>` for file tools,
  `search:`/`fetch` for web tools, the tool name otherwise, and the first line of
  its interim text.
- pi: `$ <command>` for bash, `<tool> <file>` for file tools, the tool name
  otherwise, and `<tool> failed` for a failed call.
- Grok and DeepSeek Harness: none, since their output is plain text.

Lines never include command output. Paths show their last two components,
URLs lose their query, and values that look like credentials (`Bearer` tokens,
`NAME=value` or `--name value` where the name mentions a key, token, secret, or
password, and well-known token prefixes) become `…`. The redaction is a display
heuristic, not a guarantee. The runtime forwards pending lines at most twice a
second per call and never adds them to the model's history or the tool result.

### Conversations

An agent whose CLI can resume a session takes an optional `session` argument.
Omitting it starts a conversation; the result's `session` is an SCV-issued
handle such as `codex-2`, and passing it back continues that conversation with
the agent's own context:

| Tool | Starts with | Continues with |
| --- | --- | --- |
| `agent_claude` | `--session-id <uuid>` (chosen by SCV) | `--resume <uuid>` |
| `agent_codex` | nothing; the thread ID comes from `thread.started` | `codex exec resume … <thread-id> <prompt>` |
| `agent_pi` | `--session-id <uuid>` (chosen by SCV) | `--session-id <uuid>` |

Grok and DeepSeek Harness start a fresh conversation on every call: their
resume options could not be verified headless here, so their tools offer no
`session` argument and refuse one.

The model only ever sees handles. The CLI's own session IDs stay inside SCV,
and a value that is not one of this session's handles, such as a raw vendor
ID, is refused. A conversation keeps its agent and `cwd`: continuing it
elsewhere is an error, so start a new one there instead. One turn runs at a
time; a second call while a turn runs returns `session busy`. A turn that
times out stays resumable once the CLI has reported its session, so the next
turn can ask the agent to continue where it stopped. A first turn that fails
before the CLI reports a session is forgotten.

Each session remembers at most `agent.max_conversations` conversations
(default 8; starting another forgets the least recently used idle one) and
forgets one left unused for `agent.conversation_idle_seconds` (default 86400).
Handles end with the SCV session. `scv agents ps` shows a running turn's
conversation and turn number.

The CLIs keep transcripts in their private agent homes (Claude Code under
`.claude/projects`, Codex under `sessions`, pi under `.pi/agent/sessions`).
`scv agents gc` removes old ones:

```sh
scv agents gc --dry-run                 # what would go, per agent
scv agents gc --older-than 7d codex     # default 30d, never less than an hour
```

A live conversation leaves a marker in `$SCV_HOME/state/conversations`, named
after its CLI session ID, and `gc` keeps any transcript a marker whose SCV
process still runs names. Transcripts written in the last hour are always
kept, and symlinks are never followed.

### Background jobs

Any `agent_*` call may set `"background": true`. The call then returns at once
with a job handle, `{"job":"job-1","tool":"agent_codex","status":"running",
"background":true}`, while the agent keeps working, so the turn ends and the
user can keep talking to the main agent. The system prompt makes this the
default way to work (see [Delegate first](#delegate-first)). The job runs
exactly as a foreground call would, in its conversation (`session`, `cwd`,
model, and timeout apply as usual) and tracked like any delegation, and its
final result is the structured result above. Three tools manage a session's
jobs:

- `agent_wait {job, timeout_seconds?}` blocks until the job finishes or the
  timeout passes (default `tools.agent_timeout_seconds`, at most
  `tools.max_timeout_seconds`) and returns `{"job","status","elapsed_seconds",
  "result"}`, or `"status":"running"` with its latest progress line;
- `agent_status {job?}` describes one job, or `{"jobs":[...]}` for every job
  the session remembers: running ones with their latest progress, finished
  ones with their result;
- `agent_cancel {job}` stops a running job: its call is cancelled, which stops
  the agent's process group and anything tagged with it, exactly as closing the
  session would. It waits up to 10 seconds for the job to settle and returns it
  with `"status":"cancelled"`; no report turn follows, since the model asked
  for the stop. A job that already finished is described with a note. It has
  process risk, so the approval policy decides it like `bash`.

`agent_wait` and `agent_status` are read-only. A session runs at most
`agent.max_background` jobs at once (default 4; 0 turns background calls and
all three tools off), and a start beyond that is refused with an error naming
the limit and `agent_cancel`. It remembers its 16 newest finished jobs.

When a job finishes and the model has not already seen its result through
`agent_wait` or `agent_status`, the server reports it: once the session is idle
(the user's own queued prompts run first), it starts a turn of its own whose
prompt, beginning `[SCV background report]`, names each finished job, its
agent, conversation, status, and bounded reply, and asks the model to tell the
user. That turn's `turn.started` and final event carry
`"origin":{"kind":"background","jobs":[...]}` (see
[protocol](protocol.md#server-started-turns)); one turn reports up to four jobs.
A chat channel (WeChat or Feishu) sends the owner the answer as an unprompted message; `scv exec`
prints it and stays open until every job it started has been reported; the
TUI shows it like any turn.

A job has no turn to carry an approval request to a person, so each request a
nested agent relays (over ACP or from a nested SCV) gets the answer the
session would give without asking anyone, and never more than the same
request would get in the foreground:

1. what `tools.approval_policy` decides on its own: `on-risk` approves
   read-only requests, `never` approves read-only requests and denies the rest;
2. otherwise, when the client declared in `session.start` that it approves
   every request unasked (`auto_approve`, which a chat bridge sets for its
   owner's session), that approval;
3. otherwise a denial.

So a WeChat or Feishu owner's background agents get the approvals the owner's
foreground turns get, while in the TUI, which asks a person, a background
job's non-read-only requests are denied and it relies on the agent's own
permissions, such as `permissions = "full"`. Jobs belong to their session:
closing it (a TUI or `scv exec` exiting, an idle channel conversation ending)
cancels every job still running and kills its processes. A channel
conversation stays open while its jobs run, and a full channel session table
never closes it to make room.

### Delegate first

When a session offers agent tools, the system prompt adds a *Delegating work*
section, generated from the tools actually offered (so it applies even when
`agent.system_prompt` is replaced). It names each offered agent with its
product, adds `agent.prefer` and any `[agents.<name>] use_for` / `model` /
`effort` defaults, and, when background jobs are on, asks the main
agent to stay available: answer quick things (short reads, lookups, status
checks) itself, and hand real work (changes, multi-step investigation, builds,
tests, releases, anything likely to take more than about a minute) to a
background job with a self-contained brief, then reply at once with what it
started and the job handle. It relays each `[SCV background report]`, uses
`agent_status` and `agent_cancel` when the user asks, and keeps `agent_wait`,
foreground agent calls, and long `bash` commands for quick results it needs
within the turn, since those hold the turn open and the user cannot reach it
meanwhile. If an agent declines a request and the session offers `agent_grok`,
it tells the user what the agent said and calls `agent_grok` with the same
request; a safety or guardrail refusal is not a reason to skip Grok. If
`agent_grok` itself declines, or it is not offered, it tells the user rather
than passing the request to another agent on its own, and uses a specific
agent if the user then asks for one. The wording explains why rather than
issuing capitalised rules.
Without background jobs the section only asks for self-contained briefs.
A session started for a chat channel also gets a *Chat channel* section (see
[protocol](protocol.md#sessionstart)).

### Tracking and cleanup

Every delegated process gets `SCV_PARENT=<instance>/<session>/<handle>`
(appended to an inherited chain when SCV itself runs delegated) and
`SCV_DELEGATION_DEPTH`, one more than the caller's. Its descendants inherit
both. While it runs, SCV records it in `$SCV_HOME/state/delegations/<handle>.json`
(mode `0600`, directories `0700`, written atomically): handle, agent, parent
session, `cwd`, depth, and the PID plus start time of both the agent and the
SCV process that owns it, so a reused PID never matches.

When a run ends, SCV stops its process group and then any process still tagged
with its handle (TERM, then KILL after 2 seconds), including descendants that
left the group with `setsid`, and removes the record. A run abandoned
mid-flight is killed the same way. The daemon reconciles at startup and every
60 seconds: a record whose owning SCV process is gone (for example an
`scv exec` server that was SIGKILLed) is an orphan, and its group and tagged
processes are stopped and the record removed. A `scv server --stdio` also
reconciles once when it starts. Each pass also removes conversation markers
(below) whose owning SCV process has exited, which a short-lived `scv exec`
leaves behind. On Linux the daemon is a child subreaper, so
descendants a delegated agent leaves behind reparent to it rather than to
init, and it collects those that exit.

```sh
scv agents ps          # running delegations of this instance, from any SCV process
scv agents ps --all    # also orphans awaiting cleanup
scv agents kill codex-3f9a2c
scv agents kill --orphans
```

`scv status` shows the running count and how many orphans the daemon has
stopped. Agent tools are offered only while the session's own depth is below
`agent.max_delegation_depth` (default 2), and a delegated run may not start,
stop, restart, update, or run a daemon, or manage channels. This is cooperative:
see [Delegated runs](security.md#delegated-runs).

By default SCV adds nothing to an agent's own permission settings, and in
print mode Claude Code, for example, refuses Bash, Edit, and Write without a
permission mode. `[agents.<name>] permissions = "full"` is the user's explicit
opt-in to the CLI's own full-autonomy switches, placed after the fixed
arguments: `--permission-mode bypassPermissions` for Claude Code (which also
allows WebSearch and WebFetch), `--dangerously-bypass-approvals-and-sandbox -c
web_search="live"` for Codex (`codex exec` has no `--search` flag), and
`--always-approve` for Grok (web search is on by default); DeepSeek Harness
gets `DSH_PERMISSION_MODE=danger-full-access`, and pi, which has no approval
prompts, sandbox, or built-in web search, gets nothing. The approval summary
then states `FULL PERMISSIONS`. See
[Agent permissions](configuration.md#agent-permissions).

Before approval, SCV resolves the executable through the server environment
and displays its absolute path, full argument vector, bounded prompt, requested
directory, timeout, and delegate-risk warning; the approval request also
carries the session workspace. Project configuration cannot replace the
executable or arguments.

Adapter processes use instance-private state directories under
`$SCV_HOME/agents/<name>`. In particular, `agent_codex` receives
`CODEX_HOME=$SCV_HOME/agents/codex` and does not read the user's normal
`~/.codex` state, `agent_claude` does not read `~/.claude`, and Grok, DeepSeek
Harness, and pi never read `~/.grok`, `~/.dsh`, or `~/.pi`.

### Nested SCV (`agent_scv`)

`agent_scv` delegates to another SCV: a separate session in SCV's private
home `$SCV_HOME/agents/scv`, with its own context, instructions, and tools.
Unlike the CLI adapters, which start one process per turn, it keeps one
`scv server --stdio` running for a whole conversation and speaks the
[client protocol](protocol.md) to it:

```text
initialize (v3) → session.start {cwd, delegation_depth: parent + 1} → turn.start per call
```

- Its schema has `prompt`, `cwd`, `session`, `timeout_seconds`, and `model`
  (a new conversation only, sent as the session's model override); there is no
  `effort`.
- The first call starts the nested SCV and returns a handle such as `scv-1`;
  passing it as `session` sends the next prompt to the same nested session,
  which keeps its history. A conversation keeps its `cwd`, runs one turn at a
  time, and follows `agent.max_conversations` and
  `agent.conversation_idle_seconds` like the CLI conversations.
- The nested SCV's events become `tool.progress` of the calling tool:
  completed lines of its assistant text, `bash …` and `bash done|failed` as
  its tools start and finish, and its own tools' progress lines. Tool output
  never becomes progress.
- Its `approval.requested` goes through the calling session's approval gate
  with the summary prefixed `[scv-1 depth N]`, keeping the nested tool's name
  and risk, and the answer returns as `approval.resolve`. The session's policy
  and its user (or a WeChat owner's auto-approval) therefore decide every
  nested side effect; with no approval gate the request is denied.
- The result is `{"agent":"scv","status","reply","usage","session","turn"}`.
  A call that is cancelled or times out sends `turn.cancel`; a turn that
  settles within 2 seconds leaves the conversation usable (a timed-out turn is
  resumable), otherwise the nested SCV is shut down and the conversation
  forgotten. A nested SCV that exits mid-turn fails the call with its stderr
  tail and ends the conversation.
- The nested SCV is recorded like any delegation, so `scv agents ps` lists it
  with its current turn, `scv agents kill` stops it, and the orphan
  reconcile reaps it if its parent dies. It ends when its conversation is
  forgotten, expires, or its session ends: SCV closes its stdin (the server
  exits on EOF), waits 2 seconds, then kills its process group and anything
  still tagged with it. A reaper task waits on the process from the start, so
  when it exits between turns, by itself or through `scv agents kill`, it is
  collected at once (no zombie), the rest of its group is stopped, and its
  record leaves `scv agents ps`; the next turn of that conversation fails with
  "its server exited" and a new conversation starts cleanly.
- The nested SCV runs one delegation level deeper and declares that depth in
  `session.start`, so `agent.max_delegation_depth` applies on both sides: the
  default of 2 lets it delegate once more, and it cannot start, restart, or
  update a daemon or manage channels. It has no parent daemon socket.

`agent_scv` needs the `scv` executable (searched on `PATH` and in
`~/.cargo/bin`, where `cargo install` puts it) and a provider in its private
home: `scv agents import scv` (or `scv agents login scv`) copies SCV's own
active provider there, as below. Its own delegated agents live under
`$SCV_HOME/agents/scv/agents` and are signed out unless signed in there.
Attaching to an already running daemon instead of starting a child is future
work.

### Agent Client Protocol transport

Claude Code, Codex, Grok Build, and DeepSeek Harness also speak the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP, version 1):
JSON-RPC 2.0 over stdio, one long-running server per conversation. SCV
prefers it when the server is installed, because it relays the agent's own
permission requests and reports progress as it happens:

| Agent | ACP server | Source |
| --- | --- | --- |
| `agent_claude` | `claude-agent-acp` | npm `@agentclientprotocol/claude-agent-acp` (ACP organisation; Zed's `claude-code-acp` is deprecated in its favour) |
| `agent_codex` | `codex-acp` | npm `@agentclientprotocol/codex-acp` (ACP organisation) |
| `agent_grok` | `grok agent stdio` | built in |
| `agent_dsh` | `dsh --profile acp` | built in (0.1.7-rc.1) |

pi has only a community adapter and stays on one process per turn, as does
`agent_scv`, which speaks SCV's own protocol. `[agents.<name>] transport`
chooses: `auto` (the default) uses the ACP server when it resolves (on `PATH`
or in `~/.local/bin`) and the agent's `command` is the built-in one, and
otherwise one CLI process per turn; `acp` requires the server and offers the
agent only when it is installed; `resume` always uses one process per turn. A
custom `command` points SCV at a specific CLI, which the ACP server would not
run, so `auto` keeps it; custom `args` do not matter.

```text
initialize {protocolVersion: 1, no fs or terminal capabilities}
  → session/new {cwd, mcpServers: []} → [set mode for permissions = "full"]
  → [session/set_config_option for model/effort] → session/prompt per call
```

- The schema is the CLI adapters' and always includes `session`: the first
  call returns a handle such as `claude-1`, and passing it sends the next
  prompt to the same ACP session on the same server. A conversation keeps its
  `cwd`, runs one turn at a time, and follows `agent.max_conversations` and
  `agent.conversation_idle_seconds`.
- `model` and `effort` become `session/set_config_option` on the session's
  `model` and `effort`/`reasoning_effort` options, at any turn. A value the
  agent does not offer fails the call with the offered list and keeps the
  conversation.
- `session/update` notifications become `tool.progress`: completed lines of
  the agent's message, each tool call's title (or kind), `… failed` for a
  failed tool call, and the current plan step. Thoughts and tool output never
  become progress, and titles are redacted like the other adapters' lines.
- The agent's `session/request_permission` goes through the calling session's
  approval gate as `agent_<name>` with the summary `[claude-1 acp] <title>
  (<kind>)`. The risk follows SCV's own tools: `read`, `search`, and `think`
  are read-only unless a location looks secret-like, edits, deletes, and moves
  are file-system work, `execute` is a process, `fetch` is network, and
  anything else is delegation. An approval selects the agent's allow-once
  option, a denial its reject-once option (each falls back to the "always"
  variant), and a request with neither is cancelled. With no approval gate
  every request is rejected. SCV offers no client file-system or terminal
  capabilities, so any `fs/*`, `terminal/*`, or other request is answered with
  JSON-RPC "method not found".
- `permissions = "full"` maps onto each agent's own switch: Claude's
  `bypassPermissions` and Codex's `agent-full-access` session modes (selected
  with `session/set_mode` in every new session), `grok agent --always-approve
  stdio`, and DeepSeek Harness's `DSH_PERMISSION_MODE=danger-full-access`. A
  permission request that still arrives is relayed as usual. `codex-acp` takes
  no `-c` overrides, so for Codex `"full"` also sets
  `CODEX_CONFIG={"web_search":"live"}` on the ACP server, its JSON form of
  them, keeping live web search as in resume mode without rewriting the
  private `config.toml`.
- The result has the CLI adapters' shape. `stopReason` `end_turn` completes
  the call; `cancelled` ends it as cancelled; `refusal` ends it as `declined`;
  any other reason completes it with an `(stopped early: …)` note. A JSON-RPC
  error fails it with the agent's redacted message as its `error` and, when
  that reads like a sign-in problem (including `session/new`'s "Authentication
  required"), the `scv agents login <name>` hint.
- A cancelled or timed-out call sends `session/cancel`; an agent that answers
  within 2 seconds keeps the conversation (a timed-out turn is resumable),
  otherwise it is shut down and the conversation forgotten. An agent that
  exits mid-turn fails the call with its stderr tail. The server is recorded
  like any delegation for `scv agents ps`, `kill`, and orphan reaping, and it
  ends like the nested SCV: stdin closed, 2 seconds' grace, then a group kill.
  Like the nested SCV, a server that exits between turns (or is killed there)
  is collected at once and drops out of `scv agents ps`.

The ACP servers read the same private homes and sign-ins as the CLIs, so
`scv agents login` and `import` cover both transports.

### Signing in delegated agents

Each adapter keeps its own sign-in in its private home, separate from the
user's personal login, so token refreshes by one never invalidate the other.
Sign the agents in once on the SCV host:

```sh
scv agents login claude                 # claude auth login
scv agents login codex                  # codex login
scv agents login codex -- --device-auth # extra arguments after --
scv agents import codex                 # or copy your own Codex setup
scv agents login grok -- --device-auth  # grok login, device code for SSH hosts
scv agents import grok                  # or copy your own Grok config and model profiles
scv agents login dsh                    # prompts for a DeepSeek API key
scv agents login pi                     # opens pi: run /login, then /quit
scv agents login pi --openai-compatible # or point pi at any OpenAI-compatible endpoint
scv agents import pi --from-scv-provider # or reuse SCV's own provider
scv agents status                       # every agent's sign-in state
scv agents logout claude
```

They work from any directory. For Claude Code, Codex, and Grok, `login` and
`logout` run the agent's own command with exactly the private home and cleaned
environment that the daemon's `agent_*` tool uses, with the terminal attached
for browser or device-code flows, and the agent CLI itself writes those
credentials under `$SCV_HOME/agents/<name>` (mode `0700`). For Claude Code
and Codex, `status` runs the CLI's own status command but prints only a
summary, such as `signed in (Claude account, max)` or `signed in (API key)`,
because their output names the account email or part of the key; for the
others SCV reads the credential file and reports only whether one is stored,
never its value.

Grok counts as signed in either through a `grok login` sign-in in
`.grok/auth.json` or through an API key in `.grok/config.toml`: the profile of
its `[models] default` (matched by catalog key or model id) holds an `api_key`,
or an `env_key` naming a variable that is set and that SCV does not remove from
delegated agents. Status then reads `signed in (API key in config, model
"<id>")`. `scv agents import grok` copies your own `~/.grok/config.toml` (or
`$GROK_HOME`, or `--from <dir>`) into `.grok/config.toml`, atomically with mode
`0600`. It merges by top-level table: your tables win, and tables only SCV's
copy has, such as the `[marketplace]` state Grok writes there, are kept. The
merged file is validated before anything is written, and it prints the profiles
and default model, never a key. `auth.json` sign-ins are never copied. Re-run it
after changing your own Grok config.

Every import (`codex`, `grok`, `scv`, and `pi --from-scv-provider`) is a copy,
and records a digest of what it copied, never the content, in
`$SCV_HOME/state/imports/<agent>.json`. `scv agents status` and
`scv config show` then compare the source with that digest and print either
`up to date` or that the source has changed since, with the import command to
run; see [imported agent setups](configuration.md#imported-agent-setups).

DeepSeek Harness signs in with an API key only. `scv agents login dsh` reads
it without echo, or from stdin when stdin is not a terminal, and writes it as
`refs.DEEPSEEK_API_KEY` in `.dsh/.credentials.yaml`, DeepSeek Harness's own
credential file, atomically with mode `0600`. `logout` removes that file. A key
is never accepted as an argument. DeepSeek Harness 0.1.7-rc.1 is the tested
version; 0.1.5-rc.2 fails at startup with "cannot create effect on inactive
context" because its sandbox plugin requires a different Cordis framework
version than the one it installs. Signed out, it fails with `MISSING_CREDENTIAL`,
and the `agent_dsh` result names `scv agents login dsh`.

pi's own `/login` covers its built-in providers. For any OpenAI-compatible
endpoint, `scv agents login pi --openai-compatible` asks for the base URL, the
wire API (`responses` or `chat`), and the default model (or takes them from
`--base-url`, `--wire-api`, and `--model`), then reads the key without echo.
`scv agents import pi --from-scv-provider` takes all four from SCV's active
provider instead, reading an `api_key_env` variable at import time because
delegated agents never inherit key variables. Either way SCV writes pi's own
files in `.pi/agent`, each atomically with mode `0600`: provider `scv` in
`models.json` (for the Responses API with
`compat.sessionAffinityFormat = "openai-nosession"`, because pi's default
`session_id` header is rejected by proxies that refuse underscores in header
names), its key in `auth.json`, and `defaultProvider`/`defaultModel` in
`settings.json`, so a bare `agent_pi` call uses it and `model` can name
`scv/<id>`. Other providers and settings in those files are preserved.
`status` shows the default provider, API, endpoint host, and model, and which
providers have stored sign-ins; `logout` removes `auth.json`, the `scv`
provider, and a default that points at it.

`scv agents import scv` gives the nested SCV behind `agent_scv` a copy of
SCV's own active provider: `$SCV_HOME/agents/scv/config.toml` (mode `0600`,
written atomically) gets `[provider] active = "scv"` and a `[providers.scv]`
profile with the same kind, wire API, model, base URL, timeout, and headers,
plus `[web] search = "provider"` when SCV's own config uses hosted search. The
key is resolved at import time from `api_key` or the `api_key_env` variable
and stored in that file, because delegated agents never inherit key
variables; it is never printed. Other settings already in that file are kept,
and an unreadable file is left untouched. `status` shows the provider, model,
and endpoint host; `logout` removes the file.

`scv agents import codex [--from DIR]` instead copies an existing Codex setup,
by default from `$CODEX_HOME` or `~/.codex`. It is for custom providers such as
an OpenAI-compatible relay (`model_providers` with `base_url`, `wire_api`,
`requires_openai_auth`, or `experimental_bearer_token`), which `codex login`
cannot set up:

- `config.toml` is copied whole, so the model, provider, reasoning effort,
  `sandbox_mode`, `approval_policy`, project trust, and features match the
  user's own Codex. SCV reports the model, provider, and policies it copied.
- `auth.json` is copied only when it holds an API key. A ChatGPT sign-in is
  never copied, because its rotating refresh token must stay in one home; use
  `scv agents login codex` for that.
- A provider that reads its key through `env_key` is flagged: SCV removes
  provider key variables such as `OPENAI_API_KEY` from delegated agents, and
  the user service does not load the shell profile.

Both files are validated before either is written. Copies are atomic, mode
`0600`, and never printed. The import is a snapshot rather than a link, so
delegated Codex runs cannot modify the user's own configuration; re-run it
after changing that configuration. Skills and other Codex state are not
copied. The daemon needs no restart: the next delegated call
uses the new sign-in. When a delegated run fails with output that reads like a
missing sign-in, its tool result gains a `hint` naming
`scv agents login <name>`, since the agent's own advice (`/login`) cannot be
followed from a remote chat.

A fake agent script, run through `bash` so tests never execute a freshly
written file, verifies native-agent argument boundaries, model/effort argument
mapping and validation, workspace and `cwd` selection including symlink
escapes, prompt-flag placement, the timeout ceiling, per-adapter environment
removal, and that uninstalled agents are not offered, without requiring these
CLIs in CI. Canned event streams cover each output format, sign-out, oversized
lines, the Codex `-o` file, and bounded replies; real processes cover records,
kill, a timed-out run's `setsid` descendant, and an orphan left by a
SIGKILLed `scv server --stdio`. Fake SCV homes cover the DeepSeek Harness key file, pi's endpoint
files and import, and that no sign-in output contains a key. Shared process-runner tests cover
output limits, timeout, cancellation, and background-descendant cleanup. A
bash stand-in for `scv server --stdio` covers `agent_scv` conversations on one
child, progress, approval relay (approved, denied, and without a gate), cancel
and timeout relay including a child that ignores `turn.cancel`, a child dying
mid-turn, idle and session-end teardown, and the depth limit; an end-to-end
test runs a real parent and nested `scv` against a fake provider. A Python
stand-in ACP server covers a conversation on one session with bounded,
redacted progress and the declared client capabilities, relayed permission
requests (approved, denied, and without a gate) with their risks and options,
refused `fs/*` and `terminal/*` requests, `session/cancel` on cancellation and
timeout including an agent that ignores it, an agent dying mid-turn, sign-in
and protocol-version failures, full-permission modes and model and effort
options, idle teardown, and that the registry prefers an installed ACP server,
falls back to one process per turn, and hides a required one that is missing.

## Tool extension contract

A `Tool` supplies a unique name, description, JSON Schema, declared `ToolRisk`,
approval summary, and asynchronous executor. Stable risk values are
`read_only`, `filesystem`, `process`, `delegate`, and `network`. Registration rejects
duplicate names. The loop and TUI do not contain name-specific execution code.

## `read_skill`

```json
{"name":"release-checks"}
```

At session start, the server discovers at most `skills.max_skills` valid skill
directories and builds a name-to-canonical-path map. `read_skill` accepts only a
name from that map, revalidates containment under its original project or user
skill root, and returns at most `skills.max_skill_bytes` of `SKILL.md`. It does
not accept a path and cannot be used as a general out-of-workspace read.
Tool-enabled sessions also map workspace project skills (`.agents/skills` and
`.claude/skills` of the workspace and its child projects) under
`<project>:<name>`, listed separately with the instruction to delegate to that
project with `agent_*` and `cwd`; see
[Project skills](configuration.md#project-skills). Skill
metadata and content remain untrusted instructions.
