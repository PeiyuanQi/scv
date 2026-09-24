//! [`AgentRuntime`]: the loop that runs one user turn.

use std::{future::Future, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    ApprovalGate, ApprovalRequest, ContextPolicy, CoreEvent, EventSink, HistoryLimits, Message,
    PROGRESS_INTERVAL, ProgressSink, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    TextDeltaSink, ToolApprovals, ToolCall, ToolContext, ToolOutput, ToolRegistry, TurnInput,
    Usage,
};

/// Settings of an [`AgentRuntime`].
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Most model requests in one turn before it fails with [`AgentError::StepLimit`].
    pub max_steps: usize,
    pub history_limits: HistoryLimits,
}

/// How a completed turn went: model requests made, and tokens used.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub steps: usize,
    pub usage: Usage,
}

/// Why a turn failed. [`code`](AgentError::code) is its stable wire name.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("turn cancelled")]
    Cancelled,
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    ContextLimit(String),
    #[error("agent reached its maximum step count")]
    StepLimit,
    #[error("{0}")]
    HistoryLimit(String),
    #[error("{0}")]
    ResponseLimit(String),
    #[error("{0}")]
    ToolLimit(String),
    #[error("{0}")]
    Internal(String),
}

impl AgentError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Provider(_) => "provider_error",
            Self::ContextLimit(_) => "context_limit",
            Self::StepLimit => "step_limit",
            Self::HistoryLimit(_) => "history_limit",
            Self::ResponseLimit(_) => "response_limit",
            Self::ToolLimit(_) => "tool_limit",
            Self::Internal(_) => "internal_error",
        }
    }
}

/// Runs user turns against one provider, tool registry, and context policy.
///
/// [`run_turn`](AgentRuntime::run_turn) appends the user's input to the
/// session history, then repeats up to `max_steps` times:
///
/// 1. the [`ContextPolicy`] selects the history that fits;
/// 2. the [`Provider`] answers, streaming text as [`CoreEvent::AssistantDelta`];
/// 3. with no tool calls the turn is done; otherwise each call, in order, is
///    checked by its tool, approved or denied by the [`ApprovalGate`], and run,
///    and its output is added to history for the next step.
///
/// Every step is reported to the [`EventSink`]. History stays within
/// [`HistoryLimits`] by trimming old turns into a note. If the turn fails or
/// is cancelled, history is restored to how it was before the turn.
pub struct AgentRuntime {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    context: Arc<dyn ContextPolicy>,
    config: AgentConfig,
    workspace: PathBuf,
}

