# ClawBot / WeChat iLink

Status: supported local bridge for the SCV daemon

SCV connects a local workspace to WeChat ClawBot through the iLink HTTP API.
The bridge runs as a supervised component inside the single SCV daemon, which
remains authoritative for sessions, provider selection, policy, and turn
execution. `scv-server` depends on `scv-clawbot`, which uses `scv-client` and
`scv-protocol`; the bridge does not depend on the server crate.

## User workflow

```text
scv clawbot login [--account NAME]
scv clawbot run --workspace PATH [--account NAME] [--remote-tools none|owner]
scv clawbot stop [--account NAME]
scv clawbot status [--account NAME]
scv clawbot logout [--account NAME]
scv reload
```

`login` is explicit: it renders the QR code and stores credentials without
printing the token. Saved accounts autostart under the daemon by default;
logging in preserves any saved disabled setting. Changing the account identity
or API origin requires explicit logout first, including replacing legacy
credentials whose identity is unknown. Login requests a reload when
the daemon is reachable; otherwise enabled accounts start on its next startup.

`run` validates the account and workspace, persists enablement through the
live daemon, and returns. `--remote-tools` persists the account's remote tool
authority (see [Sessions and safety](#sessions-and-safety)); omitting it keeps
the saved value. `stop` persistently disables the account and joins
its component while retaining credentials. `logout` requires a live daemon:
it persists disablement, cancels and joins the component, then removes local
credentials, delivery state, and settings. The API has no documented remote
token-revocation operation.

`status` queries the running daemon, showing its PID/version and the selected
account's identity, enabled setting, effective `remote_tools` authority, health
state, restart count, sanitized error, and last successful authenticated,
validated `getupdates` timestamp.
Saved credentials are not proof of a connection. If the daemon is unavailable,
connectivity is unknown.

## Lifecycle and settings

The daemon reconciles saved accounts on startup and every two seconds;
`scv reload` triggers immediate reconciliation. It starts each enabled account
once, and stops and joins an old instance before starting a replacement with
updated credentials or settings. Unexpected exits retry with exponential
backoff from 1 to 60 seconds. SIGTERM and Ctrl+C cancel and join components and daemon sessions with
bounded shutdown. All long-running integrations use server supervision.

Settings live at `$SCV_HOME/clawbot/settings/<account>.json` (`SCV_HOME` defaults
to `~/.scv`):

```json
{"enabled":true,"workspace":"/absolute/path/to/workspace"}
```

Missing settings default to enabled. An omitted or null workspace uses the
daemon workspace. `run --account NAME --workspace PATH` persists an explicit
workspace. To opt out offline, create or edit the settings to contain
`{"enabled":false}` before starting the daemon. Keep the file mode `0600` and
its ClawBot parent directories mode `0700`. Login honors this opt-out.

Settings reject unknown fields. The supervisor reads credentials and settings
together through `state::account_snapshot` under a short transaction lock.
A busy snapshot defers that account's reconciliation to a later pass without
stopping its current instance.
Discovery rejects more than 128 entries (including the legacy default account)
and directory-entry errors rather than silently returning a partial account set.

Stop any `0.1.9` standalone ClawBot process before enabling a supervised account.
Those older processes do not honor the account locks.

## Identity and durable state

`credential_fingerprint` binds delivery state to a SHA-256 fingerprint of the
normalized API origin and authenticated bot/user IDs. Token rotation for the
same known identity and origin preserves the cursor, pending replies, and
per-chunk client IDs. If either ID is unavailable, the conservative fingerprint
also includes the token and available identity fields. Legacy unbound state is
bound to the saved credentials before first use.

A binding mismatch fails before polling, interrupted-work recovery, or delivery.
Login rejects a different identity/origin, including a transition from legacy
unknown identity to identified credentials, until explicit logout. Login never
resets or archives a running account's state. Logout discards the old state
after stopping the component; a subsequent login starts fresh.

A lifetime advisory lock prevents cooperating runners from using the same
account concurrently. A separate transaction file lock serializes login
credential writes, settings/state writes, binding, migration, and removal.
State writes recheck the binding, preventing a stale runner from overwriting
another identity's state. These locks are nonblocking: contention returns a
retry error, and network I/O never holds the transaction lock. The daemon's
account commands (enable, disable, settings, logout) retry that error for up to
five seconds, so they wait out a running bridge's state commit instead of
failing. Lock files remain in place after logout so open descriptors cannot
refer to different lock inodes.

## iLink contract

Use the returned `baseurl` after login, falling back to
`https://ilinkai.weixin.qq.com`. Login uses `GET
/ilink/bot/get_bot_qrcode?bot_type=3`, then `GET
/ilink/bot/get_qrcode_status?qrcode=...` until confirmed, expired, or timed
out. Confirmation must provide `bot_token`, `ilink_bot_id`, and
`ilink_user_id`.

Authenticated calls use JSON, `AuthorizationType: ilink_bot_token`, bearer
authorization, and a fresh `X-WECHAT-UIN` containing base64 of a random `u32`.
Bodies include `base_info.channel_version = "1.0.0"`. Error responses with
`ret != 0` or a non-zero `errcode` are converted to redacted bridge errors.
Successful `getupdates` responses from the live iLink API omit `ret` and are
accepted only when they contain an array `msgs` field and a string
`get_updates_buf` cursor. Successful `sendmessage` responses also omit `ret`
and need not be JSON: any 2xx body without a non-zero `ret` or `errcode`
acknowledges delivery.

`POST /ilink/bot/getupdates` long-polls with the opaque `get_updates_buf`
cursor. Only inbound user text messages with sender ID, message ID, context
token, and non-empty text are accepted. iLink message IDs may be strings up to
256 bytes or unsigned 64-bit JSON integers; SCV preserves either form as an
exact string for durable deduplication. Ignored messages are durably marked.
Before connecting a sender session or submitting accepted work, the bridge
persists an in-flight claim with the message ID, recipient, and context token.
Recovery never resubmits interrupted claimed work; it records a short failure
reply instead. Completed work becomes a durable pending reply before its claim
is cleared. This prevents a crash between execution and reply storage from
replaying the turn.

Poll batches above 4096 messages are rejected before executing any message or
advancing the cursor. Deduplication retains the newest 4096 IDs; previously
seen IDs encountered in an accepted batch move to the newest end. This retains
the processed batch until its cursor checkpoint, including recovered replies.

`POST /ilink/bot/sendmessage` echoes the original
`context_token`, uses `message_type = 2`, `message_state = 2`, and a unique
`client_id`. Pending sends retain that client ID for retry after transport
failures, 5xx responses, and HTTP 401, 408, or 429. A rejection is final: an
explicit `sendmessage` refusal (non-zero `ret` or `errcode` in a 2xx body) or
any other 4xx status. Live iLink keeps refusing the same reply, including after
it already accepted an earlier copy, so the bridge logs the sanitized status or
integer `ret`/`errcode` and bounded `errmsg`, drops the rest of that reply,
marks the message handled, and resumes polling. Finality applies to every
refusal code, so a rate-limited or expired-context reply is dropped without
notice to the sender; the daemon log records it. Text replies are split at Unicode boundaries
under the configured size limit. Media, typing, and uploads are deferred.

## Sessions and safety

Each direct-chat sender has one long-lived SCV protocol-v2 socket session. A
message carrying a non-empty `group_id` uses a separate session per group and
sender, so group members never see the sender's direct-chat history. Sessions
idle for 30 minutes after their last turn ends are dropped. Turns run one at a
time per account: a long owner turn delays polling and every other sender until
it completes or reaches its limit. Completed assistant output is
sent only after `turn.completed`; failures become short non-sensitive replies.
Network failures use bounded exponential backoff. Retained duplicate message
IDs and interrupted claims do not start another turn. Pending sends retain
their client IDs across recovery; remote exactly-once delivery is not promised.
Raw diagnostics, tool arguments, tool output, tokens, and host paths are not
forwarded as failure details to WeChat.

Remote tool authority is a per-account setting, `remote_tools`, that only local
CLI or daemon control can change:

- `none` (default): every remote session uses `session.start` with
  `no_tools: true`, enforced by the server, and the bridge denies any approval
  request.
- `owner`: messages from the account's authenticated owner, the iLink
  `user_id` recorded at QR login, start a full session with every configured
  SCV tool, and the bridge approves that session's approval requests. Other
  senders, and the owner writing in a group (any non-empty `group_id`), keep
  `none` behavior. Accounts without a known owner ID grant tools to nobody.
  Owner turns may run for the configured tool ceiling
  (`tools.max_timeout_seconds`, default 30 minutes) plus five minutes, and at
  least 30 minutes, instead of 5; the ceiling is read from the workspace
  configuration when the component starts. Logout resets the setting
  to `none` before deleting credentials, so a later login never inherits it.

`scv clawbot run --remote-tools owner` reports whether the daemon actually
applied the grant; an older daemon or a login without an owner ID leaves it
inactive with a warning. Delegated `agent_claude` and `agent_codex` calls need
their CLIs signed in for SCV first; see `scv agents login` in the
[tools reference](tools.md#signing-in-delegated-agents).

The bridge never invokes `scv exec --yes`.

Credentials are stored at `$SCV_HOME/clawbot/accounts/<account>.json`; cursor
and message IDs, in-flight claims, and pending replies are stored at
`$SCV_HOME/clawbot/state/<account>.json`. Credentials, settings, and delivery
state use atomic writes and mode `0600` on Unix; parent directories are mode
`0700`. Account names contain only ASCII letters, digits, `_`, and `-`.
Project configuration cannot select accounts, workspaces, or remote authority.

`scv-clawbot` owns iLink authentication, polling, durable state, sender
sessions, and delivery retries. `scv-server::components` owns lifecycle and
health. Account selection uses `--account` (default `default`), not project
configuration. The [quality contract](quality.md) defines local-only verification.
