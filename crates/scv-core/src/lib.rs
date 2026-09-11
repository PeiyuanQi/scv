//! SCV's provider-independent agent loop and extension traits.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    HistoryNote {
        content: String,
    },
}

impl Message {
    fn estimated_tokens(&self, bytes_per_token: usize) -> usize {
        let bytes = serde_json::to_vec(self).map_or(0, |value| value.len());
        bytes.div_ceil(bytes_per_token).saturating_add(4)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    ReadOnly,
    Filesystem,
    Process,
    Delegate,
}

impl ToolRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Filesystem => "filesystem",
            Self::Process => "process",
            Self::Delegate => "delegate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub truncated: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            truncated: false,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            truncated: false,
        }
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolError(pub String);

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError>;
    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError>;
    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.spec().name;
        if self.tools.contains_key(&name) {
            return Err(ToolError(format!("duplicate tool name: {name}")));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self.tools.values().map(|tool| tool.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    fn add(&mut self, other: &Self) {
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

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone)]
pub struct AssistantResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

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

#[async_trait]
pub trait TextDeltaSink: Send + Sync {
    async fn push(&self, delta: &str) -> Result<(), ProviderError>;
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn model(&self) -> &str;

    async fn complete(
        &self,
        request: ProviderRequest,
        deltas: Arc<dyn TextDeltaSink>,
        cancellation: CancellationToken,
    ) -> Result<AssistantResponse, ProviderError>;
}

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

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub name: String,
    pub risk: ToolRisk,
    pub cwd: PathBuf,
    pub summary: String,
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError>;
}

