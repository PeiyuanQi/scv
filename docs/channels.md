# Channels

Status: supported local bridges for the SCV daemon

A channel connects chat accounts to a local workspace. One command manages
every channel: `scv channels <command> <channel>`. The channels are WeChat,
through its ClawBot iLink HTTP API, and Feishu with its international edition
Lark, through a bot app and Feishu's event long connection (`lark` is accepted
wherever `feishu` is).

Each account runs as a supervised component inside the single SCV daemon,
which remains authoritative for sessions, provider selection, policy, and turn
execution. `scv-channels` holds the bridge every channel shares; a channel
crate (`scv-clawbot` for WeChat, `scv-feishu` for Feishu) supplies only its
transport. `scv-server` depends on the channel crates, which use
`scv-channels` and, through it, `scv-client` and `scv-protocol`; no channel
crate depends on the server crate.

## User workflow

```text
scv channels login wechat [--account NAME] [--login-url URL]
scv channels login feishu|lark [--account NAME]
scv channels login feishu|lark --app-id CLI_ID [--owner-open-id OPEN_ID] [--account NAME]
scv channels run <channel> --workspace PATH [--account NAME] [--remote-tools none|owner]
scv channels stop <channel> [--account NAME]
scv channels status [<channel>] [--account NAME]
scv channels logout <channel> [--account NAME]
scv reload
```

`--account` defaults to `default`, except for `status`, which lists every
channel and account unless narrowed. `--login-url` is WeChat's iLink login
origin (default `https://ilinkai.weixin.qq.com`); `--app-id` and
`--owner-open-id` are Feishu's, and each channel refuses the other's options.
Daemon status names each account `<channel>:<account>`, such as
`wechat:default` or `feishu:default`, with a `channel` field, the bot's
identity in `bot_id` (the iLink bot, or the Feishu app ID), and the owner in
`user_id`.

`login` is explicit: it renders the QR code and stores credentials without
printing the token or secret. For Feishu, the scan creates a bot app in the
user's own Feishu or Lark account (see [Feishu contract](#feishu-contract)). Saved accounts autostart under the daemon by default;
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

`status` queries the running daemon, showing its PID/version and each selected
account's identity, enabled setting, effective `remote_tools` authority, health
state, restart count, sanitized error, and last successful contact: an
authenticated, validated WeChat `getupdates`, or for Feishu a connected long
connection that finished its catch-up or its last wait without error.
Saved credentials are not proof of a connection. If the daemon is unavailable,
connectivity is unknown.

## Lifecycle and settings

The daemon reconciles saved accounts on startup and every two seconds;
`scv reload` triggers immediate reconciliation. It starts each enabled account
once, and stops and joins an old instance before starting a replacement with
updated credentials or settings. Unexpected exits retry with exponential
backoff from 1 to 60 seconds. SIGTERM and Ctrl+C cancel and join components and daemon sessions with
bounded shutdown. All long-running integrations use server supervision.

Settings live at `$SCV_HOME/channels/<channel>/settings/<account>.json`
(`SCV_HOME` defaults to `~/.scv`):

```json
{"enabled":true,"workspace":"/absolute/path/to/workspace"}
```

Missing settings default to enabled. An omitted or null workspace uses the
daemon workspace. `run --account NAME --workspace PATH` persists an explicit
workspace. To opt out offline, create or edit the settings to contain
`{"enabled":false}` before starting the daemon. Keep the file mode `0600` and
its channel parent directories mode `0700`. Login honors this opt-out.

Settings reject unknown fields. The supervisor reads credentials and settings
together through `state::account_snapshot` under a short transaction lock.
A busy snapshot defers that account's reconciliation to a later pass without
stopping its current instance.
Discovery rejects more than 128 entries (including the legacy default account)
and directory-entry errors rather than silently returning a partial account set.
Each channel is discovered separately: one that fails stops only its own
accounts and reports a `<channel>:discovery-error` component, while the other
channel's accounts keep running.

Stop any `0.1.9` standalone ClawBot process before enabling a supervised account.
Those older processes do not honor the account locks.

## Identity and durable state

`credential_fingerprint` binds delivery state to a SHA-256 fingerprint of the
account's identity. For WeChat that is the normalized API origin and
authenticated bot/user IDs; for Feishu it is the brand, the app ID, and the
owner's `open_id`, so a rotated app secret keeps the state while another app
or owner needs a logout. Token rotation for the
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

## State saved before channels

