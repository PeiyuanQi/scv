# Context Management

Status: final design for v0.1

SCV keeps canonical session history separate from the model-visible context.
Context selection is deterministic, configurable, and replaceable through the
`ContextPolicy` trait.

## Budget

The usable model-history budget is:

```text
context.max_tokens
- context.reserve_output_tokens
- context.safety_margin_tokens
- estimated(system prompt)
- estimated(tool schemas)
```

For selection, the built-in estimator assigns each string
`ceil(UTF-8-bytes / context.bytes_per_token)` tokens plus four structural tokens
per message and the serialized JSON cost for tool calls. Provider-reported
token counts are used only for post-request accounting. The heuristic is
intentionally provider-neutral and must not be presented as exact billing data.

Static configuration is rejected at startup when the reserve and safety margin
consume the window. At turn time, SCV estimates the actual system prompt, tool
schemas, and newest user message. If required content alone exceeds the window,
the turn ends with `context_limit`; it is never silently truncated.

## Built-in budget policy

The built-in policy always retains:

1. the current system prompt and tool schemas;
2. the newest user message;
3. complete assistant-tool groups, so a tool result never appears without the
   assistant call that created it;
4. as many recent complete groups as fit the remaining budget.

When older history does not fit, the policy inserts one deterministic
compaction note containing message counts and short, bounded extracts of prior
user goals and assistant outcomes. Tool output is represented by tool name,
success, and a bounded tail; raw bulk output is not repeated. The note is data,
not a claim that a model-generated semantic summary exists.

Canonical history is separately capped by `session.max_history_bytes` and
`session.max_messages`. When it nears either limit, the server replaces the
oldest complete groups with one bounded deterministic history note and emits
`session.trimmed`. Repeated context selection never summarizes a model-view
summary; history trimming rebuilds its note from the groups being removed plus
the prior note. If an active turn alone exceeds the history cap, it fails with
`history_limit`.

## Configuration

```toml
[context]
max_tokens = 128000
reserve_output_tokens = 8192
safety_margin_tokens = 2048
bytes_per_token = 3
summary_max_chars = 6000
```

Before selection, tool results are already bounded by the tool-output cap.
Older groups that do not fit are omitted whole. If the newest assistant/tool
group cannot fit even after bounded results and compaction, the turn ends with
`context_limit` rather than sending an orphaned or partial group. Operators may
choose smaller model windows without code changes.

## Extension contract

`ContextPolicy::select` receives immutable canonical messages, the system/tool
cost, and the configured budget. It returns selected messages plus metrics. A
policy cannot mutate session history. A future semantic compactor may make a
separate provider request and persist its summary as session metadata, while
preserving this selection boundary.

The server emits `context.compacted` whenever the selected context omits
canonical messages. This makes lossy behavior visible to the TUI and protocol
clients.