#[derive(Debug, Clone)]
pub struct ContextConfig {
    pub max_tokens: usize,
    pub reserve_output_tokens: usize,
    pub safety_margin_tokens: usize,
    pub bytes_per_token: usize,
    pub summary_max_chars: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            reserve_output_tokens: 8_192,
            safety_margin_tokens: 2_048,
            bytes_per_token: 3,
            summary_max_chars: 6_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextSelection {
    pub messages: Vec<Message>,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub removed_messages: usize,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ContextError(pub String);

pub trait ContextPolicy: Send + Sync {
    fn select(
        &self,
        history: &[Message],
        system_prompt: &str,
        tools: &[ToolSpec],
    ) -> Result<ContextSelection, ContextError>;
}

pub struct BudgetContextPolicy {
    config: ContextConfig,
}

impl BudgetContextPolicy {
    pub fn new(config: ContextConfig) -> Result<Self, ContextError> {
        if config.bytes_per_token == 0 {
            return Err(ContextError(
                "context.bytes_per_token must be positive".into(),
            ));
        }
        if config
            .reserve_output_tokens
            .saturating_add(config.safety_margin_tokens)
            >= config.max_tokens
        {
            return Err(ContextError(
                "context reserve and safety margin consume the model window".into(),
            ));
        }
        Ok(Self { config })
    }

    fn string_tokens(&self, value: &str) -> usize {
        value.len().div_ceil(self.config.bytes_per_token)
    }

    fn group_messages(history: &[Message]) -> Vec<Vec<Message>> {
        let mut groups: Vec<Vec<Message>> = Vec::new();
        for message in history {
            if matches!(message, Message::User { .. }) || groups.is_empty() {
                groups.push(Vec::new());
            }
            groups
                .last_mut()
                .expect("a group was just created")
                .push(message.clone());
        }
        groups
    }

    fn summarize(&self, messages: &[Message]) -> String {
        let mut output = format!(
            "[SCV compacted {} earlier messages. Bounded extracts follow.]\n",
            messages.len()
        );
        for message in messages {
            let (label, content) = match message {
                Message::User { content } => ("user", content.as_str()),
                Message::Assistant { content, .. } => ("assistant", content.as_str()),
                Message::Tool {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let status = if *is_error { "failed" } else { "ok" };
                    output.push_str(&format!("tool {name} ({status}): "));
                    ("", content.as_str())
                }
                Message::HistoryNote { content } => ("earlier", content.as_str()),
            };
            if !label.is_empty() {
                output.push_str(label);
                output.push_str(": ");
            }
            let tail = char_tail(content, 240);
            output.push_str(&tail.replace('\n', " "));
            output.push('\n');
            if output.chars().count() >= self.config.summary_max_chars {
                break;
            }
        }
        truncate_chars(&output, self.config.summary_max_chars)
    }
}

impl ContextPolicy for BudgetContextPolicy {
    fn select(
        &self,
        history: &[Message],
        system_prompt: &str,
        tools: &[ToolSpec],
    ) -> Result<ContextSelection, ContextError> {
        if history.is_empty() {
            return Ok(ContextSelection {
                messages: Vec::new(),
                before_tokens: 0,
                after_tokens: 0,
                removed_messages: 0,
            });
        }
        let tools_bytes = serde_json::to_vec(tools).map_or(0, |value| value.len());
        let static_tokens = self
            .string_tokens(system_prompt)
            .saturating_add(tools_bytes.div_ceil(self.config.bytes_per_token))
            .saturating_add(self.config.reserve_output_tokens)
            .saturating_add(self.config.safety_margin_tokens);
        if static_tokens >= self.config.max_tokens {
            return Err(ContextError(
                "system prompt and tool schemas exceed context budget".into(),
            ));
        }
        let budget = self.config.max_tokens - static_tokens;
        let groups = Self::group_messages(history);
        let newest = groups.last().expect("history produced at least one group");
        let newest_cost: usize = newest
            .iter()
            .map(|message| message.estimated_tokens(self.config.bytes_per_token))
            .sum();
        if newest_cost > budget {
            return Err(ContextError("newest turn exceeds context budget".into()));
        }

        let before_history_tokens: usize = history
            .iter()
            .map(|message| message.estimated_tokens(self.config.bytes_per_token))
            .sum();
        let mut selected_groups: Vec<Vec<Message>> = vec![newest.clone()];
        let mut selected_cost = newest_cost;
        for group in groups[..groups.len() - 1].iter().rev() {
            let cost: usize = group
                .iter()
                .map(|message| message.estimated_tokens(self.config.bytes_per_token))
                .sum();
            if selected_cost.saturating_add(cost) <= budget {
                selected_groups.insert(0, group.clone());
                selected_cost += cost;
            } else {
                break;
            }
        }

        let mut removed_messages = groups[..groups.len() - selected_groups.len()]
            .iter()
            .map(Vec::len)
            .sum::<usize>();
        if removed_messages > 0 {
            loop {
                let note = Message::HistoryNote {
                    content: self.summarize(&history[..removed_messages]),
                };
                let note_cost = note.estimated_tokens(self.config.bytes_per_token);
                if selected_cost.saturating_add(note_cost) <= budget {
                    let mut selected: Vec<Message> =
                        selected_groups.into_iter().flatten().collect();
                    selected.insert(0, note);
                    selected_cost += note_cost;
                    return Ok(ContextSelection {
                        messages: selected,
                        before_tokens: static_tokens.saturating_add(before_history_tokens),
                        after_tokens: static_tokens.saturating_add(selected_cost),
                        removed_messages,
                    });
                }
                if selected_groups.len() == 1 {
                    let available_tokens = budget.saturating_sub(selected_cost);
                    let content = match note {
                        Message::HistoryNote { content } => content,
                        _ => unreachable!(),
                    };
                    let Some(note) =
                        fit_history_note(&content, available_tokens, self.config.bytes_per_token)
                    else {
                        return Err(ContextError(
                            "compaction note cannot fit context budget".into(),
                        ));
                    };
                    let note_cost = note.estimated_tokens(self.config.bytes_per_token);
                    let mut selected: Vec<Message> =
                        selected_groups.into_iter().flatten().collect();
                    selected.insert(0, note);
                    selected_cost += note_cost;
                    return Ok(ContextSelection {
                        messages: selected,
                        before_tokens: static_tokens.saturating_add(before_history_tokens),
                        after_tokens: static_tokens.saturating_add(selected_cost),
                        removed_messages,
                    });
                }
                let removed_group = selected_groups.remove(0);
                let removed_cost: usize = removed_group
                    .iter()
                    .map(|message| message.estimated_tokens(self.config.bytes_per_token))
                    .sum();
                selected_cost = selected_cost.saturating_sub(removed_cost);
                removed_messages += removed_group.len();
            }
        }

        let selected: Vec<Message> = selected_groups.into_iter().flatten().collect();
        Ok(ContextSelection {
            messages: selected,
            before_tokens: static_tokens.saturating_add(before_history_tokens),
            after_tokens: static_tokens.saturating_add(selected_cost),
            removed_messages,
        })
    }
}

#[derive(Debug, Clone)]
pub struct HistoryLimits {
    pub max_bytes: usize,
    pub max_messages: usize,
    pub note_max_chars: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_messages: 10_000,
            note_max_chars: 4_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    pub max_steps: usize,
    pub history_limits: HistoryLimits,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub steps: usize,
    pub usage: Usage,
}

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
        prompt: String,
        sink: Arc<dyn EventSink>,
        approvals: Arc<dyn ApprovalGate>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        let checkpoint = history.clone();
        let result = self
            .run_turn_inner(history, prompt, sink, approvals, cancellation)
            .await;
        if result.is_err() {
            *history = checkpoint;
        }
        result
    }

