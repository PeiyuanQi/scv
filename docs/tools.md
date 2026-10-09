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
in `session.start`: an account owner's WeChat, Feishu, or Slack session. It sends a
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
  homes, and state), except the media directory holding what chat users sent
  and this chat's own kept files (see `chat_keep`);
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

## Chat history (`chat_history`)

```json
{"action":"search","query":"blue door"}
{"action":"episodes","before":"2026-09-01","limit":20}
{"action":"read","episode":"2026/2026-09-21_2026-09-27/2026-09-26T14-04-05","offset":0}
```

Offered, with `chat_keep`, only in a tool-enabled session whose client named
its chat log in `session.start`: an account owner's direct chat on WeChat,
Feishu, or Slack, or a Feishu or Slack thread in it (see [Chat history](channels.md#chat-history)).
It reads that one conversation's log and nothing else (a thread's session
reads the thread, the direct chat's session the direct chat), so it is
read-only and never asks for approval.

- `search` finds messages containing every word of `query`, ignoring case, in
  their text, what they quoted, their files' names, or a voice message's
  transcript, newest first: at most
  `limit` (default 10, at most 100), each with its episode, its `index` in
  the episode, its local time, who wrote it (`owner`, `scv`, or `system`),
  and an excerpt. It reads at most 64 MiB of the log per call; `more` says
  it stopped before the end.
- `episodes` lists episodes newest first (default 20): ID, local times of
  the first and last message, message count, the start of the owner's first
  message, and whether `/new` ended it. `before` keeps those started before a
  date and `after` those started on or after one, both as `YYYY-MM-DD`.
- `read` shows an episode's messages from `offset` (default 30 at a time),
  each up to 8,000 characters, with `total`. A file shows its path while it
  is in the chat media, its new path with `kept` once kept, `gone` after its
  retention, `not_saved` for one the user sent that was never saved, or
  `sent` for a file SCV sent, and a voice message's `transcript`.

An episode ID is `<year>/<week>/<start>` as the log names it; any other shape
is refused. Results are JSON bounded by `tools.output_limit_bytes`, dropping
list entries (with `more` set) until they fit.

## Keeping chat files (`chat_keep`)

```json
{"path":"/home/u/.scv/state/media/wechat/default/3fa9c2d17e5b8a04/a1b2c3-cat.jpg"}
```

Keeps a file the user sent in this chat for good: it moves from the chat
media directory, whose files expire after `keep_days`, to the conversation's
kept files, `<archive>/<channel>/<account>/<conversation>/files/`, which
nothing removes. `path` is the absolute path shown with the file in the turn
or in `chat_history`. Only a regular file directly in this conversation's
media directory is accepted, not a symlink or a file of another chat, and a
kept file never replaces another of the same name; keeping a kept file again
reports where it is. The file is linked under its kept name, which never
replaces an existing file, before the original goes; across disks it is first
copied to a private temporary file beside the kept files, so a failed copy
leaves nothing under the kept name. The call has filesystem risk, which a chat
owner's session approves.

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

## Delegated agents (`agent`)

One tool, `agent`, hands work to another agent, and its `agent` argument names
which one. SCV knows five agent CLIs: Claude Code (`claude`), Codex (`codex`),
Grok Build (`grok`), DeepSeek Harness (`dsh`), and pi (`pi`), plus a nested
SCV (`scv`, see [Nested SCV](#nested-scv-scv)). Claude Code, Codex, Grok
Build, and DeepSeek Harness run over the
[Agent Client Protocol](#agent-client-protocol-transport) when its server is
installed. Each is one descriptor in `scv_tools::adapters` holding its default
command line, where its state lives inside the private home, the variables it
must not inherit, and how it signs in; adding an agent is one more entry. A
session offers only the agents whose executable resolves when the session
starts, so an agent installed later appears in new sessions. An agent whose
sign-in SCV checks by reading a local file (Grok, DeepSeek Harness, pi, and
the nested SCV; see [Signing in](#signing-in-delegated-agents)) is also left
out while that check says it is signed out. Claude Code and Codex report
sign-in only through their own CLI, which is too slow to run at every session
start, so they stay offered and a signed-out call fails with the sign-in hint.
A session that offers no agent has no `agent` tool.

Behind the tool, each agent runs on one of three backends: its CLI once per
turn, its ACP server for a whole conversation, or a nested
`scv server --stdio` for a whole conversation. The `agent` tool chooses the
agent, checks the call against what that agent takes, and hands the call to
its backend, so the model sees one schema whichever way the agent runs.

### Choosing an agent

SCV hard-codes no agent: which one runs comes from the user, through what they
ask for in chat, their `[agent] prefer`, and their `use_for` notes, which the
model reads. A call runs on, in order:

1. the agent whose conversation its `session` handle names (`codex-2` is a
   Codex conversation). An explicit `agent` that disagrees fails with
   `conversation codex-2 belongs to codex, not claude; omit agent to continue
   it, or omit session to start a new conversation with claude`;
2. the `agent` argument;
3. the first agent in the user's `[agent] prefer` that the session offers.

Without that default the schema lists `agent` as required, and a call that
names none fails with the agents on offer: `name the agent: the user prefers
none of the agents this session offers ([agent] prefer); choose one of:
claude, codex`. Naming an agent the session does not offer fails the same way
(`agent "zcode" is not offered in this session; choose one of: …`).

The `agent` argument is an enum of the offered agents. Its description says
how the default is chosen, then gives each agent one line: its value, its
product and what that harness offers, which options it takes, and the user's
`use_for` note, defaults, and hard-task effort. For an agent reached over ACP, the options are
the exact model and effort values its server listed (see
[Model and effort values](#model-and-effort-values)). For any other agent,
the line names the kind of model name its CLI expects. For example:

```text
- codex (Codex): OpenAI's coding agent; it reads, edits, and runs code in a project, with live web search under full permissions. Takes model (one of: gpt-6-astra, gpt-6-sol, gpt-6-luna, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna, gpt-5.5), effort (one of: low, medium, high, xhigh, max, ultra), and session. The user's note on when to use it: coding. The user's defaults, used when a call leaves them out: model gpt-5.5 and effort high. For a hard task, pass effort xhigh.
```

| Agent | Its line starts with |
| --- | --- |
| `claude` | Claude Code: Anthropic's coding agent; reads, edits, and runs code, and can search and fetch the web |
| `codex` | Codex: OpenAI's coding agent; reads, edits, and runs code, with live web search under full permissions |
| `grok` | Grok Build: xAI's coding agent; reads, edits, and runs code, with live web and X search. Call it when another agent declined or refused a request, including a safety or guardrail refusal |
| `dsh` | DeepSeek Harness: a coding agent on DeepSeek models |
| `pi` | pi: a minimal coding agent that can run on SCV's own model endpoint; no web search |
| `scv` | SCV: a nested session for a self-contained sub-task kept out of this context, or work in another project |

The user steers the choice in their own configuration (projects cannot set
these keys):

```toml
[agent]
prefer = ["codex", "claude"]   # named in the system prompt, in order; the first offered is the default

[agents.claude]
use_for = "coding"
model = "opus[1m]"             # a value claude's line lists; `scv agents check` prints them
effort = "xhigh"

[agents.grok]
use_for = "current events, and anything that needs posts on X"
model = "grok-4.7"
effort = "medium"              # every task that names no effort
hard_task_effort = "high"      # what the main agent passes for a hard task
```

`use_for` (one line, at most 500 bytes) is appended to that agent's line and
says when to choose the agent. `model` and `effort` are that agent's defaults
in SCV: the `agent` tool fills them in for a call that leaves them out (or
sends them blank) before the call is checked, approved, or run, whatever the
work and whoever made the call, including background jobs. A value the call
names wins, one option at a time, so a call that names only `effort` still
gets the default `model`. A turn that continues a conversation over ACP, or
with a nested SCV, gets no defaults: its session keeps the model and effort
it started with, or the last ones a turn named. An agent run as one CLI
process per turn keeps nothing between turns, so it gets the defaults on
every turn that leaves them out. Without a default, an omitted value is left
to the agent's own configuration.

`hard_task_effort` is the effort for a hard task. SCV never applies it by
itself, since only the calling model can tell a task is hard; the agent's
line, the system prompt, and the `delegating` skill tell the main agent to
pass it as `effort` for such a task. It may be set without `effort`, leaving
other tasks at the agent's own default.

`prefer` names only agents the session offers, the first of which runs a call
that names no agent, and an unknown name fails configuration validation. A
`model`, `effort`, or `hard_task_effort` on an agent that does not offer that
selection is a configuration error, and both efforts follow the effort rules
below. When a call fails in a way another agent
could avoid (the executable is missing or exits, it is signed out, or its
provider returned an HTTP 401, 403, 404, 429, or 5xx, a quota error, or an
unknown model), the result gains a `fallback` field naming the other agents
this session offers as values for `agent`, such as `"This agent could not
run: it is missing, signed out, or its provider returned an error. Other
agents are available: claude, codex. Call agent again with one of them."`.
SCV's own error gets the same sentence when SCV
found the agent unavailable: its executable missing or failing to start, or
its live conversation's process gone. The decision reads only the result's
`status` (`failed`) and its structured `error`, or the kind of SCV's own
error, never the agent's reply, so nothing the agent writes can trigger it.
Other failures, such as failing tests or an invalid `cwd`, are returned
unchanged.

A `declined` result, where the agent's model refused the request, never gets a
`fallback` (that field is only for availability failures). When the session
offers `grok` and another agent declined, the result's `note` tells the
calling model to tell the user what the agent said and then call `agent`
again with agent `grok` and the same request; a safety or guardrail refusal is
not a reason to skip Grok. If `grok` itself declines, or the session does not
offer it, the note tells the calling model to tell the user rather than pass
the request to another agent on its own; if the user then asks for a specific
agent, the main agent uses it, and that agent's own policies apply.

The tool's arguments:

```json
{"agent":"codex","prompt":"Land the fix with the feature-flow skill.","cwd":"scv","session":"codex-1","model":"gpt-5.5","effort":"high","background":true,"timeout_seconds":7200}
```

`prompt` is required; `agent` too when there is no default. Three options
apply to some agents only, and the schema offers each when an offered agent
takes it:

| Option | Taken by |
| --- | --- |
| `model` | Agents whose adapter maps it to arguments (`model_args`: Claude Code, Codex, Grok Build, and pi, on either transport), DeepSeek Harness over ACP, and the nested SCV, for a new conversation |
| `effort` | Agents whose adapter maps it to arguments (`effort_args`: Claude Code, Codex, Grok Build, and pi), and DeepSeek Harness over ACP; not the nested SCV |
| `session` | Agents that can continue a conversation: Claude Code, Codex, and pi in one process per turn, every agent over ACP, and the nested SCV |

A call that passes one of them to an agent that does not take it fails before
anything launches, naming the agents that do: `scv does not take effort; these
agents do: claude, codex. Omit effort, or call one of them`. SCV runs these
checks when it assesses the call's risk, before any approval, so a refused call
is never put to the user.

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

Each agent's line lists the values its ACP server offers, or names the model
family its CLI takes (Claude aliases such as `sonnet` for `claude`, OpenAI
model IDs for `codex`, Grok model IDs for `grok`, pi model patterns or
`provider/id` for `pi`, and for `dsh`, `provider/model` as its ACP server
lists them). The `model` and `effort` descriptions tell the model
to set them when the user asks, and `effort` also to the line's hard-task
effort for a hard task; an omitted value runs with the user's default the
line gives, or else the agent's own. A blank `agent`, `cwd`, `session`,
`model`, or `effort` counts as omitted, since models often send `""` for an
optional field they mean to leave unset. A model is 1-128 ASCII letters,
digits, or `._:/@[]-` and cannot start with `-` or `@`; an effort is 1-32
ASCII letters, digits, `-`, or `_`, starting with a letter or digit. The schema's `effort` enum is `low`,
`medium`, `high`, `xhigh`, and `max`, plus the values offered agents list or
the user configured as `effort` or `hard_task_effort` (see
[Model and effort values](#model-and-effort-values)).
Which efforts an agent supports is its own to check, since its levels change
with its releases. Each selected value becomes one
substituted argument, never shell text, so the CLI itself reports values it
does not support.

Each agent resolves only its configured executable and fixed argument vector,
adds any selected or default model/effort arguments, then any `prompt_args` (for CLIs
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

### Project Rust environment

Before starting a native CLI, ACP server, or nested SCV, the shared
`scv-tools::project_environment` preflight resolves Rust for that run's
canonical `cwd`. It searches ancestors for the nearest Cargo manifest and
toolchain file (`rust-toolchain` takes precedence over `rust-toolchain.toml`
in the same directory, as in rustup). Without either file, it leaves the
environment unchanged and requires no Rust installation.

Cargo's `package.rust-version` and edition provide the minimum version.
Workspace inheritance is resolved from `workspace.package`, including
`package.workspace` paths; a virtual workspace uses its common requirements.
Rust versions are compared numerically, without an SCV-specific minimum or
a hardcoded stable version. Unknown editions and malformed or unresolved
requirements fail with a diagnostic. Manifest reads are capped at 1 MiB.

SCV searches for rustup in the launch environment's `CARGO_HOME/bin`, then
the original `HOME/.cargo/bin`, then `PATH`, so a user service's short PATH
does not hide it. It retains the launch environment's `RUSTUP_HOME`, or
derives it from the original home, before relocating the agent's home.
Rustup itself interprets toolchain files, including channels, dated nightly
releases and path toolchains, and applies its normal
[override precedence](https://rust-lang.github.io/rustup/overrides.html).
An explicit `RUSTUP_TOOLCHAIN`, directory override, or project pin that is
missing or too old fails preflight; SCV does not substitute system Rust.
If only the rustup default is too old, SCV selects the newest compatible
installed stable/versioned toolchain by measured compiler version. Without
rustup or an explicit selection, system tools are accepted only when both
`rustc --version` and `cargo --version` meet the project minimum.

For a Rust project the child receives the resolved toolchain's bin directory
first on `PATH`, followed by rustup's bin directory and the existing PATH.
`RUSTC` and, when installed, `RUSTDOC` identify the selected tools explicitly.
`CARGO_HOME` is `<agent-home>/.cargo`: the user's Cargo configuration, registry
credential files and caches are not reused. `RUSTUP_HOME` shares installed tools;
the private `HOME`, XDG directories, and native-agent state remain private.
No files, user-service drop-ins, or rustup defaults are rewritten. Probes
disable automatic installation with `RUSTUP_AUTO_INSTALL=0`, never pass
`--install`, and have bounded output, cancellation, and a 20-second total
process budget (at most five seconds per probe).

Check the same resolver without calling a model or starting a daemon:

```bash
scv agents doctor --workspace /absolute/path/to/project
scv agents doctor claude --workspace /absolute/path/to/project
```

The optional agent defaults to `codex`. Doctor needs no installed agent or
sign-in, prints the detected requirements, selected executable paths and
versions, and Rust environment additions, and exits nonzero on failure. It
does not create agent homes. It inspects its own launch environment; a daemon
started with different environment variables may resolve differently.

Preflight runs once per child launch. Start a new ACP/nested-SCV conversation
after changing requirements; native CLI turns resolve again. The selected
bin directory applies for the lifetime of the child, including commands it
runs after changing directories. To select a different toolchain explicitly,
use `rustup run <toolchain> ...` and override `RUSTC`/`RUSTDOC` accordingly;
direct Cargo binaries do not implement rustup's `cargo +toolchain` shorthand.
This is a launch check, not a build: it does not enumerate every workspace
member or dependency's MSRV, resolve Cargo configuration or compiler wrappers,
or verify optional components/targets beyond the installed compiler and Cargo.
Cargo still diagnoses those during the build. Missing required tools must be
installed explicitly before retrying. As with delegated execution itself,
running doctor on a project trusts its selected tool executables.

The default invocation contracts are:

| Agent | Invocation |
| --- | --- |
| `claude` | `claude -p --output-format stream-json --verbose --session-id <uuid> [--model <model>] [--effort <effort>] <prompt>` |
| `codex` | `codex exec --json -o <file> [-m <model>] [-c model_reasoning_effort="<effort>"] <prompt>` |
| `grok` | `grok [-m <model>] [--reasoning-effort <effort>] -p <prompt>` |
| `dsh` | `dsh --profile headless <prompt>` |
| `pi` | `pi -p --mode json [--model <model>] [--thinking <effort>] <prompt>` |

Grok's `-p` and pi's `-p` run one prompt and exit. DeepSeek Harness run once
per turn takes its model and effort from its profile, so it takes neither
`model` nor `effort` then; over ACP (the default once `dsh` is installed) it
takes both (see [Agent Client Protocol transport](#agent-client-protocol-transport)).
`permissions = "full"` switches follow the fixed arguments, before the output
format arguments.

Tool-enabled sessions that offer agents also list the built-in `delegating`
skill, and the system prompt tells the model to read it before its first
`agent` call. The skill covers choosing the agent, model, and effort, and
writing a brief that stands on its own. It also covers background jobs and
continuing conversations, and what to do about each error a call can
return. It names no model: it points the model at its `agent` line for
current values, and at `scv agents check`. A `delegating` skill in the
workspace's or user's skill directory replaces it (see
[`read_skill`](#read_skill)).

### Model and effort values

Model names and effort levels belong to each agent and change when it
updates, so SCV reads them from the agent instead of shipping a list. Each
time SCV opens an ACP session, the server's `session/new` result lists its
`configOptions`. SCV saves the `model` option and the effort option
(`effort`, `reasoning_effort`, or `thought_level`), each with its values and
current value. A server may list an option's values flat or in groups; SCV
takes all of them. A value that is a JSON array of strings, as DeepSeek
Harness's `["provider","model"]` model values, is listed and taken as the
strings joined by `/`, such as `xubao/glm-5.3`, since a model name cannot hold
quotes or commas; SCV then sends the agent its own spelling of the value. The file is `$SCV_HOME/state/agent-options/<agent>.json`
(mode `0600`). It records when it was saved and the server's resolved file,
size, and modification time. SCV keeps at most 64 values per option, and
only values it can pass as one argument: model values follow the `model`
rules in [Choosing an agent](#choosing-an-agent), and effort values follow
the effort rules there.

A new session uses the saved file when the agent is reached over ACP, the
file names the same server file unchanged, and it was saved within the last
seven days. Then:

- The agent's line lists the values in place of a hint, as in `Takes model
  (one of: opus[1m], claude-fable-5-1[1m], sonnet, haiku), effort (one of:
  low, medium, high, xhigh, max), and session.` It leaves out `default`,
  since omitting the argument selects that anyway, and appends a named
  default, as in `; its default is gpt-5.6-sol`, unless the user set their
  own default for that option, which runs instead.
- The `effort` enum gains any listed value beyond SCV's own `low`, `medium`,
  `high`, `xhigh`, and `max`, such as Codex's `ultra`, and any `effort` or
  `hard_task_effort` the user configured for the agent.
- A call whose model is not listed is refused before anything starts, even
  as a background job:
  ```text
  model "opus-5.5" is not one claude offers; choose one of: opus[1m], claude-fable-5-1[1m], sonnet, haiku, or omit model for its default. This list is from claude's last session; if the user named a newer one, run `scv agents check claude` to refresh it
  ```
  When the refused model is the user's `[agents.<name>] model`, omitting it
  would bring it back, so the error says so instead:
  ```text
  model "opus-5.5" is the user's default for claude ([agents.claude] model), but not one claude offers; pass one of: opus[1m], claude-fable-5-1[1m], sonnet, haiku, and tell the user so they can change that setting. This list is from claude's last session; …
  ```
  Another refused model, while a default is set, ends `or omit model for
  the user's default, <model>` instead of `for its default`.
  Before refusing, the tool reads the saved file again. Another session, or
  `scv agents check`, may have saved a newer list, and the tool then goes by
  that list. Each new ACP session of the tool also replaces the tool's copy.
  Efforts are not refused this way: their values depend on the model, and a
  continued conversation's model is not known here. The agent checks them
  and fails the call with its own list.

Without a usable file, the line says the model must be a value the ACP
server lists, which SCV has not seen yet. A server that listed efforts but
no model gets `its ACP server lists no model to choose, so omit model`. A
value the server does not list fails in `session/set_config_option` with its
list, and that session's values are saved for the next one. An agent that
runs its CLI once per turn lists nothing, so its line keeps the adapter's
hint (such as `Claude model alias or ID, such as sonnet or opus`) and the
CLI checks the value.
`[agents.<name>] model`, `effort`, and `hard_task_effort` should be values
from the agent's list, which `scv agents check` prints (see
[Checking delegated agents](#checking-delegated-agents)).

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
the agent's own context. The handle names its agent, so a call that continues
a conversation needs no `agent`:

| Agent | Starts with | Continues with |
| --- | --- | --- |
| `claude` | `--session-id <uuid>` (chosen by SCV) | `--resume <uuid>` |
| `codex` | nothing; the thread ID comes from `thread.started` | `codex exec resume … <thread-id> <prompt>` |
| `pi` | `--session-id <uuid>` (chosen by SCV) | `--session-id <uuid>` |

Grok and DeepSeek Harness start a fresh conversation on every call: their
resume options could not be verified headless here, so in one process per
turn they take no `session`, and a call that passes one fails before launch.

The model only ever sees handles. The CLI's own session IDs stay inside SCV,
and a value that is not one of this session's handles, such as a raw vendor
ID, is refused. A conversation keeps its agent and `cwd`: continuing it
elsewhere is an error, so start a new one there instead. One turn runs at a
time; a call that continues a conversation while a turn runs, or while
prompts wait for it, follows the session's busy policy (see
[Busy conversations](#busy-conversations)). A turn that
times out stays resumable once the CLI has reported its session, so the next
turn can ask the agent to continue where it stopped. A first turn that fails
before the CLI reports a session is forgotten.

Each session remembers at most `agent.max_conversations` conversations
(default 8; starting another forgets the least recently used idle one) and
forgets one left unused for `agent.conversation_idle_seconds` (default 86400).
A [reviewed job](#reviewed-jobs) holds its builder's conversation, and its
approving reviewer's until the landing is confirmed, out of both. When every
conversation is running a turn or held so, starting another fails with `all N
conversations of this session are running a turn` (`or held by a reviewed
job` when some are held).
Handles end with the SCV session. `scv agents ps` shows a running turn's
conversation and turn number, and lists a nested SCV or ACP agent that waits
between turns of its conversation as `idle`, or `background` while a nested
SCV's own background jobs still run or wait to be reported to it.

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

Any `agent` call may set `"background": true`. The call then returns at once
with a job handle, `{"job":"job-1","agent":"codex","status":"running",
"background":true}`, while the agent keeps working, so the turn ends and the
user can keep talking to the main agent. The system prompt makes this the
default way to work (see [Delegate first](#delegate-first)). The job runs
exactly as a foreground call would, on the agent the call chose and in its
conversation (`session`, `cwd`, model, and timeout apply as usual), tracked
like any delegation, and its final result is the structured result above. A
call with `review` runs as a [reviewed job](#reviewed-jobs). A call the
`agent` tool refuses starts no job. Three tools manage a session's jobs:

- `agent_wait {job, timeout_seconds?}` blocks until the job finishes or the
  timeout passes (default `tools.agent_timeout_seconds`, at most
  `tools.max_timeout_seconds`) and returns `{"job","agent","status",
  "elapsed_seconds","result"}`, or `"status":"running"` with its latest
  progress line;
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
the limit and `agent_cancel`. A busy conversation's queued prompts (see
[Busy conversations](#busy-conversations)) do not count toward it. The
session remembers its 16 newest finished jobs.

When a job finishes and the model has not already seen its result through
`agent_wait` or `agent_status`, the server reports it: once the session is idle
(the user's own queued prompts run first), it starts a turn of its own whose
prompt, beginning `[SCV background report]`, names each finished job, its
agent, conversation, status, task, and bounded reply, and asks the model to
tell the user. That turn's `turn.started` and final event carry
`"origin":{"kind":"background","jobs":[...]}` (see
[protocol](protocol.md#server-started-turns)); one turn reports up to four jobs.
A chat channel (WeChat, Feishu, or Slack) sends the owner the answer as an unprompted message; `scv exec`
prints it and stays open until every job it started has been reported; the
TUI shows it like any turn.

A job counts as reported only once a report turn about it completes (or the
user cancels that turn, or the model looks the job up). A report turn that
fails, such as when the provider answers `HTTP 503` because no upstream
serves the model, leaves no history, so the model has not seen the result
and the job stays unreported:

- When the failure is the provider's and no tool ran in the turn, the job is
  reported again in a new turn 30 seconds later, then 120 seconds after a
  second failure, or as soon as any other turn of the session succeeds, since
  the model is then evidently reachable. Its `turn.failed` says when
  (`origin.retry_seconds`), and clients send nothing for it.
- After the third failed report turn, or at once when the failure was not the
  provider's or a tool ran (a retry could repeat the tool's side effects), the
  server reports the jobs to the client itself, without the model:
  `background.reported` carries the last failure's code and message, the
  number of failed report turns, and each job's handle, agent, task, status,
  conversation, and bounded reply. A chat channel sends it to the owner as
  SCV's own message, `scv exec` prints the replies, and the TUI shows them.
  The session's history then gets a note, as a user message beginning
  `[SCV background report, already delivered]`, quoting the same results and
  the error, so the model's next turn knows the user has them.

The server logs every failed report turn with its jobs and error. A job
waiting to be reported again still counts as unreported: a planned restart
waits for it (see [architecture](architecture.md#planned-restarts)), and
`agent_status` still shows it. So does a cancelled
[reviewed job](#reviewed-jobs) whose final outcome is still owed, until it
was sent (each job counted once).

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

So a WeChat, Feishu, or Slack owner's background agents get the approvals the owner's
foreground turns get, while in the TUI, which asks a person, a background
job's non-read-only requests are denied and it relies on the agent's own
permissions, such as `permissions = "full"`. Jobs belong to their session:
closing it (a TUI or `scv exec` exiting, an idle channel conversation ending)
cancels every job still running and kills its processes. A channel
conversation stays open while its jobs run, and a full channel session table
never closes it to make room.

### Busy conversations

Each call that continues a conversation (`session`) takes its place in that
conversation's line as it arrives, before anything runs, and keeps it until
its turn ends. Only the first call in line runs a turn, so prompts reach the
agent in the order the calls arrived, and a later call never overtakes one
already waiting. A call that finds a turn running, or other calls waiting,
does what its `on_busy` argument says, else its agent's
`[agents.<name>] on_busy`, else `agent.on_busy`:

- `queue` (the default) returns a background job handle at once, with
  `"queued":true`, and runs the prompt in the background once the calls
  ahead of it have run; `agent_status` shows it waiting, `agent_cancel`
  withdraws it without sending it, and its result is reported like any
  job's. At most `agent.max_queued_turns` prompts (default 4, at most 32)
  wait per conversation, a call beyond that is refused, and queued prompts
  do not count toward `agent.max_background`.
- `wait` keeps the call, and so the caller's turn, until the calls ahead of
  it have run, then runs its turn in the foreground. A `background: true`
  call cannot keep its caller, so it queues instead.
- `fail` refuses the call with `session busy`.
- `steer` hands the prompt to the running turn, when the conversation's ACP
  server advertised steering (see
  [Agent Client Protocol transport](#agent-client-protocol-transport)), and
  returns `{"agent","status":"steered","session"}`: the agent's answer comes
  with that turn's result. When there is nothing to steer (a CLI run once
  per turn, a nested SCV, an ACP server without steering, no turn running
  yet) or the agent refuses, `steer_fallback` (default `queue`; `queue`,
  `wait`, or `fail`) applies instead. An agent that does not answer the
  steering request within 30 seconds fails the call, since it may still
  take the prompt.

A background call follows the same policy, so `background: true` never
starts a second turn beside a running one. A [reviewed call](#reviewed-jobs)
never steers or waits: it queues, or fails with `fail`. Without background jobs
(`agent.max_background = 0`) nothing can queue, so `queue` refuses the call
as busy. Queued prompts live only in the session's memory: a daemon restart
or the end of the session drops them, as it cancels the session's jobs.

### Reviewed jobs

An `agent` call may set `review` to run as a reviewed job: the agent (the
builder) does the work, a fresh reviewer on another agent checks it and ends
with a structured verdict, and the builder fixes the blocking findings, for
at most the rounds the call names. It is one background job, with one `job`
handle, `agent_status`, `agent_wait`, `agent_cancel`, and one report, and SCV
states its outcome in its own Review and Landing lines. A call without
`review` runs exactly as before. The property is offered only where
background jobs are on, at least one offered agent can continue a
conversation, and the session keeps journals (an SCV session always does).

```json
{"agent":"codex","cwd":"shop","prompt":"…self-contained brief…",
 "review":{"rounds":3,"land":"after_approval","focus":"fix the race, no sleeps"}}
```

| `review` field | Default | Meaning |
| --- | --- | --- |
| `agent` | Routed (below) | The reviewer. Naming one turns off every fallback. |
| `rounds` | 3 | The round limit, 1 to 20. |
| `land` | No landing | `after_approval` or `before_review` (see *Landing*). |
| `focus` | None | Extra instructions for every reviewer: at most 2 KiB of text, where line breaks and tabs are allowed and other control characters are not. |
| `model`, `effort` | The reviewer's defaults | Only together with `agent`. |
| `timeout_seconds` | `tools.agent_timeout_seconds` | Each reviewer turn, at most `tools.max_timeout_seconds`. |

The call is checked before approval, and nothing launches when it fails: an
explicit `background: false`, a builder that cannot continue a conversation
(the error names the agents that can), `rounds` outside 1 to 20, an unknown
`land`, an over-long `focus`, `model` or `effort` without `agent`,
`agent.max_conversations` below 2, and any reviewer the order may launch
that fails the same checks an ordinary call to it would (each is routed with
a placeholder prompt). One approval covers the job: its summary adds the
round limit, the landing mode in capitals when the job may land, and every
reviewer launch the order may make, each with the approval text an ordinary
call to that agent would show. The job takes one `agent.max_background`
slot, or one `agent.max_queued_turns` place when it queues behind a busy
builder conversation; reviewer turns take none. The start result adds
`"review":{"reviewers":[…],"rounds":3,"land":"after_approval","journal":"rev-…"}`.

#### Choosing the reviewer

| Builder | Reviewer, the first available of |
| --- | --- |
| `claude` | `codex`, `grok`, a fresh `claude` conversation |
| `codex` | `claude`, `grok`, a fresh `codex` conversation |
| any other (`grok`, `dsh`, `pi`, `scv`) | `claude`, `codex`, `grok`, a fresh conversation of the builder's agent (for `grok`, the last two are the same) |

Agents the session does not offer are dropped before approval. A fresh
conversation of the builder's own agent shares the builder agent's private
home and memory, so SCV labels it `same agent as builder`. SCV moves to the
next agent, in the same round, only when the attempt is classed unavailable,
exactly as an ordinary call's `fallback` is decided (see
[Choosing an agent](#choosing-an-agent)): from its status and structured
error, never its reply. Such an agent is skipped for the rest of the job.
A refusal (`declined`) hands the review to `grok` for the rest of the job
when `grok` is offered and is neither the agent that refused nor the
builder's agent; otherwise, or when `grok` then declines or is unavailable,
the round ends `no_verdict` (`reviewer_declined`). After a refusal the
builder's own agent never reviews. What a reviewer that declined said goes
into the result's `review.refusals`. A named reviewer is never swapped: its
unavailability or refusal ends the round `no_verdict`. Nothing else moves to
another agent: a timeout, a kill, a failure without an availability error,
or a verdict of any kind.

#### The loop

A round is one builder turn, then one verdict on it from a reviewer
conversation started for that round:

1. The builder works in one conversation for the whole job: a new one, or
   the call's `session`. Round 1 gets the prompt plus SCV's notice of the
   review, the round limit, the landing mode, and the `scv-landing` block.
   Later rounds get a fix prompt, `[SCV review, round k of R]`, listing the
   open blocking findings by number and the last verdict's minor ones, and,
   when commits already landed, that fixes go on top. The builder may answer
   a finding with reasons instead of a change. A turn that does not
   complete stops the job (`stopped`, `builder_<status>`); a first turn
   without a conversation to continue stops it with `builder_no_session`.
2. A new reviewer conversation, in the builder's `cwd`, gets SCV's rules,
   the builder's prompt (8 KiB), `focus`, the landing mode, the builder's
   latest reply (8 KiB, marked untrusted), and from round 2 the open findings
   to settle. Every open finding is listed with its ID, severity, and title,
   since the verdict must settle each by ID; when the list would pass
   48 KiB, details are shortened first and then titles, never a finding
   dropped (the builder's fix prompt lists them the same way). It is never
   told the round limit. It finds the change itself:
   the uncommitted changes, or with a clean tree the commits the builder
   names or the landed commits; it reviews the whole change every round,
   never approves an empty diff, and escalates when it cannot identify it.
3. SCV reads its verdict. `approve` ends the review `approved`. `escalate`
   ends it `escalated`. `changes` in the last round ends it `unresolved`
   (`round_limit`), so the last builder turn always gets a verdict;
   otherwise the next round begins.

The job holds the builder conversation's place in its lane for every round,
so other calls to it queue behind the whole job, and pins it against
eviction and idle expiry. A reviewer conversation is released after its
round on every path and its handle is never returned; the approving
reviewer of an `after_approval` job is kept, pinned, until its confirmation
ends. The builder's handle comes back in the result (`session` and
`review.builder_session`). Each turn has its own timeout: builder and
landing turns the call's `timeout_seconds`, reviewer and confirmation turns
`review.timeout_seconds`, repair turns the lower of 300 seconds and that.

#### The verdict

The reviewer ends its reply with one fenced block, of which SCV reads only
the last, at most 16 KiB; it never reads prose such as "LGTM":

````text
```scv-verdict
{"verdict":"changes","summary":"The race is still masked by a sleep.",
 "prior":[{"id":"1.1","status":"open","note":"cart.rs:88 still sleeps"}],
 "findings":[{"severity":"minor","title":"Test name typo","location":"shop/tests/checkout.rs:41"}],
 "evidence":["ran cargo test -p checkout 20x: 2 failures"]}
```
````

- `verdict` is `approve`, `changes`, or `escalate`; `summary` is required.
- `findings` are new findings only, at most 20, each `blocking` or `minor`
  with a `title`; SCV numbers them `<round>.<n>`. Only blocking findings
  start another round.
- From round 2, `prior` settles every open finding by its ID exactly once:
  `resolved`, still `open`, or `withdrawn` with a `note`.
- `approve` needs `evidence`, no prior finding left open, and no new
  blocking finding; with `land: "after_approval"` it also names the commits
  it approved, `"approved":{"base":"<sha>","head":"<sha>"}`. `changes` needs
  an open prior finding or a new blocking one.
- When the builder reported a landing that round, `landing_check` says
  whether the commits are on the ref: `{"status":"confirmed|not_found|mismatch|unverifiable","ref","commits","evidence","note"}`.

Unknown fields are ignored, long strings are cut, and control characters
are removed; anything else that breaks these rules, a missing block, or one
a cut reply may have lost, is malformed. A reviewer whose turn completed and
whose conversation can be continued gets one repair turn ("Reply with only
that block"); still malformed, or one that cannot be continued, ends the
round `no_verdict` (`malformed_verdict`). A repair turn that declines,
times out, fails, or is unavailable ends the round `no_verdict` with that
cause instead (`reviewer_declined` keeps its words in `review.refusals`),
and no other reviewer is asked. A `changes` or `escalate` verdict counts
however the run ended, so no fallback gets around it; an `approve` counts
only from a run that completed.

#### Landing

| `land` | The builder may land |
| --- | --- |
| omitted | Not in this job. |
| `after_approval` | After an approving verdict, in one more builder turn, which the approving reviewer then confirms. |
| `before_review` | In each round's builder turn, before that round's review. |

A builder turn that lands ends its reply with a fenced block, the last of
which SCV reads (at most 4 KiB):

````text
```scv-landing
{"status":"landed","ref":"origin/main","commits":["4f2a9c1e0b7d"],"detail":"squash-merged PR #77"}
```
````

`status` is `landed` (with a `ref` and 1 to 20 commits of 7 to 40 lowercase
hex characters), `not_landed`, or `failed` (partial state possible); an
`https://` `url` is kept in the result, never in SCV's lines. A turn that may
land and does not end with a valid block, or does not complete, leaves the
landing `unknown`; the block is read before the status, so a landing that a
failure followed is still recorded. In a turn that may not land, a `landed`
or `failed` report is recorded as NOT authorized. Landed history is never
rewritten: fixes are new commits on top, landed again only with
`before_review`.

After an `after_approval` landing turn that reports `landed`, the approving
reviewer gets one confirmation turn in its own conversation. It checks the
repository, not the report: the commits are on the ref, together they carry
exactly the approved `base..head` (commit IDs may differ after a squash merge
or a clean rebase; the content may not), and nothing else landed with them.
It ends with an `scv-landing-check` block, the `landing_check` object. The
confirmation is not a round, never changes the review outcome, and never
leads to a fix, revert, or re-landing. Only a confirmation run that
completed counts: one that times out, declines, is unavailable, fails, or is
stopped leaves the landing NOT confirmed whatever its block says (the
journal keeps the block), as do a malformed block and a reviewer that
cannot be continued, with no repair and no other agent. A negative block
from a failed round verdict still counts because a fallback reviewer could
otherwise get around it; the confirmation has no fallback, so nothing is
gained by trusting a run that did not finish. A landing turn that does not complete or reports
`not_landed` or `failed` gets no confirmation, and the result's `fallback`
or refusal `note` is replaced by one saying the approved work did not land
and must not be landed through another agent unless the user asks.

SCV runs no `git`, so a `landed` status is labelled by who backs it:
`confirmed by reviewer <agent>` when checks found every landed commit on the
ref, `reviewer <agent> could NOT confirm: <why>` when one found them missing
or different, `NOT confirmed: <why>` when a due check produced no result,
and `builder-reported, not verified` when no check was due. A check is due
once the turn that landed completes: the round's review for a round turn,
the confirmation for the landing turn. One the job never reached, because it
was cancelled or its journal could not be written, is NOT confirmed (`the
confirmation did not run: the journal could not be written`), never
builder-reported. No check is due only when the turn that landed did not
complete, which stops the job before any review.

#### What SCV reports

The job's result is the builder's last agent result plus `review` (the
outcome, reason, round, rounds, reviewer, fallback, tried, refusals,
summary, open findings, the last verdict's numbered `findings`,
`builder_session`, and `journal`) and `landing` (the landing summary and any
`url`). The job's `status` is how its last builder turn ended (`cancelled`
when the job was cancelled), apart from the review outcome:

| Outcome | When |
| --- | --- |
| `approved` | A reviewer approved round *k*: the only approval |
| `unresolved` | The last round still had open blocking findings |
| `escalated` | The reviewer said the user must decide |
| `no_verdict` | No reviewer was available or had room, or it declined, timed out, was stopped, failed, or stayed malformed |
| `stopped` | A builder turn did not complete, the job was cancelled, or the journal could not be written before the outcome was decided (`journal_error`) |

While it runs, `agent_status` adds
`"review":{"round","rounds","phase","reviewer","land","builder_session","journal"}`,
with `phase` one of `builder`, `reviewer`, `repair`, `landing`, or
`confirmation`, and the progress line names the phase. The report, its turn's
`origin.outcomes`, the change that settles the job, and a direct
`background.reported` all carry the outcome (see
[protocol](protocol.md#reviewed-job-outcomes)), so SCV's own lines reach the
user apart from the model's summary, which is told to repeat them as given:

```text
job-4 · Review: approved · round 2 of 3 · reviewer claude
job-4 · Landing: landed · 4f2a9c1 → origin/main · confirmed by reviewer claude
job-7 · Review: NOT approved · unresolved after 3 of 3 rounds · 1 blocking finding open:
  - "Sleep instead of a lock" (shop/src/cart.rs:88)
job-7 · Landing: not attempted · review not approved
job-9 · Review: NOT approved · no verdict in round 2 of 3 · reviewer grok (codex unavailable) timed out
job-9 · Landing: landed before review · 4f2a9c1 → origin/main · confirmed by reviewer grok
```

The Review line is `approved · round k of R · reviewer <agent>`, or `NOT
approved` with `unresolved after R of R rounds` and up to five open
findings, `escalated in round k of R · reviewer <agent>: "<summary>"`, `no
verdict in round k of R · <why>`, or `stopped in round k of R · <why>`
(`stopped before round 1` when no builder turn ran). The Landing line is
`not requested`, `not attempted · <why>`, `landed[ before review] · <commits>
→ <ref> · <evidence>[ · NOT authorized by this call]`, `not landed ·
builder-reported: "<detail>"`, `failed · builder-reported · partial state
possible: "<detail>"`, or `unknown · <why> · check before relying on it`.
A Review line whose journal is incomplete ends `· journal <id> INCOMPLETE: a
write failed`. The TUI shows the lines as system items when a report turn
starts and when a call settles the job, `scv exec` prints them on stderr, and
chats send them as SCV's own message (see
[channels](channels.md#background-reports)). These lines hold SCV's words
only. The report the model reads, and the direct report a user gets when the
model cannot write one, also quote what each reviewer that declined said,
attributed and marked untrusted (at most 300 characters each), so the user
can be told.

`agent_cancel` stops the running turn and records the review as it stood:
an outcome the loop already fixed (an approving verdict) stands, otherwise
the review is `stopped` (`cancelled`); a turn that may land leaves the
landing `unknown`, and a running or still due confirmation leaves it NOT
confirmed. The cancel's change, the result, and the journal always agree. A
journal write can still fail while the job stops, so the cancel's change
states the outcome once the job has stopped, within the cancel's usual wait
of up to 10 seconds, `journal_incomplete` included. The decision itself is
fixed the moment the cancel arrives and never changes after; only the
journal goes on until the job really ends, recording whatever the stopping
turn still reports, such as a landing, which the outcome keeps as `unknown`.
The cancel never touches the journal, so a journal write stuck on a slow
disk cannot hold it past its wait.

The session is owed the job's final outcome, once the job has stopped, and
gets it exactly once:

- in the cancel's own change, when the job has stopped by the time the
  change is sent, even after the wait: the change then says the final
  outcome, never the earlier snapshot;
- otherwise the change says the journal is still open (`journal_pending`;
  its Review line ends `still open: the job is still stopping, an update
  follows`), and once the job stops the session gets the final outcome in a
  `background.updated` event (see
  [protocol](protocol.md#reviewed-job-outcomes)), the same decision with the
  journal complete or INCOMPLETE.

What is owed counts as delivered only once its event was actually sent. A
change that never goes out, because its turn was cancelled or aborted
first, the `agent_cancel` call itself was cut off, or it was dropped among
too many waiting to be sent, leaves the final outcome to a
`background.updated`, as does an update that could not be sent. What is
owed is kept apart from the session's list of jobs, so dropping old
finished jobs never loses it, and a job being cancelled is never dropped
before its cancel describes it. Until it was sent, the job keeps the
session busy for a planned restart. A client that has a job's final outcome
ignores a later snapshot of the same session saying its journal is still
open; job handles start over with each session, so that memory does too. A job
cancelled while queued, before its loop ran, also ends with a result
carrying its `review` and `landing` (`stopped before round 1`).

#### The journal

Each reviewed job writes one append-only JSONL file,
`$SCV_HOME/state/reviews/rev-<unix seconds>-<6 hex>.jsonl` (directory `0700`,
file `0600`, never through a symlink), synced after every event:
`review.started` (the job, session, builder, reviewer order, rounds, landing
mode, focus, and prompt), `builder.started` and `builder.finished`,
`reviewer.started` and `reviewer.finished` (kind `review`, `repair`, or
`confirmation`), `verdict`, `landing`, `landing_check`, and
`review.finished` (outcome, reason, round, job status, and the landing).
Every line is `{"v":1,"seq":n,"ts":<unix ms>,"event":…}` with the event's
fields; replies are bounded (builder 8 KiB, reviewer 16 KiB) and no vendor
session ID is written. A journal that cannot be created, or whose first
event cannot be written, refuses the call before its job starts. A later
write that fails stops the job at once, and is never hidden: if the outcome
was not yet decided, the review is `stopped` (`journal_error`); if it was,
such as an approval before the landing turn, that outcome stands, since the
journal already holds the verdict that earned it, and the outcome carries
`journal_incomplete` and its Review line says the journal is INCOMPLETE.
Either way the journal itself may then lack its last events, `review.finished`
included. A job cancelled while queued, or dropped with its session, still
ends its journal. Starting a review removes journals older
than 30 days. Read them with `cat`, `less`, or `jq`; `scv config show` lists
the directory. A journal without `review.finished` was cut off by a crash
or restart: its result is unknown, and nothing resumes it. To go on, the
user asks for a new reviewed call whose brief restates the open findings and
anything already landed.

#### Limits and safety

- The reviewer may read, fetch, build, and run tests, which may write build
  artifacts. SCV tells it never to edit source, commit, land, push, publish,
  deploy, or message anyone, including in the confirmation turn. That is a
  prompt rule, not a boundary SCV enforces or detects: delegated agents run
  unsandboxed as the user (see [security](security.md#delegated-runs)).
- Both roles' output is untrusted: verdicts and landing reports are parsed
  as bounded data and quoted to the other role marked as untrusted. A
  builder's report can still try to mislead its reviewer, who is told to
  check claims against the files and history.
- Approval grants nothing more: publishing still asks through `scv
  confirm`, and both roles' nested approvals get the session's unattended
  answer like any job's.
- SCV never extends the rounds. More rounds, or a fix after a failed
  confirmation, is a new reviewed call the user asks for.

### Delegate first

When a session offers the `agent` tool, the system prompt adds a *Delegating
work* section, generated from the agents actually offered (so it applies even
when `agent.system_prompt` is replaced). It names each offered agent by its
`agent` value with its product (`codex (Codex)`), adds `agent.prefer` in order
and that a call naming no agent goes to the first of them, so the main agent
names one whenever the work calls for another, any `[agents.<name>]
use_for` note (`For coding, prefer claude.`), the `model` / `effort` defaults
SCV passes when a call leaves them out, and each `hard_task_effort` to pass
for a hard task, and, when background jobs are on, asks
the main agent to stay available: answer quick things (short reads, lookups, status
checks) itself, and hand real work (changes, multi-step investigation, builds,
tests, releases, anything likely to take more than about a minute) to a
background job with a self-contained brief, then reply at once with what it
started and the job handle. It relays each `[SCV background report]`, uses
`agent_status` and `agent_cancel` when the user asks, and keeps `agent_wait`,
foreground agent calls, and long `bash` commands for quick results it needs
within the turn, since those hold the turn open and the user cannot reach it
meanwhile. If an agent declines a request and the session offers `grok`, it
tells the user what the agent said and calls the `agent` tool with agent
`grok` and the same request; a safety or guardrail refusal is not a reason to
skip Grok. If `grok` itself declines, or it is not offered, it tells the user
rather than passing the request to another agent on its own, and uses a
specific agent if the user then asks for one. Where calls can set `review`,
it adds that a review runs only when the user asks for one or accepts the
main agent's suggestion, when to suggest one (once per task), that a task
which lands, publishes, or deploys is asked about before it starts, and that
only an approved review is approved and landed work is not approved work
(see [Reviewed jobs](#reviewed-jobs)). The wording explains why rather than
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
SCV process that owns it, so a reused PID never matches. Process liveness
checks use that identity and treat an exited zombie as stopped. A run that serves a
conversation also records the conversation and its current turn, and a live
agent (a nested SCV or an ACP agent) that waits between turns records when
its last turn ended (`idle_since_unix`), until its next turn starts. A nested
SCV also records how many of its own background jobs still run or wait to be
reported to its model (`background_jobs`, omitted when none).

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

`scv status` counts the runs at work as `running` (a nested SCV whose own
background jobs still count among them) and the live agents waiting between
turns as `idle`, as `scv agents ps` lists them, and shows how many
orphans the daemon has stopped. The `agent` tool is offered only while the
session's own depth is below `agent.max_delegation_depth` (default 2), and a
delegated run may not start, stop, restart, update, or run a daemon, or manage
channels. This is cooperative: see [Delegated runs](security.md#delegated-runs).

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
carries the session workspace. The summary starts with the agent that runs the
call, as in `agent codex: Launch /home/u/.local/bin/codex with args […]`, and
the risk is always `delegate`. Project configuration cannot replace the
executable or arguments.

Adapter processes use instance-private state directories under
`$SCV_HOME/agents/<name>`. In particular, `codex` receives
`CODEX_HOME=$SCV_HOME/agents/codex` and does not read the user's normal
`~/.codex` state, `claude` does not read `~/.claude`, and Grok, DeepSeek
Harness, and pi never read `~/.grok`, `~/.dsh`, or `~/.pi`.

### Nested SCV (`scv`)

The `scv` agent is another SCV: a separate session in SCV's private
home `$SCV_HOME/agents/scv`, with its own context, instructions, and tools.
Unlike the CLI adapters, which start one process per turn, it keeps one
`scv server --stdio` running for a whole conversation and speaks the
[client protocol](protocol.md) to it:

```text
initialize (v3) → session.start {cwd, delegation_depth: parent + 1} → turn.start per call
```

- It takes `session` and `model` (a new conversation only, sent as the
  session's model override), but no `effort`, which the `agent` tool refuses
  for it before anything starts.
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
- The nested SCV's own session can run [background jobs](#background-jobs),
  which outlive the call that started them and which it reports in turns it
  starts itself. Between calls SCV keeps reading its events: it denies those
  turns' approval requests, since no call is there to carry them to a person,
  and counts the jobs that still run or wait to be reported, from the call
  that started each (`tool.completed.jobs`) until the nested model has seen
  its result, through a later call or a report turn, which counts until it
  ends; a report turn that failed and will be tried again
  (`origin.retry_seconds`) leaves its jobs counting, and so does a reviewed
  job whose cancel said its journal is still open, until its
  `background.updated`. Only job handles and
  statuses count, so a nested SCV 0.3.0, whose job
  changes name one tool per agent (`agent_codex`), is followed the same way.
  A planned restart waits for them as for a running turn (see
  [architecture](architecture.md#planned-restarts)).
- The nested SCV is recorded like any delegation, so `scv agents ps` lists it
  with its current turn (`running` during a turn, `idle` between turns, or
  `background` while its own jobs count), `scv agents kill` stops it, and the
  orphan reconcile reaps it if its parent dies. It ends when its conversation
  is forgotten, expires, or its session ends: SCV closes its stdin (the server
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

The `scv` agent needs the `scv` executable (searched on `PATH` and in
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
| `claude` | `claude-agent-acp` | npm `@agentclientprotocol/claude-agent-acp` (ACP organisation; Zed's `claude-code-acp` is deprecated in its favour) |
| `codex` | `codex-acp` | npm `@agentclientprotocol/codex-acp` (ACP organisation) |
| `grok` | `grok agent stdio` | built in |
| `dsh` | `dsh --profile acp` | built in (0.1.7-rc.1) |

pi has only a community adapter and stays on one process per turn, and the
`scv` agent speaks SCV's own protocol. `[agents.<name>] transport`
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

`initialize` records whether the server advertises
`_meta.steering.supported`. Steering a running turn (see
[Busy conversations](#busy-conversations)) sends the request
`_session/steering {sessionId, prompt: [{type: "text", text}]}` while the
turn's `session/prompt` is still unanswered. The turn's reader keeps
handling updates and permission requests and takes the request's answer: a
result other than `{"accepted": false}` means the prompt went into the turn,
while an error, `{"accepted": false}`, or the turn ending first means it went
nowhere. Between turns SCV sends no steering request.

- An agent over ACP always takes `session`: the first call returns a handle
  such as `claude-1`, and passing it sends the next prompt to the same ACP
  session on the same server. A conversation keeps its `cwd`, runs one turn at
  a time, and follows `agent.max_conversations` and
  `agent.conversation_idle_seconds`.
- `model` and `effort`, for an agent whose adapter offers them (DeepSeek
  Harness offers both over ACP only, since its headless CLI takes neither),
  become `session/set_config_option` on the session's `model` and
  `effort`/`reasoning_effort` options, at any turn, the model first, since
  choosing a model may reset its effort. The user's
  defaults are set this way when a conversation starts; a later turn sets
  only what it names, so the session keeps the rest. A value the
  agent does not offer fails the call with the offered list and keeps the
  conversation. Each new session's lists are saved and shown to later
  sessions (see [Model and effort values](#model-and-effort-values)).
- `session/update` notifications become `tool.progress`: completed lines of
  the agent's message, each tool call's title (or kind), `… failed` for a
  failed tool call, and the current plan step. Thoughts and tool output never
  become progress, and titles are redacted like the other adapters' lines.
- The agent's `session/request_permission` goes through the calling session's
  approval gate as `agent` with the summary `[claude-1 acp] <title>
  (<kind>)`, whose handle names the agent. The risk follows SCV's own tools: `read`, `search`, and `think`
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
scv agents check                        # does each one work as SCV runs it?
scv agents logout claude
```

They work from any directory. For Claude Code, Codex, and Grok, `login` and
`logout` run the agent's own command with exactly the private home and cleaned
environment that the daemon's delegated runs of it use, with the terminal attached
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
and a `dsh` result names `scv agents login dsh`.

DeepSeek Harness reaches other providers, such as an OpenAI-compatible
gateway, through its own routes: `providers` of its `@deepseek-ai/dsh-llm-pi-ai`
plugin in a patch file in its private home
(`$SCV_HOME/agents/dsh/.dsh/cordis.patch.yml`), each with the name of a key
in its credential file (`apiKeyEnv`), its protocol (`api`), `baseURL`, and
`models`. Its ACP server then lists those models too, as `<route>/<model>`,
for `[agents.dsh] model` and the `agent` tool's `model`. Version 0.1.7-rc.1
sends a header named `session_id` on a route that speaks the Responses API
(`openai-responses`) and lets no route turn it off, so a gateway whose front
end refuses header names with an underscore fails every such request (behind
Cloudflare, with HTTP 520).

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
`settings.json`, so a call to `pi` without `model` uses it and `model` can name
`scv/<id>`. Other providers and settings in those files are preserved.
`status` shows the default provider, API, endpoint host, and model, and which
providers have stored sign-ins; `logout` removes `auth.json`, the `scv`
provider, and a default that points at it.

`scv agents import scv` gives the nested SCV behind the `scv` agent a copy of
SCV's own active provider: `$SCV_HOME/agents/scv/config.toml` (mode `0600`,
written atomically) gets `[provider] active = "scv"` and a `[providers.scv]`
profile with the same kind, wire API, model, base URL, timeout, and headers,
plus `[web] search = "provider"` when SCV's own config uses hosted search;
the provider's `reasoning_effort` is SCV's own and is not copied. The
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

### Checking delegated agents

`scv agents check [<agent>] [--timeout-seconds N]` checks each installed
agent the way SCV runs it, or only the one named, and costs one short model
turn per agent. It reads only the user's configuration and prints, per agent:

- how SCV reaches it: over ACP (the server's path), through its CLI once per
  turn, or as a nested SCV. An agent that is not installed is only noted,
  unless it was named;
- the first line of its CLI's `--version`, run in its private home;
- the models and efforts its ACP server offers, as that call just saved them
  (or, if the call failed first, as an earlier session did, and when), or
  that its CLI lists none;
- the user's `hard_task_effort`, if set, which the call does not use. When
  the saved efforts lack it, the line says so and names the setting. Efforts
  depend on the model, so this is a warning and does not fail the check;
- one `agent` call, made through the same tool and backend a session uses,
  from the current directory, in the foreground. It sends `SCV is checking
  that it can reach you. Reply with exactly: ok` with the user's configured
  `[agents.<name>] model` and `effort`, if any, and times out after
  `--timeout-seconds` (default 180, 10 to 3600). The saved list does not
  refuse the model here. The agent's own list decides, so a check can
  refresh a list that no longer matches the agent. The line reports `ok`
  with the time taken and the reply, or the status and the error, and the
  sign-in hint when there is one. Replies, errors, and versions come from
  the agent, so they are printed on one line without control characters,
  cut to 300 characters.

Each call is recorded under the session `agents-check-<pid>` like any
delegation, so `scv agents ps` lists it while it runs, and the daemon stops
anything the check leaves behind if it dies. A call returns only once its
run's record is gone, that is once the agent's whole process group has
stopped. The command gives the agent 10 seconds to exit after its input
closes, then kills what is left.

For example:

```text
claude (Claude Code)
  reached   over ACP: /home/me/.local/bin/claude-agent-acp
  version   2.1.283 (Claude Code)
  models    opus[1m], claude-fable-5-1[1m], sonnet, haiku
  efforts   low, medium, high, xhigh, max
  hard task effort max
  call      ok in 3.7s with your configured model opus[1m] and effort xhigh: "ok"
```

Ctrl-C at any point cancels the running call or version probe, stops its
agent, and ends the check without calling the rest. The command exits
non-zero when any call fails, a named agent is missing, or it was
interrupted. Run
it after an agent updates, or when a delegated call fails in a way that
looks like SCV's fault. Arguments the CLI no longer accepts, or output SCV
cannot read, show up here first. Fixing those means changing the agent's
descriptor in `scv_tools::adapters`.

Scripted backends behind the `agent` tool cover its choice of agent (an
explicit `agent`, the `prefer` default, the agent a `session` handle names,
and a handle that disagrees with `agent`), refusals before anything launches
(an option the agent does not take, an agent the session does not offer, a
call that names none without a default), the schema's enum, options, and
`required`, each agent's line, the `fallback` and Grok notes, a background job
started through it, and approval summaries that name the agent.
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
bash stand-in for `scv server --stdio` covers `scv` conversations on one
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
The same stand-in shows a session saving what it offers, and later tools
listing those values and refusing an unlisted model before they start. It
also shows a tool going by a newer list saved elsewhere, and a check call
that passes an unlisted model through. It shows a check call returning only
after the server and a process it left behind are gone. Unit tests cover
reading, sanitizing, and invalidating the saved file. A fake `dsh` covers
`scv agents check` end to end, both a passing call and a signed-out one.

## Tool extension contract

A `Tool` supplies a unique name, description, JSON Schema, declared `ToolRisk`,
approval summary, and asynchronous executor. Stable risk values are
`read_only`, `filesystem`, `process`, `delegate`, and `network`. Registration rejects
duplicate names. The loop and TUI do not contain name-specific execution code;
the TUI only labels an `agent` call with the agent it names, such as
`agent codex`.

## `read_skill`

```json
{"name":"release-checks"}
```

At session start, the server discovers at most `skills.max_skills` valid skill
directories and builds a name-to-canonical-path map. `read_skill` accepts only a
name from that map, revalidates containment under its original project or user
skill root, and returns at most `skills.max_skill_bytes` of `SKILL.md`. It does
not accept a path and cannot be used as a general out-of-workspace read.
Tool-enabled sessions also map SCV's built-in skills, whose text is compiled
into the binary. There is one, `delegating` (see
[Choosing an agent](#choosing-an-agent)). A skill file of the same name in
`skills.project_dir` or `skills.user_dir` replaces a built-in one. A
workspace-root project skill of the same name does not, and is not listed:
it is meant for agents working in that project, not for SCV's main agent.
Built-in skills do not count toward `skills.max_skills`. They are listed only
in sessions that offer the `agent` tool.
A tool-free session, which cannot call `read_skill`, discovers and lists no
skills at all, so a model reading a chat stranger's message or a mail does
not learn what the owner has installed.
Tool-enabled sessions also map workspace project skills (`.agents/skills` and
`.claude/skills` of the workspace and its child projects) under
`<project>:<name>`, listed separately with the instruction to delegate to that
project with the `agent` tool and `cwd`; see
[Project skills](configuration.md#project-skills). Skill
metadata and content remain untrusted instructions.
