//! The job ledger and the owner's own calls (`owner_call`, `adopt`,
//! `cancel_for_owner`), which the workbench makes.

use scv_protocol::{AgentState, SendMode};

use super::*;
use crate::delegate::background::ledger::{self, JobLedger};

/// A store recording to a ledger under `root`, behind the `agent` tool
/// offering [`FakeAgent`].
fn recorded(root: &std::path::Path) -> Fixture {
    let (finished_tx, finished) = mpsc::unbounded_channel();
    let jobs = Arc::new(BackgroundJobs::new(2, Some(finished_tx)).with_ledger(
        JobLedger::new(root, "session-1", "TUI", std::path::Path::new("/w")).unwrap(),
    ));
    let release = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let tool = BackgroundCapable::plain(
        offering(FakeAgent {
            release: Arc::clone(&release),
            cancelled: Arc::clone(&cancelled),
        }),
        Arc::clone(&jobs),
    );
    Fixture {
        tool,
        jobs,
        release,
        cancelled,
        finished,
    }
}

fn record(root: &std::path::Path, job: &str) -> ledger::JobRecord {
    ledger::load(root, "session-1", job).unwrap()
}

async fn settled(fixture: &mut Fixture) {
    tokio::time::timeout(Duration::from_secs(5), fixture.finished.recv())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_job_is_recorded_from_its_start_to_its_report() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = recorded(root.path());
    let started = fixture
        .tool
        .execute(
            json!({"prompt":"Fix the flaky test\nmore","background":true}),
            call("call-1"),
        )
        .await
        .unwrap();
    assert!(started.content.contains("job-1"));
    let running = record(root.path(), "job-1");
    assert_eq!(running.state, AgentState::Working);
    assert_eq!(running.prompt, "Fix the flaky test\nmore");
    assert_eq!(running.task, "Fix the flaky test");
    assert_eq!(running.started_by, scv_protocol::JobOrigin::Model);
    fixture.release.notify_one();
    settled(&mut fixture).await;
    let done = record(root.path(), "job-1");
    assert_eq!(done.state, AgentState::Completed);
    assert_eq!(done.reply.as_deref(), Some("all done"));
    assert_eq!(done.conversation.as_deref(), Some("fake-1"));
    assert!(!done.reported);
    let reports = fixture.jobs.take_unreported();
    assert_eq!(reports.len(), 1);
    fixture.jobs.report_settled(&["job-1".into()]);
    assert!(record(root.path(), "job-1").reported);
}

/// A backend that records the job its call runs as.
struct JobNamer(Arc<Mutex<Option<String>>>);

#[async_trait]
impl Backend for JobNamer {
    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Delegate)
    }
    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok(String::new())
    }
    async fn execute(
        &self,
        _arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        *lock(&self.0) = context.job.clone();
        Ok(ToolOutput::success(
            json!({"status":"completed","reply":"ok"}).to_string(),
        ))
    }
}

#[tokio::test]
async fn a_jobs_call_knows_which_job_it_runs_as() {
    let seen = Arc::new(Mutex::new(None));
    let (finished_tx, mut finished) = mpsc::unbounded_channel();
    let jobs = Arc::new(BackgroundJobs::new(2, Some(finished_tx)));
    let tool = BackgroundCapable::plain(offering(JobNamer(Arc::clone(&seen))), jobs);
    tool.execute(json!({"prompt":"go","background":true}), context())
        .await
        .unwrap();
    finished.recv().await.unwrap();
    assert_eq!(lock(&seen).as_deref(), Some("job-1"));
    // A foreground call runs as no job.
    tool.execute(json!({"prompt":"go"}), context())
        .await
        .unwrap();
    assert_eq!(*lock(&seen), None);
}

#[tokio::test]
async fn the_owners_jobs_are_watched_not_reported_and_say_who_started_them() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = recorded(root.path());
    let call = fixture
        .tool
        .owner_call(
            json!({"prompt":"look around"}),
            std::env::temp_dir(),
            SendMode::Auto,
            JobMeta::default(),
        )
        .await
        .unwrap();
    let OwnerCall::Job(started) = call else {
        panic!("{call:?}");
    };
    assert_eq!(started["job"], "job-1");
    assert_eq!(
        record(root.path(), "job-1").started_by,
        scv_protocol::JobOrigin::Owner
    );
    let described = fixture.jobs.describe(Some("job-1"), "").unwrap();
    assert_eq!(described["started_by"], "owner");
    fixture.release.notify_one();
    settled(&mut fixture).await;
    assert_eq!(fixture.jobs.take_unreported(), []);
    assert!(record(root.path(), "job-1").reported);
    assert_eq!(fixture.jobs.pending(), 0);
}

