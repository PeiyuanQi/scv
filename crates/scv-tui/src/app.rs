//! [`App`]: everything the terminal UI shows, updated from server events.
//! The server owns the conversation; this is a display copy.

use std::{collections::VecDeque, time::Instant};

use anyhow::Result;
use scv_protocol::{QueueEntry, ServerEvent, ToolErrorKind, TurnOrigin, Usage};

use crate::{
    client::{SessionInfo, is_transport_error},
    input::clear_input,
    transcript::{BoundedLog, ToolStatus, TranscriptItem, bounded_text},
};

/// Longest progress line shown under a running tool.
const MAX_PROGRESS_DISPLAY_CHARS: usize = 160;

pub(crate) struct PendingApproval {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) risk: String,
    pub(crate) cwd: String,
    pub(crate) summary: String,
}

/// The turn in progress: submitted by this client, or started by the server.
pub(crate) struct ActiveTurn {
    /// The server's turn ID, known once `turn.started` arrives; until then
    /// the turn cannot be cancelled.
    pub(crate) id: Option<String>,
    pub(crate) started_at: Instant,
}

pub(crate) struct App {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) model: String,
    pub(crate) context_max_tokens: usize,
    pub(crate) context_after_tokens: Option<usize>,
    pub(crate) history_bytes: Option<usize>,
    pub(crate) items: BoundedLog<TranscriptItem>,
    pub(crate) prompt_history: BoundedLog<String>,
    /// Which prompt-history entry the composer shows, while recalling.
    pub(crate) history_index: Option<usize>,
    pub(crate) input: String,
    /// Cursor position in `input`, in characters.
    pub(crate) cursor: usize,
    pub(crate) scroll: u16,
    pub(crate) follow_output: bool,
    pub(crate) turn: Option<ActiveTurn>,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) queue: VecDeque<QueueEntry>,
    pub(crate) queue_paused: bool,
    pub(crate) queue_editing: Option<QueueEntry>,
    pub(crate) queue_selected: Option<usize>,
    pub(crate) last_seq: u64,
    /// Events of types this client does not know since `last_seq`; each may
    /// have used a sequence number.
    pub(crate) skipped_events: u64,
    pub(crate) connected: bool,
    pub(crate) quit: bool,
}

impl App {
    pub(crate) fn new(session: SessionInfo) -> Self {
        Self {
            session_id: session.id,
            cwd: session.cwd,
            model: session.model,
            context_max_tokens: session.context_max_tokens,
            context_after_tokens: None,
            history_bytes: None,
            items: BoundedLog::new(session.max_transcript_items, session.max_transcript_bytes),
            prompt_history: BoundedLog::new(
                session.max_prompt_history_items,
                session.max_prompt_history_bytes,
            ),
            history_index: None,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow_output: true,
            turn: None,
            pending_approval: None,
            queue: VecDeque::new(),
            queue_paused: false,
            queue_editing: None,
            queue_selected: None,
            last_seq: 0,
            skipped_events: 0,
            connected: true,
            quit: false,
        }
    }

    pub(crate) fn push_item(&mut self, item: TranscriptItem) {
        let trimmed = self.items.push(item);
        if trimmed
            && !matches!(self.items.front(), Some(TranscriptItem::System(value)) if value.starts_with("Display history trimmed"))
        {
            self.items.push_front(TranscriptItem::System(
                "Display history trimmed at configured limit.".into(),
            ));
        }
        self.follow_output = true;
    }

    pub(crate) fn reconnect(&mut self, session: SessionInfo) {
        self.session_id = session.id;
        self.cwd = session.cwd;
        self.model = session.model;
        self.context_max_tokens = session.context_max_tokens;
        self.items
            .set_limits(session.max_transcript_items, session.max_transcript_bytes);
        self.prompt_history.set_limits(
            session.max_prompt_history_items,
            session.max_prompt_history_bytes,
        );
        self.context_after_tokens = None;
        self.history_bytes = None;
        self.turn = None;
        self.pending_approval = None;
        self.queue.clear();
        self.queue_paused = false;
        self.queue_editing = None;
        self.queue_selected = None;
        self.last_seq = 0;
        self.skipped_events = 0;
        self.connected = true;
        self.history_index = None;
        self.prompt_history.trim();
        self.push_item(TranscriptItem::System(
            "Reconnected with a fresh session. Prior transcript is display-only; server history was not restored. Interrupted prompts and queued work were not replayed.".into(),
        ));
        self.items.trim();
    }

