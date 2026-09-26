//! What lets the model choose between delegated agents: each `agent_*` tool
//! names its product and what that harness offers, carries the user's own
//! `use_for` note, and, when a call fails because the agent is missing,
//! signed out, or its provider returned an error, names the other agents to
//! fall back on.
//!
//! That fallback is decided from the structured failure alone: the result's
//! `status` and the `error` the agent reported, read against a list of
//! phrases, or SCV's own error when SCV classed it as
//! [`ToolFailure::Unavailable`]; never from the agent's reply or the wording
//! of SCV's other errors. A `declined` result, where the agent's model
//! refused the request, never gets one. When `agent_grok` is among the other
//! agents, the result's `note` tells the calling model to call it; otherwise
//! the calling model tells the user, and only the user may then name another
//! agent.

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolFailure, ToolOutput, ToolRisk, ToolSpec};
use scv_protocol::JobStatus;
use serde_json::Value;

use crate::delegate::{
    adapters,
    output::{self, AgentReply},
};

/// Lowercase fragments of an error an agent reported that another agent
/// could avoid: the agent is missing or died, is signed out, or its provider
/// returned an error.
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

/// Whether an error an agent reported reads like it was unavailable.
pub(crate) fn reports_unavailable(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    UNAVAILABLE.iter().any(|needle| lower.contains(needle))
}

/// An `agent_*` tool as the model sees it.
pub(crate) struct ChosenAgent {
    pub(crate) inner: Arc<dyn Tool>,
    /// The user's `[agents.<name>] use_for` note.
    pub(crate) use_for: Option<String>,
    /// `[agents.<name>] model`, passed when the work matches `use_for`.
    pub(crate) model: Option<String>,
    /// `[agents.<name>] effort`, passed the same way as `model`.
    pub(crate) effort: Option<String>,
    /// The other agent tools offered in this session.
    pub(crate) alternatives: Vec<String>,
}

/// `model opus-5.5 and effort xhigh`, when either default is set.
pub fn model_effort_phrase(model: Option<&str>, effort: Option<&str>) -> Option<String> {
    match (model, effort) {
        (Some(model), Some(effort)) => Some(format!("model {model} and effort {effort}")),
        (Some(model), None) => Some(format!("model {model}")),
        (None, Some(effort)) => Some(format!("effort {effort}")),
        (None, None) => None,
    }
}

/// Tool-description suffix for `use_for` and configured model/effort defaults.
fn choice_note(use_for: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Option<String> {
    let defaults = model_effort_phrase(model, effort);
    match (use_for, defaults) {
        (Some(use_for), Some(defaults)) => Some(format!(
            " The user's note on when to use it: {use_for}. For that work, pass {defaults}; \
             omit model and effort for other work so the agent uses its own default."
        )),
        (Some(use_for), None) => Some(format!(" The user's note on when to use it: {use_for}")),
        (None, Some(defaults)) => Some(format!(
            " Pass {defaults} unless the user asks for another; omit them to use the agent's \
             own default."
        )),
        (None, None) => None,
    }
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

    fn offers_grok(&self) -> bool {
        self.alternatives.iter().any(|name| name == "agent_grok")
    }
}

/// Whether a failed agent result shows the agent was unavailable, judged by
/// its `status` and structured `error` only. A declined request, or a result
/// without a reported error, never qualifies.
fn unavailable_result(reply: &AgentReply) -> bool {
    reply.status == Some(JobStatus::Failed)
        && reply.error.as_deref().is_some_and(reports_unavailable)
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
        if let Some(note) = choice_note(
            self.use_for.as_deref(),
            self.model.as_deref(),
            self.effort.as_deref(),
        ) {
            spec.description.push_str(&note);
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
            Ok(mut result) if result.is_error() => {
                if let Ok(value) = serde_json::from_str::<Value>(&result.content)
                    && let Some(reply) = AgentReply::read(&value)
                    && let Value::Object(mut content) = value
                {
                    if unavailable_result(&reply) {
                        result.failure = Some(ToolFailure::Unavailable);
                        if let Some(fallback) = self.fallback() {
                            content.insert("fallback".into(), fallback.into());
                            result.content = Value::Object(content).to_string();
                        }
                    } else if reply.status == Some(JobStatus::Declined) && self.offers_grok() {
                        content.insert("note".into(), output::DECLINED_NOTE_TRY_GROK.into());
                        result.content = Value::Object(content).to_string();
                    }
                }
                Ok(result)
            }
            // SCV's own errors that it classed as unavailable, such as a
            // missing executable.
            Err(error) if error.kind == ToolFailure::Unavailable => Err(match self.fallback() {
                Some(fallback) => ToolError::unavailable(format!("{} {fallback}", error.message)),
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
