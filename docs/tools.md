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

`command` is passed to `/bin/bash -lc` in the workspace. The optional timeout may
only reduce the configured maximum. The result contains exit status and bounded
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
{"prompt":"Review the error handling in this workspace.","timeout_seconds":120}
```

Each tool resolves only its configured executable and fixed argument vector,
appends the prompt as one argument, and starts it directly in the workspace.
The model cannot supply flags or a different executable. Output, timeout,
cancellation, and process-group behavior match `bash`. Adapter execution has
delegate risk because the child agent may independently read, write, run
commands, access inherited credentials, or ask its own model provider.

The default invocation contracts are:

| Tool | Invocation |
| --- | --- |
| `agent_claude` | `claude -p <prompt>` |
| `agent_codex` | `codex exec <prompt>` |
| `agent_pi` | `pi -p <prompt>` |

Before approval, SCV resolves the executable through the server environment
and displays its absolute path, full fixed argument vector, bounded prompt,
workspace, and delegate-risk warning. Project configuration cannot replace the
executable or arguments.

A fake executable verifies native-agent argument boundaries and workspace
selection without requiring these CLIs in CI. Shared process-runner tests cover
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
not accept a path and cannot be used as a general out-of-workspace read. Skill
metadata and content remain untrusted instructions.
