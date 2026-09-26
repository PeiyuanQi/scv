# Channels

Status: supported local bridges for the SCV daemon

A channel connects chat accounts to a local workspace. One command manages
every channel: `scv channels <command> <channel>`. The channels are WeChat,
through its ClawBot iLink HTTP API, and Feishu with its international edition
Lark, through a bot app and Feishu's event long connection (`lark` is accepted
wherever `feishu` is).

Each account runs as a supervised component inside the single SCV daemon,
which remains authoritative for sessions, provider selection, policy, and turn
execution. `scv-channels` holds the bridge every channel shares and, in a
module behind a Cargo feature of the same name, each channel's transport
(`wechat`, `feishu`; both on by default), which supplies only receiving and
sending. `scv-server` runs accounts through `scv_channels::run`; the crate
uses `scv-client` and `scv-protocol` and never depends on the server crate.

## User workflow

```text
scv channels login wechat [--account NAME] [--login-url URL]
scv channels login feishu|lark [--account NAME]
scv channels login feishu|lark --app-id CLI_ID [--owner-open-id OPEN_ID] [--account NAME]
scv channels run <channel> --workspace PATH [--account NAME] [--remote-tools none|owner] [--senders owner|anyone]
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
authority and `--senders` whose messages it answers (see
[Sessions and safety](#sessions-and-safety)); omitting either keeps the saved
value. `stop` persistently disables the account and joins
its component while retaining credentials. `logout` requires a live daemon:
it persists disablement, cancels and joins the component, then removes local
credentials, delivery state, and the account's `[channels]` table. The API has no documented remote
token-revocation operation.

`status` queries the running daemon, showing its PID/version, a
`Channels: <connected> of <enabled> enabled accounts connected` line, and each
selected account as an indented JSON object: its identity, enabled setting,
effective `remote_tools` authority, `senders` setting, health state, restart
count, sanitized error, and last successful contact: an
authenticated, validated WeChat `getupdates`, or for Feishu a connected long
connection that finished its catch-up or its last wait without error. An
account that answers only its owner but has no owner ID on record gets a note
that it answers nobody. `scv
status` prints the same for every account.
Saved credentials are not proof of a connection. If the daemon is unavailable,
connectivity is unknown.

## Lifecycle and settings

The daemon reconciles saved accounts on startup and every two seconds;
`scv reload` triggers immediate reconciliation. It starts each enabled account
once, and stops and joins an old instance before starting a replacement with
updated credentials or settings. Unexpected exits retry with exponential
backoff from 1 to 60 seconds. SIGTERM and Ctrl+C cancel and join components and daemon sessions with
bounded shutdown. All long-running integrations use server supervision.

Each account's settings are a table in the instance's `config.toml`
(`$SCV_HOME/config.toml`, `SCV_HOME` defaulting to `~/.scv`):

```toml
[channels.wechat.default]
enabled = true
workspace = "/absolute/path/to/workspace"
remote_tools = "none"
# senders = "anyone"        # answer every sender; omitted, only the owner
```

A missing table or key defaults to enabled, tool-free, and answering only the
account's owner (`senders = "owner"`). SCV writes `senders` only when it is
`"anyone"`, so a table it edits stays readable by releases before the setting,
which reject it as unknown. An omitted workspace
uses the daemon workspace. `run --account NAME --workspace PATH` persists an
explicit workspace. To opt out offline, set `enabled = false` in the table
before starting the daemon. Login honors this opt-out. `scv channels run`,
`stop`, and `logout` edit only their own table, keeping the rest of the file
and its comments; a person's own edit takes effect at the next reconciliation.

Settings reject unknown fields. The supervisor reads credentials and settings
together through `state::account_snapshot` under a short transaction lock.
A busy snapshot defers that account's reconciliation to a later pass without
stopping its current instance. Invalid settings, or a `config.toml` that does
not parse, fail the account closed; `scv config show` names the problem.
Discovery lists `credentials/<channel>/`, rejecting more than 128 entries
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

A lifetime advisory lock (`state/channels/<channel>/<account>.lock`) prevents
cooperating runners from using the same account concurrently. A separate
transaction file lock (`<account>.transaction` beside it) serializes login
credential writes, settings/state writes, binding, and removal.
State writes recheck the binding, preventing a stale runner from overwriting
another identity's state. These locks are nonblocking: contention returns a
retry error, and network I/O never holds the transaction lock. The daemon's
account commands (enable, disable, settings, logout) retry that error for up to
five seconds, so they wait out a running bridge's state commit instead of
failing. Lock files remain in place after logout so open descriptors cannot
refer to different lock inodes.

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
cursor. Only inbound user messages (`message_type = 1`) with sender ID,
message ID, and context token are accepted, and only when they carry text or a
file (see [WeChat media](#wechat-media)); anything else, such as tool-call
items, is only marked seen. iLink message IDs may be strings up to
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
of it. The busy and voice notices described below are never held. Text replies are split at
Unicode boundaries into messages of at most 16 KiB; only the first carries the
context token. A turn's answer is kept to 64 KiB, and a longer one is cut with
a `[reply truncated]` note instead of failing the turn. Typing indicators are
not sent.

## Feishu contract

Checked live with the owner on 2026-09-24:

- The app works as soon as the scan completes, with no developer console,
  administrator approval, or public URL. It is named "<user name>的飞书 CLI";
  renaming it needs the developer console and a new app version, and no
  registration field or API that sets the name was found.
- Its availability range is the scanner alone: other members of the tenant
  cannot find or message the bot until a new app version (or the tenant's
  admin console) widens it.
- Messages SCV starts reach the owner before the owner has ever written to the
  bot and after long silences; there is no reply-token limit.
- Resending a reply with the same `uuid` returns the same message ID without a
  duplicate.
- Messages sent while SCV is disconnected are not redelivered over the socket,
  but the chat's message list returns them, which is why catch-up exists.
- The app is also subscribed to `im.message.message_read_v1`, which SCV
  acknowledges and ignores.
- Not yet checked: group chats, and company tenants whose administrators must
  approve apps.

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
nobody and, unless it answers anyone, answers nobody. Login says so.

**Receiving.** Each connection starts with
`POST {open}/callback/ws/endpoint` (`AppID`, `AppSecret`), which returns a
`wss` URL and client settings. The URL must be on the brand's domain
(`feishu.cn` or `larksuite.com`) over TLS on port 443; any other host is
refused before dialing, and connection errors never include the URL, which
carries one-time keys. Frames are protobuf `pbbp2.Frame`
(`crates/scv-channels/proto/pbbp2.proto`). SCV pings the connection's
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

**Messages.** Every kind of user message is answered, from whoever the
account answers (see [Sessions and safety](#sessions-and-safety)), except
`system` messages, which are only marked seen; see [Feishu media](#feishu-media) for
files, quotes, and forwarded messages. Rich text (`post`) becomes plain text,
one paragraph per line. In a group (any `chat_type` other than `p2p`) the bot answers
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

## Media

Chat users can send pictures, voice messages, videos, and files, quote
earlier messages, and forward bundles; the owner's agent can send files back.

**Receiving.** The transport turns each message into text, with a marker for
content that has no file (such as `[sticker]` or `[location: Office (31.2,
121.5)]`), and a list of files. Before the message's turn, within its time
limit and turn slot, the bridge fetches what the message refers to (a quoted
message, or a forwarded bundle's messages, shown before the text) and
downloads at most 16 files. Each download is bounded by the account's
[media settings](configuration.md#daemon-and-component-settings): the owner's
files up to `owner_max_mib` (50 MiB), and, on an account that answers anyone,
other senders' images up to `others_image_max_mib` (5 MiB) and never their
other files. The
platform's announced size is checked before downloading and the bytes while
downloading. A file is saved under
`$SCV_HOME/state/media/<channel>/<account>/<conversation>/` as
`<random prefix>-<sender's name>`, with the name stripped of directories,
control characters, and leading dots; files are mode `0600` and directories
`0700`. The conversation directory is a digest of the conversation, so sender
IDs never become paths. Nothing downloaded is ever executed. The type comes
from the platform's declared type, then the file's magic bytes, then its name.

