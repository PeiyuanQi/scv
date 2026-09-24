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
  unit, configuration, skills, credentials, channel state, and nested-agent
  state remain within that profile; custom homes do not fall back to `~/.scv`.
- Provider credentials are secrets. They remain server-side and are sent only
  to the configured provider endpoint, never to the TUI.
- Channel tokens, Feishu app secrets, and delivery state are secrets. Inbound
  sender IDs, cursors, and message content are untrusted remote input and are
  kept out of logs.
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
stdout/stderr. A call may choose its timeout only up to the configured
`tools.max_timeout_seconds` ceiling, which project configuration can lower but
not raise. SCV starts it in a new process group; cancellation or timeout
sends a group-wide termination signal and ends with `KILL` if any member
remains. Cancellation permits up to two seconds of graceful cleanup; reaching
the execution deadline kills immediately. Descendants and retained output pipes
cannot extend the call without bound.

Each `agent_*` tool launches only its configured adapter. It uses an executable
and argument vector without shell interpolation, appends the model-provided
prompt as one argument, uses the workspace or a directory inside it as current
directory, applies the same timeout/output bounds, kills the process group on
cancellation, and requires approval. A per-call `cwd` is resolved with symlinks
followed at launch and must remain an existing directory under the canonical
workspace; `..`, absolute paths elsewhere, and links pointing outside are
refused. The nested agent loads that directory's own instructions (such as
`AGENTS.md` or `CLAUDE.md`) and project skills, which are untrusted repository
content just like any file the agent reads there; choosing a directory never
widens what the agent could already reach with the user's permissions. SCV supplies an instance-private `HOME`, `SCV_HOME`, XDG
directories, and the agent's own state variable (`CODEX_HOME`, `GROK_HOME`,
`DSH_HOME`, or `PI_CODING_AGENT_DIR`) inside that home. It first removes from
the child environment SCV's selector variables, every variable ending in
`_API_KEY`, and the credential, endpoint, and state variables that any adapter
declares (such as `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`,
`OPENAI_BASE_URL`, `GROK_*`, `DSH_*`, and `PI_*`), so no agent inherits the
user's or another agent's credentials. Agents sign in only through
`scv agents login`, which runs the agent's own sign-in in that private home or,
for DeepSeek Harness and pi endpoints, reads an API key without echo (or from
stdin) and writes it into the agent's own credential file with mode `0600`;
keys are never command-line arguments and never printed. `scv agents import
codex` copies the user's Codex `config.toml`, plus `auth.json` only when it
holds a static API key; it never copies a ChatGPT refresh token. `scv agents
import pi --from-scv-provider` copies SCV's own provider endpoint and key into
pi's private files only on that explicit command. Delegated runs therefore
never share the user's personal Claude Code, Codex, Grok, DeepSeek Harness, or
pi session. The delegated CLI still has the user's operating system
permissions and may implement its own tools and approvals, but it cannot
silently reuse the user's normal agent state.

`[agents.<name>] permissions = "full"` is the user's explicit opt-in to hand
a delegated CLI its own full-autonomy switches (for example Claude Code's
`--permission-mode bypassPermissions` or Codex's
`--dangerously-bypass-approvals-and-sandbox`), turning off that CLI's approval
prompts and sandbox and enabling web search where the CLI gates it. It is off
by default, only user-level configuration can set it, and every approval
summary for such an agent states `FULL PERMISSIONS`. Combined with a channel's
owner tools, it lets the owner's WeChat or Feishu account run unattended
development work, equivalent to the owner running those agents unprompted in a terminal.

A session offers only agents whose executable resolves. SCV looks in the
user's per-user install directories (`~/.local/bin`, and `~/.grok/bin` for
Grok) before `PATH`, as a login shell does; anything able to write those
directories can already run code as the user.

Project skill discovery reads only `SKILL.md` files that resolve inside the
workspace, bounded in count and size, and only for tool-enabled sessions, so a
tool-free remote sender never learns the workspace's project or skill names.
Listed skills are untrusted instructions, like `.scv/skills`.

### Delegated runs

