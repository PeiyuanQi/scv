# TUI v0.2 Implementation Plan

Status: proposed; implementation requires explicit approval

## Objective

Make SCV's terminal client comfortable for sustained coding work while
preserving an authoritative server boundary. The target is the interaction
quality users expect from current coding-agent CLIs, including a collaborative,
server-owned turn queue for local clients attached to the same session.

## Delivery slices

1. Split the current single TUI module into focused state, editor, rendering,
   Markdown, command, and terminal modules without changing protocol behavior.
   Add deterministic buffer tests for the existing states before changing the
   presentation.
2. Implement the server-owned queue within the existing stdio session,
   including bounded enqueue, update, move, remove, pause, automatic dequeue,
   queue snapshots, revision conflict responses, and lifecycle tests.
3. Implement the multiline editor, bracketed-paste handling, draft-preserving
   history, queue editor, cursor viewport, and state-specific key routing.
4. Implement Markdown and diff rendering, content sanitization, wrap-aware
   transcript layout, compact responsive regions, and the unseen-output marker.
5. Implement the command palette and help, context, status, and tool-inspector
   overlays. Add best-effort platform clipboard support without introducing a
   required external runtime dependency.
6. Refine approval and failure presentation, focus notifications, terminal
   restoration, and all acceptance coverage. Update README usage text and the
   recorded quality inventory to the shipped behavior.

Each slice must leave `scv-tui` tests passing. The final change remains one
cohesive delivery commit unless review finds an independently revertible
boundary.

## Dependency and compatibility decisions

- Keep the provider and tool crates unchanged. Extend `scv-protocol` and
  `scv-server` for protocol version 2 and queue authority.
- Prefer Ratatui primitives and a small internal Markdown event renderer. A new
  parsing dependency is acceptable only if it handles CommonMark edge cases,
  supports Rust 1.88, and avoids terminal or HTML rendering dependencies.
- Do not add a syntax-highlighting dependency in v0.2. Use semantic styles for
  code fences and unified diffs; preserve code text exactly after sanitization.
- Clipboard integration detects `pbcopy`, `wl-copy`, `xclip`, or `xsel` and
  reports unavailability. It never evaluates a shell command.
- Preserve existing CLI flags and headless `scv exec` behavior. The protocol is
  intentionally breaking because v0.1 has no users.

## Quality gates

Before delivery, run:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
cargo build --release --locked
git diff --check
```

Run `cargo deny check advisories bans licenses sources` when `cargo-deny` is
installed. Manually exercise the built TUI in a pseudo-terminal at 120x36,
80x24, and 50x10, including streaming, cancellation, approval, paste, resize,
tool inspection, queue editing, and clean shutdown. Compare first-paint and idle-memory results
with `docs/evaluation.md`; investigate a same-machine regression over 10 percent.

## Review gates

The design requires explicit user approval before implementation. After
implementation, an independent review must pass feature completeness, terminal
restoration, untrusted-content rendering, input/approval separation, bounded
state, and regression safety before delivery.
