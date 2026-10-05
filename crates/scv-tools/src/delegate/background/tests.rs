//! Unit tests for `src/delegate/background.rs`.

use super::*;
use crate::{
    AgentDefaults, BusyConfig,
    args::Timeouts,
    delegate::{
        agent::{Accepts, Backend},
        conversation::{ConversationLimits, ConversationStore},
    },
};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Notify, Semaphore};

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

/// An agent whose conversations live in a real store, which refuses a turn
/// while another runs. Each turn records its prompt, then ends when a
/// release permit lets it.
struct Conversing {
    conversations: Arc<ConversationStore>,
    release: Arc<Semaphore>,
    prompts: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Backend for Conversing {
    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok("Continue the fake conversation.".into())
    }

    fn busy(&self, arguments: &Value) -> Result<bool, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        Ok(args
            .session
            .is_some_and(|handle| self.conversations.is_busy(&handle)))
    }

    async fn wait_idle(
        &self,
        arguments: &Value,
        cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        match args.session {
            Some(handle) => self.conversations.wait_idle(&handle, cancellation).await,
            None => Ok(()),
        }
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: AgentArgs = parse_args(&arguments)?;
        let turn = self.conversations.begin(
            "fake",
            args.session.as_deref(),
            std::path::Path::new("/w"),
            false,
        )?;
        lock(&self.prompts).push(args.prompt.clone());
        tokio::select! {
            permit = self.release.acquire() => {
                permit.unwrap().forget();
                let session = turn.finish(Some("thread".into()), true);
                Ok(ToolOutput::success(
                    json!({"agent":"fake","status":"completed","reply":args.prompt,"session":session})
                        .to_string(),
                ))
            }
            () = context.cancellation.cancelled() => Err(ToolError::cancelled("cancelled")),
        }
    }
}

/// A session offering [`Conversing`] as `fake`, whose conversations
/// `fake-1` and `fake-2` have each had a turn.
struct Session {
    tool: Arc<BackgroundCapable>,
    jobs: Arc<BackgroundJobs>,
    release: Arc<Semaphore>,
    prompts: Arc<Mutex<Vec<String>>>,
}

fn session(limit: usize, busy: BusyConfig) -> Session {
    let conversations = Arc::new(ConversationStore::new(
        ConversationLimits {
            max: 8,
            idle: Duration::from_secs(86400),
        },
        None,
    ));
    for _ in 0..2 {
        let turn = conversations
            .begin("fake", None, std::path::Path::new("/w"), false)
            .unwrap();
        turn.finish(Some("thread".into()), true).unwrap();
    }
    let release = Arc::new(Semaphore::new(0));
    let prompts = Arc::default();
    let offered = Offered {
        name: "fake".into(),
        backend: Arc::new(Conversing {
            conversations,
            release: Arc::clone(&release),
            prompts: Arc::clone(&prompts),
        }),
        accepts: Accepts {
            session: true,
            ..Accepts::default()
        },
        model_hint: String::new(),
        offered: None,
        use_for: None,
        defaults: AgentDefaults::default(),
        holds_settings: true,
        busy,
    };
    let timeouts = Timeouts {
        default: Duration::from_secs(60),
        max: Duration::from_secs(600),
    };
    let jobs = Arc::new(BackgroundJobs::new(limit, None));
    Session {
        tool: Arc::new(BackgroundCapable {
            inner: Arc::new(AgentTool::new(vec![offered], &["fake".into()], timeouts)),
            jobs: Arc::clone(&jobs),
        }),
        jobs,
        release,
        prompts,
    }
}

impl Session {
    async fn call(&self, arguments: Value) -> Result<Value, ToolError> {
        let output = self.tool.execute(arguments, context()).await?;
        Ok(serde_json::from_str(&output.content).unwrap())
    }

    /// Wait until the turns that ran had exactly `prompts`, in order.
    async fn ran(&self, prompts: &[&str]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock(&self.prompts).as_slice() != prompts {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("ran {:?}, not {prompts:?}", lock(&self.prompts)));
    }

    /// Let the running turn end, then wait until `next` runs.
    async fn release_then(&self, next: &[&str]) {
        self.release.add_permits(1);
        self.ran(next).await;
    }

