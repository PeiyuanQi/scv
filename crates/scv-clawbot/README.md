# ClawBot Lifecycle

`run_supervised(token, base_url, account, workspace, socket, tool_owner,
cancellation, report)` connects to the caller's daemon socket. `tool_owner` is
the authenticated owner's iLink `user_id` when the account grants remote tools;
only that sender's direct-chat sessions get tools and auto-approval, and every
other session stays tool-free. It launches no process and
spawns no adapter tasks. Cancelling drops active HTTP requests and protocol
sessions; callers should enforce an external bounded stop timeout. The health
callback reports true only after a successful authenticated, validated
`getupdates` response. Transport, decoding, HTTP, and API failures report false.
HTTP redirects are disabled, including during login.

`state::Account` preserves old credentials while optionally recording `bot_id`
and `user_id`. It intentionally has no `Debug` implementation.
`state::AccountSettings` defaults to enabled with no workspace override and
`remote_tools: none`.
`state::settings`, `save_settings`, and `account_names` support supervisor
configuration and discovery. Discovery fails on more than 128 entries (including
the legacy default account) or directory-entry errors, validates names, and
leaves individual credential validation to the caller. Settings reject unknown
fields and live separately in `clawbot/settings` with
private directory and file permissions, using the same atomic writes as state.

Before any inbound turn, the bridge durably records its message identity,
sender, and context. On restart, that marker becomes a sanitized failure reply
without repeating the turn. Pending replies and stable per-chunk client IDs are
durable before delivery; retries reuse the IDs. Deduplication retains the newest
4096 IDs, including replies recovered before the first poll. Protocol replies
are bounded to 16 KiB; oversize output becomes a sanitized failure.

State is bound to a SHA-256 fingerprint of the normalized API origin and the
authenticated bot/user IDs. A token rotation for the same known identity and
origin preserves cursor, pending deliveries, and stable send IDs. Credentials
without both IDs use a token-based fingerprint. Legacy unbound state is bound
before first use with the currently saved credentials. A mismatched binding
fails with a generic error before polling, recovery, or delivery.

Login refuses identity/origin replacement, including replacement of legacy
credentials with identified credentials, and asks for explicit logout first.
It never resets or archives another runner's state. Logout discards credentials
and delivery state after the component stops; a later login starts fresh.
Each deletion syncs its parent directory. Both credential locations are removed
durably before state and settings, preventing a crash from restoring credentials
after the disabled setting has been deleted.

A separate short transaction lock serializes credential/settings writes, state
binding/writes, migration, and logout. `state::account_snapshot` reads credentials
and settings together. State writes recheck the binding, so a stale runner cannot
overwrite another identity's state. No network I/O holds the transaction lock.
Both locks are nonblocking; contention returns a transient retry error rather
than blocking a runtime worker or shutdown.

Poll batches above 4096 messages are rejected before any message executes or the
cursor advances. Previously seen IDs encountered in an accepted batch move to
the newest end of the deduplication window, retaining the entire processed batch
until its cursor commits.

A per-account advisory lock is held throughout each run, including the legacy
`run` compatibility wrapper. `state::remove` requires the caller to stop the
component first and refuses deletion while a cooperating runner holds the lock.
Lock files remain in place so competing descriptors cannot lock different
inodes. Old **0.1.9 standalone ClawBot processes do not honor these locks**;
stop them manually before enabling the supervised component.

Focused verification:

```sh
cargo test -p scv-clawbot --locked
cargo clippy -p scv-clawbot --all-targets --locked -- -D warnings
cargo fmt -p scv-clawbot --check
```

Tests use isolated temporary stores and local HTTP/Unix sockets, with no process
environment mutation and no live WeChat credentials.