Releases before `0.1.35` kept WeChat state in `$SCV_HOME/clawbot`. The daemon,
at every reconciliation, and each `scv channels` command move that directory to
`$SCV_HOME/channels/wechat` in one rename, so credentials, settings, cursor,
deduplicated IDs, claims, pending and held replies, and the credential binding
arrive unchanged. The move holds every account's lifetime and transaction locks
from the old directory: a bridge or login of an older binary still using them
makes it fail with a retry message instead of racing it. When both directories
exist, nothing moves: the daemon reports a failed `wechat:discovery-error`
component and stops its accounts, and `scv channels status` prints both paths
so the user can keep one and move the other aside. The earliest single-file
credentials, `$SCV_HOME/clawbot.toml`, still become the `default` account on
first read.

## WeChat iLink contract

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
Before queueing accepted work, the bridge persists an in-flight claim with the
message ID, recipient, context token, and conversation, and it saves the
batch's cursor only after every claim in the batch is durable. Recovery never
resubmits interrupted claimed work; it records a short failure reply for each
claim instead. Completed work becomes a durable pending reply in the same state
write that clears its claim. This prevents a crash between execution and reply
storage from replaying the turn. A message that is already claimed or awaiting
delivery never starts a second turn.

Poll batches above 4096 messages are rejected before executing any message or
advancing the cursor. Deduplication retains the newest 4096 IDs; previously
seen IDs encountered in an accepted batch move to the newest end. This retains
the processed batch until its cursor checkpoint, including recovered replies.

`POST /ilink/bot/sendmessage` echoes the original
`context_token`, uses `message_type = 2`, `message_state = 2`, and a unique
`client_id`. Checked live with the owner (2026-09-23):

- A `sendmessage` with no `context_token` is an unprompted message, answering
  nothing: it returned HTTP 200 with `{"message_id":…}` and no `ret`, and was
  delivered. It was sent seconds after the owner's last message; whether
  iLink allows one after a long silence is untested.
- The first reply on a `context_token`, sent 120 seconds after the inbound
  message, returned 200 with a `message_id` and was delivered.
- A second send on the same `context_token` returned 200 with a `message_id`,
  exactly like a success, but was silently dropped. The API cannot tell it
  apart from a delivery.

So the bridge sends at most one message per context token: a reply longer
than one message goes out as the reply followed by unprompted continuations,
and a background report (below) is always unprompted. A silently dropped
unprompted message cannot be detected either. Pending sends retain that client ID for retry after transport
failures, 5xx responses, and HTTP 401, 408, or 429. A rejection is final: an
explicit `sendmessage` refusal (non-zero `ret` or `errcode` in a 2xx body) or
any other 4xx status. Live iLink keeps refusing the same reply, including after
it already accepted an earlier copy, so the bridge logs the sanitized status or
integer `ret`/`errcode` and bounded `errmsg`, stops sending that reply, marks
the message handled, and resumes polling.

A refused reply is not lost. The bridge holds it, never logging its content,
and delivers it with the next reply to the same conversation (direct chat, or
group and sender), marked `[Earlier reply that could not be delivered at the
time]` and followed by `[Reply to your latest message]`. iLink accepts one reply
per inbound context token, so held replies ride inside that single message:
the oldest that fit beside the new reply go first, and the rest wait for the
following one. A held reply is cut to half the message limit, and the store
keeps at most 4 replies and 32 KiB per conversation, 128 in total, for 7 days,
discarding the oldest first and logging only how many it discarded. If the
carrying reply is refused too, the carried replies return to the store ahead
of it. The busy notice described below is never held. Text replies are split at
Unicode boundaries into messages of at most 16 KiB; only the first carries the
context token. A turn's answer is kept to 64 KiB, and a longer one is cut with
a `[reply truncated]` note instead of failing the turn. Media, typing, and
uploads are deferred.

## Feishu contract