    async fn result(&self, job: &str) -> Value {
        self.jobs
            .wait(job, Duration::from_secs(5), &CancellationToken::new(), "")
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_busy_conversation_queues_prompts_in_the_order_they_arrive() {
    let session = session(2, BusyConfig::default());
    let first = session
        .call(json!({"prompt":"a","session":"fake-1","background":true}))
        .await
        .unwrap();
    assert_eq!(first.get("queued"), None);
    // The first job has not begun its turn yet, and still these wait for it.
    let second = session
        .call(json!({"prompt":"b","session":"fake-1"}))
        .await
        .unwrap();
    assert_eq!(
        (&second["job"], &second["queued"]),
        (&json!("job-2"), &json!(true))
    );
    let third = session
        .call(json!({"prompt":"c","session":"fake-1","background":true}))
        .await
        .unwrap();
    assert_eq!(third["queued"], true);
    session.ran(&["a"]).await;
    let waiting = session.jobs.describe(Some("job-3"), "").unwrap();
    assert_eq!(waiting["status"], "running");
    assert!(
        waiting["progress"].as_str().unwrap().starts_with("queued"),
        "{waiting}"
    );
    session.release_then(&["a", "b"]).await;
    session.release_then(&["a", "b", "c"]).await;
    session.release.add_permits(1);
    for (job, reply) in [("job-1", "a"), ("job-2", "b"), ("job-3", "c")] {
        let result = session.result(job).await;
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(result["result"]["reply"], reply);
    }
}

#[tokio::test]
async fn queued_prompts_are_bounded_per_conversation_not_by_max_background() {
    let session = session(
        1,
        BusyConfig {
            max_queued_turns: 1,
            ..BusyConfig::default()
        },
    );
    session
        .call(json!({"prompt":"a","session":"fake-1","background":true}))
        .await
        .unwrap();
    let queued = session
        .call(json!({"prompt":"b","session":"fake-1","background":true}))
        .await
        .unwrap();
    assert_eq!(queued["queued"], true);
    let full = session
        .call(json!({"prompt":"c","session":"fake-1"}))
        .await
        .unwrap_err();
    assert!(full.message.contains("agent.max_queued_turns"), "{full}");
    // The one background job allowed runs already.
    let other = session
        .call(json!({"prompt":"d","session":"fake-2","background":true}))
        .await
        .unwrap_err();
    assert!(other.message.contains("agent.max_background"), "{other}");
    // Another conversation has a queue of its own, and runs beside the first.
    let running = tokio::spawn({
        let tool = Arc::clone(&session.tool);
        async move {
            tool.execute(json!({"prompt":"e","session":"fake-2"}), context())
                .await
        }
    });
    session.ran(&["a", "e"]).await;
    let queued = session
        .call(json!({"prompt":"f","session":"fake-2"}))
        .await
        .unwrap();
    assert_eq!(queued["queued"], true);
    session.release.add_permits(4);
    assert!(running.await.unwrap().is_ok());
    for (job, reply) in [("job-2", "b"), ("job-3", "f")] {
        assert_eq!(session.result(job).await["result"]["reply"], reply);
    }
}

#[tokio::test]
async fn a_call_never_overtakes_prompts_already_waiting() {
    let session = session(2, BusyConfig::default());
    session
        .call(json!({"prompt":"a","session":"fake-1","background":true}))
        .await
        .unwrap();
    session
        .call(json!({"prompt":"b","session":"fake-1"}))
        .await
        .unwrap();
    let refused = session
        .call(json!({"prompt":"x","session":"fake-1","on_busy":"fail"}))
        .await
        .unwrap_err();
    assert_eq!(
        refused.message,
        "session busy: conversation fake-1 is still running a turn, and 1 more prompts wait \
         for it"
    );
    // Waiting holds the call until the prompts ahead of it have run.
    let waiting = tokio::spawn({
        let tool = Arc::clone(&session.tool);
        async move {
            tool.execute(
                json!({"prompt":"c","session":"fake-1","on_busy":"wait"}),
                context(),
            )
            .await
        }
    });
    session.ran(&["a"]).await;
    session.release_then(&["a", "b"]).await;
    assert!(!waiting.is_finished());
    session.release_then(&["a", "b", "c"]).await;
    session.release.add_permits(1);
    let output = waiting.await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output.content).unwrap()["reply"],
        "c"
    );
    assert_eq!(session.result("job-2").await["status"], "completed");
}

