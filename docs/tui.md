# Terminal User Experience

The SCV TUI is an event-driven Ratatui/Crossterm client of the daemon's Unix
socket. Rendering is derived from local UI state, while the server remains
authoritative for sessions, turns, tools, approvals, context metrics, and the
workbench of delegated agents and background jobs. It is designed for
sustained coding sessions: information stays readable while output streams,
the composer behaves like a capable terminal editor, several chats and
delegated agents can be followed side by side, and common actions are
discoverable without making the interface dense.

## Dashboard

The TUI is a dashboard of tabs. A **chat tab** is one daemon session in one
workspace: the TUI opens with a chat in the current directory, and `/open
<directory>` or the `new_tab` key opens another in a different workspace, so
one TUI can hold a chat per repository. An **agent view** follows a delegated
agent of the [workbench](tools.md#the-workbench): a background job, a
conversation, or a single run. Below the header, a one-line tab bar names
every tab with its state mark; the active tab is highlighted.

The body is the sidebar beside one or two panes. The `split` key divides the
body into two side-by-side panes (each shows a tab; `focus_other_pane` moves
between them) and joins them again. The **sidebar** groups, top to bottom:
approvals waiting for the owner, this TUI's chat tabs, every workspace's jobs
and live agent runs (each line a state mark, agent, job, and task), jobs a
restart interrupted that can be resumed, and queues kept from sessions that
stopped. `focus_sidebar` moves keys to it and back to the pane they left; there `Up`/`Down`,
`PageUp`/`PageDown`, and `Home`/`End` select, `Enter` opens the selection (a
chat tab, or an agent view), `y`/`n` answer the selected approval, `x`
pressed twice stops a running job, `r` resumes an interrupted job or
restores a kept queue, `d` pressed twice forgets a finished job, `w` lists
worktrees, and `Esc` returns to the pane focus left. `y`/`n` on a job row
name the request they will answer and act on the second press, so a new
request on that job is not answered by a key meant for the previous one.
`Enter` on a tab or view the second
pane already shows focuses that pane. The selection stays on its entry as
the list changes, and a second `x` or `d` counts only on the entry the first
was pressed on, within three seconds. When the selected entry goes away (an
approval answered or expired, a job forgotten), the selection falls to the
entry now nearest its row, and the next `Enter` or letter only moves the
selection there and says so: pressed again, it acts. The sidebar gives way
below 70 columns, and the second pane below 60 columns beside it. Below 12 rows or 50
columns the focused pane's chat is drawn alone, as it would be without the
dashboard. Keys go only to what is drawn: a chat drawn alone takes them, and
with the sidebar or the second pane given way, they go to the first pane.

Closing a chat tab ends its session, and quitting ends every tab's: their
running turns and background jobs stop, their jobs are recorded as
interrupted, and their queued prompts are dropped. When that would stop or
drop something (a running turn, an unfinished background job, or a queued
prompt, a restored and paused queue included), closing or quitting
(`Ctrl+C` on an idle chat, `/quit`) says what and waits for the same request
again within three seconds. The confirming `Ctrl+C` quits; it does not
cancel the running turn. While a chat shows an approval prompt, it takes
every key but the dashboard keys below wherever focus is, since it covers
the screen. A dashboard key may move focus, and the prompt stays that
chat's until it is answered: `y` and `n` or `Esc` still answer it, and
other keys do nothing, so nothing behind it opens or moves. On a terminal
too small for the dashboard, that prompt is drawn over the chat shown
alone, so it is still what those keys answer. A request
from another chat while one is drawn is announced and does not replace
it, including a request from the chat focus just moved to.

A tab's connection failing in a way other than ending (a server frame too
large, or one the TUI cannot read) is that tab's alone: the tab shows the
error, disconnects, and reconnects as when its connection is lost, and the
other tabs keep running. A new chat tab (`/open`, `/restore`, or a restored
queue from the sidebar) takes the focused pane when its session starts only
if focus is still where it was asked for; otherwise the tab is added and a
notification names it, so keys typed meanwhile stay where they went.

