//! [`ClientMessage`]: everything a client sends.

use serde::{Deserialize, Serialize};

use crate::{Attachment, DaemonCommand, PeerInfo};
#[cfg(doc)]
use crate::{CHAT_ATTACH_TOOL, MAX_CHANNEL_NAME_BYTES, MAX_TURN_ATTACHMENTS};

/// A message from client to server. Serialized as one JSON object per line,
/// tagged by `type` (such as `turn.start`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Control the daemon itself (status, reload, channels, delegations,
    /// restarts). Only the daemon socket accepts it.
    #[serde(rename = "daemon.control")]
    DaemonControl {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// What to do.
        command: DaemonCommand,
    },
    /// The first message on every connection.
    #[serde(rename = "initialize")]
    Initialize {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) the client speaks.
        protocol_version: u32,
        /// Who is connecting.
        client: PeerInfo,
    },
    /// Start a session in a workspace. Each connection has at most one.
    #[serde(rename = "session.start")]
    SessionStart {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The workspace directory.
        cwd: String,
        /// Provider profile to use instead of the configured default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Model to use instead of the configured default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Provider endpoint to use instead of the configured one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        /// Start the session without tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no_tools: Option<bool>,
        /// Delegation depth of the client, when it is itself a delegated
        /// agent (such as a nested SCV). Tools started from the session count
        /// from it, so the depth limit holds across processes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_depth: Option<u32>,
        /// The chat channel this session answers on, as its users name it
        /// (such as `WeChat` or `Feishu`). The user reads short plain-text
        /// replies there and never sees tool calls, so the server tells the
        /// model; a chat client also delivers files the model attaches to
        /// its reply, so a session with tools offers [`CHAT_ATTACH_TOOL`].
        /// At most [`MAX_CHANNEL_NAME_BYTES`], without control characters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// The client approves every approval request of this session
        /// without asking anyone. Background jobs, which outlive the turn
        /// that could carry their requests, then get the same answer;
        /// otherwise they get only what the approval policy grants unasked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_approve: Option<bool>,
    },
    /// Attach to an existing session. Not supported: sessions belong to the
    /// connection that started them.
    #[serde(rename = "session.attach")]
    SessionAttach {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The workspace directory.
        cwd: String,
    },
    /// Send a prompt. It starts a turn at once, or queues behind the running
    /// one.
    #[serde(rename = "turn.start")]
    TurnStart {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The user's text.
        prompt: String,
        /// Files that come with the prompt, at most [`MAX_TURN_ATTACHMENTS`].
        /// The server lists them for the model and shows it images directly
        /// when the model accepts image input.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// Replace the text of a queued prompt.
    #[serde(rename = "queue.update")]
    QueueUpdate {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The queued prompt.
        queue_id: String,
        /// The entry's current revision; a stale one is refused.
        revision: u64,
        /// The user's text.
        prompt: String,
    },
    /// Reorder a queued prompt.
    #[serde(rename = "queue.move")]
    QueueMove {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The queued prompt.
        queue_id: String,
        /// The entry's current revision; a stale one is refused.
        revision: u64,
        /// Move before this entry; `None` moves it to the end.
        before_queue_id: Option<String>,
    },
    /// Drop a queued prompt.
    #[serde(rename = "queue.remove")]
    QueueRemove {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The queued prompt.
        queue_id: String,
        /// The entry's current revision; a stale one is refused.
        revision: u64,
    },
    /// Hold or release the queue; a running turn is not affected.
    #[serde(rename = "session.pause")]
    SessionPause {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// Whether queued prompts wait instead of starting.
        paused: bool,
    },
    /// Stop the running turn.
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The turn to cancel.
        turn_id: String,
    },
    /// Answer an `approval.requested` event.
    #[serde(rename = "approval.resolve")]
    ApprovalResolve {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
        /// The approval request being answered.
        approval_id: String,
        /// Whether the call may run.
        approved: bool,
    },
    /// Forget the session's history and queue.
    #[serde(rename = "session.clear")]
    SessionClear {
        /// Chosen by the client; the events that answer this message carry it.
        request_id: String,
        /// The session, as `session.started` named it.
        session_id: String,
    },
}

impl ClientMessage {
    /// The client-chosen ID that answering events carry.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Initialize { request_id, .. }
            | Self::DaemonControl { request_id, .. }
            | Self::SessionStart { request_id, .. }
            | Self::SessionAttach { request_id, .. }
            | Self::TurnStart { request_id, .. }
            | Self::QueueUpdate { request_id, .. }
            | Self::QueueMove { request_id, .. }
            | Self::QueueRemove { request_id, .. }
            | Self::SessionPause { request_id, .. }
            | Self::TurnCancel { request_id, .. }
            | Self::ApprovalResolve { request_id, .. }
            | Self::SessionClear { request_id, .. } => request_id,
        }
    }
}
