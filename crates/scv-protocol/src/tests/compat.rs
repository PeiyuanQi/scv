//! Compatibility with SCV 0.2.2, the release before the typed error codes:
//! frames it sent, frozen, must decode with today's types and encode back to
//! the same bytes, so a 0.2.2 client (such as the planned-restart watchdog,
//! which is always the previous release) reads what this server sends.
//!
//! The JSON below was produced by 0.2.2's own types. Never edit it: a change
//! that needs it edited breaks protocol version 3.

use super::*;

/// One frame of every `ServerEvent` 0.2.2 sends, as it sent them.
const SERVER_EVENTS_0_2_2: &[&str] = &[
    r#"{"type":"daemon.status","request_id":"d1","status":{"version":"0.2.2","pid":1234,"components":[{"id":"wechat:default","channel":"wechat","account":"default","bot_id":"bot-example","user_id":"user-example","enabled":true,"state":"connected","last_success_unix_seconds":1750000000,"error":null,"restarts":0,"remote_tools":"owner"},{"id":"feishu:work","channel":"feishu","account":"work","bot_id":null,"user_id":null,"enabled":false,"state":"backoff","last_success_unix_seconds":null,"error":"connection refused","restarts":3,"remote_tools":"none"}],"delegations":{"active":1,"reaped":2,"entries":[{"handle":"codex-3f9a2c","agent":"codex","session":"5d1c","depth":1,"pid":4321,"owner_pid":1234,"processes":3,"cwd":"/workspace/scv","started_unix_seconds":1750000000,"orphaned":false,"conversation":"codex-2","turn":3}],"killed":["codex-1"]},"restart":{"to_version":"0.2.3","waiting_for":"delegation codex-3f9a2c","requester":"codex-3f9a2c","origin":"wechat:default","deadline_unix_seconds":1750000600}}}"#,
    r#"{"type":"initialized","request_id":"1","protocol_version":3,"server":{"name":"scv-server","version":"0.2.2"}}"#,
    r#"{"type":"session.started","request_id":"2","session_id":"s","cwd":"/w","model":"gpt","context_max_tokens":128000,"max_server_frame_bytes":8388608,"max_transcript_bytes":8388608,"max_transcript_items":10000,"max_prompt_history_bytes":1048576,"max_prompt_history_items":200}"#,
    r#"{"type":"queue.snapshot","request_id":null,"session_id":"s","seq":1,"entries":[{"queue_id":"q1","revision":1,"prompt":"next","submitter":"r2","attachments":[{"kind":"image","path":"/m/p.jpg","name":"p.jpg","mime":"image/jpeg","size":10}]}],"paused":false}"#,
    r#"{"type":"queue.enqueued","request_id":"r2","session_id":"s","seq":2,"entry":{"queue_id":"q1","revision":1,"prompt":"next","submitter":"r2","attachments":[{"kind":"image","path":"/m/p.jpg","name":"p.jpg","mime":"image/jpeg","size":10}]},"position":0}"#,
    r#"{"type":"queue.updated","request_id":"r3","session_id":"s","seq":3,"entry":{"queue_id":"q1","revision":1,"prompt":"next","submitter":"r2","attachments":[{"kind":"image","path":"/m/p.jpg","name":"p.jpg","mime":"image/jpeg","size":10}]}}"#,
    r#"{"type":"queue.moved","request_id":"r4","session_id":"s","seq":4,"queue_id":"q1","position":0,"revision":3}"#,
    r#"{"type":"queue.removed","request_id":"r5","session_id":"s","seq":5,"queue_id":"q1","revision":4}"#,
    r#"{"type":"queue.dequeued","request_id":"r2","session_id":"s","seq":6,"queue_id":"q1","turn_id":"t"}"#,
    r#"{"type":"session.paused","request_id":"r6","session_id":"s","seq":7,"paused":true}"#,
    r#"{"type":"turn.started","request_id":"3","session_id":"s","turn_id":"t","seq":8}"#,
    r#"{"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":9,"origin":{"kind":"background","jobs":["job-1"]}}"#,
    r#"{"type":"assistant.delta","request_id":"3","session_id":"s","turn_id":"t","seq":10,"content":"I found "}"#,
    r#"{"type":"assistant.completed","request_id":"3","session_id":"s","turn_id":"t","seq":11,"content":"I found it."}"#,
    r#"{"type":"tool.proposed","request_id":"3","session_id":"s","turn_id":"t","seq":12,"call_id":"call_1","name":"agent_codex","arguments":{"background":true,"prompt":"Fix it"}}"#,
    r#"{"type":"approval.requested","request_id":"3","session_id":"s","turn_id":"t","seq":13,"approval_id":"a1","call_id":"call_1","name":"bash","risk":"process","cwd":"/w","summary":"Run shell command: cargo test"}"#,
    r#"{"type":"tool.started","request_id":"3","session_id":"s","turn_id":"t","seq":14,"call_id":"call_1","name":"agent_codex"}"#,
    r#"{"type":"tool.progress","request_id":"3","session_id":"s","turn_id":"t","seq":15,"call_id":"call_1","text":"$ cargo test"}"#,
    r#"{"type":"tool.completed","request_id":"3","session_id":"s","turn_id":"t","seq":16,"call_id":"call_1","name":"agent_codex","success":true,"output":"{\"job\":\"job-1\",\"tool\":\"agent_codex\",\"status\":\"running\",\"background\":true}","truncated":false}"#,
    r#"{"type":"tool.completed","request_id":"3","session_id":"s","turn_id":"t","seq":17,"call_id":"call_2","name":"bash","success":false,"output":"tool call denied by policy or user","truncated":false}"#,
    r#"{"type":"context.compacted","request_id":"3","session_id":"s","turn_id":"t","seq":18,"before_tokens":130000,"after_tokens":96000,"removed_messages":42}"#,
    r#"{"type":"session.trimmed","request_id":"3","session_id":"s","seq":19,"removed_messages":250,"history_bytes":12000000}"#,
    r#"{"type":"session.cleared","request_id":"6","session_id":"s","seq":20}"#,
    r#"{"type":"turn.completed","request_id":"3","session_id":"s","turn_id":"t","seq":21,"steps":4,"usage":{"input_tokens":1200,"output_tokens":240}}"#,
    r#"{"type":"turn.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":22,"steps":1,"usage":{},"origin":{"kind":"background","jobs":["job-1"]}}"#,
    r#"{"type":"turn.cancelled","request_id":"3","session_id":"s","turn_id":"t","seq":23}"#,
    r#"{"type":"turn.failed","request_id":"3","session_id":"s","turn_id":"t","seq":24,"code":"step_limit","message":"agent reached its maximum step count"}"#,
    r#"{"type":"error","request_id":"2","code":"invalid_request","message":"cwd is not a directory","fatal":false}"#,
    r#"{"type":"error","code":"invalid_json","message":"invalid protocol JSON: unknown variant `x`","fatal":false}"#,
    r#"{"type":"error","request_id":"1","code":"version_mismatch","message":"server supports protocol 3","fatal":true}"#,
];

