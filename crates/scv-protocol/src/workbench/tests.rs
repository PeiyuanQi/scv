//! Unit tests for `src/workbench.rs`.

use super::*;
use crate::{DaemonCommand, DaemonStatus, ServerEvent};

#[test]
fn agent_states_use_snake_case_and_unknown_ones_read_as_unknown() {
    assert_eq!(
        serde_json::to_string(&AgentState::AwaitingInput).unwrap(),
        r#""awaiting_input""#
    );
    assert_eq!(
        serde_json::from_str::<AgentState>(r#""paused_by_gremlins""#).unwrap(),
        AgentState::Unknown
    );
    for state in [
        AgentState::Completed,
        AgentState::Failed,
        AgentState::Declined,
        AgentState::Timeout,
        AgentState::Cancelled,
        AgentState::Interrupted,
    ] {
        assert!(state.is_final(), "{state}");
    }
    for state in [
        AgentState::Starting,
        AgentState::Working,
        AgentState::AwaitingInput,
        AgentState::Idle,
        AgentState::Queued,
        AgentState::Unknown,
    ] {
        assert!(!state.is_final(), "{state}");
    }
    assert!(AgentState::AwaitingInput.needs_attention());
    assert_eq!(AgentState::Timeout.to_string(), "timeout");
}

#[test]
fn run_events_round_trip_and_unknown_kinds_are_kept() {
    let event = RunEvent {
        seq: 3,
        ts_ms: 1,
        kind: RunEventKind::ApprovalRequested,
        text: "[codex-1 acp] Run cargo test (execute)".into(),
        turn: None,
        status: None,
        job: None,
        risk: Some("process".into()),
        by: None,
    };
    let wire = serde_json::to_string(&event).unwrap();
    assert!(wire.contains(r#""kind":"approval_requested""#), "{wire}");
    assert!(!wire.contains("turn"), "{wire}");
    assert_eq!(serde_json::from_str::<RunEvent>(&wire).unwrap(), event);
    let newer = r#"{"seq":1,"ts_ms":2,"kind":"telepathy","text":"hi"}"#;
    assert_eq!(
        serde_json::from_str::<RunEvent>(newer).unwrap().kind,
        RunEventKind::Unknown
    );
}

#[test]
fn workbench_actions_are_tagged_daemon_commands() {
    let watch = serde_json::to_string(&DaemonCommand::Workbench { watch: true }).unwrap();
    assert_eq!(watch, r#"{"action":"workbench","watch":true}"#);
    assert_eq!(
        serde_json::to_string(&DaemonCommand::Workbench { watch: false }).unwrap(),
        r#"{"action":"workbench"}"#
    );
    let send: DaemonCommand = serde_json::from_str(
        r#"{"action":"workbench_send","session":"s","conversation":"codex-2","prompt":"go on"}"#,
    )
    .unwrap();
    assert_eq!(
        send,
        DaemonCommand::WorkbenchSend {
            session: "s".into(),
            conversation: "codex-2".into(),
            prompt: "go on".into(),
            mode: SendMode::Auto,
        }
    );
    let start = DaemonCommand::WorkbenchStart {
        workspace: "/w".into(),
        agent: "codex".into(),
        prompt: "fix it".into(),
        cwd: None,
        model: None,
        effort: None,
        worktree: true,
    };
    let wire = serde_json::to_string(&start).unwrap();
    assert_eq!(
        wire,
        r#"{"action":"workbench_start","workspace":"/w","agent":"codex","prompt":"fix it","worktree":true}"#
    );
    for (wire, command) in [
        (
            r#"{"action":"run_log","handle":"codex-3f9a2c","after":120}"#,
            DaemonCommand::RunLog {
                handle: "codex-3f9a2c".into(),
                after: 120,
                limit: None,
            },
        ),
        (
            r#"{"action":"workbench_approve","id":"ap-1","approved":false}"#,
            DaemonCommand::WorkbenchApprove {
                id: "ap-1".into(),
                approved: false,
            },
        ),
        (r#"{"action":"worktrees"}"#, DaemonCommand::Worktrees),
        (
            r#"{"action":"worktree_remove","path":"/h/worktrees/a/b"}"#,
            DaemonCommand::WorktreeRemove {
                path: "/h/worktrees/a/b".into(),
                force: false,
            },
        ),
    ] {
        assert_eq!(
            serde_json::from_str::<DaemonCommand>(wire).unwrap(),
            command
        );
    }
}

#[test]
fn a_status_without_workbench_fields_reads_and_writes_as_before() {
    let older =
        r#"{"version":"0.4.1","pid":7,"components":[],"delegations":{"active":0,"reaped":0}}"#;
    let status: DaemonStatus = serde_json::from_str(older).unwrap();
    assert!(status.workbench.is_none() && status.run_log.is_none() && status.worktrees.is_empty());
    assert_eq!(serde_json::to_string(&status).unwrap(), older);
}

#[test]
fn snapshots_round_trip_and_leave_out_what_is_empty() {
    let snapshot = WorkbenchSnapshot {
        generated_unix_seconds: 5,
        sessions: vec![WorkbenchSession {
            id: "s".into(),
            label: "TUI".into(),
            workspace: "/w".into(),
            live: true,
            state: AgentState::Idle,
            detail: None,
        }],
        jobs: vec![WorkbenchJob {
            session: "s".into(),
            job: "job-1".into(),
            agent: "codex".into(),
            task: "Fix it".into(),
            workspace: "/w".into(),
            cwd: "/h/worktrees/w-1/codex-abc123".into(),
            state: AgentState::Interrupted,
            detail: Some("the SCV process that ran it stopped".into()),
            started_by: JobOrigin::Owner,
            conversation: Some("codex-1".into()),
            run: None,
            worktree: Some(WorktreeRef {
                path: "/h/worktrees/w-1/codex-abc123".into(),
                branch: "scv/codex-abc123".into(),
                repo: "/w".into(),
            }),
            reviewed: false,
            resumable: true,
            resumed_as: None,
            created_unix_seconds: 1,
            updated_unix_seconds: 2,
        }],
        runs: Vec::new(),
        approvals: Vec::new(),
        restorable: Vec::new(),
    };
    let wire = serde_json::to_string(&snapshot).unwrap();
    assert!(
        !wire.contains("approvals") && !wire.contains("\"runs\""),
        "{wire}"
    );
    assert!(wire.contains(r#""started_by":"owner""#), "{wire}");
    assert_eq!(
        serde_json::from_str::<WorkbenchSnapshot>(&wire).unwrap(),
        snapshot
    );
    let info = WorktreeInfo {
        worktree: snapshot.jobs[0].worktree.clone().unwrap(),
        dirty: Some(true),
        jobs: vec!["s/job-1".into()],
    };
    let wire = serde_json::to_string(&info).unwrap();
    assert!(wire.contains(r#""branch":"scv/codex-abc123""#), "{wire}");
    assert_eq!(serde_json::from_str::<WorktreeInfo>(&wire).unwrap(), info);
}

#[test]
fn session_started_carries_keys_and_a_restored_queue_only_when_set() {
    let started =
        |keys: std::collections::BTreeMap<String, String>, restored| ServerEvent::SessionStarted {
            request_id: "2".into(),
            session_id: "s".into(),
            cwd: "/w".into(),
            model: "m".into(),
            context_max_tokens: 1,
            max_server_frame_bytes: 1,
            max_transcript_bytes: 1,
            max_transcript_items: 1,
            max_prompt_history_bytes: 1,
            max_prompt_history_items: 1,
            keys,
            restored,
        };
    let plain = serde_json::to_string(&started(std::collections::BTreeMap::new(), None)).unwrap();
    assert!(
        !plain.contains("keys") && !plain.contains("restored"),
        "{plain}"
    );
    let keys = std::collections::BTreeMap::from([("next_tab".to_owned(), "alt+.".to_owned())]);
    let restored = RestoredQueue {
        from: "old".into(),
        queued: 2,
        interrupted: Some("the long build".into()),
        dropped: 0,
    };
    let event = started(keys, Some(restored));
    let wire = serde_json::to_string(&event).unwrap();
    assert!(wire.contains(r#""keys":{"next_tab":"alt+."}"#), "{wire}");
    assert!(!wire.contains("dropped"), "{wire}");
    assert_eq!(serde_json::from_str::<ServerEvent>(&wire).unwrap(), event);
    let mut event = event;
    let ServerEvent::SessionStarted {
        restored: Some(restored),
        ..
    } = &mut event
    else {
        panic!("session.started");
    };
    restored.dropped = 1;
    let wire = serde_json::to_string(&event).unwrap();
    assert!(wire.contains(r#""dropped":1"#), "{wire}");
    assert_eq!(serde_json::from_str::<ServerEvent>(&wire).unwrap(), event);
}

#[test]
fn attach_actions_are_tagged_daemon_commands_with_optional_fields_left_out() {
    let attach = DaemonCommand::Attach {
        session: "s".into(),
        conversation: "codex-2".into(),
        cols: Some(120),
        rows: None,
    };
    assert_eq!(
        serde_json::to_string(&attach).unwrap(),
        r#"{"action":"attach","session":"s","conversation":"codex-2","cols":120}"#
    );
    let io: DaemonCommand = serde_json::from_str(r#"{"action":"attach_io","id":"at-1"}"#).unwrap();
    assert_eq!(
        io,
        DaemonCommand::AttachIo {
            id: "at-1".into(),
            after: 0,
            input: String::new(),
            cols: None,
            rows: None,
        }
    );
    for wire in [
        r#"{"action":"attach_detach","id":"at-1"}"#,
        r#"{"action":"attach_end","session":"s","conversation":"codex-2"}"#,
    ] {
        let command: DaemonCommand = serde_json::from_str(wire).unwrap();
        assert_eq!(serde_json::to_string(&command).unwrap(), wire);
    }
}

#[test]
fn an_attach_page_round_trips_and_leaves_defaults_off_the_wire() {
    let page = AttachPage {
        id: "at-1".into(),
        session: "s".into(),
        conversation: "codex-2".into(),
        agent: "codex".into(),
        run: Some("codex-3f9a2c".into()),
        generation: 4,
        cols: 80,
        rows: 24,
        lines: vec![AttachRow {
            row: 3,
            spans: vec![
                AttachSpan {
                    text: "ok".into(),
                    fg: Some(AttachColor::Index(2)),
                    bold: true,
                    ..AttachSpan::default()
                },
                AttachSpan {
                    text: " done".into(),
                    bg: Some(AttachColor::Rgb([1, 2, 3])),
                    ..AttachSpan::default()
                },
            ],
        }],
        full: false,
        cursor: Some([3, 7]),
        application_cursor: true,
        bracketed_paste: false,
        ended: false,
        exit: None,
        note: None,
    };
    let wire = serde_json::to_string(&page).unwrap();
    assert!(wire.contains(r#""fg":2"#), "{wire}");
    assert!(wire.contains(r#""bg":[1,2,3]"#), "{wire}");
    for absent in [
        "\"full\"",
        "\"ended\"",
        "\"exit\"",
        "\"note\"",
        "\"italic\"",
        "\"bracketed_paste\"",
    ] {
        assert!(!wire.contains(absent), "{absent} in {wire}");
    }
    assert_eq!(serde_json::from_str::<AttachPage>(&wire).unwrap(), page);
}

#[test]
fn a_status_with_or_without_a_screen_reads_on_either_side_of_the_change() {
    // A daemon before attach sends no `attach`; this client reads none.
    let older = r#"{"version":"0.4.4","pid":1,"components":[]}"#;
    let status: DaemonStatus = serde_json::from_str(older).unwrap();
    assert!(status.attach.is_none());
    // An older client ignores the field it does not know.
    let mut status = status;
    status.attach = Some(AttachPage {
        id: "at-1".into(),
        session: "s".into(),
        conversation: "codex-2".into(),
        agent: "codex".into(),
        run: None,
        generation: 1,
        cols: 80,
        rows: 24,
        lines: Vec::new(),
        full: true,
        cursor: None,
        application_cursor: false,
        bracketed_paste: false,
        ended: true,
        exit: Some("exit 0".into()),
        note: None,
    });
    let event = ServerEvent::DaemonStatus {
        request_id: "d1".into(),
        status,
    };
    let wire = serde_json::to_value(&event).unwrap();
    #[derive(serde::Deserialize)]
    #[allow(dead_code, reason = "the fields an older client knows")]
    struct OlderStatus {
        version: String,
        pid: u32,
        components: Vec<serde_json::Value>,
    }
    let older: OlderStatus = serde_json::from_value(wire["status"].clone()).unwrap();
    assert_eq!(older.version, "0.4.4");
}
