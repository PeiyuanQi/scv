# SCV Client Protocol

Status: protocol version 3

The SCV client protocol is bidirectional newline-delimited JSON over the local
Unix socket or stdin/stdout. Each line is one UTF-8 JSON object. The server
writes diagnostics only to stderr. `scv-client` owns the default socket path
(`$SCV_HOME/state/server.sock`, normally `~/.scv/state/server.sock`) and the daemon control
helper; it depends on wire types in `scv-protocol`, not server implementation.

## Version and envelopes

The protocol version is the integer `3`. Every client message has a `type` and
`request_id`. Every server event has a `type`; events produced in response to a
request also carry its `request_id`. Session events carry a monotonically
increasing `seq`, allowing clients to detect a dropped or duplicated frame.
Session and turn events carry their identifiers explicitly.

Unknown object fields are ignored. Unknown message types are rejected with an
`error` event. A client must initialize before sending other messages. A version
mismatch is a fatal error so neither side silently interprets incompatible
semantics. Version 3 added `tool.progress`, which a version 2 client could not
parse, so a v2 client receives `version_mismatch`; a TUI started before an
update must be restarted after it.

## Client messages

### `initialize`

```json
{"type":"initialize","request_id":"1","protocol_version":3,"client":{"name":"scv-tui","version":"0.2.0"}}
```

### `daemon.control`

After initialization, a socket client may manage components without creating
an agent session. The `command` object is tagged by `action`:

```json
{"type":"daemon.control","request_id":"d1","command":{"action":"status"}}
{"type":"daemon.control","request_id":"d2","command":{"action":"reload"}}
{"type":"daemon.control","request_id":"d3","command":{"action":"channel_set","channel":"wechat","account":"default","enabled":true,"workspace":"/workspace/project","remote_tools":"owner"}}
{"type":"daemon.control","request_id":"d4","command":{"action":"channel_set","channel":"wechat","account":"default","enabled":false,"workspace":null}}
{"type":"daemon.control","request_id":"d5","command":{"action":"channel_logout","channel":"wechat","account":"default"}}
{"type":"daemon.control","request_id":"d6","command":{"action":"delegations","all":false}}
{"type":"daemon.control","request_id":"d7","command":{"action":"delegation_kill","handle":"codex-3f9a2c","orphans":false}}
{"type":"daemon.control","request_id":"d8","command":{"action":"delegation_kill","orphans":true}}
```

