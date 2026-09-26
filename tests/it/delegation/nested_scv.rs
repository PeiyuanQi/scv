//! `agent_scv` end to end: a real `scv server --stdio` delegates to a real
//! nested SCV over the SCV protocol. The nested SCV's `bash` approval comes
//! back to the parent's client, its events arrive as progress, a second turn
//! continues the same nested session, its record shows it at work during a
//! turn and idle between turns, and nothing outlives the parent.

use crate::support::{
    Isolated, alive, call, read_http_request, sse_response, tagged, text, write_private,
};
use std::{
    io::{BufReader as StdBufReader, Write as _},
    net::TcpListener,
    path::Path,
    process::Stdio,
    thread,
    time::Duration,
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};
use scv_tools::delegation::{DelegationEntry, DelegationRegistry};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

/// One provider for both SCVs, told apart by the model each asks for. The
/// parent delegates twice; the nested SCV runs `bash` on its first turn and
/// recalls the first turn's word on its second.
fn serve_provider(listener: TcpListener) {
    thread::spawn(move || {
        let mut parent_step = 0;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let body = read_http_request(&mut StdBufReader::new(stream.try_clone().unwrap()));
            let request: Value = serde_json::from_slice(&body).unwrap();
            let body = String::from_utf8_lossy(&body);
            let response = if request["model"] == "child-model" {
                if body.contains("CHILD-TWO") {
                    text(if body.contains("remember the word heron") {
                        "the word was heron"
                    } else {
                        "no memory of a word"
                    })
                } else if body.contains("function_call_output") {
                    text(if body.contains("denied by policy") {
                        "bash was denied"
                    } else {
                        "noted heron"
                    })
                } else {
                    call("call_1", "bash", json!({"command":"echo child-ran"}))
                }
            } else {
                parent_step += 1;
                match parent_step {
                    1 => call(
                        "call_1",
                        "agent_scv",
                        json!({"prompt":"CHILD-ONE remember the word heron"}),
                    ),
                    3 => call(
                        "call_1",
                        "agent_scv",
                        json!({"prompt":"CHILD-TWO which word?","session":"scv-1"}),
                    ),
                    _ => text("parent done"),
                }
            };
            stream
                .write_all(sse_response(&response).as_bytes())
                .unwrap();
        }
    });
}

/// What the parent's client saw during one turn.
#[derive(Default)]
struct Turn {
    tool_output: Option<String>,
    progress: Vec<String>,
    /// Approvals as (name, summary).
    approvals: Vec<(String, String)>,
}

fn delegation_pid(home: &Path) -> Option<(u32, String)> {
    let dir = home.join("state").join("delegations");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let record: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
        if record["agent"] == "scv" {
            return Some((
                u32::try_from(record["process"]["pid"].as_u64()?).ok()?,
                record["handle"].as_str()?.to_owned(),
            ));
        }
    }
    None
}

/// The nested SCV's run as `scv agents ps` lists it.
fn nested_run(registry: &DelegationRegistry) -> DelegationEntry {
    let mut entries = registry.list(false);
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.remove(0)
}