State marks read without color: `◌` starting, `●` working, `?` waiting for
input, `○` idle, `…` queued, `✓` completed, `✗` failed or timed out, `⊘`
declined, `■` cancelled, `⟳` interrupted, and `~` unknown. The header counts
jobs working, waiting for the owner, and interrupted. A chat tab is working
while its turn runs, waiting while it shows an approval, and unknown while
disconnected.

An agent view is the agent's live activity log as the daemon records it:
turn boundaries with each prompt, the agent's messages, tool calls and
failures, plans, approvals asked and answered, steering, and the turn's
outcome, with run separators when a conversation's turns ran in several
processes. Its top line says the agent, state, and why (for example
`waiting for your approval: Run cargo publish`, or `no activity for 6 min; last: $ cargo test`).
It follows new lines while at the bottom; `PageUp`/`PageDown` scroll and
`End` returns. The view has its own one-line composer: `Enter` sends the
text to the agent's conversation (into its running turn when the agent can
take steering, else as the conversation's next turn, queued behind a running
one). If the daemon refuses it, the connection breaks, or the send is not
answered in time, the text comes back; a send that timed out may still have
reached the daemon, so check the conversation before sending it again. Text
typed while it was in flight stays after it, on the next line.
One prompt is sent at a time: `Enter` while the
last is unanswered keeps the text in the composer and says so. `Ctrl+J`
inserts a newline, and `Esc` clears it. While the job waits for an approval,
`Ctrl+Y` approves and `Ctrl+N` denies the request the view is showing. When
a later request replaces that one, the first of those chords names the new
request and the next one answers it. `y` and `n` are always text, so a
prompt starting "yes" or "no" never answers it. `Ctrl+X` twice
stops the job; `Ctrl+R` resumes an interrupted one. In the activity view
input is sent only by these keys; typing into the agent's own terminal is
attaching, below.

### Attaching to an agent's terminal

