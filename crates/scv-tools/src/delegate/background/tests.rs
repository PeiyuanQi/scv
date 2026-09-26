//! Unit tests for `src/delegate/background.rs`.

use super::*;
use crate::delegate::agent::{Accepts, Backend};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// An agent that reports one progress line, then finishes when released
/// or stops when cancelled.
struct FakeAgent {
    release: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl Backend for FakeAgent {
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        if arguments.get("prompt").and_then(Value::as_str).is_none() {
            return Err(ToolError::failed("prompt is required"));
        }
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok("Launch the fake agent.".into())
    }

    async fn execute(
        &self,
        _arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        context.progress.report("$ step one");
        tokio::select! {
            () = self.release.notified() => Ok(ToolOutput::success(
                json!({"agent":"fake","status":"completed","reply":"all done","session":"fake-1"})
                    .to_string(),
            )),
            () = context.cancellation.cancelled() => {
                self.cancelled.store(true, Ordering::SeqCst);
                Err(ToolError::cancelled("cancelled"))
            }
        }
    }
}

struct Fixture {
    tool: BackgroundCapable,
    jobs: Arc<BackgroundJobs>,
    release: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
    finished: mpsc::UnboundedReceiver<()>,
}

fn fixture(limit: usize) -> Fixture {
    let (finished_tx, finished) = mpsc::unbounded_channel();
    let jobs = Arc::new(BackgroundJobs::new(limit, Some(finished_tx)));
    let release = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let tool = BackgroundCapable {
        inner: offering(FakeAgent {
            release: Arc::clone(&release),
            cancelled: Arc::clone(&cancelled),
        }),
        jobs: Arc::clone(&jobs),
    };
    Fixture {
        tool,
        jobs,
        release,
        cancelled,
        finished,
    }
}

/// The `agent` tool offering `backend` as `fake`, its default, beside
/// `codex`.
fn offering(backend: impl Backend + 'static) -> Arc<AgentTool> {
    Arc::new(AgentTool::beside(
        "fake",
        Arc::new(backend),
        Accepts::default(),
        &["codex"],
    ))
}

fn context() -> ToolContext {
    ToolContext::new(std::env::temp_dir(), CancellationToken::new())
}

/// The context of the model's call `call_id`.
fn call(call_id: &str) -> ToolContext {
    ToolContext {
        call_id: call_id.into(),
        ..context()
    }
}

async fn start(tool: &BackgroundCapable) -> Value {
    start_as(tool, "").await
}

/// Start a job as the model's call `call_id`.
async fn start_as(tool: &BackgroundCapable, call_id: &str) -> Value {
    let output = tool
        .execute(
            json!({"prompt":"work\nthen report","background":true}),
            call(call_id),
        )
        .await
        .unwrap();
    serde_json::from_str(&output.content).unwrap()
}

/// `job-1` of the fake agent, as a change with `status`.
fn job_1(status: JobStatus) -> JobChange {
    JobChange {
        job: "job-1".into(),
        tool: "agent".into(),
        agent: "fake".into(),
        status,
        task: "work".into(),
    }
}

