//! Wire-format tests: JSON shapes and round trips.

use super::*;

mod compat;

#[test]
fn client_message_round_trip() {
    let message = ClientMessage::TurnStart {
        request_id: "3".into(),
        session_id: "session".into(),
        prompt: "hello".into(),
        attachments: Vec::new(),
    };
    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains("\"type\":\"turn.start\""));
    assert!(!json.contains("attachments"), "{json}");
    assert_eq!(
        serde_json::from_str::<ClientMessage>(&json).unwrap(),
        message
    );
}

#[test]
fn turns_carry_attachments_and_older_frames_have_none() {
    let message = ClientMessage::TurnStart {
        request_id: "3".into(),
        session_id: "session".into(),
        prompt: "what is this?".into(),
        attachments: vec![Attachment {
            kind: "image".into(),
            path: "/media/photo.jpg".into(),
            name: "photo.jpg".into(),
            mime: "image/jpeg".into(),
            size: 1234,
            transcript: None,
        }],
    };
    let wire = serde_json::to_string(&message).unwrap();
    assert!(wire.contains(r#""kind":"image""#), "{wire}");
    assert!(!wire.contains("transcript"), "{wire}");
    assert_eq!(
        serde_json::from_str::<ClientMessage>(&wire).unwrap(),
        message
    );
    let older = r#"{"type":"turn.start","request_id":"1","session_id":"s","prompt":"hi"}"#;
    assert!(matches!(
        serde_json::from_str::<ClientMessage>(older).unwrap(),
        ClientMessage::TurnStart { attachments, .. } if attachments.is_empty()
    ));
}

#[test]
fn only_a_successful_chat_attach_reports_an_attachment() {
    let output = r#"{"attached":{"path":"/w/report.pdf","name":"report.pdf","mime":"application/pdf","size":10},"note":"sent after your reply"}"#;
    let attached = reply_attachment(CHAT_ATTACH_TOOL, true, output).unwrap();
    assert_eq!(attached.name, "report.pdf");
    assert_eq!(attached.size, 10);
    assert!(attached.caption.is_empty());
    assert_eq!(reply_attachment(CHAT_ATTACH_TOOL, false, output), None);
    assert_eq!(reply_attachment("bash", true, output), None);
    assert_eq!(reply_attachment(CHAT_ATTACH_TOOL, true, "not json"), None);
}

#[test]
fn tool_progress_and_delegation_depth_round_trip() {
    let event = ServerEvent::ToolProgress {
        request_id: "r".into(),
        session_id: "s".into(),
        turn_id: "t".into(),
        seq: 4,
        call_id: "c".into(),
        text: "$ cargo test\nupdate …/src/lib.rs".into(),
    };
    let wire = serde_json::to_string(&event).unwrap();
    assert!(wire.contains(r#""type":"tool.progress""#));
    assert_eq!(serde_json::from_str::<ServerEvent>(&wire).unwrap(), event);

    let start = |depth| ClientMessage::SessionStart {
        request_id: "1".into(),
        cwd: "/w".into(),
        provider: None,
        model: None,
        base_url: None,
        no_tools: None,
        delegation_depth: depth,
        channel: None,
        auto_approve: None,
    };
    let nested = serde_json::to_string(&start(Some(2))).unwrap();
    assert!(nested.contains(r#""delegation_depth":2"#));
    assert_eq!(
        serde_json::from_str::<ClientMessage>(&nested).unwrap(),
        start(Some(2))
    );
    // Omitted when unset, and optional on the wire.
    let direct = serde_json::to_string(&start(None)).unwrap();
    assert!(!direct.contains("delegation_depth"));
    let older = r#"{"type":"session.start","request_id":"1","cwd":"/w"}"#;
    assert_eq!(
        serde_json::from_str::<ClientMessage>(older).unwrap(),
        start(None)
    );
    assert_eq!(PROTOCOL_VERSION, 3);
}

#[test]
fn chat_sessions_name_their_channel_and_approval_mode() {
    let chat = ClientMessage::SessionStart {
        request_id: "1".into(),
        cwd: "/w".into(),
        provider: None,
        model: None,
        base_url: None,
        no_tools: Some(false),
        delegation_depth: None,
        channel: Some("WeChat".into()),
        auto_approve: Some(true),
    };
    let wire = serde_json::to_string(&chat).unwrap();
    assert!(wire.contains(r#""channel":"WeChat""#), "{wire}");
    assert!(wire.contains(r#""auto_approve":true"#), "{wire}");
    assert_eq!(serde_json::from_str::<ClientMessage>(&wire).unwrap(), chat);
    // Frames from older clients omit both.
    let older = r#"{"type":"session.start","request_id":"1","cwd":"/w"}"#;
    assert!(matches!(
        serde_json::from_str::<ClientMessage>(older).unwrap(),
        ClientMessage::SessionStart {
            channel: None,
            auto_approve: None,
            ..
        }
    ));
}

#[test]
fn restart_requests_and_scheduled_restarts_round_trip() {
    let request = DaemonCommand::RestartWhenIdle {
        version: Some("0.1.37".into()),
        commit: Some("abc1234".into()),
        parent: Some("0a1b2c3d/session/codex-3f9a2c".into()),
        max_wait_seconds: Some(600),
    };
    let wire = serde_json::to_string(&request).unwrap();
    assert!(wire.contains(r#""action":"restart_when_idle""#), "{wire}");
    assert_eq!(
        serde_json::from_str::<DaemonCommand>(&wire).unwrap(),
        request
    );
    assert_eq!(
        serde_json::from_str::<DaemonCommand>(r#"{"action":"restart_when_idle"}"#).unwrap(),
        DaemonCommand::RestartWhenIdle {
            version: None,
            commit: None,
            parent: None,
            max_wait_seconds: None,
        }
    );
    // Status from a daemon without a scheduled restart omits it, and
    // older status frames parse.
    let status: DaemonStatus =
        serde_json::from_str(r#"{"version":"0.1.0","pid":1,"components":[]}"#).unwrap();
    assert!(status.restart.is_none());
    assert!(!serde_json::to_string(&status).unwrap().contains("restart"));
}

#[test]
fn owner_questions_and_their_state_round_trip() {
    let ask = DaemonCommand::ConfirmAsk {
        question: "Publish SCV 0.3.0?".into(),
        parent: Some("0a1b2c3d/session/codex-3f9a2c".into()),
        timeout_seconds: Some(600),
    };
    let wire = serde_json::to_string(&ask).unwrap();
    assert!(wire.contains(r#""action":"confirm_ask""#), "{wire}");
    assert_eq!(serde_json::from_str::<DaemonCommand>(&wire).unwrap(), ask);
    assert_eq!(
        serde_json::from_str::<DaemonCommand>(r#"{"action":"confirm_ask","question":"q"}"#)
            .unwrap(),
        DaemonCommand::ConfirmAsk {
            question: "q".into(),
            parent: None,
            timeout_seconds: None,
        }
    );
    let poll = DaemonCommand::ConfirmStatus { id: "a1b2".into() };
    assert_eq!(
        serde_json::to_string(&poll).unwrap(),
        r#"{"action":"confirm_status","id":"a1b2"}"#
    );
    let status: DaemonStatus = serde_json::from_str(
        r#"{"version":"0.3.0","pid":1,"components":[],"confirm":{"id":"a1b2","state":"expired","chat":"wechat:default","deadline_unix_seconds":5}}"#,
    )
    .unwrap();
    let confirm = status.confirm.clone().unwrap();
    assert_eq!(
        (confirm.state, confirm.chat.as_str()),
        (ConfirmState::Expired, "wechat:default")
    );
    assert_eq!(
        serde_json::from_str::<DaemonStatus>(&serde_json::to_string(&status).unwrap()).unwrap(),
        status
    );
    for (state, wire) in [
        (ConfirmState::Pending, "pending"),
        (ConfirmState::Yes, "yes"),
        (ConfirmState::No, "no"),
        (ConfirmState::Expired, "expired"),
        (ConfirmState::Withdrawn, "withdrawn"),
        (ConfirmState::Failed, "failed"),
    ] {
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            format!("\"{wire}\"")
        );
    }
    // A newer daemon's state parses, and a status without a question omits it.
    assert_eq!(
        serde_json::from_str::<ConfirmState>(r#""deferred""#).unwrap(),
        ConfirmState::Unknown
    );
    let plain: DaemonStatus =
        serde_json::from_str(r#"{"version":"0.2.2","pid":1,"components":[]}"#).unwrap();
    assert!(plain.confirm.is_none());
    assert!(!serde_json::to_string(&plain).unwrap().contains("confirm"));
}

#[test]
fn additive_fields_are_ignored() {
    let json = r#"{"type":"session.clear","request_id":"1","session_id":"s","future":true}"#;
    assert!(matches!(
        serde_json::from_str::<ClientMessage>(json).unwrap(),
        ClientMessage::SessionClear { .. }
    ));
}

#[test]
fn event_round_trip() {
    let event = ServerEvent::AssistantDelta {
        request_id: "1".into(),
        session_id: "s".into(),
        turn_id: "t".into(),
        seq: 4,
        content: "hello".into(),
    };
    let encoded = serde_json::to_string(&event).unwrap();
    assert_eq!(
        serde_json::from_str::<ServerEvent>(&encoded).unwrap(),
        event
    );
}

#[test]
fn queue_messages_and_events_round_trip() {
    let message = ClientMessage::QueueMove {
        request_id: "q1".into(),
        session_id: "s".into(),
        queue_id: "q".into(),
        revision: 2,
        before_queue_id: None,
    };
    let encoded = serde_json::to_string(&message).unwrap();
    assert_eq!(
        serde_json::from_str::<ClientMessage>(&encoded).unwrap(),
        message
    );
    let event = ServerEvent::QueueSnapshot {
        request_id: None,
        session_id: "s".into(),
        seq: 4,
        entries: vec![QueueEntry {
            queue_id: "q".into(),
            revision: 1,
            prompt: "hello".into(),
            submitter: "cli".into(),
            attachments: Vec::new(),
        }],
        paused: false,
    };
    let encoded = serde_json::to_string(&event).unwrap();
    assert_eq!(
        serde_json::from_str::<ServerEvent>(&encoded).unwrap(),
        event
    );
}

#[test]
fn remote_tools_fields_are_additive() {
    let legacy: DaemonCommand = serde_json::from_str(
        r#"{"action":"channel_set","channel":"wechat","account":"a","enabled":true,"workspace":null}"#,
    )
    .unwrap();
    assert!(matches!(
        legacy,
        DaemonCommand::ChannelSet {
            remote_tools: None,
            ..
        }
    ));
    let owner = DaemonCommand::ChannelSet {
        channel: "wechat".into(),
        account: "a".into(),
        enabled: true,
        workspace: None,
        remote_tools: Some(RemoteTools::Owner),
        senders: None,
    };
    let encoded = serde_json::to_string(&owner).unwrap();
    assert!(encoded.contains(r#""remote_tools":"owner""#));
    assert_eq!(
        serde_json::from_str::<DaemonCommand>(&encoded).unwrap(),
        owner
    );
    let health: ComponentHealth = serde_json::from_str(
        r#"{"id":"clawbot:a","account":"a","bot_id":null,"user_id":null,"enabled":true,"state":"connected","last_success_unix_seconds":null,"error":null,"restarts":0}"#,
    )
    .unwrap();
    assert_eq!(health.remote_tools, RemoteTools::None);
    // Daemons before channels reported no channel.
    assert!(health.channel.is_empty());
}

#[test]
fn senders_fields_are_additive() {
    let legacy: DaemonCommand = serde_json::from_str(
        r#"{"action":"channel_set","channel":"wechat","account":"a","enabled":true,"workspace":null,"remote_tools":"owner"}"#,
    )
    .unwrap();
    assert!(matches!(
        legacy,
        DaemonCommand::ChannelSet { senders: None, .. }
    ));
    let anyone = DaemonCommand::ChannelSet {
        channel: "feishu".into(),
        account: "a".into(),
        enabled: true,
        workspace: None,
        remote_tools: None,
        senders: Some(Senders::Anyone),
    };
    let encoded = serde_json::to_string(&anyone).unwrap();
    assert_eq!(
        encoded,
        r#"{"action":"channel_set","channel":"feishu","account":"a","enabled":true,"workspace":null,"senders":"anyone"}"#
    );
    assert_eq!(
        serde_json::from_str::<DaemonCommand>(&encoded).unwrap(),
        anyone
    );
    // A daemon before the setting reports none: it answers anyone.
    let older: ComponentHealth = serde_json::from_str(
        r#"{"id":"wechat:a","channel":"wechat","account":"a","bot_id":null,"user_id":null,"enabled":true,"state":"connected","last_success_unix_seconds":null,"error":null,"restarts":0,"remote_tools":"none"}"#,
    )
    .unwrap();
    assert_eq!(older.senders, None);
    let current = ComponentHealth {
        senders: Some(Senders::Owner),
        ..older
    };
    assert!(
        serde_json::to_string(&current)
            .unwrap()
            .ends_with(r#""remote_tools":"none","senders":"owner"}"#)
    );
    assert_eq!(Senders::default(), Senders::Owner);
}

#[test]
fn delegation_control_round_trips_and_older_status_still_parses() {
    for (command, wire) in [
        (
            DaemonCommand::Delegations { all: true },
            r#"{"action":"delegations","all":true}"#,
        ),
        (
            DaemonCommand::DelegationKill {
                handle: Some("codex-3f9a2c".into()),
                orphans: false,
            },
            r#"{"action":"delegation_kill","handle":"codex-3f9a2c","orphans":false}"#,
        ),
    ] {
        assert_eq!(serde_json::to_string(&command).unwrap(), wire);
        assert_eq!(
            serde_json::from_str::<DaemonCommand>(wire).unwrap(),
            command
        );
    }
    assert_eq!(
        serde_json::from_str::<DaemonCommand>(r#"{"action":"delegation_kill","orphans":true}"#)
            .unwrap(),
        DaemonCommand::DelegationKill {
            handle: None,
            orphans: true
        }
    );
    // A status from a daemon without delegation tracking.
    let status: DaemonStatus =
        serde_json::from_str(r#"{"version":"0.1.23","pid":7,"components":[]}"#).unwrap();
    assert_eq!(status.delegations, DelegationSummary::default());
}

#[test]
fn an_idle_live_delegation_is_additive() {
    // As 0.3.0 lists a delegation: no idle time or background jobs, and
    // none written back.
    let listed = r#"{"handle":"scv-92f0c3","agent":"scv","session":"s","depth":1,"pid":4321,"owner_pid":1234,"processes":1,"cwd":"/w","started_unix_seconds":1750000000,"orphaned":false,"conversation":"scv-1","turn":1}"#;
    let entry: DelegationInfo = serde_json::from_str(listed).unwrap();
    assert_eq!(entry.idle_since_unix_seconds, None);
    assert_eq!(entry.background_jobs, None);
    assert_eq!(serde_json::to_string(&entry).unwrap(), listed);
    let idle = DelegationInfo {
        idle_since_unix_seconds: Some(1_750_000_600),
        ..entry
    };
    let encoded = serde_json::to_string(&idle).unwrap();
    assert!(
        encoded.ends_with(r#""turn":1,"idle_since_unix_seconds":1750000600}"#),
        "{encoded}"
    );
    assert_eq!(
        serde_json::from_str::<DelegationInfo>(&encoded).unwrap(),
        idle
    );
    // A nested SCV between turns whose own background job still runs.
    let background = DelegationInfo {
        background_jobs: Some(1),
        ..idle
    };
    let encoded = serde_json::to_string(&background).unwrap();
    assert!(
        encoded.ends_with(r#""idle_since_unix_seconds":1750000600,"background_jobs":1}"#),
        "{encoded}"
    );
    assert_eq!(
        serde_json::from_str::<DelegationInfo>(&encoded).unwrap(),
        background
    );
}

#[test]
fn the_idle_delegation_count_is_additive() {
    // As 0.3.0 sums up delegations: no idle count, and none written back.
    let older = r#"{"active":2,"reaped":1}"#;
    let summary: DelegationSummary = serde_json::from_str(older).unwrap();
    assert_eq!(summary.idle, None);
    assert_eq!(serde_json::to_string(&summary).unwrap(), older);
    let counted = DelegationSummary {
        idle: Some(1),
        ..summary
    };
    let encoded = serde_json::to_string(&counted).unwrap();
    assert_eq!(encoded, r#"{"active":2,"idle":1,"reaped":1}"#);
    assert_eq!(
        serde_json::from_str::<DelegationSummary>(&encoded).unwrap(),
        counted
    );
    // A 0.3.0 client ignores the count and still reads every live run.
    #[derive(serde::Deserialize)]
    struct OlderSummary {
        active: u64,
        reaped: u64,
    }
    let read: OlderSummary = serde_json::from_str(&encoded).unwrap();
    assert_eq!((read.active, read.reaped), (2, 1));
}

#[test]
fn server_started_turns_carry_their_origin_and_client_turns_omit_it() {
    let started = ServerEvent::TurnStarted {
        request_id: "background:1".into(),
        session_id: "s".into(),
        turn_id: "t".into(),
        seq: 4,
        origin: Some(TurnOrigin {
            kind: OriginKind::Background,
            jobs: vec!["job-1".into()],
        }),
    };
    let json = serde_json::to_value(&started).unwrap();
    assert_eq!(
        json["origin"],
        serde_json::json!({"kind":"background","jobs":["job-1"]})
    );
    assert_eq!(
        serde_json::from_value::<ServerEvent>(json).unwrap(),
        started
    );
    assert_eq!(started.turn_request_id(), Some("background:1"));
    // A client's own turn has no origin on the wire, and older frames parse.
    let own: ServerEvent = serde_json::from_str(
        r#"{"type":"turn.completed","request_id":"r","session_id":"s","turn_id":"t","seq":9,"steps":1,"usage":{}}"#,
    )
    .unwrap();
    assert!(matches!(
        own,
        ServerEvent::TurnCompleted { origin: None, .. }
    ));
    assert!(!serde_json::to_string(&own).unwrap().contains("origin"));
}

#[test]
fn tool_calls_report_the_jobs_they_start_and_settle() {
    let started = JobChange {
        job: "job-1".into(),
        tool: "agent_codex".into(),
        status: JobStatus::Running,
        task: "Land the fix".into(),
    };
    assert!(started.started());
    assert_eq!(
        serde_json::to_value(&started).unwrap(),
        serde_json::json!({"job":"job-1","tool":"agent_codex","status":"running","task":"Land the fix"})
    );
    // The statuses are the strings the model reads in the job tools' results.
    for (status, wire) in [
        (JobStatus::Running, "running"),
        (JobStatus::Completed, "completed"),
        (JobStatus::Failed, "failed"),
        (JobStatus::Declined, "declined"),
        (JobStatus::Timeout, "timeout"),
        (JobStatus::Cancelled, "cancelled"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), wire);
        assert_eq!(status.to_string(), wire);
    }
    let settled: JobChange =
        serde_json::from_str(r#"{"job":"job-1","tool":"agent_codex","status":"paused"}"#).unwrap();
    assert_eq!(settled.status, JobStatus::Unknown);
    assert!(settled.task.is_empty());
    assert!(!settled.started());
    let event = ServerEvent::ToolCompleted {
        request_id: "r".into(),
        session_id: "s".into(),
        turn_id: "t".into(),
        seq: 3,
        call_id: "c".into(),
        name: "agent_codex".into(),
        success: true,
        output: "{}".into(),
        truncated: false,
        error: None,
        jobs: vec![started],
    };
    let wire = serde_json::to_string(&event).unwrap();
    assert!(wire.contains(r#""jobs":[{"job":"job-1""#), "{wire}");
    assert_eq!(serde_json::from_str::<ServerEvent>(&wire).unwrap(), event);
    // A newer server's origin kind parses too.
    let origin: TurnOrigin = serde_json::from_str(r#"{"kind":"schedule"}"#).unwrap();
    assert_eq!(origin.kind, OriginKind::Unknown);
    assert!(origin.jobs.is_empty());
}

#[test]
fn error_codes_keep_their_wire_names_and_unknown_ones_parse() {
    for code in [
        ErrorCode::InvalidJson,
        ErrorCode::NotInitialized,
        ErrorCode::VersionMismatch,
        ErrorCode::InvalidRequest,
        ErrorCode::Unsupported,
        ErrorCode::SessionNotFound,
        ErrorCode::TurnActive,
        ErrorCode::TurnNotFound,
        ErrorCode::ApprovalNotFound,
        ErrorCode::QueueLimit,
        ErrorCode::QueueNotFound,
        ErrorCode::QueueConflict,
        ErrorCode::ComponentError,
        ErrorCode::DelegationError,
        ErrorCode::RestartError,
        ErrorCode::ConfirmError,
        ErrorCode::ProviderError,
        ErrorCode::ContextLimit,
        ErrorCode::StepLimit,
        ErrorCode::HistoryLimit,
        ErrorCode::ResponseLimit,
        ErrorCode::ToolLimit,
        ErrorCode::InternalError,
    ] {
        let wire = serde_json::to_string(&code).unwrap();
        assert_eq!(wire, format!("\"{code}\""));
        assert_eq!(serde_json::from_str::<ErrorCode>(&wire).unwrap(), code);
    }
    assert_eq!(ErrorCode::QueueLimit.to_string(), "queue_limit");
    // A newer server's code does not fail the event it arrives in.
    let error: ServerEvent = serde_json::from_str(
        r#"{"type":"error","request_id":"1","code":"rate_limited","message":"slow down","fatal":false}"#,
    )
    .unwrap();
    assert!(matches!(
        error,
        ServerEvent::Error {
            code: ErrorCode::Unknown,
            ref message,
            ..
        } if message == "slow down"
    ));
}

#[test]
fn tool_error_kinds_keep_their_wire_names_and_unknown_ones_parse() {
    for (kind, wire) in [
        (ToolErrorKind::Denied, "denied"),
        (ToolErrorKind::Cancelled, "cancelled"),
        (ToolErrorKind::InvalidArguments, "invalid_arguments"),
        (ToolErrorKind::Unavailable, "unavailable"),
        (ToolErrorKind::Limit, "limit"),
        (ToolErrorKind::Failed, "failed"),
        (ToolErrorKind::UnknownTool, "unknown_tool"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), wire);
        assert_eq!(kind.to_string(), wire);
        assert_eq!(
            serde_json::from_value::<ToolErrorKind>(wire.into()).unwrap(),
            kind
        );
    }
    assert_eq!(
        serde_json::from_str::<ToolErrorKind>(r#""sandboxed""#).unwrap(),
        ToolErrorKind::Unknown
    );
}

#[test]
fn unknown_event_types_parse_as_unknown() {
    let event: ServerEvent = serde_json::from_str(
        r#"{"type":"session.renamed","request_id":"r","session_id":"s","seq":3,"name":{"x":1}}"#,
    )
    .unwrap();
    assert_eq!(event, ServerEvent::Unknown);
    assert_eq!(event.turn_request_id(), None);
    // A known type with a malformed body is still an error.
    assert!(serde_json::from_str::<ServerEvent>(r#"{"type":"turn.started"}"#).is_err());
}
