//! Stable error codes: why a request or turn failed ([`ErrorCode`]), and why
//! a tool call did ([`ToolErrorKind`]).
//!
//! Both are closed lists on the wire, but a newer server may add a value. An
//! unrecognized string parses as `Unknown` rather than failing the whole
//! event, so a client keeps working and shows the event's message.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a request (`error`) or a turn (`turn.failed`) failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The frame is not JSON, or not a message this server knows, such as a
    /// `daemon.control` action it predates.
    InvalidJson,
    /// A message other than `initialize` came first.
    NotInitialized,
    /// The client speaks another protocol version; the connection closes.
    VersionMismatch,
    /// The message is well-formed but its values are refused.
    InvalidRequest,
    /// This endpoint does not offer the request, such as component
    /// management on stdio.
    Unsupported,
    /// No session, or a different one, is running on this connection.
    SessionNotFound,
    /// The session must be idle for this request.
    TurnActive,
    /// The turn named is not running.
    TurnNotFound,
    /// The approval is unknown or already resolved.
    ApprovalNotFound,
    /// The session's queue is full.
    QueueLimit,
    /// The queued prompt is gone.
    QueueNotFound,
    /// The queued prompt's revision is stale.
    QueueConflict,
    /// A channel account could not be changed.
    ComponentError,
    /// A delegation control request named nothing, or an unknown handle.
    DelegationError,
    /// The daemon refused to schedule a restart.
    RestartError,
    /// The daemon could not ask the owner a question (no owner chat to ask
    /// in, or one already waiting there), or does not know the one named.
    ConfirmError,
    /// The model provider failed the turn.
    ProviderError,
    /// The turn's history does not fit the model's context window.
    ContextLimit,
    /// The turn reached `agent.max_steps`.
    StepLimit,
    /// The turn alone exceeds the session's history limits.
    HistoryLimit,
    /// A response exceeded a size limit.
    ResponseLimit,
    /// A response asked for too many tool calls, or too large arguments.
    ToolLimit,
    /// A server invariant failed.
    InternalError,
    /// A code this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// The code as it appears on the wire, such as `queue_limit`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::NotInitialized => "not_initialized",
            Self::VersionMismatch => "version_mismatch",
            Self::InvalidRequest => "invalid_request",
            Self::Unsupported => "unsupported",
            Self::SessionNotFound => "session_not_found",
            Self::TurnActive => "turn_active",
            Self::TurnNotFound => "turn_not_found",
            Self::ApprovalNotFound => "approval_not_found",
            Self::QueueLimit => "queue_limit",
            Self::QueueNotFound => "queue_not_found",
            Self::QueueConflict => "queue_conflict",
            Self::ComponentError => "component_error",
            Self::DelegationError => "delegation_error",
            Self::RestartError => "restart_error",
            Self::ConfirmError => "confirm_error",
            Self::ProviderError => "provider_error",
            Self::ContextLimit => "context_limit",
            Self::StepLimit => "step_limit",
            Self::HistoryLimit => "history_limit",
            Self::ResponseLimit => "response_limit",
            Self::ToolLimit => "tool_limit",
            Self::InternalError => "internal_error",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a tool call failed (`tool.completed.error`). The model reads the
/// call's output either way; this tells a client how to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolErrorKind {
    /// The approval policy or the user refused the call.
    Denied,
    /// The call was cancelled while it ran.
    Cancelled,
    /// The call's arguments were refused before it ran.
    InvalidArguments,
    /// The tool, or the agent it runs, could not be used: missing, signed
    /// out, or its provider unreachable.
    Unavailable,
    /// A configured size, count, depth, or time limit stopped the call.
    Limit,
    /// The call ran and failed.
    Failed,
    /// The model named a tool the session does not have.
    UnknownTool,
    /// A kind this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

impl ToolErrorKind {
    /// The kind as it appears on the wire, such as `denied`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::InvalidArguments => "invalid_arguments",
            Self::Unavailable => "unavailable",
            Self::Limit => "limit",
            Self::Failed => "failed",
            Self::UnknownTool => "unknown_tool",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ToolErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
