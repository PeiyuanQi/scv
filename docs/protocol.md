# SCV Client Protocol

Status: proposed protocol version 2 for the stdio queue

The SCV client protocol is bidirectional newline-delimited JSON over stdin and
stdout. Each line is one UTF-8 JSON object. The server writes diagnostics only
to stderr.

## Version and envelopes

The protocol version is the integer `2`. Every client message has a `type` and
`request_id`. Every server event has a `type`; events produced in response to a
request also carry its `request_id`. Session events carry a monotonically
increasing `seq`, allowing clients to detect a dropped or duplicated frame.
Session and turn events carry their identifiers explicitly.

Unknown object fields are ignored. Unknown message types are rejected with an
`error` event. A client must initialize before sending other messages. A version
mismatch is a fatal error so neither side silently interprets incompatible
semantics.

## Client messages

### `initialize`

```json
{"type":"initialize","request_id":"1","protocol_version":1,"client":{"name":"scv-tui","version":"0.1.0"}}
```

### `session.start`

`cwd` is an absolute path chosen by the client. The server canonicalizes it and
rejects a missing or non-directory workspace.

```json
{"type":"session.start","request_id":"2","cwd":"/workspace/project"}
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
{"type":"initialized","request_id":"1","protocol_version":1,"server":{"name":"scv-server","version":"0.1.0"}}
{"type":"session.started","request_id":"2","session_id":"...","cwd":"/workspace/project","model":"gpt-4.1-mini","context_max_tokens":128000,"max_server_frame_bytes":8388608,"max_transcript_bytes":8388608,"max_transcript_items":10000,"max_prompt_history_bytes":1048576,"max_prompt_history_items":200}
```

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
state from local input. Cross-client broadcast is deferred until a shared local
transport is implemented.

### Tool lifecycle and approval

```json
{"type":"tool.proposed","request_id":"3","session_id":"...","turn_id":"...","seq":5,"call_id":"call_123","name":"bash","arguments":{"command":"cargo test"}}
{"type":"approval.requested","request_id":"3","session_id":"...","turn_id":"...","seq":6,"approval_id":"...","call_id":"call_123","name":"bash","risk":"process","cwd":"/workspace/project","summary":"Run shell command: cargo test"}
{"type":"tool.started","request_id":"3","session_id":"...","turn_id":"...","seq":7,"call_id":"call_123","name":"bash"}
{"type":"tool.completed","request_id":"3","session_id":"...","turn_id":"...","seq":8,"call_id":"call_123","name":"bash","success":true,"output":"...","truncated":false}
```

Arguments and outputs are bounded by configuration before serialization. A
denied call completes with `success: false` and a model-visible denial message.

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

### Errors

```json
{"type":"error","request_id":"2","code":"invalid_request","message":"cwd is not a directory","fatal":false}
```

Stable request/server error codes are `invalid_json`, `not_initialized`,
`version_mismatch`, `invalid_request`, `session_not_found`, `turn_active`,
`turn_not_found`, `approval_not_found`, `queue_not_found`, `queue_conflict`,
and `internal_error`. Stable
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
concurrently. An EOF from the client cancels the active turn and shuts down the
server. A broken stdout pipe or a backpressure timeout enters common cleanup;
active work and the writer receive a three-second grace period and are then
aborted and joined rather than left in the background.
