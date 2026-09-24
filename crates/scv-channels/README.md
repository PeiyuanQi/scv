# SCV Channels

The bridge every SCV chat channel shares. A channel crate implements
`Transport` (receive a batch of messages after a checkpoint, send one part of a
message) and `state::Credentials` (how its saved credentials bind delivery
state); `run(transport, account, workspace, socket, tool_owner, store, running,
report)` does the rest on the caller's daemon socket. It launches no process and
spawns no tasks: cancelling the future drops active requests and protocol
sessions, and callers enforce an external bounded stop timeout. `report(true)`
follows only a successful `receive`; receive and send failures report false.

`run_linked` adds a `hub::Link` for accounts the daemon runs. Through the
daemon's `hub::Hub` it sees which direct chat each daemon session answers,
how many owner messages are claimed but unanswered, each chat's background
work not yet reported, and the chat the account owner last wrote from; it can
queue a notice into an account's durable outbox. Each account's first
recovery after a planned restart answers interrupted claims with
`restarted_reply` instead of the failure reply.

`tool_owner` is the authenticated owner when the account grants remote tools;
only that sender's direct-chat sessions get tools and auto-approval, and every
other session, including the owner's group messages, stays tool-free.

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

`state::Store<C>` keeps one channel's accounts where the instance layout puts
them: credentials in `<SCV home>/credentials/<channel>/<account>.json`,
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

Focused verification:

```sh
cargo test -p scv-channels --locked
cargo clippy -p scv-channels --all-targets --locked -- -D warnings
```
