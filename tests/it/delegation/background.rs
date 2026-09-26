//! Background delegations: an `agent_*` call with `background: true` returns
//! a job at once, and when the job finishes the server reports it in a turn
//! of its own unless the model already waited for it.

use crate::support::{Isolated, call, read_http_request, sse_response, text, write_private};
use serde_json::{Value, json};
use std::{
    io::{BufReader as StdBufReader, Write as _},
    net::TcpListener,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::Stdio,
    sync::mpsc,
    thread,
    time::Duration,
};

use scv_protocol::{ClientMessage, OriginKind, PROTOCOL_VERSION, PeerInfo, ServerEvent};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

/// A provider answering each request with the next body, passing on every
/// request body it received.
fn serve_provider(listener: TcpListener, bodies: Vec<String>) -> mpsc::Receiver<String> {
    let (requests, received) = mpsc::channel();
    thread::spawn(move || {
        for body in bodies {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = StdBufReader::new(stream);
            let request = read_http_request(&mut reader);
            let _ = requests.send(String::from_utf8_lossy(&request).into_owned());
            reader
                .get_mut()
                .write_all(sse_response(&body).as_bytes())
                .unwrap();
        }
    });
    received
}

/// An SCV home whose Codex is a script that works for about a second.
fn home_with_fake_codex() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let agent = home_path.join("fake-codex");
    std::fs::write(
        &agent,
        concat!(
            "#!/bin/sh\n",
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}'\n",
            "sleep 1\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"landed 0.9.9\"}}'\n",
            "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &home_path.join("config.toml"),
        &format!(
            "[agents.codex]\ncommand = {:?}\ntransport = \"resume\"\n",
            agent.display().to_string()
        ),
    );
    (home, home_path)
}

struct Server {
    _child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    session: String,
}

impl Server {
    async fn start(home: &Path, address: std::net::SocketAddr, workspace: &Path) -> Self {
        Self::start_on(home, address, workspace, None).await
    }

    /// A session a chat bridge would start for `channel`.
    async fn start_on(
        home: &Path,
        address: std::net::SocketAddr,
        workspace: &Path,
        channel: Option<&str>,
    ) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_scv"))
            .isolated(home)
            .arg("--scv-home")
            .arg(home)
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
        let input = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut server = Self {
            _child: child,
            input,
            lines,
            session: String::new(),
        };
        server
            .send(ClientMessage::Initialize {
                request_id: "init".into(),
                protocol_version: PROTOCOL_VERSION,
                client: PeerInfo {
                    name: "background-test".into(),
                    version: "0".into(),
                },
            })
            .await;
        server
            .send(ClientMessage::SessionStart {
                request_id: "session".into(),
                cwd: workspace.display().to_string(),
                provider: None,
                model: None,
                base_url: None,
                no_tools: None,
                delegation_depth: None,
                channel: channel.map(str::to_owned),
                auto_approve: channel.map(|_| true),
            })
            .await;
        loop {
            if let ServerEvent::SessionStarted { session_id, .. } = server.next().await.unwrap() {
                server.session = session_id;
                return server;
            }
        }
    }

    async fn send(&mut self, message: ClientMessage) {
        self.input
            .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    /// The next event, approving every approval request on the way.
    async fn next(&mut self) -> Option<ServerEvent> {
        let line = timeout(Duration::from_secs(30), self.lines.next_line())
            .await
            .ok()?
            .unwrap()?;
        let event: ServerEvent = serde_json::from_str(&line).unwrap();
        if let ServerEvent::ApprovalRequested { approval_id, .. } = &event {
            let approval = ClientMessage::ApprovalResolve {
                request_id: "approve".into(),
                session_id: self.session.clone(),
                approval_id: approval_id.clone(),
                approved: true,
            };
            self.send(approval).await;
        }
        Some(event)
    }

    async fn turn(&mut self, prompt: &str) {
        let session_id = self.session.clone();
        self.send(ClientMessage::TurnStart {
            request_id: "turn".into(),
            session_id,
            prompt: prompt.into(),
            attachments: Vec::new(),
        })
        .await;
    }
}

