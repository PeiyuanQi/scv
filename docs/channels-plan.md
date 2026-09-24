# Channels Implementation Plan

Status: approved; in progress

## Objective

Let people reach their SCV instance from the chat apps they already use, with
one command for every chat platform and a sign-in as easy as scanning a QR
code. WeChat is the first channel; Feishu (and its international edition,
Lark) is the second.

## Decisions

- One command manages every channel:
  `scv channels <login|run|stop|status|logout> <channel> [--account NAME]`.
  `status` without a channel lists every channel and account. `scv clawbot`
  and `scv clawbot-login` are removed without aliases. Channel names are
  `wechat` and `feishu`, with `lark` accepted as an alias of `feishu`.
- Options that belong to one platform stay on that platform: WeChat keeps its
  iLink `--login-url`; Feishu needs none, because its sign-in reports whether
  the account is on Feishu or Lark.
- Each channel keeps its state in `$SCV_HOME/channels/<channel>/`, with the
  same private layout (`accounts`, `settings`, `state`, `locks`,
  `transactions`) and the same account-name rules.
- Daemon components are named `<channel>:<account>` and report a `channel`
  field. Control actions are `channel_set` and `channel_logout`, each naming
  its channel.
- The transport-independent bridge logic is shared by every channel: durable
  claims before acknowledgement, deduplication, pending and held replies,
  per-conversation sessions and limits, owner-only remote tools that never
  apply in groups, and background reports routed by `TurnOrigin`. A channel
  supplies only its transport: sign-in, receiving messages, and sending them.
- Only the account's authenticated owner can receive remote tools, as for
  WeChat today: the owner is the identity recorded at sign-in.

## Steps

Each step is one PR landed through the feature-flow skill. Small steps merge
without a version bump and ship together, keeping crates.io under 20 versions
a day.

| # | Step | Depends on |
|---|---|---|
| 1 | `scv channels` for WeChat; state moves to `channels/wechat` (done, 0.1.35) | — |
| 2 | Shared bridge core crate `scv-channels`; `scv-clawbot` becomes the WeChat transport | 1 |
| 3 | Feishu live check with the owner | — |
| 4 | Feishu channel: QR sign-in, manual fallback, long connection, owner tools | 2, 3 |
| 5 | Feishu background reports as unprompted messages | 4 |
| 6 | Feishu cards: streaming progress and a typing reaction (optional) | 4 |

### 1. `scv channels` for WeChat

`scv channels login|run|stop|status|logout wechat` replace the `scv clawbot`
commands with the same behavior. WeChat state moves from `$SCV_HOME/clawbot`
to `$SCV_HOME/channels/wechat` in one rename, done by the daemon at each
reconciliation and by every `scv channels` command, while every account's
lifetime and transaction locks are held. Both directories existing is refused
and reported, never merged.

### 2. Shared bridge core

A new crate, `scv-channels`, holds the bridge logic and the state store, and
defines the transport trait. `scv-clawbot` keeps iLink sign-in, polling,
message parsing, and sending behind that trait. The dependency chain becomes
`server -> clawbot -> channels -> client -> protocol`. No behavior changes.

### 3. Feishu live check

A one-off script, run once with the owner scanning, answers what the design
cannot assume:

- whether the app the QR sign-in creates (`archetype=PersonalAgent`) already
  has the bot capability, message scopes, and long-connection events, since
  Feishu's own OpenClaw installer configures nothing after the scan;
- whether a company account can finish sign-in without an administrator
  approving the app;
- whether events that arrive while SCV is offline are delivered after it
  reconnects; if not, SCV catches up by listing the chat's messages since the
  last one it saw;
- whether an unprompted message is delivered after a long silence.

### 4. Feishu channel

- **Sign-in** (`scv channels login feishu`): the device flow that Lark's own
  CLI (`lark-cli config init`) and Feishu's OpenClaw installer use.
  `POST https://accounts.feishu.cn/oauth/v1/app/registration` with
  `action=init` (require `client_secret` in `supported_auth_methods`), then
  `action=begin` with `archetype=PersonalAgent`, `auth_method=client_secret`,
  and `request_user_info=open_id tenant_brand`. SCV shows a terminal QR code
  for `https://open.feishu.cn/page/cli?user_code=…`, which the user scans with
  the Feishu app they already have. SCV then polls with `action=poll` at the
  returned interval until `expire_in`: `authorization_pending` keeps waiting,
  `slow_down` adds five seconds, and `access_denied` or `expired_token` stops.
  When `user_info.tenant_brand` is `lark`, polling switches once to
  `accounts.larksuite.com`. The result is the app ID and secret, the owner's
  `open_id`, and the brand. Feishu issues `open_id` per app, so it matches the
  sender ID of the owner's messages to this bot.
- **Manual fallback**: `scv channels login feishu --app-id cli_…` reads the
  app secret from a hidden prompt or stdin, never an argument, and validates
  it with `/open-apis/auth/v3/tenant_access_token/internal` before writing
  anything. It covers company accounts whose administrators approve apps,
  existing bot apps, and a change to the registration endpoint, which Feishu
  does not publicly document.
- **Receiving**: a native Rust client for Feishu's long connection, modelled
  on `oapi-sdk-go/ws`. `POST {open}/callback/ws/endpoint` with the app
  credentials returns a `wss` URL and `PingInterval` and reconnect settings.
  Frames are protobuf (`pbbp2.Frame`); control frames carry ping and pong,
  and data frames carry events split by the `sum` and `seq` headers. SCV
  subscribes to `im.message.receive_v1` and acknowledges an event only after
  its claim is durable. No public URL or webhook is needed.
- **Sending**: a cached `tenant_access_token`, renewed before its two-hour
  expiry. Replies use `im/v1/messages/{message_id}/reply`, and every send
  carries a stable `uuid` so a retry is delivered at most once. Feishu has no
  single-use reply token like iLink's `context_token`, so replies need no
  unprompted continuations.
- **Safety**: only the Feishu and Lark hosts and the socket host they return
  are trusted, redirects are not followed, and response sizes are bounded.
  The app secret is stored in `$SCV_HOME/channels/feishu/accounts/<name>.json`
  with mode `0600` and never printed; status shows the app ID and owner only.

### 5. Feishu background reports

Finished background jobs reach the owner through
`im/v1/messages?receive_id_type=open_id`, with the same durable pending
delivery and stable `uuid` as replies.

### 6. Feishu cards

Streaming cards (CardKit) can show `tool.progress` events while a turn runs,
which WeChat cannot. A reaction on the inbound message can serve as a typing
signal.