#[tokio::test]
async fn background_calls_return_a_job_that_wait_and_status_observe() {
    let mut fixture = fixture(2);
    let started = start_as(&fixture.tool, "call-1").await;
    assert_eq!(
        started,
        json!({
            "job":"job-1","agent":"fake","status":"running","background":true,
            "note":started["note"]
        })
    );
    // The starting call reports the job to the session's clients, once.
    assert_eq!(
        fixture.jobs.take_changes("call-1"),
        [job_1(JobStatus::Running)]
    );
    assert!(fixture.jobs.take_changes("call-1").is_empty());
    // Running, with the job's latest progress.
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = fixture.jobs.describe(Some("job-1"), "call-2").unwrap();
            if status.get("progress").is_some() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status["status"], "running");
    assert_eq!(status["progress"], "$ step one");
    // A short wait returns it still running.
    let waited = fixture
        .jobs
        .wait(
            "job-1",
            Duration::from_millis(50),
            &CancellationToken::new(),
            "call-3",
        )
        .await
        .unwrap();
    assert_eq!(waited["status"], "running");
    // Seeing it run changes nothing for the clients.
    assert!(fixture.jobs.take_changes("call-2").is_empty());
    assert!(fixture.jobs.take_changes("call-3").is_empty());

    fixture.release.notify_one();
    let waited = fixture
        .jobs
        .wait(
            "job-1",
            Duration::from_secs(5),
            &CancellationToken::new(),
            "call-4",
        )
        .await
        .unwrap();
    assert_eq!(waited["status"], "completed");
    assert_eq!(waited["result"]["reply"], "all done");
    assert_eq!(waited["result"]["session"], "fake-1");
    // The call that showed the model the result settles the job.
    assert_eq!(
        fixture.jobs.take_changes("call-4"),
        [job_1(JobStatus::Completed)]
    );
    // The session was woken, but the model already saw the result.
    fixture.finished.recv().await.unwrap();
    assert!(fixture.jobs.take_unreported().is_empty());
    let all = fixture.jobs.describe(None, "call-5").unwrap();
    assert_eq!(all["jobs"][0]["job"], "job-1");
    // A result seen before is not settled again.
    assert!(fixture.jobs.take_changes("call-5").is_empty());
}

#[tokio::test]
async fn listing_jobs_settles_only_the_finished_ones_the_model_had_not_seen() {
    let mut fixture = fixture(2);
    start_as(&fixture.tool, "call-1").await;
    fixture.release.notify_one();
    fixture.finished.recv().await.unwrap();
    start_as(&fixture.tool, "call-2").await;
    let listed = fixture.jobs.describe(None, "call-3").unwrap();
    assert_eq!(listed["jobs"][0]["status"], "completed");
    assert_eq!(listed["jobs"][1]["status"], "running");
    assert_eq!(
        fixture.jobs.take_changes("call-3"),
        [job_1(JobStatus::Completed)]
    );
    // The listing replaced the report turn.
    assert!(fixture.jobs.take_unreported().is_empty());
    // Changes stay with the call that made them.
    assert_eq!(fixture.jobs.take_changes("call-2").len(), 1);
    assert_eq!(fixture.jobs.take_changes("call-1").len(), 1);
}

#[tokio::test]
async fn a_finished_job_nobody_looked_at_is_reported_once() {
    let mut fixture = fixture(2);
    start(&fixture.tool).await;
    fixture.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), fixture.finished.recv())
        .await
        .unwrap()
        .unwrap();
    let reports = fixture.jobs.take_unreported();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(
        (
            report.job.as_str(),
            report.agent.as_str(),
            report.status.as_str(),
            report.session.as_deref(),
            report.reply.as_str()
        ),
        ("job-1", "fake", "completed", Some("fake-1"), "all done")
    );
    let prompt = report_prompt(&reports);
    assert!(prompt.starts_with("[SCV background report]"), "{prompt}");
    assert!(
        prompt.contains("job-1 (fake, conversation fake-1): completed\nall done"),
        "{prompt}"
    );
    assert!(fixture.jobs.take_unreported().is_empty(), "reported twice");
}

#[tokio::test]
async fn at_most_the_limit_runs_at_once() {
    let mut fixture = fixture(1);
    start(&fixture.tool).await;
    let refused = fixture
        .tool
        .execute(json!({"prompt":"more","background":true}), context())
        .await
        .unwrap_err();
    assert!(
        refused.message.contains("agent.max_background"),
        "{refused}"
    );
    fixture.release.notify_one();
    fixture.finished.recv().await.unwrap();
    assert_eq!(start(&fixture.tool).await["job"], "job-2");
    assert_eq!(fixture.jobs.running(), 1);
}

#[tokio::test]
async fn dropping_the_session_store_cancels_running_jobs() {
    let fixture = fixture(2);
    start(&fixture.tool).await;
    let cancelled = Arc::clone(&fixture.cancelled);
    drop(fixture);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the job was cancelled with its session");
}

