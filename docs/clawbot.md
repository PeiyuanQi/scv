# ClawBot / WeChat iLink

Status: proposed design for the ClawBot bridge

SCV connects a local workspace to WeChat ClawBot through the iLink HTTP API.
The bridge is a local process; SCV's existing stdio server remains authoritative
for sessions, tools, policy, approvals, and turn execution.

## User workflow

```text
scv clawbot login
scv run --workspace PATH
scv start --workspace PATH
scv stop
scv restart --workspace PATH
scv status
scv clawbot logout
```

`login` renders the QR code and prints its URL, reports waiting/scanned/
confirmed/expired states, and stores credentials without printing the token.
`run` is the single foreground daemon; `start`, `stop`, `restart`, and `status`
control its user-level service. `logout` removes local credentials. The API has
no documented remote token-revocation operation.

## iLink contract

Use the returned `baseurl` after login, falling back to
`https://ilinkai.weixin.qq.com`. Login uses `GET
/ilink/bot/get_bot_qrcode?bot_type=3`, then `GET
/ilink/bot/get_qrcode_status?qrcode=...` until confirmed, expired, or timed
out. Confirmation must provide `bot_token`, `ilink_bot_id`, and
`ilink_user_id`.

Authenticated calls use JSON, `AuthorizationType: ilink_bot_token`, bearer
authorization, and a fresh `X-WECHAT-UIN` containing base64 of a random `u32`.
Bodies include `base_info.channel_version = "1.0.0"`. `ret != 0`, `errcode`,
and `errmsg` are validated and converted to redacted bridge errors; `-14`
pauses the account and requests re-login.

`POST /ilink/bot/getupdates` long-polls with the opaque `get_updates_buf`
cursor. Only inbound user text messages with sender ID, message ID, context
token, and non-empty text are accepted. Every inbound message is durably marked
accepted or ignored before its response cursor is persisted.
`POST /ilink/bot/sendmessage` echoes the original
`context_token`, uses `message_type = 2`, `message_state = 2`, and a unique
`client_id`. Pending sends retain that client ID for retry. Text replies are
split at Unicode boundaries under the configured size limit. Media, typing,
and uploads are deferred.

## Sessions and safety

Each sender has one SCV protocol-v2 child session. Completed assistant output is
sent only after `turn.completed`; failures become short non-sensitive replies.
Network failures use bounded exponential backoff with jitter. Duplicate inbound
message IDs never start a second turn. Raw diagnostics, tool arguments, tool
output, tokens, and host paths are never sent to WeChat.

The current bridge invokes `scv exec --yes` for incoming messages, so run it in
a dedicated workspace and treat incoming WeChat users as trusted operators.

Credentials are stored at `$SCV_HOME/clawbot.toml`; cursor,
message IDs, and pending delivery records are under
`$SCV_HOME/clawbot/state/<account>/`. Files are atomic and mode 0600 on Unix.
Project configuration cannot select accounts, workspaces, or remote authority.

## Configuration

```toml
[clawbot]
default_account = "personal"
poll_timeout_seconds = 45
retry_max_seconds = 60
max_concurrent_senders = 4
max_pending_messages = 256
max_reply_bytes = 16384
approval_mode = "never" # or "operator"
approval_timeout_seconds = 300
```

`scv-clawbot` owns typed iLink models, authentication, polling, durable state,
sender sessions, and local approvals. Tests use fake iLink and SCV servers to
cover QR expiry, headers, returned-host selection, response errors, restart
recovery, retry idempotency, sender isolation, approval behavior, redaction,
and shutdown. No test contacts WeChat.
