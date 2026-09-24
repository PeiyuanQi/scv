//! Background delegations: an `agent_*` call with `background: true` returns a
//! job handle at once while the agent keeps working; `agent_wait` and
//! `agent_status` observe the job, and the session is told when it finishes
//! so the server can report it in a turn of its own.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use scv_core::{ProgressSink, Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{Timeouts, bounded, parse_args, timeout_schema};

/// Finished jobs a session keeps for `agent_status` beyond the running ones.
const MAX_FINISHED: usize = 16;
/// Characters of a finished job's reply quoted in a server-started report turn.
const REPORT_REPLY_CHARS: usize = 6000;
/// Jobs one report turn covers; any more wait for the next.
const REPORT_MAX_JOBS: usize = 4;

/// One session's background jobs. Dropping the store (with the session's
/// tools) cancels every job still running.
pub struct BackgroundJobs {
    limit: usize,
    state: Mutex<JobsState>,
    cancellation: CancellationToken,
    /// Woken whenever a job finishes, so the session can report it.
    finished: Option<mpsc::UnboundedSender<()>>,
}

impl std::fmt::Debug for BackgroundJobs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackgroundJobs")
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct JobsState {
    next: u64,
    jobs: Vec<Job>,
}

struct Job {
    id: String,
    tool: String,
    started: Instant,
    progress: ProgressSink,
    last_progress: Option<String>,
    outcome: Option<Outcome>,
    /// The model has seen the result (through `agent_wait`, `agent_status`,
    /// or a report turn), so it needs no report turn.
    reported: bool,
    done: watch::Receiver<bool>,
}

struct Outcome {
    output: ToolOutput,
    elapsed: Duration,
}

/// A finished job not yet seen by the model, for a server-started report turn.
#[derive(Debug, Clone)]
pub struct JobReport {
    pub job: String,
    pub tool: String,
    pub status: String,
    pub session: Option<String>,
    pub reply: String,
}

impl Drop for BackgroundJobs {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl BackgroundJobs {
    /// At most `limit` jobs run at once. `finished` is woken as jobs finish.
    pub fn new(limit: usize, finished: Option<mpsc::UnboundedSender<()>>) -> Self {
        Self {
            limit,
            state: Mutex::default(),
            cancellation: CancellationToken::new(),
            finished,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    fn state(&self) -> std::sync::MutexGuard<'_, JobsState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start `tool` with `arguments` in the background and return its job's
    /// `{"job","status":"running","background":true}` description.
    fn start(
        self: &Arc<Self>,
        tool: Arc<dyn Tool>,
        name: &str,
        arguments: Value,
        workspace: PathBuf,
    ) -> Result<Value, ToolError> {
        let (id, progress, done_tx, cancellation) = {
            let mut state = self.state();
            let running = state
                .jobs
                .iter()
                .filter(|job| job.outcome.is_none())
                .count();
            if running >= self.limit {
                return Err(ToolError(format!(
                    "{running} background jobs are already running, the limit \
                     (agent.max_background); wait for one with agent_wait first"
                )));
            }
            state.next += 1;
            let id = format!("job-{}", state.next);
            let progress = ProgressSink::buffered();
            let (done_tx, done) = watch::channel(false);
            state.jobs.push(Job {
                id: id.clone(),
                tool: name.to_owned(),
                started: Instant::now(),
                progress: progress.clone(),
                last_progress: None,
                outcome: None,
                reported: false,
                done,
            });
            (id, progress, done_tx, self.cancellation.child_token())
        };
        let jobs = Arc::downgrade(self);
        let job = id.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let mut context = ToolContext::new(workspace, cancellation);
            // A background job cannot ask anyone: nested approval requests
            // are denied, so it runs on the agent's own permissions.
            context.progress = progress;
            let output = tool
                .execute(arguments, context)
                .await
                .unwrap_or_else(|error| ToolOutput::failure(error.to_string()));
            finish(&jobs, &job, output, started.elapsed());
            let _ = done_tx.send(true);
        });
        Ok(json!({
            "job": id,
            "tool": name,
            "status": "running",
            "background": true,
            "note": "The agent is working in the background. SCV reports the result when it \
                     finishes; agent_wait blocks for it and agent_status shows its progress."
        }))
    }

