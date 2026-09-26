# Testing and Performance

Status: current correctness and performance contract

SCV correctness tests require no network, provider credential, or installed
third-party agent. The correctness contract covers:

- core completion, grouped context selection, repeatable history trimming,
  history-limit failure, multi-step tools, denials, maximum steps, usage
  aggregation, and duplicate tool registration;
- OpenAI-compatible request shaping, fragmented streamed text/tool arguments,
  usage, bounded error handling, and credential redaction through a local fake
  server;
- protocol round trips and forward-compatible additive fields; every error
  code and tool error kind keeping its wire name, and unknown event types,
  codes, and kinds parsing as `unknown`;
- a frozen copy of every event 0.2.2 sent decoding with today's types and
  encoding back to the same bytes, and a failed call adding only its `error`
  kind, so the previous release (the planned-restart watchdog) reads what the
  new server sends;
- tool results typed by `ToolFailure`, the model's text byte-identical (the
  denial message included), and the TUI and the agent fallback reading the
  kind rather than the text;
- filesystem containment, symlink escape, bounded reads, atomic writes, stale
  hashes, process timeout, process-group cancellation, bounded process output,
  and native-agent argument/cwd and model/effort mapping behavior;
- configuration trust boundaries, stricter project limits, and cross-field
  bounds;
- bounded client/server frame reading, CRLF boundaries, server handshake,
  session startup, and a complete streamed turn through a fake provider; and
- TUI prompt history, Unicode editing, primary-region rendering, bounded frame
  reads, and authoritative session clearing.

Daemon and component changes require focused coverage for:

- daemon control handshake, action serialization, status responses, bounded
  requests, and no mutation replay after ambiguous failures;
- one component per account, recovery with 1-to-60-second backoff, sanitized
  health, successful-contact timestamps, and credentials not implying connected;
- default autostart, persistent stop, login honoring opt-out, periodic and
  explicit reconciliation, and joining before credential/settings replacement;
- logout joining before deletion, private settings/state, interrupted in-flight
  claims preventing replay, pending delivery retaining client IDs, live send
  acknowledgements without `ret` (empty or `{}`) completing delivery once, and
  explicit send rejections or permanent 4xx statuses dropping the reply and
  resuming polling, with only integer codes logged;
- remote tools only for an `owner`-mode account's known owner: owner sessions
  start with tools and auto-approve, while other senders and unknown owners
  stay tool-free and deny approvals, as do the owner's group messages (string
  or non-string `group_id`); the setting persists when omitted;
- delegated agents rejecting prompts that start with `-` and models that start
  with `-` or `@`, and signed-out agent failures gaining a
  `scv agents login <name>` hint while other failures do not;
- Codex import copying `config.toml` and API-key `auth.json` atomically with
  mode `0600` into the instance agent home, never copying a ChatGPT session,
  flagging `env_key` providers, printing no secrets, and writing nothing when
  either input is invalid;
- every agent descriptor self-consistent (templates carry their placeholder,
  stored credentials live under the relocated state directory), prompt flags
  placed just before the prompt, per-adapter and `*_API_KEY` removal keeping
  the adapter's own state variables, per-user install directories winning over
  `PATH`, and uninstalled agents not offered;
- DeepSeek Harness keys and pi endpoints written in the agent's native files
  with mode `0600` from a piped key, merged without disturbing other entries,
  validated before any write, reported and removed without printing a key, and
  pi importing SCV's own provider;
- identity/origin binding, same-identity token rotation, conservative legacy
  binding, replacement requiring logout, and stale-runner write rejection;
- account settings read from and written to `[channels.<channel>.<account>]`
  in `config.toml`, keeping a person's other tables and comments, taking effect
  at the next reconciliation, and failing the account closed when invalid;
  files of the layout before `0.2.0` never read;
- `scv config show` naming each setting's origin, hiding every credential,
  and listing entries of the home SCV does not read; `scv agents import`
  records reporting a changed source;
- nonblocking transaction/lifetime locks, serialized login/removal, atomic
  account snapshots, busy snapshots deferred without stopping the current
  instance, and strict settings/discovery validation;
- batches above 4096 messages rejected before execution or cursor advancement,
  responses and string IDs byte-bounded, unsigned 64-bit integer message IDs
  preserved exactly, and encountered duplicate IDs retained through the batch
  checkpoint;