    async fn run_turn_inner(
        &self,
        history: &mut Vec<Message>,
        prompt: String,
        sink: Arc<dyn EventSink>,
        approvals: Arc<dyn ApprovalGate>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        history.push(Message::User { content: prompt });
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
                    tool.execute(
                        call.arguments.clone(),
                        ToolContext {
                            workspace: self.workspace.clone(),
                            cancellation: cancellation.child_token(),
                        },
                    )
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

    async fn enforce_history_limits(
        &self,
        history: &mut Vec<Message>,
        sink: &dyn EventSink,
    ) -> Result<(), AgentError> {
        let limits = &self.config.history_limits;
        let mut total_removed = 0;
        while history.len() > limits.max_messages || history_bytes(history) > limits.max_bytes {
            let latest_user = history
                .iter()
                .rposition(|message| matches!(message, Message::User { .. }))
                .unwrap_or(0);
            let active = &history[latest_user..];
            if active.len() > limits.max_messages || history_bytes(active) > limits.max_bytes {
                return Err(AgentError::HistoryLimit(
                    "active turn exceeds configured session history limit".into(),
                ));
            }
            let first_user = history
                .iter()
                .position(|message| matches!(message, Message::User { .. }))
                .unwrap_or(latest_user);
            if first_user == latest_user {
                if matches!(history.first(), Some(Message::HistoryNote { .. })) {
                    history.remove(0);
                    total_removed += 1;
                    continue;
                }
                return Err(AgentError::HistoryLimit(
                    "session history cannot be reduced within its configured limit".into(),
                ));
            }
            let end = history[first_user + 1..]
                .iter()
                .position(|message| matches!(message, Message::User { .. }))
                .map(|index| first_user + 1 + index)
                .ok_or_else(|| {
                    AgentError::HistoryLimit(
                        "session history has no complete group available to trim".into(),
                    )
                })?;
            let removed: Vec<Message> = history.drain(..end).collect();
            total_removed += removed.len();
            let note = Message::HistoryNote {
                content: summarize_history_trim(&removed, total_removed, limits.note_max_chars),
            };
            if matches!(history.first(), Some(Message::HistoryNote { .. })) {
                history.remove(0);
            }
            history.insert(0, note);
        }
        if total_removed > 0 {
            sink.emit(CoreEvent::SessionTrimmed {
                removed_messages: total_removed,
                history_bytes: history_bytes(history),
            })
            .await?;
        }
        Ok(())
    }
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

fn history_bytes(history: &[Message]) -> usize {
    serde_json::to_vec(history).map_or(usize::MAX, |value| value.len())
}

fn summarize_history_trim(messages: &[Message], removed: usize, max_chars: usize) -> String {
    let mut note =
        format!("[SCV trimmed {removed} earlier canonical messages to enforce session limits.]\n");
    for message in messages {
        let (label, content) = match message {
            Message::User { content } => ("user", content.as_str()),
            Message::Assistant { content, .. } => ("assistant", content.as_str()),
            Message::Tool {
                name,
                content,
                is_error,
                ..
            } => {
                let status = if *is_error { "failed" } else { "ok" };
                note.push_str(&format!("tool {name} ({status}): "));
                ("", content.as_str())
            }
            Message::HistoryNote { content } => ("earlier", content.as_str()),
        };
        if !label.is_empty() {
            note.push_str(label);
            note.push_str(": ");
        }
        note.push_str(&char_tail(content, 160).replace('\n', " "));
        note.push('\n');
        if note.chars().count() >= max_chars {
            break;
        }
    }
    truncate_chars(&note, max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn fit_history_note(
    content: &str,
    available_tokens: usize,
    bytes_per_token: usize,
) -> Option<Message> {
    let chars: Vec<char> = content.chars().collect();
    let mut low = 0usize;
    let mut high = chars.len();
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = Message::HistoryNote {
            content: chars[..middle].iter().collect(),
        };
        if candidate.estimated_tokens(bytes_per_token) <= available_tokens {
            best = Some(candidate);
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best
}

fn char_tail(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    value
        .chars()
        .skip(count.saturating_sub(max_chars))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use tokio::sync::Notify;

    use super::*;

    struct ScriptedProvider {
        responses: Mutex<VecDeque<AssistantResponse>>,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn model(&self) -> &str {
            "test-model"
        }

        async fn complete(
            &self,
            _request: ProviderRequest,
            deltas: Arc<dyn TextDeltaSink>,
            _cancellation: CancellationToken,
        ) -> Result<AssistantResponse, ProviderError> {
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            deltas.push(&response.content).await?;
            Ok(response)
        }
    }

    struct CollectSink(Mutex<Vec<CoreEvent>>);

    #[async_trait]
    impl EventSink for CollectSink {
        async fn emit(&self, event: CoreEvent) -> Result<(), AgentError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    struct Allow;

    #[async_trait]
    impl ApprovalGate for Allow {
        async fn approve(
            &self,
            _request: ApprovalRequest,
            _cancellation: CancellationToken,
        ) -> Result<bool, AgentError> {
            Ok(true)
        }
    }

    struct Deny;

    #[async_trait]
    impl ApprovalGate for Deny {
        async fn approve(
            &self,
            _request: ApprovalRequest,
            _cancellation: CancellationToken,
        ) -> Result<bool, AgentError> {
            Ok(false)
        }
    }

    struct WaitForCancellation {
        entered: Arc<Notify>,
    }

    #[async_trait]
    impl ApprovalGate for WaitForCancellation {
        async fn approve(
            &self,
            _request: ApprovalRequest,
            cancellation: CancellationToken,
        ) -> Result<bool, AgentError> {
            self.entered.notify_one();
            cancellation.cancelled().await;
            Err(AgentError::Cancelled)
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "Echo a value".into(),
                parameters: serde_json::json!({
                    "type":"object",
                    "properties":{"value":{"type":"string"}},
                    "required":["value"]
                }),
            }
        }

        fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
            arguments
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError("value must be a string".into()))?;
            Ok(ToolRisk::ReadOnly)
        }

        fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
            self.risk(arguments)?;
            Ok("Echo a value".into())
        }

        async fn execute(
            &self,
            arguments: Value,
            _context: ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success(
                arguments["value"].as_str().unwrap_or_default(),
            ))
        }
    }

    #[tokio::test]
    async fn completes_a_simple_turn() {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::from([AssistantResponse {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Usage {
                    input_tokens: Some(3),
                    output_tokens: Some(1),
                },
            }])),
        });
        let runtime = AgentRuntime::new(
            provider,
            Arc::new(ToolRegistry::default()),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 2,
                history_limits: HistoryLimits::default(),
            },
            PathBuf::from("/tmp"),
        );
        let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
        let mut history = Vec::new();
        let outcome = runtime
            .run_turn(
                &mut history,
                "hello".into(),
                sink.clone(),
                Arc::new(Allow),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.steps, 1);
        assert_eq!(history.len(), 2);
        assert!(matches!(
            sink.0.lock().unwrap().last(),
            Some(CoreEvent::AssistantCompleted { .. })
        ));
    }

    #[test]
    fn context_keeps_tool_groups_together() {
        let policy = BudgetContextPolicy::new(ContextConfig {
            max_tokens: 120,
            reserve_output_tokens: 10,
            safety_margin_tokens: 10,
            bytes_per_token: 3,
            summary_max_chars: 120,
        })
        .unwrap();
        let history = vec![
            Message::User {
                content: "old request ".repeat(20),
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"a"}),
                }],
            },
            Message::Tool {
                call_id: "1".into(),
                name: "read".into(),
                content: "result".into(),
                is_error: false,
            },
            Message::User {
                content: "new".into(),
            },
        ];
        let selection = policy.select(&history, "system", &[]).unwrap();
        assert!(selection.removed_messages > 0);
        assert_eq!(
            selection.removed_messages,
            history.len() - (selection.messages.len() - 1)
        );
        assert!(matches!(
            selection.messages.last(),
            Some(Message::User { .. })
        ));
        assert!(
            !selection
                .messages
                .iter()
                .any(|message| matches!(message, Message::Tool { call_id, .. } if call_id == "1"))
        );
    }

    #[tokio::test]
    async fn repeated_history_trimming_rebuilds_the_note_and_makes_progress() {
        let runtime = AgentRuntime::new(
            Arc::new(ScriptedProvider {
                responses: Mutex::new(VecDeque::new()),
            }),
            Arc::new(ToolRegistry::default()),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 1,
                history_limits: HistoryLimits {
                    max_bytes: 4096,
                    max_messages: 3,
                    note_max_chars: 80,
                },
            },
            PathBuf::from("/tmp"),
        );
        let sink = CollectSink(Mutex::new(Vec::new()));
        let mut history = vec![
            Message::HistoryNote {
                content: "previous trim".into(),
            },
            Message::User {
                content: "old request".into(),
            },
            Message::Assistant {
                content: "old answer".into(),
                tool_calls: Vec::new(),
            },
            Message::User {
                content: "active request".into(),
            },
        ];
        runtime
            .enforce_history_limits(&mut history, &sink)
            .await
            .unwrap();
        assert!(history.len() <= 3);
        assert!(matches!(history.first(), Some(Message::HistoryNote { .. })));
        assert!(matches!(history.last(), Some(Message::User { .. })));
    }

    #[tokio::test]
    async fn active_turn_over_history_limit_rolls_back() {
        let runtime = AgentRuntime::new(
            Arc::new(ScriptedProvider {
                responses: Mutex::new(VecDeque::new()),
            }),
            Arc::new(ToolRegistry::default()),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 1,
                history_limits: HistoryLimits {
                    max_bytes: 16,
                    max_messages: 10,
                    note_max_chars: 8,
                },
            },
            PathBuf::from("/tmp"),
        );
        let sink = CollectSink(Mutex::new(Vec::new()));
        let mut history = Vec::new();
        let result = runtime
            .run_turn(
                &mut history,
                "too large for the configured history".into(),
                Arc::new(sink),
                Arc::new(Allow),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AgentError::HistoryLimit(_))));
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn executes_a_multi_step_tool_loop_and_aggregates_usage() {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::from([
                AssistantResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"value":"hello"}),
                    }],
                    usage: Usage {
                        input_tokens: Some(2),
                        output_tokens: Some(1),
                    },
                },
                AssistantResponse {
                    content: "done".into(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: Some(4),
                        output_tokens: Some(2),
                    },
                },
            ])),
        });
        let mut registry = ToolRegistry::default();
        registry.register(Arc::new(EchoTool)).unwrap();
        let runtime = AgentRuntime::new(
            provider,
            Arc::new(registry),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 3,
                history_limits: HistoryLimits::default(),
            },
            PathBuf::from("/tmp"),
        );
        let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
        let mut history = Vec::new();
        let outcome = runtime
            .run_turn(
                &mut history,
                "start".into(),
                sink,
                Arc::new(Allow),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.usage.input_tokens, Some(6));
        assert_eq!(outcome.usage.output_tokens, Some(3));
        assert!(matches!(
            history.get(2),
            Some(Message::Tool {
                content,
                is_error: false,
                ..
            }) if content == "hello"
        ));
    }

    #[tokio::test]
    async fn denial_is_recorded_as_a_model_visible_tool_failure() {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::from([
                AssistantResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"value":"blocked"}),
                    }],
                    usage: Usage::default(),
                },
                AssistantResponse {
                    content: "handled".into(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                },
            ])),
        });
        let mut registry = ToolRegistry::default();
        registry.register(Arc::new(EchoTool)).unwrap();
        let runtime = AgentRuntime::new(
            provider,
            Arc::new(registry),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 3,
                history_limits: HistoryLimits::default(),
            },
            PathBuf::from("/tmp"),
        );
        let mut history = Vec::new();
        runtime
            .run_turn(
                &mut history,
                "start".into(),
                Arc::new(CollectSink(Mutex::new(Vec::new()))),
                Arc::new(Deny),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            history.get(2),
            Some(Message::Tool {
                content,
                is_error: true,
                ..
            }) if content.contains("denied")
        ));
    }

    #[tokio::test]
    async fn cancellation_during_approval_rolls_back_the_active_tool_group() {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::from([AssistantResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-cancel".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"value":"hello"}),
                }],
                usage: Usage::default(),
            }])),
        });
        let mut registry = ToolRegistry::default();
        registry.register(Arc::new(EchoTool)).unwrap();
        let runtime = AgentRuntime::new(
            provider,
            Arc::new(registry),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 2,
                history_limits: HistoryLimits::default(),
            },
            PathBuf::from("/tmp"),
        );
        let before = vec![
            Message::User {
                content: "previous".into(),
            },
            Message::Assistant {
                content: "answer".into(),
                tool_calls: Vec::new(),
            },
        ];
        let mut history = before.clone();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let entered = Arc::new(Notify::new());
        let wait = Arc::clone(&entered);
        let run = runtime.run_turn(
            &mut history,
            "new turn".into(),
            Arc::new(CollectSink(Mutex::new(Vec::new()))),
            Arc::new(WaitForCancellation { entered }),
            cancellation,
        );
        let cancel_when_waiting = async move {
            wait.notified().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(run, cancel_when_waiting);
        assert!(matches!(result, Err(AgentError::Cancelled)));
        assert_eq!(history, before);
    }

    #[tokio::test]
    async fn stops_after_the_configured_maximum_step() {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::from([AssistantResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"value":"one"}),
                }],
                usage: Usage::default(),
            }])),
        });
        let mut registry = ToolRegistry::default();
        registry.register(Arc::new(EchoTool)).unwrap();
        let runtime = AgentRuntime::new(
            provider,
            Arc::new(registry),
            Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
            AgentConfig {
                system_prompt: "test".into(),
                max_steps: 1,
                history_limits: HistoryLimits::default(),
            },
            PathBuf::from("/tmp"),
        );
        let result = runtime
            .run_turn(
                &mut Vec::new(),
                "start".into(),
                Arc::new(CollectSink(Mutex::new(Vec::new()))),
                Arc::new(Allow),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AgentError::StepLimit)));
    }

    #[test]
    fn duplicate_tool_registration_does_not_replace_the_original() {
        let mut registry = ToolRegistry::default();
        registry.register(Arc::new(EchoTool)).unwrap();
        assert!(registry.register(Arc::new(EchoTool)).is_err());
        assert_eq!(registry.tools.len(), 1);
        assert!(registry.get("echo").is_some());
    }
}
