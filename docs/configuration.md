# Configuration

Status: final design

Run `scv config init` on first use to create the user file from `config.example.toml`. Select a profile with `provider.active` or `--provider`.

An SCV instance is identified by its home root. Use `--scv-home PATH` or
`SCV_HOME` to isolate a daemon and its configuration from other SCV processes;
the root owns the config file, credentials, agent homes, skills, runtime state,
and systemd unit identity (see [Instance layout](#instance-layout)). Use
`--config PATH` or `SCV_CONFIG` for an additional
explicit file. Both selectors are captured once, when an SCV process starts,
and passed down explicitly; the home is resolved to its canonical path when it
exists. The environment only hands the selection on to child processes (the
systemd unit, the planned-restart watchdog, and delegated agents). A custom
home never merges or falls back to the default `~/.scv` file.

For concurrently running daemons, each process must use a different
`--scv-home` root. A different `--config` file alone does not create a separate
socket or systemd service identity.

SCV merges configuration in this order, from lowest to highest precedence:

1. built-in defaults;
2. `$SCV_HOME/config.toml` or `~/.scv/config.toml`;
3. `<workspace>/.scv/config.toml`;
4. documented environment variables;
5. command-line flags.

Unknown keys and invalid values are startup errors, reported with the file
and line but never the line's text, which may hold a key. Project configuration is
treated as untrusted input: it cannot contain credentials or disable an
interactive approval required by user-level policy. When the workspace's
`.scv/config.toml` is the user configuration itself, as when SCV runs from `~`
with the default home, it is applied once as the user layer and there is no
project layer. `scv agents` reads no project layer at all, since project
configuration cannot set `[agents]`.

## Instance layout

An instance keeps everything under its home, `SCV_HOME` (default `~/.scv`), in
five places:

```text
~/.scv/
├── config.toml        settings you edit: provider and key, limits, tools,
│                      [agents.<name>], [channels.<channel>.<account>], and
│                      [notify]
├── credentials/       sign-ins SCV writes itself (0700)
│   ├── wechat/<account>.json
│   └── feishu/<account>.json
├── agents/<name>/     private homes of the delegated agents (claude, codex,
│                      grok, dsh, pi, scv), with their own sign-ins
├── skills/            your SCV skills
└── state/             runtime data SCV writes and reads back (0700)
    ├── server.sock, server.lock
    ├── config.lock
    ├── delegations/<handle>.json
    ├── conversations/
    ├── imports/<agent>.json
    ├── daemon.json    the running daemon, removed on a clean stop
    ├── update.json    a planned restart, until its outcome is announced
    ├── last-owner.json  the chat the owner last wrote from
    └── channels/<channel>/<account>.json, .lock, .transaction
```

The rules behind it:

- **One file to edit.** Every setting a person changes is in `config.toml`,
  including chat accounts. The only other writers are `scv channels run`,
  `stop`, and `logout`, which change just their own
  `[channels.<channel>.<account>]` table and keep the rest of the file,
  comments included.
- **Credentials apart from settings.** Channel logins, which SCV writes and a
  token rotation rewrites, live in `credentials/`, so a machine write never
  replaces a file a person edits.
- **The provider key stays in `config.toml`.** A person enters it and SCV never
  writes it, so it belongs with the settings rather than with machine-written
  credentials; one private (0600) file is simpler to check than two. Use
  `api_key_env` to keep it in the environment instead. `scv config show`
  hides it, so the overview is safe to share.
- **Agents keep their own sign-ins.** Delegated CLIs run with their agent home
  as `HOME`, and store their logins there in their own formats (such as
  `agents/codex/auth.json`). `scv agents login|logout` manage them, and
  `scv config show` reports each file without reading it.
- **State is not configuration.** `state/` holds only what SCV writes and
  reads back: the daemon socket, delegated-run records, channel delivery
  checkpoints, and locks. Nothing there is meant to be edited.

Outside the home are the systemd user unit
(`~/.config/systemd/user/scv.service`, or a hashed name for a custom home) and,
per project, `<workspace>/.scv/config.toml` and `.scv/skills/`.

SCV reads nothing else in the home. `scv config show` lists other entries under
"Not used by SCV", and the daemon logs a warning at startup for paths of the
layout before `0.2.0` (`adapters/`, `channels/`, `run/`, `server.sock`), which
no release reads; see [release notes](release.md#upgrading-to-020).

### Seeing it all

`scv config show` prints, for a session started in the current directory:

- every path above, with its mode, whether a daemon listens on the socket,
  and the service unit;
- each setting that is not a default, as `key = value [origin]`, where the
  origin is `config.toml`, `project .scv/config.toml`, `SCV_CONFIG`,
  `env SCV_MODEL` (and the other variables below), or a `--flag`; `--all`
  adds the defaults. Keys named `api_key`, `*_api_key`, `*secret*`,
  `*password*`, and every provider header show `<hidden>`;
- the provider in effect and where its key comes from;
- each channel account's settings and credential file, or why they are
  invalid;
- each agent's credential files and, for an imported setup, whether its source
  has changed since;
- entries of the home that SCV does not read.

It reads files only and takes no lock a running daemon needs. `scv config path`
prints the path of `config.toml`, for example `$EDITOR "$(scv config path)"`.

### Imported agent setups

Delegated agents never read the user's own `~/.codex` or `~/.grok`: SCV runs
them in their agent homes so they cannot reuse or change the user's personal
setup. `scv agents import codex|grok` copies that setup in, and `scv agents
import scv|pi` copies SCV's own provider. Each import records a digest of what
it copied in `state/imports/<agent>.json`; `scv agents status` and `scv config
show` compare it with the source now and say when the copy is stale, so a
changed Grok profile or rotated key shows up instead of going unnoticed. Run
the import again to refresh it.

## Schema

```toml
[provider]
active = "openai"

[providers.openai]
kind = "openai-compatible"
model = "gpt-4.1-mini"
base_url = "https://api.openai.com/v1"
api_key = "sk-your-key"
api_key_env = "OPENAI_API_KEY" # optional fallback
timeout_seconds = 600
image_input = true # show attached images to the model

[providers.custom]
kind = "openai-compatible"
model = "your-model"
base_url = "https://provider.example/v1"
api_key_env = "CUSTOM_PROVIDER_KEY"
headers = { "X-Organization" = "example" }
timeout_seconds = 600

[agent]
max_steps = 128
system_prompt = "You are SCV, a concise and careful agent."
max_delegation_depth = 2
max_conversations = 8
conversation_idle_seconds = 86400
max_background = 4
prefer = []

[session]
max_history_bytes = 16777216
max_messages = 10000

[context]
max_tokens = 128000
reserve_output_tokens = 8192
safety_margin_tokens = 2048
bytes_per_token = 3
summary_max_chars = 6000

[tools]
approval_policy = "on-risk"
command_timeout_seconds = 600
agent_timeout_seconds = 3600
max_timeout_seconds = 14400
output_limit_bytes = 65536
max_read_bytes = 262144
max_write_bytes = 1048576

[protocol]
max_client_frame_bytes = 1048576
max_server_frame_bytes = 8388608

[tui]
max_transcript_bytes = 8388608
max_transcript_items = 10000
max_prompt_history_bytes = 1048576
max_prompt_history_items = 200

[provider_limits]
max_sse_event_bytes = 1048576
max_response_bytes = 4194304
max_assistant_bytes = 1048576
max_tool_calls = 32
max_tool_arguments_bytes = 262144
max_retries = 2

[skills]
user_dir = "~/.scv/skills"  # left at this default: the instance's own skills/
project_dir = ".scv/skills"
scan_projects = true
max_skills = 128
max_skill_bytes = 262144

[web]
enabled = true
fetch_max_bytes = 2097152
fetch_timeout_seconds = 30
max_redirects = 5
auto_approve_domains = ["docs.rs", "crates.io", "doc.rust-lang.org", "docs.python.org", "pypi.org", "developer.mozilla.org"]
allow_private_addresses = false
search = "off" # "provider", "searxng", or "brave"
# searxng_url = "https://searx.example"
brave_url = "https://api.search.brave.com/res/v1/web/search"
# brave_api_key = "your-brave-key"
brave_api_key_env = "BRAVE_SEARCH_API_KEY"
max_search_results = 8

[agents.claude]
command = "claude"
args = ["-p"]
permissions = "default"
model_args = ["--model", "{model}"]
effort_args = ["--effort", "{effort}"]
# use_for = "coding"
# model = "opus-5.5"
# effort = "xhigh"

[agents.codex]
command = "codex"
args = ["exec"]
model_args = ["-m", "{model}"]
effort_args = ["-c", "model_reasoning_effort=\"{effort}\""]

[agents.dsh]
command = "dsh"
args = ["--profile", "headless"]
prompt_args = []
model_args = []
effort_args = []

[agents.grok]
command = "grok"
args = []
prompt_args = ["-p"]
model_args = ["-m", "{model}"]
effort_args = ["--reasoning-effort", "{effort}"]

[agents.pi]
command = "pi"
args = ["-p"]
model_args = ["--model", "{model}"]
effort_args = ["--thinking", "{effort}"]

[agents.scv]
command = "scv"
args = ["server", "--stdio"]

[channels.wechat.default]
enabled = true
workspace = "/absolute/path/to/workspace" # optional; the daemon's workspace otherwise
remote_tools = "none"                     # or "owner"
# senders = "anyone"                      # omitted, only the owner is answered
```

Every table also accepts `prompt_args` (default `[]` except Grok),
`permissions` (default `"default"`; see [Agent permissions](#agent-permissions)),
`transport` (default `"auto"`; see [Agent transport](#agent-transport)), and
`use_for`, an optional one-line note (at most 500 bytes) on when to choose that
agent, added to its tool description, and optional `model` and `effort` defaults
for that work (see [Choosing an agent](tools.md#choosing-an-agent)). The agent
names are fixed; an unknown `[agents.<name>]` is a startup error that lists the
known ones.

`agents.*.args` is an argument vector, not a shell string. SCV appends the
delegated prompt as the final argument and runs the child in the session
workspace, or in the directory inside it that the call names with `cwd`. When a call selects a `model` or `effort`, SCV substitutes the value
for `{model}` or `{effort}` in `model_args` or `effort_args` and inserts those
arguments between the fixed arguments and the prompt, followed by
`prompt_args` for CLIs whose prompt is a flag value. An empty template means
the adapter offers no such selection; a non-empty one must contain its
placeholder. Overriding only `command` or `args` keeps the built-in templates. The built-in adapters are enabled when their executable is
available; attempting to call a missing adapter returns a clear tool error.

`provider`, `agents.*`, and `skills.user_dir` are accepted only from built-in,
user, explicit `SCV_CONFIG`, environment, and CLI layers. `[channels]` is
accepted only in the instance's own `config.toml`, where SCV's channel store
reads it. Project configuration
cannot change a model endpoint, credential-variable name, user skill root,
executable, or fixed arguments. A native-agent approval shows the resolved
absolute executable, the complete fixed argument vector, the workspace, and
the bounded prompt argument before launch.

Project configuration may lower context, size, output, and timeout limits; make
approval policy stricter; set `skills.project_dir` within the workspace; turn
`skills.scan_projects`, `web.enabled`, or `web.search` off; and append project
instructions. Attempts to weaken a limit or set a user-only key
are startup errors rather than ignored fields.

### Tool timeouts

A tool call may choose its own `timeout_seconds`, so the model can grant long
work more time when it starts it, for example when the owner asks from WeChat
to land a change. Three keys bound this:

- `command_timeout_seconds` (default 600) applies to `bash` calls that do not
  choose a timeout, enough for a cold release build or strict Clippy;
- `agent_timeout_seconds` (default 3600) applies to `agent_*` calls that do not
  choose one, enough for a delegated feature through CI, landing, release, and
  deploy (such runs have taken 27 to 48 minutes);
- `max_timeout_seconds` (default 14400, four hours) is the ceiling any single call may
  request, and is advertised in each tool's schema. A request above it is
  refused with an error naming the ceiling rather than silently shortened.

Both defaults must not exceed the ceiling, and the ceiling is at most 86400
(one day). To allow longer delegated jobs, raise the ceiling in the user file:

```toml
[tools]
max_timeout_seconds = 28800
```

A channel owner turn (WeChat or Feishu) may run for the ceiling plus five minutes of model time,
and never less than 30 minutes, so four hours and five minutes by default; the
component reads the ceiling from the workspace configuration each time it
starts. `agent.max_steps` (default 128) bounds model/tool rounds per turn.
`agent.max_delegation_depth` (default 2) offers the `agent_*` tools only while
the session's own delegation depth is below it: the top SCV (depth 0) and an
SCV started by one of its agents (depth 1) may delegate, one more level down
may not, and `0` turns delegation off. It lives under `[agent]` rather than
`[agents]`, which holds one table per adapter and which project configuration
cannot set. `agent.max_conversations` (default 8) and
`agent.conversation_idle_seconds` (default 86400) bound how many delegated
conversations a session remembers and for how long; see
[Conversations](tools.md#conversations). Both must be positive, and project
configuration may only lower them. `agent.max_background` (default 4, at most
16) bounds how many background agent jobs (`background: true`) a session runs
at once; `0` turns background delegation off, and project configuration may
only lower it. The default leaves room for a main agent that hands most work to
background jobs; see [Background jobs](tools.md#background-jobs).
`agent.prefer` (default empty) lists the agents the user prefers, in order,
such as `["codex", "claude"]`; the system prompt names the ones a session
offers. Unknown names fail validation, and project configuration cannot set
it. `[agents.<name>] use_for`, `model`, and `effort` add per-agent defaults
for a kind of work (see [Choosing an agent](tools.md#choosing-an-agent)).
`providers.*.timeout_seconds` (default 600) bounds each whole model request,
including its streamed response, not just idle time, so it must cover the
longest single response. Project configuration may lower all of these but not
raise them.

### Provider errors and retries

A provider error always fails the turn with `provider_error` and the
provider's own message, redacted of the credential, flattened to one line, and
cut to 300 characters. That covers an HTTP error status, an `error` or
`response.failed` stream event, a `response.incomplete` event, a plain JSON
error body sent in place of the stream, and a stream that ends before
`response.completed` or `[DONE]`. None of them completes a turn with empty
text.

`provider_limits.max_retries` (default 2, at most 10) is how many more times
SCV sends a request that failed transiently: HTTP 429 or 5xx, any failure to
send the request before a response status arrives (a refused connection, a
reset, or a pooled keep-alive connection the server had already closed), an
overload, rate-limit, unavailable, or server error, or a stream that ends
early, but not a request that reached `timeout_seconds`. Retries
wait about 1 second, doubling each time with random jitter, or the
`Retry-After` seconds the provider sends; a provider asking for more than 60
seconds is reported instead. A retry happens only while nothing from
that response has streamed, so text is never repeated. Cancelling the turn
interrupts the wait. Set `0` to report the first failure. Project
configuration may lower it but not raise it.

`image_input` (default `true`) sends images a turn attaches, such as photos
chat users send, to the model as Responses `input_image` items. Set it to
`false` for a model without vision; the model then sees each image named in
the prompt instead, with its path when the session has tools. When the
provider rejects a request with images and its error mentions images, SCV
describes images for the rest of that session and sends the request again.

SCV reuses provider connections but retires one after 30 idle seconds.
Proxies in front of providers commonly close idle keep-alive connections after
a minute or so, and a request written to a connection that is already closed
fails before any response. Retiring connections sooner makes that rare, and
the retry above covers the rest.

### Agent permissions

`[agents.<name>] permissions` sets how much a delegated CLI may do without its
own prompts:

- `"default"` adds nothing; the CLI's own configuration decides. This is the
  built-in value, so no install bypasses an agent's safeguards unless its user
  asks.
- `"full"` adds the CLI's own full-autonomy switches after `args`, so the agent
  can research, edit files, run commands, and use the network without asking:

| Agent | `"full"` adds | Web search |
| --- | --- | --- |
| `claude` | `--permission-mode bypassPermissions` | WebSearch and WebFetch, unprompted |
| `codex` | `--dangerously-bypass-approvals-and-sandbox -c web_search="live"` | native `web_search` tool (if the provider supports it) |
| `grok` | `--always-approve` | on by default |
| `dsh` | `DSH_PERMISSION_MODE=danger-full-access` (sandbox off, approvals `never`) | its own web tool |
| `pi` | nothing: pi has no approval prompts or sandbox | none built in |

```toml
[agents.claude]
permissions = "full"

[agents.codex]
args = ["exec", "--skip-git-repo-check"]
permissions = "full"
```

Every approval summary for such an agent says `FULL PERMISSIONS`. Only the
user layers can set `[agents]`; see [security](security.md).

Over the Agent Client Protocol, `"full"` selects the agent's own equivalent
instead:

| Agent | `"full"` over ACP | Web search |
| --- | --- | --- |
| `claude` | `bypassPermissions` session mode | WebSearch and WebFetch, unprompted |
| `codex` | `agent-full-access` session mode and `CODEX_CONFIG={"web_search":"live"}` | native `web_search` tool (if the provider supports it) |
| `grok` | `grok agent --always-approve stdio` | on by default |
| `dsh` | the same `DSH_PERMISSION_MODE` variable | its own web tool |

`codex-acp` takes no `-c` overrides; `CODEX_CONFIG` is its JSON form of them,
merged into every session, so SCV sets it on the ACP server rather than
rewriting the imported `$SCV_HOME/agents/codex/config.toml`. An inherited
`CODEX_CONFIG` is removed from every delegated agent's environment.

### Agent transport

`[agents.<name>] transport` chooses how SCV talks to Claude Code, Codex, Grok
Build, or DeepSeek Harness, which also speak the Agent Client Protocol (ACP):

- `"auto"` (default): the agent's ACP server when it resolves and `command` is
  the built-in one, otherwise one CLI process per turn. A custom `command`
  keeps one process per turn because the ACP server would not run it.
- `"acp"`: only the ACP server; the agent is not offered while the server is
  missing. A startup error for pi and `scv`, which have none.
- `"resume"`: always one CLI process per turn, continued through the CLI's own
  resume.

The ACP servers are `claude-agent-acp` (npm
`@agentclientprotocol/claude-agent-acp`), `codex-acp` (npm
`@agentclientprotocol/codex-acp`), `grok agent stdio`, and `dsh --profile acp`;
see [the ACP transport](tools.md#agent-client-protocol-transport).

```toml
[agents.codex]
transport = "resume"
```

### Project skills

With `skills.scan_projects` (default `true`), tool-enabled sessions also list
the agent skills of the workspace and of each immediate, non-hidden child
directory: `SKILL.md` files under `.agents/skills/<name>/` (Codex) and
`.claude/skills/<name>/` (Claude Code). A child project's skill is listed as
`<project>:<name>`, a workspace-root skill as `<name>`, and names from
`skills.project_dir` or `skills.user_dir` win collisions. The listing tells the
model to delegate with `agent_*` and `cwd` set to the project: the nested agent
then loads that project's instructions and skills natively, so a repository
adds skills without any SCV registration. `read_skill` can load a listed skill
for reference. At most 256 child projects and `skills.max_skills` skills in
total are considered; entries that resolve outside the workspace, or that
cannot be read, are skipped. Tool-free sessions, such as WeChat senders
without remote tools, never list project skills.

### Web tools

With `web.enabled` (default `true`), tool-enabled sessions get `web_fetch`,
and web search when `web.search` names a source:

- `off` (default): no search.
- `provider`: offer the provider endpoint's hosted Responses `web_search` tool
  with every request. The provider runs the searches itself and returns a
  cited answer. Use it only with an endpoint that supports it (OpenAI does, as
  do relays that pass the tool through); others reject the request.
- `searxng`: a `web_search` tool that queries `web.searxng_url`, a SearXNG
  instance with the JSON format enabled.
- `brave`: a `web_search` tool that queries the Brave Search API with the key
  from `web.brave_api_key` or the variable named by `web.brave_api_key_env`.
  The user service does not load the shell profile, so put the key in the
  private user file or the unit's environment. Without a key the tool is left
  out and the daemon logs a warning.

`web_fetch` reads HTTPS pages on `web.auto_approve_domains` without approval
and asks before any other URL; an entry is a host name, or `*.example.com` for
its subdomains. It downloads at most `web.fetch_max_bytes`, follows at most
`web.max_redirects` redirects, and gives up after `web.fetch_timeout_seconds`,
which must not exceed `tools.max_timeout_seconds`. `web.allow_private_addresses
= true` lets it reach loopback and private networks, such as an intranet
documentation server. See [web tools](tools.md#web_fetch) and
[security](security.md#web-access).

`web.auto_approve_domains`, `web.allow_private_addresses`, and the search
endpoints and keys are user-only. Project configuration may only disable web
access, turn search off, or lower the limits.

Cross-field validation requires every specific content limit plus serialization
overhead to fit its protocol frame limit, tool arguments to fit provider
responses, and all count/byte limits to be positive. SCV fails startup with the
conflicting key names instead of silently clamping values.

Approval policies are:

- `on-risk` (default): approve ordinary `read` calls, `web_search`, and
  `web_fetch` of auto-approved hosts; prompt for secret-like reads, `write`,
  `bash`, other `web_fetch` URLs, and every `agent_*` tool;
- `always`: prompt for every tool;
- `never`: deny tools whose declared risk is not read-only.

The daemon uses `on-risk` when no policy is supplied. Set the policy explicitly
for a managed daemon with a global flag, for example
`scv --approval-policy always start --workspace /path/to/workspace`.

Starting or restarting the daemon also checks whether the current user has
usable sudo authorization. An interactive start without it asks whether to
continue; `--allow-sudo` invokes sudo authentication first. That option only
validates the user's existing operating-system policy; SCV never edits sudoers
or grants privileges. A non-interactive start without verified sudo
authorization fails so unattended jobs do not silently run with reduced
capability.

The updater reads an optional Cargo registry index from the user configuration:

```toml
[update]
index_url = "https://mirrors.ustc.edu.cn/crates.io-index"
```

`SCV_CARGO_INDEX_URL` and `scv update --index-url URL` override the configured
value in that order. Project configuration cannot select an update registry.
SCV passes the URL to Cargo, which remains responsible for registry
authentication and downloads.

The daemon sends some notices nobody asked for: an update started from a
terminal or the TUI, a rollback of a failed update, a restart after the daemon
stopped unexpectedly, and an enabled chat account disconnected for ten
minutes. `[notify].owner` lists the accounts that may carry them, as
`<channel>:<account>`:

```toml
[notify]
owner = ["feishu:default", "wechat:default"]
```

A notice goes to the account owner's direct chat on the first listed account
that is connected, and only there; an account still connecting keeps its place
for two minutes. A notice about an account never goes through that account.
Without a list, notices go to the chat the owner last wrote from, and with no
such chat only to the log. Updates asked for from a chat are answered in that
chat, falling back to this list when it does not connect in time. A question
to the owner (`scv confirm`) asked by work that did not start in a chat goes
to the same owner chat, on the first listed account connected right then (see
[channels](channels.md#questions-to-the-owner)). Project configuration cannot
set `[notify]`.

Project configuration may make policy stricter but not weaker than user
configuration. A command-line flag may weaken policy because it is an explicit
choice for that invocation.

## Environment variables

SCV reads:

- `SCV_HOME` for the instance home (default `~/.scv`); see
  [Instance layout](#instance-layout);
- `SCV_CONFIG` for one additional explicit configuration file;
- `SCV_MODEL`;
- `SCV_BASE_URL`;
- `SCV_API_KEY_ENV` (the name of the credential variable, not its value);
- `SCV_CARGO_INDEX_URL` for the update registry;
- the credential variable named by `provider.api_key_env`;
- `RUST_LOG` for diagnostics. The daemon (`scv run`) and `scv server` log to
  stderr, default level `warn`; the generated user service sets `info` and
  its log is read with `journalctl --user -u scv.service`.

For example, two independent daemons can use different models without sharing
their sockets or settings:

```bash
scv --scv-home ~/.scv-work --model gpt-4.1-mini start --workspace /repo
scv --scv-home ~/.scv-review --model o4-mini start --workspace /repo
```

The default `scv.service` is retained for the default home. Custom homes use a
stable hashed service name and persist `SCV_HOME`/`SCV_CONFIG` in that unit.
`scv update` installs the shared binary but restarts only the selected instance.

Native agent adapters run with an instance-private `HOME`, `SCV_HOME`, XDG
configuration/data/state directories, and the agent's own state variable
pointing inside it (`CODEX_HOME`, `GROK_HOME`, `DSH_HOME`, or
`PI_CODING_AGENT_DIR`). They do not inherit `SCV_CONFIG`, `SCV_MODEL`,
`SCV_PROVIDER`, `SCV_BASE_URL`, `SCV_API_KEY_ENV`, any `*_API_KEY` variable, or
any adapter's credential, endpoint, or state variables (such as
`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, `GROK_*`, `DSH_*`, or `PI_*`),
and therefore cannot silently reuse or alter the user's normal agent
configuration. Sign the agents in for SCV with `scv agents login <name>`, copy
your Codex provider setup with `scv agents import codex` or your Grok model
profiles with `scv agents import grok`, or point pi at SCV's own provider with
`scv agents import pi --from-scv-provider`; see
[Signing in delegated agents](tools.md#signing-in-delegated-agents). The nested
SCV behind `agent_scv` gets `SCV_HOME=$SCV_HOME/agents/scv`, so its own
`config.toml`, skills, and delegations live there; give it SCV's own provider
with `scv agents import scv` (see [Nested SCV](tools.md#nested-scv-agent_scv)).

Secrets are never included in diagnostics, protocol events, approval summaries,
or tool results.

## Daemon and component settings

The daemon listens on the instance's socket, `$SCV_HOME/state/server.sock`
(normally `~/.scv/state/server.sock`), which `scv_client::Layout` places;
the TUI and daemon control commands take the same path from the instance they
resolved at startup.
`scv status` queries the running server; `scv reload` immediately reconciles
saved accounts and component settings without restarting unrelated sessions.
The daemon also reconciles on startup and every two seconds.

Delegated agent runs are recorded in `$SCV_HOME/state/delegations/<handle>.json`
(mode `0600`) while they run. The daemon stops orphans, whose owning SCV
process has died, at startup and every 60 seconds; `scv agents ps` and
`scv agents kill` list and stop runs. See
[Tracking and cleanup](tools.md#tracking-and-cleanup).

Channel credentials live in `credentials/<channel>/<account>.json`
(`<channel>` is `wechat` or `feishu`) and durable delivery state in
`state/channels/<channel>/<account>.json` under the same root. Each account's
settings are a table in the instance's `config.toml`:

```toml
[channels.wechat.default]
enabled = true
workspace = "/absolute/path/to/workspace"
remote_tools = "none"
# senders = "anyone"
```

Account tables may also limit the files senders send:

```toml
[channels.wechat.default.media]
owner_max_mib = 50
others_image_max_mib = 5
keep_days = 7
```

`owner_max_mib` is the largest file downloaded from the account owner (0 turns
downloads off for everyone), `others_image_max_mib` the largest image from any
other sender, on an account that answers anyone, whose other files are never
downloaded (0 turns their images off too), and `keep_days` how long received
files and copies of sent files stay in
`$SCV_HOME/state/media`. SCV leaves the defaults above out of the file. See
[channel media](channels.md#media).

A missing table or key defaults to `enabled = true`, `remote_tools =
"none"`, and `senders = "owner"`; an omitted `workspace` uses the daemon
workspace. `senders` says whose messages the account answers: `"owner"`, only
the account's authenticated owner, dropping everyone else's messages unanswered
(with no owner on record, nobody), or `"anyone"`, every sender, tool-free
unless the owner holds remote tools. SCV writes the key only as `"anyone"`, so
an owner-only table stays readable by releases before `0.3.0`, which reject
the key. An explicit workspace must be an existing absolute
directory. Saved accounts autostart when enabled, but QR login is always
explicit. Logging in again preserves a saved disabled setting. A Feishu
account's credentials hold the app ID and secret, its brand (`feishu` or
`lark`), and the owner's `open_id`; the secret is never printed.

Account settings reject unknown keys. The daemon reads credentials and settings
together under the account transaction lock, on every reconciliation, so an
edit to a `[channels]` table takes effect within two seconds without a
restart. SCV's own edits hold `state/config.lock`, rewrite the file atomically
with mode `0600` (through a symlinked `config.toml` to its target), and keep
every other table and comment. Login can rotate a token for the
same known bot/user identity and normalized API origin while preserving delivery
state. A different identity/origin requires explicit logout first; legacy
credentials without both IDs are conservatively bound to their token and also
require logout before replacement with an identified account.
A busy transaction during the snapshot defers reconciliation; the current
instance keeps running until a later pass can read the account.

`scv channels run <channel> --account NAME --workspace PATH` persists enablement and the
resolved workspace through the live daemon, then returns. Adding
`--remote-tools owner` grants the account's authenticated owner full,
auto-approved tools from that chat account; `--remote-tools none` revokes it. The value is
saved as `remote_tools` (`"none"` by default) in the account settings; see the
[security model](security.md#supervised-remote-bridge) before enabling it.
`--senders anyone` makes the account answer every sender, and `--senders owner`
only its owner again; the value is saved as `senders`. `scv channels stop
<channel> --account NAME` persists `enabled: false` and joins the instance while
retaining credentials. Credential or settings changes join the old instance
before a replacement starts. `scv channels logout <channel> --account NAME` requires a live daemon
and removes credentials, delivery state, and the account's `[channels]` table
only after joining.

For an offline opt-out, set `enabled = false` in the account's table before
starting the daemon. Keep channel directories mode
`0700` and files mode `0600`; credential and state files must be private
regular files. Invalid or inaccessible settings fail that account closed.
Project configuration cannot select accounts, component workspaces, or remote
authority. See [channels](channels.md) for the lifecycle and status contract.

`scv update` installs the published binary and restarts an active systemd user
daemon. A foreground daemon requires an explicit restart; its in-memory code
does not change when the executable is replaced.

`scv restart --when-idle [--version V] [--commit C] [--max-wait SECONDS]` asks
a daemon running as its systemd user unit to restart into the binary now
installed at its own path once the delegated agent running the command (if
any) has finished and its report is stored, and no owner message is being
answered; after `--max-wait` (default 600 seconds) it restarts anyway. A
watchdog outside the daemon checks the new release and rolls back to the
previous binary (kept as `<binary>.prev`) when it fails to start or reconnect
its accounts, unless the releases differ in config layout. It is the one daemon
lifecycle command a delegated agent may run; it exits 3 when no daemon answers
or the daemon predates it. See
[architecture](architecture.md#planned-restarts).