- iLink requests carrying the account's bearer token, only trusted origins
  accepted (including a returned regional host) with the TLS port pinned and
  redirects not followed, oversized responses rejected before parsing, and
  replies chunked on UTF-8 boundaries within the byte limit;
- Feishu: registration posting `init`, `begin`, and `poll` forms, waiting out
  pending polls, following a Lark tenant once, and stopping when declined or
  when secret-based apps are not offered; the long-connection URL refused
  before dialing unless it is on the brand's domain over TLS; frames round
  tripping with their required fields, split events reassembled within bounds,
  a ping on connect, and message events acknowledged only on the next receive
  while other events are acknowledged at once; catch-up from chat history on
  every (re)connection, skipping older, deleted, and app messages; group
  messages answered only when they mention the bot; replies and direct
  messages retried with the same `uuid`, refusals final, invalid tokens
  renewed, and outgoing `<at` tags broken; a caught-up message answered
  through the full bridge; app secrets absent from debug output and status;
  channel login options refused on the other channel and IDs checked before
  any request; and a Feishu account supervised beside WeChat, surviving
  WeChat's discovery failure;
- polling and other senders continuing during a long owner turn, a
  conversation's messages running in order with their own context tokens, at
  most four turns at once, busy notices beyond the queue limits without a
  turn, recovery answering every claim once without replay, and shutdown
  closing every running turn while keeping its claim;
- the bridge's side-effect-free intake (`classify`): seen and unanswerable
  messages only marked seen, claimed or undelivered ones never run twice,
  direct and group conversation keys, tools and the owner limit only for the
  tool owner's direct chat, the owner recognized without the grant, busy
  beyond the claim and queue limits, and a full session table closing only an
  idle conversation;
- refused replies held per conversation within count, byte, total, and age
  limits, delivered ahead of the next reply only as far as one message allows,
  restored when the carrying reply is refused, busy notices never held, and
  reply content absent from logs;
- no-tools remote sessions by default, SIGTERM/Ctrl+C shutdown, and tracked
  session cleanup;
- writer/turn descendants joined after forced handler abort, cancellation-aware
  reconciliation, and management locks released before blocked response writes;
- delegated runs: each output format parsed from canned event streams
  (unknown events, sign-out, oversized lines, the Codex `-o` file, bounded
  replies), records private and removed at the end, `kill`, a timed-out run's
  `setsid` descendant stopped, an orphan left by a SIGKILLed
  `scv server --stdio` reaped by the next reconcile, the depth limit and
  nested daemon-command refusal, and agent status never printing an email or
  key;
- background jobs: each call's `tool.completed.jobs` naming the jobs it
  started and those whose results it first showed the model (a job settles
  once seen or stopped, never merely finished, and never twice), report turns
  naming theirs in `origin.jobs`, and the channel bridge and `scv exec`
  keeping a session open from those events alone;
- TUI reconnect creating a fresh session without history restoration or
  automatic replay of submitted work.

Use fake components, local protocol peers, and fake HTTP services for these
checks. Correctness tests must not contact WeChat, Feishu, or a live model
provider.

## Required checks

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources
cargo build --release --locked
git diff --check
```

Run `cargo-deny` locally when installed; CI requires it. This document defines
required coverage and checks, not verification results for a particular change.

CI runs formatting, strict Clippy (also for `scv-channels` with each channel
feature alone and with none), workspace tests/builds, and `cargo-deny`.
Tests also run on macOS and under the declared Rust 1.88 MSRV. The tagged-release
workflow builds release archives and smoke-tests them on their native Linux and
macOS runners.

## Test layout

Unit tests sit next to the code they test, in a file of their own, so a reader
finds both by path and the source file holds only the implementation:

- `src/foo.rs` ends with `#[cfg(test)] mod tests;`, and its tests live in
  `src/foo/tests.rs`. The tests of `src/lib.rs`, `src/main.rs`, or a
  `mod.rs` live in `tests.rs` beside it. This holds for the `scv` binary
  too: the command line's own logic is tested under `src/cli/**/tests.rs`.
- A large suite may split by topic into `src/foo/tests/<topic>.rs`, declared
  with `mod <topic>;` in `src/foo/tests.rs`.
