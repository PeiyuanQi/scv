//! SCV's provider-independent agent loop and the traits it is built from.
//!
//! [`AgentRuntime::run_turn`] runs one user turn: it selects the history the
//! model sees, asks the [`Provider`] for a response, runs the requested
//! [`Tool`]s in order through the [`ApprovalGate`], and repeats until the model
//! answers without tool calls, reporting everything to an [`EventSink`].
//! [`ContextPolicy`] decides what history fits. This crate knows no concrete
//! provider, tool, transport, or user interface; those live in the crates
//! that depend on it.

#![forbid(unsafe_code)]

mod approval;
mod context;
mod event;
mod history;
mod message;
mod progress;
mod provider;
mod runtime;
mod tool;

pub use approval::{ApprovalGate, ApprovalRequest};
pub use context::{
    BudgetContextPolicy, ContextConfig, ContextError, ContextPolicy, ContextSelection,
};
pub use event::{CoreEvent, EventSink};
pub use history::HistoryLimits;
pub use message::{IMAGE_TOKENS, ImageInput, Message, ToolCall, TurnInput};
pub use progress::{
    MAX_PROGRESS_EVENT_BYTES, MAX_PROGRESS_LINE_BYTES, PROGRESS_INTERVAL, ProgressSink,
};
pub use provider::{
    AssistantResponse, Provider, ProviderError, ProviderErrorKind, ProviderRequest, TextDeltaSink,
    Usage,
};
pub use runtime::{AgentConfig, AgentError, AgentRuntime, TurnOutcome};
pub use tool::{
    Tool, ToolApprovals, ToolContext, ToolError, ToolFailure, ToolOutput, ToolRegistry, ToolRisk,
    ToolSpec,
};
