//! The `scv` agent end to end: a real `scv server --stdio` delegates to a
//! real nested SCV over the SCV protocol. The nested SCV's `bash` approval comes
//! back to the parent's client, its events arrive as progress, a second turn
//! continues the same nested session, its record shows it at work during a
//! turn and idle between turns (unless a background job of its own still
//! runs or awaits its report), and nothing outlives the parent.

use crate::support::{
    Isolated, alive, call, read_http_request, sse_response, tagged, text, write_private,
};
use std::{
    io::{BufReader as StdBufReader, Write as _},
    net::{SocketAddr, TcpListener},
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::Stdio,
    thread,
    time::Duration,
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};
use scv_tools::delegation::{DelegationEntry, DelegationRegistry};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
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
                        "agent",
                        json!({"agent":"scv","prompt":"CHILD-ONE remember the word heron"}),
                    ),
                    // The handle alone names the agent it continues.
                    3 => call(
                        "call_1",
                        "agent",
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
    // What `scv agents import scv` would write: the nested SCV's own provider.
    write_private(
        &home_path.join("agents/scv/config.toml"),
        &format!(
            "[provider]\nactive = \"t\"\n\n[providers.t]\nkind = \"openai-compatible\"\nmodel = \"child-model\"\nbase_url = \"http://{address}/v1\"\napi_key = \"test-only\"\n"
        ),
    );
    let registry = DelegationRegistry::new(&scv_client::Layout::new(&home_path));
    let mut parent = Parent::start(&home_path, address, workspace.path()).await;

    let mut turns = Vec::new();
    for (index, prompt) in ["first", "second"].into_iter().enumerate() {
        parent.turn_start(&format!("turn-{index}"), prompt).await;
        let mut turn = Turn::default();
        loop {
            match parent.event().await {
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
                    let approved = name == "agent" || approve_nested;
                    turn.approvals.push((name, summary));
                    parent.resolve(approval_id, approved).await;
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
    assert_eq!(first.approvals[0].0, "agent");
    assert!(
        first.approvals[0].1.starts_with("agent scv: Send prompt"),
        "{}",
        first.approvals[0].1
    );
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

    parent.stop(child, &handle).await;
}

/// The parent: a real `scv server --stdio` with one session, as a client
/// drives it.
struct Parent {
    server: Child,
    input: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    session_id: String,
}

impl Parent {
    /// Start the parent in `home`, whose `scv` agent runs this build's `scv`.
    async fn start(home: &Path, address: SocketAddr, workspace: &Path) -> Self {
        write_private(
            &home.join("config.toml"),
            &format!("[agents.scv]\ncommand = {:?}\n", env!("CARGO_BIN_EXE_scv")),
        );
        let mut server = Command::new(env!("CARGO_BIN_EXE_scv"))
            .isolated(home)
            .arg("--scv-home")
            .arg(home)
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
        let mut parent = Self {
            input: server.stdin.take().unwrap(),
            lines: BufReader::new(server.stdout.take().unwrap()).lines(),
            server,
            session_id: String::new(),
        };
        parent
            .send(ClientMessage::Initialize {
                request_id: "init".into(),
                protocol_version: PROTOCOL_VERSION,
                client: PeerInfo {
                    name: "agent-scv-test".into(),
                    version: "0".into(),
                },
            })
            .await;
        parent
            .send(ClientMessage::SessionStart {
                request_id: "session".into(),
                cwd: workspace.display().to_string(),
                provider: None,
                model: None,
                base_url: None,
                no_tools: None,
                delegation_depth: None,
                channel: None,
                auto_approve: None,
            })
            .await;
        parent.session_id = loop {
            if let ServerEvent::SessionStarted { session_id, .. } = parent.event().await {
                break session_id;
            }
        };
        parent
    }

    async fn send(&mut self, message: ClientMessage) {
        self.input
            .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    async fn event(&mut self) -> ServerEvent {
        let line = timeout(Duration::from_secs(60), self.lines.next_line())
            .await
            .expect("server went quiet")
            .unwrap()
            .expect("server exited");
        serde_json::from_str(&line).unwrap()
    }

    async fn turn_start(&mut self, request_id: &str, prompt: &str) {
        self.send(ClientMessage::TurnStart {
            request_id: request_id.into(),
            session_id: self.session_id.clone(),
            prompt: prompt.into(),
            attachments: Vec::new(),
        })
        .await;
    }

    async fn resolve(&mut self, approval_id: String, approved: bool) {
        self.send(ClientMessage::ApprovalResolve {
            request_id: "approve".into(),
            session_id: self.session_id.clone(),
            approval_id,
            approved,
        })
        .await;
    }

    /// The parent ends: its nested SCV `child`, recorded as `handle`, and
    /// everything tagged with it go too.
    async fn stop(mut self, child: u32, handle: &str) {
        drop(self.input);
        timeout(Duration::from_secs(20), self.server.wait())
            .await
            .expect("the parent did not exit")
            .unwrap();
        for _ in 0..100 {
            if !alive(child) && live_tagged(handle).is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "the nested SCV {child} or its processes {:?} outlived the parent",
            live_tagged(handle)
        );
    }
}

#[tokio::test]
async fn a_nested_scv_serves_a_conversation_and_relays_approvals() {
    delegate(true).await;
}

#[tokio::test]
async fn a_denied_nested_approval_reaches_the_nested_agent() {
    delegate(false).await;
}

/// One provider for both SCVs. The parent delegates once; the nested SCV
/// starts a Codex job in the background, ends its turn, and later answers
/// the job's report in a turn of its own.
fn serve_background_provider(listener: TcpListener) {
    thread::spawn(move || {
        let mut parent_step = 0;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let body = read_http_request(&mut StdBufReader::new(stream.try_clone().unwrap()));
            let request: Value = serde_json::from_slice(&body).unwrap();
            let body = String::from_utf8_lossy(&body);
            let response = if request["model"] == "child-model" {
                if body.contains("Delegated work you started in the background has finished") {
                    text("job-1 landed")
                } else if body.contains("function_call_output") {
                    text("started job-1")
                } else {
                    call(
                        "call_1",
                        "agent",
                        json!({"agent":"codex","prompt":"land it","background":true}),
                    )
                }
            } else {
                parent_step += 1;
                if parent_step == 1 {
                    call(
                        "call_1",
                        "agent",
                        json!({"agent":"scv","prompt":"land it in the background"}),
                    )
                } else {
                    text("parent done")
                }
            };
            stream
                .write_all(sse_response(&response).as_bytes())
                .unwrap();
        }
    });
}

#[tokio::test]
async fn a_nested_scvs_own_background_job_keeps_it_at_work_between_turns() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    serve_background_provider(listener);

    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // The nested SCV's Codex works until the test lets it finish.
    let finish = home_path.join("codex-may-finish");
    let codex = home_path.join("fake-codex");
    std::fs::write(
        &codex,
        format!(
            concat!(
                "#!/bin/sh\n",
                "echo '{{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}}'\n",
                "i=0\n",
                "while [ ! -e {finish:?} ] && [ $i -lt 1200 ]; do sleep 0.1; i=$((i+1)); done\n",
                "echo '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"landed 0.9.9\"}}}}'\n",
                "echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
            ),
            finish = finish.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &home_path.join("agents/scv/config.toml"),
        &format!(
            "[provider]\nactive = \"t\"\n\n[providers.t]\nkind = \"openai-compatible\"\nmodel = \"child-model\"\nbase_url = \"http://{address}/v1\"\napi_key = \"test-only\"\n\n[agents.codex]\ncommand = {:?}\ntransport = \"resume\"\n",
            codex.display().to_string()
        ),
    );
    let registry = DelegationRegistry::new(&scv_client::Layout::new(&home_path));
    let mut parent = Parent::start(&home_path, address, workspace.path()).await;

    parent.turn_start("turn-0", "first").await;
    let mut output = None;
    loop {
        match parent.event().await {
            // The parent's call to scv and the nested call to codex.
            ServerEvent::ApprovalRequested { approval_id, .. } => {
                parent.resolve(approval_id, true).await;
            }
            ServerEvent::ToolCompleted {
                output: completed, ..
            } => output = Some(completed),
            ServerEvent::TurnCompleted { .. } => break,
            ServerEvent::TurnFailed { message, .. } | ServerEvent::Error { message, .. } => {
                panic!("turn failed: {message}")
            }
            _ => {}
        }
    }
    let output: Value = serde_json::from_str(&output.unwrap()).unwrap();
    assert_eq!(output["reply"], "started job-1", "{output}");

    // The nested SCV's turn has ended, but the job it started runs on inside
    // it: it is still at work, which holds a planned restart.
    let run = nested_run(&registry);
    assert!(run.record.idle_since_unix.is_some(), "{run:?}");
    assert_eq!(run.record.background_jobs, Some(1), "{run:?}");
    assert!(run.working());

    // The job finishes and the nested SCV reports it to its model in a turn
    // of its own; once that ends, the nested SCV is idle.
    std::fs::write(&finish, "").unwrap();
    for _ in 0..600 {
        if !nested_run(&registry).working() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let run = nested_run(&registry);
    assert!(!run.working(), "{run:?}");
    assert_eq!(run.record.background_jobs, None);
    assert!(run.processes > 0);

    let (child, handle) = delegation_pid(&home_path).expect("the nested SCV ended");
    parent.stop(child, &handle).await;
}

/// Tagged processes that are still running.
fn live_tagged(handle: &str) -> Vec<u32> {
    tagged(handle)
        .into_iter()
        .filter(|pid| alive(*pid))
        .collect()
}
