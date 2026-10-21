//! The workbench end to end: the owner's own jobs started and followed from
//! the command line, jobs a daemon crash interrupted resumed through the
//! agent's own conversation, and a TUI session's queued prompts kept across
//! a daemon that was killed.

use std::{os::unix::fs::PermissionsExt as _, path::Path, process::Stdio, time::Duration};

use scv_protocol::{
    AgentState, ClientMessage, DaemonCommand, PROTOCOL_VERSION, PeerInfo, ServerEvent,
    WorkbenchSnapshot,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
};

use crate::{
    daemon::{start, status, terminate},
    support::{Isolated, write_private},
};

/// A fake Codex that records its arguments. A first turn reports its
/// thread and then works for a long while (or briefly with `quick` in the
/// prompt); a resumed one answers at once.
fn fake_codex(home: &Path) -> std::path::PathBuf {
    let agent = home.join("fake-codex");
    std::fs::write(
        &agent,
        format!(
            concat!(
                "#!/bin/sh\n",
                "echo \"$@\" >> {log:?}\n",
                "case \"$*\" in\n",
                "  *\" resume \"*)\n",
                "    echo '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"i2\",\"type\":\"agent_message\",\"text\":\"resumed where it stopped\"}}}}'\n",
                "    echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
                "    ;;\n",
                "  *quick*)\n",
                "    echo '{{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}}'\n",
                "    echo '{{\"type\":\"item.started\",\"item\":{{\"type\":\"command_execution\",\"command\":\"cargo test\"}}}}'\n",
                "    echo '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"landed 0.9.9\"}}}}'\n",
                "    echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
                "    ;;\n",
                "  *)\n",
                "    echo '{{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a54\"}}'\n",
                "    sleep 30\n",
                "    ;;\n",
                "esac\n",
            ),
            log = home.join("codex-calls").display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    agent
}

/// An instance home whose only Codex is [`fake_codex`], once per turn.
fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(home.path()).unwrap();
    let agent = fake_codex(&path);
    write_private(
        &path.join("config.toml"),
        &format!(
            "[agents.codex]\ncommand = {:?}\ntransport = \"resume\"\n",
            agent.display().to_string()
        ),
    );
    (home, path)
}

fn scv(home: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scv"));
    command
        .isolated(home)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("OPENAI_API_KEY", "test-only");
    command
}

async fn snapshot(home: &Path) -> WorkbenchSnapshot {
    scv_client::control(
        &home.join("state/server.sock"),
        DaemonCommand::Workbench { watch: false },
    )
    .await
    .unwrap()
    .workbench
    .unwrap()
}

/// Wait until the workbench shows a job that `matches`.
async fn until_job(
    home: &Path,
    matches: impl Fn(&scv_protocol::WorkbenchJob) -> bool,
) -> scv_protocol::WorkbenchJob {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(job) = snapshot(home).await.jobs.into_iter().find(&matches) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap()
}

async fn kill_hard(child: &mut Child) {
    let killed = Command::new("kill")
        .args(["-KILL", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(killed.success());
    let _ = child.wait().await;
}

#[tokio::test]
async fn the_owners_job_is_started_followed_and_read_from_the_command_line() {
    let (_home, home) = home();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let started = scv(&home, &workspace)
        .args(["jobs", "start", "--agent", "codex", "a quick fix"])
        .output()
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&started.stdout);
    assert!(
        started.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(said.starts_with("Started workbench-"), "{said}");
    let job = until_job(&home, |job| job.state == AgentState::Completed).await;
    assert_eq!(job.started_by, scv_protocol::JobOrigin::Owner);
    assert_eq!(job.task, "a quick fix");
    let listed = scv(&home, &workspace)
        .args(["jobs", "list", "--all"])
        .output()
        .await
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("completed") && listed.contains("you"),
        "{listed}"
    );
    let log = scv(&home, &workspace)
        .args(["jobs", "log", &job.job])
        .output()
        .await
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("  ● $ cargo test"), "{log}");
    assert!(log.contains("landed 0.9.9"), "{log}");
    assert!(log.contains("turn completed"), "{log}");
    terminate(&mut daemon).await;
    // The records outlive the daemon: listing reads them from disk.
    let offline = scv(&home, &workspace)
        .args(["jobs", "list", "--all"])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&offline.stdout).contains("a quick fix"));
}

