//! Unit tests for `src/delegate/choice.rs`.

use super::*;
use crate::delegate::output;
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
        model: None,
        effort: None,
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
        description.contains(
            "Call it when another agent declined or refused a request, including a safety \
                 or guardrail refusal"
        ),
        "{description}"
    );
    assert!(
        description.ends_with("The user's note on when to use it: current events and posts on X"),
        "{description}"
    );
    // An unknown tool keeps its own description.
    let other = chosen("agent_fake", Ok(ToolOutput::success("")), &[]);
    assert!(other.spec().description.starts_with("Runs as"));
    assert_eq!(product("agent_codex"), "Codex");
    assert_eq!(product("agent_fake"), "agent_fake");
}

#[test]
fn descriptions_name_task_defaults_for_matching_work() {
    let mut grok = chosen("agent_grok", Ok(ToolOutput::success("")), &[]);
    grok.model = Some("grok-4.7".into());
    grok.effort = Some("high".into());
    let description = grok.spec().description;
    assert!(
        description.contains(
            "The user's note on when to use it: current events and posts on X. For that work, \
             pass model grok-4.7 and effort high; omit model and effort for other work so the \
             agent uses its own default."
        ),
        "{description}"
    );
    let mut claude = chosen("agent_claude", Ok(ToolOutput::success("")), &[]);
    claude.use_for = None;
    claude.model = Some("sonnet".into());
    let description = claude.spec().description;
    assert!(
        description.contains(
            "Pass model sonnet unless the user asks for another; omit them to use the agent's \
             own default."
        ),
        "{description}"
    );
}

fn failed(error: &str) -> ToolOutput {
    ToolOutput::failure(
        json!({"agent":"grok","status":"failed","reply":"","error":error}).to_string(),
    )
}

#[tokio::test]
async fn availability_failures_name_the_other_agents() {
    for error in [
        "HTTP 404: model grok-4.7 not found",
        "Not logged in · Please run /login",
        "API Error: 429 rate limited",
        "the agent exited",
    ] {
        let agent = chosen(
            "agent_grok",
            Ok(failed(error)),
            &["agent_claude", "agent_codex"],
        );
        let output = run(&agent).await.unwrap();
        let content: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(
            content["fallback"],
            "This agent could not run: it is missing, signed out, or its provider returned \
                 an error. Other agents are available: agent_claude, agent_codex.",
            "{error}"
        );
    }
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

fn declined_output(agent: &str, note: &str) -> ToolOutput {
    ToolOutput::failure(
        json!({
            "agent": agent,
            "status": "declined",
            "reply": "I won't help get past authentication; that 403 is forbidden for a reason.",
            "note": note
        })
        .to_string(),
    )
}

#[tokio::test]
async fn a_declined_request_names_grok_when_it_is_offered() {
    // The reply is the agent's own words and may mention anything.
    let agent = chosen(
        "agent_claude",
        Ok(declined_output(
            "claude",
            "The agent declined this request.",
        )),
        &["agent_grok"],
    );
    let content: Value = serde_json::from_str(&run(&agent).await.unwrap().content).unwrap();
    assert_eq!(content["status"], "declined");
    assert_eq!(content["note"], output::DECLINED_NOTE_TRY_GROK);
    assert!(content.get("fallback").is_none(), "{content}");
}

#[tokio::test]
async fn a_declined_request_without_grok_does_not_name_another_agent() {
    let declined = declined_output("claude", output::DECLINED_NOTE);
    let agent = chosen("agent_claude", Ok(declined.clone()), &["agent_codex"]);
    assert_eq!(run(&agent).await.unwrap().content, declined.content);
    let grok = chosen(
        "agent_grok",
        Ok(declined_output("grok", output::DECLINED_NOTE)),
        &["agent_claude", "agent_codex"],
    );
    let content: Value = serde_json::from_str(&run(&grok).await.unwrap().content).unwrap();
    assert_eq!(content["note"], output::DECLINED_NOTE);
    assert!(content.get("fallback").is_none(), "{content}");
}

#[tokio::test]
async fn only_the_reported_error_decides_a_fallback() {
    // An unavailable-sounding reply with no reported error, as when a
    // task genuinely failed while discussing a 403.
    let reply_only = ToolOutput::failure(
        json!({"status":"failed","reply":"authentication returns 403; 3 tests failed"}).to_string(),
    );
    let agent = chosen("agent_codex", Ok(reply_only.clone()), &["agent_claude"]);
    assert_eq!(run(&agent).await.unwrap().content, reply_only.content);
    let tests_failed = failed("cargo test: 3 tests failed");
    let agent = chosen("agent_codex", Ok(tests_failed.clone()), &["agent_claude"]);
    assert_eq!(run(&agent).await.unwrap().content, tests_failed.content);
    // Unstructured output is never judged.
    let plain = ToolOutput::failure("not signed in");
    let agent = chosen("agent_codex", Ok(plain.clone()), &["agent_claude"]);
    assert_eq!(run(&agent).await.unwrap().content, plain.content);
    // A timeout is not an availability failure.
    let timeout = ToolOutput::failure(
        json!({"status":"timeout","reply":"","error":"503 upstream"}).to_string(),
    );
    let agent = chosen("agent_codex", Ok(timeout.clone()), &["agent_claude"]);
    assert_eq!(run(&agent).await.unwrap().content, timeout.content);
    // A lone agent has nobody to name, and successes are left alone.
    let alone = chosen("agent_codex", Ok(failed("not signed in")), &[]);
    assert!(!run(&alone).await.unwrap().content.contains("fallback"));
    let fine = ToolOutput::success("404 pages fixed");
    let agent = chosen("agent_codex", Ok(fine.clone()), &["agent_claude"]);
    assert_eq!(run(&agent).await.unwrap().content, fine.content);
}
