# Terminal User Experience

Status: proposed final design for v0.2

The SCV TUI is an event-driven Ratatui/Crossterm client. Rendering is derived
from local UI state, while the server remains authoritative for sessions,
turns, tools, approvals, and context metrics. The v0.2 experience is designed
for sustained coding sessions: information stays readable while output streams,
the composer behaves like a capable terminal editor, and common actions are
discoverable without making the interface dense.

## Layout and responsive behavior

The normal layout contains a compact one-line header, a scrollable transcript,
an adaptive composer, and a one-line status bar. The header shows SCV, model,
shortened workspace, and connection state. The status bar shows run state,
elapsed time, context use, queued-input state, viewport position when scrolled,
and only the shortcuts relevant to the current state.

The composer grows from three to ten rows. Input scrolls within that fixed
maximum so a long prompt cannot consume the transcript. The cursor remains
visible for wrapped text, explicit newlines, wide Unicode characters, and
terminal resize. Terminals narrower than 60 columns omit secondary header and
status fields before truncating primary content. Terminals shorter than 12 rows
use a compact composer and full-screen overlays. No supported size causes a
panic or produces overlapping regions.

The transcript follows new output only while already at the bottom. Scrolling
up freezes the viewport and displays an unseen-output marker; `End` returns to
live output. Scroll offsets are based on rendered, wrapped rows rather than raw
newline counts, so resize and long lines do not skip or strand content.

## Transcript presentation

User, assistant, tool, system, and error entries have distinct but restrained
styles that remain understandable without color. Assistant Markdown renders
headings, emphasis, inline code, lists, block quotes, links, fenced code, and
horizontal rules. Unsupported Markdown degrades to readable plain text.
Streaming content may temporarily show incomplete Markdown, but completed
content must settle into the same output as a non-streaming render.

Fenced code uses a neutral code-block treatment. Diff fences and tool output
that resembles a unified diff distinguish added, removed, hunk, and metadata
lines. Rendering never interprets terminal escape sequences from model or tool
content; control characters are replaced with visible, inert text.

Each tool call is one compact row with state, tool name, a bounded summary, and
locally measured elapsed time. Running rows visibly animate without changing
layout width. Completed output is collapsed by default. `Ctrl+O` opens a tool
inspector for the most recent tool; `Up` and `Down` select adjacent calls,
`Enter` toggles full arguments or output, and `Esc` closes the inspector. The
inspector preserves server-provided truncation and applies an additional
display cap.

The rendered transcript remains capped by `tui.max_transcript_bytes` and
`tui.max_transcript_items`. Oldest complete items are evicted at the cap and a
visible marker explains that display history was trimmed. This never mutates
server history.

## Composer and input

The composer supports bracketed paste as one operation, including multiline
and large text, without submitting pasted newlines. It provides these editing
bindings in addition to printable input:

- `Enter`: submit when idle; while a turn runs, enqueue the current prompt.
- `Ctrl+J`, `Shift+Enter`, or `Alt+Enter`: insert a newline.
- `Left`/`Right`, `Ctrl+B`/`Ctrl+F`: move by one character.
- `Alt+Left`/`Alt+Right`, `Alt+B`/`Alt+F`: move by one word.
- `Up`/`Down`: move by visual line in multiline input; recall prompt history at
  the first or last line when no vertical move is possible.
- `Home`/`End`, `Ctrl+A`/`Ctrl+E`: move to the visual line boundary.
- `Ctrl+W`: delete the preceding word; `Ctrl+U`/`Ctrl+K`: delete to the start or
  end of the logical line.
- `Ctrl+C`: clear non-empty input, cancel a running turn when input is empty,
  or quit when idle.
- `Esc`: cancel the active turn; close an overlay first when one is visible.

Prompt history remains separately capped by `tui.max_prompt_history_bytes` and
`tui.max_prompt_history_items`. Recall preserves the draft that was present
before history navigation. A submitted prompt larger than the byte cap is sent
normally but is not retained for recall.

## Shared turn queue

The server owns an ordered, bounded queue for each stdio session. A submitted prompt
starts immediately only when its session is idle; otherwise it receives a
server-generated queue ID and is appended. The server starts the next queued
prompt immediately after every terminal turn event, including completion,
failure, and cancellation. A client must remove or pause queued work before
cancelling a turn when it does not want the next prompt to run.

