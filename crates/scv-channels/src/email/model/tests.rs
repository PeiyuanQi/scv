//! Unit tests for `src/email/model.rs`.

use super::*;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

/// A fake daemon that answers one triage session with `events` after the
/// turn starts, and returns every frame the client sent.
async fn daemon(listener: UnixListener, events: Vec<Value>) -> Vec<Value> {
    let (stream, _) = listener.accept().await.unwrap();
    let mut side = tokio::io::BufReader::new(stream);
    let mut received = Vec::new();
    let mut line = String::new();
    let send = |value: Value| format!("{value}\n");
    side.read_line(&mut line).await.unwrap();
    received.push(serde_json::from_str::<Value>(&line).unwrap());
    side.get_mut()
        .write_all(send(json!({"type":"initialized","request_id":"mail-init","protocol_version":PROTOCOL_VERSION,"server":{"name":"t","version":"0"}})).as_bytes())
        .await
        .unwrap();
    line.clear();
    side.read_line(&mut line).await.unwrap();
    received.push(serde_json::from_str::<Value>(&line).unwrap());
    side.get_mut()
        .write_all(send(json!({"type":"session.started","request_id":"mail-session","session_id":"s1","cwd":"/","model":"m","context_max_tokens":1,"max_server_frame_bytes":1,"max_transcript_bytes":1,"max_transcript_items":1,"max_prompt_history_bytes":1,"max_prompt_history_items":1})).as_bytes())
        .await
        .unwrap();
    line.clear();
    side.read_line(&mut line).await.unwrap();
    received.push(serde_json::from_str::<Value>(&line).unwrap());
    for event in events {
        let _ = side.get_mut().write_all(send(event).as_bytes()).await;
    }
    received
}

fn bound() -> (tempfile::TempDir, std::path::PathBuf, UnixListener) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    (directory, socket, listener)
}

#[tokio::test]
async fn a_triage_session_is_tool_free_channel_less_and_uses_the_fixed_frame() {
    let (directory, socket, listener) = bound();
    let events = vec![
        json!({"type":"assistant.delta","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":1,"content":"{\"notify\":"}),
        json!({"type":"assistant.completed","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":2,"content":"{\"notify\": true}"}),
        json!({"type":"turn.completed","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":3,"steps":1,"usage":{"input_tokens":700,"output_tokens":40}}),
    ];
    let server = tokio::spawn(daemon(listener, events));
    let cwd = directory.path().join("empty");
    let result = turn(&socket, &cwd, Some("cheap-model"), "FRAME", "PROMPT")
        .await
        .unwrap();
    assert_eq!(
        result,
        Turn {
            answer: "{\"notify\": true}".into(),
            tokens: Some(740)
        }
    );
    let frames = server.await.unwrap();
    let start = &frames[1];
    assert_eq!(start["type"], "session.start");
    assert_eq!(start["no_tools"], true);
    assert_eq!(start["auto_approve"], false);
    assert_eq!(start["system_prompt"], "FRAME");
    assert_eq!(start["model"], "cheap-model");
    assert_eq!(start["cwd"], cwd.display().to_string());
    let object = start.as_object().unwrap();
    assert!(!object.contains_key("channel"), "{start}");
    assert!(!object.contains_key("chat"), "{start}");
    assert_eq!(frames[2]["type"], "turn.start");
    assert_eq!(frames[2]["prompt"], "PROMPT");
    assert!(
        frames[2]["attachments"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
}

#[tokio::test]
async fn any_tool_or_approval_event_ends_the_turn() {
    for event in [
        json!({"type":"tool.started","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":1,"tool_call_id":"c","name":"bash","arguments":{}}),
        json!({"type":"tool.proposed","request_id":"mail-turn","session_id":"s1"}),
        json!({"type":"approval.requested","request_id":"mail-turn","session_id":"s1","approval_id":"a"}),
        json!({"type":"turn.started","request_id":"x","session_id":"s1","turn_id":"t2","seq":1,"origin":{"kind":"background_report","jobs":[]}}),
    ] {
        let (directory, socket, listener) = bound();
        let server = tokio::spawn(daemon(listener, vec![event.clone()]));
        let result = turn(&socket, directory.path(), None, "F", "P").await;
        assert!(matches!(result, Err(TurnError::ToolEvent)), "{event}");
        let frames = server.await.unwrap();
        // No model override was sent.
        assert!(!frames[1].as_object().unwrap().contains_key("model"));
    }
}

#[tokio::test]
async fn a_failed_turn_or_a_closed_session_is_a_plain_failure() {
    let (directory, socket, listener) = bound();
    let server = tokio::spawn(daemon(
        listener,
        vec![
            json!({"type":"turn.failed","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":1,"message":"quota: the prompt was <<<MAIL secret"}),
        ],
    ));
    let error = turn(&socket, directory.path(), None, "F", "P")
        .await
        .unwrap_err();
    let TurnError::Failed(error) = error else {
        panic!("a failure");
    };
    assert!(!error.to_string().contains("secret"), "{error}");
    server.await.unwrap();
    let (directory, socket, listener) = bound();
    let server = tokio::spawn(daemon(listener, Vec::new()));
    assert!(matches!(
        turn(&socket, directory.path(), None, "F", "P").await,
        Err(TurnError::Failed(_))
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn a_long_answer_is_cut() {
    let (directory, socket, listener) = bound();
    let long = "x".repeat(MAX_ANSWER_BYTES * 2);
    let server = tokio::spawn(daemon(
        listener,
        vec![
            json!({"type":"assistant.completed","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":1,"content":long}),
            json!({"type":"turn.completed","request_id":"mail-turn","session_id":"s1","turn_id":"t","seq":2,"steps":1,"usage":{}}),
        ],
    ));
    let result = turn(&socket, directory.path(), None, "F", "P")
        .await
        .unwrap();
    assert_eq!(result.answer.len(), MAX_ANSWER_BYTES);
    assert_eq!(result.tokens, None);
    server.await.unwrap();
}