impl AgentRuntime {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        context: Arc<dyn ContextPolicy>,
        config: AgentConfig,
        workspace: PathBuf,
    ) -> Self {
        Self {
            provider,
            tools,
            context,
            config,
            workspace,
        }
    }

    pub fn model(&self) -> &str {
        self.provider.model()
    }

    pub async fn run_turn(
        &self,
        history: &mut Vec<Message>,
        prompt: impl Into<TurnInput>,
        sink: Arc<dyn EventSink>,
        approvals: Arc<dyn ApprovalGate>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        let checkpoint = history.clone();
        let result = self
            .run_turn_inner(history, prompt.into(), sink, approvals, cancellation)
            .await;
        if result.is_err() {
            *history = checkpoint;
        }
        result
    }

    async fn run_turn_inner(
        &self,
        history: &mut Vec<Message>,
        prompt: TurnInput,
        sink: Arc<dyn EventSink>,
        approvals: Arc<dyn ApprovalGate>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        history.push(Message::User {
            content: prompt.text,
            images: prompt.images,
        });
        self.enforce_history_limits(history, sink.as_ref()).await?;
        let specs = self.tools.specs();
        let mut usage = Usage::default();

        for step in 1..=self.config.max_steps {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let selection = self
                .context
                .select(history, &self.config.system_prompt, &specs)
                .map_err(|error| AgentError::ContextLimit(error.to_string()))?;
            if selection.removed_messages > 0 {
                sink.emit(CoreEvent::ContextCompacted {
                    before_tokens: selection.before_tokens,
                    after_tokens: selection.after_tokens,
                    removed_messages: selection.removed_messages,
                })
                .await?;
            }
            let delta_sink: Arc<dyn TextDeltaSink> = Arc::new(ForwardDeltas {
                sink: Arc::clone(&sink),
            });
            let response = self
                .provider
                .complete(
                    ProviderRequest {
                        system_prompt: self.config.system_prompt.clone(),
                        messages: selection.messages,
                        tools: specs.clone(),
                    },
                    delta_sink,
                    cancellation.child_token(),
                )
                .await
                .map_err(map_provider_error)?;
            usage.add(&response.usage);
            sink.emit(CoreEvent::AssistantCompleted {
                content: response.content.clone(),
            })
            .await?;
            let calls = response.tool_calls.clone();
            history.push(Message::Assistant {
                content: response.content,
                tool_calls: response.tool_calls,
            });
            self.enforce_history_limits(history, sink.as_ref()).await?;
            if calls.is_empty() {
                return Ok(TurnOutcome { steps: step, usage });
            }

            for call in calls {
                if cancellation.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                sink.emit(CoreEvent::ToolProposed {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await?;
                let Some(tool) = self.tools.get(&call.name) else {
                    let output = ToolOutput::failure(format!("unknown tool: {}", call.name));
                    sink.emit(CoreEvent::ToolCompleted {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        output: output.clone(),
                    })
                    .await?;
                    history.push(Message::Tool {
                        call_id: call.id,
                        name: call.name,
                        content: output.content,
                        is_error: true,
                    });
                    self.enforce_history_limits(history, sink.as_ref()).await?;
                    continue;
                };
                let risk = match tool.risk(&call.arguments) {
                    Ok(risk) => risk,
                    Err(error) => {
                        self.record_tool_error(history, sink.as_ref(), &call, error.to_string())
                            .await?;
                        self.enforce_history_limits(history, sink.as_ref()).await?;
                        continue;
                    }
                };
                let summary = match tool.approval_summary(&call.arguments) {
                    Ok(summary) => summary,
                    Err(error) => {
                        self.record_tool_error(history, sink.as_ref(), &call, error.to_string())
                            .await?;
                        self.enforce_history_limits(history, sink.as_ref()).await?;
                        continue;
                    }
                };
                let approved = approvals
                    .approve(
                        ApprovalRequest {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            risk,
                            cwd: self.workspace.clone(),
                            summary,
                        },
                        cancellation.child_token(),
                    )
                    .await?;
                let output = if approved {
                    sink.emit(CoreEvent::ToolStarted {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                    })
                    .await?;
                    let progress = ProgressSink::buffered();
                    let execution = tool.execute(
                        call.arguments.clone(),
                        ToolContext {
                            workspace: self.workspace.clone(),
                            cancellation: cancellation.child_token(),
                            progress: progress.clone(),
                            approvals: ToolApprovals::new(Arc::clone(&approvals), call.id.clone()),
                        },
                    );
                    forward_progress(execution, &progress, sink.as_ref(), &call.id)
                        .await
                        .unwrap_or_else(|error| ToolOutput::failure(error.to_string()))
                } else {
                    ToolOutput::failure("tool call denied by policy or user")
                };
                sink.emit(CoreEvent::ToolCompleted {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                })
                .await?;
                history.push(Message::Tool {
                    call_id: call.id,
                    name: call.name,
                    content: output.content,
                    is_error: output.is_error,
                });
                self.enforce_history_limits(history, sink.as_ref()).await?;
            }
        }
        Err(AgentError::StepLimit)
    }

    async fn record_tool_error(
        &self,
        history: &mut Vec<Message>,
        sink: &dyn EventSink,
        call: &ToolCall,
        message: String,
    ) -> Result<(), AgentError> {
        let output = ToolOutput::failure(message);
        sink.emit(CoreEvent::ToolCompleted {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: output.clone(),
        })
        .await?;
        history.push(Message::Tool {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: output.content,
            is_error: true,
        });
        Ok(())
    }

    pub(crate) async fn enforce_history_limits(
        &self,
        history: &mut Vec<Message>,
        sink: &dyn EventSink,
    ) -> Result<(), AgentError> {
        crate::history::enforce_limits(history, &self.config.history_limits, sink).await
    }
}

/// Run a tool while forwarding what it reports to `sink`, at most one
/// `ToolProgress` event per `PROGRESS_INTERVAL`. Progress is best effort: a
/// sink error stops forwarding but never fails the tool.
async fn forward_progress<T>(
    execution: impl Future<Output = T>,
    progress: &ProgressSink,
    sink: &dyn EventSink,
    call_id: &str,
) -> T {
    let mut execution = std::pin::pin!(execution);
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + PROGRESS_INTERVAL,
        PROGRESS_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_sent: Option<tokio::time::Instant> = None;
    let mut forwarding = true;
    let result = loop {
        tokio::select! {
            biased;
            result = &mut execution => break result,
            _ = ticker.tick(), if forwarding => {
                if let Some(text) = progress.take() {
                    let event = CoreEvent::ToolProgress { call_id: call_id.to_owned(), text };
                    forwarding = sink.emit(event).await.is_ok();
                    last_sent = Some(tokio::time::Instant::now());
                }
            }
        }
    };
    // Lines reported since the last event, when sending them keeps the pace.
    if forwarding
        && last_sent.is_none_or(|sent| sent.elapsed() >= PROGRESS_INTERVAL)
        && let Some(text) = progress.take()
    {
        let _ = sink
            .emit(CoreEvent::ToolProgress {
                call_id: call_id.to_owned(),
                text,
            })
            .await;
    }
    result
}

fn map_provider_error(error: ProviderError) -> AgentError {
    match error.kind {
        ProviderErrorKind::Provider => AgentError::Provider(error.message),
        ProviderErrorKind::ResponseLimit => AgentError::ResponseLimit(error.message),
        ProviderErrorKind::ToolLimit => AgentError::ToolLimit(error.message),
        ProviderErrorKind::Cancelled => AgentError::Cancelled,
    }
}

struct ForwardDeltas {
    sink: Arc<dyn EventSink>,
}

#[async_trait]
impl TextDeltaSink for ForwardDeltas {
    async fn push(&self, delta: &str) -> Result<(), ProviderError> {
        self.sink
            .emit(CoreEvent::AssistantDelta {
                content: delta.to_owned(),
            })
            .await
            .map_err(|error| match error {
                AgentError::Cancelled => {
                    ProviderError::new(ProviderErrorKind::Cancelled, "turn cancelled")
                }
                AgentError::ResponseLimit(message) => {
                    ProviderError::new(ProviderErrorKind::ResponseLimit, message)
                }
                AgentError::ToolLimit(message) => {
                    ProviderError::new(ProviderErrorKind::ToolLimit, message)
                }
                error => ProviderError::new(ProviderErrorKind::Provider, error.to_string()),
            })
    }
}

#[cfg(test)]
mod tests;
