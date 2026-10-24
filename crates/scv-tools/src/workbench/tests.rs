//! Unit tests for `src/workbench.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use scv_core::{Tool as _, ToolContext, ToolError, ToolOutput, ToolRisk};
use scv_protocol::JobOrigin;
use tokio::sync::Notify;

use super::*;
use crate::delegate::{
    activity::{RunActivity, RunHeader, RunTransport},
    agent::{Accepts, AgentTool, Backend},
    background::ledger::{JobLedger, JobRecord},
    records::ProcessIdentity,
};

struct Bench {
    root: tempfile::TempDir,
    layout: Layout,
    workbench: Arc<Workbench>,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path());
    let registry = Arc::new(DelegationRegistry::for_test(&layout));
    Bench {
        workbench: Workbench::new(&layout, registry),
        layout,
        root: dir,
    }
}

fn record(session: &str, job: &str, state: AgentState, owner: ProcessIdentity) -> JobRecord {
    JobRecord {
        v: 1,
        session: session.into(),
        job: job.into(),
        agent: "codex".into(),
        task: "Fix it".into(),
        prompt: "Fix it\nfully".into(),
        arguments: json!({"agent":"codex","cwd":"shop","effort":"high"}),
        workspace: PathBuf::from("/w"),
        label: "TUI".into(),
        started_by: JobOrigin::Model,
        reviewed: false,
        owner,
        state,
        created_unix: 10,
        updated_unix: 10,
        finished_unix: None,
        conversation: None,
        worktree: None,
        reply: None,
        error: None,
        reported: false,
        interrupted: None,
        resumed_as: None,
        resumed_from: None,
        dismissed: false,
    }
}

#[test]
fn an_interrupted_job_stays_listed_past_the_finished_cap() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let owner = dead();
    for n in 0..201 {
        let mut done = record("s", &format!("job-{n}"), AgentState::Completed, owner);
        done.created_unix = 1000 + n;
        done.reported = true;
        ledger::save(&jobs, &done).unwrap();
    }
    let mut interrupted = record("s", "job-old", AgentState::Interrupted, owner);
    interrupted.created_unix = 1;
    interrupted.interrupted = Some("the SCV process that ran it stopped".into());
    ledger::save(&jobs, &interrupted).unwrap();
    let snapshot = bench.workbench.snapshot();
    assert!(
        snapshot
            .jobs
            .iter()
            .any(|job| job.job == "job-old" && job.resumable),
        "the resumable job was dropped with the finished cap"
    );
}

fn dead() -> ProcessIdentity {
    ProcessIdentity {
        pid: u32::MAX - 3,
        start_time: 1,
    }
}

fn run(
    bench: &Bench,
    handle: &str,
    session: &str,
    job: &str,
    owner: ProcessIdentity,
) -> RunActivity {
    let activity = RunActivity::open(
        &bench.layout.runs(),
        owner,
        RunHeader {
            handle: handle.into(),
            agent: "codex".into(),
            session: session.into(),
            transport: RunTransport::Cli,
            cwd: PathBuf::from("/w/shop"),
            conversation: Some(("codex-1".into(), 1)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-1"), 1, "Fix it", Some(job));
    activity
}

#[test]
fn a_snapshot_joins_jobs_with_their_runs_and_finds_interrupted_ones() {
    let bench = bench();
    let me = ProcessIdentity::current().unwrap();
    let jobs = bench.layout.jobs();
    ledger::save(
        &jobs,
        &record("other-session", "job-1", AgentState::Working, dead()),
    )
    .unwrap();
    ledger::save(
        &jobs,
        &record("other-session", "job-2", AgentState::Working, me),
    )
    .unwrap();
    let mut done = record("other-session", "job-3", AgentState::Failed, me);
    done.error = Some("tests fail\nmore".into());
    ledger::save(&jobs, &done).unwrap();
    let live = run(&bench, "codex-0000a1", "other-session", "job-2", me);
    live.tool("$ cargo test");
    let snapshot = bench.workbench.snapshot();
    let job = |name: &str| {
        snapshot
            .jobs
            .iter()
            .find(|job| job.job == name)
            .unwrap()
            .clone()
    };
    let crashed = job("job-1");
    assert_eq!(crashed.state, AgentState::Interrupted);
    assert!(crashed.resumable);
    // job-2's own process (this one) still runs it, but its session is not
    // live here, so it ended with that session.
    let orphan = job("job-2");
    assert_eq!(orphan.state, AgentState::Interrupted);
    assert_eq!(orphan.run.as_deref(), Some("codex-0000a1"));
    assert_eq!(orphan.cwd, "/w/shop");
    // Its session is not live here, so its conversation names nothing the
    // owner could send to.
    assert_eq!(orphan.conversation, None);
    assert_eq!(snapshot.runs[0].conversation, None);
    assert_eq!(snapshot.runs[0].jobs, ["job-2"]);
    let failed = job("job-3");
    assert_eq!(failed.state, AgentState::Failed);
    assert_eq!(failed.detail.as_deref(), Some("tests fail"));
    assert_eq!(failed.cwd, "/w/shop");
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == "other-session")
        .unwrap();
    assert_eq!(session.label, "TUI");
    assert_eq!(snapshot.runs[0].handle, "codex-0000a1");
}

/// An agent that finishes when released.
struct Waiting {
    release: Arc<Notify>,
    ran: Arc<AtomicBool>,
}

#[async_trait]
impl Backend for Waiting {
    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Delegate)
    }
    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok(String::new())
    }
    fn adoptable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.ran.store(true, Ordering::SeqCst);
        tokio::select! {
            () = self.release.notified() => Ok(ToolOutput::success(
                json!({"agent":"codex","status":"completed","reply":arguments["prompt"],"session":arguments.get("session").cloned().unwrap_or(json!("codex-1"))})
                    .to_string(),
            )),
            () = context.cancellation.cancelled() => Err(ToolError::cancelled("cancelled")),
        }
    }
}

