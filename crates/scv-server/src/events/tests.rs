//! Unit tests for `src/events.rs`.

use super::*;

#[test]
fn progress_text_is_bounded_on_a_character_boundary() {
    let text = "é".repeat(400);
    let bounded = bounded_progress(text);
    assert!(bounded.len() <= scv_core::MAX_PROGRESS_EVENT_BYTES);
    assert!(bounded.chars().all(|character| character == 'é'));
    assert_eq!(bounded_progress("short".into()), "short");
}

#[test]
fn every_tool_failure_has_its_wire_kind_and_turn_errors_their_codes() {
    for (failure, kind) in [
        (ToolFailure::Denied, ToolErrorKind::Denied),
        (ToolFailure::Cancelled, ToolErrorKind::Cancelled),
        (
            ToolFailure::InvalidArguments,
            ToolErrorKind::InvalidArguments,
        ),
        (ToolFailure::Unavailable, ToolErrorKind::Unavailable),
        (ToolFailure::Limit, ToolErrorKind::Limit),
        (ToolFailure::Failed, ToolErrorKind::Failed),
        (ToolFailure::UnknownTool, ToolErrorKind::UnknownTool),
    ] {
        assert_eq!(tool_error_kind(failure), kind);
    }
    for (error, code) in [
        (AgentError::Provider("down".into()), "provider_error"),
        (AgentError::ContextLimit("full".into()), "context_limit"),
        (AgentError::StepLimit, "step_limit"),
        (AgentError::HistoryLimit("long".into()), "history_limit"),
        (AgentError::ResponseLimit("big".into()), "response_limit"),
        (AgentError::ToolLimit("many".into()), "tool_limit"),
        (AgentError::Internal("bug".into()), "internal_error"),
    ] {
        assert_eq!(error_code(&error).as_str(), code, "{error}");
    }
}
