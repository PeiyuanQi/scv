# SCV WeChat Channel (ClawBot)

The WeChat channel for the SCV daemon: an iLink ClawBot transport for the
shared bridge in `scv-channels`.

`login(base_url, account)` renders the QR code and saves credentials without
printing the token. `run_supervised(token, base_url, account, workspace, socket,
tool_owner, cancellation, report)` runs one account until cancelled; `tool_owner`
carries the owner's iLink `user_id`, recorded at QR login. Health turns true
only after an authenticated, validated `getupdates` response. HTTP redirects are
disabled, including during login, and responses are bounded before parsing.

`state::Account` preserves old credentials while optionally recording `bot_id`
and `user_id`; it intentionally has no `Debug` implementation. Its fingerprint
covers the normalized API origin and the authenticated bot/user IDs, so a token
rotation for the same identity preserves delivery state; credentials without
both IDs are bound to their token.

State lives in `<SCV home>/channels/wechat`. `state::migrate` moves the
directory releases before channels used, `<SCV home>/clawbot`, there in one
rename while holding every account's locks, and refuses when both exist. The
earliest single-file credentials, `<SCV home>/clawbot.toml`, are read as the
`default` account. Old **0.1.9 standalone ClawBot processes do not honor the
account locks**; stop them manually before enabling the supervised component.

iLink accepts one message per context token, so only the first part of a reply
carries it and later parts go out unprompted. Poll batches above 4096 messages
are rejected before any message executes or the cursor advances.

Focused verification:

```sh
cargo test -p scv-clawbot --locked
cargo clippy -p scv-clawbot --all-targets --locked -- -D warnings
cargo fmt -p scv-clawbot --check
```

Tests use isolated temporary stores and local HTTP/Unix sockets, with no process
environment mutation and no live WeChat credentials.