/// A live session of this process with an `agent` tool offering
/// [`Waiting`] as `codex`.
struct Live {
    _handle: SessionHandle,
    tool: Arc<BackgroundCapable>,
    jobs: Arc<BackgroundJobs>,
    release: Arc<Notify>,
}

fn live(bench: &Bench, session: &str) -> Live {
    live_for(bench, session, Path::new("/w"), None)
}

fn live_for(bench: &Bench, session: &str, workspace: &Path, worktrees: Option<PathBuf>) -> Live {
    let handle = bench.workbench.register_session(session, "TUI", workspace);
    let release = Arc::new(Notify::new());
    let jobs = Arc::new(
        BackgroundJobs::new(4, None)
            .with_ledger(JobLedger::new(&bench.layout.jobs(), session, "TUI", workspace).unwrap()),
    );
    let accepts = Accepts {
        session: true,
        ..Accepts::default()
    };
    let mut agent = AgentTool::beside(
        "codex",
        Arc::new(Waiting {
            release: Arc::clone(&release),
            ran: Arc::default(),
        }),
        accepts,
        &[],
    );
    if let Some(root) = worktrees {
        agent = agent.with_worktrees(Some(root));
    }
    let tool = Arc::new(BackgroundCapable::plain(Arc::new(agent), Arc::clone(&jobs)));
    bench
        .workbench
        .attach(session, Arc::downgrade(&tool), Arc::downgrade(&jobs));
    Live {
        _handle: handle,
        tool,
        jobs,
        release,
    }
}

