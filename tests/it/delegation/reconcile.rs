//! Delegated runs outlive a killed SCV process only until the next reconcile.
use crate::support::{Isolated, read_http_request, sse_response, tagged, write_private};
use std::{
    io::Write as _,
    net::TcpListener,
    os::unix::fs::PermissionsExt as _,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};
use scv_tools::delegation::{DelegationRegistry, ProcessIdentity};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

/// Processes whose environment tags them with `handle`.
#[tokio::test]
async fn a_killed_scv_process_leaves_nothing_after_the_next_reconcile() {
    // A provider that asks for one agent_claude call.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let body = concat!(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"prompt\\\":\\\"work\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"agent_claude\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
        );
        let (stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream);
        read_http_request(&mut reader);
        reader
            .get_mut()
            .write_all(sse_response(body).as_bytes())
            .unwrap();
    });

    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // A fake agent that starts a detached descendant and keeps running.
    let agent = home_path.join("fake-claude");
    std::fs::write(&agent, "#!/bin/sh\nsetsid sleep 120 &\nexec sleep 120\n").unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &home_path.join("config.toml"),
        &format!(
            "[agents.claude]\ncommand = {:?}\n",
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
            name: "delegation-test".into(),
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
        channel: None,
        auto_approve: None,
    })
    .await;
    let mut session_id = None;
    let mut approved = false;
    let registry = DelegationRegistry::new(&home_path);
    let mut record = None;
    // Poll for events and the record under one generous deadline: a busy
    // machine may take a while to start the server and the agent.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(Ok(Some(line))) = timeout(Duration::from_millis(25), lines.next_line()).await {
            match serde_json::from_str::<ServerEvent>(&line).unwrap() {
                ServerEvent::SessionStarted { session_id: id, .. } => {
                    send(ClientMessage::TurnStart {
                        request_id: "turn".into(),
                        session_id: id.clone(),
                        prompt: "delegate".into(),
                        attachments: Vec::new(),
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
                    approved = true;
                }
                ServerEvent::Error { message, .. } => panic!("server error: {message}"),
                _ => {}
            }
        }
        // `scv agents ps` view: the run is listed while its owner lives.
        if let [entry] = registry.list(false).as_slice()
            && tagged(&entry.record.handle).len() >= 2
        {
            record = Some(entry.record.clone());
            break;
        }
    }
    assert!(approved, "the delegation was never approved");
    let record = record.expect("the delegation was never recorded");
    assert_eq!(record.agent, "claude");
    assert_eq!(record.depth, 1);
    assert_eq!(Some(record.session), session_id);
    let owner = ProcessIdentity::of(server.id().unwrap()).unwrap();
    assert_eq!(record.owner, owner);

    // SIGKILL the owning SCV process: it cannot clean up after itself.
    server.kill().await.unwrap();
    assert!(record.process.is_alive(), "the agent outlived its owner");
    let orphans = registry.list(true);
    assert_eq!(orphans.len(), 1);
    assert!(orphans[0].orphaned);

    let report = registry.reconcile().await;
    assert_eq!(report.reaped, vec![record.handle.clone()]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !tagged(&record.handle).is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        tagged(&record.handle).is_empty(),
        "tagged processes survived"
    );
    assert!(!record.process.is_alive());
    assert!(registry.list(true).is_empty());
    assert!(
        std::fs::read_dir(registry.record_dir())
            .unwrap()
            .next()
            .is_none(),
        "the record was removed"
    );
}
