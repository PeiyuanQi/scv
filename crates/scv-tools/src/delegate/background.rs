//! Background delegations: an `agent_*` call with `background: true` returns a
//! job handle at once while the agent keeps working; `agent_status` and
//! `agent_wait` observe the job, `agent_cancel` stops it, and the session is
//! told when it finishes so the server can report it in a turn of its own.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use scv_core::{
    ApprovalGate, ProgressSink, Tool, ToolApprovals, ToolContext, ToolError, ToolOutput, ToolRisk,
    ToolSpec,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    args::{Timeouts, bounded, parse_args, timeout_schema},
    sync::lock,
};

/// Finished jobs a session keeps for `agent_status` beyond the running ones.
const MAX_FINISHED: usize = 16;
/// Characters of a finished job's reply quoted in a server-started report turn.
const REPORT_REPLY_CHARS: usize = 6000;
/// Jobs one report turn covers; any more wait for the next.
const REPORT_MAX_JOBS: usize = 4;
/// How long `agent_cancel` waits for a stopped job to settle.
const CANCEL_SETTLE: Duration = Duration::from_secs(10);

/// One session's background jobs. Dropping the store (with the session's
/// tools) cancels every job still running.
pub struct BackgroundJobs {
    limit: usize,
    state: Mutex<JobsState>,
    cancellation: CancellationToken,
    /// Woken whenever a job finishes, so the session can report it.
    finished: Option<mpsc::UnboundedSender<()>>,
    /// Decides a running job's nested approval requests, since no turn is
    /// left to carry them to a person. Without it they are denied.
    approvals: Option<Arc<dyn ApprovalGate>>,
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
    /// Stops this job alone.
    cancel: CancellationToken,
    /// `agent_cancel` stopped it.
    cancelled: bool,
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
            approvals: None,
        }
    }

    /// Decide running jobs' nested approval requests with `gate`, which must
    /// never grant more than the session's foreground would.
    #[must_use]
    pub fn with_approvals(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approvals = Some(gate);
        self
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    fn state(&self) -> std::sync::MutexGuard<'_, JobsState> {
        lock(&self.state)
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
                     (agent.max_background). Start this one after a job finishes, or \
                     stop one with agent_cancel if the user no longer needs it."
                )));
            }
            state.next += 1;
            let id = format!("job-{}", state.next);
            let progress = ProgressSink::buffered();
            let (done_tx, done) = watch::channel(false);
            let cancel = self.cancellation.child_token();
            state.jobs.push(Job {
                id: id.clone(),
                tool: name.to_owned(),
                started: Instant::now(),
                progress: progress.clone(),
                last_progress: None,
                cancel: cancel.clone(),
                cancelled: false,
                outcome: None,
                reported: false,
                done,
            });
            (id, progress, done_tx, cancel)
        };
        let jobs = Arc::downgrade(self);
        let job = id.clone();
        let approvals = self.approvals.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let mut context = ToolContext::new(workspace, cancellation);
            // No turn is left to ask a person, so nested approval requests
            // get the session's unattended answer, or are denied.
            if let Some(gate) = &approvals {
                context.approvals = ToolApprovals::new(Arc::clone(gate), job.clone());
            }
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
            "note": "The agent is working in the background. SCV reports the result in a new \
                     turn when it finishes. agent_status shows its progress and agent_cancel \
                     stops it."
        }))
    }

    /// Stop `job` and wait briefly for it to settle, then describe it. A
    /// stopped job needs no report turn: the model asked for the stop.
    async fn cancel(&self, job: &str) -> Result<Value, ToolError> {
        let mut done = {
            let mut state = self.state();
            let entry = state
                .jobs
                .iter_mut()
                .find(|candidate| candidate.id == job)
                .ok_or_else(|| unknown_job(job))?;
            if entry.outcome.is_some() {
                let mut value = entry.describe();
                value["note"] = "The job had already finished.".into();
                return Ok(value);
            }
            entry.cancelled = true;
            entry.cancel.cancel();
            entry.done.clone()
        };
        let _ = tokio::time::timeout(CANCEL_SETTLE, done.wait_for(|finished| *finished)).await;
        self.describe(Some(job))
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
            () = cancellation.cancelled() => return Err(ToolError("wait cancelled".into())),
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
            // Stopped on request: the model already knows.
            job.reported |= job.cancelled;
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
                let status = if self.cancelled {
                    "cancelled".to_owned()
                } else {
                    job_status(&outcome.output, &result)
                };
                value.insert("status".into(), status.into());
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
    result.get("status").and_then(Value::as_str).map_or_else(
        || {
            if output.is_error {
                "failed".into()
            } else {
                "completed".into()
            }
        },
        str::to_owned,
    )
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
                    "description":"Run in the background: the call returns a job handle at once, \
                        the user can keep talking to you while the agent works, and SCV reports \
                        the result in a new turn when it finishes. Use it for any substantial \
                        task; run in the foreground only for quick work whose result you need \
                        within this turn."
                }),
            );
        }
        spec.description.push_str(&format!(
            " Set background to true for anything beyond a quick task: the call returns a job \
             handle at once (at most {} running per session), SCV reports the result when the \
             job finishes, agent_status shows progress, and agent_cancel stops it. A background \
             job's own approval requests get only the answer this session would give without \
             asking a person.",
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
                status running when the timeout passes. Waiting holds your turn open, so the \
                user cannot reach you meanwhile; usually let SCV report the result instead."
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    job: String,
}

/// `agent_cancel`: stop one running background job.
pub(crate) struct CancelTool {
    pub(crate) jobs: Arc<BackgroundJobs>,
}

#[async_trait]
impl Tool for CancelTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent_cancel".into(),
            description: "Stop a running background agent job (the `job` handle an agent_* call \
                with background: true returned), for example when the user no longer wants \
                it. The agent and every process it started are stopped; work it already wrote \
                stays. Returns the job with status cancelled, and no report turn follows."
                .into(),
            parameters: json!({
                "type":"object",
                "properties":{"job":{"type":"string"}},
                "required":["job"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: CancelArgs = parse_args(arguments)?;
        Ok(ToolRisk::Process)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: CancelArgs = parse_args(arguments)?;
        Ok(format!("Stop background job {}", bounded(&args.job, 64)))
    }

    async fn execute(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: CancelArgs = parse_args(&arguments)?;
        let value = self.jobs.cancel(&args.job).await?;
        Ok(ToolOutput::success(value.to_string()))
    }
}

#[cfg(test)]
mod tests;
