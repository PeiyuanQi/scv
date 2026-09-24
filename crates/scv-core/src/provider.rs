//! The [`Provider`] trait: one model request and its streamed response.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{Message, ToolCall, ToolSpec};

/// Token counts a provider reported; `None` when it reported none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    pub(crate) fn add(&mut self, other: &Self) {
        self.input_tokens = add_optional(self.input_tokens, other.input_tokens);
        self.output_tokens = add_optional(self.output_tokens, other.output_tokens);
    }
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

/// One model request: the system prompt, the selected history, and the tools.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

/// The model's complete answer to one request.
#[derive(Debug, Clone)]
pub struct AssistantResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// What kind of failure a [`ProviderError`] is; the runtime maps each to an
/// [`AgentError`](crate::AgentError).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Provider,
    ResponseLimit,
    ToolLimit,
    Cancelled,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Where a provider streams answer text as it arrives. An error from
/// [`push`](TextDeltaSink::push) means stop: the turn was cancelled or hit a
/// limit.
#[async_trait]
pub trait TextDeltaSink: Send + Sync {
    async fn push(&self, delta: &str) -> Result<(), ProviderError>;
}

/// A model backend. [`complete`](Provider::complete) sends one request,
/// streams text deltas to `deltas`, and returns the full answer with any tool
/// calls. It must stop promptly when `cancellation` fires.
#[async_trait]
pub trait Provider: Send + Sync {
    /// The model name, for display.
    fn model(&self) -> &str;

    async fn complete(
        &self,
        request: ProviderRequest,
        deltas: Arc<dyn TextDeltaSink>,
        cancellation: CancellationToken,
    ) -> Result<AssistantResponse, ProviderError>;
}