#[tokio::test]
async fn cancelling_a_queued_prompt_frees_its_place_without_running_it() {
    let session = session(2, BusyConfig::default());
    for prompt in ["a", "b", "c"] {
        session
            .call(json!({"prompt":prompt,"session":"fake-1","background":true}))
            .await
            .unwrap();
    }
    session.ran(&["a"]).await;
    let cancelled = session.jobs.cancel("job-2", "").await.unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    session.release_then(&["a", "c"]).await;
    session.release.add_permits(1);
    assert_eq!(session.result("job-3").await["status"], "completed");
    assert_eq!(lock(&session.prompts).as_slice(), ["a", "c"]);
}

#[tokio::test]
async fn background_calls_and_steering_fall_back_to_the_busy_policy() {
    let session = session(
        2,
        BusyConfig {
            behavior: BusyBehavior::Steer,
            steer_fallback: BusyBehavior::Fail,
            max_queued_turns: 4,
        },
    );
    session
        .call(json!({"prompt":"a","session":"fake-1","background":true}))
        .await
        .unwrap();
    // This agent cannot steer, so its fallback refuses the call.
    for arguments in [
        json!({"prompt":"b","session":"fake-1"}),
        json!({"prompt":"b","session":"fake-1","background":true}),
        json!({"prompt":"b","session":"fake-1","on_busy":"fail","background":true}),
    ] {
        let error = session.call(arguments).await.unwrap_err();
        assert!(error.message.starts_with("session busy"), "{error}");
    }
    // A background call that would wait queues instead; a blank policy is
    // the agent's own.
    let queued = session
        .call(json!({"prompt":"b","session":"fake-1","on_busy":"wait","background":true}))
        .await
        .unwrap();
    assert_eq!(queued["queued"], true);
    let error = session
        .call(json!({"prompt":"c","session":"fake-1","on_busy":""}))
        .await
        .unwrap_err();
    assert!(error.message.starts_with("session busy"), "{error}");
    // A steer fallback of steer queues rather than failing.
    let session = self::session(
        2,
        BusyConfig {
            behavior: BusyBehavior::Steer,
            steer_fallback: BusyBehavior::Steer,
            max_queued_turns: 4,
        },
    );
    session
        .call(json!({"prompt":"a","session":"fake-1","background":true}))
        .await
        .unwrap();
    let queued = session
        .call(json!({"prompt":"b","session":"fake-1"}))
        .await
        .unwrap();
    assert_eq!(queued["queued"], true);
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
        prompt.contains("job-1 (fake, conversation fake-1): completed\nTask: work\nall done"),
        "{prompt}"
    );
    // Its report turn has it; no other takes it meanwhile.
    assert!(fixture.jobs.take_unreported().is_empty(), "reported twice");
    assert_eq!(fixture.jobs.pending(), 1);
    // The turn completed: the model has seen it.
    fixture.jobs.report_settled(&["job-1".to_owned()]);
    assert_eq!(fixture.jobs.pending(), 0);
    assert!(fixture.jobs.take_unreported().is_empty(), "reported twice");
}

/// A fixture whose job-1 has finished, unseen by the model.
async fn finished_job() -> Fixture {
    let mut fixture = fixture(2);
    start(&fixture.tool).await;
    fixture.release.notify_one();
    fixture.finished.recv().await.unwrap();
    fixture
}

fn job_1_only() -> Vec<String> {
    vec!["job-1".to_owned()]
}