Checked live with the owner on 2026-09-24; see the
[channels plan](channels-plan.md#0-feishu-live-check).

**Sign-in by scan.** `scv channels login feishu` runs the device flow that
Lark's own CLI uses. It posts forms to
`https://accounts.feishu.cn/oauth/v1/app/registration`: `action=init` must list
`client_secret` in `supported_auth_methods`; `action=begin` with
`archetype=PersonalAgent`, `auth_method=client_secret`, and
`request_user_info=open_id tenant_brand` returns a device code, a user code,
an interval, and an expiry (an hour live). SCV prints a terminal QR code for
`https://open.feishu.cn/page/cli?user_code=…` (`open.larksuite.com` for
`lark`) and polls with `action=poll`: `authorization_pending` keeps waiting,
`slow_down` adds five seconds (at most 60 between polls), `access_denied` and
`expired_token` stop, and other errors stop with only their code shown. When
`user_info.tenant_brand` names Lark, polling moves once to
`accounts.larksuite.com`, which issues the credentials. The result is the app
ID and secret and the creator's `open_id`, recorded as the owner; Feishu issues
`open_id` per app, so it is the sender ID of the owner's messages to this bot.
The app works at once, with no developer console, administrator approval, or
public URL. It is named after its creator ("…的飞书 CLI"); login prints the
developer-console link for renaming it, which needs a new app version.
Registration ignores unknown fields, so whether it can take a name is unknown.
An account that is already signed in must log out first.

**Sign-in with an existing app.** `--app-id` adds an app the user already has,
such as one a company administrator approved. The secret is read from a
hidden prompt or stdin, never an argument, and checked against
`/open-apis/auth/v3/tenant_access_token/internal` before anything is written.
`--owner-open-id` names the owner; without it the account grants tools to
nobody.

**Receiving.** Each connection starts with
`POST {open}/callback/ws/endpoint` (`AppID`, `AppSecret`), which returns a
`wss` URL and client settings. The URL must be on the brand's domain
(`feishu.cn` or `larksuite.com`) over TLS on port 443; any other host is
refused before dialing, and connection errors never include the URL, which
carries one-time keys. Frames are protobuf `pbbp2.Frame`
(`crates/scv-feishu/proto/pbbp2.proto`). SCV pings the connection's
`service_id` at once and then at the server's `PingInterval` (90 seconds
live), applies intervals a pong reports, and treats two intervals plus 30
seconds of silence as a lost connection. Events split by the `sum` and `seq`
headers are reassembled within 30 seconds, 64 parts, and 8 MiB. Only `event`
data frames are handled; `im.message.receive_v1` becomes a message and every
other event, such as `im.message.message_read_v1`, is acknowledged at once and
ignored. A message event is acknowledged, with the same frame, a `biz_rt`
header, and a `{"code":200}` payload, only when the bridge asks for the next
batch, which it does after the event's claim and checkpoint are durable.

**Catch-up.** Feishu does not redeliver messages sent while SCV was
disconnected. The checkpoint records, for up to 64 chats, whether each is a
group and the newest message time handed to the bridge. On every connection,
before reading the socket, SCV lists each of the 32 most recently active
chats' messages since that time, reaching back at most 24 hours, through
`GET /open-apis/im/v1/messages` (`container_id_type=chat`, oldest first, up to
four pages of 50), and hands them over like socket events. The bot's own and
other apps' messages and deleted ones are skipped. A chat whose history Feishu
refuses is skipped with a warning; a transport failure fails the catch-up so
it runs again. Deduplication by message ID drops anything already claimed or
answered, including a late socket redelivery. Messages from chats SCV has not
yet seen are not caught up.

**Messages.** Text and rich-text (`post`) messages from users are answered;
rich text becomes plain text, one paragraph per line. Other types are only
marked seen. In a group (any `chat_type` other than `p2p`) the bot answers
only messages that mention it, identified by its own `open_id` from
`/open-apis/bot/v3/info`; without that ID, group messages go unanswered.
Mention placeholders become `@name`, and the bot's own mention is dropped.

**Sending.** Calls use a tenant token cached until ten minutes before it
expires and dropped whenever Feishu reports it invalid. A reply goes to
`POST /open-apis/im/v1/messages/{message_id}/reply`; a message that answers
nothing, such as a background report, goes to
`POST /open-apis/im/v1/messages?receive_id_type=open_id`. Both send `text`
with the part's stable client ID as `uuid`, which Feishu uses to deliver a
resent part once within an hour. Feishu has no single-reply limit, so every
part of a long answer replies to its message. `<at` in outgoing text gets a
zero-width space, so model output can never mention anyone, including `@all`.
Transport failures, HTTP 5xx, 408, and 429, invalid-token codes, and rate-limit
codes (99991400, 230020, 11232, 11233) retry with the same `uuid`; any other
error code is a final refusal, logged by code only, and the reply is held as
for WeChat.

**Untested:** group chats, and sign-in from company accounts whose
administrators must approve apps.

## Background reports