#[tokio::test]
async fn foreground_calls_pass_through_and_the_schema_offers_background() {
    let fixture = fixture(2);
    let spec = fixture.tool.spec();
    assert_eq!(spec.name, "agent");
    assert_eq!(
        spec.parameters["properties"]["background"]["type"],
        "boolean"
    );
    assert!(
        spec.description.contains("at most 2 running"),
        "{}",
        spec.description
    );
    let summary = fixture
        .tool
        .approval_summary(&json!({"prompt":"work","background":true}))
        .unwrap();
    assert!(summary.starts_with("agent fake: "), "{summary}");
    assert!(summary.contains("Runs in the background"), "{summary}");
    assert!(
        !fixture
            .tool
            .approval_summary(&json!({"prompt":"work"}))
            .unwrap()
            .contains("background")
    );
    // A bad call fails at once instead of returning a job.
    assert!(
        fixture
            .tool
            .execute(json!({"background":true}), context())
            .await
            .is_err()
    );
    fixture.release.notify_one();
    let output = fixture
        .tool
        .execute(json!({"prompt":"work","background":false}), context())
        .await
        .unwrap();
    assert!(output.content.contains("all done"));
    assert_eq!(fixture.jobs.running(), 0);
    assert!(fixture.jobs.describe(Some("job-1"), "").is_err());
}

#[tokio::test]
async fn agent_cancel_stops_a_running_job_without_a_report() {
    let mut fixture = fixture(2);
    start(&fixture.tool).await;
    let cancel = CancelTool {
        jobs: Arc::clone(&fixture.jobs),
    };
    assert_eq!(
        cancel.risk(&json!({"job":"job-1"})).unwrap(),
        ToolRisk::Process
    );
    assert!(cancel.risk(&json!({})).is_err());
    let output = cancel
        .execute(json!({"job":"job-1"}), call("call-9"))
        .await
        .unwrap();
    let stopped: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(stopped["status"], "cancelled");
    assert!(fixture.cancelled.load(Ordering::SeqCst), "the agent saw it");
    // No report follows, so the stop settles it.
    assert_eq!(
        fixture.jobs.take_changes("call-9"),
        [job_1(JobStatus::Cancelled)]
    );
    // The session is woken, but the model asked for the stop.
    fixture.finished.recv().await.unwrap();
    assert!(fixture.jobs.take_unreported().is_empty());
    assert_eq!(fixture.jobs.running(), 0);
    let again = cancel
        .execute(json!({"job":"job-1"}), call("call-10"))
        .await
        .unwrap();
    assert!(
        again.content.contains("already finished"),
        "{}",
        again.content
    );
    assert!(fixture.jobs.take_changes("call-10").is_empty());
    let unknown = cancel
        .execute(json!({"job":"job-9"}), context())
        .await
        .unwrap_err();
    assert!(
        unknown.message.contains("unknown background job"),
        "{unknown}"
    );
    // The freed slot takes a new job.
    assert_eq!(start(&fixture.tool).await["job"], "job-2");
}

/// An agent that asks its session to approve one nested command and
/// replies with the answer.
struct AskingAgent;

#[async_trait]
impl Backend for AskingAgent {
    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok("Launch the asking agent.".into())
    }

    async fn execute(
        &self,
        _arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let approved = context
            .approvals
            .request(
                "bash",
                ToolRisk::Process,
                context.workspace.clone(),
                "cargo test",
                context.cancellation.clone(),
            )
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?;
        Ok(ToolOutput::success(
            json!({"status":"completed","reply":if approved {"approved"} else {"denied"}})
                .to_string(),
        ))
    }
}

struct FixedGate {
    approve: bool,
    seen: Mutex<Vec<(String, ToolRisk)>>,
}

#[async_trait]
impl ApprovalGate for FixedGate {
    async fn approve(
        &self,
        request: scv_core::ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, scv_core::AgentError> {
        self.seen
            .lock()
            .unwrap()
            .push((request.call_id, request.risk));
        Ok(self.approve)
    }
}

async fn nested_answer(gate: Option<Arc<dyn ApprovalGate>>) -> String {
    let (finished_tx, mut finished) = mpsc::unbounded_channel();
    let mut jobs = BackgroundJobs::new(2, Some(finished_tx));
    if let Some(gate) = gate {
        jobs = jobs.with_approvals(gate);
    }
    let jobs = Arc::new(jobs);
    let tool = BackgroundCapable {
        inner: offering(AskingAgent),
        jobs: Arc::clone(&jobs),
    };
    tool.execute(json!({"prompt":"ask","background":true}), context())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), finished.recv())
        .await
        .unwrap()
        .unwrap();
    jobs.take_unreported().remove(0).reply
}

