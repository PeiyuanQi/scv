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
    // Two restores of it at once (a tab reconnecting while `r` opens
    // another): only one gets the queue.
    let (first, second) = tokio::join!(
        session(&home, &workspace, Some(session_id.clone())),
        session(&home, &workspace, Some(session_id.clone())),
    );
    let restoring = |started: &ServerEvent| {
        matches!(
            started,
            ServerEvent::SessionStarted {
                restored: Some(_),
                ..
            }
        )
    };
    assert_ne!(restoring(&first.2), restoring(&second.2));
    let (mut reader, mut write, started) = if restoring(&first.2) { first } else { second };
    let ServerEvent::SessionStarted {
        restored,
        session_id: new_session,
        ..
    } = started
    else {
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
    // Resuming the idle session's queue runs the restored prompt at once.
    send(
        &mut write,
        &ClientMessage::SessionPause {
            request_id: "resume".into(),
            session_id: new_session,
            paused: false,
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(next(&mut reader).await, ServerEvent::QueueDequeued { .. }) {}
    })
    .await
    .unwrap();
    terminate(&mut daemon).await;
}

/// A fake Codex with full permissions whose `resume <thread>` (not `exec
/// resume`) is its interactive UI: it records its PID and any hangup, and
/// answers each line typed into its terminal. Its `exec` turns finish at
/// once on one thread.
fn takeover_home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(home.path()).unwrap();
    let agent = path.join("fake-codex");
    std::fs::write(
        &agent,
        format!(
            concat!(
                "#!/bin/sh\n",
                "case \"$1\" in\n",
                "  resume)\n",
                "    echo $$ > {pid:?}\n",
                "    trap 'echo hup >> {hup:?}; exit 0' HUP\n",
                "    echo \"codex terminal on $2\"\n",
                "    while IFS= read -r line; do echo \"typed: $line\"; done\n",
                "    ;;\n",
                "  *)\n",
                "    echo \"$@\" >> {calls:?}\n",
                "    echo '{{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a55\"}}'\n",
                "    echo '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"done\"}}}}'\n",
                "    echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
                "    ;;\n",
                "esac\n",
            ),
            pid = path.join("terminal-pid").display().to_string(),
            hup = path.join("terminal-hangups").display().to_string(),
            calls = path.join("codex-calls").display().to_string(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &path.join("config.toml"),
        &format!(
            "[agents.codex]\ncommand = {:?}\ntransport = \"resume\"\npermissions = \"full\"\n",
            agent.display().to_string()
        ),
    );
    (home, path)
}

async fn control(home: &Path, command: DaemonCommand) -> scv_protocol::DaemonStatus {
    scv_client::control(&home.join("state/server.sock"), command)
        .await
        .unwrap()
}

/// Job completion can precede the conversation becoming available for
/// takeover. Wait for that observable precondition, failing any other error.
async fn until_attached(
    home: &Path,
    job: &scv_protocol::WorkbenchJob,
    cols: Option<u16>,
    rows: Option<u16>,
) -> scv_protocol::AttachPage {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let result = scv_client::control(
                &home.join("state/server.sock"),
                DaemonCommand::Attach {
                    session: job.session.clone(),
                    conversation: job.conversation.clone().unwrap(),
                    cols,
                    rows,
                },
            )
            .await;
            match result {
                Ok(status) => return status.attach.unwrap(),
                Err(scv_client::ControlError::Server {
                    code: scv_protocol::ErrorCode::WorkbenchError,
                    message,
                }) if message.contains("attach once it is idle") => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("could not attach the idle conversation: {error}"),
            }
        }
    })
    .await
    .expect("the conversation becomes available for takeover")
}

