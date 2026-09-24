//! Turning a turn's `CoreEvent`s into protocol `ServerEvent`s.

use async_trait::async_trait;
use scv_core::{AgentError, CoreEvent, EventSink};
use scv_protocol::ServerEvent;
use tokio_util::sync::CancellationToken;

use crate::{
    outbound::{OutboundSender, send_turn_event},
    session::{TurnMeta, next_seq},
};

/// Cut progress text to `MAX_PROGRESS_EVENT_BYTES` on a character boundary.
pub(crate) fn bounded_progress(mut text: String) -> String {
    let limit = scv_core::MAX_PROGRESS_EVENT_BYTES;
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

pub(crate) struct ProtocolSink {
    pub(crate) meta: TurnMeta,
    pub(crate) output: OutboundSender,
    pub(crate) cancellation: CancellationToken,
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
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
                success: !output.is_error,
                output: output.content,
                truncated: output.truncated,
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
