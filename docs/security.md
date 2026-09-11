# Security Model

Status: final design for v0.1

SCV is a local agent running with the user's operating-system account. v0.1
provides workspace path containment, bounded I/O, transparent side effects, and
interactive approval. It does not provide an OS security boundary.

## Trust boundaries

- Model output, tool arguments, project files, project configuration, skill
  text, command output, and delegated-agent output are untrusted.
- User configuration and an approval response express policy but are not
  additional filesystem or process isolation.
- Provider credentials are secrets. They remain server-side and are never sent
  to the TUI except indirectly to the configured provider endpoint.
- Project configuration cannot select the provider endpoint, credential
  variable, user skill root, or native-agent executable/arguments. Those values
  require a user, explicit-config, environment, or CLI layer.

## Tool rules

`read` and `write` operate through an open workspace directory capability, so
path resolution, directory creation, file opening, and rename remain confined
under that root even if symlinks are changed concurrently. Both reject absolute
paths and parent traversal. Lexically secret-like read paths such as `.env`,
private keys, and credential files require approval, but this heuristic is not
a confidentiality boundary. Writes use a capability-contained temporary file
plus an atomic install in the destination directory and may require an expected
SHA-256 when replacing a file. The hash detects stale content observed before
the write; it does not lock out an external concurrent writer. Both tools cap
output or input according to configuration.

`bash` runs the configured command through `/bin/bash -lc` with the session
workspace as its current directory. It inherits the SCV process environment,
runs with the user's full permissions, and is not sandboxed. It requires
approval under the default policy, has a wall-clock timeout, and bounds combined
stdout/stderr. SCV starts it in a new process group; cancellation or timeout
sends a group-wide termination signal and ends with `KILL` if any member
remains. Cancellation permits up to two seconds of graceful cleanup; reaching
the execution deadline kills immediately. Descendants and retained output pipes
cannot extend the call without bound.

Each `agent_*` tool launches only its configured adapter. It uses an executable
and argument vector without shell interpolation, appends the model-provided
prompt as one argument, uses the workspace as current directory, applies the
same timeout/output bounds, kills the process group on cancellation, and
requires approval. The delegated CLI has the user's full permissions and may
implement its own tools and approvals.

## Approval behavior

An approval prompt includes the exact tool name and a bounded, human-readable
summary. Approval is per call. A denial is returned to the model as a failed
tool result. Cancellation denies and terminates pending work.

The server, not the TUI, decides whether approval is required. This prevents a
custom client from bypassing policy. `never` means deny side effects; it does
not mean silently execute them.

## Network and protocol

The only v0.1 network client is the configured model provider. Project
configuration cannot provide inline credentials. Stdio is the only server
transport, avoiding a listening socket and remote authentication surface.

Protocol lines, tool arguments, tool output, and provider responses are size
bounded. Malformed messages fail closed. Diagnostics are separated from the
protocol stream and redact authorization headers and credential values.

Provider streaming is capped at a 1 MiB SSE event, 4 MiB total response, 1 MiB
assistant text, 256 KiB per tool argument object, and 32 tool calls per model
response. A limit violation cancels the response and fails the turn before any
not-yet-started call from that response is executed.

## Responsible operation

Users should run SCV in a version-controlled workspace, inspect approvals, and
use operating-system sandboxing or a container when executing untrusted
repositories. Installing a skill or configuring a delegate does not make it
safe; these inputs can influence a model or launch software with user authority.

Security reports follow the private process in `SECURITY.md`; public issues
should not contain unreleased vulnerability details.

User configuration may contain provider credentials and must be mode 0600 on Unix. Project configuration cannot select provider profiles, endpoints, headers, or credentials.
