//! Delegated runs outlive a killed SCV process only until the next reconcile.
#![cfg(target_os = "linux")]

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
use scv_server::delegation::{DelegationRegistry, ProcessIdentity};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

fn write_private(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Processes whose environment tags them with `handle`.
fn tagged(handle: &str) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let environ = std::fs::read(entry.path().join("environ")).ok()?;
            environ
                .split(|byte| *byte == 0)
                .filter_map(|entry| entry.strip_prefix(b"SCV_PARENT="))
                .any(|chain| {
                    String::from_utf8_lossy(chain)
                        .split(';')
                        .any(|entry| entry.rsplit('/').next() == Some(handle))
                })
                .then_some(pid)
        })
        .collect()
}

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
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 65536];
        let _ = stream.read(&mut request);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
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
    })
    .await;
    let mut session_id = None;
    let mut approved = false;
    let registry = DelegationRegistry::new(&home_path);
    let mut record = None;
    for _ in 0..400 {
        if let Ok(Ok(Some(line))) = timeout(Duration::from_millis(25), lines.next_line()).await {
            match serde_json::from_str::<ServerEvent>(&line).unwrap() {
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
    for _ in 0..100 {
        if tagged(&record.handle).is_empty() {
            break;
        }
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