    pub(crate) fn disconnect(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        self.turn = None;
        self.pending_approval = None;
        self.queue.clear();
        self.queue_paused = false;
        self.queue_selected = None;
        if self.queue_editing.take().is_some() {
            clear_input(self);
        }
        self.context_after_tokens = None;
        self.history_bytes = None;
        self.last_seq = 0;
        self.skipped_events = 0;
        self.history_index = None;
        self.finish_pending_tools(ToolStatus::Failed);
        self.items.update_all(|item| {
            if let TranscriptItem::Assistant { streaming, .. } = item {
                *streaming = false;
            }
        });
        self.push_item(TranscriptItem::Error(
            "Server disconnected; reconnecting. Pending work has an unknown outcome and will not be replayed. Press Ctrl+C to exit.".into(),
        ));
        self.items.trim();
    }

    pub(crate) fn handle_connection_error(&mut self, error: anyhow::Error) -> Result<()> {
        if !is_transport_error(&error) {
            return Err(error);
        }
        self.disconnect();
        Ok(())
    }

    pub(crate) fn handle_server_result(
        &mut self,
        result: Result<Option<ServerEvent>>,
    ) -> Result<()> {
        match result {
            Ok(Some(event)) => self.handle_server_event(event),
            Ok(None) => self.disconnect(),
            Err(error) => self.handle_connection_error(error)?,
        }
        Ok(())
    }

    pub(crate) fn add_prompt_history(&mut self, prompt: String) {
        if prompt.len() > self.prompt_history.max_bytes() {
            return;
        }
        self.prompt_history.push(prompt);
        self.history_index = None;
    }

    fn update_seq(&mut self, event: &ServerEvent) {
        if matches!(event, ServerEvent::Unknown) {
            self.skipped_events += 1;
            return;
        }
        let Some(seq) = event_seq(event) else { return };
        let next = self.last_seq + 1;
        if self.last_seq != 0 && !(next..=next + self.skipped_events).contains(&seq) {
            self.push_item(TranscriptItem::Error(format!(
                "Protocol event sequence jumped from {} to {seq}",
                self.last_seq
            )));
        }
        self.last_seq = seq;
        self.skipped_events = 0;
    }

    pub(crate) fn handle_server_event(&mut self, event: ServerEvent) {
        self.update_seq(&event);
        match event {
            ServerEvent::QueueSnapshot {
                entries, paused, ..
            } => {
                self.queue = entries.into();
                self.queue_paused = paused;
                self.queue_selected = None;
            }
            ServerEvent::QueueEnqueued {
                entry, position, ..
            } => {
                self.queue.insert(position.min(self.queue.len()), entry);
            }
            ServerEvent::QueueUpdated { entry, .. } => self.queue_updated(entry),
            ServerEvent::QueueMoved {
                queue_id,
                position,
                revision,
                ..
            } => self.queue_moved(&queue_id, position, revision),
            ServerEvent::QueueRemoved { queue_id, .. }
            | ServerEvent::QueueDequeued { queue_id, .. } => self.queue_removed(&queue_id),
            ServerEvent::SessionPaused { paused, .. } => self.queue_paused = paused,
            ServerEvent::TurnStarted {
                turn_id, origin, ..
            } => self.turn_started(turn_id, origin),
            ServerEvent::AssistantDelta { content, .. } => self.assistant_delta(content),
            ServerEvent::AssistantCompleted { content, .. } => self.assistant_completed(content),
            ServerEvent::ToolProposed {
                call_id,
                name,
                arguments,
                ..
            } => self.push_item(TranscriptItem::Tool {
                call_id,
                name: call_label(&name, &arguments),
                status: ToolStatus::Proposed,
                arguments: arguments.to_string(),
                output: String::new(),
                progress: String::new(),
                expanded: false,
            }),
            ServerEvent::ApprovalRequested {
                call_id,
                approval_id,
                name,
                risk,
                cwd,
                summary,
                ..
            } => {
                self.set_tool_status(&call_id, ToolStatus::Approval, None);
                self.pending_approval = Some(PendingApproval {
                    id: approval_id,
                    name,
                    risk,
                    cwd,
                    summary,
                });
            }
            ServerEvent::ToolStarted { call_id, .. } => {
                self.pending_approval = None;
                self.set_tool_status(&call_id, ToolStatus::Running, None);
            }
            ServerEvent::ToolProgress { call_id, text, .. } => {
                self.set_tool_progress(&call_id, &text);
            }
            ServerEvent::ToolCompleted {
                call_id,
                success,
                output,
                error,
                ..
            } => self.tool_completed(&call_id, success, error, output),
            ServerEvent::ContextCompacted {
                after_tokens,
                removed_messages,
                ..
            } => {
                self.context_after_tokens = Some(after_tokens);
                self.push_item(TranscriptItem::System(format!(
                    "Context compacted: {after_tokens} estimated tokens, {removed_messages} messages omitted."
                )));
            }
            ServerEvent::SessionTrimmed {
                removed_messages,
                history_bytes,
                ..
            } => {
                self.history_bytes = Some(history_bytes);
                self.push_item(TranscriptItem::System(format!(
                    "Session history trimmed: {removed_messages} messages removed."
                )));
            }
            ServerEvent::TurnCompleted { steps, usage, .. } => self.turn_completed(steps, &usage),
            ServerEvent::TurnCancelled { .. } => self.turn_ended(
                ToolStatus::Cancelled,
                TranscriptItem::System("Turn cancelled.".into()),
            ),
            ServerEvent::TurnFailed { code, message, .. } => self.turn_ended(
                ToolStatus::Failed,
                TranscriptItem::Error(format!("{code}: {message}")),
            ),
            ServerEvent::SessionCleared { .. } => {
                self.items.clear();
                self.prompt_history.clear();
                self.queue.clear();
            }
            ServerEvent::Error { code, message, .. } => {
                self.push_item(TranscriptItem::Error(format!("{code}: {message}")));
            }
            // Events a newer server added are skipped.
            ServerEvent::Initialized { .. }
            | ServerEvent::SessionStarted { .. }
            | ServerEvent::DaemonStatus { .. }
            | ServerEvent::Unknown => {}
        }
        self.items.trim();
    }

