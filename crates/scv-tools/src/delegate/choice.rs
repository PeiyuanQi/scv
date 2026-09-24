//! What lets the model choose between delegated agents: each `agent_*` tool
//! names its product and what that harness offers, carries the user's own
//! `use_for` note, and, when a call fails because the agent is missing,
//! signed out, or its provider returned an error, names the other agents to
//! fall back on.
//!
//! That fallback is decided from the structured failure alone (the result's
//! `status` and `error`, or SCV's own error message), never from the agent's
//! reply. A `declined` result, where the agent's model refused the request,
//! never gets one: the calling model tells the user instead, and only the
//! user may then name another agent.

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde_json::Value;

use crate::delegate::adapters;

/// Lowercase fragments of a reported failure that another agent could avoid:
/// the agent is missing or died, is signed out, or its provider returned an
/// error.
const UNAVAILABLE: &[&str] = &[
    "not found on path",
    "no such file or directory",
    "the agent exited",
    "acp server exited",
    "not logged in",
    "not signed in",
    "not authenticated",
    "unauthorized",
    "authentication",
    "missing_credential",
    "no api key",
    "auth_required",
    "forbidden",
    "401",
    "403",
    "404",
    "429",
    "502",
    "503",
    "529",
    "rate limit",
    "rate_limit",
    "quota",
    "overloaded",
    "service unavailable",
    "model not found",
    "no such model",
    "does not exist",
    "insufficient",
    "billing",
    "connection refused",
    "connection reset",
];

fn unavailable(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    UNAVAILABLE.iter().any(|needle| lower.contains(needle))
}

/// An `agent_*` tool as the model sees it.
pub(crate) struct ChosenAgent {
    pub(crate) inner: Arc<dyn Tool>,
    /// The user's `[agents.<name>] use_for` note.
    pub(crate) use_for: Option<String>,
    /// The other agent tools offered in this session.
    pub(crate) alternatives: Vec<String>,
}

impl ChosenAgent {
    fn fallback(&self) -> Option<String> {
        (!self.alternatives.is_empty()).then(|| {
            format!(
                "This agent could not run: it is missing, signed out, or its provider \
                 returned an error. Other agents are available: {}.",
                self.alternatives.join(", ")
            )
        })
    }
}

/// Whether a failed agent result shows the agent was unavailable, judged by
/// its `status` and structured `error` only. A declined request, or a result
/// without a reported error, never qualifies.
fn unavailable_result(content: &serde_json::Map<String, Value>) -> bool {
    content.get("status").and_then(Value::as_str) == Some("failed")
        && content
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(unavailable)
}

/// `agent_codex` → the Codex descriptor, when it is a known adapter.
fn descriptor(tool: &str) -> Option<&'static adapters::AdapterDescriptor> {
    adapters::adapter(tool.strip_prefix("agent_")?)
}

#[async_trait]
impl Tool for ChosenAgent {
    fn spec(&self) -> ToolSpec {
        let mut spec = self.inner.spec();
        if let Some(descriptor) = descriptor(&spec.name) {
            spec.description = format!(
                "{}: {}. {}",
                descriptor.product, descriptor.offers, spec.description
            );
        }
        if let Some(use_for) = &self.use_for {
            spec.description
                .push_str(&format!(" The user's note on when to use it: {use_for}"));
        }
        spec
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        self.inner.risk(arguments)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        self.inner.approval_summary(arguments)
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        match self.inner.execute(arguments, context).await {
            Ok(mut output) if output.is_error => {
                if let Ok(Value::Object(mut content)) =
                    serde_json::from_str::<Value>(&output.content)
                    && unavailable_result(&content)
                    && let Some(fallback) = self.fallback()
                {
                    content.insert("fallback".into(), fallback.into());
                    output.content = Value::Object(content).to_string();
                }
                Ok(output)
            }
            // SCV's own messages, such as a missing executable.
            Err(error) if unavailable(&error.0) => Err(match self.fallback() {
                Some(fallback) => ToolError(format!("{} {fallback}", error.0)),
                None => error,
            }),
            other => other,
        }
    }
}

/// The product name of an `agent_*` tool, such as `Codex`, or the tool name.
pub fn product(tool: &str) -> &str {
    descriptor(tool).map_or(tool, |descriptor| descriptor.product)
}

#[cfg(test)]
mod tests;
