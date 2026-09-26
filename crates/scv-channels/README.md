# SCV Channels

SCV's chat channels and the bridge they share. Each channel is a module behind
a Cargo feature of the same name, both on by default:

- `wechat`: WeChat through a ClawBot (iLink) bot: QR sign-in, long-polled
  `getupdates`, replies, and files on the AES-encrypted CDN;
- `feishu`: Feishu and Lark through a bot app: sign-in by QR scan (which
  creates the app) or with an existing app, the event long connection with
  catch-up from chat history, replies, and files.

A channel implements `Channel` (`WeChat`, `Feishu`): signing an account in
under a `Layout`, what its credentials say (owner, bot, platform name), and
running it. The daemon reaches channels only through `ChannelKind` (`ALL`,
`name`, `parse`, `accounts`), `ChannelCredentials`, `Accounts` (one channel's
saved accounts: discovery, snapshots, settings, removal, inspection), and
`run(AccountRun)`. `AccountRun` carries the instance layout, the account, its
credentials and whole settings table, the account owner (whether or not it
holds the remote-tool grant), the owner turn timeout exactly when it does, the
workspace, the daemon socket, the `hub::Link`, and a health callback. `run`
launches no process and spawns no tasks: dropping its future drops active
requests and protocol sessions, and callers enforce an external bounded stop
timeout. The health callback reports `true` only after a successful receive,
and `false` when receiving, sending, or the run fails. Nothing in the crate
reads `SCV_HOME`: accounts, their state, and their media (under
`Layout::media`, with the shared `Layout::outbox`) are found through the
`Layout` the caller passes.

Beyond those, the public API is the `hub` module (below); the account
settings the daemon validates and edits (`state::AccountSettings`,
`state::validate_name`, `media::MediaSettings`); `state::Credentials`, which
a channel's credentials implement; the reply-file size limit
(`media::MAX_REPLY_FILE_BYTES`); `owner_turn_timeout`; and each channel's
`CHANNEL` name, `Account` credentials, and `Login` (plus Feishu's `Brand`).
Everything else, including the bridge, the transports, and the store, is
crate-private.

Inside the crate each channel supplies a `Transport` (receive a batch of
messages after a checkpoint, send one part of a message, and optionally a
label for the messages SCV writes itself, which the bridge then sends in a
Markdown code block; WeChat sets it to `system msg: `) and
`state::Credentials` (how its saved credentials bind delivery state); the
shared bridge does the rest on the daemon socket. `intake::classify` decides,
without side effects, what the bridge does with each received message: ignore
it, leave an existing claim alone, answer with the busy notice, or claim it
and run a turn in its conversation.

Through the daemon's `hub::Hub` the daemon sees which direct chat each daemon
session answers, how many owner messages are claimed but unanswered, each
chat's background work not yet reported, and the chat the account owner last
wrote from; it can queue a notice into an account's durable outbox. Each
account's first recovery after a planned restart answers interrupted claims
with `restarted_reply` instead of the failure reply.

Only the account owner's direct-chat sessions get tools and auto-approval,
and only when the account grants the owner remote tools; every other session,
including the owner's group messages, stays tool-free.

Before any inbound turn, the bridge durably records its message identity,
sender, reply handle, and conversation, and saves the batch's checkpoint only
after every claim in it is durable. On restart, each claim becomes a sanitized
failure reply without repeating the turn, and each direct chat whose
background jobs were still running (recorded in state as they start) is told
which ones stopped. Pending replies and stable per-part
client IDs are durable before delivery; retries reuse the IDs. A refusal is
final for that reply, which is held and delivered ahead of the conversation's
next reply within count, byte, and age limits. Deduplication retains the
newest 4096 IDs, including replies recovered before the first receive.
Conversations run their turns in order, at most four at once, with bounded
queues answered by a busy notice beyond them.

Inside the crate, `state::Store<C>` keeps one channel's accounts where the
instance layout puts them: credentials in
`<SCV home>/credentials/<channel>/<account>.json`,
settings as `[channels.<channel>.<account>]` in `<SCV home>/config.toml`
(edited in place, keeping the rest of the file and its comments), and delivery
state with its `.lock` and `.transaction` files in
`<SCV home>/state/channels/<channel>`, with private directories, mode `0600`
files, and atomic writes.
State is bound to the credentials' fingerprint; a mismatched binding fails
before receiving, recovery, or delivery, and replacing an account's identity
requires logout first. A short, nonblocking transaction lock serializes
credential, settings, and state writes, binding, and removal, and no network
I/O holds it. A lifetime lock is held throughout each run; `remove` refuses
while one is held. `inspect` reads an account for display without any lock.
Discovery fails on more than 128 entries or
directory-entry errors and leaves credential validation to the caller.

The Feishu long connection's frames follow `proto/pbbp2.proto`.

Focused verification:

```sh
cargo test -p scv-channels --locked
cargo clippy -p scv-channels --all-targets --locked -- -D warnings
cargo clippy -p scv-channels --all-targets --locked --no-default-features --features wechat -- -D warnings
cargo clippy -p scv-channels --all-targets --locked --no-default-features --features feishu -- -D warnings
```
