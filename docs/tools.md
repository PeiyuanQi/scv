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
{"command":"cargo test --workspace","timeout_seconds":120}
```

`command` is passed to `/bin/bash -lc` in the workspace. Without
`timeout_seconds` a call gets `tools.command_timeout_seconds` (default 120).
A call may choose any timeout up to `tools.max_timeout_seconds` (default 1800),
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

The tools `agent_claude`, `agent_codex`, and `agent_pi` share this schema:

```json
{"prompt":"Land the fix with the feature-flow skill.","cwd":"scv","timeout_seconds":1800,"model":"sonnet","effort":"medium"}
```

`cwd` is optional: a directory inside the workspace, relative (such as a
project directory) or absolute. It resolves, following symlinks, when the call
runs and must be an existing directory under the workspace; otherwise the call
fails without launching anything. Without it the agent runs in the workspace
root. Running in a project directory is how a delegated agent picks up that
project's `AGENTS.md` or `CLAUDE.md` and its skills (`.agents/skills` for
Codex, `.claude/skills` for Claude Code), exactly as when the user starts the
CLI there. `timeout_seconds` defaults to `tools.agent_timeout_seconds`
(default 600) and may be raised up to `tools.max_timeout_seconds` (default
1800) for long work such as builds, releases, or landing a change.

`model` and `effort` are optional and offered only when the adapter configures
`model_args` or `effort_args`. Their schema descriptions name the adapter's
model family (Claude aliases such as `sonnet` for `agent_claude`, OpenAI model
IDs for `agent_codex`) and tell the model to set them only when the user asks,
so an omitted value leaves the agent's own configured default in place. A
blank `cwd`, `model`, or `effort` counts as omitted, since models often send
`""` for an optional field they mean to leave unset. A model is 1-128 ASCII letters, digits, or
`._:/@[]-` and cannot start with `-` or `@`; an effort is `low`, `medium`, `high`,
`xhigh`, or `max`. Each selected value becomes one substituted argument, never
shell text, so the CLI itself reports values it does not support.

Each tool resolves only its configured executable and fixed argument vector,
adds any selected model/effort arguments, appends the prompt as one argument,
and starts it directly in the workspace or the selected `cwd`. A prompt cannot start with `-`, so it
is never read as a flag.
Native adapters receive an instance-private `HOME`, `SCV_HOME`, and XDG
configuration/data/state directory, plus `CODEX_HOME` for Codex. SCV selector
variables, provider API-key variables, `CLAUDE_CODE_OAUTH_TOKEN`, and
`CLAUDE_CONFIG_DIR` are removed so a nested agent cannot reuse the parent
instance's configuration or credentials from outside its private home. The `bash` tool retains the normal inherited
environment for compatibility.
The model cannot supply other flags or a different executable. Output, timeout,
cancellation, and process-group behavior match `bash`. Adapter execution has
delegate risk because the child agent may independently read, write, run
commands, access inherited credentials, or ask its own model provider.

The default invocation contracts are:

| Tool | Invocation |
| --- | --- |
| `agent_claude` | `claude -p [--model <model>] [--effort <effort>] <prompt>` |
| `agent_codex` | `codex exec [-m <model>] [-c model_reasoning_effort="<effort>"] <prompt>` |
| `agent_pi` | `pi -p <prompt>` |

Before approval, SCV resolves the executable through the server environment
and displays its absolute path, full argument vector, bounded prompt, requested
directory, timeout, and delegate-risk warning; the approval request also
carries the session workspace. Project configuration cannot replace the
executable or arguments.

Adapter processes use instance-private state directories under
`$SCV_HOME/adapters/<name>`. In particular, `agent_codex` receives
`CODEX_HOME=$SCV_HOME/adapters/codex` and does not read the user's normal
`~/.codex` state, and `agent_claude` does not read `~/.claude`.

### Signing in delegated agents

Each adapter keeps its own sign-in in its private home, separate from the
user's personal login, so token refreshes by one never invalidate the other.
Sign the agents in once on the SCV host:

```sh
scv agents login claude                 # claude auth login
scv agents login codex                  # codex login
scv agents login codex -- --device-auth # extra arguments after --
scv agents import codex                 # or copy your own Codex setup
scv agents status                       # both agents' sign-in state
scv agents logout claude
```

They work from any directory. `login`, `status`, and `logout` run the agent's
own command with exactly the private home and cleaned environment that the
daemon's `agent_*` tool uses, with the terminal attached for browser or
device-code flows. The agent CLI itself writes those credentials under
`$SCV_HOME/adapters/<name>` (mode `0700`).

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
escapes, and the timeout ceiling without requiring these CLIs in CI. Shared process-runner tests cover
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