/// A `daemon.status` body of 0.2.2 with no components, delegations, or restart.
const IDLE_STATUS_0_2_2: &str =
    r#"{"version":"0.2.2","pid":7,"components":[],"delegations":{"active":0,"reaped":0}}"#;

#[test]
fn every_0_2_2_event_decodes_and_encodes_to_the_same_bytes() {
    for frame in SERVER_EVENTS_0_2_2 {
        let event: ServerEvent = serde_json::from_str(frame).unwrap_or_else(|error| {
            panic!("0.2.2 frame no longer decodes ({error}): {frame}");
        });
        assert!(!matches!(event, ServerEvent::Unknown), "{frame}");
        assert_eq!(serde_json::to_string(&event).unwrap(), *frame);
    }
    let status: DaemonStatus = serde_json::from_str(IDLE_STATUS_0_2_2).unwrap();
    assert_eq!(serde_json::to_string(&status).unwrap(), IDLE_STATUS_0_2_2);
}

#[test]
fn the_0_2_2_codes_parse_as_their_typed_values() {
    let codes: Vec<_> = SERVER_EVENTS_0_2_2
        .iter()
        .filter_map(|frame| match serde_json::from_str(frame).unwrap() {
            ServerEvent::TurnFailed { code, .. } | ServerEvent::Error { code, .. } => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(
        codes,
        [
            ErrorCode::StepLimit,
            ErrorCode::InvalidRequest,
            ErrorCode::InvalidJson,
            ErrorCode::VersionMismatch
        ]
    );
    // 0.2.2 sent no tool error kind: a failed call parses without one.
    let denied = SERVER_EVENTS_0_2_2
        .iter()
        .find(|frame| frame.contains("denied by policy"))
        .unwrap();
    assert!(matches!(
        serde_json::from_str(denied).unwrap(),
        ServerEvent::ToolCompleted {
            success: false,
            error: None,
            ..
        }
    ));
}

#[test]
fn a_failed_call_adds_only_its_error_kind_to_the_0_2_2_frame() {
    let denied = SERVER_EVENTS_0_2_2
        .iter()
        .find(|frame| frame.contains("denied by policy"))
        .unwrap();
    let ServerEvent::ToolCompleted {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id,
        name,
        success,
        output,
        truncated,
        ..
    } = serde_json::from_str(denied).unwrap()
    else {
        panic!("not a tool.completed frame");
    };
    let typed = ServerEvent::ToolCompleted {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id,
        name,
        success,
        output,
        truncated,
        error: Some(ToolErrorKind::Denied),
        jobs: Vec::new(),
    };
    let mut sent = serde_json::to_value(&typed).unwrap();
    assert_eq!(sent["error"], "denied");
    // A 0.2.2 client ignores the new field and reads the rest unchanged.
    sent.as_object_mut().unwrap().remove("error");
    assert_eq!(
        sent,
        serde_json::from_str::<serde_json::Value>(denied).unwrap()
    );
}

#[test]
fn a_call_that_starts_a_job_adds_only_its_jobs_to_the_0_2_2_frame() {
    let started = SERVER_EVENTS_0_2_2
        .iter()
        .find(|frame| frame.contains(r#"\"background\":true"#))
        .unwrap();
    let mut event: ServerEvent = serde_json::from_str(started).unwrap();
    let ServerEvent::ToolCompleted { jobs, .. } = &mut event else {
        panic!("not a tool.completed frame");
    };
    assert!(jobs.is_empty(), "0.2.2 sent no jobs");
    jobs.push(JobChange {
        job: "job-1".into(),
        tool: "agent_codex".into(),
        status: JobStatus::Running,
        task: "Fix it".into(),
    });
    let mut sent = serde_json::to_value(&event).unwrap();
    assert_eq!(sent["jobs"][0]["status"], "running");
    sent.as_object_mut().unwrap().remove("jobs");
    assert_eq!(
        sent,
        serde_json::from_str::<serde_json::Value>(started).unwrap()
    );
    // The report turn's origin keeps its 0.2.2 shape.
    let report = SERVER_EVENTS_0_2_2
        .iter()
        .find(|frame| frame.contains(r#""type":"turn.started""#) && frame.contains("origin"))
        .unwrap();
    let ServerEvent::TurnStarted {
        origin: Some(origin),
        ..
    } = serde_json::from_str(report).unwrap()
    else {
        panic!("not a report turn");
    };
    assert_eq!(origin.kind, OriginKind::Background);
    assert_eq!(origin.jobs, ["job-1"]);
}

#[test]
fn what_a_0_2_2_client_sends_still_parses() {
    // The watchdog of a planned restart (the previous release) checks the
    // new daemon with `initialize` and `status`; 0.2.2's `scv restart
    // --when-idle` sends the restart request.
    for frame in [
        r#"{"type":"initialize","request_id":"init","protocol_version":3,"client":{"name":"scv-control","version":"0.2.2"}}"#,
        r#"{"type":"daemon.control","request_id":"control","command":{"action":"status"}}"#,
        r#"{"type":"daemon.control","request_id":"control","command":{"action":"restart_when_idle","version":"0.3.0","commit":"abc1234","parent":"0a1b2c3d/session/codex-3f9a2c","max_wait_seconds":600}}"#,
    ] {
        serde_json::from_str::<ClientMessage>(frame)
            .unwrap_or_else(|error| panic!("0.2.2 message no longer parses ({error}): {frame}"));
    }
}