#[tokio::test]
async fn the_owner_starts_sends_to_and_stops_jobs_of_live_sessions() {
    let bench = bench();
    let live = live(&bench, "session-a");
    let started = bench
        .workbench
        .start("session-a", json!({"agent":"codex","prompt":"look"}))
        .await
        .unwrap();
    assert_eq!(started, "session-a/job-1");
    let snapshot = bench.workbench.snapshot();
    let job = &snapshot.jobs[0];
    assert_eq!(job.started_by, JobOrigin::Owner);
    assert_eq!(job.state, AgentState::Working);
    assert_eq!(snapshot.sessions[0].id, "session-a");
    assert!(snapshot.sessions[0].live);
    // Sending to a conversation that is not this session's fails cleanly.
    assert!(
        bench
            .workbench
            .send("nobody", "codex-1", "hi", SendMode::Auto)
            .await
            .unwrap_err()
            .contains("does not run in this daemon")
    );
    let note = bench.workbench.cancel("session-a", "job-1").unwrap();
    assert!(note.contains("Stopping job-1"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while live.jobs.running() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        ledger::load(&bench.layout.jobs(), "session-a", "job-1")
            .unwrap()
            .state,
        AgentState::Cancelled
    );
    // A finished job can be forgotten; a running one cannot.
    bench
        .workbench
        .start("session-a", json!({"agent":"codex","prompt":"two"}))
        .await
        .unwrap();
    assert!(bench.workbench.dismiss("session-a", "job-2").is_err());
    assert!(bench.workbench.dismiss("session-a", "job-1").is_ok());
    assert!(ledger::load(&bench.layout.jobs(), "session-a", "job-1").is_none());
    live.release.notify_waiters();
    drop(live.tool);
}

/// An unattended gate that always answers `answer`.
struct Fixed(bool);

#[async_trait]
impl ApprovalGate for Fixed {
    async fn approve(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        Ok(self.0)
    }
}

fn request() -> ApprovalRequest {
    ApprovalRequest {
        call_id: "job-7".into(),
        name: "agent".into(),
        risk: scv_core::ToolRisk::Process,
        cwd: PathBuf::from("/w"),
        summary: "[codex-1 acp] Run make deploy (execute)".into(),
    }
}

#[tokio::test]
async fn a_watched_background_request_waits_for_the_owner_and_never_loses_an_approval() {
    let bench = bench();
    // What the session approves unattended is never held.
    let allowing = bench.workbench.attended_gate("s", Arc::new(Fixed(true)));
    bench.workbench.watch();
    assert!(
        allowing
            .approve(request(), CancellationToken::new())
            .await
            .unwrap()
    );
    assert_eq!(bench.workbench.snapshot().approvals, []);

    let denying = bench.workbench.attended_gate("s", Arc::new(Fixed(false)));
    let asked = tokio::spawn({
        let denying = Arc::clone(&denying);
        async move { denying.approve(request(), CancellationToken::new()).await }
    });
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(approval) = bench.workbench.snapshot().approvals.pop() {
                break approval;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        (id.session.as_str(), id.job.as_str(), id.risk.as_str()),
        ("s", "job-7", "process")
    );
    let note = bench.workbench.approve(&id.id, true).unwrap();
    assert!(note.starts_with("Approved job-7"), "{note}");
    assert!(asked.await.unwrap().unwrap());
    assert!(bench.workbench.approve(&id.id, true).is_err());
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_or_unwatched_request_gets_the_unattended_answer() {
    let bench = bench();
    let denying = bench.workbench.attended_gate("s", Arc::new(Fixed(false)));
    // Nobody watches: denied at once.
    assert!(
        !denying
            .approve(request(), CancellationToken::new())
            .await
            .unwrap()
    );
    bench.workbench.watch();
    let asked = tokio::spawn({
        let denying = Arc::clone(&denying);
        async move { denying.approve(request(), CancellationToken::new()).await }
    });
    tokio::time::sleep(ATTENDED_WAIT + Duration::from_secs(1)).await;
    assert!(!asked.await.unwrap().unwrap());
    assert_eq!(bench.workbench.snapshot().approvals, []);
    // A cancelled request is withdrawn.
    bench.workbench.watch();
    let cancellation = CancellationToken::new();
    let asked = tokio::spawn({
        let denying = Arc::clone(&denying);
        let cancellation = cancellation.clone();
        async move { denying.approve(request(), cancellation).await }
    });
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert!(matches!(asked.await.unwrap(), Err(AgentError::Cancelled)));
    assert_eq!(bench.workbench.snapshot().approvals, []);
}

#[tokio::test]
async fn an_interrupted_job_resumes_its_agents_own_conversation_once() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let mut crashed = record("old-session", "job-4", AgentState::Interrupted, dead());
    crashed.interrupted = Some("the SCV process that ran it stopped".into());
    ledger::save(&jobs, &crashed).unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let activity = RunActivity::open(
        &bench.layout.runs(),
        dead(),
        RunHeader {
            handle: "codex-00f00d".into(),
            agent: "codex".into(),
            session: "old-session".into(),
            transport: RunTransport::Cli,
            cwd: workdir.path().to_owned(),
            conversation: Some(("codex-3".into(), 2)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-3"), 2, "Fix it", Some("job-4"));
    activity.set_vendor("019a-thread-id");
    std::mem::forget(activity);

    let host = live(&bench, "workbench-host");
    let resumed = bench
        .workbench
        .resume("workbench-host", "old-session", "job-4")
        .await
        .unwrap();
    assert_eq!(resumed, "workbench-host/job-1");
    let new = ledger::load(&jobs, "workbench-host", "job-1").unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some("old-session/job-4"));
    assert_eq!(new.started_by, JobOrigin::Owner);
    // It continues the adopted conversation, told it was cut off.
    assert_eq!(new.arguments["session"], "codex-1");
    assert!(
        new.prompt
            .starts_with("[SCV] SCV stopped before your last turn"),
        "{}",
        new.prompt
    );
    assert!(new.prompt.ends_with("Fix it\nfully"));
    let old = ledger::load(&jobs, "old-session", "job-4").unwrap();
    assert_eq!(old.resumed_as.as_deref(), Some("workbench-host/job-1"));
    let again = bench
        .workbench
        .resume("workbench-host", "old-session", "job-4")
        .await
        .unwrap_err();
    assert!(again.contains("already resumed"), "{again}");
    host.release.notify_waiters();
}

#[tokio::test]
async fn a_queued_follow_up_resumes_in_its_conversations_directory() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let mut follow = record("old-session", "job-9", AgentState::Interrupted, dead());
    follow.arguments = json!({"session": "codex-1"});
    follow.prompt = "also run clippy".into();
    follow.worktree = None;
    follow.interrupted = Some("its session ended before it finished".into());
    ledger::save(&jobs, &follow).unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let activity = RunActivity::open(
        &bench.layout.runs(),
        dead(),
        RunHeader {
            handle: "codex-00abcd".into(),
            agent: "codex".into(),
            session: "old-session".into(),
            transport: RunTransport::Cli,
            cwd: workdir.path().to_owned(),
            conversation: Some(("codex-1".into(), 1)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-1"), 1, "Fix it", Some("job-1"));
    std::mem::forget(activity);
    let _host = live(&bench, "workbench-host");
    let resumed = bench
        .workbench
        .resume("workbench-host", "old-session", "job-9")
        .await
        .unwrap();
    let new = ledger::load(&jobs, "workbench-host", resumed.split('/').nth(1).unwrap()).unwrap();
    assert_eq!(
        new.arguments["cwd"],
        workdir.path().display().to_string(),
        "the follow-up resumed in the workspace root"
    );
    assert!(new.arguments.get("session").is_none());
}

#[tokio::test]
async fn a_queued_follow_up_resumes_in_its_conversations_worktree() {
    if which::which("git").is_err() {
        return;
    }
    let bench = bench();
    let home = std::fs::canonicalize(bench.root.path()).unwrap();
    let workspace = home.join("workspace");
    let repo = workspace.join("project");
    std::fs::create_dir_all(repo.join("shop")).unwrap();
    std::fs::write(repo.join("shop/cart.rs"), "fn main() {}\n").unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "first",
    ]);
    let created = crate::delegate::worktree::create(
        &bench.layout.worktrees(),
        &repo.join("shop"),
        &workspace,
        "codex",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let jobs = bench.layout.jobs();
    let mut follow = record("old-session", "job-9", AgentState::Interrupted, dead());
    follow.arguments = json!({"session": "codex-1"});
    follow.prompt = "also run clippy".into();
    follow.worktree = None;
    follow.interrupted = Some("its session ended before it finished".into());
    ledger::save(&jobs, &follow).unwrap();
    let activity = RunActivity::open(
        &bench.layout.runs(),
        dead(),
        RunHeader {
            handle: "codex-00abcd".into(),
            agent: "codex".into(),
            session: "old-session".into(),
            transport: RunTransport::Cli,
            cwd: created.cwd.clone(),
            conversation: Some(("codex-1".into(), 1)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-1"), 1, "Fix it", Some("job-1"));
    std::mem::forget(activity);
    let host = live(&bench, "workbench-host");
    let resumed = bench
        .workbench
        .resume("workbench-host", "old-session", "job-9")
        .await
        .unwrap();
    let new = ledger::load(&jobs, "workbench-host", resumed.split('/').nth(1).unwrap()).unwrap();
    assert_eq!(
        new.arguments["cwd"],
        created.cwd.display().to_string(),
        "the follow-up resumed in the conversation's worktree"
    );
    assert_eq!(new.worktree.as_ref(), Some(&created.worktree));
    assert!(new.arguments.get("session").is_none());
    host.release.notify_waiters();
}

#[tokio::test]
async fn a_queued_follow_up_waits_behind_the_resumed_head_instead_of_adopting_twice() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let workdir = tempfile::tempdir().unwrap();
    let mut head = record("old-session", "job-1", AgentState::Interrupted, dead());
    head.arguments = json!({
        "agent": "codex",
        "cwd": workdir.path().display().to_string(),
    });
    head.interrupted = Some("the SCV process that ran it stopped".into());
    ledger::save(&jobs, &head).unwrap();
    let mut follow = record("old-session", "job-2", AgentState::Interrupted, dead());
    follow.arguments = json!({"session": "codex-1"});
    follow.prompt = "also run clippy".into();
    follow.worktree = None;
    follow.interrupted = Some("its session ended before it finished".into());
    ledger::save(&jobs, &follow).unwrap();
    let activity = RunActivity::open(
        &bench.layout.runs(),
        dead(),
        RunHeader {
            handle: "codex-00f00d".into(),
            agent: "codex".into(),
            session: "old-session".into(),
            transport: RunTransport::Cli,
            cwd: workdir.path().to_owned(),
            conversation: Some(("codex-1".into(), 1)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-1"), 1, "Fix it", Some("job-1"));
    activity.set_vendor("019a-thread-id");
    std::mem::forget(activity);
    let host = live(&bench, "workbench-host");
    let early = bench
        .workbench
        .resume("workbench-host", "old-session", "job-2")
        .await
        .unwrap_err();
    assert!(
        early.contains("old-session/job-1") && early.contains("resume"),
        "{early}"
    );
    let resumed_head = bench
        .workbench
        .resume("workbench-host", "old-session", "job-1")
        .await
        .unwrap();
    let head_job = ledger::load(&jobs, "workbench-host", job_id(&resumed_head)).unwrap();
    assert_eq!(head_job.arguments["session"], "codex-1");
    assert_eq!(head_job.state, AgentState::Working);
    let resumed_follow = bench
        .workbench
        .resume("workbench-host", "old-session", "job-2")
        .await
        .unwrap();
    let follow_job = ledger::load(&jobs, "workbench-host", job_id(&resumed_follow)).unwrap();
    assert_eq!(
        follow_job.arguments["session"], head_job.arguments["session"],
        "the follow-up adopted the agent session a second time"
    );
    assert_eq!(follow_job.state, AgentState::Queued);
    host.release.notify_waiters();
}

#[tokio::test]
async fn a_queued_follow_up_adopts_once_its_head_job_has_finished() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let workdir = tempfile::tempdir().unwrap();
    let mut head = record("old-session", "job-1", AgentState::Completed, dead());
    head.arguments = json!({"agent": "codex"});
    ledger::save(&jobs, &head).unwrap();
    let mut follow = record("old-session", "job-2", AgentState::Interrupted, dead());
    follow.arguments = json!({"session": "codex-1"});
    follow.prompt = "also run clippy".into();
    follow.interrupted = Some("its session ended before it finished".into());
    ledger::save(&jobs, &follow).unwrap();
    let activity = RunActivity::open(
        &bench.layout.runs(),
        dead(),
        RunHeader {
            handle: "codex-00abcd".into(),
            agent: "codex".into(),
            session: "old-session".into(),
            transport: RunTransport::Cli,
            cwd: workdir.path().to_owned(),
            conversation: Some(("codex-1".into(), 1)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-1"), 1, "Fix it", Some("job-1"));
    activity.set_vendor("019a-thread-id");
    std::mem::forget(activity);
    let host = live(&bench, "workbench-host");
    let resumed = bench
        .workbench
        .resume("workbench-host", "old-session", "job-2")
        .await
        .unwrap();
    let new = ledger::load(&jobs, "workbench-host", job_id(&resumed)).unwrap();
    assert_eq!(new.arguments["session"], "codex-1");
    assert_eq!(new.state, AgentState::Working);
    host.release.notify_waiters();
}

fn job_id(resumed: &str) -> &str {
    resumed.split('/').nth(1).unwrap()
}

#[tokio::test]
async fn a_job_without_a_saved_conversation_starts_over_and_a_running_one_is_refused() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    ledger::save(
        &jobs,
        &record("old", "job-1", AgentState::Interrupted, dead()),
    )
    .unwrap();
    let me = ProcessIdentity::current().unwrap();
    ledger::save(&jobs, &record("busy", "job-1", AgentState::Completed, me)).unwrap();
    let mut reviewed = record("old", "job-2", AgentState::Interrupted, dead());
    reviewed.reviewed = true;
    ledger::save(&jobs, &reviewed).unwrap();
    let _host = live(&bench, "host");
    let resumed = bench
        .workbench
        .resume("host", "old", "job-1")
        .await
        .unwrap();
    let new = ledger::load(&jobs, "host", resumed.split('/').nth(1).unwrap()).unwrap();
    assert_eq!(new.arguments["agent"], "codex");
    assert_eq!(new.arguments["cwd"], "shop");
    assert!(new.arguments.get("session").is_none());
    assert!(
        new.prompt.starts_with("[SCV] An earlier attempt"),
        "{}",
        new.prompt
    );
    assert!(
        bench
            .workbench
            .resume("host", "busy", "job-1")
            .await
            .unwrap_err()
            .contains("not interrupted")
    );
    assert!(
        bench
            .workbench
            .resume("host", "old", "job-2")
            .await
            .unwrap_err()
            .contains("reviewed job")
    );
}

#[test]
fn maintenance_settles_what_dead_processes_left_and_hosts_are_stable() {
    let bench = bench();
    ledger::save(
        &bench.layout.jobs(),
        &record("s", "job-1", AgentState::Working, dead()),
    )
    .unwrap();
    std::mem::forget(run(&bench, "codex-0000b2", "s", "job-1", dead()));
    let done = bench.workbench.maintain();
    assert_eq!(done.interrupted_jobs, vec!["s/job-1".to_owned()]);
    assert_eq!(done.interrupted_runs, vec!["codex-0000b2".to_owned()]);
    assert_eq!(bench.workbench.maintain(), Maintenance::default());
    let host = host_session(Path::new("/w/project"));
    assert!(host.starts_with("workbench-") && host.len() == "workbench-".len() + 16);
    assert_eq!(host, host_session(Path::new("/w/project")));
    assert_ne!(host, host_session(Path::new("/w/other")));
    assert!(ledger::valid_name(&host));
}

#[test]
fn a_session_state_shows_until_its_handle_drops() {
    let bench = bench();
    let handle = bench
        .workbench
        .register_session("s1", "WeChat", Path::new("/w"));
    handle.set_state(
        AgentState::AwaitingInput,
        Some("waiting for approval: x".into()),
    );
    let session = bench.workbench.snapshot().sessions.pop().unwrap();
    assert_eq!(
        (session.label.as_str(), session.state, session.live),
        ("WeChat", AgentState::AwaitingInput, true)
    );
    drop(handle);
    assert_eq!(bench.workbench.snapshot().sessions, []);
    assert!(
        bench
            .workbench
            .run_log("codex-ffffff", 0, 10)
            .unwrap_err()
            .contains("no activity log")
    );
}

#[tokio::test]
async fn a_conversation_shows_only_while_its_live_session_holds_it() {
    let bench = bench();
    let me = ProcessIdentity::current().unwrap();
    let jobs = bench.layout.jobs();
    let mut done = record("workbench-host", "job-1", AgentState::Completed, me);
    done.conversation = Some("codex-1".into());
    ledger::save(&jobs, &done).unwrap();
    let first = live(&bench, "workbench-host");
    assert_eq!(
        first.tool.adopt("codex", Path::new("/w"), "019a-thread"),
        Some("codex-1".into())
    );
    let shown = bench.workbench.snapshot();
    assert_eq!(shown.jobs[0].conversation.as_deref(), Some("codex-1"));
    // The session goes and comes back under its ID, as a workbench session
    // does: the old handle names nothing now, so it is not offered.
    drop(first);
    let _second = live(&bench, "workbench-host");
    let hidden = bench.workbench.snapshot();
    assert_eq!(hidden.jobs[0].conversation, None);
}

#[tokio::test]
async fn a_job_is_resumed_once_at_a_time() {
    let bench = bench();
    ledger::save(
        &bench.layout.jobs(),
        &record("old", "job-1", AgentState::Interrupted, dead()),
    )
    .unwrap();
    let _host = live(&bench, "host");
    lock(&bench.workbench.resuming).insert("old/job-1".into());
    let refused = bench
        .workbench
        .resume("host", "old", "job-1")
        .await
        .unwrap_err();
    assert!(refused.contains("being resumed already"), "{refused}");
    lock(&bench.workbench.resuming).clear();
    bench
        .workbench
        .resume("host", "old", "job-1")
        .await
        .unwrap();
    assert!(lock(&bench.workbench.resuming).is_empty(), "released");
}

#[tokio::test]
async fn a_job_another_names_as_what_it_resumes_was_resumed() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    ledger::save(
        &jobs,
        &record("old", "job-1", AgentState::Interrupted, dead()),
    )
    .unwrap();
    // SCV stopped after the new job's record, before the old one's said so.
    let mut successor = record("host-old", "job-3", AgentState::Interrupted, dead());
    successor.resumed_from = Some("old/job-1".into());
    ledger::save(&jobs, &successor).unwrap();
    let snapshot = bench.workbench.snapshot();
    let old = snapshot.jobs.iter().find(|job| job.job == "job-1").unwrap();
    assert!(!old.resumable);
    assert_eq!(old.resumed_as.as_deref(), Some("host-old/job-3"));
    let _host = live(&bench, "host");
    let refused = bench
        .workbench
        .resume("host", "old", "job-1")
        .await
        .unwrap_err();
    assert!(
        refused.contains("already resumed as host-old/job-3"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_dismissed_successor_still_marks_its_job_resumed() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    ledger::save(
        &jobs,
        &record("old", "job-1", AgentState::Interrupted, dead()),
    )
    .unwrap();
    // SCV stopped before the old record said it was resumed, and the owner
    // then dismissed the job that resumed it.
    let mut successor = record("host-old", "job-3", AgentState::Interrupted, dead());
    successor.resumed_from = Some("old/job-1".into());
    ledger::save(&jobs, &successor).unwrap();
    ledger::dismiss(&jobs, "host-old", "job-3").unwrap();
    let snapshot = bench.workbench.snapshot();
    assert!(!snapshot.jobs.iter().any(|job| job.job == "job-3"));
    let old = snapshot.jobs.iter().find(|job| job.job == "job-1").unwrap();
    assert!(!old.resumable);
    let _host = live(&bench, "host");
    let refused = bench
        .workbench
        .resume("host", "old", "job-1")
        .await
        .unwrap_err();
    assert!(
        refused.contains("already resumed as host-old/job-3"),
        "{refused}"
    );
}

#[test]
fn a_job_another_live_process_has_yet_to_report_is_not_dismissed() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let other = ProcessIdentity::of(child.id()).unwrap();
    let mut finished = record("exec", "job-1", AgentState::Completed, other);
    ledger::save(&jobs, &finished).unwrap();
    let refused = bench.workbench.dismiss("exec", "job-1").unwrap_err();
    assert!(refused.contains("not reached its session"), "{refused}");
    finished.reported = true;
    ledger::save(&jobs, &finished).unwrap();
    bench.workbench.dismiss("exec", "job-1").unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    // One left unfinished by a dead process is settled as it is dismissed,
    // so pruning removes it in time.
    ledger::save(&jobs, &record("gone", "job-2", AgentState::Working, dead())).unwrap();
    bench.workbench.dismiss("gone", "job-2").unwrap();
    let left = std::fs::read(jobs.join("gone/job-2.json")).unwrap();
    let left: JobRecord = serde_json::from_slice(&left).unwrap();
    assert!(left.dismissed);
    assert_eq!(left.state, AgentState::Interrupted);
}

#[tokio::test]
async fn a_dismissed_job_keeps_its_number_and_an_unreported_one_stays() {
    let bench = bench();
    let jobs = bench.layout.jobs();
    let live = live(&bench, "session-a");
    // A model's job whose result the session has not reported yet.
    live.tool
        .execute(
            json!({"agent":"codex","prompt":"look","background":true}),
            ToolContext::new(PathBuf::from("/w"), CancellationToken::new()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while live.jobs.running() > 0 {
            live.release.notify_waiters();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let refused = bench.workbench.dismiss("session-a", "job-1").unwrap_err();
    assert!(refused.contains("not reached its session"), "{refused}");
    live.jobs.report_settled(&["job-1".into()]);
    bench.workbench.dismiss("session-a", "job-1").unwrap();
    assert!(ledger::load(&jobs, "session-a", "job-1").is_none());
    assert!(
        !bench
            .workbench
            .snapshot()
            .jobs
            .iter()
            .any(|job| job.job == "job-1")
    );
    // A session back under its ID does not give job-1 to another job.
    let again = JobLedger::new(&jobs, "session-a", "TUI", Path::new("/w")).unwrap();
    assert_eq!(again.last_number(), 1);
    drop(live);
}

#[test]
fn a_rerun_stays_in_its_worktree_or_gets_a_new_one_once_it_was_removed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("shop");
    let worktree = dir.path().join("worktrees/shop-1a2b3c4d/codex-9f3e21");
    std::fs::create_dir_all(worktree.join("api")).unwrap();
    std::fs::create_dir_all(repo.join("api")).unwrap();
    let mut job = record("old", "job-1", AgentState::Interrupted, dead());
    job.arguments = json!({
        "agent":"codex",
        "cwd": worktree.join("api").display().to_string(),
    });
    job.worktree = Some(scv_protocol::WorktreeRef {
        path: worktree.display().to_string(),
        branch: "scv/codex-9f3e21".into(),
        repo: repo.display().to_string(),
    });
    let kept = rerun_arguments(&job, |_| true);
    assert_eq!(kept["cwd"], worktree.join("api").display().to_string());
    assert_eq!(kept.get("worktree"), None);
    std::fs::remove_dir_all(&worktree).unwrap();
    let fresh = rerun_arguments(&job, |_| true);
    assert_eq!(fresh["cwd"], repo.join("api").display().to_string());
    assert_eq!(fresh["worktree"], true);
}

#[tokio::test]
async fn a_follow_up_whose_worktree_was_removed_resumes_in_a_new_one() {
    if which::which("git").is_err() {
        return;
    }
    let bench = bench();
    let home = std::fs::canonicalize(bench.root.path()).unwrap();
    let workspace = home.join("workspace");
    let repo = workspace.join("project");
    std::fs::create_dir_all(repo.join("shop")).unwrap();
    std::fs::write(repo.join("shop/cart.rs"), "fn main() {}\n").unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "first",
    ]);
    let created = crate::delegate::worktree::create(
        &bench.layout.worktrees(),
        &repo.join("shop"),
        &workspace,
        "codex",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let jobs = bench.layout.jobs();
    let mut head = record("old-session", "job-1", AgentState::Interrupted, dead());
    head.arguments = json!({
        "agent": "codex",
        "cwd": created.cwd.display().to_string(),
    });
    head.worktree = Some(created.worktree.clone());
    head.interrupted = Some("the SCV process that ran it stopped".into());
    ledger::save(&jobs, &head).unwrap();
    let mut follow = record("old-session", "job-2", AgentState::Interrupted, dead());
    follow.arguments = json!({"agent": "codex", "session": "codex-1"});
    follow.prompt = "also run clippy".into();
    follow.worktree = None;
    follow.interrupted = Some("its session ended before it finished".into());
    ledger::save(&jobs, &follow).unwrap();
    let activity = RunActivity::open(
        &bench.layout.runs(),
        dead(),
        RunHeader {
            handle: "codex-00beef".into(),
            agent: "codex".into(),
            session: "old-session".into(),
            transport: RunTransport::Cli,
            cwd: created.cwd.clone(),
            conversation: Some(("codex-1".into(), 1)),
            streams: true,
        },
    )
    .unwrap();
    activity.turn_started(Some("codex-1"), 1, "Fix it", Some("job-1"));
    std::mem::forget(activity);
    crate::delegate::worktree::remove(
        &bench.layout.worktrees(),
        Path::new(&created.worktree.path),
        true,
    )
    .await
    .unwrap();
    assert!(!created.cwd.is_dir(), "the worktree is still there");
    let host = live_for(
        &bench,
        "workbench-host",
        &workspace,
        Some(bench.layout.worktrees()),
    );
    let resumed = bench
        .workbench
        .resume("workbench-host", "old-session", "job-2")
        .await
        .unwrap();
    let new = ledger::load(&jobs, "workbench-host", job_id(&resumed)).unwrap();
    let worktree = new
        .worktree
        .as_ref()
        .expect("the follow-up resumed in the workspace root");
    assert_ne!(worktree.path, workspace.display().to_string());
    assert_ne!(worktree.path, created.worktree.path);
    let cwd = new.arguments["cwd"].as_str().unwrap();
    assert!(
        Path::new(cwd).starts_with(&worktree.path),
        "cwd {cwd} is not inside {}",
        worktree.path
    );
    assert_ne!(cwd, workspace.display().to_string());
    host.release.notify_waiters();
}
