# SCV Client Protocol

Status: protocol version 3

The SCV client protocol is bidirectional newline-delimited JSON over the local
Unix socket or stdin/stdout. Each line is one UTF-8 JSON object. The server
writes diagnostics only to stderr. `scv-client` owns the instance layout,
which places the socket at `$SCV_HOME/state/server.sock` (normally
`~/.scv/state/server.sock`), and the daemon control helper; it depends on wire
types in `scv-protocol`, not server implementation.

## Version and envelopes

The protocol version is the integer `3`. Every client message has a `type` and
`request_id`. Every server event has a `type`; events produced in response to a
request also carry its `request_id`. Session events carry a monotonically
increasing `seq`, allowing clients to detect a dropped or duplicated frame.
Session and turn events carry their identifiers explicitly.

Unknown object fields are ignored. Unknown message types are rejected with an
`error` event. A client must initialize before sending other messages. A version
mismatch is a fatal error so neither side silently interprets incompatible
semantics.

Clients since 0.3.0 tolerate what a newer server adds within version 3: an
event `type` they do not know parses as `ServerEvent::Unknown` and is skipped
(it may have used a `seq` number, so a skipped event excuses one gap), and an
error `code` or tool error kind they do not know parses as `unknown`. Older
clients fail to parse an unknown event type, so the server sends an event added
later only in answer to a request that only newer clients make. The daemon
answers a `daemon.control` action it does not know with `invalid_json`, which
is how a client tells that the daemon predates the action; replies to new
actions come as additive `daemon.status` fields. Version 3 added `tool.progress`, which a version 2 client could not
parse, so a v2 client receives `version_mismatch`; a TUI started before an
update must be restarted after it.

## Client messages

### `initialize`

```json
{"type":"initialize","request_id":"1","protocol_version":3,"client":{"name":"scv-tui","version":"0.3.0"}}
```

### `daemon.control`

After initialization, a socket client may manage components without creating
an agent session. The `command` object is tagged by `action`:

```json
{"type":"daemon.control","request_id":"d1","command":{"action":"status"}}
{"type":"daemon.control","request_id":"d2","command":{"action":"reload"}}
{"type":"daemon.control","request_id":"d3","command":{"action":"channel_set","channel":"wechat","account":"default","enabled":true,"workspace":"/workspace/project","remote_tools":"owner"}}
{"type":"daemon.control","request_id":"d4","command":{"action":"channel_set","channel":"wechat","account":"default","enabled":false,"workspace":null}}
{"type":"daemon.control","request_id":"d10","command":{"action":"channel_set","channel":"feishu","account":"default","enabled":true,"workspace":null,"senders":"anyone"}}
{"type":"daemon.control","request_id":"d5","command":{"action":"channel_logout","channel":"wechat","account":"default"}}
{"type":"daemon.control","request_id":"d6","command":{"action":"delegations","all":false}}
{"type":"daemon.control","request_id":"d7","command":{"action":"delegation_kill","handle":"codex-3f9a2c","orphans":false}}
{"type":"daemon.control","request_id":"d8","command":{"action":"delegation_kill","orphans":true}}
{"type":"daemon.control","request_id":"d9","command":{"action":"restart_when_idle","version":"0.1.37","commit":"abc1234","parent":"0a1b2c3d/<session>/codex-3f9a2c","max_wait_seconds":600}}
{"type":"daemon.control","request_id":"d10","command":{"action":"confirm_ask","question":"Publish SCV 0.3.0 to crates.io?","parent":"0a1b2c3d/<session>/codex-3f9a2c","timeout_seconds":1800}}
{"type":"daemon.control","request_id":"d11","command":{"action":"confirm_status","id":"5f0c9a1e2b3d"}}
```

