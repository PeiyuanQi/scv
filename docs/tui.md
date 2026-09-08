# Terminal User Experience

Status: final design for v0.1

The Peon TUI is an event-driven Ratatui/Crossterm client. Rendering is derived
from local UI state, while the server is authoritative for sessions, turns,
tools, approvals, and context metrics.

## Layout

The header shows Peon, the active model, workspace, and connection state. The
scrollable transcript occupies the main region. The composer grows to a bounded
height and keeps a visible cursor. The footer shows run state, elapsed time,
context usage, and shortcut hints. A centered approval panel temporarily owns
`y`, `n`, and `Esc`.

Assistant deltas update one in-progress transcript item without moving the
viewport when the user has scrolled away from the bottom. Tool rows show
pending, awaiting approval, running, success, denied, cancelled, or failed
state. Completed tool output is collapsed by default and toggled with
`Ctrl+O`.

## Input behavior

- `Enter`: submit a non-empty prompt when idle; keep input intact on send error.
- `Ctrl+J`: insert a newline.
- `Up`/`Down`: recall prompt history when the composer is one line.
- `PageUp`/`PageDown`: scroll the transcript.
- `End`: return to live output.
- `Esc`: cancel the active turn; deny a visible approval.
- `Ctrl+C`: clear non-empty input, otherwise cancel a running turn, otherwise
  exit.
- `Ctrl+O`: expand or collapse the most recent tool output.

Local commands are `/help`, `/clear`, `/context`, and `/quit`. They never enter
model context. `/clear` is allowed only while idle: it sends `session.clear`,
waits for `session.cleared`, then resets the transcript and local prompt history.
`/context` shows the current budget, estimated selected tokens, history use, and
last compaction metrics.

The rendered transcript is capped by `tui.max_transcript_bytes` and
`tui.max_transcript_items`. Oldest complete items are evicted at the cap and a
visible marker explains that display history was trimmed. This does not mutate
server history.

Prompt recall is separately capped by `tui.max_prompt_history_bytes` and
`tui.max_prompt_history_items`. The oldest whole prompt is evicted first; a
submitted prompt larger than the byte cap is sent normally but is not retained
for recall.

## Failure behavior

Configuration, provider, protocol, and tool errors appear as concise transcript
items with a remediation hint. Normal interaction never displays raw protocol
JSON. Child-server EOF changes the UI to disconnected and preserves the visible
transcript until the user exits.

Terminal raw mode and the alternate screen are restored on clean exit, error,
panic, and termination signals handled by the process. Resize events reflow the
layout. Styling uses terminal-default foreground/background colors and remains
legible when color is unavailable.

## Acceptance checks

The checked-in v0.1 unit tests cover bounded prompt recall, Unicode editor
boundaries, the primary rendered terminal regions, oversized server frames, and
authoritative session clearing. Manual pseudo-terminal evaluation measures
first paint and idle memory. Exhaustive reducer/key-state coverage, render
snapshots for every transcript state, and automated pseudo-terminal restoration
scenarios are release-expansion work tracked by the quality plan.
