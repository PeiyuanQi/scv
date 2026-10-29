//! Unit tests for `src/cli/jobs.rs`.

use scv_protocol::{AttachOffer, AttachState, JobOrigin, WorkbenchJob};

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
        attach: None,
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
    assert!(!acts(&JobsCommand::List {
        all: true,
        json: true
    }));
    assert!(!acts(&JobsCommand::Log {
        target: "job-1".into(),
        follow: false,
        json: true
    }));
    assert!(acts(&JobsCommand::Cancel {
        job: "job-1".into(),
        json: false
    }));
    assert!(acts(&JobsCommand::Approve { id: "ap-1".into() }));
    // Waiting on, reading, and following the owner's jobs is automation
    // for the owner, refused inside a delegated run like acting.
    assert!(acts(&JobsCommand::Wait {
        job: "job-1".into(),
        timeout: Some(1),
        json: true
    }));
    assert!(acts(&JobsCommand::Result {
        job: "job-1".into(),
        json: true
    }));
    assert!(acts(&JobsCommand::Events {
        follow: true,
        json: true
    }));
}

#[test]
fn a_wait_exits_0_for_completed_1_for_any_other_end_and_goes_on_otherwise() {
    assert_eq!(wait_exit(AgentState::Completed), Some(0));
    for state in [
        AgentState::Failed,
        AgentState::Declined,
        AgentState::Timeout,
        AgentState::Cancelled,
        AgentState::Interrupted,
    ] {
        assert_eq!(wait_exit(state), Some(WAIT_ENDED_OTHERWISE), "{state}");
    }
    for state in [
        AgentState::Starting,
        AgentState::Working,
        AgentState::AwaitingInput,
        AgentState::Idle,
        AgentState::Queued,
        AgentState::Unknown,
    ] {
        assert_eq!(wait_exit(state), None, "{state}");
    }
    assert_eq!(
        (WAIT_ENDED_OTHERWISE, WAIT_TIMED_OUT, WAIT_REFUSED),
        (1, 2, 3)
    );
}

#[test]
fn json_output_escapes_every_control_character_and_reads_back_unchanged() {
    let reply = "done\u{1b}[2J\u{9b}31m\u{7f}\r\nline two\u{7}";
    let result = JobResult {
        session: "s".into(),
        job: "job-1".into(),
        agent: "codex".into(),
        state: AgentState::Completed,
        detail: None,
        reply: Some(reply.into()),
        error: None,
        truncated: false,
        outcome: None,
        finished_unix_seconds: Some(2),
    };
    let line = json_line(&result).unwrap();
    assert!(!line.chars().any(char::is_control), "{line:?}");
    assert!(
        line.contains(r"\u001b[2J\u009b31m\u007f\r\nline two\u0007"),
        "{line}"
    );
    let back: JobResult = serde_json::from_str(&line).unwrap();
    assert_eq!(back, result);
}

#[test]
fn a_result_from_the_record_is_cut_as_job_result_cuts_it() {
    let whole = JobResult {
        session: "s".into(),
        job: "job-1".into(),
        agent: "codex".into(),
        state: AgentState::Failed,
        detail: None,
        reply: Some("short".into()),
        error: Some("short".into()),
        truncated: false,
        outcome: None,
        finished_unix_seconds: None,
    };
    assert_eq!(bounded(whole.clone()), whole);
    // A multi-byte character straddles the bound, so the cut backs off.
    let long = format!(
        "{}é{}",
        "a".repeat(MAX_JOB_RESULT_BYTES - 1),
        "b".repeat(900)
    );
    let cut = bounded(JobResult {
        error: Some(long.clone()),
        ..whole.clone()
    });
    assert_eq!(
        (cut.reply.as_deref(), cut.error.as_deref(), cut.truncated),
        (Some("short"), Some(&long[..MAX_JOB_RESULT_BYTES - 1]), true)
    );
}

#[test]
fn a_printed_reply_keeps_its_lines_and_loses_every_other_control_character() {
    assert_eq!(
        inert_text("ok\u{1b}[31m red\r\n\u{9b}2Jnext\tline\n"),
        "ok [31m red \n 2Jnext line\n"
    );
}

#[test]
fn listing_keeps_unfinished_and_resumable_jobs_and_live_runs_unless_all() {
    let finished = WorkbenchJob {
        state: AgentState::Completed,
        ..job("s", "job-1")
    };
    let interrupted = WorkbenchJob {
        state: AgentState::Interrupted,
        resumable: true,
        ..job("s", "job-2")
    };
    let snapshot = WorkbenchSnapshot {
        jobs: vec![finished, interrupted, job("s", "job-3")],
        ..WorkbenchSnapshot::default()
    };
    let names = |snapshot: WorkbenchSnapshot| -> Vec<String> {
        snapshot.jobs.into_iter().map(|job| job.job).collect()
    };
    assert_eq!(names(listed(snapshot.clone(), false)), ["job-2", "job-3"]);
    assert_eq!(names(listed(snapshot, true)), ["job-1", "job-2", "job-3"]);
}

#[test]
fn a_json_log_line_is_its_event_with_the_run_it_belongs_to() {
    let event = RunEvent {
        seq: 4,
        ts_ms: 9,
        kind: RunEventKind::Text,
        text: "landed\u{1b}]0;x".into(),
        turn: Some(1),
        status: None,
        job: None,
        risk: None,
        by: None,
    };
    assert_eq!(
        json_line(&Logged {
            run: "codex-3f9a2c",
            event: &event
        })
        .unwrap(),
        r#"{"run":"codex-3f9a2c","seq":4,"ts_ms":9,"kind":"text","text":"landed\u001b]0;x","turn":1}"#
    );
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
    let relayed = RunEvent {
        risk: Some("x\u{9b}2Jy".into()),
        ..event(RunEventKind::ApprovalRequested, "Run make")
    };
    assert_eq!(describe(&relayed), "? approval (x 2Jy): Run make");
}

#[test]
fn a_jobs_line_says_whether_its_conversation_can_be_taken_over_and_how() {
    let mut job = job("3f2a9c1e-aaaa", "job-3");
    job.conversation = Some("codex-2".into());
    let offer = |state, interrupt, detail: Option<&str>| AttachOffer {
        state,
        detail: detail.map(str::to_owned),
        interrupt,
        job: interrupt.then(|| "job-3".to_owned()),
    };
    assert_eq!(
        attach_line(&job, &offer(AttachState::Ready, false, None)),
        "attach: codex-2 is idle; `scv jobs attach 3f2a9c1e/job-3` takes it over"
    );
    assert_eq!(
        attach_line(&job, &offer(AttachState::TakenOver, false, None)),
        "attach: codex-2 is taken over; `scv jobs attach 3f2a9c1e/job-3` attaches, `--end` ends it"
    );
    assert_eq!(
        attach_line(
            &job,
            &offer(AttachState::Busy, true, Some("codex-2 has a turn"))
        ),
        "attach: not while job-3 runs its turn; `scv jobs attach 3f2a9c1e/job-3 --interrupt` \
         stops it and takes codex-2 over"
    );
    // Why not, in SCV's words, with nothing an agent wrote left to act on.
    assert_eq!(
        attach_line(
            &job,
            &offer(
                AttachState::Ineligible,
                false,
                Some("claude cannot be taken over\x1b[2J in its terminal")
            )
        ),
        "attach: no (ineligible): claude cannot be taken over [2J in its terminal"
    );
    assert_eq!(
        attach_line(
            &job,
            &offer(AttachState::Busy, false, Some("codex-2 is running a turn"))
        ),
        "attach: no (busy): codex-2 is running a turn"
    );
}