The saved files go to the turn as `turn.start` attachments (see
[protocol](protocol.md#turnstart)): the model sees images directly when it
accepts image input, and the owner's model sees every file's path, so it can
read it or hand it to an agent. A file that is not downloaded leaves a note
for the model, such as `[file a.zip: not opened for this sender]`,
`[image: download failed]`, or `[video clip.mp4: not downloaded, larger than
the 50 MB limit]`. A voice message with the platform's transcript keeps it,
in the attachment or in the note. A message with no text, no downloaded file,
and no voice transcript gets a short reply instead of a turn, such as `SCV can
read text and pictures from you here, but not a file.`

The model cannot listen to audio, so a voice message that carries no
transcript and no text is answered as soon as it arrives with "SCV cannot
listen to voice messages yet. Please type your message instead.": no
download, no daemon session, and no model turn. That is every Feishu voice
message, which comes without a transcript, and a WeChat one whose iLink
transcript is missing. Like the busy notice, the reply is queued as a durable
pending delivery on the message's own reply handle, retried with the same
client ID, and never held if the platform refuses it; the message is then
marked seen like any answered one. It goes to whoever the account answers
(see [Sessions and safety](#sessions-and-safety)), and needs no turn slot.

Files and copies of sent files are removed after `keep_days` (7), checked
when the account starts and hourly.

**Sending.** In an owner's session the model can call
[`chat_attach`](tools.md#sending-files-to-a-chat-chat_attach), which copies
a checked file into `$SCV_HOME/state/media/outbox`. The bridge reads
attachments from the turn's `tool.completed` events, keeps at most 8 regular
files that are inside the outbox, and records them in the reply's durable
pending delivery, each with its own stable client ID. It sends them after the
text parts, in order, recording progress after each, so a restart resumes
with the next file and resends an interrupted one with the same client ID. A
file the platform refuses is skipped and logged; a refused text drops its
files. Each copy is deleted once sent or skipped. Captions go into the reply
text, and files beyond the limit, or outside the outbox, are dropped with a
`[N attached files could not be sent]` note. Background report turns may
attach files too.

### WeChat media

iLink message items have a type: 1 text, 2 image, 3 voice, 4 file, 5 video.
Media items point at the CDN (`https://novac2c.cdn.weixin.qq.com/c2c`): a
`full_url`, or an `encrypt_query_param` for `…/download?encrypted_query_param=`,
and an AES-128 key, either `image_item.aeskey` (hex, preferred for images) or
`media.aes_key` (base64 of the 16 raw bytes, or of their 32 hex digits). The
CDN stores files encrypted with AES-128-ECB and PKCS#7 padding. SCV downloads
only over HTTPS from `qq.com` hosts, decrypts, and checks the padding. Voice
items give their encoding (`encode_type` 6 is SILK, 5 AMR, 7 MP3, 8 Ogg) and
often iLink's transcript (`voice_item.text`); SCV keeps the audio as it is
and passes the transcript. A voice message without one gets the voice reply
described above instead. A quoted message (`ref_msg`) becomes
`[Quoting: <title> | <text or [image]>]` before the text, and its file, if
any, is downloaded like the message's own. File names come from
`file_item.file_name`.

To send, SCV reads the outbox copy, picks the upload type (1 image, 2 video,
3 file), and asks `POST /ilink/bot/getuploadurl` for an address with a random
file key and AES key, the file's MD5 and sizes, and `no_need_thumb`. It posts
the encrypted bytes to the returned `upload_full_url` (or builds
`…/upload?encrypted_query_param=…&filekey=…`), again only HTTPS on `qq.com`,
and takes the CDN's `x-encrypted-param` reply header. Then `sendmessage`
sends an `image_item`, `video_item`, or `file_item` pointing at it, with the
key's hex digits in base64 as `aes_key`. Because iLink delivers one message
per context token, only the reply's first message, text or file, carries the
token; files after text go out unprompted. A 4xx from `getuploadurl` or the
CDN, or a refusal code, is final; other failures retry up to three times with
backoff, then again later with the same client ID.

### Feishu media

Message types map as follows. `image` (`image_key`), `file` (`file_key`,
`file_name`), `audio` (`file_key`, Opus), and `media` (video, `file_key`,
`file_name`) become files, although a voice message, which Feishu sends
without a transcript, gets the voice reply above rather than a download; a
`post` keeps its embedded `img` and `media`
elements as files and `emotion` elements as `[emoji]`. `sticker`,
`share_chat`, `share_user`, `location`, and `interactive` cards become text:
`[sticker]` (Feishu does not serve sticker files), `[shared a group chat]`,
`[shared a contact card]`, `[location: …]`, and `[card]` with the card's
titles and text, at most 2000 characters. `merge_forward` becomes
`[Forwarded messages]`; other types become `[<type> message]`, and `system`
messages are only marked seen. Files download through
`GET /open-apis/im/v1/messages/{message_id}/resources/{key}?type=image|file`
(`image` for images, `file` for the rest), from the message that holds them;
Feishu reports errors as JSON, sometimes with status 200, which SCV treats as
failures.

A reply to an earlier message (`parent_id`) fetches that message through
`GET /open-apis/im/v1/messages/{parent_id}` and shows it as
`[Quoting: <text>]` (or `an image`, `a file`, …), with its files. A forwarded
bundle fetches `GET /open-apis/im/v1/messages/{message_id}`, whose items after
the bundle itself name it in `upper_message_id`: up to 50 of them, and 16 KiB
of text, are listed under `[The forwarded messages:]`, with their files.

To send, SCV uploads with a `multipart/form-data` request: images of at most
10 MiB to `POST /open-apis/im/v1/images` (`image_type=message`), anything else,
larger images included, to `POST /open-apis/im/v1/files` with `file_type`
`pdf`, `doc`, `xls`, `ppt`, or `stream` and the file name. It then sends an
`image` or `file` message like a text part: a reply to the message, or a new
message to the owner's `open_id`, with the file's client ID as `uuid`. Checked
live on 2026-09-24, the scan-created app may read message resources and
upload images and files without any console change. An app that lacks a
scope gets code 99991672; SCV then logs that `im:resource` or `im:message` must
be added in the app's developer console and a version published, and the
model sees `[… download failed]`.

## Background reports

When the owner's session starts a background delegation (an `agent_*` call
with `background: true`; see [tools](tools.md#background-jobs)), the bridge
notes the job from the call's `tool.completed.jobs` (see
[protocol](protocol.md#tool-lifecycle-and-approval)) and keeps that
conversation's session open, without the 30-minute idle limit, until the model
has seen the job's result: in a report turn, or through a later
`agent_wait`, `agent_status`, or `agent_cancel` call. When the server
starts a turn reporting it (`turn.started` with an `origin`), the bridge
answers that turn's approval requests like the owner's own, collects its
answer, and sends it to the owner as an unprompted message (for Feishu, a
message to the owner's `open_id`): recorded as a pending delivery before
sending, retried with the same client ID, and moved to the held-reply store if
the platform refuses it outright. A report that finishes
during one of the owner's turns follows that turn's reply. Only direct chats
receive reports; group and non-owner sessions have no tools.

This is what lets the owner keep chatting while work runs: an owner session
starts with `auto_approve: true`, so the background agents it starts get the
approvals the owner's own turns get, and its system prompt tells the model to
hand real work to background agents and answer at once with the job handle
(see [Delegate first](tools.md#delegate-first)). The owner can ask how a job
is going or have it stopped at any time.

If an owner turn runs out of time while jobs are running, the bridge cancels
that turn (`turn.cancel`) and keeps the session, rather than replacing it,
which would cancel the jobs; the owner still gets the failure reply.

The bridge keeps reading a report turn that starts during one of the owner's
turns and finishes after it, so its answer goes out without waiting for the
owner's next message. It also records each running job in the account's
delivery state (`jobs`: the chat, job handle, delegating tool, and the task
the daemon named, the first line of the delegated prompt) until the job is
reported or its session closes.
A restart ends every session and so every job: on the account's next run, each
chat whose jobs were recorded gets one message listing the jobs that stopped.

## Restarts and notices

An owner can ask SCV from a chat to change, publish, and deploy SCV itself:
the delegated agent runs the feature flow, whose `publish.sh` first asks the
owner in that chat whether to publish (see
[Questions to the owner](#questions-to-the-owner)) and whose `deploy.sh` ends
with `scv restart --when-idle`. The daemon then restarts only once that agent has
finished, its report is stored in the chat's outbox, and no owner message is
being answered, or after ten minutes at the latest (see
[architecture](architecture.md#planned-restarts)). Across the restart:

- Messages whose turns the restart interrupted are answered with "SCV
  restarted to update to vX before finishing this; ask again if you still need
  it." instead of the generic failure reply, on each account's first run after
  a planned restart only.
- Each chat is told which of its background jobs stopped, as above.
- When the watchdog has checked the new release, the daemon announces "SCV
  updated: now running vX (commit)." in the chat that asked, or that the
  update failed and was rolled back, or failed and was not rolled back. If that
  chat does not connect within two minutes, the announcement goes to the
  `[notify]` accounts, saying which chat asked.

Notices nobody asked for (an update started from a terminal, a restart after
the daemon stopped unexpectedly, an enabled account disconnected for ten
minutes, which may mean its sign-in expired) go to the owner of the first
connected account in `[notify].owner`, or else to the chat the owner last
wrote from, and never through the account the notice is about. Each is queued
in that account's outbox like a background report. See
[configuration](configuration.md#daemon-and-component-settings).

## Questions to the owner

Before a step that cannot be undone, a delegated agent (or anything else on
the host) can ask the owner yes or no with `scv confirm [--timeout SECS]
QUESTION`; the feature flow's `publish.sh` asks this way before publishing SCV
to crates.io. The question goes to the chat that started the work, found as
for a planned restart: the caller's `SCV_PARENT` chain names the delegation,
the delegation its daemon session, and the hub the direct chat that session
answers. Work that did not start in a chat, such as a TUI session, asks where
unprompted notices go: the owner of the first connected `[notify].owner`
account, or else the chat the owner last wrote from. Only an account owner's
direct chat can be asked; with none reachable, nothing is asked.

The daemon queues the question in that account's outbox like a notice, as one
unprompted message:

```text
<question>

Reply yes or no. No answer in <N> minutes counts as no.
```

Once the question is in the outbox, the owner's next direct message in that
chat that is an explicit answer decides it. After trimming, lowercasing, and dropping trailing `.`, `!`, `。`,
and `！`, the words `yes`, `y`, `ok`, `okay`, `是`, `是的`, `好`, `好的`,
`确认`, `可以`, and `同意` mean yes, and `no`, `n`, `否`, `不`, `不要`, `取消`,
`算了`, and `stop` mean no; a message with files is not an answer. An answer
starts no turn: the bridge replies to it "OK, going ahead." or "OK, stopped."
once that reply is durable, and only then hands the answer to the asker. Any
other message runs as a normal turn while the question keeps waiting. Other
senders and group messages, the owner's own in a group included, never answer.
With no answer in time (default 30 minutes, at most 4 hours) the chat is told
"No answer, so stopped."; an asker that stops following the question for a
minute (it was killed) has it withdrawn, and the chat is told "The question
was withdrawn, so stopped."

A chat holds at most one question; asking again while one waits is refused.
Questions live only in the daemon's memory, in the channel hub next to the
notices, and a daemon restart drops them. `scv confirm` exits 0 for yes; 1 for
no or no answer in time; and 2 when nothing could be asked or the answer was
not learned: no daemon, a daemon too old for the command, no owner chat to
ask in, a question already waiting there, or the daemon restarting while it
waited. A delegated agent may run it; it manages nothing.

## Sessions and safety

Whose messages an account answers is a per-account setting, `senders`, that
only local CLI or daemon control (or an edit of `config.toml`) can change:

- `owner` (default): only the account's authenticated owner, the iLink
  `user_id` recorded at QR login or the Feishu `open_id` recorded at sign-in,
  is answered, in direct chats and, when a message mentions the bot, in
  groups. Anyone else's message is dropped silently: no reply or busy notice,
  no download, no daemon session, and no model turn. It is still marked seen
  and checkpointed like any handled message, so it is never replayed, and the
  log records only that the account ignored a message from someone other
  than its owner, never who sent it or what it said. An account with no known
  owner ID answers nobody; `scv channels status`, `scv config show`, and login
  say so.
- `anyone`: every sender who can reach the bot is answered, tool-free unless
  it is the owner holding remote tools (below).

Each direct-chat sender has one long-lived SCV protocol-v3 socket session. A
group message (a non-empty WeChat `group_id`, or a Feishu chat other than
`p2p`) uses a separate session per group and sender, so group members never see the sender's direct-chat history. Sessions
idle for 30 minutes after their last turn ends are dropped, and at most 32
sessions are live; a new conversation closes the least recently used idle one
that has no background jobs running or reports to send, and gets the busy
notice when every live conversation is busy that way.

Every session starts with `channel` set to the channel's name as its users know
it (`WeChat`, or `Feishu`/`Lark` by the account's brand), so the model knows it
is writing chat messages: short plain text, one message per turn, with no tool
output visible to the user (see [protocol](protocol.md#sessionstart)).

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
a warning. `--senders` likewise reports what the daemon applied, and warns when
an owner-only account has no owner ID and so answers nobody. Logout resets
`senders` to `owner` along with the tool grant. Delegated `agent_claude` and `agent_codex` calls need
their CLIs signed in for SCV first; see `scv agents login` in the
[tools reference](tools.md#signing-in-delegated-agents).

The bridge never invokes `scv exec --yes`.

Credentials are stored at `$SCV_HOME/credentials/<channel>/<account>.json`;
the checkpoint (WeChat's cursor, Feishu's per-chat times) and message IDs,
in-flight claims, pending replies, and held replies are stored at
`$SCV_HOME/state/channels/<channel>/<account>.json`. Claims and pending replies keep the
single-object form older bridges wrote while at most one of each exists, and
become lists when several do; a bridge older than `0.1.23` cannot read state
that holds several. Credentials, settings, and delivery
state use atomic writes and mode `0600` on Unix; the directories between the
SCV home and them are mode `0700`. Account names contain only ASCII letters, digits, `_`, and `-`.
Project configuration cannot select accounts, workspaces, or remote authority.

The shared bridge in `scv-channels` owns durable state, claims, sender
sessions, held replies, media fetching, storage, and retention, and delivery
retries; its `wechat` module owns iLink authentication, polling, message
parsing, the CDN's encryption, and sending; its `feishu` module owns Feishu
sign-in, the long connection, catch-up, message parsing, resources, uploads,
and sending. `scv-server::components` owns lifecycle and
health. Account selection uses `--account` (default `default`), not project
configuration. The [quality contract](quality.md) defines local-only verification.
