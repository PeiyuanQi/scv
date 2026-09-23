mod common;

use common::Isolated;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Stdio,
    thread,
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{Duration, timeout},
};

#[tokio::test]
async fn server_handshake_and_session_start() {
    let workspace = tempfile::tempdir().unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv-server"))
        .isolated(config_home.path())
        .arg("--stdio")
        .env("OPENAI_API_KEY", "test-only")
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
                    "{{\"type\":\"initialize\",\"request_id\":\"1\",\"protocol_version\":{},\"client\":{{\"name\":\"test\",\"version\":\"0\"}}}}\n",
                    "{{\"type\":\"session.start\",\"request_id\":\"2\",\"cwd\":{:?}}}\n"
                ),
                PROTOCOL_VERSION,
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
        assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello \"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"from fake\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let workspace = tempfile::tempdir().unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv-server"))
        .isolated(config_home.path())
        .args([
            "--stdio",
            "--model",
            "fake-model",
            "--base-url",
            &format!("http://{address}/v1"),
        ])
        .env("OPENAI_API_KEY", "test-only")
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
            protocol_version: PROTOCOL_VERSION,
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
            provider: None,
            model: None,
            base_url: None,
            no_tools: None,
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
    let _usage = loop {
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

#[tokio::test]
async fn a_provider_stream_error_fails_the_turn_instead_of_completing_empty() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_json_body(&mut stream);
        let body = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"model is not available\"}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let workspace = tempfile::tempdir().unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv-server"))
        .isolated(config_home.path())
        .args([
            "--stdio",
            "--model",
            "fake-model",
            "--base-url",
            &format!("http://{address}/v1"),
        ])
        .env("OPENAI_API_KEY", "test-only")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    send(
        &mut input,
        &ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
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
            provider: None,
            model: None,
            base_url: None,
            no_tools: None,
        },
    )
    .await;
    assert!(matches!(
        next_event(&mut lines).await,
        ServerEvent::Initialized { .. }
    ));
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

    let (code, message) = loop {
        match next_event(&mut lines).await {
            ServerEvent::TurnFailed { code, message, .. } => break (code, message),
            ServerEvent::TurnCompleted { .. } => panic!("a provider error completed the turn"),
            ServerEvent::AssistantCompleted { content, .. } => {
                panic!("a provider error produced an assistant message {content:?}")
            }
            _ => {}
        }
    };
    assert_eq!(code, "provider_error");
    assert!(message.contains("model is not available"), "{message}");

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

