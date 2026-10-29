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