#[tokio::test(start_paused = true)]
async fn a_failed_report_is_due_again_after_a_delay_then_reported_directly() {
    let fixture = finished_job().await;
    let jobs = &fixture.jobs;
    assert_eq!(jobs.take_unreported().len(), 1);
    assert_eq!(
        jobs.report_failed(&job_1_only(), true),
        ReportFailure::Retry(Duration::from_secs(30))
    );
    // Still unreported, and so still holding a planned restart, but not due.
    assert_eq!(jobs.pending(), 1);
    assert!(jobs.take_unreported().is_empty());
    let due = jobs.next_retry().unwrap();
    assert_eq!(due - tokio::time::Instant::now(), Duration::from_secs(30));

    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(jobs.take_unreported().len(), 1);
    // The turn that has it is not waiting for anything.
    assert_eq!(jobs.next_retry(), None);
    assert_eq!(
        jobs.report_failed(&job_1_only(), true),
        ReportFailure::Retry(Duration::from_secs(120))
    );
    tokio::time::advance(Duration::from_secs(119)).await;
    assert!(jobs.take_unreported().is_empty());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(jobs.take_unreported().len(), 1);

    // The third failure is the last: the result goes to the user directly.
    let ReportFailure::GiveUp { attempts, reports } = jobs.report_failed(&job_1_only(), true)
    else {
        panic!("the third failed report turn is not retried");
    };
    assert_eq!(attempts, 3);
    assert_eq!(
        reports,
        [JobReport {
            job: "job-1".into(),
            agent: "fake".into(),
            task: "work".into(),
            status: JobStatus::Completed,
            session: Some("fake-1".into()),
            reply: "all done".into(),
        }]
    );
    assert_eq!(jobs.pending(), 0);
    assert_eq!(jobs.next_retry(), None);
    assert!(jobs.take_unreported().is_empty());
    // It is still there to look up, and looking settles nothing new.
    let status = jobs.describe(Some("job-1"), "call-1").unwrap();
    assert_eq!(status["result"]["reply"], "all done");
    assert!(jobs.take_changes("call-1").is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_must_not_be_retried_is_reported_directly_at_once() {
    let fixture = finished_job().await;
    assert_eq!(fixture.jobs.take_unreported().len(), 1);
    let ReportFailure::GiveUp { attempts, reports } =
        fixture.jobs.report_failed(&job_1_only(), false)
    else {
        panic!("a failure that must not be retried is not retried");
    };
    assert_eq!((attempts, reports.len()), (1, 1));
    assert_eq!(fixture.jobs.pending(), 0);
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(fixture.jobs.take_unreported().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_report_turn_settles_its_jobs() {
    let fixture = finished_job().await;
    assert_eq!(fixture.jobs.take_unreported().len(), 1);
    fixture.jobs.report_settled(&job_1_only());
    assert_eq!(fixture.jobs.pending(), 0);
    // A late failure of the same turn changes nothing.
    assert_eq!(
        fixture.jobs.report_failed(&job_1_only(), true),
        ReportFailure::Settled
    );
    assert!(fixture.jobs.take_unreported().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_succeeds_makes_a_waiting_report_due_at_once() {
    let fixture = finished_job().await;
    let jobs = &fixture.jobs;
    // Nothing waits yet.
    assert!(!jobs.retry_now());
    jobs.take_unreported();
    jobs.report_failed(&job_1_only(), true);
    assert!(jobs.take_unreported().is_empty());
    assert!(jobs.retry_now());
    assert!(!jobs.retry_now());
    assert_eq!(jobs.next_retry(), None);
    // It keeps the failures it had: one more try is left after this one.
    assert_eq!(jobs.take_unreported().len(), 1);
    assert_eq!(
        jobs.report_failed(&job_1_only(), true),
        ReportFailure::Retry(Duration::from_secs(120))
    );
}

#[tokio::test(start_paused = true)]
async fn a_job_the_model_looks_up_needs_no_report_however_its_turn_ends() {
    // Looked up while its report waits to be tried again.
    let fixture = finished_job().await;
    fixture.jobs.take_unreported();
    fixture.jobs.report_failed(&job_1_only(), true);
    fixture.jobs.describe(Some("job-1"), "call-1").unwrap();
    assert_eq!(
        fixture.jobs.take_changes("call-1"),
        [job_1(JobStatus::Completed)]
    );
    assert_eq!(fixture.jobs.pending(), 0);
    assert_eq!(fixture.jobs.next_retry(), None);
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(fixture.jobs.take_unreported().is_empty());

    // Looked up during its own report turn, which then fails.
    let fixture = finished_job().await;
    fixture.jobs.take_unreported();
    fixture.jobs.describe(Some("job-1"), "call-1").unwrap();
    assert_eq!(
        fixture.jobs.report_failed(&job_1_only(), true),
        ReportFailure::Settled
    );
    assert_eq!(fixture.jobs.pending(), 0);
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
