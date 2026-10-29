//! Unit tests for `src/workbench.rs`.

use std::os::unix::fs::PermissionsExt as _;

use scv_client::Layout;
use scv_protocol::{AgentState, AttachState, JobOrigin, RunEventKind, SendMode, WorkbenchJob};

use super::*;

/// An instance whose Codex is a script that answers at once.
fn instance_with_fake_codex() -> (tempfile::TempDir, Instance) {
    let home = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(home.path()).unwrap();
    let agent = path.join("fake-codex");
    std::fs::write(
        &agent,
        concat!(
            "#!/bin/sh\n",
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}'\n",
            "echo '{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"command\":\"cargo test\"}}'\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"landed 0.9.9\"}}'\n",
            "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        path.join("config.toml"),
        format!(
            "[agents.codex]\ncommand = {:?}\ntransport = \"resume\"\n",
            agent.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        path.join("config.toml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let instance = Instance {
        layout: Layout::new(&path),
        overrides: ConfigOverrides::default(),
    };
    (home, instance)
}

fn hosts(instance: &Instance) -> Arc<Hosts> {
    let registry = Arc::new(DelegationRegistry::new(&instance.layout));
    Hosts::new(instance.clone(), registry)
}

async fn snapshot(hosts: &Arc<Hosts>) -> scv_protocol::WorkbenchSnapshot {
    control(hosts, DaemonCommand::Workbench { watch: false })
        .await
        .unwrap()
        .workbench
        .unwrap()
}

#[tokio::test]
async fn the_owners_job_runs_in_the_workbench_session_for_its_workspace() {
    let (_home, instance) = instance_with_fake_codex();
    let hosts = hosts(&instance);
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
    let status = control(
        &hosts,
        DaemonCommand::WorkbenchStart {
            workspace: workspace_path.display().to_string(),
            agent: "codex".into(),
            prompt: "Land the fix".into(),
            cwd: None,
            model: None,
            effort: None,
            worktree: false,
        },
    )
    .await
    .unwrap();
    let host = scv_tools::workbench::host_session(&workspace_path);
    assert_eq!(
        status.workbench_job.as_deref(),
        Some(format!("{host}/job-1").as_str())
    );
    assert!(status.workbench_note.unwrap().starts_with("Started"));
    let job = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = snapshot(&hosts).await;
            if let Some(job) = snapshot
                .jobs
                .into_iter()
                .find(|job| job.state == AgentState::Completed)
            {
                break job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(job.session, host);
    assert_eq!(job.started_by, JobOrigin::Owner);
    assert_eq!(job.task, "Land the fix");
    let run = job.run.clone().unwrap();
    let page = control(
        &hosts,
        DaemonCommand::RunLog {
            handle: run.clone(),
            after: 0,
            limit: None,
        },
    )
    .await
    .unwrap()
    .run_log
    .unwrap();
    let texts: Vec<(RunEventKind, String)> = page
        .events
        .into_iter()
        .map(|event| (event.kind, event.text))
        .collect();
    assert!(
        texts.contains(&(RunEventKind::Tool, "$ cargo test".into())),
        "{texts:?}"
    );
    assert!(
        texts.contains(&(RunEventKind::Text, "landed 0.9.9".into())),
        "{texts:?}"
    );
    // The job's result: its reply, without a snapshot.
    let status = control(
        &hosts,
        DaemonCommand::JobResult {
            session: host.clone(),
            job: "job-1".into(),
        },
    )
    .await
    .unwrap();
    assert!(status.workbench.is_none());
    let result = status.job_result.unwrap();
    assert_eq!(
        (result.state, result.reply.as_deref(), result.truncated),
        (AgentState::Completed, Some("landed 0.9.9"), false)
    );
    assert!(result.finished_unix_seconds.is_some());
    let unknown = control(
        &hosts,
        DaemonCommand::JobResult {
            session: host.clone(),
            job: "job-9".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(unknown.contains("no record"), "{unknown}");
    // The same workspace reuses its workbench session.
    assert_eq!(
        hosts
            .session_for(&workspace_path.display().to_string())
            .await
            .unwrap(),
        host
    );
    let session = snapshot(&hosts)
        .await
        .sessions
        .into_iter()
        .find(|session| session.id == host)
        .unwrap();
    assert_eq!((session.label.as_str(), session.live), (LABEL, true));
    hosts.shutdown().await;
    assert!(
        !snapshot(&hosts)
            .await
            .sessions
            .iter()
            .any(|session| session.id == host && session.live)
    );
}

#[tokio::test]
async fn workbench_actions_refuse_what_they_cannot_do_with_safe_messages() {
    let (_home, instance) = instance_with_fake_codex();
    let hosts = hosts(&instance);
    let relative = control(
        &hosts,
        DaemonCommand::WorkbenchStart {
            workspace: "relative/dir".into(),
            agent: "codex".into(),
            prompt: "go".into(),
            cwd: None,
            model: None,
            effort: None,
            worktree: false,
        },
    )
    .await
    .unwrap_err();
    assert!(relative.contains("absolute"), "{relative}");
    let unknown = control(
        &hosts,
        DaemonCommand::WorkbenchSend {
            session: "nobody".into(),
            conversation: "codex-1".into(),
            prompt: "hi".into(),
            mode: SendMode::Auto,
        },
    )
    .await
    .unwrap_err();
    assert!(unknown.contains("does not run in this daemon"), "{unknown}");
    let log = control(
        &hosts,
        DaemonCommand::RunLog {
            handle: "codex-abcdef".into(),
            after: 0,
            limit: None,
        },
    )
    .await
    .unwrap_err();
    assert!(log.contains("no activity log"), "{log}");
    let approval = control(
        &hosts,
        DaemonCommand::WorkbenchApprove {
            id: "ap-x".into(),
            approved: true,
        },
    )
    .await
    .unwrap_err();
    assert!(approval.contains("no approval request"), "{approval}");
    let resume = control(
        &hosts,
        DaemonCommand::WorkbenchResume {
            session: "nobody".into(),
            job: "job-1".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(resume.contains("no record"), "{resume}");
    let status = control(&hosts, DaemonCommand::Worktrees).await.unwrap();
    assert!(status.worktrees.is_empty() && status.workbench.is_none());
    assert!(control(&hosts, DaemonCommand::Status).await.is_err());
    assert!(is_workbench_command(&DaemonCommand::Worktrees));
    assert!(!is_workbench_command(&DaemonCommand::Reload));
}

#[tokio::test]
async fn the_snapshot_lists_queues_kept_from_sessions_that_stopped() {
    let (_home, instance) = instance_with_fake_codex();
    let hosts = hosts(&instance);
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let gone = scv_tools::delegation::ProcessIdentity::of(child.id()).unwrap();
    child.wait().unwrap();
    persist::save(
        &instance.layout,
        &persist::SavedQueue {
            v: 1,
            session: "old-session".into(),
            label: "TUI".into(),
            workspace: "/w".into(),
            owner: gone,
            saved_unix: persist::unix_now(),
            paused: false,
            entries: vec![scv_protocol::QueueEntry {
                queue_id: "q".into(),
                revision: 1,
                prompt: "next".into(),
                submitter: "r".into(),
                attachments: Vec::new(),
            }],
            active: None,
        },
    );
    let snapshot = snapshot(&hosts).await;
    assert_eq!(snapshot.restorable.len(), 1);
    assert_eq!(snapshot.restorable[0].id, "old-session");
}

/// An instance whose Codex, with `permissions = "full"`, answers each turn
/// at once (one whose prompt says `slow` after 30 seconds) and opens an
/// interactive terminal for `resume <thread>`.
fn instance_with_terminal_codex() -> (tempfile::TempDir, Instance) {
    let (home, instance) = instance_with_fake_codex();
    let agent = instance.layout.home().join("fake-codex");
    std::fs::write(
        &agent,
        concat!(
            "#!/bin/sh\n",
            "case \"$1\" in\n",
            "  resume)\n",
            "    echo \"codex terminal on $2\"\n",
            "    while IFS= read -r line; do echo \"typed: $line\"; done\n",
            "    ;;\n",
            "  *)\n",
            "    echo '{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}'\n",
            "    case \"$*\" in *slow*) sleep 30 ;; esac\n",
            "    echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"done\"}}'\n",
            "    echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
            "    ;;\n",
            "esac\n",
        ),
    )
    .unwrap();
    std::fs::write(
        instance.layout.home().join("config.toml"),
        format!(
            "[agents.codex]\ncommand = {:?}\ntransport = \"resume\"\npermissions = \"full\"\n",
            agent.display().to_string()
        ),
    )
    .unwrap();
    (home, instance)
}

#[tokio::test]
async fn terminal_reads_skip_the_delegations_and_nothing_is_taken_over_once_stopping() {
    let (_home, instance) = instance_with_terminal_codex();
    let hosts = hosts(&instance);
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
    control(
        &hosts,
        DaemonCommand::WorkbenchStart {
            workspace: workspace_path.display().to_string(),
            agent: "codex".into(),
            prompt: "a quick fix".into(),
            cwd: None,
            model: None,
            effort: None,
            worktree: false,
        },
    )
    .await
    .unwrap();
    let job = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(job) = snapshot(&hosts)
                .await
                .jobs
                .into_iter()
                .find(|job| job.state == AgentState::Completed && job.conversation.is_some())
            {
                break job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let attach = DaemonCommand::Attach {
        session: job.session.clone(),
        conversation: job.conversation.clone().unwrap(),
        cols: Some(80),
        rows: Some(24),
        interrupt: false,
        interrupt_job: None,
    };
    let attached = control(&hosts, attach.clone()).await.unwrap();
    // The screen's reads come every 50 ms: no process table scan for them.
    assert_eq!(attached.delegations, DelegationSummary::default());
    let id = attached.attach.unwrap().id;
    let read = control(
        &hosts,
        DaemonCommand::AttachIo {
            id,
            after: 0,
            input: "hi\r".into(),
            cols: None,
            rows: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(read.delegations, DelegationSummary::default());
    assert!(read.attach.is_some());
    // Other actions count them, the takeover's run among them.
    let listed = control(&hosts, DaemonCommand::Workbench { watch: false })
        .await
        .unwrap();
    assert!(listed.delegations.active >= 1, "{:?}", listed.delegations);
    hosts.shutdown().await;
    let refused = control(&hosts, attach).await.unwrap_err();
    assert!(refused.contains("stopping"), "{refused}");
}

#[tokio::test]
async fn an_interrupting_attach_stops_the_job_running_a_turn_and_takes_it_over() {
    let (_home, instance) = instance_with_terminal_codex();
    let hosts = hosts(&instance);
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
    control(
        &hosts,
        DaemonCommand::WorkbenchStart {
            workspace: workspace_path.display().to_string(),
            agent: "codex".into(),
            prompt: "a quick fix".into(),
            cwd: None,
            model: None,
            effort: None,
            worktree: false,
        },
    )
    .await
    .unwrap();
    let job = |name: &'static str, ready: fn(&WorkbenchJob) -> bool| {
        let hosts = Arc::clone(&hosts);
        async move {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if let Some(job) = snapshot(&hosts)
                        .await
                        .jobs
                        .into_iter()
                        .find(|job| job.job == name && ready(job))
                    {
                        break job;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{name} never got there"))
        }
    };
    let first = job("job-1", |job| {
        job.state == AgentState::Completed && job.conversation.is_some()
    })
    .await;
    assert_eq!(first.attach.unwrap().state, AttachState::Ready);
    let (session, conversation) = (first.session, first.conversation.unwrap());
    control(
        &hosts,
        DaemonCommand::WorkbenchSend {
            session: session.clone(),
            conversation: conversation.clone(),
            prompt: "slow work".into(),
            mode: SendMode::Queue,
        },
    )
    .await
    .unwrap();
    let running = job("job-2", |job| {
        job.attach
            .as_ref()
            .is_some_and(|offer| offer.state == AttachState::Busy)
    })
    .await;
    let offer = running.attach.unwrap();
    assert!(
        offer.interrupt && offer.job.as_deref() == Some("job-2"),
        "{offer:?}"
    );
    let attach = |interrupt, job: Option<&str>| DaemonCommand::Attach {
        session: session.clone(),
        conversation: conversation.clone(),
        cols: None,
        rows: None,
        interrupt,
        interrupt_job: job.map(str::to_owned),
    };
    // Without interrupt the running turn is left alone, as before.
    let refused = control(&hosts, attach(false, None)).await.unwrap_err();
    assert!(refused.contains("attach once it is idle"), "{refused}");
    // Nor is it stopped for an owner told of another job.
    let refused = control(&hosts, attach(true, Some("job-1")))
        .await
        .unwrap_err();
    assert!(
        refused.contains("not job-1's, so nothing was stopped"),
        "{refused}"
    );
    let taken = control(&hosts, attach(true, offer.job.as_deref()))
        .await
        .unwrap();
    assert_eq!(
        taken.workbench_note,
        Some(format!("Stopped job-2 and took {conversation} over."))
    );
    assert!(!taken.attach.unwrap().ended);
    job("job-2", |job| job.state == AgentState::Cancelled).await;
    let first = job("job-1", |_| true).await;
    assert_eq!(first.attach.unwrap().state, AttachState::TakenOver);
    control(
        &hosts,
        DaemonCommand::AttachEnd {
            session,
            conversation,
        },
    )
    .await
    .unwrap();
    hosts.shutdown().await;
}
