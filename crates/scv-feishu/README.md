# SCV Feishu Channel

The Feishu (and Lark) channel for the SCV daemon: a transport for the shared
bridge in `scv-channels` over Feishu's event long connection and Open
Platform API. It needs no public URL, webhook, or developer console.

`login::login(account, brand)` creates a bot app by the device flow Lark's own
CLI uses: it prints a terminal QR code, waits for the user's scan, and saves
the app ID and secret with the creator's `open_id` as the owner.
`login::login_existing` adds an existing app after checking its secret with
Feishu. `run_supervised(credentials, account, workspace, socket, tool_owner,
cancellation, report, link)` runs one account until cancelled; `tool_owner`
carries the owner's `open_id`, and `link` connects the account to the
daemon's hub (`scv_channels::hub`).

The transport connects through `callback/ws/endpoint`, accepting only a `wss`
host on the brand's domain, and speaks the `pbbp2.Frame` protobuf of
`proto/pbbp2.proto` (derived with prost; no protoc). It pings at the server's
interval, reassembles split events within bounds, and acknowledges a message
event only when the bridge asks for the next batch, after the event's claim
is durable. Feishu does not redeliver messages sent while SCV was away, so
every connection first lists the history of the chats in its checkpoint since
their newest message (at most 24 hours, 32 chats, four pages of 50 each).
Sends use a cached tenant token and a stable `uuid` per part, which makes
retries idempotent; outgoing `<at` tags are broken so replies never mention
anyone. In groups the bot answers only messages that mention it.

`state::Account` has a redacting `Debug`. Its fingerprint covers the brand,
app ID, and owner, so a rotated secret keeps delivery state. Credentials live
in `<SCV home>/credentials/feishu`, settings in `[channels.feishu.<account>]`
of `<SCV home>/config.toml`, and delivery state in
`<SCV home>/state/channels/feishu`.

Focused verification:

```sh
cargo test -p scv-feishu --locked
cargo clippy -p scv-feishu --all-targets --locked -- -D warnings
cargo fmt -p scv-feishu --check
```

Tests use fake HTTP and WebSocket services on loopback and temporary stores,
with no live Feishu credentials.
