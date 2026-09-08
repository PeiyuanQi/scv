# Peon v0.1 Evaluation

Status: final v0.1 evaluation

Date: 2026-09-08

## Outcome

Peon v0.1 implements the minimal coding-agent slice: a bounded agent
loop, extension traits, deterministic context selection, OpenAI-compatible
streaming, built-in coding tools, native Claude/Codex/Pi adapters, a versioned
client/server boundary, and an interactive TUI. The workspace builds on Linux
and macOS release targets and is packaged as Apache-2.0 open source.

This is a usable foundation, not feature parity with mature agents. Codex, Pi,
and Claude Code have broader model/auth support, richer rendering and editing,
session persistence, longer-lived extension ecosystems, and substantially more
production hardening.

## Baselines

- Codex architecture/reference source: `openai/codex` at commit `d648947`.
- Pi architecture/reference source: `earendil-works/pi` v0.85.1.
- Local runtime comparison: `codex-cli 0.146.0` and Claude Code `2.1.220`.
- Pi was not installed locally, so it is included in the source-backed feature
  comparison but not the runtime measurements.

The comparison is architectural: Peon does not copy source from these projects.
Its Apache-2.0 implementation uses ordinary ecosystem crates documented in the
Cargo manifests and lockfile.

## Feature comparison

| Capability | Peon v0.1 | Codex/Pi/Claude maturity comparison |
| --- | --- | --- |
| Agent loop | Bounded streamed multi-step tool loop with cancellation and usage accounting | Peon covers the minimal loop; mature agents handle more response modes, recovery paths, and provider-specific behavior |
| Extension boundary | Rust traits for providers, tools, context, approvals, and event sinks; Markdown skills | Pi has a particularly broad extension/event surface; Codex has a larger internal service ecosystem |
| Context | Deterministic token-budget selection, whole tool-call groups, bounded summaries, configurable limits | Peon lacks mature agents' richer persistence, resume, caching, and model-aware compaction strategies |
| Coding tools | Contained read, atomic write, Bash, skill loading, and native Claude/Codex/Pi subprocess adapters | Peon lacks a true OS sandbox, patch/diff workflow, MCP, web tools, and broad tool catalogs |
| Process split | Versioned JSONL stdio server with concurrent I/O, sessions, approvals, cancellation, and bounded queues/frames | Comparable architectural seam, with a much smaller protocol and no remote transport |
| Terminal UX | Streaming transcript, multiline editing, history, scrolling, folded tool output, approvals, cancel, clear, context, help | Useful baseline, but behind mature Markdown/diff rendering, completion, themes, resume/history browsing, and accessibility polish |
| Distribution | Cargo install/source build plus four Linux/macOS release archives | No package-manager distribution, updater, signing, or Windows support yet |

## Performance results

Measurements used the Rust 1.88 release toolchain on an Apple M4 MacBook Air,
macOS 26.6.2 (arm64). They exclude model and network time. Criterion intervals
are the reported estimate ranges; startup numbers are small local samples and
are not cross-project quality scores.

| Peon measurement | Result | v0.1 target |
| --- | ---: | ---: |
| Protocol encode | 252-254 ns | under 100 us/message |
| Protocol decode | 347-349 ns | under 100 us/message |
| Serialized client-message round trip | 284-285 ns | under 100 us/message |
| Context selection, 10,000 messages | 1.24-1.26 ms | under 20 ms |
| Warm client/server initialization, 20 samples | 2.02 ms median; 3.59 ms p95 | under 150 ms |
| Warm first TUI paint, 10 samples | 16.86 ms median | under 250 ms |
| Server idle RSS | 4.31 MiB median | informational |
| Combined TUI/server idle RSS | 8.00 MiB median | under 75 MiB |
| Release binary size | `peon` 5.3 MiB; `peon-server` 4.8 MiB | informational |

The first post-link process/TUI samples were 315 ms and 295 ms respectively,
consistent with a macOS cold-cache outlier; they are reported rather than
silently included in the warm distributions.

A narrow five-sample PTY comparison measured time to first output and idle RSS:

| Local executable | First output median | Idle RSS median |
| --- | ---: | ---: |
| Peon v0.1 | 16.86 ms (10-sample Peon run) | 8.00 MiB including server |
| Codex CLI 0.146.0 | 28.59 ms | 22.34 MiB |
| Claude Code 2.1.220 | 249.55 ms | 356.77 MiB |

Different startup work, installed configuration, and process models make these
figures directional only. They do not measure interaction latency, model
quality, safety, or full-session memory. Claude Code also required a forced
termination after the measurement window on this machine.

## Verification

The release candidate passes formatting, strict workspace Clippy, 46 offline
tests, a locked release build, `cargo-deny` advisories/bans/licenses/sources,
and `git diff --check`. The design received an independent subagent PASS before
implementation. The first independent implementation gate found no P0 issues
and five P1 correctness areas—process descendants, cancellation history
integrity, server backpressure, pathname containment/concurrency claims, and
hard history caps—plus a P1 verification-contract mismatch.
Those findings produced capability-relative file operations, transactional
failed-turn history, byte-bounded cancellable output, bounded joined shutdown,
and focused regressions. A second gate found the TUI/server grace mismatch; it
was repaired and covered with a TERM-ignoring descendant test. The final focused
read-only Codex CLI re-review returned PASS with no remaining related P0/P1.

The configured collaboration-agent retries for the final snapshot were blocked
by the account usage limit, so the final PASS used the locally installed Codex
CLI in a read-only sandbox. Pi comparison remains source-backed because Pi was
not installed locally.

## Next maturity steps

The highest-value follow-ups are an OS sandbox and explicit patch workflow,
durable sessions/resume, richer TUI Markdown and diff UX, additional providers
and authentication flows, MCP or an equivalent external tool protocol, fuzz and
load testing, signed releases, and package-manager installation.