#[tokio::test]
async fn a_job_the_owner_stops_is_reported_to_the_model_as_stopped_by_the_user() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = recorded(root.path());
    start(&fixture.tool).await;
    fixture.jobs.cancel_for_owner("job-1").unwrap();
    settled(&mut fixture).await;
    assert!(fixture.cancelled.load(Ordering::SeqCst));
    assert_eq!(record(root.path(), "job-1").state, AgentState::Cancelled);
    let reports = fixture.jobs.take_unreported();
    assert_eq!(reports[0].status, JobStatus::Cancelled);
    assert!(report_prompt(&reports).contains("stopped by the user"));
    assert!(fixture.jobs.cancel_for_owner("job-1").is_err());
    assert!(fixture.jobs.cancel_for_owner("job-9").is_err());
}

#[tokio::test]
async fn a_session_that_ends_records_its_unfinished_jobs_as_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let fixture = recorded(root.path());
    start(&fixture.tool).await;
    drop(fixture);
    let interrupted = record(root.path(), "job-1");
    assert_eq!(interrupted.state, AgentState::Interrupted);
    assert!(interrupted.resumable());
    assert!(interrupted.interrupted.unwrap().contains("session ended"));
}

#[tokio::test]
async fn the_owners_prompt_to_a_busy_conversation_queues_or_refuses_to_steer() {
    let session = session(2, BusyConfig::default());
    session
        .call(json!({"prompt":"model","session":"codex-1","background":true}))
        .await
        .unwrap();
    session.ran(&["model"]).await;
    let refused = session
        .tool
        .owner_call(
            json!({"prompt":"steer it","session":"codex-1"}),
            std::env::temp_dir(),
            SendMode::Steer,
            JobMeta::default(),
        )
        .await
        .unwrap_err();
    assert!(
        refused.message.contains("cannot take a prompt"),
        "{refused}"
    );
    // Auto falls back to the queue when the turn cannot be steered.
    let queued = session
        .tool
        .owner_call(
            json!({"prompt":"owner","session":"codex-1"}),
            std::env::temp_dir(),
            SendMode::Auto,
            JobMeta::default(),
        )
        .await
        .unwrap();
    let OwnerCall::Job(queued) = queued else {
        panic!("{queued:?}");
    };
    assert_eq!(queued["queued"], true);
    session.release_then(&["model", "owner"]).await;
    session.release.add_permits(1);
    let result = session.result(queued["job"].as_str().unwrap()).await;
    assert_eq!(result["result"]["reply"], "owner");
    // A handle this session does not have is refused before a job starts.
    let unknown = session
        .tool
        .owner_call(
            json!({"prompt":"again","session":"codex-2"}),
            std::env::temp_dir(),
            SendMode::Queue,
            JobMeta::default(),
        )
        .await
        .unwrap_err();
    assert!(
        unknown.message.contains("no conversation codex-2"),
        "{unknown}"
    );
}

/// An agent that continues a conversation from its own session ID.
struct Resumable;

#[async_trait]
impl Backend for Resumable {
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
        _arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("{}"))
    }
}

#[test]
fn only_agents_that_resume_by_id_take_in_a_saved_conversation() {
    let accepts = Accepts {
        session: true,
        ..Accepts::default()
    };
    let resumable = BackgroundCapable::plain(
        Arc::new(AgentTool::beside(
            "codex",
            Arc::new(Resumable),
            accepts,
            &[],
        )),
        Arc::new(BackgroundJobs::new(1, None)),
    );
    let handle = resumable
        .adopt("codex", std::path::Path::new("/w"), "019a-thread")
        .unwrap();
    assert_eq!(handle, "codex-1");
    assert_eq!(
        resumable.conversations.cwd(&handle).as_deref(),
        Some(std::path::Path::new("/w"))
    );
    assert!(
        resumable
            .adopt("codex", std::path::Path::new("/w"), "bad id!")
            .is_none()
    );
    assert!(
        resumable
            .adopt("claude", std::path::Path::new("/w"), "x")
            .is_none()
    );
    let fixed = fixture(1);
    assert!(
        fixed
            .tool
            .adopt("fake", std::path::Path::new("/w"), "x1")
            .is_none()
    );
}

