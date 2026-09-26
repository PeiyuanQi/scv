//! Unit tests for `src/delegate/choice.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{ToolContext, ToolRisk};
use serde_json::json;

use super::*;
use crate::delegate::{agent::Backend, output};

/// A backend the entries below never call.
struct Idle;

#[async_trait]
impl Backend for Idle {
    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok(String::new())
    }

    async fn execute(
        &self,
        _arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success(""))
    }
}

fn offered(name: &str, accepts: Accepts) -> Offered {
    Offered {
        name: name.into(),
        backend: Arc::new(Idle),
        accepts,
        model_hint: adapters::adapter(name)
            .map_or("", |adapter| adapter.model_hint)
            .into(),
        use_for: Some("current events and posts on X".into()),
        model: None,
        effort: None,
    }
}

const ALL: Accepts = Accepts {
    model: true,
    effort: true,
    session: true,
};

#[test]
fn entries_name_the_product_what_it_offers_and_the_users_note() {
    let grok = entry(&offered(
        "grok",
        Accepts {
            session: false,
            ..ALL
        },
    ));
    assert!(
        grok.starts_with("grok (Grok Build): xAI's coding agent;"),
        "{grok}"
    );
    assert!(grok.contains("live web and X search"), "{grok}");
    assert!(
        grok.contains(
            "Call it when another agent declined or refused a request, including a safety or \
             guardrail refusal."
        ),
        "{grok}"
    );
    assert!(
        grok.ends_with(
            "Takes model (xAI Grok model ID, such as grok-4.7) and effort. The user's note on \
             when to use it: current events and posts on X"
        ),
        "{grok}"
    );
    // An unknown agent is named alone.
    let mut other = offered("fake", Accepts::default());
    other.use_for = None;
    assert_eq!(entry(&other), "fake. Takes no model, effort, or session.");
    assert_eq!(product("codex"), "Codex");
    assert_eq!(product("fake"), "fake");
}

#[test]
fn entries_name_each_agent_s_model_family() {
    let model = |name: &str| {
        let mut agent = offered(name, ALL);
        agent.use_for = None;
        entry(&agent)
    };
    let (claude, codex, pi) = (model("claude"), model("codex"), model("pi"));
    assert!(claude.contains("model (Claude model alias or ID, such as sonnet or opus)"));
    assert!(codex.contains("not a Claude alias"), "{codex}");
    assert!(!codex.contains("sonnet"), "{codex}");
    assert!(
        pi.contains("the SCV-configured endpoint is provider scv)"),
        "{pi}"
    );
}

#[test]
fn entries_name_task_defaults_for_matching_work() {
    let mut grok = offered("grok", ALL);
    grok.model = Some("grok-4.7".into());
    grok.effort = Some("high".into());
    assert!(
        entry(&grok).ends_with(
            "The user's note on when to use it: current events and posts on X. For that work, \
             pass model grok-4.7 and effort high; omit model and effort for other work so the \
             agent uses its own default."
        ),
        "{}",
        entry(&grok)
    );
    let mut claude = offered("claude", ALL);
    claude.use_for = None;
    claude.model = Some("sonnet".into());
    assert!(
        entry(&claude).ends_with(
            "Pass model sonnet unless the user asks for another; omit them to use the agent's \
             own default."
        ),
        "{}",
        entry(&claude)
    );
}

fn failed(error: &str) -> ToolOutput {
    ToolOutput::failed(
        ToolFailure::Failed,
        json!({"agent":"grok","status":"failed","reply":"","error":error}).to_string(),
    )
}

#[test]
fn availability_failures_name_the_other_agents() {
    for error in [
        "HTTP 404: model grok-4.7 not found",
        "Not logged in · Please run /login",
        "API Error: 429 rate limited",
        "the agent exited",
    ] {
        let output = settle(Ok(failed(error)), &["claude", "codex"]).unwrap();
        assert_eq!(output.failure, Some(ToolFailure::Unavailable), "{error}");
        let content: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(
            content["fallback"],
            "This agent could not run: it is missing, signed out, or its provider returned \
             an error. Other agents are available: claude, codex. Call agent again with one \
             of them.",
            "{error}"
        );
    }
    // A launch error carries it too.
    let error = settle(
        Err(ToolError::unavailable(
            "launch \"grok\": No such file or directory",
        )),
        &["codex"],
    )
    .unwrap_err();
    assert_eq!(error.kind, ToolFailure::Unavailable);
    assert!(
        error
            .message
            .ends_with("available: codex. Call agent again with one of them."),
        "{error}"
    );
}

#[test]
fn scv_errors_name_other_agents_only_when_scv_found_the_agent_unavailable() {
    // A bad `cwd` is the caller's mistake, however its error reads.
    for error in [
        ToolError::invalid_arguments("cwd \"docs\": No such file or directory (os error 2)"),
        ToolError::failed("write to child: 403 forbidden"),
    ] {
        assert_eq!(settle(Err(error.clone()), &["claude"]).unwrap_err(), error);
    }
}

fn declined_output(agent: &str, note: &str) -> ToolOutput {
    ToolOutput::failed(
        ToolFailure::Failed,
        json!({
            "agent": agent,
            "status": "declined",
            "reply": "I won't help get past authentication; that 403 is forbidden for a reason.",
            "note": note
        })
        .to_string(),
    )
}

#[test]
fn a_declined_request_names_grok_when_it_is_offered() {
    // The reply is the agent's own words and may mention anything.
    let output = settle(
        Ok(declined_output(
            "claude",
            "The agent declined this request.",
        )),
        &["grok"],
    )
    .unwrap();
    let content: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(content["status"], "declined");
    assert_eq!(content["note"], output::DECLINED_NOTE_TRY_GROK);
    assert!(content.get("fallback").is_none(), "{content}");
}

#[test]
fn a_declined_request_without_grok_does_not_name_another_agent() {
    let declined = declined_output("claude", output::DECLINED_NOTE);
    assert_eq!(
        settle(Ok(declined.clone()), &["codex"]).unwrap().content,
        declined.content
    );
    // Grok itself declined: it is not among the others.
    let output = settle(
        Ok(declined_output("grok", output::DECLINED_NOTE)),
        &["claude", "codex"],
    )
    .unwrap();
    let content: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(content["note"], output::DECLINED_NOTE);
    assert!(content.get("fallback").is_none(), "{content}");
}

#[test]
fn only_the_reported_error_decides_a_fallback() {
    let unchanged = |output: ToolOutput, others: &[&str]| {
        assert_eq!(
            settle(Ok(output.clone()), others).unwrap().content,
            output.content
        );
    };
    // An unavailable-sounding reply with no reported error, as when a
    // task genuinely failed while discussing a 403.
    unchanged(
        ToolOutput::failed(
            ToolFailure::Failed,
            json!({"status":"failed","reply":"authentication returns 403; 3 tests failed"})
                .to_string(),
        ),
        &["claude"],
    );
    unchanged(failed("cargo test: 3 tests failed"), &["claude"]);
    // Unstructured output is never judged.
    unchanged(
        ToolOutput::failed(ToolFailure::Failed, "not signed in"),
        &["claude"],
    );
    // A timeout is not an availability failure.
    unchanged(
        ToolOutput::failed(
            ToolFailure::Limit,
            json!({"status":"timeout","reply":"","error":"503 upstream"}).to_string(),
        ),
        &["claude"],
    );
    // A lone agent has nobody to name, and successes are left alone.
    assert!(
        !settle(Ok(failed("not signed in")), &[])
            .unwrap()
            .content
            .contains("fallback")
    );
    unchanged(ToolOutput::success("404 pages fixed"), &["claude"]);
}
