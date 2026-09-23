//! A delegated agent's structured events reach protocol v3 clients as
//! bounded `tool.progress` events, and v2 clients are turned away.

mod common;

use common::Isolated;
use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::Stdio,
    thread,
    time::Duration,
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

fn write_private(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A provider that asks for one `agent_codex` call, then answers "done".
fn serve_provider(listener: TcpListener) {
    thread::spawn(move || {
        let bodies = [
            concat!(
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"prompt\\\":\\\"work\\\"}\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"agent_codex\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            ),
        ];
        for body in bodies {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 65536];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
}

#[tokio::test]
async fn delegated_agent_events_arrive_as_bounded_tool_progress() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    serve_provider(listener);

    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // A fake Codex that streams `exec --json` events over about 1.5 seconds,
    // including tool output that must never reach the client.
    let agent = home_path.join("fake-codex");
    std::fs::write(
        &agent,
        concat!(
            "#!/bin/sh\n",
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}'\n",
            "echo '{\"type\":\"item.started\",\"item\":{\"id\":\"i1\",\"type\":\"command_execution\",\"command\":\"/bin/bash -lc '\"'\"'cargo test'\"'\"'\",\"aggregated_output\":\"\",\"exit_code\":null}}'\n",
            "sleep 0.7\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"aggregated_output\":\"PRIVATE TOOL OUTPUT\",\"exit_code\":0}}'\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i2\",\"type\":\"file_change\",\"changes\":[{\"path\":\"/w/crates/core/src/lib.rs\",\"kind\":\"update\"}]}}'\n",
            "sleep 0.7\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i3\",\"type\":\"web_search\",\"query\":\"curl -H Authorization: Bearer abc123\"}}'\n",
            "sleep 0.7\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i4\",\"type\":\"agent_message\",\"text\":\"finished\"}}'\n",
            "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &home_path.join("config.toml"),
        &format!(
            "[agents.codex]\ncommand = {:?}\n",
            agent.display().to_string()
        ),
    );

    let mut server = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(&home_path)
        .arg("--scv-home")
        .arg(&home_path)
        .args([
            "--model",
            "fake-model",
            "--base-url",
            &format!("http://{address}/v1"),
            "server",
            "--stdio",
        ])
        .env("OPENAI_API_KEY", "test-only")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = server.stdin.take().unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let mut send = async |message: ClientMessage| {
        input
            .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
    };
    send(ClientMessage::Initialize {
        request_id: "init".into(),
        protocol_version: PROTOCOL_VERSION,
        client: PeerInfo {
            name: "progress-test".into(),
            version: "0".into(),
        },
    })
    .await;
    send(ClientMessage::SessionStart {
        request_id: "session".into(),
        cwd: workspace.path().display().to_string(),
        provider: None,
        model: None,
        base_url: None,
        no_tools: None,
        delegation_depth: None,
    })
    .await;

    let mut session_id = None;
    let mut started_seq = None;
    let mut progress = Vec::new();
    let mut completed_output = None;
    let mut last_seq = 0;
    loop {
        let line = timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("server went quiet")
            .unwrap()
            .expect("server exited");
        let event: ServerEvent = serde_json::from_str(&line).unwrap();
        match event {
            ServerEvent::SessionStarted { session_id: id, .. } => {
                send(ClientMessage::TurnStart {
                    request_id: "turn".into(),
                    session_id: id.clone(),
                    prompt: "delegate".into(),
                })
                .await;
                session_id = Some(id);
            }
            ServerEvent::ApprovalRequested { approval_id, .. } => {
                send(ClientMessage::ApprovalResolve {
                    request_id: "approve".into(),
                    session_id: session_id.clone().unwrap(),
                    approval_id,
                    approved: true,
                })
                .await;
            }
            ServerEvent::ToolStarted { seq, .. } => started_seq = Some(seq),
            ServerEvent::ToolProgress {
                seq, call_id, text, ..
            } => {
                assert!(seq > last_seq, "progress out of order");
                assert!(started_seq.is_some_and(|started| seq > started));
                assert!(completed_output.is_none(), "progress after completion");
                assert_eq!(call_id, "call_1");
                assert!(text.len() <= 512, "{} bytes", text.len());
                progress.push(text);
            }
            ServerEvent::ToolCompleted { output, .. } => completed_output = Some(output),
            ServerEvent::TurnCompleted { .. } => break,
            ServerEvent::TurnFailed { message, .. } | ServerEvent::Error { message, .. } => {
                panic!("turn failed: {message}")
            }
            _ => {}
        }
        if let Some(seq) = seq_of(&line) {
            last_seq = seq;
        }
    }

    let lines: Vec<&str> = progress.iter().flat_map(|text| text.lines()).collect();
    assert!(lines.contains(&"$ cargo test"), "{lines:?}");
    assert!(lines.contains(&"update …/src/lib.rs"), "{lines:?}");
    let all = progress.join("\n");
    assert!(!all.contains("PRIVATE TOOL OUTPUT"), "tool output leaked");
    assert!(!all.contains("abc123"), "credential leaked: {all}");
    let output = completed_output.expect("the delegation never completed");
    assert!(output.contains("finished"));
    assert!(
        !output.contains("cargo test"),
        "progress entered the result"
    );
}

fn seq_of(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("seq")?
        .as_u64()
}

#[tokio::test]
async fn protocol_version_2_clients_are_told_to_upgrade() {
    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(&home_path)
        .arg("--scv-home")
        .arg(&home_path)
        .args(["server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = server.stdin.take().unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let message = ClientMessage::Initialize {
        request_id: "init".into(),
        protocol_version: 2,
        client: PeerInfo {
            name: "old-client".into(),
            version: "0.1.28".into(),
        },
    };
    input
        .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
    let line = timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match serde_json::from_str::<ServerEvent>(&line).unwrap() {
        ServerEvent::Error { code, message, .. } => {
            assert_eq!(code, "version_mismatch");
            assert!(message.contains(&PROTOCOL_VERSION.to_string()), "{message}");
        }
        other => panic!("expected version_mismatch, got {other:?}"),
    }
}
