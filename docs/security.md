# Security Model

Status: final design for v0.1

SCV is a local agent running with the user's operating-system account. v0.1
provides workspace path containment, bounded I/O, transparent side effects, and
interactive approval. It does not provide an OS security boundary.

The daemon remains a user-level process. `scv start --allow-sudo` asks sudo to
authenticate the current user's existing policy so later approved commands can
use the user's sudo credential cache; it cannot add the user to sudoers, grant
new privileges, or run the daemon as root. Without verified sudo authorization,
interactive starts ask whether to continue and non-interactive starts fail.

## Trust boundaries

- Model output, tool arguments, project files, project configuration, skill
  text, command output, and delegated-agent output are untrusted.
- User configuration and an approval response express policy but are not
  additional filesystem or process isolation.
- Each SCV instance is isolated by its `SCV_HOME` root. Its socket, service
  unit, configuration, skills, credentials, ClawBot state, and nested-agent
  state remain within that profile; custom homes do not fall back to `~/.scv`.
- Provider credentials are secrets. They remain server-side and are sent only
  to the configured provider endpoint, never to the TUI.
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
requires approval. SCV supplies an instance-private `HOME`, `SCV_HOME`, XDG
directories, and `CODEX_HOME` for Codex, while removing SCV selector variables,
provider API-key variables, `CLAUDE_CODE_OAUTH_TOKEN`, and `CLAUDE_CONFIG_DIR`
from the child environment. Agents sign in only through `scv agents login`,
which stores the agent's own credentials in that private home, or through
`scv agents import codex`. The import copies the user's Codex `config.toml`,
plus `auth.json` only when it holds a static API key; it never copies a ChatGPT
refresh token. Delegated runs therefore never share the user's personal Claude
Code or Codex session. The delegated CLI still has the user's operating
system permissions and may implement its own tools and approvals, but it cannot
silently reuse the user's normal Codex state.

## Approval behavior

An approval prompt includes the exact tool name and a bounded, human-readable
summary. Approval is per call. A denial is returned to the model as a failed
tool result. Cancellation denies and terminates pending work.

The server, not the TUI, decides whether approval is required. This prevents a
custom client from bypassing policy. `never` means deny side effects; it does
not mean silently execute them.

## Network and protocol

Outbound network clients include the configured model provider and the ClawBot
iLink adapter. The updater delegates registry downloads to Cargo. Project
configuration cannot provide inline credentials or redirect these authorities.

The normal server transport is a local Unix socket at `$SCV_HOME/server.sock`
(default `~/.scv/server.sock`) with mode `0600`. It is a trusted local-user
interface, including daemon management; it is not a remotely authenticated
network service. A process running as the same user can access that authority.
The stdio endpoint remains available for one-shot local clients and does not
support component management. Both transports require the versioned handshake.

Protocol lines, tool arguments, tool output, and provider responses are size
bounded. Malformed messages fail closed. Diagnostics are separated from the
protocol stream and redact authorization headers and credential values.

Provider streaming is capped at a 1 MiB SSE event, 4 MiB total response, 1 MiB
assistant text, 256 KiB per tool argument object, and 32 tool calls per model
response. A limit violation cancels the response and fails the turn before any
not-yet-started call from that response is executed.

## Supervised remote bridge

QR login is explicit. Saved ClawBot accounts are enabled by default and start
under the daemon; login honors a saved opt-out. To opt out before daemon startup,
set `enabled: false` in the private per-account settings file. Credentials,
delivery state, and settings under `$SCV_HOME/clawbot/{accounts,state,settings}`
use mode `0600`, atomic writes, and mode `0700` parent directories. Project
configuration cannot choose bridge accounts, workspaces, or remote authority.

By default, remote sessions request `no_tools: true`, enforced by the server,
and the bridge denies any approval request, so remote messages do not authorize
filesystem, shell, or delegated-agent tools. An account's `remote_tools =
"owner"` setting, changeable only through local CLI or daemon control, grants
the authenticated account owner (the iLink `user_id` from QR login) full tools
with every approval request auto-approved. That makes the owner's WeChat
account equivalent to local shell access as the daemon user: anyone who can
send messages from it can read and change files, run commands, and launch
delegated agents without confirmation. Other senders, the owner's messages in
group chats, and accounts without a known owner ID stay tool-free; group
conversations never share the owner's direct-chat session. Logout clears the
grant before deleting credentials. Owner replies are ordinary assistant output and may quote
tool results the model chose to include; bridge failure details remain
sanitized. Bridge failures produce short sanitized
replies; raw diagnostics, credentials, tool output, and host paths are not
forwarded as diagnostics to WeChat. Status reports identity and live health,
with sanitized errors and successful-contact timestamps, never bearer tokens.
Saved credentials alone do not establish connectivity.

Delivery state is bound to a SHA-256 fingerprint of the normalized API origin
and authenticated bot/user IDs. Known-identity token rotation preserves state;
credentials without both IDs use a conservative token-based fingerprint.
Legacy unbound state is bound before first use. A mismatch prevents polling,
recovery, and delivery. Login refuses identity/origin replacement, including
legacy-to-identified replacement, until explicit logout discards the old state.
This prevents pending replies from leaking into a different account or origin.

A lifetime account lock excludes cooperating runners. A separate transaction
file lock serializes login, settings/state writes, binding, migration, and
removal; state writes recheck identity binding. Credentials and settings are
read together with `account_snapshot`. Both locks are nonblocking and no network
I/O holds the transaction lock. Lock files remain after logout to preserve lock
identity. Old `0.1.9` standalone bridges do not honor these locks and must be
stopped before enabling supervised accounts.

An inbound message is durably claimed before submitting a turn. Recovery does
not replay interrupted claimed work; pending replies retain their client IDs
across retries. This protects against duplicate execution without promising
exactly-once delivery by the remote service.

Poll batches above 4096 messages fail before execution or cursor advancement.
Response bodies are capped at 4 MiB and durable string message IDs at 256 bytes.
The 4096-ID deduplication window refreshes IDs encountered again so the
processed batch remains retained until its cursor checkpoint.

The server supervisor joins an old account instance before starting its
replacement with updated credentials or settings. Logout requires a live daemon,
persists disablement, and joins the component before deleting credentials, delivery state, and
settings. Local logout does not revoke the token remotely. SIGTERM and Ctrl+C
cancel and join supervised components and tracked sessions with bounded cleanup.
Session writer and turn tasks remain tracked through forced handler aborts and
are joined before shutdown completes. Periodic reconciliation observes
cancellation separately from socket acceptance, and the component management
lock is released before response writes so a slow reader cannot hold it.
All future long-running integrations must use this same lifecycle.

## Responsible operation

Users should run SCV in a version-controlled workspace, inspect approvals, and
use operating-system sandboxing or a container when executing untrusted
repositories. Installing a skill or configuring a delegate does not make it
safe; these inputs can influence a model or launch software with user authority.

Security reports follow the private process in `SECURITY.md`; public issues
should not contain unreleased vulnerability details.

User configuration may contain provider credentials and must be mode 0600 on Unix. Project configuration cannot select provider profiles, endpoints, headers, or credentials.