SCV tags each delegated process through its environment (`SCV_PARENT`,
`SCV_DELEGATION_DEPTH`), records it under `$SCV_HOME/state/delegations` while it
runs, stops its process group and tagged descendants when it ends, and has the
daemon stop orphans whose owning SCV process died. This cleanup is
cooperative. Delegated agents run as the user, unsandboxed, so one that
deliberately clears its environment, deletes its record, leaves its process
group or cgroup, or asks another service (such as `systemd-run --user`, a tmux
server, or cron) to start a process for it escapes the bookkeeping; the
mechanism exists to clean up accidental leaks, not to contain a hostile agent.
Records are private files, but a same-user process can still edit them. Before
killing, SCV checks a recorded process's PID and start time so a reused PID is
never signalled, and a process group only when its leader matches or has
exited.

Delegation depth is bounded by `agent.max_delegation_depth`, and at any depth
above zero the `scv` CLI refuses to run, start, stop, restart, or update a
daemon or manage channels, so an SCV started by a delegated agent cannot manage
its parent. The one exception is `scv restart --when-idle`, which only asks
the daemon to restart into the release already installed at its own path: the
daemon decides when, refuses a binary that does not answer `scv build-info`,
and keeps the previous binary for a rollback. It gives a delegated agent
nothing it could not already do as the user (such as `systemctl --user
restart`); it exists so a chat-driven release restarts after its report. Conversation handles belong to one SCV session and are checked
against it, so the model cannot reach another session's conversation or pass a
CLI session ID of its choosing; a continued conversation keeps its original
`cwd`. `scv agents gc` removes only regular transcript files below each
agent home's transcript directory, never follows symlinks, and keeps those
of live conversations and anything written in the last hour.
`scv agents status` prints a summary of Claude Code's and Codex's
own status output, never the account email or key fragment that output
contains.