#[tokio::test]
async fn a_finished_background_job_is_reported_in_a_turn_the_server_starts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = serve_provider(
        listener,
        vec![
            call(
                "call_1",
                "agent_codex",
                json!({"prompt":"land it","background":true}),
            ),
            text("Started job-1."),
            text("job-1 landed 0.9.9."),
        ],
    );
    let (_home, home) = home_with_fake_codex();
    let workspace = tempfile::tempdir().unwrap();
    let mut server = Server::start(&home, address, workspace.path()).await;
    server.turn("land it in the background").await;

    let mut started_output = None;
    let mut own_completed = false;
    let mut report_origin = None;
    let mut report_text = String::new();
    loop {
        let event = server.next().await.expect("server went quiet");
        match event {
            ServerEvent::ToolCompleted { name, output, .. } if name == "agent_codex" => {
                started_output = Some(output);
            }
            ServerEvent::TurnCompleted {
                request_id,
                origin: None,
                ..
            } => {
                assert_eq!(request_id, "turn");
                own_completed = true;
            }
            ServerEvent::TurnStarted {
                request_id,
                origin: Some(origin),
                ..
            } => {
                assert!(own_completed, "the report waits for the user's turn");
                assert!(request_id.starts_with("background:"), "{request_id}");
                report_origin = Some(origin);
            }
            ServerEvent::AssistantDelta { content, .. } if report_origin.is_some() => {
                report_text.push_str(&content);
            }
            ServerEvent::TurnCompleted {
                origin: Some(origin),
                ..
            } => {
                assert_eq!(Some(origin), report_origin);
                break;
            }
            ServerEvent::TurnFailed { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
    let started: Value = serde_json::from_str(&started_output.unwrap()).unwrap();
    assert_eq!(
        (
            started["job"].as_str(),
            started["status"].as_str(),
            started["background"].as_bool()
        ),
        (Some("job-1"), Some("running"), Some(true))
    );
    let origin = report_origin.unwrap();
    assert_eq!(origin.kind, OriginKind::Background);
    assert_eq!(origin.jobs, vec!["job-1".to_owned()]);
    assert_eq!(report_text, "job-1 landed 0.9.9.");
    // The model was told what finished: the job, its conversation, its reply.
    let bodies: Vec<String> = requests.try_iter().collect();
    assert_eq!(bodies.len(), 3);
    let report = &bodies[2];
    assert!(report.contains("[SCV background report]"), "{report}");
    assert!(
        report.contains("job-1 (agent_codex, conversation codex-1): completed"),
        "{report}"
    );
    assert!(report.contains("landed 0.9.9"), "{report}");
}

#[tokio::test]
async fn a_job_the_model_waited_for_is_not_reported_again() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = serve_provider(
        listener,
        vec![
            call(
                "call_1",
                "agent_codex",
                json!({"prompt":"land it","background":true}),
            ),
            call(
                "call_2",
                "agent_wait",
                json!({"job":"job-1","timeout_seconds":30}),
            ),
            text("It landed."),
        ],
    );
    let (_home, home) = home_with_fake_codex();
    let workspace = tempfile::tempdir().unwrap();
    let mut server = Server::start(&home, address, workspace.path()).await;
    server.turn("land it and wait").await;
    let mut waited = None;
    loop {
        match server.next().await.expect("server went quiet") {
            ServerEvent::ToolCompleted { name, output, .. } if name == "agent_wait" => {
                waited = Some(output);
            }
            ServerEvent::TurnCompleted { origin: None, .. } => break,
            ServerEvent::TurnFailed { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
    let waited: Value = serde_json::from_str(&waited.unwrap()).unwrap();
    assert_eq!(waited["status"], "completed");
    assert_eq!(waited["result"]["reply"], "landed 0.9.9");
    // No report turn follows: the model already has the result.
    let quiet = timeout(Duration::from_secs(3), async {
        loop {
            if let Some(ServerEvent::TurnStarted { .. }) = server.next().await {
                return;
            }
        }
    })
    .await;
    assert!(
        quiet.is_err(),
        "a report turn repeated a result the model had seen"
    );
    assert_eq!(requests.try_iter().count(), 3);
}

#[tokio::test]
async fn tool_calls_carry_the_jobs_they_start_and_settle() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let _requests = serve_provider(
        listener,
        vec![
            call(
                "call_1",
                "agent_codex",
                json!({"prompt":"Land the fix\nthen publish","background":true}),
            ),
            call(
                "call_2",
                "agent_wait",
                json!({"job":"job-1","timeout_seconds":30}),
            ),
            text("It landed."),
        ],
    );
    let (_home, home) = home_with_fake_codex();
    let workspace = tempfile::tempdir().unwrap();
    let mut server = Server::start(&home, address, workspace.path()).await;
    server.turn("land it and wait").await;
    let mut changes = Vec::new();
    loop {
        match server.next().await.expect("server went quiet") {
            ServerEvent::ToolCompleted { call_id, jobs, .. } => changes.push((call_id, jobs)),
            ServerEvent::TurnCompleted { origin: None, .. } => break,
            ServerEvent::TurnFailed { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
    let job = |status| scv_protocol::JobChange {
        job: "job-1".into(),
        tool: "agent_codex".into(),
        status,
        task: "Land the fix".into(),
    };
    assert_eq!(
        changes,
        [
            (
                "call_1".to_owned(),
                vec![job(scv_protocol::JobStatus::Running)]
            ),
            (
                "call_2".to_owned(),
                vec![job(scv_protocol::JobStatus::Completed)]
            ),
        ]
    );
}

#[tokio::test]
async fn scv_exec_stays_until_its_background_jobs_are_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let _requests = serve_provider(
        listener,
        vec![
            call(
                "call_1",
                "agent_codex",
                json!({"prompt":"land it","background":true}),
            ),
            text("Started job-1."),
            text("job-1 landed 0.9.9."),
        ],
    );
    let (_home, home) = home_with_fake_codex();
    let workspace = tempfile::tempdir().unwrap();
    let output = timeout(
        Duration::from_secs(60),
        Command::new(env!("CARGO_BIN_EXE_scv"))
            .isolated(&home)
            .current_dir(workspace.path())
            .arg("--scv-home")
            .arg(&home)
            .args([
                "--model",
                "fake-model",
                "--base-url",
                &format!("http://{address}/v1"),
                "exec",
                "--yes",
                "land it in the background",
            ])
            .env("OPENAI_API_KEY", "test-only")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("scv exec returned")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("Started job-1."), "{stdout}");
    assert!(stdout.contains("job-1 landed 0.9.9."), "{stdout}");
    assert!(
        stderr.contains("waiting for 1 background job(s)"),
        "{stderr}"
    );
    assert!(stderr.contains("[background report: job-1]"), "{stderr}");
}

#[tokio::test]
async fn a_chat_session_is_told_its_channel_and_to_delegate_in_the_background() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = serve_provider(listener, vec![text("Hi.")]);
    let (_home, home) = home_with_fake_codex();
    let workspace = tempfile::tempdir().unwrap();
    let mut server = Server::start_on(&home, address, workspace.path(), Some("WeChat")).await;
    server.turn("hello").await;
    loop {
        match server.next().await.expect("server went quiet") {
            ServerEvent::TurnCompleted { origin: None, .. } => break,
            ServerEvent::TurnFailed { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
    let body: Value = serde_json::from_str(&requests.recv().unwrap()).unwrap();
    let text = body.to_string();
    assert!(text.contains("takes place on WeChat"), "{text}");
    assert!(text.contains("# Delegating work"), "{text}");
    assert!(text.contains("agent_codex (Codex)"), "{text}");
    assert!(text.contains("background set to true"), "{text}");
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for tool in ["agent_codex", "agent_status", "agent_wait", "agent_cancel"] {
        assert!(tools.contains(&tool), "{tool} missing from {tools:?}");
    }
}