    fn queue_updated(&mut self, entry: QueueEntry) {
        if let Some(existing) = self
            .queue
            .iter_mut()
            .find(|existing| existing.queue_id == entry.queue_id)
        {
            *existing = entry;
        }
    }

    fn queue_moved(&mut self, queue_id: &str, position: usize, revision: u64) {
        if let Some(index) = self
            .queue
            .iter()
            .position(|entry| entry.queue_id == queue_id)
            && let Some(mut entry) = self.queue.remove(index)
        {
            entry.revision = revision;
            self.queue.insert(position.min(self.queue.len()), entry);
        }
    }

    fn queue_removed(&mut self, queue_id: &str) {
        if let Some(index) = self
            .queue
            .iter()
            .position(|entry| entry.queue_id == queue_id)
        {
            self.queue.remove(index);
            self.queue_selected = self.queue_selected.and_then(|selected| {
                if self.queue.is_empty() {
                    None
                } else {
                    Some(selected.min(self.queue.len() - 1))
                }
            });
        }
    }

    fn turn_started(&mut self, turn_id: String, origin: Option<TurnOrigin>) {
        self.turn = Some(ActiveTurn {
            id: Some(turn_id),
            started_at: Instant::now(),
        });
        if let Some(origin) = origin {
            self.push_item(TranscriptItem::System(format!(
                "Background work finished ({}); SCV is reporting it.",
                origin.jobs.join(", ")
            )));
        }
    }

    fn assistant_delta(&mut self, content: String) {
        let appended = self.items.update_back(|item| match item {
            TranscriptItem::Assistant {
                content: current,
                streaming: true,
            } => {
                current.push_str(&content);
                true
            }
            _ => false,
        });
        if appended != Some(true) {
            self.push_item(TranscriptItem::Assistant {
                content,
                streaming: true,
            });
        }
    }

    fn assistant_completed(&mut self, content: String) {
        if matches!(self.items.back(), Some(TranscriptItem::Assistant { .. })) {
            self.items.update_back(|item| {
                if let TranscriptItem::Assistant {
                    content: current,
                    streaming,
                } = item
                {
                    *current = content;
                    *streaming = false;
                }
            });
        } else if !content.is_empty() {
            self.push_item(TranscriptItem::Assistant {
                content,
                streaming: false,
            });
        }
    }

    fn tool_completed(
        &mut self,
        call_id: &str,
        success: bool,
        error: Option<ToolErrorKind>,
        output: String,
    ) {
        self.pending_approval = None;
        let status = match error {
            _ if success => ToolStatus::Success,
            Some(ToolErrorKind::Denied) => ToolStatus::Denied,
            Some(ToolErrorKind::Cancelled) => ToolStatus::Cancelled,
            _ => ToolStatus::Failed,
        };
        self.set_tool_status(call_id, status, Some(output));
    }

