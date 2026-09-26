//! The [`Tool`] trait, its inputs and outputs, and the [`ToolRegistry`].

use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{AgentError, ApprovalGate, ApprovalRequest, ProgressSink};

/// What the model is told about a tool: its name, what it does, and the JSON
/// Schema of its arguments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// How much a tool call can affect; the approval policy decides per risk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    ReadOnly,
    Filesystem,
    Process,
    Delegate,
    /// Sends a request to a host outside the auto-approved set, whose URL can
    /// carry data the model has read.
    Network,
}

impl ToolRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Filesystem => "filesystem",
            Self::Process => "process",
            Self::Delegate => "delegate",
            Self::Network => "network",
        }
    }

    /// The risk named by [`ToolRisk::as_str`], such as one a nested SCV
    /// reported for its own tool call.
    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::ReadOnly,
            Self::Filesystem,
            Self::Process,
            Self::Delegate,
            Self::Network,
        ]
        .into_iter()
        .find(|risk| risk.as_str() == value)
    }
}

/// What a running tool call gets besides its arguments.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// The model's ID for this call, which its `ToolCompleted` event carries;
    /// empty for a call the model did not make, such as a background job's.
    pub call_id: String,
    pub workspace: PathBuf,
    pub cancellation: CancellationToken,
    /// Where the tool may report short status lines while it runs.
    pub progress: ProgressSink,
    /// The session's approval gate, for a tool relaying a nested agent's own
    /// approval requests.
    pub approvals: ToolApprovals,
}

impl ToolContext {
    /// A context whose progress reports go nowhere and whose relayed
    /// approval requests are denied, for a call without an ID.
    pub fn new(workspace: PathBuf, cancellation: CancellationToken) -> Self {
        Self {
            call_id: String::new(),
            workspace,
            cancellation,
            progress: ProgressSink::default(),
            approvals: ToolApprovals::default(),
        }
    }
}

/// The session's approval gate as seen by one running tool call. A tool that
/// drives a nested agent (such as another SCV) asks it on the nested agent's
/// behalf, so the session's policy and its user decide every nested side
/// effect too. Without a gate, every request is denied.
#[derive(Clone, Default)]
pub struct ToolApprovals {
    gate: Option<Arc<dyn ApprovalGate>>,
    call_id: String,
}

impl fmt::Debug for ToolApprovals {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolApprovals")
            .field("enabled", &self.gate.is_some())
            .field("call_id", &self.call_id)
            .finish()
    }
}

impl ToolApprovals {
    /// Requests for the tool call `call_id`, decided by `gate`.
    pub fn new(gate: Arc<dyn ApprovalGate>, call_id: impl Into<String>) -> Self {
        Self {
            gate: Some(gate),
            call_id: call_id.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.gate.is_some()
    }

    /// Ask the session's gate to approve a nested agent's tool call. The
    /// request carries this tool call's ID; `name`, `risk`, and `summary`
    /// describe the nested call.
    pub async fn request(
        &self,
        name: impl Into<String>,
        risk: ToolRisk,
        cwd: PathBuf,
        summary: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        let Some(gate) = &self.gate else {
            return Ok(false);
        };
        gate.approve(
            ApprovalRequest {
                call_id: self.call_id.clone(),
                name: name.into(),
                risk,
                cwd,
                summary: summary.into(),
            },
            cancellation,
        )
        .await
    }
}

/// Why a tool call failed. The model reads the call's output either way;
/// this tells the session's clients what happened without reading the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolFailure {
    /// The approval policy or the user refused the call.
    Denied,
    /// The call was cancelled while it ran.
    Cancelled,
    /// The call's arguments were refused, so nothing ran.
    InvalidArguments,
    /// The tool, or the agent it runs, could not be used: missing, signed
    /// out, or its provider unreachable. Another agent might succeed.
    Unavailable,
    /// A configured size, count, depth, or time limit stopped the call.
    Limit,
    /// The call ran and failed.
    Failed,
    /// The model named a tool the session does not have.
    UnknownTool,
}

