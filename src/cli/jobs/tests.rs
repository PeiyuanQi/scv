//! Unit tests for `src/cli/jobs.rs`.

use scv_protocol::{JobOrigin, WorkbenchJob};

use super::*;

fn job(session: &str, name: &str) -> WorkbenchJob {
    WorkbenchJob {
        session: session.into(),
        job: name.into(),
        agent: "codex".into(),
        task: "task".into(),
        workspace: "/w".into(),
        cwd: "/w".into(),
        state: AgentState::Working,
        detail: None,
        started_by: JobOrigin::Owner,
        conversation: None,
        run: None,
        worktree: None,
        reviewed: false,
        resumable: false,
        resumed_as: None,
        created_unix_seconds: 1,
        updated_unix_seconds: 1,
    }
}

#[test]
fn a_job_is_named_by_its_handle_or_a_session_prefix_and_handle() {
    let snapshot = WorkbenchSnapshot {
        jobs: vec![
            job("3f2a9c1e-aaaa", "job-1"),
            job("77b0d2e4-bbbb", "job-1"),
            job("workbench-0123456789abcdef", "job-2"),
        ],
        ..WorkbenchSnapshot::default()
    };
    let ambiguous = find_job(&snapshot, "job-1").unwrap_err().to_string();
    assert!(ambiguous.contains("3f2a9c1e/job-1"), "{ambiguous}");
    assert_eq!(
        find_job(&snapshot, "77b0/job-1").unwrap().session,
        "77b0d2e4-bbbb"
    );
    assert_eq!(
        find_job(&snapshot, "job-2").unwrap().session,
        "workbench-0123456789abcdef"
    );
    assert!(find_job(&snapshot, "job-9").is_err());
    assert_eq!(short("3f2a9c1e-aaaa"), "3f2a9c1e");
    assert_eq!(short("workbench-0123456789abcdef"), "workbench-012345");
}

#[test]
fn only_reading_commands_run_inside_a_delegation() {
    assert!(!acts(&JobsCommand::List { all: true }));
    assert!(!acts(&JobsCommand::Log {
        target: "job-1".into(),
        follow: false
    }));
    assert!(acts(&JobsCommand::Cancel {
        job: "job-1".into()
    }));
    assert!(acts(&JobsCommand::Approve { id: "ap-1".into() }));
}

#[test]
fn log_lines_are_one_line_each_and_say_what_happened() {
    let event = |kind, text: &str| RunEvent {
        seq: 1,
        ts_ms: 0,
        kind,
        text: text.into(),
        turn: Some(2),
        status: Some("completed".into()),
        job: Some("job-1".into()),
        risk: Some("process".into()),
        by: None,
    };
    assert_eq!(
        describe(&event(RunEventKind::TurnStarted, "go")),
        "── turn 2 · job-1 ── go"
    );
    assert_eq!(describe(&event(RunEventKind::Tool, "$ ls")), "  ● $ ls");
    assert_eq!(
        describe(&event(RunEventKind::ApprovalRequested, "Run make")),
        "? approval (process): Run make"
    );
    assert_eq!(
        describe(&event(RunEventKind::Text, "a\u{1b}[31mb")),
        "a [31mb"
    );
    assert_eq!(
        describe(&event(RunEventKind::TurnEnded, "")),
        "── turn completed ──"
    );
}