`status` reads live daemon health. `reload` reconciles saved accounts and
settings immediately; periodic reconciliation also runs every two seconds.
`channel_set` persists a channel account's enablement and an optional existing
absolute workspace; `channel` names the channel (`wechat` or `feishu`), and an
unknown one is a `component_error`;
an omitted or null workspace leaves the saved workspace unchanged. The optional
`remote_tools` (`none` or `owner`) likewise persists only when present, as
does the optional `senders` (`owner` or `anyone`), whose messages the account
answers. Component status reports the effective `remote_tools`, which is
`owner` only when the account also has a known owner ID; older clients may omit
the field. It reports `senders` as set (an `owner` account without a known
owner ID answers nobody). Daemons before 0.3.0 answer anyone, omit it from
status, and ignore it in `channel_set`. Without a
saved workspace, the account uses the daemon workspace. Replacements stop and
join the old instance first. `channel_logout` persists disablement and joins
before removing credentials, delivery state, and settings. Successful actions
return `daemon.status`; enablement does not imply successful remote contact.

`delegations` lists the instance's running delegated agent runs, from any SCV
process; `all` adds orphans whose owning process died. `delegation_kill` stops
one run by handle, every orphan with `orphans`, or both; naming neither, or an
unknown handle, is a `delegation_error`. Both return `daemon.status` with
`delegations.entries`, and `delegation_kill` also lists the handles it stopped
in `delegations.killed`.