When the owner's session starts a background delegation (an `agent_*` call
with `background: true`; see [tools](tools.md#background-jobs)), the bridge
notes the job from the tool result and keeps that conversation's session open,
without the 30-minute idle limit, until the job is reported. When the server
starts a turn reporting it (`turn.started` with an `origin`), the bridge
answers that turn's approval requests like the owner's own, collects its
answer, and sends it to the owner as an unprompted message (for Feishu, a
message to the owner's `open_id`): recorded as a pending delivery before
sending, retried with the same client ID, and moved to the held-reply store if
the platform refuses it outright. A report that finishes
during one of the owner's turns follows that turn's reply. Only direct chats
receive reports; group and non-owner sessions have no tools.

If an owner turn runs out of time while jobs are running, the bridge cancels
that turn (`turn.cancel`) and keeps the session, rather than replacing it,
which would cancel the jobs; the owner still gets the failure reply.

## Sessions and safety

Each direct-chat sender has one long-lived SCV protocol-v3 socket session. A
group message (a non-empty WeChat `group_id`, or a Feishu chat other than
`p2p`) uses a separate session per group and sender, so group members never see the sender's direct-chat history. Sessions
idle for 30 minutes after their last turn ends are dropped, and at most 32
sessions are live; a new conversation closes the least recently used idle one.

Polling continues while turns run. Each conversation runs its own messages in
order, one turn at a time, so a sender's later messages wait behind its current
turn while other senders are answered. At most four conversations run turns at
once; the rest wait for a slot, and a turn's time limit starts when its slot
does. A conversation may have 8 claimed messages and the account 64; a message
beyond either limit is answered at once with a busy notice asking the sender
to retry, without starting a turn. Each reply goes out with its own message's
context token. A turn the server fails keeps its session; a turn that times
out, or a session whose connection broke, resets only its own conversation's
session (a timed-out turn with background jobs running is cancelled instead;
see Background reports). Replies are delivered in the order turns complete, and a delivery
that keeps failing is retried with backoff without stopping polling or other
turns. Completed assistant output is
sent only after `turn.completed`; failures become short non-sensitive replies.
Network failures use bounded exponential backoff. Retained duplicate message
IDs and interrupted claims do not start another turn. Pending sends retain
their client IDs across recovery; remote exactly-once delivery is not promised.
Raw diagnostics, tool arguments, tool output, tokens, and host paths are not
forwarded as failure details to the chat.

Remote tool authority is a per-account setting, `remote_tools`, that only local
CLI or daemon control can change:

- `none` (default): every remote session uses `session.start` with
  `no_tools: true`, enforced by the server, and the bridge denies any approval
  request.
- `owner`: messages from the account's authenticated owner, the iLink
  `user_id` recorded at QR login or the Feishu `open_id` recorded at sign-in,
  start a full session with every configured SCV tool, and the bridge approves
  that session's approval requests. Other senders, and the owner writing in a
  group, keep `none` behavior. Accounts without a known owner ID grant tools to nobody.
  Owner turns may run for the configured tool ceiling
  (`tools.max_timeout_seconds`, default four hours) plus five minutes, so four
  hours and five minutes by default and at least 30 minutes, instead of 5; the
  ceiling is read from the workspace
  configuration when the component starts. Logout resets the setting
  to `none` before deleting credentials, so a later login never inherits it.

`scv channels run <channel> --remote-tools owner` reports whether the daemon
actually applied the grant; a login without an owner ID leaves it inactive with
a warning. Delegated `agent_claude` and `agent_codex` calls need
their CLIs signed in for SCV first; see `scv agents login` in the
[tools reference](tools.md#signing-in-delegated-agents).

The bridge never invokes `scv exec --yes`.

Credentials are stored at `$SCV_HOME/channels/<channel>/accounts/<account>.json`;
the checkpoint (WeChat's cursor, Feishu's per-chat times) and message IDs,
in-flight claims, pending replies, and held replies are stored at
`$SCV_HOME/channels/<channel>/state/<account>.json`. Claims and pending replies keep the
single-object form older bridges wrote while at most one of each exists, and
become lists when several do; a bridge older than `0.1.23` cannot read state
that holds several. Credentials, settings, and delivery
state use atomic writes and mode `0600` on Unix; parent directories are mode
`0700`. Account names contain only ASCII letters, digits, `_`, and `-`.
Project configuration cannot select accounts, workspaces, or remote authority.

`scv-channels` owns durable state, claims, sender sessions, held replies, and
delivery retries; `scv-clawbot` owns iLink authentication, polling, message
parsing, and sending; `scv-feishu` owns Feishu sign-in, the long connection,
catch-up, message parsing, and sending. `scv-server::components` owns lifecycle and
health. Account selection uses `--account` (default `default`), not project
configuration. The [quality contract](quality.md) defines local-only verification.