#[tokio::test]
async fn background_approvals_follow_the_session_gate_or_are_denied() {
    // Without a gate nobody can answer, so the request is denied.
    assert_eq!(nested_answer(None).await, "denied");
    for approve in [true, false] {
        let gate = Arc::new(FixedGate {
            approve,
            seen: Mutex::default(),
        });
        let expected = if approve { "approved" } else { "denied" };
        assert_eq!(
            nested_answer(Some(Arc::clone(&gate) as Arc<dyn ApprovalGate>)).await,
            expected
        );
        // The gate saw the nested call's risk, filed under the job.
        assert_eq!(
            *gate.seen.lock().unwrap(),
            [("job-1".to_owned(), ToolRisk::Process)]
        );
    }
}

#[tokio::test]
async fn wait_and_status_tools_validate_and_are_read_only() {
    let fixture = fixture(2);
    let wait = WaitTool {
        jobs: Arc::clone(&fixture.jobs),
        timeouts: Timeouts {
            default: Duration::from_secs(60),
            max: Duration::from_secs(120),
        },
    };
    assert_eq!(
        wait.risk(&json!({"job":"job-1"})).unwrap(),
        ToolRisk::ReadOnly
    );
    assert!(
        wait.risk(&json!({"job":"job-1","timeout_seconds":121}))
            .is_err()
    );
    let unknown = wait
        .execute(json!({"job":"job-9"}), context())
        .await
        .unwrap_err();
    assert!(
        unknown.message.contains("unknown background job"),
        "{unknown}"
    );
    let status = StatusTool {
        jobs: Arc::clone(&fixture.jobs),
    };
    assert_eq!(status.risk(&json!({})).unwrap(), ToolRisk::ReadOnly);
    let empty = status.execute(json!({}), context()).await.unwrap();
    assert_eq!(empty.content, json!({"jobs":[]}).to_string());
}

#[tokio::test]
async fn a_background_call_runs_the_agent_it_names_and_a_refused_one_starts_no_job() {
    let mut fixture = fixture(2);
    // The dispatcher refuses an agent the session does not offer, or an
    // option the agent does not take, before any job starts.
    for refused in [
        json!({"agent":"zcode","prompt":"work","background":true}),
        json!({"agent":"fake","prompt":"work","model":"m","background":true}),
    ] {
        let error = fixture
            .tool
            .execute(refused, call("call-0"))
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            scv_core::ToolFailure::InvalidArguments,
            "{error}"
        );
    }
    assert_eq!(fixture.jobs.running(), 0);
    assert!(fixture.jobs.take_changes("call-0").is_empty());
    // The job belongs to the agent the call named, however it is named.
    let started = fixture
        .tool
        .execute(
            json!({"agent":"fake","prompt":"work","background":true}),
            call("call-1"),
        )
        .await
        .unwrap();
    let started: Value = serde_json::from_str(&started.content).unwrap();
    assert_eq!(started["agent"], "fake");
    let [change] = fixture.jobs.take_changes("call-1").try_into().unwrap();
    assert_eq!(
        (change.tool.as_str(), change.agent_name()),
        ("agent", "fake")
    );
    let listed = fixture.jobs.describe(None, "call-2").unwrap();
    assert_eq!(listed["jobs"][0]["agent"], "fake");
    assert!(listed["jobs"][0].get("tool").is_none(), "{listed}");
    fixture.release.notify_one();
    fixture.finished.recv().await.unwrap();
    assert_eq!(fixture.jobs.take_unreported()[0].agent, "fake");
}

#[test]
fn a_task_is_the_first_line_of_the_prompt_shortened() {
    assert_eq!(task_line("\n  Fix the build\nthen test"), "Fix the build");
    let long = "x".repeat(200);
    let task = task_line(&long);
    assert_eq!(task.chars().count(), TASK_CHARS + 1);
    assert!(task.ends_with('…'));
    assert_eq!(task_line(""), "");
}
