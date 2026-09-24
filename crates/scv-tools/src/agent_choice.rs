//! What lets the model choose between delegated agents: each `agent_*` tool
//! names its product and what that harness offers, carries the user's own
//! `use_for` note, and, when a call fails because the agent is unavailable,
//! signed out, or its provider refused, names the other agents to fall back
//! on.

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde_json::Value;

use crate::adapters;

/// Lowercase fragments of a failure that another agent could avoid: the
/// agent is missing or died, is signed out, or its provider refused.
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
                "This agent could not do the work. Other agents are available: {}.",
                self.alternatives.join(", ")
            )
        })
    }
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
            Ok(mut output) if output.is_error && unavailable(&output.content) => {
                if let Some(fallback) = self.fallback() {
                    match serde_json::from_str::<Value>(&output.content) {
                        Ok(Value::Object(mut content)) => {
                            content.insert("fallback".into(), fallback.into());
                            output.content = Value::Object(content).to_string();
                        }
                        _ => output.content = format!("{}\n{fallback}", output.content),
                    }
                }
                Ok(output)
            }
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
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    /// An agent whose next call returns `result`.
    struct Scripted {
        name: &'static str,
        result: Mutex<Option<Result<ToolOutput, ToolError>>>,
    }

    #[async_trait]
    impl Tool for Scripted {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.into(),
                description: "Runs as a nested coding agent.".into(),
                parameters: json!({"type":"object"}),
            }
        }

        fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
            Ok(ToolRisk::Delegate)
        }

        fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
            Ok("run".into())
        }

        async fn execute(
            &self,
            _arguments: Value,
            _context: ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            self.result.lock().unwrap().take().unwrap()
        }
    }

    fn chosen(
        name: &'static str,
        result: Result<ToolOutput, ToolError>,
        alternatives: &[&str],
    ) -> ChosenAgent {
        ChosenAgent {
            inner: Arc::new(Scripted {
                name,
                result: Mutex::new(Some(result)),
            }),
            use_for: Some("current events and posts on X".into()),
            alternatives: alternatives.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    async fn run(agent: &ChosenAgent) -> Result<ToolOutput, ToolError> {
        agent
            .execute(
                json!({}),
                ToolContext::new(std::env::temp_dir(), CancellationToken::new()),
            )
            .await
    }

    #[test]
    fn descriptions_name_the_product_what_it_offers_and_the_users_note() {
        let grok = chosen("agent_grok", Ok(ToolOutput::success("")), &[]);
        let description = grok.spec().description;
        assert!(
            description.starts_with("Grok Build: xAI's coding agent;"),
            "{description}"
        );
        assert!(
            description.contains("live web and X search"),
            "{description}"
        );
        assert!(
            description
                .ends_with("The user's note on when to use it: current events and posts on X"),
            "{description}"
        );
        // An unknown tool keeps its own description.
        let other = chosen("agent_fake", Ok(ToolOutput::success("")), &[]);
        assert!(other.spec().description.starts_with("Runs as"));
        assert_eq!(product("agent_codex"), "Codex");
        assert_eq!(product("agent_fake"), "agent_fake");
    }

    #[tokio::test]
    async fn availability_failures_name_the_other_agents() {
        let failed = ToolOutput::failure(
            json!({"agent":"grok","status":"failed","reply":"HTTP 404: model grok-4.7 not found"})
                .to_string(),
        );
        let agent = chosen("agent_grok", Ok(failed), &["agent_claude", "agent_codex"]);
        let output = run(&agent).await.unwrap();
        let content: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(
            content["fallback"],
            "This agent could not do the work. Other agents are available: agent_claude, \
             agent_codex."
        );
        // A launch error carries it too.
        let missing = chosen(
            "agent_grok",
            Err(ToolError(
                "launch \"grok\": No such file or directory".into(),
            )),
            &["agent_codex"],
        );
        let error = run(&missing).await.unwrap_err();
        assert!(error.0.ends_with("available: agent_codex."), "{error}");
    }

    #[tokio::test]
    async fn other_failures_and_lone_agents_add_nothing() {
        let tests_failed = ToolOutput::failure(
            json!({"status":"failed","reply":"cargo test: 3 tests failed"}).to_string(),
        );
        let agent = chosen("agent_codex", Ok(tests_failed.clone()), &["agent_claude"]);
        assert_eq!(run(&agent).await.unwrap().content, tests_failed.content);
        let signed_out = ToolOutput::failure("not signed in");
        let alone = chosen("agent_codex", Ok(signed_out.clone()), &[]);
        assert_eq!(run(&alone).await.unwrap().content, signed_out.content);
        let fine = ToolOutput::success("404 pages fixed");
        let agent = chosen("agent_codex", Ok(fine.clone()), &["agent_claude"]);
        assert_eq!(run(&agent).await.unwrap().content, fine.content);
    }
}