- A test file starts with a `//!` line naming the source file it tests, then
  `use super::*;`, so it can reach the module's private items without making
  them public.
- Black-box tests that run the `scv` binaries or use only public APIs live in
  one test binary, the root `tests/it/`: `main.rs` declares one module per
  area (such as `daemon.rs` or `delegation/background.rs`), and shared helpers
  live in `tests/it/support.rs`. Every spawned SCV binary goes through
  `support::Isolated`. One binary means one link step however many modules
  there are.

`tests/it/guard.rs` enforces the rule: a `#[test]` or
`#[tokio::test]` in any other source file fails it, and so does a test file
that its parent module never declares (such a file would never run).

Run one crate's unit tests with `cargo test -p <crate> [<name filter>]`, the
`scv` binary's with `cargo test -p scv-cli --bin scv`, and the black-box tests
of one area with `cargo test -p scv-cli --test it <module>::` (such as
`daemon::`).

## Lints and formatting

`rustfmt.toml` and `clippy.toml` hold the repository's settings, and every
package opts into the workspace `[workspace.lints]` table with
`[lints] workspace = true`. Crates with no `unsafe` code declare
`#![forbid(unsafe_code)]`, and every `unsafe` block carries a `SAFETY:`
comment.

The workspace lints are a ratchet: a lint joins the table only once it has no
findings, so CI's `-D warnings` stays green. Clippy's `pedantic` group is on,
without the lints that do not pay for themselves here (`missing_errors_doc`,
`missing_panics_doc`, `must_use_candidate`, `module_name_repetitions`,
`similar_names`, `unreadable_literal`, `struct_field_names`, `doc_markdown`,
`verbose_bit_mask`). `undocumented_unsafe_blocks` and
`allow_attributes_without_reason` deny, and `unwrap_used` warns outside tests:
state an invariant with `expect("…")` instead.

The next ratchet steps are the pedantic lints still allowed under the "Next
ratchet" comment in `Cargo.toml`, such as `format_push_string`,
`items_after_statements`, `needless_pass_by_value`, the `cast_*` lints, and
`too_many_lines` at the `clippy.toml` threshold. Each still has findings that
need a hand-written change; remove its line together with that cleanup. Then
come `unreachable_pub`, and `missing_docs` crate by crate as each crate's
public items are documented.

The suite is a foundation, not a claim of exhaustive terminal or provider
compatibility. Snapshot coverage for every TUI state, randomized protocol
fuzzing, every malformed SSE variant, and sustained backpressure/load tests are
release-expansion work.

The TUI verification contract includes stable render-buffer assertions for idle,
streaming, scrolled, tool-running, tool-inspector, approval, command-palette,
help, error, and disconnected states across normal and constrained terminal
sizes. Focused state tests cover multiline editing, Unicode boundaries,
bracketed paste, queued prompts, command dispatch, Markdown and diff rendering,
wrapped-row scrolling, overlay precedence, and terminal-content sanitization. A
pseudo-terminal integration test should cover paste, resize, interruption, and
terminal-mode restoration. These additions do not require a live provider.

## Performance harness

The checked-in Criterion harness measures protocol encode/decode, a serialized
client-message round trip, and context selection over 10,000 messages. The final
evaluation separately measures release-build initialization, first terminal
paint, and idle resident memory with local process/PTY scripts. Those smoke
scripts are machine-specific evaluation aids rather than CI gates.

Reference-machine targets are:

| Measurement | Target |
| --- | --- |
| Protocol encode/decode | p95 under 100 us/message |
| Context selection, 10,000 small messages | p95 under 20 ms |
| First TUI paint | p95 under 250 ms |
| Client/server protocol initialization | p95 under 150 ms |
| Combined idle RSS | under 75 MiB |

Machine-dependent targets are reported, not made flaky CI gates. A regression
greater than 10 percent against a same-machine saved baseline requires review.

## Comparative evaluation

The final report pins the comparison baselines, records the local machine and
tool versions, and distinguishes measured runtime data from architectural
comparison. It does not infer overall agent quality from startup or framework
microbenchmarks and does not claim parity where features are deferred. See the
[`v0.1 evaluation`](evaluation.md).