fn screen_text(page: &scv_protocol::AttachPage) -> String {
    page.lines
        .iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read attached terminal `id` until its screen shows `wanted`.
async fn until_screen(
    home: &Path,
    id: &str,
    input: &str,
    wanted: &str,
) -> scv_protocol::AttachPage {
    let mut input = input.to_owned();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let page = control(
                home,
                DaemonCommand::AttachIo {
                    id: id.to_owned(),
                    after: 0,
                    input: std::mem::take(&mut input),
                    cols: None,
                    rows: None,
                },
            )
            .await
            .attach
            .unwrap();
            if screen_text(&page).contains(wanted) {
                return page;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the terminal never showed {wanted:?}"))
}

fn terminal_pid(home: &Path) -> i32 {
    std::fs::read_to_string(home.join("terminal-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether process `pid` runs: it exists and, where the test can tell, is
/// not a zombie.
fn running(pid: i32) -> bool {
    if cfg!(target_os = "linux") {
        return std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        });
    }
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn until_gone(pid: i32) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while running(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("terminal process {pid} is still running"));
}

#[tokio::test]
async fn the_owner_takes_a_conversation_over_in_its_agents_terminal_and_detaches_from_it() {
    let (_home, home) = takeover_home();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let started = scv(&home, &workspace)
        .args(["jobs", "start", "--agent", "codex", "a quick fix"])
        .output()
        .await
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    let job = until_job(&home, |job| {
        job.state == AgentState::Completed && job.conversation.is_some()
    })
    .await;
    let conversation = job.conversation.clone().unwrap();

    let attached = until_attached(&home, &job, Some(80), Some(24)).await;
    assert_eq!((attached.cols, attached.rows), (80, 24));
    let id = attached.id.clone();
    until_screen(
        &home,
        &id,
        "",
        "codex terminal on 0199a213-81c0-7800-8aa1-bbab2a035a55",
    )
    .await;
    until_screen(&home, &id, "hello\r", "typed: hello").await;
    let pid = terminal_pid(&home);

    // SCV's own turns wait while the owner has the conversation.
    let queued = control(
        &home,
        DaemonCommand::WorkbenchSend {
            session: job.session.clone(),
            conversation: conversation.clone(),
            prompt: "after the takeover".into(),
            mode: scv_protocol::SendMode::Queue,
        },
    )
    .await
    .workbench_job
    .unwrap();
    let (_, queued_job) = queued.rsplit_once('/').unwrap();
    until_job(&home, |job| {
        job.job == queued_job && job.state == AgentState::Queued
    })
    .await;
    // Cancelling a job queued behind the takeover leaves the terminal be.
    let doomed = control(
        &home,
        DaemonCommand::WorkbenchSend {
            session: job.session.clone(),
            conversation: conversation.clone(),
            prompt: "never mind".into(),
            mode: scv_protocol::SendMode::Queue,
        },
    )
    .await
    .workbench_job
    .unwrap();
    let (_, doomed) = doomed.rsplit_once('/').unwrap();
    until_job(&home, |job| {
        job.job == doomed && job.state == AgentState::Queued
    })
    .await;
    control(
        &home,
        DaemonCommand::WorkbenchCancel {
            session: job.session.clone(),
            job: doomed.to_owned(),
        },
    )
    .await;
    until_job(&home, |job| {
        job.job == doomed && job.state == AgentState::Cancelled
    })
    .await;
    until_screen(&home, &id, "still\r", "typed: still").await;
    assert!(running(pid));
    let taken = snapshot(&home).await;
    assert!(
        taken.runs.iter().any(|run| run.transport == "terminal"
            && run.live
            && run.conversation.as_deref() == Some(conversation.as_str())),
        "{taken:?}"
    );

    // Detached, the agent keeps running; attached again, its screen is there.
    let detached = control(&home, DaemonCommand::AttachDetach { id: id.clone() }).await;
    assert!(
        detached.workbench_note.unwrap().contains("keeps running"),
        "detach note"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(running(pid));
    let again = until_attached(&home, &job, Some(100), Some(30)).await;
    assert_ne!(again.id, id);
    assert!(screen_text(&again).contains("typed: hello"), "{again:?}");
    assert_eq!((again.cols, again.rows), (100, 30));
    // The first hold no longer works.
    let stale = scv_client::control(
        &home.join("state/server.sock"),
        DaemonCommand::AttachIo {
            id: id.clone(),
            after: 0,
            input: "ignored\r".into(),
            cols: None,
            rows: None,
        },
    )
    .await;
    assert!(stale.is_err(), "{stale:?}");

    // Ending it hangs the terminal up; the queued turn then runs.
    let ended = control(
        &home,
        DaemonCommand::AttachEnd {
            session: job.session.clone(),
            conversation: conversation.clone(),
        },
    )
    .await;
    assert!(ended.attach.unwrap().ended);
    until_gone(pid).await;
    assert_eq!(
        std::fs::read_to_string(home.join("terminal-hangups"))
            .unwrap()
            .trim(),
        "hup"
    );
    until_job(&home, |job| {
        job.job == queued_job && job.state == AgentState::Completed
    })
    .await;
    let calls = std::fs::read_to_string(home.join("codex-calls")).unwrap();
    assert!(
        calls.lines().any(|line| line.starts_with("exec resume")
            && line.contains("0199a213-81c0-7800-8aa1-bbab2a035a55")
            && line.contains("after the takeover")),
        "{calls}"
    );

    // A daemon that dies takes its terminals with it: the kernel hangs
    // them up as their master side closes.
    let attached = until_attached(&home, &job, None, None).await;
    until_screen(&home, &attached.id, "", "codex terminal on").await;
    let pid = terminal_pid(&home);
    assert!(running(pid));
    kill_hard(&mut daemon).await;
    until_gone(pid).await;
    let hangups = std::fs::read_to_string(home.join("terminal-hangups")).unwrap();
    assert_eq!(hangups.lines().count(), 2, "{hangups}");
}

/// A pseudo-terminal pair of 100 by 30, as a person's terminal would be:
/// the master side to read and type into, and the slave side for `scv`.
fn terminal_pair() -> (std::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd as _;
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty(3) writes a descriptor into each integer and reads
    // the window size; null asks for the default name and attributes.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(opened, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: both descriptors are open and owned by nothing else.
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::os::fd::OwnedFd::from_raw_fd(slave),
        )
    }
}

/// Read the terminal's output on a thread, until it closes.
fn read_terminal(mut master: std::fs::File) -> std::sync::Arc<std::sync::Mutex<String>> {
    use std::io::Read as _;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let shared = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(read) = master.read(&mut buffer) {
            if read == 0 {
                break;
            }
            shared
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    });
    seen
}

async fn until_shown(seen: &std::sync::Mutex<String>, wanted: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !seen.lock().unwrap().contains(wanted) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never shown: {wanted:?} in {:?}", seen.lock().unwrap()));
}

#[tokio::test]
async fn scv_jobs_attach_types_into_the_agents_terminal_and_detaches_or_ends_it() {
    use std::io::Write as _;
    let (_home, home) = takeover_home();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut daemon = start(&home, &workspace);
    status(&home).await;
    let started = scv(&home, &workspace)
        .args(["jobs", "start", "--agent", "codex", "a quick fix"])
        .output()
        .await
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    let job = until_job(&home, |job| {
        job.state == AgentState::Completed && job.conversation.is_some()
    })
    .await;
    // Without a terminal, it says so and changes nothing.
    let headless = scv(&home, &workspace)
        .args(["jobs", "attach", &job.job])
        .output()
        .await
        .unwrap();
    assert!(!headless.status.success());
    assert!(
        String::from_utf8_lossy(&headless.stderr).contains("needs a terminal"),
        "{headless:?}"
    );
    // Inside a delegated run it is refused, like every action.
    let delegated = scv(&home, &workspace)
        .args(["jobs", "attach", &job.job])
        .env("SCV_DELEGATION_DEPTH", "1")
        .output()
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&delegated.stderr).contains("refused inside a delegated"),
        "{delegated:?}"
    );

    let (master, slave) = terminal_pair();
    let mut keyboard = master.try_clone().unwrap();
    let seen = read_terminal(master);
    let mut attach = scv(&home, &workspace);
    attach
        .args(["jobs", "attach", &job.job])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut attach = attach.spawn().unwrap();
    // The screen is drawn cell by cell, so words come apart: look for one.
    until_shown(&seen, "0199a213-81c0-7800-8aa1-bbab2a035a55").await;
    keyboard.write_all(b"hello\r").unwrap();
    until_shown(&seen, "typed:").await;
    let pid = terminal_pid(&home);
    // Ctrl+], as the byte a terminal sends for it.
    keyboard.write_all(b"\x1d").unwrap();
    let detached = tokio::time::timeout(Duration::from_secs(10), attach.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(detached.success(), "{detached:?}");
    until_shown(&seen, "keeps running in its terminal").await;
    // Its terminal modes were restored: the alternate screen was left.
    assert!(seen.lock().unwrap().contains("\x1b[?1049l"));
    assert!(running(pid));

    let ended = scv(&home, &workspace)
        .args(["jobs", "attach", &job.job, "--end"])
        .output()
        .await
        .unwrap();
    assert!(ended.status.success(), "{ended:?}");
    assert!(
        String::from_utf8_lossy(&ended.stdout).contains("Ended the takeover"),
        "{ended:?}"
    );
    until_gone(pid).await;
    terminate(&mut daemon).await;
}

