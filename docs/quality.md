# Testing and Performance

Status: final design and verification record for v0.1

Peon correctness tests require no network, provider credential, or installed
third-party agent. The checked-in v0.1 suite contains 46 tests covering:

- core completion, grouped context selection, repeatable history trimming,
  history-limit failure, multi-step tools, denials, maximum steps, usage
  aggregation, and duplicate tool registration;
- OpenAI-compatible request shaping, fragmented streamed text/tool arguments,
  usage, bounded error handling, and credential redaction through a local fake
  server;
- protocol round trips and forward-compatible additive fields;
- filesystem containment, symlink escape, bounded reads, atomic writes, stale
  hashes, process timeout, process-group cancellation, bounded process output,
  and native-agent argument/cwd behavior;
- configuration trust boundaries, stricter project limits, and cross-field
  bounds;
- bounded client/server frame reading, CRLF boundaries, server handshake,
  session startup, and a complete streamed turn through a fake provider; and
- TUI prompt history, Unicode editing, primary-region rendering, bounded frame
  reads, and authoritative session clearing.

CI runs formatting, strict Clippy, workspace tests/builds, and `cargo-deny`.
Tests also run on macOS and under the declared Rust 1.88 MSRV. The tagged-release
workflow builds release archives and smoke-tests them on their native Linux and
macOS runners.

The v0.1 suite is a foundation, not a claim of exhaustive terminal or provider
compatibility. Snapshot coverage for every TUI state, randomized protocol
fuzzing, every malformed SSE variant, and sustained backpressure/load tests are
release-expansion work.

## Performance harness

The checked-in Criterion harness measures protocol encode/decode, a serialized
client-message round trip, and context selection over 10,000 messages. The final
evaluation separately measures release-build initialization, first terminal
paint, and idle resident memory with local process/PTY scripts. Those smoke
scripts are machine-specific evaluation aids rather than CI gates.

Reference-machine targets are:

| Measurement | v0.1 target |
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
