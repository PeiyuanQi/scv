//! Unit tests for `src/cli/jobs/events.rs`.

use scv_protocol::{JobOrigin, RunEventKind};

use super::*;

fn job(name: &str, state: AgentState) -> WorkbenchJob {
    WorkbenchJob {
        session: "3f2a9c1e-aaaa".into(),
        job: name.into(),
        agent: "codex".into(),
        task: "task".into(),
        workspace: "/w".into(),
        cwd: "/w".into(),
        state,
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

fn approval(id: &str) -> WorkbenchApproval {
    WorkbenchApproval {
        id: id.into(),
        session: "3f2a9c1e-aaaa".into(),
        job: "job-2".into(),
        name: "bash".into(),
        risk: "process".into(),
        cwd: "/w".into(),
        summary: "Run make".into(),
        since_unix_seconds: 1,
        expires_unix_seconds: 2,
    }
}

fn snapshot(jobs: Vec<WorkbenchJob>, approvals: Vec<WorkbenchApproval>) -> WorkbenchSnapshot {
    WorkbenchSnapshot {
        generated_unix_seconds: 5,
        jobs,
        approvals,
        ..WorkbenchSnapshot::default()
    }
}

#[test]
fn snapshots_become_added_changed_and_removed_jobs_and_approvals() {
    let mut seen = Seen::default();
    // Newest first, as the workbench lists them; added oldest first.
    let first = seen.update(&snapshot(
        vec![
            job("job-2", AgentState::Working),
            job("job-1", AgentState::Working),
        ],
        vec![approval("ap-1")],
    ));
    assert_eq!(
        first,
        [
            Event::JobAdded {
                job: job("job-1", AgentState::Working)
            },
            Event::JobAdded {
                job: job("job-2", AgentState::Working)
            },
            Event::ApprovalRequested {
                approval: approval("ap-1")
            },
        ]
    );
    // Only the detail changed: nothing to say.
    let quiet = WorkbenchJob {
        detail: Some("running for 9 s".into()),
        ..job("job-2", AgentState::Working)
    };
    assert_eq!(
        seen.update(&snapshot(
            vec![quiet.clone(), job("job-1", AgentState::Working)],
            vec![approval("ap-1")],
        )),
        []
    );
    let done = job("job-2", AgentState::Completed);
    assert_eq!(
        seen.update(&snapshot(vec![done.clone()], Vec::new())),
        [
            Event::JobChanged {
                from: AgentState::Working,
                job: done
            },
            Event::JobRemoved {
                session: "3f2a9c1e-aaaa".into(),
                job: "job-1".into()
            },
            Event::ApprovalResolved {
                id: "ap-1".into(),
                session: "3f2a9c1e-aaaa".into(),
                job: "job-2".into()
            },
        ]
    );
}

#[test]
fn every_event_is_one_json_line_tagged_with_its_type() {
    let lines: Vec<String> = [
        Event::JobRemoved {
            session: "s".into(),
            job: "job-1".into(),
        },
        Event::ApprovalResolved {
            id: "ap-1".into(),
            session: "s".into(),
            job: "job-1".into(),
        },
        Event::Ready {
            generated_unix_seconds: 7,
        },
        Event::Run {
            run: "codex-3f9a2c".into(),
            session: "s".into(),
            event: RunEvent {
                seq: 2,
                ts_ms: 3,
                kind: RunEventKind::Text,
                text: "line\none\u{9b}".into(),
                turn: None,
                status: None,
                job: None,
                risk: None,
                by: None,
            },
        },
        Event::JobChanged {
            from: AgentState::Working,
            job: job("job-1", AgentState::Failed),
        },
    ]
    .iter()
    .map(|event| json_line(event).unwrap())
    .collect();
    assert_eq!(
        lines[..4],
        [
            r#"{"type":"job.removed","session":"s","job":"job-1"}"#,
            r#"{"type":"approval.resolved","id":"ap-1","session":"s","job":"job-1"}"#,
            r#"{"type":"ready","generated_unix_seconds":7}"#,
            r#"{"type":"run.event","run":"codex-3f9a2c","session":"s","event":{"seq":2,"ts_ms":3,"kind":"text","text":"line\none\u009b"}}"#,
        ]
    );
    assert!(
        lines[4].starts_with(r#"{"type":"job.changed","from":"working","job":{"session":"3f2a9c1e-aaaa","job":"job-1","#),
        "{}",
        lines[4]
    );
    for line in &lines {
        assert!(!line.contains('\n') && !line.chars().any(char::is_control));
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(value["type"].is_string(), "{line}");
    }
}

#[test]
fn text_lines_are_inert_and_ready_is_left_out() {
    let changed = Event::JobChanged {
        from: AgentState::Working,
        job: WorkbenchJob {
            detail: Some("boom\u{1b}[2J".into()),
            ..job("job-1", AgentState::Failed)
        },
    };
    assert_eq!(
        describe_event(&changed).unwrap(),
        "~ 3f2a9c1e/job-1 working → failed: boom [2J"
    );
    assert_eq!(
        describe_event(&Event::Ready {
            generated_unix_seconds: 1
        }),
        None
    );
}

fn run(handle: &str, live: bool) -> WorkbenchRun {
    WorkbenchRun {
        handle: handle.into(),
        agent: "codex".into(),
        session: "s".into(),
        transport: "cli".into(),
        conversation: None,
        turn: None,
        job: Some("job-1".into()),
        jobs: Vec::new(),
        cwd: "/w".into(),
        state: AgentState::Working,
        detail: None,
        started_unix_seconds: 1,
        last_activity_unix_seconds: 1,
        live,
        steerable: false,
        attach: None,
    }
}

fn append(dir: &std::path::Path, handle: &str, seq: u64, kind: &str, text: &str) {
    use std::io::Write as _;
    let line = serde_json::json!({"v": 1, "seq": seq, "ts_ms": seq, "kind": kind, "text": text});
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{handle}.jsonl")))
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

fn texts(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .map(|event| match event {
            Event::Run { run, event, .. } => (run.clone(), event.text.clone()),
            other => panic!("not a log line: {other:?}"),
        })
        .collect()
}

#[test]
fn logs_are_tailed_from_their_end_at_first_and_from_the_start_when_new() {
    let dir = tempfile::tempdir().unwrap();
    append(dir.path(), "codex-aaaaaa", 1, "text", "before");
    append(dir.path(), "codex-bbbbbb", 1, "text", "long over");
    let mut tails = Tails::new(dir.path().to_owned());
    let before = [run("codex-aaaaaa", true), run("codex-bbbbbb", false)];
    assert_eq!(tails.poll(&before, true), []);
    append(dir.path(), "codex-aaaaaa", 2, "text", "after");
    append(dir.path(), "codex-bbbbbb", 2, "text", "never read");
    append(dir.path(), "codex-cccccc", 1, "run_started", "new run");
    append(dir.path(), "codex-cccccc", 2, "run_ended", "");
    // Newest first: the new run is listed before the others.
    let later = [
        run("codex-cccccc", false),
        run("codex-aaaaaa", true),
        run("codex-bbbbbb", false),
    ];
    assert_eq!(
        texts(&tails.poll(&later, false)),
        [
            ("codex-aaaaaa".into(), "after".into()),
            ("codex-cccccc".into(), "new run".into()),
            ("codex-cccccc".into(), String::new()),
        ]
    );
    // Its log ended, so it is not read again.
    append(dir.path(), "codex-cccccc", 3, "text", "after its end");
    assert_eq!(tails.poll(&later, false), []);
}