/// Run `scv` with `arguments` and return its exit status, stdout, and
/// stderr.
async fn jobs(home: &Path, cwd: &Path, arguments: &[&str]) -> (Option<i32>, String, String) {
    let output = scv(home, cwd).args(arguments).output().await.unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn json_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not JSON: {line}")))
        .collect()
}

/// The next line `lines` prints, as JSON.
async fn next_json(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> serde_json::Value {
    let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn a_script_starts_waits_on_reads_and_follows_jobs_with_json() {
    let (_home, home) = home();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut daemon = start(&home, &workspace);
    status(&home).await;
    // Following from before the job starts sees it added, its activity,
    // and then its end.
    let mut follow = scv(&home, &workspace)
        .args(["jobs", "events", "--follow", "--json"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut followed = BufReader::new(follow.stdout.take().unwrap()).lines();
    assert_eq!(next_json(&mut followed).await["type"], "ready");

    let (code, out, err) = jobs(
        &home,
        &workspace,
        &["jobs", "start", "--json", "--agent", "codex", "a quick fix"],
    )
    .await;
    assert_eq!(code, Some(0), "{out}{err}");
    let started: scv_protocol::DaemonStatus = serde_json::from_str(out.trim()).unwrap();
    let name = started.workbench_job.unwrap();
    let (session, job) = name.split_once('/').unwrap();
    assert_eq!(job, "job-1");

    let (code, out, err) = jobs(
        &home,
        &workspace,
        &["jobs", "wait", "job-1", "--json", "--timeout", "30"],
    )
    .await;
    assert_eq!(code, Some(0), "{out}{err}");
    let result: scv_protocol::JobResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        (
            result.session.as_str(),
            result.state,
            result.reply.as_deref()
        ),
        (session, AgentState::Completed, Some("landed 0.9.9"))
    );

    // A quick job may already be done when a poll first sees it.
    let mut seen = Vec::new();
    loop {
        let event = next_json(&mut followed).await;
        let ended = (event["type"] == "job.added" || event["type"] == "job.changed")
            && event["job"]["job"] == "job-1"
            && event["job"]["state"] == "completed";
        seen.push(event);
        if ended {
            break;
        }
    }
    assert!(
        seen.iter()
            .any(|event| event["type"] == "job.added" && event["job"]["job"] == "job-1"),
        "{seen:?}"
    );
    let said = seen
        .iter()
        .position(|event| event["type"] == "run.event" && event["event"]["text"] == "landed 0.9.9");
    assert!(said.is_some_and(|said| said < seen.len() - 1), "{seen:?}");
    drop(follow);

    let (code, out, _) = jobs(&home, &workspace, &["jobs", "result", "job-1", "--json"]).await;
    assert_eq!(code, Some(0));
    let read: scv_protocol::JobResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(read.reply.as_deref(), Some("landed 0.9.9"));
    let (code, out, _) = jobs(&home, &workspace, &["jobs", "result", "job-1"]).await;
    assert_eq!((code, out.as_str()), (Some(0), "landed 0.9.9\n"));

    let (code, out, _) = jobs(&home, &workspace, &["jobs", "list", "--all", "--json"]).await;
    assert_eq!(code, Some(0));
    let listed: WorkbenchSnapshot = serde_json::from_str(out.trim()).unwrap();
    assert!(listed.jobs.iter().any(|listed| listed.job == "job-1"));
    let (code, out, _) = jobs(&home, &workspace, &["jobs", "log", "job-1", "--json"]).await;
    assert_eq!(code, Some(0));
    let logged = json_lines(&out);
    assert!(
        logged
            .iter()
            .all(|event| event["run"].is_string() && event["kind"].is_string())
    );
    assert!(
        logged.iter().any(|event| event["text"] == "landed 0.9.9"),
        "{out}"
    );
    let (code, out, _) = jobs(&home, &workspace, &["jobs", "events", "--json"]).await;
    assert_eq!(code, Some(0));
    let events = json_lines(&out);
    assert_eq!(events.last().unwrap()["type"], "ready");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "job.added" && event["job"]["job"] == "job-1")
    );

    // A long job: the wait times out, and after a cancel it ends otherwise.
    let (code, out, err) = jobs(
        &home,
        &workspace,
        &["jobs", "start", "--agent", "codex", "the long task"],
    )
    .await;
    assert_eq!(code, Some(0), "{out}{err}");
    let (code, out, _) = jobs(
        &home,
        &workspace,
        &["jobs", "wait", "job-2", "--timeout", "1", "--json"],
    )
    .await;
    assert_eq!(code, Some(2), "{out}");
    let (code, out, err) = jobs(&home, &workspace, &["jobs", "cancel", "job-2", "--json"]).await;
    assert_eq!(code, Some(0), "{out}{err}");
    let cancelled: scv_protocol::DaemonStatus = serde_json::from_str(out.trim()).unwrap();
    assert!(
        cancelled
            .workbench_note
            .unwrap()
            .starts_with("Stopping job-2")
    );
    let (code, out, _) = jobs(
        &home,
        &workspace,
        &["jobs", "wait", "job-2", "--timeout", "30", "--json"],
    )
    .await;
    assert_eq!(code, Some(1), "{out}");
    let ended: scv_protocol::JobResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(ended.state, AgentState::Cancelled);
    let (code, _, err) = jobs(&home, &workspace, &["jobs", "wait", "job-99"]).await;
    assert_eq!(code, Some(3), "{err}");

    // Inside a delegated run, waiting, reading results, and following are
    // refused; listing still reads.
    for (arguments, refused) in [
        (&["jobs", "wait", "job-1"][..], Some(3)),
        (&["jobs", "result", "job-1", "--json"][..], Some(1)),
        (&["jobs", "events", "--json"][..], Some(1)),
        (
            &["jobs", "start", "--json", "--agent", "codex", "quick"][..],
            Some(1),
        ),
    ] {
        let output = scv(&home, &workspace)
            .env("SCV_DELEGATION_DEPTH", "1")
            .args(arguments)
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), refused, "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("refused inside a delegated agent run"),
            "{arguments:?}: {output:?}"
        );
    }
    let listed = scv(&home, &workspace)
        .env("SCV_DELEGATION_DEPTH", "1")
        .args(["jobs", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(snapshot(&home).await.jobs.len(), 2);
    terminate(&mut daemon).await;
}

/// How a scripted daemon answers `job_result`.
#[derive(Clone, Copy)]
enum JobResultAnswer {
    /// With the job's result.
    Result,
    /// As a daemon before `job_result` does: as a frame it cannot parse.
    Older,
    /// With an event no client expects.
    Garbled,
}

/// A daemon that answers every management connection by its action from
/// a fixed workbench listing job-1 in `state`, recording each
/// `daemon.control` command.
fn scripted_workbench(
    home: &Path,
    state: &'static str,
    answer: JobResultAnswer,
) -> std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
    use serde_json::json;
    std::fs::create_dir_all(home.join("state")).unwrap();
    let listener = tokio::net::UnixListener::bind(home.join("state/server.sock")).unwrap();
    let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&commands);
    let job = json!({"session":"3f2a9c1e-aaaa","job":"job-1","agent":"codex","task":"fix",
        "workspace":"/w","cwd":"/w","state":state,"started_by":"owner",
        "conversation":"codex-1","created_unix_seconds":1,"updated_unix_seconds":2});
    let approval = json!({"id":"ap-1","session":"3f2a9c1e-aaaa","job":"job-1","name":"bash",
        "risk":"process","cwd":"/w","summary":"Run make","since_unix_seconds":1,
        "expires_unix_seconds":2});
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let init: serde_json::Value = serde_json::from_str(&line).unwrap();
            let initialized = json!({"type":"initialized","request_id":init["request_id"],
                "protocol_version":init["protocol_version"],
                "server":{"name":"scv-server","version":"0.4.7"}});
            stream
                .get_mut()
                .write_all(format!("{initialized}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let command = request["command"].clone();
            seen.lock().unwrap().push(command.clone());
            let mut status = json!({"version":"0.4.7","pid":1,"components":[]});
            match (command["action"].as_str().unwrap(), answer) {
                ("workbench", _) => {
                    status["workbench"] = json!({"generated_unix_seconds":3,"jobs":[job],
                        "approvals":[approval]});
                }
                ("job_result", JobResultAnswer::Result) => {
                    status["job_result"] = json!({"session":"3f2a9c1e-aaaa","job":"job-1",
                        "agent":"codex","state":"completed",
                        "reply":"done\u{1b}[2J\u{9b}31m\u{7f}"});
                }
                ("job_result", JobResultAnswer::Older) => {
                    status = json!({"type":"error","request_id":request["request_id"],
                        "code":"invalid_json","message":"invalid JSON","fatal":false});
                }
                ("job_result", JobResultAnswer::Garbled) => {
                    status = json!({"type":"surprise","request_id":request["request_id"]});
                }
                _ => {
                    status["workbench_note"] = json!("Done.");
                    status["workbench_job"] = json!("3f2a9c1e-aaaa/job-2");
                }
            }
            let reply = if status.get("type").is_some() {
                status
            } else {
                json!({"type":"daemon.status","request_id":request["request_id"],
                    "status":status})
            };
            stream
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    commands
}

#[tokio::test]
async fn automation_never_watches_approves_or_prints_raw_control_characters() {
    let home = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let commands = scripted_workbench(&home, "completed", JobResultAnswer::Result);
    // `scv jobs result` reads the job's record from disk.
    let jobs_dir = scv_client::Layout::new(&home).jobs();
    let record = serde_json::json!({"v":1,"session":"3f2a9c1e-aaaa","job":"job-1",
        "agent":"codex","task":"fix","prompt":"fix","workspace":"/w","label":"workbench",
        "started_by":"owner","owner":scv_tools::delegation::ProcessIdentity::current().unwrap(),
        "state":"completed","created_unix":1,"updated_unix":2,"finished_unix":2,
        "reply":"line one\u{1b}]0;owned\u{7}\nline two\u{9b}2J"});
    write_private(
        &jobs_dir.join("3f2a9c1e-aaaa/job-1.json"),
        &record.to_string(),
    );
    for arguments in [
        &["jobs", "list", "--json"][..],
        &["jobs", "start", "--json", "--agent", "codex", "go"][..],
        &["jobs", "send", "job-1", "go on", "--json"][..],
        &["jobs", "cancel", "job-1", "--json"][..],
        &["jobs", "resume", "job-1", "--json"][..],
        &["jobs", "dismiss", "job-1", "--json"][..],
        &["jobs", "events", "--json"][..],
    ] {
        let (code, out, err) = jobs(&home, &workspace, arguments).await;
        assert_eq!(code, Some(0), "{arguments:?}: {out}{err}");
        for line in out.lines() {
            assert!(!line.chars().any(char::is_control), "{line:?}");
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }

    // A reply's escape sequences are escaped in JSON and blanked in text.
    let (code, out, _) = jobs(&home, &workspace, &["jobs", "wait", "job-1", "--json"]).await;
    assert_eq!(code, Some(0));
    assert!(!out.trim_end().chars().any(char::is_control), "{out:?}");
    assert!(out.contains(r"done\u001b[2J\u009b31m\u007f"), "{out}");
    let waited: scv_protocol::JobResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        waited.reply.as_deref(),
        Some("done\u{1b}[2J\u{9b}31m\u{7f}")
    );
    let (code, out, _) = jobs(&home, &workspace, &["jobs", "result", "job-1", "--json"]).await;
    assert_eq!(code, Some(0));
    assert!(!out.trim_end().chars().any(char::is_control), "{out:?}");
    let read: scv_protocol::JobResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        read.reply.as_deref(),
        Some("line one\u{1b}]0;owned\u{7}\nline two\u{9b}2J")
    );
    let (code, out, _) = jobs(&home, &workspace, &["jobs", "result", "job-1"]).await;
    assert_eq!(
        (code, out.as_str()),
        (Some(0), "line one ]0;owned \nline two 2J\n")
    );

    let commands = commands.lock().unwrap().clone();
    let actions: Vec<&str> = commands
        .iter()
        .map(|command| command["action"].as_str().unwrap())
        .collect();
    for expected in [
        "workbench",
        "workbench_start",
        "workbench_send",
        "workbench_cancel",
        "workbench_resume",
        "workbench_dismiss",
        "job_result",
    ] {
        assert!(actions.contains(&expected), "{expected} not in {actions:?}");
    }
    // An approval waited the whole time; nothing answered it, and nothing
    // asked as a watching dashboard, which would hold such requests.
    assert!(!actions.contains(&"workbench_approve"), "{actions:?}");
    assert!(
        commands
            .iter()
            .all(|command| command.get("watch").is_none()),
        "{commands:?}"
    );
}