#[tokio::test]
async fn the_model_cannot_stop_the_owners_own_job() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = recorded(root.path());
    fixture
        .tool
        .owner_call(
            json!({"prompt":"look around"}),
            std::env::temp_dir(),
            SendMode::Auto,
            JobMeta::default(),
        )
        .await
        .unwrap();
    let refused = fixture.jobs.cancel("job-1", "call-1").await.unwrap_err();
    assert!(refused.message.contains("user's own job"), "{refused}");
    assert!(!fixture.cancelled.load(Ordering::SeqCst));
    fixture.release.notify_one();
    settled(&mut fixture).await;
    assert_eq!(record(root.path(), "job-1").state, AgentState::Completed);
}

#[tokio::test]
async fn a_resumed_jobs_record_keeps_what_it_resumes_after_it_ends() {
    // What the workbench passes in is in the record from its first write,
    // so the job's own later writes keep it.
    let root = tempfile::tempdir().unwrap();
    let mut fixture = recorded(root.path());
    let worktree = scv_protocol::WorktreeRef {
        path: "/w/.scv/worktrees/shop-1a2b3c4d/codex-9f3e21".into(),
        branch: "scv/codex-9f3e21".into(),
        repo: "/w/shop".into(),
    };
    fixture
        .tool
        .owner_call(
            json!({"prompt":"again"}),
            std::env::temp_dir(),
            SendMode::Auto,
            JobMeta {
                resumed_from: Some("old/job-4".into()),
                worktree: Some(worktree.clone()),
                ..JobMeta::default()
            },
        )
        .await
        .unwrap();
    fixture.release.notify_one();
    settled(&mut fixture).await;
    let done = record(root.path(), "job-1");
    assert_eq!(done.state, AgentState::Completed);
    assert!(done.reported);
    assert_eq!(done.resumed_from.as_deref(), Some("old/job-4"));
    assert_eq!(done.worktree, Some(worktree));
    assert_eq!(done.started_by, scv_protocol::JobOrigin::Owner);
}

#[test]
fn jobs_are_numbered_after_the_ones_runs_name_without_records() {
    let root = tempfile::tempdir().unwrap();
    let ledger =
        JobLedger::new(root.path(), "session-1", "TUI", std::path::Path::new("/w")).unwrap();
    // No records are left; a run of the session still names job-4.
    let jobs = BackgroundJobs::new(1, None).with_ledger(ledger);
    assert_eq!(jobs.recorded_conversations(), [] as [String; 0]);
    jobs.continue_after(["job-2", "job-4", "job-x", "other-9"]);
    assert_eq!(jobs.state().next, 4);
    // Never lowered.
    jobs.continue_after(["job-1"]);
    assert_eq!(jobs.state().next, 4);
}

#[test]
fn a_session_back_under_its_id_numbers_conversations_after_its_records() {
    let root = tempfile::tempdir().unwrap();
    let ledger =
        JobLedger::new(root.path(), "session-1", "TUI", std::path::Path::new("/w")).unwrap();
    let mut first = ledger
        .record(&ledger::NewJob {
            job: "job-1",
            agent: "codex",
            task: "t",
            arguments: &json!({"prompt":"p"}),
            started_by: scv_protocol::JobOrigin::Owner,
            reviewed: false,
            queued: false,
        })
        .unwrap();
    first.conversation = Some("codex-3".into());
    ledger.save(&first);
    let mut second = first.clone();
    second.job = "job-2".into();
    second.conversation = None;
    second.arguments = json!({"session":"claude-ai-2"});
    ledger.save(&second);
    let mut names = ledger.conversations().unwrap();
    names.sort();
    assert_eq!(names, ["claude-ai-2", "codex-3"]);

    let accepts = Accepts {
        session: true,
        ..Accepts::default()
    };
    let tool = BackgroundCapable::plain(
        Arc::new(AgentTool::beside(
            "codex",
            Arc::new(Resumable),
            accepts,
            &[],
        )),
        Arc::new(BackgroundJobs::new(1, None).with_ledger(ledger)),
    );
    tool.conversations.continue_after(
        tool.jobs
            .recorded_conversations()
            .iter()
            .map(String::as_str),
    );
    let handle = tool
        .adopt("codex", std::path::Path::new("/w"), "019a-thread")
        .unwrap();
    assert_eq!(handle, "codex-4");
    assert!(tool.knows("codex-4"));
    assert!(!tool.knows("codex-3"));
}
