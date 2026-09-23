# Configuration

Status: final design for v0.1

Run `scv config init` on first use to create the user file from `config.example.toml`. Select a profile with `provider.active` or `--provider`.

An SCV instance is identified by its home root. Use `--scv-home PATH` or
`SCV_HOME` to isolate a daemon and its configuration from other SCV processes;
the root owns the config file, socket, skills, ClawBot state, adapter state, and
systemd unit identity. Use `--config PATH` or `SCV_CONFIG` for an additional
explicit file. Both selectors are captured before SCV starts its server or
TUI child. A custom home never merges or falls back to the default `~/.scv`
file.

For concurrently running daemons, each process must use a different
`--scv-home` root. A different `--config` file alone does not create a separate
socket or systemd service identity.

SCV merges configuration in this order, from lowest to highest precedence:

1. built-in defaults;
2. `$SCV_HOME/config.toml` or `~/.scv/config.toml`;
3. `<workspace>/.scv/config.toml`;
4. documented environment variables;
5. command-line flags.

Unknown keys and invalid values are startup errors. Project configuration is
treated as untrusted input: it cannot contain credentials or disable an
interactive approval required by user-level policy. When the workspace's
`.scv/config.toml` is the user configuration itself, as when SCV runs from `~`
with the default home, it is applied once as the user layer and there is no
project layer. `scv agents` reads no project layer at all, since project
configuration cannot set `[agents]`.

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

[providers.custom]
kind = "openai-compatible"
model = "your-model"
base_url = "https://provider.example/v1"
api_key_env = "CUSTOM_PROVIDER_KEY"
headers = { "X-Organization" = "example" }
timeout_seconds = 600

[agent]
max_steps = 128
system_prompt = "You are SCV, a concise and careful coding agent."
max_delegation_depth = 2
max_conversations = 8
conversation_idle_seconds = 86400

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
user_dir = "~/.scv/skills"
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
```

Every table also accepts `prompt_args` (default `[]` except Grok) and
`permissions` (default `"default"`; see [Agent permissions](#agent-permissions)). The agent
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
user, explicit `SCV_CONFIG`, environment, and CLI layers. Project configuration
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

A ClawBot owner turn may run for the ceiling plus five minutes of model time,
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
configuration may only lower them.
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
SCV sends a request that failed transiently: HTTP 429 or 5xx, a failed
connection, an overload, rate-limit, unavailable, or server error, or a stream
that ends early, but not a request that reached `timeout_seconds`. Retries
wait about 1 second, doubling each time with random jitter, or the
`Retry-After` seconds the provider sends; a provider asking for more than 60
seconds is reported instead. A retry happens only while nothing from
that response has streamed, so text is never repeated. Cancelling the turn
interrupts the wait. Set `0` to report the first failure. Project
configuration may lower it but not raise it.

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
cannot be read, are skipped. Tool-free sessions, such as ClawBot senders
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

Project configuration may make policy stricter but not weaker than user
configuration. A command-line flag may weaken policy because it is an explicit
choice for that invocation.

## Environment variables

SCV v0.1 reads:

- `SCV_HOME` for the user configuration, skills, daemon socket, and ClawBot state
  root (default `~/.scv`);
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
scv --scv-home ~/.scv/work --model gpt-4.1-mini start --workspace /repo
scv --scv-home ~/.scv/review --model o4-mini start --workspace /repo
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
[Signing in delegated agents](tools.md#signing-in-delegated-agents).

Secrets are never included in diagnostics, protocol events, approval summaries,
or tool results.

## Daemon and component settings

`scv-client` resolves the default socket as `$SCV_HOME/server.sock`, normally
`~/.scv/server.sock`. The TUI and daemon control commands use this same path.
`scv status` queries the running server; `scv reload` immediately reconciles
saved accounts and component settings without restarting unrelated sessions.
The daemon also reconciles on startup and every two seconds.

Delegated agent runs are recorded in `$SCV_HOME/run/delegations/<handle>.json`
(mode `0600`) while they run. The daemon stops orphans, whose owning SCV
process has died, at startup and every 60 seconds; `scv agents ps` and
`scv agents kill` list and stop runs. See
[Tracking and cleanup](tools.md#tracking-and-cleanup).

ClawBot credentials live in `clawbot/accounts/<account>.json` and durable
delivery state in `clawbot/state/<account>.json` under the same root. Per-account
settings are separate from project TOML, at
`$SCV_HOME/clawbot/settings/<account>.json`:

```json
{"enabled":true,"workspace":"/absolute/path/to/workspace","remote_tools":"none"}
```

Missing settings default to `enabled: true` and `remote_tools: "none"`; an
omitted or null `workspace` uses the daemon workspace. An explicit workspace must be an existing absolute
directory. Saved accounts autostart when enabled, but QR login is always
explicit. Logging in again preserves a saved disabled setting.

Account settings reject unknown keys. The daemon reads credentials and settings
together under the account transaction lock. Login can rotate a token for the
same known bot/user identity and normalized API origin while preserving delivery
state. A different identity/origin requires explicit logout first; legacy
credentials without both IDs are conservatively bound to their token and also
require logout before replacement with an identified account.
A busy transaction during the snapshot defers reconciliation; the current
instance keeps running until a later pass can read the account.

`scv clawbot run --account NAME --workspace PATH` persists enablement and the
resolved workspace through the live daemon, then returns. Adding
`--remote-tools owner` grants the account's authenticated owner full,
auto-approved tools from WeChat; `--remote-tools none` revokes it. The value is
saved as `remote_tools` (`"none"` by default) in the account settings; see the
[security model](security.md#supervised-remote-bridge) before enabling it. `scv clawbot stop
--account NAME` persists `enabled: false` and joins the instance while retaining
credentials. Credential or settings changes join the old instance before a
replacement starts. `scv clawbot logout --account NAME` requires a live daemon
and removes credentials, delivery state, and settings only after joining.

For an offline opt-out, create or edit the account settings to contain
`{"enabled":false}` before starting the daemon. Keep ClawBot directories mode
`0700` and files mode `0600`; account, settings, and state files must be private
regular files. Invalid or inaccessible settings fail that account closed.
Project configuration cannot select accounts, component workspaces, or remote
authority. See [ClawBot](clawbot.md) for the lifecycle and status contract.

`scv update` installs the published binary and restarts an active systemd user
daemon. A foreground daemon requires an explicit restart; its in-memory code
does not change when the executable is replaced.