An agent reached over the Agent Client Protocol (`claude-agent-acp`,
`codex-acp`, `grok agent stdio`, `dsh --profile acp`) is the same agent with
the same private home, environment, and trust; ACP changes only how SCV talks
to it. SCV declares no client file-system or terminal capabilities and refuses
every `fs/*`, `terminal/*`, or other agent-to-client request, so the agent
cannot use SCV to read, write, or run anything. The agent's own
`session/request_permission` requests go through the calling session's
approval gate (labelled `[claude-1 acp]`) with a risk derived from the ACP tool
kind in the same way as SCV's own tools: reads are read-only unless they touch
a secret-like path, edits are file-system work, commands are processes, and
fetches are network. Under the default `on-risk` policy a relayed read is
therefore approved without asking, like SCV's own `read`. A denial selects the
agent's reject option, and a relay without a gate rejects. `permissions =
"full"` still turns the agent's own prompts off, through its own session mode
or flag, so nothing is relayed then. Progress lines carry tool titles, never
tool output or the agent's thoughts, and are redacted.

A nested SCV (`agent_scv`) is a delegated agent like the others and adds no
new trust: it runs as the user, unsandboxed, in its private home, one
delegation level deeper. Its tools are still gated by approval, and each of
its `approval.requested` events is relayed to the calling session's own
approval gate (labelled `[scv-1 depth N]`, with the nested tool's name and
risk), so the policy and user deciding the parent's side effects also decide
the nested ones; a relay without a gate denies. `scv agents import scv`
stores a copy of SCV's provider key in the nested SCV's `config.toml`
(mode `0600`), because delegated agents never inherit key variables. The
nested SCV has no parent daemon socket, and its protocol lines are bounded
by its frame limit.

A background job (`background: true`) is the same delegated run, detached from
the turn that started it. It has no turn to ask a person through, so each
approval request its agent relays gets the answer the session gives without
asking anyone: the approval policy's own decision (read-only requests under
`on-risk` or `never`), else an approval when the client declared
`auto_approve` in `session.start`, else a denial. A job therefore never gets
more than the same request would get in the foreground: in the TUI its other
requests are denied and it acts within the agent's own permissions (a step
that needs approval fails), while an owner chat session, whose bridge already
approves every request, grants its jobs the same. `auto_approve` is the
client's own statement about how it answers, so it grants nothing that client
could not already grant. `agent_cancel` stops a job at the model's request and
has process risk. Jobs belong to their session and are cancelled, with their
processes, when it closes. The turn the server starts to report a finished job has the session's
own tools and approval policy, like any turn; its prompt quotes the job's
bounded reply, which is untrusted delegated-agent output. Over WeChat or Feishu
the report goes only to the owner's direct chat, as an unprompted message.

SCV never routes a refused request to another agent by itself. A run whose
model refused (`declined`) gets no fallback suggestion, and its note and the
system prompt tell the main agent to report the refusal to the user instead of
retrying elsewhere; the user may still name another agent, whose own policies
then apply. Fallback suggestions for real availability failures are decided
from the result's status and structured `error` only, so text in an agent's
reply cannot trigger one.

## Web access

`web_fetch` sends a GET request to a URL the model chooses, so the URL can
carry anything the model has read, such as a secret that a prompt-injected
page told it to send. Fetches therefore need approval unless the URL is HTTPS
on a host in `web.auto_approve_domains`, a user-only list of documentation and
registry hosts where a GET cannot publish data. The default list is `docs.rs`,
`crates.io`, `doc.rust-lang.org`, `docs.python.org`, `pypi.org`, and
`developer.mozilla.org`. A redirect cannot leave that list without failing the
call.

To keep fetches off the host's own network, `web_fetch` refuses loopback,
private, link-local (including cloud metadata), shared, multicast, and
reserved addresses, and IPv6 forms that embed them. Names resolve once through
a checking resolver whose addresses are the only ones connected to, so DNS
rebinding cannot swap in a private address; IP literals and redirect targets
are checked before any request. `web.allow_private_addresses` lifts this for
trusted networks. Requests send no cookies, credentials, or referrer and
ignore proxy variables.

A configured `web_search` backend receives only the query, and hosted search
sends queries only to the model provider. Neither is offered to tool-free
sessions. Project configuration can disable web access but cannot add
auto-approved hosts, allow private addresses, or choose search endpoints or
keys.

## Approval behavior

An approval prompt includes the exact tool name and a bounded, human-readable
summary. Approval is per call. A denial is returned to the model as a failed
tool result. Cancellation denies and terminates pending work.

The server, not the TUI, decides whether approval is required. This prevents a
custom client from bypassing policy. `never` means deny side effects; it does
not mean silently execute them.

## Network and protocol

Outbound network clients include the configured model provider, the WeChat
channel's iLink adapter, the Feishu channel's Open Platform and long-connection
clients, and the web tools (`web_fetch` and a configured search
backend). The updater delegates registry downloads to Cargo. Project
configuration cannot provide inline credentials or redirect these authorities.

The normal server transport is a local Unix socket at
`$SCV_HOME/state/server.sock` (default `~/.scv/state/server.sock`) with mode
`0600` in a mode `0700` directory. It is a trusted local-user
interface, including daemon management; it is not a remotely authenticated
network service. A process running as the same user can access that authority.
The stdio endpoint remains available for one-shot local clients and does not
support component management. Both transports require the versioned handshake.

The opt-in WeChat channel is an outbound HTTPS client of iLink, not a server
transport; it opens no listening port. It accepts only trusted iLink origins,
validates response envelopes, persists state atomically, and never reports
bearer tokens. The opt-in Feishu channel is likewise outbound only: HTTPS to
its brand's Open Platform and accounts hosts (`open.feishu.cn` and
`accounts.feishu.cn`, or the `larksuite.com` equivalents), without following
redirects, and one WebSocket whose host must be on the brand's domain over TLS
on port 443, checked before dialing. Responses are capped at 4 MiB and events
at 8 MiB; the app secret, tenant token, and the socket URL's one-time keys
never reach logs, errors, or status. Remote sessions are tool-free unless the account grants its owner
tools; see [Supervised remote bridge](#supervised-remote-bridge). Remote
messages cannot bypass server policy.

Protocol lines, tool arguments, tool output, and provider responses are size
bounded. Malformed messages fail closed. Diagnostics are separated from the
protocol stream and redact authorization headers and credential values.

Provider streaming is capped at a 1 MiB SSE event, 4 MiB total response, 1 MiB
assistant text, 256 KiB per tool argument object, and 32 tool calls per model
response. A limit violation cancels the response and fails the turn before any
not-yet-started call from that response is executed. Provider error text is
redacted of the credential, flattened, and bounded before it reaches clients or
logs; channel senders receive only a generic failure reply.

## Supervised remote bridge

QR login is explicit. Saved channel accounts are enabled by default and start
under the daemon; login honors a saved opt-out. To opt out before daemon startup,
set `enabled = false` in the account's `[channels.<channel>.<account>]` table
in `$SCV_HOME/config.toml`. Credentials in `$SCV_HOME/credentials/<channel>`,
delivery state in `$SCV_HOME/state/channels/<channel>`, and `config.toml` use
mode `0600` and atomic writes, in mode `0700` directories. Project
configuration and `SCV_CONFIG` cannot choose bridge accounts, workspaces, or
remote authority.

By default, remote sessions request `no_tools: true`, enforced by the server,
and the bridge denies any approval request, so remote messages do not authorize
filesystem, shell, or delegated-agent tools. An account's `remote_tools =
"owner"` setting, changeable only through local CLI or daemon control, grants
the authenticated account owner (the iLink `user_id` from QR login, or the
Feishu `open_id` of the app's creator, or the one named with
`--owner-open-id`) full tools with every approval request auto-approved. That
makes the owner's chat account equivalent to local shell access as the daemon
user: anyone who can
send messages from it can read and change files, run commands, and launch
delegated agents without confirmation. Other senders, the owner's messages in
group chats, and accounts without a known owner ID stay tool-free; group
conversations never share the owner's direct-chat session. Logout clears the
grant before deleting credentials. Owner replies are ordinary assistant output and may quote
tool results the model chose to include; bridge failure details remain
sanitized. Bridge failures produce short sanitized
replies; raw diagnostics, credentials, tool output, and host paths are not
forwarded as diagnostics to the chat. Status reports identity and live health,
with sanitized errors and successful-contact timestamps, never bearer tokens.
Saved credentials alone do not establish connectivity.

Files chat users send are untrusted input. The bridge downloads them only
from the platform (HTTPS on the WeChat CDN's `qq.com` hosts, or the Feishu
resource API on the brand's own host), within per-account size limits, and
from senders other than the owner only images; it saves them with mode `0600`
under `$SCV_HOME/state/media` behind random prefixes and sanitized names,
never executes them, and removes them after the retention period. A
tool-free session's model never sees their paths. Images reach the model as
image input and can carry injected instructions like any other content.

The owner's model may send files back with `chat_attach`. Because a prompt
could ask it to mail out secrets, the tool refuses the SCV instance directory
(except received media), credential and key locations in the user's home,
host secrets, and secret-like names, all after resolving symlinks, and sends
only a private copy it makes in the media outbox; the bridge sends nothing
from anywhere else. With `bash` available the model could still copy data
out another way, so this narrows accidents and simple injections rather than
isolating anything; see [tools](tools.md#sending-files-to-a-chat-chat_attach).

Delivery state is bound to a SHA-256 fingerprint of the account's identity:
for WeChat the normalized API origin and authenticated bot/user IDs, for
Feishu the brand, app ID, and owner `open_id` (a rotated app secret keeps the
binding). Known-identity token rotation preserves state;
credentials without both IDs use a conservative token-based fingerprint.
Legacy unbound state is bound before first use. A mismatch prevents polling,
recovery, and delivery. Login refuses identity/origin replacement, including
legacy-to-identified replacement, until explicit logout discards the old state.
This prevents pending replies from leaking into a different account or origin.

Notices the daemon sends on its own (update outcomes, jobs a restart stopped,
restarts after an unexpected stop, accounts that stay disconnected) go only
to an account owner's direct chat: the chat that asked, the `[notify]`
accounts' owners, or the chat the owner last wrote from. Their text is
composed by SCV, not the model, and names versions, commits, job handles, and
the first line of each stopped job's delegated prompt; the restart plan and
the owner's last chat are private files under `$SCV_HOME/state`.

Feishu text turns `<at user_id=…>` into mentions, including `@all`, so SCV
breaks every `<at` in outgoing text with a zero-width space: model output, which
a group member can influence, never notifies anyone. In groups the bot answers
only messages that mention it, and those sessions stay tool-free.

A lifetime account lock excludes cooperating runners. A separate transaction
file lock serializes login, settings/state writes, binding, migration, and
removal; state writes recheck identity binding. Credentials and settings are
read together with `account_snapshot`. Both locks are nonblocking and no network
I/O holds the transaction lock. Lock files remain after logout to preserve lock
identity. Old `0.1.9` standalone bridges do not honor these locks and must be
stopped before enabling supervised accounts.

An inbound message is durably claimed before its turn is queued, and the poll
cursor moves past a batch only after its claims are durable. Recovery does not
replay interrupted claimed work; pending replies retain their client IDs
across retries. This protects against duplicate execution without promising
exactly-once delivery by the remote service. Conversations run concurrently,
at most four turns at a time and each conversation in order, and their
sessions never share history. Feishu socket events are acknowledged only once
their claims are durable, and the catch-up that follows every reconnection is
deduplicated like any batch. A reply the platform refuses is held in the private
state file, bounded and for at most 7 days, and delivered only with the next
reply to the same conversation; its content never enters logs.

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
