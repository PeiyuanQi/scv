# ClawBot / WeChat iLink

Status: supported local bridge for the SCV daemon

SCV connects a local workspace to WeChat ClawBot through the iLink HTTP API.
The bridge is a local process. The single SCV Unix-socket daemon remains
authoritative for sessions, provider selection, policy, and turn execution.

## User workflow

```text
scv clawbot login [--account NAME]
scv clawbot run --workspace PATH [--account NAME]
scv clawbot status [--account NAME]
scv clawbot logout [--account NAME]
```

`login` renders the QR code and prints its URL, reports waiting/scanned/
confirmed/expired states, and stores
credentials without printing the token. `run` validates the account and
workspace, then reports connected, reconnecting, expired-session, approval,
and stopped states. `status` distinguishes saved credentials from a running
bridge. Remote approval requests are denied by default. `logout` removes local credentials and state. The API has no documented
remote token-revocation operation.

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

Each sender has one long-lived SCV protocol-v2 socket session. Completed assistant output is
sent only after `turn.completed`; failures become short non-sensitive replies.
Network failures use bounded exponential backoff with jitter. Duplicate inbound
message IDs never start a second turn. Raw diagnostics, tool arguments, tool
output, tokens, and host paths are never sent to WeChat.

Remote sessions run with no tools enabled. The bridge never invokes
`scv exec --yes`.

Credentials are stored at `$SCV_HOME/clawbot/accounts/<account>.json`; cursor
and message IDs are stored at `$SCV_HOME/clawbot/state/<account>.json`. Files
are atomic and mode 0600 on Unix; their parent directories are mode 0700.
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
