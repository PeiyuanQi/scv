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
fn server_started_turns_carry_their_origin_and_client_turns_omit_it() {
    let started = ServerEvent::TurnStarted {
        request_id: "background:1".into(),
        session_id: "s".into(),
        turn_id: "t".into(),
        seq: 4,
        origin: Some(TurnOrigin {
            kind: ORIGIN_BACKGROUND.into(),
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
fn background_job_updates_come_from_start_wait_and_status_outputs() {
    let started = background_job_update(
        r#"{"job":"job-1","tool":"agent_codex","status":"running","background":true}"#,
    );
    assert_eq!(started.started, vec!["job-1".to_owned()]);
    assert!(started.settled.is_empty());
    // A running job listed by agent_status is neither started nor settled.
    let listed = background_job_update(
        r#"{"jobs":[{"job":"job-1","status":"running"},{"job":"job-2","status":"failed"}]}"#,
    );
    assert!(listed.started.is_empty());
    assert_eq!(listed.settled, vec!["job-2".to_owned()]);
    let waited = background_job_update(r#"{"job":"job-1","status":"completed"}"#);
    assert_eq!(waited.settled, vec!["job-1".to_owned()]);
    // Ordinary agent results and non-JSON output are no job updates.
    for other in [
        r#"{"agent":"codex","status":"completed"}"#,
        "plain text",
        "[1]",
    ] {
        assert_eq!(background_job_update(other), BackgroundJobUpdate::default());
    }
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
