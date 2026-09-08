use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Stdio,
    thread,
};

use peon_protocol::{ClientMessage, PeerInfo, ServerEvent};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{Duration, timeout},
};

#[tokio::test]
async fn server_handshake_and_session_start() {
    let workspace = tempfile::tempdir().unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_peon-server"))
        .arg("--stdio")
        .env("OPENAI_API_KEY", "test-only")
        .env("XDG_CONFIG_HOME", config_home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    input
        .write_all(
            format!(
                concat!(
                    "{{\"type\":\"initialize\",\"request_id\":\"1\",\"protocol_version\":1,\"client\":{{\"name\":\"test\",\"version\":\"0\"}}}}\n",
                    "{{\"type\":\"session.start\",\"request_id\":\"2\",\"cwd\":{:?}}}\n"
                ),
                workspace.path().display().to_string()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let mut lines = BufReader::new(output).lines();
    let first = timeout(Duration::from_secs(3), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second = timeout(Duration::from_secs(3), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        serde_json::from_str::<ServerEvent>(&first).unwrap(),
        ServerEvent::Initialized { .. }
    ));
    assert!(matches!(
        serde_json::from_str::<ServerEvent>(&second).unwrap(),
        ServerEvent::SessionStarted { .. }
    ));
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn server_completes_a_streamed_turn_with_a_fake_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 64 * 1024];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"from fake\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let workspace = tempfile::tempdir().unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_peon-server"))
        .args([
            "--stdio",
            "--model",
            "fake-model",
            "--base-url",
            &format!("http://{address}/v1"),
        ])
        .env("OPENAI_API_KEY", "test-only")
        .env("XDG_CONFIG_HOME", config_home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let mut lines = BufReader::new(output).lines();

    send(
        &mut input,
        &ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: 1,
            client: PeerInfo {
                name: "integration-test".into(),
                version: "0".into(),
            },
        },
    )
    .await;
    send(
        &mut input,
        &ClientMessage::SessionStart {
            request_id: "session".into(),
            cwd: workspace.path().display().to_string(),
        },
    )
    .await;

    let initialized = next_event(&mut lines).await;
    assert!(matches!(initialized, ServerEvent::Initialized { .. }));
    let session_id = match next_event(&mut lines).await {
        ServerEvent::SessionStarted { session_id, .. } => session_id,
        event => panic!("expected session.started, received {event:?}"),
    };
    send(
        &mut input,
        &ClientMessage::TurnStart {
            request_id: "turn".into(),
            session_id,
            prompt: "say hello".into(),
        },
    )
    .await;

    let mut streamed = String::new();
    let usage = loop {
        match next_event(&mut lines).await {
            ServerEvent::AssistantDelta { content, .. } => streamed.push_str(&content),
            ServerEvent::TurnCompleted { usage, .. } => break usage,
            ServerEvent::TurnFailed { code, message, .. } => {
                panic!("turn failed with {code}: {message}")
            }
            _ => {}
        }
    };
    assert_eq!(streamed, "hello from fake");
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, Some(3));

    input.shutdown().await.unwrap();
    drop(input);
    provider.join().unwrap();
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

async fn send(input: &mut tokio::process::ChildStdin, message: &ClientMessage) {
    input
        .write_all(format!("{}\n", serde_json::to_string(message).unwrap()).as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
}

async fn next_event(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> ServerEvent {
    let line = timeout(Duration::from_secs(3), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("server closed stdout before the expected event");
    serde_json::from_str(&line).unwrap()
}