/// A tool's result as the model sees it. A failure is still a result: the
/// model reads it and can react.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    /// Why the call failed; `None` when it succeeded.
    pub failure: Option<ToolFailure>,
    pub truncated: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            failure: None,
            truncated: false,
        }
    }

    /// A failed result whose `content` tells the model why.
    pub fn failed(failure: ToolFailure, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            failure: Some(failure),
            truncated: false,
        }
    }

    pub fn is_error(&self) -> bool {
        self.failure.is_some()
    }
}

impl From<ToolError> for ToolOutput {
    /// The failed result the model sees for a call that could not run.
    fn from(error: ToolError) -> Self {
        Self::failed(error.kind, error.message)
    }
}

/// A tool call that could not run, such as invalid arguments. The runtime
/// turns it into a failed [`ToolOutput`] whose content is `message`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ToolError {
    pub kind: ToolFailure,
    pub message: String,
}

impl ToolError {
    pub(crate) fn new(kind: ToolFailure, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The arguments were refused; the model can correct them.
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(ToolFailure::InvalidArguments, message)
    }

    /// The tool or its agent could not be used.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ToolFailure::Unavailable, message)
    }

    /// A configured limit stopped the call.
    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(ToolFailure::Limit, message)
    }

    /// The call was cancelled.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(ToolFailure::Cancelled, message)
    }

    /// The call ran and failed.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ToolFailure::Failed, message)
    }
}

impl From<String> for ToolError {
    /// A [`ToolFailure::Failed`] error.
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

/// Something the model can call. The runtime asks for the call's [`risk`] and
/// [`approval_summary`] first, so both must validate the arguments without
/// side effects; only an approved call reaches [`execute`].
///
/// [`risk`]: Tool::risk
/// [`approval_summary`]: Tool::approval_summary
/// [`execute`]: Tool::execute
///
/// # Example
///
/// ```
/// use async_trait::async_trait;
/// use serde_json::{Value, json};
/// use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
///
/// struct Echo;
///
/// #[async_trait]
/// impl Tool for Echo {
///     fn spec(&self) -> ToolSpec {
///         ToolSpec {
///             name: "echo".into(),
///             description: "Repeat a value".into(),
///             parameters: json!({
///                 "type": "object",
///                 "properties": {"value": {"type": "string"}},
///                 "required": ["value"]
///             }),
///         }
///     }
///
///     fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
///         arguments["value"]
///             .as_str()
///             .ok_or_else(|| ToolError::invalid_arguments("value must be a string"))?;
///         Ok(ToolRisk::ReadOnly)
///     }
///
///     fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
///         self.risk(arguments)?;
///         Ok("Repeat a value".into())
///     }
///
///     async fn execute(
///         &self,
///         arguments: Value,
///         _context: ToolContext,
///     ) -> Result<ToolOutput, ToolError> {
///         Ok(ToolOutput::success(arguments["value"].as_str().unwrap_or_default()))
///     }
/// }
///
/// let mut registry = scv_core::ToolRegistry::default();
/// registry.register(std::sync::Arc::new(Echo)).unwrap();
/// assert_eq!(registry.specs()[0].name, "echo");
/// ```
#[async_trait]
pub trait Tool: Send + Sync {
    /// The name, description, and argument schema shown to the model.
    fn spec(&self) -> ToolSpec;
    /// The risk of this call, which selects the approval rule.
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError>;
    /// One line describing this call for a person approving it.
    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError>;
    /// Run an approved call. Honour `context.cancellation`.
    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

/// The tools of one session, by unique name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Add `tool`; a second tool with the same name is refused.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.spec().name;
        if self.tools.contains_key(&name) {
            return Err(ToolError::failed(format!("duplicate tool name: {name}")));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Every tool's spec, sorted by name so requests are stable.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self.tools.values().map(|tool| tool.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

#[cfg(test)]
mod tests;