A delegated agent runs headless (a CLI process per turn, or an ACP server
speaking JSON-RPC on its standard streams), so its activity view is SCV's
log of it, not a screen. `Ctrl+O` in an agent view takes the view's idle
conversation over in the agent's own interactive terminal instead: the
daemon opens the agent's own UI on the conversation's session (for Codex,
`codex resume <thread>`), in the conversation's directory and the agent's
private home, on a pseudo-terminal the daemon holds (see
[taking a conversation over](tools.md#taking-a-conversation-over)). The
view then shows that terminal's screen live, sized to the pane, under a
line naming it, and every key goes to the agent, the dashboard's keys and
the view's own chords included; a paste goes as one bracketed paste when
the agent asked for that. `Ctrl+]` detaches: the view is the activity log
again, and the agent keeps running in its terminal with nobody watching.
`Ctrl+O` attaches again, to the screen as it is now. `Ctrl+X` twice in the
activity view of a conversation taken over ends the takeover: its terminal
is hung up, which asks the agent's UI to save and exit, and its process
group is stopped two seconds later if it has not. The agent leaving its UI
ends it too. However it ends, the view says how and
shows the activity log, and the conversation's next turn continues the same
agent session, with what was done in the terminal.

While a conversation is taken over, SCV runs no turn of it: a prompt sent
to it from a view, by the model, or with `scv jobs send` waits in its queue
until the takeover ends, and a call that does not wait fails saying so.
`Ctrl+O` is refused, with why, when the conversation runs a turn or has one
waiting (stop its job first), is held by a reviewed job between its rounds,
has no agent session yet, or belongs to an
agent whose terminal may not be offered: only an agent whose terminal asks
nothing SCV's approval gate decides can be taken over, which is Codex with
`permissions = "full"`. Claude Code is always refused, because its terminal
asks before some actions even with its own prompts off, and keys typed
there would answer those prompts instead of the gate. Attaching is not a
way to approve anything: approval requests go where they always go, and
the keys typed into a terminal reach only the agent. A daemon before 0.4.5
answers `Ctrl+O` with a notice and the view stays as it was; an
attach that fails, a terminal that closes, or a connection that breaks
falls back to the activity log and says so.

The screen is drawn from what the daemon sends: styled text, never the
agent's escape sequences, so nothing the agent prints reaches the owner's
terminal as a control sequence, and values that look like credentials are
masked by the activity log's rules. One dashboard holds a terminal at a
time: attaching from another takes it, and the first falls back to the
activity log at its next read. A dashboard that stops reading (closed or
killed) counts as detached after 30 seconds, and the agent runs on. The
terminal is read on a connection of its own every 50 ms, and at once after
each key; starting and ending a takeover, which may take seconds, go on a
third, so typing into another attached view never waits for them.

The dashboard asks the daemon for a workbench snapshot every second and
reads each open view's log every 400 ms, on one connection of its own.
While that connection is down it asks only for the snapshot, once a second,
and an action it could not deliver is reported, not dropped. An older daemon
without the workbench leaves the TUI a set of chat tabs and says so once;
the TUI asks again every 30 seconds, so a daemon restarted into this release
is found. Changes raise notifications: an approval request, a job waiting
for input, completing, failing, timing out, being declined or interrupted
(also when it is first seen already so), and a job going quiet. The newest shows in the footer for ten seconds,
`notifications` lists the last hundred, and an approval, a wait for input, a
failure, or an interruption also rings the terminal bell.

### Keys

Dashboard keys come before a chat's own keys. Defaults:

| Action (`[tui.keys]` name) | Default | Does |
| --- | --- | --- |
| `focus_sidebar` | `Ctrl+G` | focus the sidebar, or go back to the pane |
| `toggle_sidebar` | `Alt+S` | show or hide the sidebar |
| `next_tab` / `prev_tab` | `Alt+.` / `Alt+,` | next or previous tab |
| `tab_1` … `tab_9` | `Alt+1` … `Alt+9` | go to that tab |
| `new_tab` | `Ctrl+T` | open a chat tab in another workspace (types `/open ` into an empty composer) |
| `close_tab` | `Alt+W` | close the tab (an agent view, or a chat that is not the last) |
| `split` | `Alt+V` | split into two panes, or join them |
| `focus_other_pane` | `Alt+O` | focus the other pane |
| `notifications` | `Alt+N` | list notifications |
| `help` | `F1` | keys and commands |

`[tui.keys]` in the home configuration rebinds them
([configuration](configuration.md#terminal-ui-keys)): each value is modifiers (`ctrl`,
`alt`, `shift`) joined by `+` to one key (a character, `f1`-`f12`, `enter`,
`esc`, `tab`, `space`, arrows, `home`, `end`, `pageup`, `pagedown`, `insert`,
`delete`, `backspace`), such as `ctrl+pagedown`. A binding needs `ctrl` or
`alt` unless it is a function key, since the rest is the composer's and the
views' own, and may not take a key they use (`ctrl+a`, `ctrl+b`, `ctrl+c`,
`ctrl+e`, `ctrl+f`, `ctrl+j`, `ctrl+k`, `ctrl+n`, `ctrl+o`, `ctrl+r`,
`ctrl+u`, `ctrl+w`, `ctrl+x`, `ctrl+y`, `alt+b`, `alt+f`, `alt+p`,
`alt+enter`, and `alt` with an arrow). `shift` with a character means the
character (`ctrl+shift+x` is `ctrl+x`), and `shift+tab` is the terminal's back-tab. The server sends the
table with `session.started`. Configured bindings come first, in the order
of the table above; an action whose configured key the TUI cannot use, or
that an earlier action's configured key took, keeps its default, and an
action whose default a configured binding took has no key. The TUI says
which in a notification at startup.

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
locally measured elapsed time. A delegation's row names its agent beside the
tool, as `agent codex`, or the conversation it continues (`agent codex-1`),
when the call's argument is a plain name. Running rows visibly animate without changing
layout width. While a tool runs, the newest line of its `tool.progress` (such
as a delegated agent's current command) appears under its row and disappears
when the tool completes. Headless `scv exec` prints progress lines to stderr. Completed output is collapsed by default. `Ctrl+O` opens a tool
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
entry; `Alt+P` pauses or resumes automatic dequeue. These keys work whenever
the queue holds prompts, not only while a turn runs, and resuming an idle
session's queue starts its next prompt at once. The help overlay presents
these bindings rather than keeping them in the footer.

Edits, moves, and removals name both queue ID and revision. The server rejects
a stale revision and supplies the current entry; the TUI then shows a conflict
overlay where the user can discard the draft, replace the current text, or copy
the current text into the composer. The active turn is immutable queue state
and cannot be edited or reordered.

Queue size and prompt bytes are server-configured limits. Enqueueing beyond a
limit fails without losing the composer input. `session.clear` clears queued
prompts as well as history, and a session can be paused to prevent automatic
dequeue while preserving its queue.

The TUI's sessions keep their queues on disk (`session.start` with
`persist_queue`): after every queue change and turn boundary, before it
announces the change, the daemon records the queued prompts and the running
one. When the TUI leaves (closing the connection or losing it), the record
goes. When the daemon stops or dies instead, it stays: a TUI that
reconnects asks for it (`restore`), and its new session starts with the old
queue **paused**, with a note naming how many prompts came back, how many
were left out because an attachment was missing or a prompt was too long,
and the prompt that was running, which is not sent again because its outcome
is unknown; `Alt+P` resumes the queue once reviewed, starting its first prompt.
A chat tab reconnects once at a time, however many keys arrive meanwhile, and
waits up to a minute for its session to start (connecting gets three
seconds), so the queue it restores stays with it; a TUI that leaves before
its session starts leaves the queue as it was, to restore again. A queue no
TUI reconnected for is listed under "Restorable queues" in the sidebar (and by
`scv jobs`) for seven days; `r` there or `/restore <id prefix>` opens a chat
tab in its workspace that restores it. A queue is restored at most once.

## Commands and overlays

Typing `/` at the beginning of the composer opens a command palette. It shows
command names, short descriptions, and availability in the current state.
Typing filters by command prefix and words; `Up`/`Down` changes selection,
`Tab` completes, `Enter` runs the selected command, and `Esc` closes the
palette without clearing input. Unknown slash-prefixed text remains a normal
prompt rather than being silently discarded.

The local commands are:

- `/help`: open a searchable key and command reference overlay.
- `/clear`: clear the authoritative session after server confirmation.
- `/context`: open context, history, transcript-cap, and prompt-history details.
- `/status`: open session, model, workspace, approval, connection, and current
  turn details available to the client.
- `/copy`: copy the latest completed assistant response through the platform
  clipboard command when one is available, otherwise show a clear error.
- `/quit` and `/exit`: leave the TUI after orderly child-server shutdown
  (asked again first when that would stop work; see [Dashboard](#dashboard)).
- `/open <directory>`: open a chat tab in another workspace (`~` and paths
  relative to the tab's workspace work).
- `/close`: close the active tab.
- `/jobs`: show and focus the sidebar.
- `/job <agent> [--cwd DIR] [--model M] [--effort E] [--worktree] <prompt>`:
  start a background job of the owner's own in this tab's workspace (see
  [owner jobs](tools.md#the-workbench)).
- `/restore <id prefix>`: open a chat tab restoring a kept queue.
- `/notifications`: list notifications.
- `/worktrees`: list the worktrees SCV created, with their jobs.

Help, context, status, tool inspection, and approval are overlays rather than
transcript messages. They scroll when their content exceeds the available
height and preserve the underlying transcript and composer state.

## Approval and failure behavior

An approval overlay shows the tool, risk, workspace, full bounded summary, and
the exact available decisions. `y` allows once; `n` or `Esc` denies; other
keys, `Enter` included, do nothing.
Input meant for the composer cannot leak into an approval decision. The TUI
emits a terminal bell when an approval arrives while terminal focus is lost.

A background job's approval is not a chat overlay: it waits in the sidebar
under "Waiting for you" and in the job's agent view, and rings the bell.
While a dashboard is open the daemon holds such a request for the owner (up
to two minutes) instead of denying it at once; see
[attended approvals](tools.md#the-workbench).

Configuration, provider, protocol, and tool errors appear as concise transcript
entries with a remediation hint when one is known. Normal interaction never
displays raw protocol JSON. Child-server EOF changes the UI to disconnected,
preserves the visible transcript and draft, and disables submissions.

Terminal raw mode, focus reporting, bracketed paste, and the alternate screen
are restored on clean exit, errors, panics, and handled termination signals.
Resize and focus events never enter the composer as text. Styling uses terminal
default foreground and background colors and remains legible when color is
unavailable.

## Headless `scv exec`

`scv exec [--yes] PROMPT` runs one prompt without the TUI, for scripts and
quick questions. It starts a private `scv server --stdio` for a session in the
current directory, so it needs no running daemon, and passes on the global
`--model`, `--provider`, `--base-url`, and `--approval-policy` flags. The answer
streams to stdout; tool progress, approval requests, and failures go to
stderr. The approval policy decides first, so under `on-risk` reads run
unasked; every request that would ask a person is denied unless `--yes`
approves it for this run. A cancelled or failed turn makes the command fail.
When the turn starts background jobs, `scv exec` stays open until each one has
been reported, and prints the report, waiting through a failed report turn the
server will try again. When the server reports jobs directly because the model
could not, it prints each job's reply to stdout and why to stderr. The TUI
shows a failed report turn's error with when it is tried again, and a direct
report as a note quoting each job's reply.

## Compatibility boundary

The TUI uses SCV protocol version 3 over the daemon's Unix socket. It permits
one active turn per chat session. Agent views show the structured activity
log SCV records for a delegated agent: SCV runs agents headless (JSON event
streams or ACP), so there is no screen of a running turn to mirror, and a
prompt sent from the view goes through the agent's conversation. Typing into
an agent's own terminal is a takeover of its idle conversation in the agent's
interactive UI, offered only for agents whose terminal asks nothing SCV's
approval gate decides; it is not available on every transport or agent, and
it never mirrors or types into a headless run. Delegated processes, a
takeover's terminal included, do not outlive the daemon; after a restart, an
interrupted job is resumed in the agent's own conversation, not reattached. The TUI does not add remote network
connections, model switching inside a session, shell-command shortcuts, file
mentions, or image input.

## Acceptance checks

Automated tests cover editor operations at Unicode and word boundaries,
multiline cursor visibility, paste behavior, server queue transitions,
concurrent queue edit conflicts, command filtering and dispatch, Markdown and diff rendering, wrap-aware scrolling,
tool-inspector selection, overlay precedence, transcript and prompt-history
caps, authoritative session clearing, narrow and short layouts, oversized
server frames, and terminal-content sanitization. Dashboard tests cover key
specifications and `[tui.keys]` conflicts and reserved keys, tab and pane
movement, keys reaching only what is drawn, sidebar grouping and actions
(confirmations bound to their entry), one reconnection per tab, approval
prompts' keys (nothing behind a chat's prompt acts, and an agent view's
plain `y`/`n` are text), a stale sidebar selection only moving, quitting
with work running or prompts queued, a tab's failed connection staying that
tab's, a slow new tab not taking a pane the owner left, agent-view log
paging (no line twice), sending one prompt at a time and getting a refused
prompt back, control characters in workbench text, notifications from
snapshot changes, an older daemon checked again, restored-queue notes, and
`/job` parsing. Attach tests cover keys encoded as a terminal sends them
(application cursor keys, modifiers, the detach key however the terminal
reports it), pastes that cannot end their bracket early, every key but
detach reaching the agent (an approval chord typed into the terminal
answers nothing), one read in flight, starting and ending a takeover on a
connection of their own, falling back to the activity log on a
refusal, a daemon without attach, a terminal that closes, or a lost
connection, a closed view detaching, and `Ctrl+X` twice ending a takeover.
Integration tests cover a queue restored across a killed daemon and resumed,
and a conversation taken over, typed into, detached, attached again, ended,
and hung up by a daemon that was killed.

Render snapshots or stable buffer assertions cover idle, streaming, scrolled,
tool-running, tool-inspector, approval, command-palette, help, error, and
disconnected states at 120x36, 80x24, and 50x10. A pseudo-terminal integration
test verifies bracketed paste does not submit, resize does not panic, and
terminal modes are restored after normal exit and interruption.
