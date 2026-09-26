//! Turning a turn's `CoreEvent`s into protocol `ServerEvent`s.

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{AgentError, CoreEvent, EventSink, ToolFailure};
use scv_protocol::{ErrorCode, ServerEvent, ToolErrorKind};
use scv_tools::background::BackgroundJobs;
use tokio_util::sync::CancellationToken;

use crate::{
    outbound::{OutboundSender, send_turn_event},
    session::{TurnMeta, next_seq},
};

/// Cut progress text to `MAX_PROGRESS_EVENT_BYTES` on a character boundary.
pub(crate) fn bounded_progress(mut text: String) -> String {
    let limit = scv_core::MAX_PROGRESS_EVENT_BYTES;
    let end = scv_client::text::utf8_prefix(&text, limit).len();
    text.truncate(end);
    text
}

/// The `turn.failed` code of a turn that ended with `error`.
pub(crate) fn error_code(error: &AgentError) -> ErrorCode {
    match error {
        AgentError::Provider(_) => ErrorCode::ProviderError,
        AgentError::ContextLimit(_) => ErrorCode::ContextLimit,
        AgentError::StepLimit => ErrorCode::StepLimit,
        AgentError::HistoryLimit(_) => ErrorCode::HistoryLimit,
        AgentError::ResponseLimit(_) => ErrorCode::ResponseLimit,
        AgentError::ToolLimit(_) => ErrorCode::ToolLimit,
        // A cancelled turn ends with `turn.cancelled`, never `turn.failed`.
        AgentError::Cancelled | AgentError::Internal(_) => ErrorCode::InternalError,
    }
}

/// How a failed tool call is shown in `tool.completed.error`.
pub(crate) fn tool_error_kind(failure: ToolFailure) -> ToolErrorKind {
    match failure {
        ToolFailure::Denied => ToolErrorKind::Denied,
        ToolFailure::Cancelled => ToolErrorKind::Cancelled,
        ToolFailure::InvalidArguments => ToolErrorKind::InvalidArguments,
        ToolFailure::Unavailable => ToolErrorKind::Unavailable,
        ToolFailure::Limit => ToolErrorKind::Limit,
        ToolFailure::Failed => ToolErrorKind::Failed,
        ToolFailure::UnknownTool => ToolErrorKind::UnknownTool,
    }
}

pub(crate) struct ProtocolSink {
    pub(crate) meta: TurnMeta,
    pub(crate) output: OutboundSender,
    pub(crate) cancellation: CancellationToken,
    /// The session's background jobs, whose changes each call's
    /// `tool.completed` carries.
    pub(crate) background: Option<Arc<BackgroundJobs>>,
}

#[async_trait]
impl EventSink for ProtocolSink {
    async fn emit(&self, event: CoreEvent) -> Result<(), AgentError> {
        let seq = next_seq(&self.meta.seq);
        let event = match event {
            CoreEvent::AssistantDelta { content } => ServerEvent::AssistantDelta {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                content,
            },
            CoreEvent::AssistantCompleted { content } => ServerEvent::AssistantCompleted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                content,
            },
            CoreEvent::ToolProposed {
                call_id,
                name,
                arguments,
            } => ServerEvent::ToolProposed {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
                arguments,
            },
            CoreEvent::ToolStarted { call_id, name } => ServerEvent::ToolStarted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
            },
            CoreEvent::ToolProgress { call_id, text } => ServerEvent::ToolProgress {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                // Tools report through a bounded sink; bound again here so a
                // custom tool can never grow a frame past the documented size.
                text: bounded_progress(text),
            },
            CoreEvent::ToolCompleted {
                call_id,
                name,
                output,
            } => ServerEvent::ToolCompleted {
                jobs: self
                    .background
                    .as_ref()
                    .map_or_else(Vec::new, |jobs| jobs.take_changes(&call_id)),
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
                success: !output.is_error(),
                output: output.content,
                truncated: output.truncated,
                error: output.failure.map(tool_error_kind),
            },
            CoreEvent::ContextCompacted {
                before_tokens,
                after_tokens,
                removed_messages,
            } => ServerEvent::ContextCompacted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                before_tokens,
                after_tokens,
                removed_messages,
            },
            CoreEvent::SessionTrimmed {
                removed_messages,
                history_bytes,
            } => ServerEvent::SessionTrimmed {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                seq,
                removed_messages,
                history_bytes,
            },
        };
        send_turn_event(
            &self.output,
            event,
            self.meta.max_server_frame,
            &self.cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