async fn delegate(approve_nested: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    serve_provider(listener);

    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    write_private(
        &home_path.join("config.toml"),
        &format!("[agents.scv]\ncommand = {:?}\n", env!("CARGO_BIN_EXE_scv")),
    );
    // What `scv agents import scv` would write: the nested SCV's own provider.
    write_private(
        &home_path.join("agents/scv/config.toml"),
        &format!(
            "[provider]\nactive = \"t\"\n\n[providers.t]\nkind = \"openai-compatible\"\nmodel = \"child-model\"\nbase_url = \"http://{address}/v1\"\napi_key = \"test-only\"\n"
        ),
    );
    let registry = DelegationRegistry::new(&scv_client::Layout::new(&home_path));

    let mut server = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(&home_path)
        .arg("--scv-home")
        .arg(&home_path)
        .args([
            "--model",
            "parent-model",
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
            name: "agent-scv-test".into(),
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
    let session_id = loop {
        let line = timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("server went quiet")
            .unwrap()
            .expect("server exited");
        if let ServerEvent::SessionStarted { session_id, .. } = serde_json::from_str(&line).unwrap()
        {
            break session_id;
        }
    };

    let mut turns = Vec::new();
    for (index, prompt) in ["first", "second"].into_iter().enumerate() {
        send(ClientMessage::TurnStart {
            request_id: format!("turn-{index}"),
            session_id: session_id.clone(),
            prompt: prompt.into(),
            attachments: Vec::new(),
        })
        .await;
        let mut turn = Turn::default();
        loop {
            let line = timeout(Duration::from_secs(60), lines.next_line())
                .await
                .expect("server went quiet")
                .unwrap()
                .expect("server exited");
            match serde_json::from_str(&line).unwrap() {
                ServerEvent::ApprovalRequested {
                    approval_id,
                    name,
                    summary,
                    ..
                } => {
                    if name == "bash" {
                        // The nested SCV asks mid-turn: it is at work, which
                        // holds a planned restart.
                        let run = nested_run(&registry);
                        assert!(run.working(), "{run:?}");
                        assert_eq!(run.record.turn, Some(1));
                    }
                    let approved = name == "agent_scv" || approve_nested;
                    turn.approvals.push((name, summary));
                    send(ClientMessage::ApprovalResolve {
                        request_id: "approve".into(),
                        session_id: session_id.clone(),
                        approval_id,
                        approved,
                    })
                    .await;
                }
                ServerEvent::ToolProgress { call_id, text, .. } => {
                    assert_eq!(call_id, "call_1");
                    turn.progress.push(text);
                }
                ServerEvent::ToolCompleted { output, .. } => turn.tool_output = Some(output),
                ServerEvent::TurnCompleted { .. } => break,
                ServerEvent::TurnFailed { message, .. } | ServerEvent::Error { message, .. } => {
                    panic!("turn failed: {message}")
                }
                _ => {}
            }
        }
        turns.push(turn);
        // Between turns the nested SCV lives on, idle: a planned restart
        // does not wait for it.
        let run = nested_run(&registry);
        assert!(run.processes > 0, "{run:?}");
        assert!(run.record.idle_since_unix.is_some(), "{run:?}");
        assert!(!run.working());
        assert_eq!(run.record.turn, Some(u32::try_from(index).unwrap() + 1));
    }
    let (child, handle) = delegation_pid(&home_path).expect("the nested SCV ended between turns");
    assert!(alive(child));

    // Turn one: the nested bash approval came back here, labelled.
    let first = &turns[0];
    assert_eq!(first.approvals[0].0, "agent_scv");
    let nested = &first.approvals[1];
    assert_eq!(nested.0, "bash");
    assert!(nested.1.starts_with("[scv-1 depth 1] "), "{}", nested.1);
    let output: Value = serde_json::from_str(first.tool_output.as_ref().unwrap()).unwrap();
    assert_eq!(output["agent"], "scv");
    assert_eq!(output["status"], "completed");
    assert_eq!(output["session"], "scv-1");
    assert_eq!(output["turn"], 1);
    let expected = if approve_nested {
        "noted heron"
    } else {
        "bash was denied"
    };
    assert_eq!(output["reply"], expected);
    let progress = first.progress.join("\n");
    assert!(progress.contains("bash"), "{progress}");
    assert!(progress.contains(expected), "{progress}");

    // Turn two continued the same nested session and needed no approval of
    // its own beyond the call itself.
    let second = &turns[1];
    assert_eq!(second.approvals.len(), 1);
    let output: Value = serde_json::from_str(second.tool_output.as_ref().unwrap()).unwrap();
    assert_eq!(output["reply"], "the word was heron");
    assert_eq!(output["turn"], 2);

    // The parent ends: its nested SCV and everything tagged with it go too.
    drop(input);
    timeout(Duration::from_secs(20), server.wait())
        .await
        .expect("the parent did not exit")
        .unwrap();
    for _ in 0..100 {
        if !alive(child) && live_tagged(&handle).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "the nested SCV {child} or its processes {:?} outlived the parent",
        live_tagged(&handle)
    );
}

#[tokio::test]
async fn a_nested_scv_serves_a_conversation_and_relays_approvals() {
    delegate(true).await;
}

#[tokio::test]
async fn a_denied_nested_approval_reaches_the_nested_agent() {
    delegate(false).await;
}

/// Tagged processes that are still running.
fn live_tagged(handle: &str) -> Vec<u32> {
    tagged(handle)
        .into_iter()
        .filter(|pid| alive(*pid))
        .collect()
}
