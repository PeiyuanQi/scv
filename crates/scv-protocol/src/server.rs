//! [`ServerEvent`]: everything the server sends.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DaemonStatus, ErrorCode, PeerInfo, QueueEntry, ToolErrorKind, TurnOrigin, Usage};

/// A message from server to client. Serialized as one JSON object per line,
/// tagged by `type` (such as `turn.completed`). Turn events carry the
/// session's consecutive `seq`.
///
/// A `type` this client does not know parses as [`ServerEvent::Unknown`], so
/// a newer server can add events without breaking older clients; a client
/// ignores them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerEvent {
    /// The answer to a `daemon.control` status request.
    #[serde(rename = "daemon.status")]
    DaemonStatus {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The daemon's state.
        status: DaemonStatus,
    },
    /// The answer to `initialize`.
    #[serde(rename = "initialized")]
    Initialized {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) the server speaks.
        protocol_version: u32,
        /// The server's name and version.
        server: PeerInfo,
    },
    /// The answer to `session.start`, with the limits the client should use.
    #[serde(rename = "session.started")]
    SessionStarted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's workspace directory.
        cwd: String,
        /// The model answering.
        model: String,
        /// The model's context window.
        context_max_tokens: usize,
        /// Largest event frame the server sends; clients size their reads by it.
        max_server_frame_bytes: usize,
        /// Transcript bytes a client should keep for display.
        max_transcript_bytes: usize,
        /// Transcript items a client should keep for display.
        max_transcript_items: usize,
        /// Prompt-history bytes a client should keep.
        max_prompt_history_bytes: usize,
        /// Prompt-history entries a client should keep.
        max_prompt_history_items: usize,
    },
    /// The whole queue, sent when a session starts or on request.
    #[serde(rename = "queue.snapshot")]
    QueueSnapshot {
        /// The client request this answers or belongs to.
        request_id: Option<String>,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Queued prompts in the order they will run.
        entries: Vec<QueueEntry>,
        /// Whether queued prompts wait instead of starting.
        paused: bool,
    },
    /// A prompt was queued behind the running turn.
    #[serde(rename = "queue.enqueued")]
    QueueEnqueued {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The queued prompt.
        entry: QueueEntry,
        /// Its index in the queue.
        position: usize,
    },
    /// A queued prompt's text changed.
    #[serde(rename = "queue.updated")]
    QueueUpdated {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The queued prompt.
        entry: QueueEntry,
    },
    /// A queued prompt moved.
    #[serde(rename = "queue.moved")]
    QueueMoved {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The queued prompt.
        queue_id: String,
        /// Its index in the queue.
        position: usize,
        /// The entry's revision after the change.
        revision: u64,
    },
    /// A queued prompt was dropped.
    #[serde(rename = "queue.removed")]
    QueueRemoved {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The queued prompt.
        queue_id: String,
        /// The entry's revision after the change.
        revision: u64,
    },
    /// A queued prompt left the queue to run as a turn.
    #[serde(rename = "queue.dequeued")]
    QueueDequeued {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The queued prompt.
        queue_id: String,
        /// The turn this belongs to.
        turn_id: String,
    },
    /// The queue was held or released.
    #[serde(rename = "session.paused")]
    SessionPaused {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Whether queued prompts wait instead of starting.
        paused: bool,
    },
    /// A turn began.
    #[serde(rename = "turn.started")]
    TurnStarted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    /// Answer text as it streams.
    #[serde(rename = "assistant.delta")]
    AssistantDelta {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The text.
        content: String,
    },
    /// The model's complete answer for one step.
    #[serde(rename = "assistant.completed")]
    AssistantCompleted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The text.
        content: String,
    },
    /// The model asked to call a tool.
    #[serde(rename = "tool.proposed")]
    ToolProposed {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The tool call, as the model named it.
        call_id: String,
        /// The tool.
        name: String,
        /// The call's arguments as the model sent them.
        arguments: Value,
    },
    /// A tool call waits for a person's decision.
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Answer with `approval.resolve` naming this ID.
        approval_id: String,
        /// The tool call, as the model named it.
        call_id: String,
        /// The tool.
        name: String,
        /// The call's risk, which selected the approval rule.
        risk: String,
        /// The session's workspace directory.
        cwd: String,
        /// What the call will do, for the person deciding.
        summary: String,
    },
    /// An approved tool call began running.
    #[serde(rename = "tool.started")]
    ToolStarted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The tool call, as the model named it.
        call_id: String,
        /// The tool.
        name: String,
    },
    /// Short status lines from a running tool, at most two events a second
    /// per call and 512 bytes each. Display only; not part of the history.
    #[serde(rename = "tool.progress")]
    ToolProgress {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The tool call, as the model named it.
        call_id: String,
        /// The status lines.
        text: String,
    },
    /// A tool call finished; its output goes to the model.
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// The tool call, as the model named it.
        call_id: String,
        /// The tool.
        name: String,
        /// Whether the tool succeeded.
        success: bool,
        /// What the model sees as the result.
        output: String,
        /// Whether `output` was cut to its limit.
        truncated: bool,
        /// Why the call failed; absent when it succeeded, and from servers
        /// before 0.3.0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ToolErrorKind>,
    },
    /// Older history was summarized to fit the model's context window.
    #[serde(rename = "context.compacted")]
    ContextCompacted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Estimated request size before compaction.
        before_tokens: usize,
        /// Estimated request size as sent.
        after_tokens: usize,
        /// Messages left out or removed.
        removed_messages: usize,
    },
    /// Old turns were removed to keep the session within its history limits.
    #[serde(rename = "session.trimmed")]
    SessionTrimmed {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Messages left out or removed.
        removed_messages: usize,
        /// Size of the session history afterwards.
        history_bytes: usize,
    },
    /// The session's history and queue were cleared.
    #[serde(rename = "session.cleared")]
    SessionCleared {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
    },
    /// A turn finished.
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Model requests the turn made.
        steps: usize,
        /// Tokens the turn used, as the provider reported them.
        usage: Usage,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    /// A turn was cancelled; its history changes were rolled back.
    #[serde(rename = "turn.cancelled")]
    TurnCancelled {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    /// A turn failed; its history changes were rolled back.
    #[serde(rename = "turn.failed")]
    TurnFailed {
        /// The client request this answers or belongs to.
        request_id: String,
        /// The session this concerns.
        session_id: String,
        /// The turn this belongs to.
        turn_id: String,
        /// The session's event sequence number: consecutive, so a gap means events were lost.
        seq: u64,
        /// Stable machine-readable error code.
        code: ErrorCode,
        /// What went wrong, for people.
        message: String,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    /// A request failed, or a connection-level error.
    #[serde(rename = "error")]
    Error {
        /// The client request this answers or belongs to.
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// Stable machine-readable error code.
        code: ErrorCode,
        /// What went wrong, for people.
        message: String,
        /// Whether the connection is unusable after this error.
        fatal: bool,
    },
    /// An event this client does not know, from a newer server. Never sent.
    #[serde(other, rename = "unknown")]
    Unknown,
}

impl ServerEvent {
    /// The submitting request of a turn-scoped event, which identifies the
    /// turn to a client that has several turns' events interleaved.
    pub fn turn_request_id(&self) -> Option<&str> {
        match self {
            Self::QueueDequeued { request_id, .. }
            | Self::TurnStarted { request_id, .. }
            | Self::AssistantDelta { request_id, .. }
            | Self::AssistantCompleted { request_id, .. }
            | Self::ToolProposed { request_id, .. }
            | Self::ApprovalRequested { request_id, .. }
            | Self::ToolStarted { request_id, .. }
            | Self::ToolProgress { request_id, .. }
            | Self::ToolCompleted { request_id, .. }
            | Self::ContextCompacted { request_id, .. }
            | Self::TurnCompleted { request_id, .. }
            | Self::TurnCancelled { request_id, .. }
            | Self::TurnFailed { request_id, .. } => Some(request_id),
            _ => None,
        }
    }
}