#[tokio::test]
async fn a_wait_takes_an_older_daemons_listing_and_gives_up_on_a_garbled_answer() {
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    // A daemon before `job_result` lists the job interrupted, though its
    // record, kept by a process still alive, says it works: the wait goes
    // by the daemon's listing and the record's reply, cut to 4 KiB.
    let older = tempfile::tempdir().unwrap();
    let older = std::fs::canonicalize(older.path()).unwrap();
    let commands = scripted_workbench(&older, "interrupted", JobResultAnswer::Older);
    let reply = "r".repeat(scv_protocol::MAX_JOB_RESULT_BYTES + 100);
    let record = serde_json::json!({"v":1,"session":"3f2a9c1e-aaaa","job":"job-1",
        "agent":"codex","task":"fix","prompt":"fix","workspace":"/w","label":"workbench",
        "started_by":"owner","owner":scv_tools::delegation::ProcessIdentity::current().unwrap(),
        "state":"working","created_unix":1,"updated_unix":2,"reply":reply});
    write_private(
        &scv_client::Layout::new(&older)
            .jobs()
            .join("3f2a9c1e-aaaa/job-1.json"),
        &record.to_string(),
    );
    let (code, out, err) = jobs(
        &older,
        &workspace,
        &["jobs", "wait", "job-1", "--timeout", "30", "--json"],
    )
    .await;
    assert_eq!(code, Some(1), "{out}{err}");
    let waited: scv_protocol::JobResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        (
            waited.state,
            waited.reply.map(|reply| reply.len()),
            waited.truncated
        ),
        (
            AgentState::Interrupted,
            Some(scv_protocol::MAX_JOB_RESULT_BYTES),
            true
        )
    );
    let actions: Vec<String> = commands
        .lock()
        .unwrap()
        .iter()
        .map(|command| command["action"].as_str().unwrap().to_owned())
        .collect();
    assert!(actions.contains(&"job_result".to_owned()), "{actions:?}");

    // A daemon that answers what no client expects is asked a few times,
    // then the wait gives up rather than waiting for ever.
    let garbled = tempfile::tempdir().unwrap();
    let garbled = std::fs::canonicalize(garbled.path()).unwrap();
    let commands = scripted_workbench(&garbled, "working", JobResultAnswer::Garbled);
    let (code, out, err) = jobs(&garbled, &workspace, &["jobs", "wait", "job-1"]).await;
    assert_eq!(code, Some(3), "{out}{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("unexpected SCV management response"), "{err}");
    let asked = commands
        .lock()
        .unwrap()
        .iter()
        .filter(|command| command["action"] == "job_result")
        .count();
    assert_eq!(asked, 4);
}
