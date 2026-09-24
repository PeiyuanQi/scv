//! What the runtime reports while a turn runs.

use async_trait::async_trait;
use serde_json::Value;

use crate::{AgentError, ToolOutput};

#[derive(Debug, Clone)]
pub enum CoreEvent {
    AssistantDelta {
        content: String,
    },
    AssistantCompleted {
        content: String,
    },
    ToolProposed {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolStarted {
        call_id: String,
        name: String,
    },
    /// Status lines a running tool reported, for display only.
    ToolProgress {
        call_id: String,
        text: String,
    },
    ToolCompleted {
        call_id: String,
        name: String,
        output: ToolOutput,
    },
    ContextCompacted {
        before_tokens: usize,
        after_tokens: usize,
        removed_messages: usize,
    },
    SessionTrimmed {
        removed_messages: usize,
        history_bytes: usize,
    },
}

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: CoreEvent) -> Result<(), AgentError>;
}