#[tokio::test]
async fn tool_results_are_replayed_after_their_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = thread::spawn(move || {
        let responses = [
            concat!(
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\\\"README.md\\\"}\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"read it\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            ),
        ];
        let mut bodies = Vec::new();
        for body in responses {
            let (mut stream, _) = listener.accept().unwrap();
            bodies.push(read_json_body(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
        bodies
    });

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("README.md"), "fixture text").unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv-server"))
        .isolated(config_home.path())
        .args([
            "--stdio",
            "--model",
            "fake-model",
            "--base-url",
            &format!("http://{address}/v1"),
        ])
        .env("OPENAI_API_KEY", "test-only")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    send(
        &mut input,
        &ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
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
            provider: None,
            model: None,
            base_url: None,
            no_tools: None,
        },
    )
    .await;
    assert!(matches!(
        next_event(&mut lines).await,
        ServerEvent::Initialized { .. }
    ));
    let session_id = match next_event(&mut lines).await {
        ServerEvent::SessionStarted { session_id, .. } => session_id,
        event => panic!("expected session.started, received {event:?}"),
    };
    send(
        &mut input,
        &ClientMessage::TurnStart {
            request_id: "turn".into(),
            session_id,
            prompt: "read the README".into(),
        },
    )
    .await;
    loop {
        match next_event(&mut lines).await {
            ServerEvent::TurnCompleted { .. } => break,
            ServerEvent::TurnFailed { code, message, .. } => {
                panic!("turn failed with {code}: {message}")
            }
            _ => {}
        }
    }

    input.shutdown().await.unwrap();
    drop(input);
    let bodies = provider.join().unwrap();
    let replayed = bodies[1]["input"].as_array().unwrap();
    assert_eq!(replayed.len(), 3, "{replayed:?}");
    assert_eq!(
        replayed[1],
        serde_json::json!({
            "type":"function_call",
            "call_id":"call_1",
            "name":"read",
            "arguments":"{\"path\":\"README.md\"}"
        })
    );
    assert_eq!(replayed[2]["type"], "function_call_output");
    assert_eq!(replayed[2]["call_id"], "call_1");
    assert!(
        replayed[2]["output"]
            .as_str()
            .unwrap()
            .contains("fixture text")
    );
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

/// Reads one HTTP request and returns its JSON body.
fn read_json_body(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let head = String::from_utf8(request).unwrap();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("request has a Content-Length");
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
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

/// Allowlisted HTTPS fetches run without approval, other hosts ask first, and
/// a provider with hosted search is offered it beside the function tools.
#[tokio::test]
async fn web_fetch_is_auto_approved_only_for_allowlisted_https_hosts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = thread::spawn(move || {
        let call = |id: &str, url: &str| {
            let arguments = serde_json::json!({ "url": url }).to_string();
            format!(
                "data: {}\n\ndata: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{}}}}\n\n",
                serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":arguments}),
                serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":id,"name":"web_fetch"}}),
            )
        };
        let responses = [
            call("call_1", "https://localhost:9/docs"),
            call("call_2", "http://example.invalid/?q=secret"),
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n".to_owned(),
        ];
        let mut bodies = Vec::new();
        for body in responses {
            let (mut stream, _) = listener.accept().unwrap();
            bodies.push(read_json_body(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
        bodies
    });

    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[provider]\nactive = \"fake\"\n\n[providers.fake]\nkind = \"openai-compatible\"\nmodel = \"fake-model\"\nbase_url = \"http://{address}/v1\"\napi_key = \"test-only\"\n\n[web]\nauto_approve_domains = [\"localhost\"]\nsearch = \"provider\"\n"
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv-server"))
        .isolated(home.path())
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    send(
        &mut input,
        &ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
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
            provider: None,
            model: None,
            base_url: None,
            no_tools: None,
        },
    )
    .await;
    assert!(matches!(
        next_event(&mut lines).await,
        ServerEvent::Initialized { .. }
    ));
    let session_id = match next_event(&mut lines).await {
        ServerEvent::SessionStarted { session_id, .. } => session_id,
        event => panic!("expected session.started, received {event:?}"),
    };
    send(
        &mut input,
        &ClientMessage::TurnStart {
            request_id: "turn".into(),
            session_id: session_id.clone(),
            prompt: "read the docs".into(),
        },
    )
    .await;
    let mut approvals = Vec::new();
    let mut completed = Vec::new();
    loop {
        match next_event(&mut lines).await {
            ServerEvent::ApprovalRequested {
                approval_id,
                call_id,
                name,
                risk,
                summary,
                ..
            } => {
                approvals.push((call_id, name, risk, summary));
                send(
                    &mut input,
                    &ClientMessage::ApprovalResolve {
                        request_id: "deny".into(),
                        session_id: session_id.clone(),
                        approval_id,
                        approved: false,
                    },
                )
                .await;
            }
            ServerEvent::ToolCompleted {
                call_id,
                success,
                output,
                ..
            } => completed.push((call_id, success, output)),
            ServerEvent::TurnCompleted { .. } => break,
            ServerEvent::TurnFailed { code, message, .. } => {
                panic!("turn failed with {code}: {message}")
            }
            _ => {}
        }
    }
    input.shutdown().await.unwrap();
    drop(input);
    let bodies = provider.join().unwrap();

    // The allowlisted HTTPS host ran without asking; the address check then
    // refused it, since localhost is not public.
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    let (call_id, name, risk, summary) = &approvals[0];
    assert_eq!(
        (call_id.as_str(), name.as_str(), risk.as_str()),
        ("call_2", "web_fetch", "network")
    );
    assert!(summary.contains("example.invalid"), "{summary}");
    assert_eq!(completed.len(), 2, "{completed:?}");
    assert_eq!(completed[0].0, "call_1");
    assert!(!completed[0].1);
    assert!(completed[0].2.contains("non-public"), "{}", completed[0].2);
    assert_eq!(completed[1].0, "call_2");
    assert!(completed[1].2.contains("denied"), "{}", completed[1].2);

    let tools = bodies[0]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "web_fetch"));
    assert!(tools.contains(&serde_json::json!({"type":"web_search"})));
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

/// Tool-free sessions, such as ClawBot senders without remote tools, get
/// neither web tools nor the provider's hosted search.
#[tokio::test]
async fn tool_free_sessions_get_no_web_access() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let body = read_json_body(&mut stream);
        let events = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
            events.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        body
    });
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[provider]\nactive = \"fake\"\n\n[providers.fake]\nkind = \"openai-compatible\"\nmodel = \"fake-model\"\nbase_url = \"http://{address}/v1\"\napi_key = \"test-only\"\n\n[web]\nsearch = \"provider\"\n"
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv-server"))
        .isolated(home.path())
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    send(
        &mut input,
        &ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
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
            provider: None,
            model: None,
            base_url: None,
            no_tools: Some(true),
        },
    )
    .await;
    next_event(&mut lines).await;
    let session_id = match next_event(&mut lines).await {
        ServerEvent::SessionStarted { session_id, .. } => session_id,
        event => panic!("expected session.started, received {event:?}"),
    };
    send(
        &mut input,
        &ClientMessage::TurnStart {
            request_id: "turn".into(),
            session_id,
            prompt: "search the web".into(),
        },
    )
    .await;
    loop {
        match next_event(&mut lines).await {
            ServerEvent::TurnCompleted { .. } => break,
            ServerEvent::TurnFailed { code, message, .. } => {
                panic!("turn failed with {code}: {message}")
            }
            _ => {}
        }
    }
    input.shutdown().await.unwrap();
    drop(input);
    let body = provider.join().unwrap();
    assert!(body.get("tools").is_none(), "{body}");
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