#[tokio::test]
async fn a_job_a_crash_interrupted_resumes_its_agents_own_conversation() {
    let (_home, home) = home();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let started = scv(&home, &workspace)
        .args(["jobs", "start", "--agent", "codex", "the long task"])
        .output()
        .await
        .unwrap();
    assert!(started.status.success());
    // Working, with the agent's thread known.
    until_job(&home, |job| {
        job.state == AgentState::Working && job.run.is_some()
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let states = scv_tools::activity::list_states(&home.join("state/runs"));
            if states.iter().any(|state| state.vendor.is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    kill_hard(&mut daemon).await;

    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let interrupted = until_job(&home, |job| {
        job.state == AgentState::Interrupted && job.resumable
    })
    .await;
    assert_eq!(interrupted.task, "the long task");
    let resumed = scv(&home, &workspace)
        .args(["jobs", "resume", &interrupted.job])
        .output()
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&resumed.stdout);
    assert!(
        resumed.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(said.starts_with("Resumed"), "{said}");
    let done = until_job(&home, |job| {
        job.state == AgentState::Completed
            && (job.session != interrupted.session || job.job != interrupted.job)
    })
    .await;
    assert_eq!(done.started_by, scv_protocol::JobOrigin::Owner);
    let calls = std::fs::read_to_string(home.join("codex-calls")).unwrap();
    let resume = calls
        .lines()
        .find(|line| line.starts_with("exec resume"))
        .unwrap();
    assert!(
        resume.contains("0199a213-81c0-7800-8aa1-bbab2a035a54")
            && resume.contains("[SCV] SCV stopped before your last turn"),
        "{calls}"
    );
    let old = snapshot(&home)
        .await
        .jobs
        .into_iter()
        .find(|job| job.session == interrupted.session && job.job == interrupted.job)
        .unwrap();
    assert!(old.resumed_as.is_some() && !old.resumable);
    terminate(&mut daemon).await;
}

/// A connection with a session that keeps its queue, restoring `restore`.
async fn session(
    home: &Path,
    workspace: &Path,
    restore: Option<String>,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    ServerEvent,
) {
    let stream = UnixStream::connect(home.join("state/server.sock"))
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    for message in [
        ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
            client: PeerInfo {
                name: "scv-tui".into(),
                version: "0".into(),
            },
        },
        ClientMessage::SessionStart {
            request_id: "start".into(),
            cwd: workspace.display().to_string(),
            provider: None,
            model: None,
            base_url: None,
            no_tools: Some(true),
            delegation_depth: None,
            channel: None,
            auto_approve: None,
            chat: None,
            system_prompt: None,
            persist_queue: Some(true),
            restore,
        },
    ] {
        send(&mut write, &message).await;
        let event = next(&mut reader).await;
        if matches!(event, ServerEvent::SessionStarted { .. }) {
            return (reader, write, event);
        }
    }
    unreachable!()
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, message: &ClientMessage) {
    write
        .write_all(&serde_json::to_vec(message).unwrap())
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
}

async fn next(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> ServerEvent {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn a_killed_daemon_leaves_the_queue_for_the_next_session_to_restore_paused() {
    // A provider that takes requests and never answers, so a turn stays
    // running until the daemon dies.
    let provider = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = provider.local_addr().unwrap();
    let _held = std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in provider.incoming() {
            held.push(stream);
        }
    });
    let home = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    write_private(
        &home.join("config.toml"),
        &format!("[provider]\nmodel = \"m\"\nbase_url = \"http://{address}/v1\"\n"),
    );
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let (mut reader, mut write, started) = session(&home, &workspace, None).await;
    let ServerEvent::SessionStarted { session_id, .. } = started else {
        unreachable!()
    };
    for (request, prompt) in [("1", "the running one"), ("2", "the queued one")] {
        send(
            &mut write,
            &ClientMessage::TurnStart {
                request_id: request.into(),
                session_id: session_id.clone(),
                prompt: prompt.into(),
                attachments: Vec::new(),
            },
        )
        .await;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(next(&mut reader).await, ServerEvent::QueueEnqueued { .. }) {}
    })
    .await
    .unwrap();
    kill_hard(&mut daemon).await;

    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let restorable = snapshot(&home).await.restorable;
    assert_eq!(restorable.len(), 1);
    assert_eq!(restorable[0].id, session_id);
    let (mut reader, _write, started) = session(&home, &workspace, Some(session_id.clone())).await;
    let ServerEvent::SessionStarted { restored, .. } = started else {
        unreachable!()
    };
    let restored = restored.unwrap();
    assert_eq!(restored.from, session_id);
    assert_eq!(restored.queued, 1);
    assert_eq!(restored.interrupted.as_deref(), Some("the running one"));
    let ServerEvent::QueueSnapshot {
        entries, paused, ..
    } = next(&mut reader).await
    else {
        panic!("no queue snapshot");
    };
    assert!(paused);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].prompt, "the queued one");
    // It was restored once: nothing is left to restore.
    assert_eq!(snapshot(&home).await.restorable, []);
    terminate(&mut daemon).await;
}