`restart_when_idle` asks the daemon to restart into the release installed at
its own executable path, as `scv restart --when-idle` does after `cargo
install`; every field is optional. The daemon refuses with a `restart_error`
when it does not run as its systemd user unit, when the installed binary does
not answer `scv build-info`, or when that reports a version other than
`version`. Otherwise it saves a plan and replies at once with
`daemon.status`, whose `restart` shows what it waits for: the delegation that
`parent` (the caller's `SCV_PARENT` chain) names until it has finished (a
nested SCV or ACP agent, which lives for its whole conversation, until its
turn has ended and, for a nested SCV, the background jobs of its own session
have been reported to it) and its report is stored, then any owner message a
chat bridge has claimed but not answered. At `max_wait_seconds` (default 600,
at most 3600) it restarts anyway. A second request for the same version
returns the scheduled restart. The restart itself runs in a watchdog unit
outside the daemon; see [architecture.md](architecture.md#planned-restarts).

`confirm_ask` asks the owner a yes/no `question` (at most 4 KiB) in chat, as
`scv confirm` does: in the chat that started the delegation `parent` names,
or else the owner chat notices go to (see
[channels](channels.md#questions-to-the-owner)). `timeout_seconds` (default
1800, at most 14400) is how long no answer waits before it counts as no. The
daemon refuses with a `confirm_error` when no owner's direct chat is
reachable or a question already waits there; otherwise it replies at once
with `daemon.status`, whose `confirm` names the question, and sends it in the
background. `confirm_status` reports where question `id` stands in the same
`confirm` field, and a `confirm_error` for an ID the daemon does not know,
such as after a restart: questions live in memory only. Following a question
keeps it alive; one nobody asks about for a minute is withdrawn. A client
asks and then follows the question, because a single request cannot outlast
the control helper's time limit.

Component management is unsupported on stdio. The client helper bounds its
exchange and never automatically retries a mutation after an ambiguous failure;
query status before retrying.

The control helper has a 20-second timeout. Status and other control commands
can wait for the component lock while reconciliation joins replacements. Many
slow replacements can therefore make even a status query time out; a timeout
does not prove the daemon is down or that a mutation failed. Query status again
before deciding whether to retry a mutation; mutations are never automatically
retried. Current channel cancellation drops its owned I/O and sessions
immediately, but future components with slower shutdown can expose this limit.

### `session.start`

`cwd` is an absolute path chosen by the client. The server canonicalizes it and
rejects a missing or non-directory workspace.

Optional `provider`, `model`, and `base_url` overrides apply only to this session.
`no_tools: true` disables all tools in the server-owned runtime; channel remote
sessions set it for every sender except an account owner granted
`remote_tools = "owner"`.

`delegation_depth` is optional and declared by a client that is itself a
delegated agent, such as a nested SCV or an `scv` command run by one (from its
`SCV_DELEGATION_DEPTH`). The session's delegated runs count from the larger of
it and the server process's own depth, so `agent.max_delegation_depth` holds
across processes. A direct client omits it. The `scv` agent is such a client: it
opens its nested SCV's session with its own depth plus one, runs each
delegated prompt as a `turn.start` on that session, answers the nested
`approval.requested` events with `approval.resolve` after asking its own
session's approval gate, and sends `turn.cancel` when its call is cancelled or
times out (see [Nested SCV](tools.md#nested-scv-scv)).

`channel` is optional and set by a chat bridge: the channel's name as its
users know it (`WeChat`, `Feishu`, or `Lark`), at most 32 bytes without control
characters; anything else is rejected with `invalid_request`. The server then
adds a *Chat channel* section to the system prompt: the user reads short
plain-text chat messages there, only the last message of each turn reaches
them, and they never see tool calls or their output. A chat client also
delivers files the model attaches to its reply, so a tool-enabled session
with a channel offers the model `chat_attach` (see
[tools](tools.md#sending-files-to-a-chat-chat_attach)). A client that is not
a chat omits it.

`auto_approve` is optional: `true` declares that this client answers every
approval request of the session with an approval, without asking anyone. A
chat bridge sets it for an owner session, which it auto-approves (and `false`
otherwise). The server still sends `approval.requested` for the client's own
turns; the declaration only lets background jobs, which outlive the turn that
could carry their requests, get the same answer (see
[Background jobs](tools.md#background-jobs)). It grants nothing the client
could not grant itself. Both fields are additive and keep protocol version 3.

```json
{"type":"session.start","request_id":"2","cwd":"/workspace/project"}
{"type":"session.start","request_id":"channel-session","cwd":"/workspace","no_tools":false,"channel":"WeChat","auto_approve":true}
```

### `turn.start`

```json
{"type":"turn.start","request_id":"3","session_id":"...","prompt":"Fix the failing test."}
{"type":"turn.start","request_id":"4","session_id":"...","prompt":"what is this?","attachments":[{"kind":"image","path":"/home/u/.scv/state/media/wechat/default/1a2b/9f3e-photo.jpg","name":"photo.jpg","mime":"image/jpeg","size":48213}]}
```

An idle session starts this prompt immediately. When another turn is active,
the server appends it to the session queue, attachments included, and emits
`queue.enqueued`. A prompt may be empty only when it has attachments.

`attachments` is optional: at most 16 files already saved on the daemon's
host, such as media a chat user sent. Each has a `kind` (`image`, `audio`,
`video`, `file`, or `sticker`), an absolute `path` to a regular file (not a
symlink), a byte `size`, and optionally the sender's `name`, a `mime` type,
and for a voice message the platform's `transcript`; anything else is
refused with `invalid_request`. The server adds a list of the files to the
prompt (a tool-free session sees names, types, and sizes but no paths) and
sends each PNG, JPEG, GIF, or WebP image of at most 8 MiB to the model as
image input when `provider.image_input` allows (see
[configuration](configuration.md)). History keeps image paths, not bytes: the
newest eight images are read again for each request, and an image deleted
since is replaced by a note.

### Queue operations

```json
{"type":"queue.update","request_id":"4","session_id":"...","queue_id":"...","revision":2,"prompt":"Run the focused tests after the fix."}
{"type":"queue.move","request_id":"5","session_id":"...","queue_id":"...","revision":3,"before_queue_id":"..."}
{"type":"queue.remove","request_id":"6","session_id":"...","queue_id":"...","revision":3}
{"type":"session.pause","request_id":"7","session_id":"...","paused":true}
```

`queue.update`, `queue.move`, and `queue.remove` apply only to queued work and
require the entry's current revision. Stale changes receive `queue_conflict`
with the current entry. `session.pause` prevents automatic dequeue without
altering queue order.

### `turn.cancel`

```json
{"type":"turn.cancel","request_id":"4","session_id":"...","turn_id":"..."}
```

Cancellation aborts provider, tool, or approval work at the next asynchronous
cancellation point and produces `turn.cancelled`.

### `approval.resolve`

```json
{"type":"approval.resolve","request_id":"5","session_id":"...","approval_id":"...","approved":true}
```

An unknown or already-resolved `approval_id` is rejected. Approval IDs are
server-generated UUIDs and never reused. Approval applies only to that call;
the protocol has no "always allow" state.

### `session.clear`

```json
{"type":"session.clear","request_id":"6","session_id":"..."}
```

The session must be idle. The server drops canonical messages, history
compaction metadata, usage totals, and queued prompts while preserving the
monotonic session sequence, then replies with `session.cleared`. The client
clears its transcript only after that event.

## Server events

### Handshake and session

```json
{"type":"initialized","request_id":"1","protocol_version":3,"server":{"name":"scv-server","version":"0.3.0"}}
{"type":"session.started","request_id":"2","session_id":"...","cwd":"/workspace/project","model":"gpt-4.1-mini","context_max_tokens":128000,"max_server_frame_bytes":8388608,"max_transcript_bytes":8388608,"max_transcript_items":10000,"max_prompt_history_bytes":1048576,"max_prompt_history_items":200}
```

### `daemon.status`

```json
{"type":"daemon.status","request_id":"d1","status":{"version":"0.3.0","pid":1234,"components":[{"id":"wechat:default","channel":"wechat","account":"default","bot_id":"bot-example","user_id":"user-example","enabled":true,"state":"connected","last_success_unix_seconds":1750000000,"error":null,"restarts":0,"remote_tools":"none"}],"delegations":{"active":1,"idle":0,"reaped":0}}}
{"type":"daemon.status","request_id":"d6","status":{"version":"0.3.0","pid":1234,"components":[],"delegations":{"active":1,"idle":0,"reaped":0,"entries":[{"handle":"codex-3f9a2c","agent":"codex","session":"5d1c…","depth":1,"pid":4321,"owner_pid":1234,"processes":3,"cwd":"/workspace/scv","started_unix_seconds":1750000000,"orphaned":false,"conversation":"codex-2","turn":3}]}}}
{"type":"daemon.status","request_id":"d10","status":{"version":"0.3.0","pid":1234,"components":[],"delegations":{"active":1,"idle":0,"reaped":0},"confirm":{"id":"5f0c9a1e2b3d","state":"pending","chat":"wechat:default","deadline_unix_seconds":1750001800}}}
```

Version and PID identify the responding server, not the installed client.
Component states are `disabled`, `starting`, `connected`, `disconnected`,
`backoff`, `stopping`, `stopped`, and `failed`. Identity fields may be null for
legacy or unavailable credentials. `last_success_unix_seconds` is null until
successful contact and is a historical timestamp, not a guarantee of current
connectivity. Errors are sanitized; credentials never appear in status.
Loading credentials alone cannot produce `connected`. Management responses
carry the request ID but no session or sequence number.

`status` contains `version`, `pid`, `components`, and `delegations`, and
`restart` while a planned restart is scheduled: `to_version`, `waiting_for`
(omitted once it restarts), `requester`, `origin` (`<channel>:<account>` of
the chat that asked), and `deadline_unix_seconds`. Replies to `confirm_ask`
and `confirm_status` also carry `confirm`: the question's `id`, its `state`
(`scv_protocol::ConfirmState`: `pending`, `yes`, `no`, `expired` when no
answer came in time, `withdrawn` when its asker stopped following it, or
`failed` when it could not be sent, the platform refused it or had not
delivered it by its deadline, or its answer was lost; a state a client
does not know parses as `unknown`), the `chat` asked (`<channel>:<account>`),
and `deadline_unix_seconds`. A status from a daemon older
than 0.1.26 has no `delegations` and parses as zero.
`delegations.active` counts running delegated runs of the instance, live
agents waiting between turns included; `idle` says how many of them are such
agents, apart from a nested SCV whose own background jobs still count
(omitted by older daemons), and `reaped` counts the orphans this daemon
has stopped since it started. `entries` and
`killed` appear only in `delegations` and `delegation_kill` responses. An
entry that is a turn of a delegated conversation also carries `conversation`
(the handle, such as `codex-2`) and `turn`; both are omitted otherwise. A
live agent (a nested SCV or an ACP agent) with no turn running also carries
`idle_since_unix_seconds`, when its last turn ended; it is omitted while the
agent works, for per-turn runs, and by older daemons. A nested SCV whose own
background jobs still run or wait to be reported to it carries
`background_jobs`, their number; it is omitted when there are none and by
older daemons. Each
component contains `id` (`<channel>:<account>`), `channel`, `account`,
`bot_id`, `user_id`, `enabled`, `state`, `last_success_unix_seconds`, `error`,
and `restarts`; a daemon older than 0.1.35 omits `channel`, which parses as
empty. `bot_id` is the WeChat iLink bot or the Feishu app ID, and `user_id`
the account's owner. For WeChat, successful contact means an authenticated,
validated `getupdates` response; for Feishu, a connected long connection that
finished its catch-up or a wait for events without error. The state
fingerprint, bearer token, app secret, and delivery state are private storage
fields, not health fields.

### Turn and assistant output

```json
{"type":"turn.started","request_id":"3","session_id":"...","turn_id":"...","seq":1}
{"type":"assistant.delta","request_id":"3","session_id":"...","turn_id":"...","seq":2,"content":"I found "}
{"type":"assistant.delta","request_id":"3","session_id":"...","turn_id":"...","seq":3,"content":"the issue."}
{"type":"assistant.completed","request_id":"3","session_id":"...","turn_id":"...","seq":4,"content":"I found the issue."}
```

`assistant.delta` is append-only streamed text. `assistant.completed` is the
canonical complete message and may contain an empty `content` when the response
only requests tools.

### Shared queue

On session start, the server emits a `queue.snapshot` containing the
ordered queue entries, each with `queue_id`, `revision`, prompt, submitter
label, and the `attachments` of its `turn.start` when it had any. It emits `queue.enqueued`, `queue.updated`, `queue.moved`,
`queue.removed`, and `queue.dequeued` on that connection. Queue events carry the
session sequence and never reorder relative to terminal turn events. The server
validates and assigns IDs, revisions, and positions; clients never infer queue
state from local input. Sessions remain independent per connection; the shared
daemon socket does not imply cross-client queue broadcast or session attachment.

### Tool lifecycle and approval

```json
{"type":"tool.proposed","request_id":"3","session_id":"...","turn_id":"...","seq":5,"call_id":"call_123","name":"bash","arguments":{"command":"cargo test"}}
{"type":"approval.requested","request_id":"3","session_id":"...","turn_id":"...","seq":6,"approval_id":"...","call_id":"call_123","name":"bash","risk":"process","cwd":"/workspace/project","summary":"Run shell command: cargo test"}
{"type":"tool.started","request_id":"3","session_id":"...","turn_id":"...","seq":7,"call_id":"call_123","name":"agent"}
{"type":"tool.progress","request_id":"3","session_id":"...","turn_id":"...","seq":8,"call_id":"call_123","text":"$ cargo test --workspace\nupdate …/src/lib.rs"}
{"type":"tool.completed","request_id":"3","session_id":"...","turn_id":"...","seq":9,"call_id":"call_123","name":"agent","success":true,"output":"...","truncated":false}
{"type":"tool.completed","request_id":"3","session_id":"...","turn_id":"...","seq":12,"call_id":"call_124","name":"bash","success":false,"output":"tool call denied by policy or user","truncated":false,"error":"denied"}
```

Arguments and outputs are bounded by configuration before serialization. A
denied call completes with `success: false` and a model-visible denial message.
A failed call also carries `error`, why it failed
(`scv_protocol::ToolErrorKind`): `denied` by the approval policy or the user,
`cancelled`, `invalid_arguments` refused before it ran, `unavailable` (the tool
or the agent it runs is missing, signed out, or its provider unreachable), a
size, count, depth, or time `limit`, `unknown_tool`, or `failed` otherwise.
Clients show a call's outcome from `error`, never by reading `output`. A
successful call omits it, as do servers before 0.3.0.

A call that starts a [background job](tools.md#background-jobs), or shows the
model a job's result, also carries `jobs`: one entry per job, with its `job`
handle, the delegating `tool` (`agent`), the `agent` that runs it (such as
`codex`), its `status` (`scv_protocol::JobStatus`), and its `task`, the first
line of the delegated prompt, shortened (omitted when empty). Servers of 0.3.0
and older, which had one delegation tool per agent, send no `agent` and name
the agent in the tool instead (`agent_codex`); `JobChange::agent_name` reads
either form, taking `agent` when present and otherwise what follows `agent_`
in `tool`.

```json
{"type":"tool.completed","request_id":"3","session_id":"...","turn_id":"...","seq":14,"call_id":"call_125","name":"agent","success":true,"output":"{\"job\":\"job-1\",…}","truncated":false,"jobs":[{"job":"job-1","tool":"agent","agent":"codex","status":"running","task":"Land the fix"}]}
{"type":"tool.completed","request_id":"7","session_id":"...","turn_id":"...","seq":31,"call_id":"call_140","name":"agent_wait","success":true,"output":"…","truncated":false,"jobs":[{"job":"job-1","tool":"agent","agent":"codex","status":"completed","task":"Land the fix"}]}
```

A job appears as `running` in the `agent` call that started it, and once
more, with how it ended (`completed`, `failed`, `declined`, `timeout`, or
`cancelled`), in the `agent_wait`, `agent_status`, or `agent_cancel` call
through which the model saw its result or asked for its stop; a job reported
in a server-started turn is named by that turn's `origin.jobs` instead. A job
is therefore settled once the model has seen its result, not when it
finishes: a client keeps the session open until then, since closing it
cancels the session's jobs. The field is omitted when a call touched no job,
and by servers before 0.3.0.

A successful `chat_attach` call's output is
`{"attached":{"path":…,"name":…,"size":…,"caption":…},"note":…}`, where
`path` is a private copy in the channels' media outbox. A chat client reads it
from `tool.completed` (`scv_protocol::reply_attachment`) and sends the file
after the turn's reply; the file belongs to the turn its `request_id` names,
like assistant text.

`tool.progress` carries short status lines a running tool reported: for a
delegated agent, the commands it runs, files it changes, searches, and tools
it calls, never their output. Lines are single lines of at most 200 bytes with
credential-like values replaced by `…`. One event joins the lines reported
since the previous one with newlines; it is at most 512 bytes, keeping the
newest lines behind a leading `…` line when older ones were dropped. A call
produces at most one event per 500 ms, only between its `tool.started` and
`tool.completed`; lines reported within 500 ms of `tool.completed` may be
dropped. Progress is display-only: it never enters the model's history or the
tool result, and channels never forward it.

### Context and completion

```json
{"type":"context.compacted","request_id":"3","session_id":"...","turn_id":"...","seq":9,"before_tokens":130000,"after_tokens":96000,"removed_messages":42}
{"type":"turn.completed","request_id":"3","session_id":"...","turn_id":"...","seq":10,"steps":4,"usage":{"input_tokens":1200,"output_tokens":240}}
{"type":"turn.cancelled","request_id":"3","session_id":"...","turn_id":"...","seq":11}
{"type":"turn.failed","request_id":"3","session_id":"...","turn_id":"...","seq":11,"code":"step_limit","message":"agent reached 32 steps"}
{"type":"session.trimmed","request_id":"3","session_id":"...","seq":12,"removed_messages":250,"history_bytes":12000000}
{"type":"session.cleared","request_id":"6","session_id":"...","seq":13}
```

Usage fields are omitted when the provider does not report them.

### Server-started turns

The server may start a turn itself: when a background delegation (see
[tools](tools.md#background-jobs)) finishes and the model has not seen its
result, it reports the job in a new turn once the session is idle and its
queue is empty. Such a turn is announced and ended like any other, and its
`turn.started` and terminal event carry an `origin`: its `kind`
(`scv_protocol::OriginKind`, `background` for these reports; a kind a client
does not know parses as `unknown`) and the `jobs` it reports, whose results the
model sees in this turn. A client's own turns have none, and older frames
without the field parse as client turns.

```json
{"type":"turn.started","request_id":"background:…","session_id":"...","turn_id":"...","seq":20,"origin":{"kind":"background","jobs":["job-1"]}}
{"type":"assistant.completed","request_id":"background:…","session_id":"...","turn_id":"...","seq":21,"content":"job-1 finished: …"}
{"type":"turn.completed","request_id":"background:…","session_id":"...","turn_id":"...","seq":22,"steps":1,"usage":{},"origin":{"kind":"background","jobs":["job-1"]}}
```

Its `request_id` is server-generated, so a client tells its own turns' events
apart by `request_id`. A `turn.start` sent while it runs is queued as usual.
The optional field keeps protocol version 3: v3 clients that predate it read
the turn as an ordinary one.

### Errors

```json
{"type":"error","request_id":"2","code":"invalid_request","message":"cwd is not a directory","fatal":false}
```

The `code` of `error` and `turn.failed` is a `scv_protocol::ErrorCode`, written
in snake_case; that enum is the list of codes and says what each means. An
`error` answers a message the server refused: one it cannot parse
(`invalid_json`, also for a message type or `daemon.control` action it does not
know), one before `initialize` (`not_initialized`), a different protocol
version (`version_mismatch`, fatal), or a request it declines, such as
`invalid_request`, `session_not_found`, `queue_limit`, `component_error`, or
`confirm_error`.
`turn.failed` carries `provider_error`, one of the `*_limit` codes, or
`internal_error`. Component failures use sanitized messages without credential
or raw transport details. An error after `turn.started`, including a provider
error, is represented only by `turn.failed`. A terminal turn event is exactly one of `turn.completed`,
`turn.cancelled`, or `turn.failed`.

## Ordering and backpressure

Events for a turn are emitted in execution order. `tool.proposed` precedes an
approval request; `tool.started` is emitted only after approval; and one
terminal turn event is last. The writer queue is bounded both by 256 frames and
by serialized bytes: at least 16 MiB and otherwise twice the configured maximum
frame size. Turn emission waits for capacity without dropping semantic events,
but cancellation interrupts that wait. Connection-level sends time out after
three seconds of backpressure and enter bounded connection cleanup.

Client frames are limited to 1 MiB, prompts to 256 KiB, and encoded server
frames to 8 MiB. Frame limits count encoded JSON bytes and exclude the newline
delimiter. The server-frame
default includes room for worst-case JSON escaping of a capped 1 MiB assistant
message plus envelope overhead. Tool output is capped separately before it
reaches the protocol. Oversized input is an `invalid_request` error; a provider
message whose encoded completion frame would exceed the configured server-frame
limit fails the turn with `response_limit` before that frame is emitted.

The provider reader caps an SSE event at 1 MiB, a whole response at 4 MiB,
assistant text at 1 MiB, each accumulated tool-argument object at 256 KiB, and
tool calls at 32 per response. Exceeding one of these limits stops parsing,
cancels the HTTP body, executes no unstarted call from that response, and emits
`turn.failed` with `response_limit` or `tool_limit`.

Input, completed-turn notifications, and writer failure are selected
concurrently. Client EOF cancels the active turn and closes that connection;
it ends a stdio server but does not stop the shared daemon. A broken transport
or a backpressure timeout enters common cleanup;
active work and the writer receive a three-second grace period and are then
aborted and joined rather than left in the background.

SIGTERM or Ctrl+C stops the daemon with bounded cancellation and joining of
supervised components and tracked session tasks. Reconnecting TUI clients
initialize and start fresh sessions. Server history and queues are not restored,
and submitted work is never automatically replayed.

Writer and turn tasks are tracked and joined even if their connection handler
must be aborted. Daemon management releases the component lock before response
writes, so a blocked client does not hold component management or reconciliation.