`status` reads live daemon health. `reload` reconciles saved accounts and
settings immediately; periodic reconciliation also runs every two seconds.
`channel_set` persists a channel account's enablement and an optional existing
absolute workspace; `channel` names the channel (`wechat` or `feishu`), and an
unknown one is a `component_error`;
an omitted or null workspace leaves the saved workspace unchanged. The optional
`remote_tools` (`none` or `owner`) likewise persists only when present.
Component status reports the effective `remote_tools`, which is `owner` only
when the account also has a known owner ID; older clients may omit the field. Without a
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
across processes. A direct client omits it. `agent_scv` is such a client: it
opens its nested SCV's session with its own depth plus one, runs each
delegated prompt as a `turn.start` on that session, answers the nested
`approval.requested` events with `approval.resolve` after asking its own
session's approval gate, and sends `turn.cancel` when its call is cancelled or
times out (see [Nested SCV](tools.md#nested-scv-agent_scv)).

`channel` is optional and set by a chat bridge: the channel's name as its
users know it (`WeChat`, `Feishu`, or `Lark`), at most 32 bytes without control
characters; anything else is rejected with `invalid_request`. The server then
adds a *Chat channel* section to the system prompt: the user reads short
plain-text chat messages there, only the last message of each turn reaches
them, and they never see tool calls or their output. A client that is not a
chat omits it.

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
```

An idle session starts this prompt immediately. When another turn is active,
the server appends it to the session queue and emits `queue.enqueued`. Empty
prompts are rejected.

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
v0.1 has no "always allow" protocol state.

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
{"type":"initialized","request_id":"1","protocol_version":3,"server":{"name":"scv-server","version":"0.2.0"}}
{"type":"session.started","request_id":"2","session_id":"...","cwd":"/workspace/project","model":"gpt-4.1-mini","context_max_tokens":128000,"max_server_frame_bytes":8388608,"max_transcript_bytes":8388608,"max_transcript_items":10000,"max_prompt_history_bytes":1048576,"max_prompt_history_items":200}
```

### `daemon.status`

```json
{"type":"daemon.status","request_id":"d1","status":{"version":"0.2.0","pid":1234,"components":[{"id":"wechat:default","channel":"wechat","account":"default","bot_id":"bot-example","user_id":"user-example","enabled":true,"state":"connected","last_success_unix_seconds":1750000000,"error":null,"restarts":0,"remote_tools":"none"}],"delegations":{"active":1,"reaped":0}}}
{"type":"daemon.status","request_id":"d6","status":{"version":"0.2.0","pid":1234,"components":[],"delegations":{"active":1,"reaped":0,"entries":[{"handle":"codex-3f9a2c","agent":"codex","session":"5d1c…","depth":1,"pid":4321,"owner_pid":1234,"processes":3,"cwd":"/workspace/scv","started_unix_seconds":1750000000,"orphaned":false,"conversation":"codex-2","turn":3}]}}}
```

Version and PID identify the responding server, not the installed client.
Component states are `disabled`, `starting`, `connected`, `disconnected`,
`backoff`, `stopping`, `stopped`, and `failed`. Identity fields may be null for
legacy or unavailable credentials. `last_success_unix_seconds` is null until
successful contact and is a historical timestamp, not a guarantee of current
connectivity. Errors are sanitized; credentials never appear in status.
Loading credentials alone cannot produce `connected`. Management responses
carry the request ID but no session or sequence number.

`status` contains `version`, `pid`, `components`, and `delegations`; a status
from a daemon older than 0.1.26 has no `delegations` and parses as zero.
`delegations.active` counts running delegated runs of the instance and
`reaped` the orphans this daemon has stopped since it started; `entries` and
`killed` appear only in `delegations` and `delegation_kill` responses. An
entry that is a turn of a delegated conversation also carries `conversation`
(the handle, such as `codex-2`) and `turn`; both are omitted otherwise. Each
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
ordered queue entries, each with `queue_id`, `revision`, prompt, and submitter
label. It emits `queue.enqueued`, `queue.updated`, `queue.moved`,
`queue.removed`, and `queue.dequeued` on that connection. Queue events carry the
session sequence and never reorder relative to terminal turn events. The server
validates and assigns IDs, revisions, and positions; clients never infer queue
state from local input. Sessions remain independent per connection; the shared
daemon socket does not imply cross-client queue broadcast or session attachment.

### Tool lifecycle and approval

```json
{"type":"tool.proposed","request_id":"3","session_id":"...","turn_id":"...","seq":5,"call_id":"call_123","name":"bash","arguments":{"command":"cargo test"}}
{"type":"approval.requested","request_id":"3","session_id":"...","turn_id":"...","seq":6,"approval_id":"...","call_id":"call_123","name":"bash","risk":"process","cwd":"/workspace/project","summary":"Run shell command: cargo test"}
{"type":"tool.started","request_id":"3","session_id":"...","turn_id":"...","seq":7,"call_id":"call_123","name":"agent_codex"}
{"type":"tool.progress","request_id":"3","session_id":"...","turn_id":"...","seq":8,"call_id":"call_123","text":"$ cargo test --workspace\nupdate …/src/lib.rs"}
{"type":"tool.completed","request_id":"3","session_id":"...","turn_id":"...","seq":9,"call_id":"call_123","name":"agent_codex","success":true,"output":"...","truncated":false}
```

Arguments and outputs are bounded by configuration before serialization. A
denied call completes with `success: false` and a model-visible denial message.

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
`turn.started` and terminal event carry an `origin`; a client's own turns have
none, and older frames without the field parse as client turns.

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

Stable request/server error codes are `invalid_json`, `not_initialized`,
`version_mismatch`, `invalid_request`, `session_not_found`, `turn_active`,
`turn_not_found`, `approval_not_found`, `queue_not_found`, `queue_conflict`,
`unsupported`, `component_error`, `delegation_error`, and `internal_error`. Component failures use
sanitized messages without credential or raw transport details. Stable
`turn.failed` codes are `provider_error`, `context_limit`, `step_limit`,
`history_limit`, `response_limit`, `tool_limit`, and `internal_error`. An error
after `turn.started`, including a provider error, is represented only by
`turn.failed`. A terminal turn event is exactly one of `turn.completed`,
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
