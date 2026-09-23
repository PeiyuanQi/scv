# Built-in Tools

Status: final design for v0.1

All tool inputs are validated JSON objects. Tools execute serially and receive a
cancellation token, workspace root, timeout, and output limits from the server.

## `read`

```json
{"path":"src/main.rs","offset":0,"limit":65536}
```

`path` is a required workspace-relative UTF-8 path. `offset` and `limit` are
optional byte values. The result reports the selected content, total byte
length, and whether it was truncated. The selected byte range must be UTF-8 or
the tool returns a clear error in v0.1. Workspace reads are read-only risk;
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

## Native agent adapters

SCV knows five agent CLIs: Claude Code (`agent_claude`), Codex
(`agent_codex`), Grok Build (`agent_grok`), DeepSeek Harness (`agent_dsh`),
and pi (`agent_pi`). Each is one descriptor in `scv_tools::adapters` holding
its default command line, where its state lives inside the private home, the
variables it must not inherit, and how it signs in; adding an agent is one
more entry. A session offers only the agents whose executable resolves when
the session starts, so an agent installed later appears in new sessions.
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
`provider/id` for `agent_pi`) and tell the model to set them only when the user asks,
so an omitted value leaves the agent's own configured default in place. A
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
The model cannot supply other flags or a different executable. Output, timeout,
cancellation, and process-group behavior match `bash`. Adapter execution has
delegate risk because the child agent may independently read, write, run
commands, access inherited credentials, or ask its own model provider.

The default invocation contracts are:

| Tool | Invocation |
| --- | --- |
| `agent_claude` | `claude -p [--model <model>] [--effort <effort>] <prompt>` |
| `agent_codex` | `codex exec [-m <model>] [-c model_reasoning_effort="<effort>"] <prompt>` |
| `agent_grok` | `grok [-m <model>] [--reasoning-effort <effort>] -p <prompt>` |
| `agent_dsh` | `dsh --profile headless <prompt>` |
| `agent_pi` | `pi -p [--model <model>] [--thinking <effort>] <prompt>` |

Grok's `-p` and pi's `-p` run one prompt and exit. DeepSeek Harness takes its
model from its profile, so `agent_dsh` offers no `model` or `effort`.

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
`$SCV_HOME/adapters/<name>`. In particular, `agent_codex` receives
`CODEX_HOME=$SCV_HOME/adapters/codex` and does not read the user's normal
`~/.codex` state, `agent_claude` does not read `~/.claude`, and Grok, DeepSeek
Harness, and pi never read `~/.grok`, `~/.dsh`, or `~/.pi`.

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
credentials under `$SCV_HOME/adapters/<name>` (mode `0700`). Claude Code and
Codex report their own `status`; for the others SCV reads the credential file
and reports only whether one is stored, never its value.

DeepSeek Harness signs in with an API key only. `scv agents login dsh` reads
it without echo, or from stdin when stdin is not a terminal, and writes it as
`refs.DEEPSEEK_API_KEY` in `.dsh/.credentials.yaml`, DeepSeek Harness's own
credential file, atomically with mode `0600`. `logout` removes that file. A key
is never accepted as an argument.

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
CLIs in CI. Fake SCV homes cover the DeepSeek Harness key file, pi's endpoint
files and import, and that no sign-in output contains a key. Shared process-runner tests cover
output limits, timeout, cancellation, and background-descendant cleanup.

## Tool extension contract

A `Tool` supplies a unique name, description, JSON Schema, declared `ToolRisk`,
approval summary, and asynchronous executor. Stable risk values are
`read_only`, `filesystem`, `process`, and `delegate`. Registration rejects
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