    fn turn_completed(&mut self, steps: usize, usage: &Usage) {
        self.turn = None;
        self.push_item(TranscriptItem::System(format!(
            "Completed in {steps} step(s){}.",
            usage
                .input_tokens
                .zip(usage.output_tokens)
                .map_or_else(String::new, |(input, output)| format!(
                    " · {input}↑ {output}↓"
                ))
        )));
    }

    /// A turn cancelled or failed: its unfinished tools get `status`.
    fn turn_ended(&mut self, status: ToolStatus, note: TranscriptItem) {
        self.finish_pending_tools(status);
        self.turn = None;
        self.pending_approval = None;
        self.push_item(note);
    }

    /// Show the newest line of a running tool's progress under it.
    fn set_tool_progress(&mut self, call_id: &str, text: &str) {
        let latest = text.lines().rev().find(|line| !line.trim().is_empty());
        self.items.update_last(
            |item| matches!(item, TranscriptItem::Tool { call_id: current, .. } if current == call_id),
            |item| {
                if let TranscriptItem::Tool {
                    status, progress, ..
                } = item
                    && *status == ToolStatus::Running
                    && let Some(latest) = latest
                {
                    *progress = bounded_text(latest.trim(), MAX_PROGRESS_DISPLAY_CHARS);
                }
            },
        );
    }

    fn set_tool_status(&mut self, call_id: &str, status: ToolStatus, output: Option<String>) {
        self.items.update_last(
            |item| matches!(item, TranscriptItem::Tool { call_id: current, .. } if current == call_id),
            |item| {
                if let TranscriptItem::Tool {
                    status: current_status,
                    output: current_output,
                    progress,
                    ..
                } = item
                {
                    *current_status = status;
                    if status != ToolStatus::Running {
                        progress.clear();
                    }
                    if let Some(output) = output {
                        *current_output = output;
                    }
                }
            },
        );
    }

    fn finish_pending_tools(&mut self, status: ToolStatus) {
        self.items.update_all(|item| {
            if let TranscriptItem::Tool {
                status: current, ..
            } = item
                && matches!(
                    current,
                    ToolStatus::Proposed | ToolStatus::Approval | ToolStatus::Running
                )
            {
                *current = status;
            }
        });
    }
}

/// How the transcript names a call: an `agent` call with the agent it
/// names (`agent codex`), or else the conversation it continues (`agent
/// codex-1`), since every delegation is the same tool; any other call by its
/// tool. A value that is not a plain name is left to the arguments shown
/// beside it.
pub(crate) fn call_label(name: &str, arguments: &serde_json::Value) -> String {
    let plain = |key: &str| {
        arguments
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 64
                    && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
    };
    match plain("agent").or_else(|| plain("session")) {
        Some(agent) if name == "agent" => format!("{name} {agent}"),
        _ => name.to_owned(),
    }
}

/// The event's sequence number, for events that carry one.
fn event_seq(event: &ServerEvent) -> Option<u64> {
    match event {
        ServerEvent::TurnStarted { seq, .. }
        | ServerEvent::AssistantDelta { seq, .. }
        | ServerEvent::AssistantCompleted { seq, .. }
        | ServerEvent::ToolProposed { seq, .. }
        | ServerEvent::ApprovalRequested { seq, .. }
        | ServerEvent::ToolStarted { seq, .. }
        | ServerEvent::ToolProgress { seq, .. }
        | ServerEvent::ToolCompleted { seq, .. }
        | ServerEvent::ContextCompacted { seq, .. }
        | ServerEvent::SessionTrimmed { seq, .. }
        | ServerEvent::SessionCleared { seq, .. }
        | ServerEvent::TurnCompleted { seq, .. }
        | ServerEvent::TurnCancelled { seq, .. }
        | ServerEvent::TurnFailed { seq, .. } => Some(*seq),
        ServerEvent::QueueSnapshot { seq, .. }
        | ServerEvent::QueueEnqueued { seq, .. }
        | ServerEvent::QueueUpdated { seq, .. }
        | ServerEvent::QueueMoved { seq, .. }
        | ServerEvent::QueueRemoved { seq, .. }
        | ServerEvent::QueueDequeued { seq, .. }
        | ServerEvent::SessionPaused { seq, .. } => Some(*seq),
        ServerEvent::Initialized { .. }
        | ServerEvent::SessionStarted { .. }
        | ServerEvent::DaemonStatus { .. }
        | ServerEvent::Error { .. }
        | ServerEvent::Unknown => None,
    }
}