    /// Wait up to `limit` for `job` and describe it; a finished job is then
    /// marked seen.
    async fn wait(
        &self,
        job: &str,
        limit: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Value, ToolError> {
        let mut done = self
            .state()
            .jobs
            .iter()
            .find(|candidate| candidate.id == job)
            .map(|candidate| candidate.done.clone())
            .ok_or_else(|| unknown_job(job))?;
        tokio::select! {
            _ = cancellation.cancelled() => return Err(ToolError("wait cancelled".into())),
            _ = tokio::time::timeout(limit, done.wait_for(|finished| *finished)) => {}
        }
        self.describe(Some(job))
    }

    /// Describe one job, or every job this session remembers; finished jobs
    /// described are marked seen.
    fn describe(&self, job: Option<&str>) -> Result<Value, ToolError> {
        let mut state = self.state();
        if let Some(job) = job {
            let entry = state
                .jobs
                .iter_mut()
                .find(|candidate| candidate.id == job)
                .ok_or_else(|| unknown_job(job))?;
            return Ok(entry.describe());
        }
        let jobs: Vec<Value> = state.jobs.iter_mut().map(Job::describe).collect();
        Ok(json!({ "jobs": jobs }))
    }

    /// Finished jobs the model has not seen yet, marked seen, for a report
    /// turn. At most a few per call; the rest stay for the next.
    pub fn take_unreported(&self) -> Vec<JobReport> {
        let mut state = self.state();
        state
            .jobs
            .iter_mut()
            .filter(|job| job.outcome.is_some() && !job.reported)
            .take(REPORT_MAX_JOBS)
            .map(|job| {
                job.reported = true;
                let outcome = job.outcome.as_ref().expect("filtered on outcome");
                let result = result_value(&outcome.output);
                JobReport {
                    job: job.id.clone(),
                    tool: job.tool.clone(),
                    status: job_status(&outcome.output, &result),
                    session: result
                        .get("session")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    reply: bounded(
                        result
                            .get("reply")
                            .and_then(Value::as_str)
                            .unwrap_or(outcome.output.content.as_str()),
                        REPORT_REPLY_CHARS,
                    ),
                }
            })
            .collect()
    }

    /// Whether any job is still running.
    pub fn running(&self) -> usize {
        self.state()
            .jobs
            .iter()
            .filter(|job| job.outcome.is_none())
            .count()
    }
}

fn finish(jobs: &Weak<BackgroundJobs>, id: &str, output: ToolOutput, elapsed: Duration) {
    // The session ended first: its jobs were cancelled with it.
    let Some(jobs) = jobs.upgrade() else {
        return;
    };
    {
        let mut state = jobs.state();
        if let Some(job) = state.jobs.iter_mut().find(|job| job.id == id) {
            job.last_progress = job.progress.take().or(job.last_progress.take());
            job.outcome = Some(Outcome { output, elapsed });
        }
        // Keep every running job and the newest finished ones.
        let finished = state
            .jobs
            .iter()
            .filter(|job| job.outcome.is_some())
            .count();
        let mut excess = finished.saturating_sub(MAX_FINISHED);
        state.jobs.retain(|job| {
            if excess > 0 && job.outcome.is_some() && job.reported {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
    if let Some(finished) = &jobs.finished {
        let _ = finished.send(());
    }
}

impl Job {
    fn describe(&mut self) -> Value {
        if let Some(line) = self.progress.take() {
            self.last_progress = Some(line);
        }
        let mut value = Map::new();
        value.insert("job".into(), self.id.clone().into());
        value.insert("tool".into(), self.tool.clone().into());
        match &self.outcome {
            None => {
                value.insert("status".into(), "running".into());
                value.insert(
                    "elapsed_seconds".into(),
                    self.started.elapsed().as_secs().into(),
                );
                if let Some(progress) = &self.last_progress {
                    value.insert("progress".into(), progress.clone().into());
                }
            }
            Some(outcome) => {
                self.reported = true;
                let result = result_value(&outcome.output);
                value.insert("status".into(), job_status(&outcome.output, &result).into());
                value.insert("elapsed_seconds".into(), outcome.elapsed.as_secs().into());
                value.insert("result".into(), result);
            }
        }
        Value::Object(value)
    }
}

/// The agent tool's structured result, or its text when it was not JSON.
fn result_value(output: &ToolOutput) -> Value {
    match serde_json::from_str::<Value>(&output.content) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ "reply": output.content, "is_error": output.is_error }),
    }
}

fn job_status(output: &ToolOutput, result: &Value) -> String {
    result
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if output.is_error {
                "failed".into()
            } else {
                "completed".into()
            }
        })
}

fn unknown_job(job: &str) -> ToolError {
    ToolError(format!(
        "unknown background job {:?}; agent_status lists this session's jobs",
        bounded(job, 64)
    ))
}

/// The server-started turn that reports finished jobs to the model.
pub fn report_prompt(reports: &[JobReport]) -> String {
    let mut prompt = String::from(
        "[SCV background report] Delegated work you started in the background has \
         finished. The user did not send this message: tell them briefly what \
         happened and the key result.\n",
    );
    for report in reports {
        prompt.push_str(&format!(
            "\n{} ({}{}): {}\n{}\n",
            report.job,
            report.tool,
            report
                .session
                .as_deref()
                .map_or_else(String::new, |session| format!(", conversation {session}")),
            report.status,
            report.reply.trim()
        ));
    }
    prompt
}

/// An agent tool that can also run in the background.
pub(crate) struct BackgroundCapable {
    pub(crate) inner: Arc<dyn Tool>,
    pub(crate) jobs: Arc<BackgroundJobs>,
}

/// Split `background` off an agent call's arguments.
fn split_background(arguments: &Value) -> (Value, bool) {
    let mut arguments = arguments.clone();
    let background = arguments
        .as_object_mut()
        .and_then(|object| object.remove("background"))
        .is_some_and(|value| value.as_bool() == Some(true));
    (arguments, background)
}

#[async_trait]
impl Tool for BackgroundCapable {
    fn spec(&self) -> ToolSpec {
        let mut spec = self.inner.spec();
        if let Some(properties) = spec
            .parameters
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            properties.insert(
                "background".into(),
                json!({
                    "type":"boolean",
                    "description":"Run in the background: return a job handle at once and keep \
                        working with the user while the agent runs. SCV reports the result when \
                        it finishes. Use it for long work such as landing or releasing a change."
                }),
            );
        }
        spec.description.push_str(&format!(
            " Set background to true for long work: the call returns a job handle at once \
             (at most {} running per session), SCV reports the result when the job finishes, \
             and agent_wait / agent_status observe it meanwhile.",
            self.jobs.limit()
        ));
        spec
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        self.inner.risk(&split_background(arguments).0)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let (arguments, background) = split_background(arguments);
        let mut summary = self.inner.approval_summary(&arguments)?;
        if background {
            summary.push_str(
                " Runs in the background: the call returns at once and the result is \
                 reported when the agent finishes.",
            );
        }
        Ok(summary)
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let (arguments, background) = split_background(&arguments);
        if !background {
            return self.inner.execute(arguments, context).await;
        }
        // Validate before returning a job handle, so a bad call fails now.
        self.inner.risk(&arguments)?;
        let name = self.inner.spec().name;
        let started =
            self.jobs
                .start(Arc::clone(&self.inner), &name, arguments, context.workspace)?;
        Ok(ToolOutput::success(started.to_string()))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    job: String,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    job: Option<String>,
}

/// `agent_wait`: block until a background job finishes, or the timeout.
pub(crate) struct WaitTool {
    pub(crate) jobs: Arc<BackgroundJobs>,
    pub(crate) timeouts: Timeouts,
}

#[async_trait]
impl Tool for WaitTool {
    fn spec(&self) -> ToolSpec {
        let mut timeout = timeout_schema(self.timeouts);
        timeout["description"] = format!(
            "Seconds to wait before returning the job still running. Defaults to {}; at most {}.",
            self.timeouts.default.min(self.timeouts.max).as_secs(),
            self.timeouts.max.as_secs()
        )
        .into();
        ToolSpec {
            name: "agent_wait".into(),
            description: "Wait for a background agent job (the `job` handle an agent_* call with \
                background: true returned) to finish, and return its result. Returns early with \
                status running when the timeout passes."
                .into(),
            parameters: json!({
                "type":"object",
                "properties":{"job":{"type":"string"},"timeout_seconds":timeout},
                "required":["job"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: WaitArgs = parse_args(arguments)?;
        self.timeouts.resolve(args.timeout_seconds)?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: WaitArgs = parse_args(arguments)?;
        Ok(format!(
            "Wait for background job {}",
            bounded(&args.job, 64)
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: WaitArgs = parse_args(&arguments)?;
        let limit = self.timeouts.resolve(args.timeout_seconds)?;
        let value = self
            .jobs
            .wait(&args.job, limit, &context.cancellation)
            .await?;
        Ok(ToolOutput::success(value.to_string()))
    }
}

/// `agent_status`: describe one background job or all of them.
pub(crate) struct StatusTool {
    pub(crate) jobs: Arc<BackgroundJobs>,
}

#[async_trait]
impl Tool for StatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent_status".into(),
            description: "Show this session's background agent jobs: running ones with their \
                latest progress, finished ones with their result. Pass job for one job."
                .into(),
            parameters: json!({
                "type":"object",
                "properties":{"job":{"type":"string"}},
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: StatusArgs = parse_args(arguments)?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: StatusArgs = parse_args(arguments)?;
        Ok(args.job.map_or_else(
            || "List background jobs".into(),
            |job| format!("Show background job {}", bounded(&job, 64)),
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: StatusArgs = parse_args(&arguments)?;
        let value = self.jobs.describe(args.job.as_deref())?;
        Ok(ToolOutput::success(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    /// An agent that reports one progress line, then finishes when released
    /// or stops when cancelled.
    struct FakeAgent {
        release: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for FakeAgent {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "agent_fake".into(),
                description: "Fake agent.".into(),
                parameters: json!({
                    "type":"object",
                    "properties":{"prompt":{"type":"string"}},
                    "required":["prompt"],
                    "additionalProperties":false
                }),
            }
        }

        fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
            if arguments.get("prompt").and_then(Value::as_str).is_none() {
                return Err(ToolError("prompt is required".into()));
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
                _ = self.release.notified() => Ok(ToolOutput::success(
                    json!({"agent":"fake","status":"completed","reply":"all done","session":"fake-1"})
                        .to_string(),
                )),
                _ = context.cancellation.cancelled() => {
                    self.cancelled.store(true, Ordering::SeqCst);
                    Err(ToolError("cancelled".into()))
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
            inner: Arc::new(FakeAgent {
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

    fn context() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), CancellationToken::new())
    }

    async fn start(tool: &BackgroundCapable) -> Value {
        let output = tool
            .execute(json!({"prompt":"work","background":true}), context())
            .await
            .unwrap();
        serde_json::from_str(&output.content).unwrap()
    }

    #[tokio::test]
    async fn background_calls_return_a_job_that_wait_and_status_observe() {
        let mut fixture = fixture(2);
        let started = start(&fixture.tool).await;
        assert_eq!(
            started,
            json!({
                "job":"job-1","tool":"agent_fake","status":"running","background":true,
                "note":started["note"]
            })
        );
        assert_eq!(
            scv_protocol::background_job_update(&started.to_string()).started,
            vec!["job-1".to_owned()]
        );
        // Running, with the job's latest progress.
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = fixture.jobs.describe(Some("job-1")).unwrap();
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
            )
            .await
            .unwrap();
        assert_eq!(waited["status"], "running");

        fixture.release.notify_one();
        let waited = fixture
            .jobs
            .wait("job-1", Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(waited["status"], "completed");
        assert_eq!(waited["result"]["reply"], "all done");
        assert_eq!(waited["result"]["session"], "fake-1");
        assert_eq!(
            scv_protocol::background_job_update(&waited.to_string()).settled,
            vec!["job-1".to_owned()]
        );
        // The session was woken, but the model already saw the result.
        fixture.finished.recv().await.unwrap();
        assert!(fixture.jobs.take_unreported().is_empty());
        let all = fixture.jobs.describe(None).unwrap();
        assert_eq!(all["jobs"][0]["job"], "job-1");
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
                report.tool.as_str(),
                report.status.as_str(),
                report.session.as_deref(),
                report.reply.as_str()
            ),
            (
                "job-1",
                "agent_fake",
                "completed",
                Some("fake-1"),
                "all done"
            )
        );
        let prompt = report_prompt(&reports);
        assert!(prompt.starts_with("[SCV background report]"), "{prompt}");
        assert!(
            prompt.contains("job-1 (agent_fake, conversation fake-1): completed\nall done"),
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
        assert!(refused.0.contains("agent.max_background"), "{refused}");
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
        assert_eq!(spec.name, "agent_fake");
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
        assert!(fixture.jobs.describe(Some("job-1")).is_err());
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
        assert!(unknown.0.contains("unknown background job"), "{unknown}");
        let status = StatusTool {
            jobs: Arc::clone(&fixture.jobs),
        };
        assert_eq!(status.risk(&json!({})).unwrap(), ToolRisk::ReadOnly);
        let empty = status.execute(json!({}), context()).await.unwrap();
        assert_eq!(empty.content, json!({"jobs":[]}).to_string());
    }
}