The TUI receives the queue snapshot and subsequent ordered changes for its
session. It renders queue position, prompt preview, submitter label, and
revision. It supports adding, selecting, moving, editing, and removing queued
prompts. `Alt+Up` selects the previous queued prompt for editing; `Alt+Down`
selects the next one; `Enter` saves the edit; `Ctrl+X` removes the selected
entry; `Alt+P` pauses or resumes automatic dequeue. The help overlay presents
these bindings rather than keeping them in the footer.

Edits, moves, and removals name both queue ID and revision. The server rejects
a stale revision and supplies the current entry; the TUI then shows a conflict
overlay where the user can discard the draft, replace the current text, or copy
the current text into the composer. The active turn is immutable queue state
and cannot be edited or reordered.

Queue size and prompt bytes are server-configured limits. Enqueueing beyond a
limit fails without losing the composer input. `session.clear` clears queued
prompts as well as history, and a session can be paused to prevent automatic
dequeue while preserving its queue. Queue state is retained while the local
server is running but is not durable across a server restart in v0.2.

## Commands and overlays

Typing `/` at the beginning of the composer opens a command palette. It shows
command names, short descriptions, and availability in the current state.
Typing filters by command prefix and words; `Up`/`Down` changes selection,
`Tab` completes, `Enter` runs the selected command, and `Esc` closes the
palette without clearing input. Unknown slash-prefixed text remains a normal
prompt rather than being silently discarded.

The v0.2 local commands are:

- `/help`: open a searchable key and command reference overlay.
- `/clear`: clear the authoritative session after server confirmation.
- `/context`: open context, history, transcript-cap, and prompt-history details.
- `/status`: open session, model, workspace, approval, connection, and current
  turn details available to the client.
- `/copy`: copy the latest completed assistant response through the platform
  clipboard command when one is available, otherwise show a clear error.
- `/quit` and `/exit`: leave the TUI after orderly child-server shutdown.

Help, context, status, tool inspection, and approval are overlays rather than
transcript messages. They scroll when their content exceeds the available
height and preserve the underlying transcript and composer state.

## Approval and failure behavior

An approval overlay shows the tool, risk, workspace, full bounded summary, and
the exact available decisions. `y` or `Enter` allows once; `n` or `Esc` denies.
Input meant for the composer cannot leak into an approval decision. The TUI
emits a terminal bell when an approval arrives while terminal focus is lost.

Configuration, provider, protocol, and tool errors appear as concise transcript
entries with a remediation hint when one is known. Normal interaction never
displays raw protocol JSON. Child-server EOF changes the UI to disconnected,
preserves the visible transcript and draft, and disables submissions.

Terminal raw mode, focus reporting, bracketed paste, and the alternate screen
are restored on clean exit, errors, panics, and handled termination signals.
Resize and focus events never enter the composer as text. Styling uses terminal
default foreground and background colors and remains legible when color is
unavailable.

## Compatibility boundary

The v0.2 TUI uses SCV protocol version 2 over the existing authenticated stdio
child server. It supports multiple queued prompts in one client session, but
does not yet share a queue between separate server processes. It still permits
only one active turn per session. It does not add durable sessions, remote
network connections, model
switching inside a session, shell-command shortcuts, file mentions, image
input, or configurable key maps.

## Acceptance checks

Automated tests cover editor operations at Unicode and word boundaries,
multiline cursor visibility, paste behavior, server queue transitions,
concurrent queue edit conflicts, command filtering and dispatch, Markdown and diff rendering, wrap-aware scrolling,
tool-inspector selection, overlay precedence, transcript and prompt-history
caps, authoritative session clearing, narrow and short layouts, oversized
server frames, and terminal-content sanitization.

Render snapshots or stable buffer assertions cover idle, streaming, scrolled,
tool-running, tool-inspector, approval, command-palette, help, error, and
disconnected states at 120x36, 80x24, and 50x10. A pseudo-terminal integration
test verifies bracketed paste does not submit, resize does not panic, and
terminal modes are restored after normal exit and interruption.
