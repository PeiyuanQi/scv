//! Background delegations: an `agent` call with `background: true` returns a
//! job handle at once while the agent keeps working; `agent_status` and
//! `agent_wait` observe the job, `agent_cancel` stops it, and the session is
//! told when it finishes so the server can report it in a turn of its own.
//!
//! Each call that starts a job, or shows the model a job's result, leaves a
//! [`JobChange`] under its call ID, which the server hands to the session's
//! clients with the call's `tool.completed` ([`BackgroundJobs::take_changes`]).
//!
//! A finished job stays unreported until a report turn about it succeeds. A
//! failed one makes it due again after a delay, a bounded number of times;
//! after that, or when another turn must not be tried, the server reports it
//! to the client directly ([`BackgroundJobs::report_failed`]).
//!
//! A call that continues a conversation takes its place in that
//! conversation's lane as it arrives. When a turn or another call is
//! ahead, the call's busy policy decides: it steers the running turn, waits
//! in the foreground, fails, or becomes a queued job that runs once its
//! place comes up.
//!
//! A call with `review` runs as a reviewed job ([`review`](super::review)):
//! the same job, whose one call runs the whole builder and reviewer loop and
//! whose changes and report carry SCV's outcome.

pub(in crate::delegate) mod lane;
mod owed;

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use scv_core::{
    ApprovalGate, ProgressSink, Tool, ToolApprovals, ToolContext, ToolError, ToolOutput, ToolRisk,
    ToolSpec,
};
use scv_protocol::{JobChange, JobOutcome, JobReport, JobStatus, describe_reports};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use self::lane::{Lanes, Place};
use crate::{
    BusyBehavior,
    args::{Timeouts, bounded, parse_args, timeout_schema},
    delegate::{
        agent::{AGENT_TOOL, AgentTool, Offered},
        conversation::{self, ConversationStore},
        output::AgentReply,
        request::AgentArgs,
        review::{self, Record, ReviewedTool},
    },
    sync::lock,
};

/// Finished jobs a session keeps for `agent_status` beyond the running ones.
const MAX_FINISHED: usize = 16;
/// Characters of a finished job's reply quoted in a server-started report turn.
const REPORT_REPLY_CHARS: usize = 6000;
/// Jobs one report turn covers; any more wait for the next.
const REPORT_MAX_JOBS: usize = 4;
/// How long after each failed report turn the next one is due: after the
/// first failure, then after the second. Once they run out, the job is
/// reported directly.
const REPORT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(30), Duration::from_secs(120)];
/// How long `agent_cancel` waits for a stopped job to settle.
const CANCEL_SETTLE: Duration = Duration::from_secs(10);
/// Job changes kept for calls whose `tool.completed` has not taken them,
/// such as a call whose turn was aborted; the oldest go first.
const MAX_PENDING_CHANGES: usize = 64;
/// Characters of a delegated prompt's first line kept as its job's task.
const TASK_CHARS: usize = 80;

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
    /// The `agent` calls continuing each conversation, in arrival order.
    lanes: Arc<Lanes>,
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
    /// Changes by the ID of the call that made them, oldest first.
    changes: Vec<(String, JobChange)>,
    /// The final outcomes of cancelled reviewed jobs the session is owed.
    owed: owed::Owed,
}

impl JobsState {
    /// Bring the outcome in the change the call `call_id` made for
    /// `outcome`'s job up to date, if no client has taken it yet.
    fn refresh(&mut self, call_id: &str, outcome: JobOutcome) {
        if let Some((_, change)) = self
            .changes
            .iter_mut()
            .rev()
            .find(|(call, change)| call == call_id && change.job == outcome.job)
        {
            change.outcome = Some(outcome);
        }
    }

    /// Keep `change` for the call `call_id`, which a client learns of with
    /// that call's `tool.completed`. A call without an ID has no event.
    fn record(&mut self, call_id: &str, change: JobChange) {
        if call_id.is_empty() {
            return;
        }
        if self.changes.len() >= MAX_PENDING_CHANGES {
            let (call, dropped) = self.changes.remove(0);
            self.owed.evicted(&call, &dropped.job);
        }
        self.changes.push((call_id.to_owned(), change));
    }
}

struct Job {
    id: String,
    /// The tool that started it, `agent`.
    tool: String,
    /// The agent that runs it, such as `codex`.
    agent: String,
    /// The first line of the delegated prompt, shortened.
    task: String,
    started: Instant,
    progress: ProgressSink,
    last_progress: Option<String>,
    /// Stops this job alone.
    cancel: CancellationToken,
    /// `agent_cancel` stopped it.
    cancelled: bool,
    /// A busy conversation's queued prompt, which `agent.max_queued_turns`
    /// bounds instead of `agent.max_background`.
    queued: bool,
    outcome: Option<Outcome>,
    /// The model has seen the result (through `agent_wait`, `agent_status`,
    /// or a report turn that completed), the user stopped its report turn,
    /// the server reported it directly, or the model asked for the stop with
    /// `agent_cancel`, so it needs no report turn.
    reported: bool,
    /// A report turn about it runs.
    reporting: bool,
    /// Report turns about it that failed.
    report_attempts: u32,
    /// After a failed report turn: when the next one is due.
    report_due: Option<tokio::time::Instant>,
    done: watch::Receiver<bool>,
    /// A reviewed job's record, whose outcome its changes and report state.
    review: Option<Arc<Record>>,
    /// `agent_cancel` waits for it, and describes it after: never pruned
    /// meanwhile.
    cancelling: bool,
}

/// `agent_cancel` waiting for a job to stop. Settled after the wait, or,
/// if the call is dropped first, as when its turn is aborted, its change
/// withdrawn and any outcome it owed left to an update.
struct Waiting<'a> {
    jobs: &'a BackgroundJobs,
    job: String,
    call: String,
    /// The job is reviewed: the session is owed its final outcome.
    owed: bool,
    settled: bool,
}

impl Waiting<'_> {
    /// The wait is over: the change the call made says the final outcome
    /// if the job stopped, else the outcome as decided, its journal still
    /// open; [`BackgroundJobs::take_changes`] says the final one if the job
    /// stops before the change is sent.
    fn settle(mut self) {
        self.settled = true;
        let mut state = self.jobs.state();
        let Some(entry) = state.jobs.iter_mut().find(|entry| entry.id == self.job) else {
            return;
        };
        entry.cancelling = false;
        if !self.owed {
            return;
        }
        let pending = entry.outcome.is_none().then(|| entry.outcome()).flatten();
        let pending = pending.map(|mut outcome| {
            outcome.review.journal_pending = true;
            outcome
        });
        if let Some(outcome) = state.owed.waited(&self.job, &self.call, pending) {
            state.refresh(&self.call, outcome);
        }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut state = self.jobs.state();
        if let Some(entry) = state.jobs.iter_mut().find(|entry| entry.id == self.job) {
            entry.cancelling = false;
        }
        if !self.owed {
            return;
        }
        // Its call is gone, so its change is never sent.
        let (call, job) = (&self.call, &self.job);
        state
            .changes
            .retain(|(recorded, change)| !(recorded == call && &change.job == job));
        let due = state.owed.abandoned(job, call);
        drop(state);
        if due {
            self.jobs.wake();
        }
    }
}

struct Outcome {
    output: ToolOutput,
    elapsed: Duration,
}

/// What a failed report turn leaves its jobs to.
#[derive(Debug, PartialEq, Eq)]
pub enum ReportFailure {
    /// Another report turn about them is due after this long.
    Retry(Duration),
    /// No more report turns: the server reports them directly. `attempts`
    /// counts the report turns that failed.
    GiveUp {
        attempts: u32,
        reports: Vec<JobReport>,
    },
    /// The model saw each of them through a call during the turn.
    Settled,
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
            lanes: Arc::default(),
        }
    }

    /// Decide running jobs' nested approval requests with `gate`, which must
    /// never grant more than the session's foreground would.
    #[must_use]
    pub fn with_approvals(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approvals = Some(gate);
        self
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    fn state(&self) -> std::sync::MutexGuard<'_, JobsState> {
        lock(&self.state)
    }

    /// Start `tool` with `arguments` in the background for the call in
    /// `context` (whose workspace and call ID it takes; the job has its own
    /// cancellation), on `agent`, and return its job's
    /// `{"job","agent","status":"running","background":true}` description.
    /// A reviewed job's `review` records its start in its journal first, and
    /// the call fails if it cannot. That write happens outside the job
    /// table's lock, so a slow disk never holds up the session's other job
    /// calls: the job's number is taken first, and a failed write leaves it
    /// unused.
    #[allow(
        clippy::too_many_arguments,
        reason = "a job's call, its context, its turn, and its review, each its own"
    )]
    pub(in crate::delegate) fn start(
        self: &Arc<Self>,
        tool: Arc<dyn Tool>,
        name: &str,
        agent: &str,
        arguments: Value,
        context: &ToolContext,
        turn: Turn,
        review: Option<&Arc<ReviewedTool>>,
    ) -> Result<Value, ToolError> {
        let task = task_line(
            arguments
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let queued = matches!(turn, Turn::Queued(_));
        let reserved = match review {
            Some(review) => {
                let id = {
                    let mut state = self.state();
                    self.room(&state, queued)?;
                    state.next += 1;
                    format!("job-{}", state.next)
                };
                review.begin(&id)?;
                Some(id)
            }
            None => None,
        };
        let (id, progress, done_tx, cancellation) = {
            let mut state = self.state();
            // Checked again: another call may have started a job meanwhile.
            self.room(&state, queued)?;
            let id = reserved.unwrap_or_else(|| {
                state.next += 1;
                format!("job-{}", state.next)
            });
            let progress = ProgressSink::buffered();
            let (done_tx, done) = watch::channel(false);
            let cancel = self.cancellation.child_token();
            state.record(
                &context.call_id,
                JobChange {
                    job: id.clone(),
                    tool: name.to_owned(),
                    agent: agent.to_owned(),
                    status: JobStatus::Running,
                    task: task.clone(),
                    journal: review.map(|review| review.journal_id()),
                    outcome: None,
                },
            );
            state.jobs.push(Job {
                id: id.clone(),
                tool: name.to_owned(),
                agent: agent.to_owned(),
                task,
                started: Instant::now(),
                progress: progress.clone(),
                last_progress: None,
                cancel: cancel.clone(),
                cancelled: false,
                queued,
                outcome: None,
                reported: false,
                reporting: false,
                report_attempts: 0,
                report_due: None,
                done,
                review: review.map(|review| Arc::clone(review.record())),
                cancelling: false,
            });
            (id, progress, done_tx, cancel)
        };
        let jobs = Arc::downgrade(self);
        let job = id.clone();
        let approvals = self.approvals.clone();
        let workspace = context.workspace.clone();
        let reviewed = review.map(Arc::clone);
        tokio::spawn(async move {
            let started = Instant::now();
            let mut context = ToolContext::new(workspace, cancellation);
            // No turn is left to ask a person, so nested approval requests
            // get the session's unattended answer, or are denied.
            if let Some(gate) = &approvals {
                context.approvals = ToolApprovals::new(Arc::clone(gate), job.clone());
            }
            context.progress = progress;
            let place = turn.place();
            let output = match &place {
                Some(place) if queued => {
                    context.progress.report(&format!(
                        "queued: waits for conversation {}'s running turn",
                        place.handle()
                    ));
                    match place.wait_first(&context.cancellation).await {
                        Ok(()) => {
                            context.progress.take();
                            context.progress.report(&format!(
                                "started its turn in conversation {}",
                                place.handle()
                            ));
                            tool.execute(arguments, context).await
                        }
                        // A reviewed job ends its review even when its loop
                        // never ran, so its result says how it ended.
                        Err(error) => match &reviewed {
                            Some(review) => Ok(review.unstarted(&error)),
                            None => Err(error),
                        },
                    }
                }
                _ => tool.execute(arguments, context).await,
            }
            .unwrap_or_else(ToolOutput::from);
            // Free the conversation before the result is visible, so a call
            // continuing it after `agent_wait` finds it idle.
            drop(place);
            finish(&jobs, &job, output, started.elapsed());
            let _ = done_tx.send(true);
        });
        let mut started = json!({
            "job": id,
            "agent": agent,
            "status": "running",
            "background": true,
            "note": "The agent is working in the background. SCV reports the result in a new \
                     turn when it finishes. agent_status shows its progress and agent_cancel \
                     stops it."
        });
        if queued {
            started["queued"] = true.into();
            started["note"] = "The conversation is busy, so this prompt is queued: it runs in \
                the background after the turns ahead of it, and SCV reports the result in a new \
                turn when it finishes. agent_status shows where it stands and agent_cancel \
                withdraws it."
                .into();
        }
        Ok(started)
    }

    /// Refuse a job beyond `agent.max_background`; a queued prompt counts
    /// toward `agent.max_queued_turns` instead.
    fn room(&self, state: &JobsState, queued: bool) -> Result<(), ToolError> {
        let running = state
            .jobs
            .iter()
            .filter(|job| job.outcome.is_none() && !job.queued)
            .count();
        if !queued && running >= self.limit {
            return Err(ToolError::limit(format!(
                "{running} background jobs are already running, the limit \
                 (agent.max_background). Start this one after a job finishes, or \
                 stop one with agent_cancel if the user no longer needs it."
            )));
        }
        Ok(())
    }

    /// Stop `job` for the call `call_id` and wait briefly for it to settle,
    /// then describe it. A stopped job needs no report turn: the model asked
    /// for the stop.
    ///
    /// A reviewed job's decision is fixed at once, but its journal goes on
    /// until it really stops, so the session is owed its final outcome
    /// ([`owed`]): the call's change says it if the job stops before the
    /// change is sent, and otherwise says the journal is still open, and a
    /// `background.updated` follows ([`Self::take_updates`]). The wait
    /// reads only the job's live state, never its journal.
    async fn cancel(&self, job: &str, call_id: &str) -> Result<Value, ToolError> {
        let (mut done, owed) = {
            let mut state = self.state();
            let index = state
                .jobs
                .iter()
                .position(|candidate| candidate.id == job)
                .ok_or_else(|| unknown_job(job))?;
            let entry = &mut state.jobs[index];
            if entry.outcome.is_some() {
                let (mut value, change) = entry.describe();
                value["note"] = "The job had already finished.".into();
                if let Some(change) = change {
                    state.record(call_id, change);
                }
                return Ok(value);
            }
            entry.cancelled = true;
            entry.cancel.cancel();
            entry.cancelling = true;
            let done = entry.done.clone();
            let mut owed = false;
            if !entry.reported {
                entry.reported = true;
                let change = entry.change(JobStatus::Cancelled);
                owed = entry.review.is_some();
                state.record(call_id, change);
                if owed {
                    state.owed.cancelling(job, call_id);
                }
            }
            (done, owed)
        };
        let waiting = Waiting {
            jobs: self,
            job: job.to_owned(),
            call: call_id.to_owned(),
            owed,
            settled: false,
        };
        let _ = tokio::time::timeout(CANCEL_SETTLE, done.wait_for(|finished| *finished)).await;
        waiting.settle();
        self.describe(Some(job), call_id)
    }

    /// The final outcomes of cancelled reviewed jobs the session is owed
    /// and has not been sent, for the server's `background.updated`. Each
    /// is being sent until [`Self::updated`] says how that went.
    pub fn take_updates(&self) -> Vec<JobOutcome> {
        self.state().owed.take_updates()
    }

    /// The update naming `jobs` reached the client, when `delivered`, which
    /// ends what was owed; or it could not be sent, which leaves them due.
    pub fn updated(&self, jobs: &[String], delivered: bool) {
        self.state().owed.updated(jobs, delivered);
    }

    /// The `tool.completed` of the call `call_id`, carrying the changes
    /// [`Self::take_changes`] gave, reached the client, when `delivered`, or
    /// could not be sent, as when its turn was cancelled first. A change
    /// that did not deliver a reviewed job's final outcome leaves it to a
    /// `background.updated`.
    pub fn published(&self, call_id: &str, delivered: bool) {
        let due = self.state().owed.sent(call_id, delivered);
        if due {
            self.wake();
        }
    }

    /// A turn ended: no change of its calls can be sent any more, so what
    /// such a change owed goes to a `background.updated`.
    pub fn turn_ended(&self) {
        let due = self.state().owed.turn_ended();
        if due {
            self.wake();
        }
    }

    /// For tests of how a client of this store delivers reviewed jobs'
    /// outcomes, such as the server's, with the `test-support` feature: owe
    /// the session the final outcome of `change.job`, through `change`,
    /// recorded for the call `call_id` once `agent_cancel` waited, which says
    /// the journal is still open; `last` is the final outcome if the job has
    /// stopped.
    #[cfg(any(test, feature = "test-support"))]
    pub fn owe_for_tests(&self, call_id: &str, change: JobChange, last: Option<JobOutcome>) {
        let mut state = self.state();
        state.owed.owe(call_id, &change, last);
        state.record(call_id, change);
    }

    /// For the same tests: the job of `outcome` stops with it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn stop_for_tests(&self, outcome: &JobOutcome) {
        let due = self.state().owed.stopped(&outcome.job, outcome);
        if due {
            self.wake();
        }
    }

    /// Wake the session, as when a job finishes, for what is now due.
    fn wake(&self) {
        if let Some(finished) = &self.finished {
            let _ = finished.send(());
        }
    }

    /// Wait up to `limit` for `job` and describe it for the call `call_id`;
    /// a finished job is then marked seen.
    async fn wait(
        &self,
        job: &str,
        limit: Duration,
        cancellation: &CancellationToken,
        call_id: &str,
    ) -> Result<Value, ToolError> {
        let mut done = self
            .state()
            .jobs
            .iter()
            .find(|candidate| candidate.id == job)
            .map(|candidate| candidate.done.clone())
            .ok_or_else(|| unknown_job(job))?;
        tokio::select! {
            () = cancellation.cancelled() => return Err(ToolError::cancelled("wait cancelled")),
            _ = tokio::time::timeout(limit, done.wait_for(|finished| *finished)) => {}
        }
        self.describe(Some(job), call_id)
    }

    /// Describe one job, or every job this session remembers, for the call
    /// `call_id`; finished jobs described are marked seen.
    fn describe(&self, job: Option<&str>, call_id: &str) -> Result<Value, ToolError> {
        let mut state = self.state();
        let mut changes = Vec::new();
        let value = if let Some(job) = job {
            let entry = state
                .jobs
                .iter_mut()
                .find(|candidate| candidate.id == job)
                .ok_or_else(|| unknown_job(job))?;
            let (value, change) = entry.describe();
            changes.extend(change);
            value
        } else {
            let jobs: Vec<Value> = state
                .jobs
                .iter_mut()
                .map(|entry| {
                    let (value, change) = entry.describe();
                    changes.extend(change);
                    value
                })
                .collect();
            json!({ "jobs": jobs })
        };
        for change in changes {
            state.record(call_id, change);
        }
        Ok(value)
    }

    /// The jobs the call `call_id` started or showed the model the result of,
    /// for its `tool.completed`, which then reports how its sending went
    /// ([`Self::published`]). A cancelled reviewed job's change says its
    /// final outcome if the job has stopped by now, superseding the
    /// snapshot taken when the cancel's wait ended.
    pub fn take_changes(&self, call_id: &str) -> Vec<JobChange> {
        let mut state = self.state();
        let mut taken = Vec::new();
        state.changes.retain(|(call, change)| {
            if call == call_id {
                taken.push(change.clone());
                false
            } else {
                true
            }
        });
        for change in &mut taken {
            state.owed.take(call_id, change);
        }
        taken
    }

    /// Finished jobs the model has not seen yet whose report is due, at most
    /// a few, for a report turn. Each stays unreported, and is not taken
    /// again, until the turn ends: [`report_settled`](Self::report_settled)
    /// or [`report_failed`](Self::report_failed).
    pub fn take_unreported(&self) -> Vec<JobReport> {
        let now = tokio::time::Instant::now();
        let mut state = self.state();
        state
            .jobs
            .iter_mut()
            .filter(|job| job.awaits_report() && job.report_due.is_none_or(|due| due <= now))
            .take(REPORT_MAX_JOBS)
            .filter_map(|job| {
                job.reporting = true;
                job.report()
            })
            .collect()
    }

    /// The report turn about `jobs` ended without failing: it completed, so
    /// the model has seen them, or the user cancelled it. Either settles them.
    pub fn report_settled(&self, jobs: &[String]) {
        let mut state = self.state();
        for job in state.jobs.iter_mut().filter(|job| jobs.contains(&job.id)) {
            job.reporting = false;
            job.reported = true;
            job.report_due = None;
        }
    }

    /// The report turn about `jobs` failed, so the model has not seen them
    /// (a failed turn leaves no history). With `retry`, another turn is due
    /// after a delay, up to three turns in all; otherwise, or once they are
    /// used up, the jobs are settled and returned for the server to report
    /// directly. A turn that reported several jobs decides for all of them,
    /// by the one that failed most.
    pub fn report_failed(&self, jobs: &[String], retry: bool) -> ReportFailure {
        let mut state = self.state();
        let mut failed: Vec<&mut Job> = state
            .jobs
            .iter_mut()
            .filter(|job| job.reporting && jobs.contains(&job.id))
            .collect();
        for job in &mut failed {
            job.reporting = false;
            job.report_attempts += 1;
        }
        // One the turn's model looked up through a call is settled already.
        failed.retain(|job| !job.reported);
        let Some(attempts) = failed.iter().map(|job| job.report_attempts).max() else {
            return ReportFailure::Settled;
        };
        let delay = usize::try_from(attempts - 1)
            .ok()
            .and_then(|earlier| REPORT_RETRY_DELAYS.get(earlier));
        if retry && let Some(&delay) = delay {
            let due = tokio::time::Instant::now() + delay;
            for job in failed {
                job.report_due = Some(due);
            }
            return ReportFailure::Retry(delay);
        }
        let reports = failed
            .into_iter()
            .filter_map(|job| {
                job.reported = true;
                job.report_due = None;
                job.report()
            })
            .collect();
        ReportFailure::GiveUp { attempts, reports }
    }

    /// Make every report waiting out a failed turn due now, as once a turn
    /// succeeds and the model is evidently reachable again. Whether any was
    /// waiting.
    pub fn retry_now(&self) -> bool {
        let mut state = self.state();
        let mut waiting = false;
        for job in state.jobs.iter_mut().filter(|job| job.awaits_report()) {
            waiting |= job.report_due.take().is_some();
        }
        waiting
    }

    /// When the earliest report waiting out a failed turn is due; it may
    /// already have passed.
    pub fn next_retry(&self) -> Option<tokio::time::Instant> {
        self.state()
            .jobs
            .iter()
            .filter(|job| job.awaits_report())
            .filter_map(|job| job.report_due)
            .min()
    }

    /// Jobs still running or whose result is not reported yet, including
    /// those a report turn is reporting now, and cancelled reviewed jobs
    /// whose final outcome the session is still owed, until it was sent;
    /// each job once.
    pub fn pending(&self) -> usize {
        let state = self.state();
        let mut pending: HashSet<&str> = state
            .jobs
            .iter()
            .filter(|job| job.outcome.is_none() || !job.reported)
            .map(|job| job.id.as_str())
            .collect();
        pending.extend(state.owed.jobs());
        pending.len()
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

/// How a job's call takes its conversation's turn.
pub(in crate::delegate) enum Turn {
    /// A new conversation, which nothing else can reach yet.
    New,
    /// The conversation's next turn, which nothing was ahead of.
    Next(Place),
    /// A prompt queued behind the turns ahead of it.
    Queued(Place),
}

impl Turn {
    fn place(self) -> Option<Place> {
        match self {
            Self::New => None,
            Self::Next(place) | Self::Queued(place) => Some(place),
        }
    }
}

fn finish(jobs: &Weak<BackgroundJobs>, id: &str, output: ToolOutput, elapsed: Duration) {
    // The session ended first: its jobs were cancelled with it.
    let Some(jobs) = jobs.upgrade() else {
        return;
    };
    {
        let mut state = jobs.state();
        let mut last = None;
        if let Some(job) = state.jobs.iter_mut().find(|job| job.id == id) {
            job.last_progress = job.progress.take().or(job.last_progress.take());
            job.outcome = Some(Outcome { output, elapsed });
            // Stopped on request: the model already knows.
            job.reported |= job.cancelled;
            last = job.outcome();
        }
        // What a cancel owed of it is now final.
        if let Some(last) = last {
            state.owed.stopped(id, &last);
        }
        // Keep every running job and the newest finished ones.
        let finished = state
            .jobs
            .iter()
            .filter(|job| job.outcome.is_some())
            .count();
        let mut excess = finished.saturating_sub(MAX_FINISHED);
        state.jobs.retain(|job| {
            if excess > 0 && job.outcome.is_some() && job.reported && !job.cancelling {
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
    /// Finished, unseen by the model, and not in a report turn.
    fn awaits_report(&self) -> bool {
        self.outcome.is_some() && !self.reported && !self.reporting
    }

    /// The finished job's result, for a report.
    fn report(&self) -> Option<JobReport> {
        let outcome = self.outcome.as_ref()?;
        let reply = AgentReply::read(&result_value(&outcome.output)).unwrap_or_default();
        Some(JobReport {
            job: self.id.clone(),
            agent: self.agent.clone(),
            task: self.task.clone(),
            status: job_status(&outcome.output, &reply),
            session: reply.session,
            reply: bounded(
                reply
                    .reply
                    .as_deref()
                    .unwrap_or(outcome.output.content.as_str()),
                REPORT_REPLY_CHARS,
            ),
            outcome: self.outcome(),
        })
    }

    /// A reviewed job's outcome as it stands. A cancelled job's is what it
    /// was when the cancel arrived.
    fn outcome(&self) -> Option<JobOutcome> {
        self.review.as_ref().map(|record| {
            let (review, landing) = record
                .live
                .snapshot(self.cancelled || self.cancel.is_cancelled());
            JobOutcome {
                job: self.id.clone(),
                review,
                landing,
            }
        })
    }

    /// This job with `status`, as its clients learn of it: a reviewed job's
    /// settling change carries its outcome.
    fn change(&self, status: JobStatus) -> JobChange {
        JobChange {
            job: self.id.clone(),
            tool: self.tool.clone(),
            agent: self.agent.clone(),
            status,
            task: self.task.clone(),
            journal: None,
            outcome: if status == JobStatus::Running {
                None
            } else {
                self.outcome()
            },
        }
    }

    /// The job as the model reads it, and its change when this is the first
    /// time the model sees its result.
    fn describe(&mut self) -> (Value, Option<JobChange>) {
        if let Some(line) = self.progress.take() {
            self.last_progress = Some(line);
        }
        let mut change = None;
        let mut value = Map::new();
        value.insert("job".into(), self.id.clone().into());
        value.insert("agent".into(), self.agent.clone().into());
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
                if let Some(record) = &self.review {
                    value.insert("review".into(), record.live.status());
                }
            }
            Some(outcome) => {
                let result = result_value(&outcome.output);
                let status = if self.cancelled {
                    JobStatus::Cancelled
                } else {
                    job_status(
                        &outcome.output,
                        &AgentReply::read(&result).unwrap_or_default(),
                    )
                };
                value.insert("status".into(), status.as_str().into());
                value.insert("elapsed_seconds".into(), outcome.elapsed.as_secs().into());
                value.insert("result".into(), result);
                if !self.reported {
                    self.reported = true;
                    change = Some(self.change(status));
                }
            }
        }
        (Value::Object(value), change)
    }
}

/// The agent tool's structured result, or its text when it was not JSON.
fn result_value(output: &ToolOutput) -> Value {
    match serde_json::from_str::<Value>(&output.content) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ "reply": output.content, "is_error": output.is_error() }),
    }
}

/// How a finished job ended: its agent's reported status, else whether its
/// call failed.
fn job_status(output: &ToolOutput, reply: &AgentReply) -> JobStatus {
    reply.status.unwrap_or(if output.is_error() {
        JobStatus::Failed
    } else {
        JobStatus::Completed
    })
}

/// The first non-empty line of `prompt`, at most [`TASK_CHARS`] characters,
/// which names a job to people.
fn task_line(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut task: String = line
        .chars()
        .filter(|character| !character.is_control())
        .take(TASK_CHARS)
        .collect();
    if line.chars().count() > TASK_CHARS {
        task.push('…');
    }
    task
}

fn unknown_job(job: &str) -> ToolError {
    ToolError::invalid_arguments(format!(
        "unknown background job {:?}; agent_status lists this session's jobs",
        bounded(job, 64)
    ))
}

/// What the model is told when a report includes reviewed jobs, whose
/// outcome SCV states itself.
const REVIEWED_NOTE: &str = " SCV shows the user its own Review and Landing lines for \
     reviewed jobs. If you mention the outcome, repeat them as given; never call unapproved \
     work approved or done, and never call landed work approved. If a reviewer declined, tell \
     the user what it said. If a landing is NOT confirmed, or the reviewer could NOT confirm \
     it, say so plainly, and don't fix, revert, or land anything on your own.";

fn reviewed_note(reports: &[JobReport]) -> &'static str {
    if reports.iter().any(|report| report.outcome.is_some()) {
        REVIEWED_NOTE
    } else {
        ""
    }
}

/// The server-started turn that reports finished jobs to the model.
pub fn report_prompt(reports: &[JobReport]) -> String {
    format!(
        "[SCV background report] Delegated work you started in the background has \
         finished. The user did not send this message: tell them briefly what \
         happened and the key result.{}\n\n{}",
        reviewed_note(reports),
        describe_reports(reports)
    )
}

/// What the model is told of jobs the server reported directly, because
/// their report turns failed with `error`: the user has their results.
pub fn delivered_note(reports: &[JobReport], error: &str) -> String {
    format!(
        "[SCV background report, already delivered] Delegated work you started in \
         the background has finished. Your report of it failed ({error}), so SCV \
         sent the user the results below directly. The user did not send this \
         message; repeat the results only if they ask.{}\n\n{}",
        reviewed_note(reports),
        describe_reports(reports)
    )
}

/// The `agent` tool, able to run a call in the background as well, and to
/// run one as a reviewed job.
pub(crate) struct BackgroundCapable {
    pub(crate) inner: Arc<AgentTool>,
    pub(crate) jobs: Arc<BackgroundJobs>,
    /// The session's conversations, which a reviewed job pins and releases.
    pub(crate) conversations: Arc<ConversationStore>,
    /// Where reviewed jobs keep their journals; without it, `review` is not
    /// offered.
    pub(crate) reviews: Option<PathBuf>,
    /// The SCV session, which a review's journal names.
    pub(crate) session: Option<String>,
}

/// The `agent` tool for a queued prompt, whose turn has come by the time it
/// runs, so no busy policy applies to it again.
struct QueuedTool(Arc<AgentTool>);

#[async_trait]
impl Tool for QueuedTool {
    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        self.0.risk(arguments)
    }
    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        self.0.approval_summary(arguments)
    }
    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.0.execute_when_idle(arguments, context).await
    }
}

/// An agent call's arguments with `background` and `review` split off.
struct Split {
    arguments: Value,
    /// `background` as the call gave it.
    background: Option<bool>,
    /// The `review` object, when the call gave one.
    review: Option<Value>,
}

impl Split {
    fn new(arguments: &Value) -> Self {
        let mut arguments = arguments.clone();
        let object = arguments.as_object_mut();
        let (background, review) = match object {
            Some(object) => (
                object
                    .remove("background")
                    .map(|value| value.as_bool() == Some(true)),
                object.remove("review").filter(|review| !review.is_null()),
            ),
            None => (None, None),
        };
        Self {
            arguments,
            background,
            review,
        }
    }

    fn background(&self) -> bool {
        self.background == Some(true)
    }
}

#[async_trait]
impl Tool for BackgroundCapable {
    fn spec(&self) -> ToolSpec {
        let mut spec = self.inner.spec();
        let reviewed = self.offers_review();
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
            if reviewed {
                properties.insert("review".into(), review::schema(&self.inner));
            }
        }
        spec.description.push_str(&format!(
            " Set background to true for anything beyond a quick task: the call returns a job \
             handle at once (at most {} running per session), SCV reports the result when the \
             job finishes, agent_status shows progress, and agent_cancel stops it. A background \
             job's own approval requests get only the answer this session would give without \
             asking a person.",
            self.jobs.limit()
        ));
        if reviewed {
            spec.description.push_str(
                " A call with review runs as a reviewed background job: an independent \
                 reviewer checks the work in up to review.rounds rounds.",
            );
        }
        spec
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let split = Split::new(arguments);
        if let Some(review) = &split.review {
            self.plan(&split, review)?;
        }
        self.inner.risk(&split.arguments)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let split = Split::new(arguments);
        let mut summary = self.inner.approval_summary(&split.arguments)?;
        if let Some(review) = &split.review {
            let plan = self.plan(&split, review)?;
            summary.push_str(
                " Runs in the background: the call returns at once and the result is \
                 reported when the review ends.",
            );
            summary.push_str(&review::approval_summary(&plan, &self.inner)?);
        } else if split.background() {
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
        let split = Split::new(&arguments);
        if let Some(review) = &split.review {
            return self.start_reviewed(&split, review, &context);
        }
        let background = split.background();
        let arguments = split.arguments;
        let (agent, routed) = self.inner.route(&arguments)?;
        // Validate before returning a job handle, so a bad call fails now.
        if background {
            agent.backend.risk(&routed)?;
        }
        let Some(handle) = parse_args::<AgentArgs>(&routed)?
            .session
            .filter(|session| conversation::is_handle(session))
        else {
            return if background {
                self.start(agent, arguments, &context, Turn::New)
            } else {
                self.inner.execute(arguments, context).await
            };
        };
        // Taken before anything runs or waits, so calls keep their order.
        let place = self.jobs.lanes.join(&handle);
        if place.ahead == 0 && !agent.backend.busy(&routed)? {
            return if background {
                self.start(agent, arguments, &context, Turn::Next(place))
            } else {
                // Held until the turn ends, so a later call finds it busy.
                let output = self.inner.execute(arguments, context).await;
                drop(place);
                output
            };
        }
        let behavior = match agent.on_busy(&routed)? {
            BusyBehavior::Steer => {
                if let Some(output) = agent.backend.steer(routed, context.clone()).await? {
                    return Ok(output);
                }
                agent.busy.fallback()
            }
            behavior => behavior,
        };
        match behavior {
            // A background call cannot hold its caller, so it queues instead.
            BusyBehavior::Wait if !background => {
                place.wait_first(&context.cancellation).await?;
                let output = self.inner.execute_when_idle(arguments, context).await;
                drop(place);
                output
            }
            BusyBehavior::Fail => Err(busy(&handle, &place)),
            _ => {
                queue_room(agent, &handle, &place)?;
                self.start(agent, arguments, &context, Turn::Queued(place))
            }
        }
    }
}

/// The `session busy` error for a call to conversation `handle` that found
/// a turn running, or calls waiting, at `place`.
fn busy(handle: &str, place: &Place) -> ToolError {
    // Calls ahead of this one other than the running turn.
    let waiting = place.ahead.saturating_sub(1);
    ToolError::failed(if waiting == 0 {
        format!("session busy: conversation {handle} is still running a turn")
    } else {
        format!(
            "session busy: conversation {handle} is still running a turn, and {waiting} more \
             prompts wait for it"
        )
    })
}

/// Refuse a prompt queued at `place` beyond `agent.max_queued_turns`.
fn queue_room(agent: &Offered, handle: &str, place: &Place) -> Result<(), ToolError> {
    let waiting = place.ahead.saturating_sub(1);
    if waiting >= agent.busy.max_queued_turns {
        return Err(ToolError::limit(format!(
            "session busy: conversation {handle} is running a turn and {waiting} prompts \
             already wait for it, of the {} agent.max_queued_turns allows. Send this one \
             once they have run, or stop one with agent_cancel.",
            agent.busy.max_queued_turns
        )));
    }
    Ok(())
}

impl BackgroundCapable {
    /// `inner` with `jobs` and a store of its own, without reviews: for
    /// tests of plain background calls.
    #[cfg(test)]
    pub(crate) fn plain(inner: Arc<AgentTool>, jobs: Arc<BackgroundJobs>) -> Self {
        Self {
            inner,
            jobs,
            conversations: Arc::new(ConversationStore::new(
                conversation::ConversationLimits {
                    max: 8,
                    idle: Duration::from_secs(86400),
                },
                None,
            )),
            reviews: None,
            session: None,
        }
    }

    /// Whether calls may set `review`: background jobs exist (this tool),
    /// the session keeps journals, and an offered agent can continue a
    /// conversation.
    fn offers_review(&self) -> bool {
        self.reviews.is_some() && !self.inner.continuing().is_empty()
    }

    /// Check a reviewed call, before approval and again before it starts.
    fn plan(&self, split: &Split, review: &Value) -> Result<review::Plan, ToolError> {
        if !self.offers_review() {
            return Err(ToolError::invalid_arguments(
                "review is not available in this session",
            ));
        }
        if split.background == Some(false) {
            return Err(ToolError::invalid_arguments(
                "a reviewed call runs as a background job; omit background or set it to true",
            ));
        }
        let (builder, routed) = self.inner.route(&split.arguments)?;
        builder.backend.risk(&routed)?;
        review::plan(
            &self.inner,
            builder,
            &routed,
            review,
            self.conversations.limits().max,
        )
    }

    /// Start a reviewed call as a background job. A busy builder
    /// conversation queues it, or fails it with `on_busy: fail`; it never
    /// waits, since it is a background job, and never steers, since its
    /// prompt opens a review of its own.
    fn start_reviewed(
        &self,
        split: &Split,
        review: &Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let plan = self.plan(split, review)?;
        let arguments = split.arguments.clone();
        let (agent, routed) = self.inner.route(&arguments)?;
        let turn = match parse_args::<AgentArgs>(&routed)?
            .session
            .filter(|session| conversation::is_handle(session))
        {
            None => Turn::New,
            Some(handle) => {
                let place = self.jobs.lanes.join(&handle);
                if place.ahead == 0 && !agent.backend.busy(&routed)? {
                    Turn::Next(place)
                } else if agent.on_busy(&routed)? == BusyBehavior::Fail {
                    return Err(busy(&handle, &place));
                } else {
                    queue_room(agent, &handle, &place)?;
                    Turn::Queued(place)
                }
            }
        };
        let dir = self.reviews.as_deref().ok_or_else(|| {
            ToolError::invalid_arguments("review is not available in this session")
        })?;
        let reviewers: Vec<String> = plan
            .reviewer_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let (rounds, land) = (plan.rounds, plan.land);
        let tool = Arc::new(ReviewedTool::new(
            Arc::clone(&self.inner),
            Arc::clone(&self.jobs.lanes),
            Arc::clone(&self.conversations),
            plan,
            arguments.clone(),
            self.session.clone(),
            dir,
        )?);
        let queued = matches!(turn, Turn::Queued(_));
        let mut started = self.jobs.start(
            Arc::clone(&tool) as Arc<dyn Tool>,
            AGENT_TOOL,
            &agent.name,
            arguments,
            context,
            turn,
            Some(&tool),
        )?;
        let first = reviewers.first().cloned().unwrap_or_default();
        started["review"] = json!({
            "reviewers": reviewers,
            "rounds": rounds,
            "land": review::land_name(land),
            "journal": tool.journal_id(),
        });
        started["note"] = format!(
            "{}The agent works on it, then a fresh {first} reviews it (or the next reviewer \
             available), for up to {rounds} rounds. SCV reports the result, with its own Review \
             and Landing lines, in a new turn when the review ends. agent_status shows the \
             round and phase; agent_cancel stops the whole review.",
            if queued {
                "The conversation is busy, so this reviewed call is queued behind the turns \
                 ahead of it. "
            } else {
                ""
            }
        )
        .into();
        Ok(ToolOutput::success(started.to_string()))
    }

    /// Start the call on `agent` as a background job of this session.
    fn start(
        &self,
        agent: &Offered,
        arguments: Value,
        context: &ToolContext,
        turn: Turn,
    ) -> Result<ToolOutput, ToolError> {
        let tool: Arc<dyn Tool> = if matches!(turn, Turn::Queued(_)) {
            Arc::new(QueuedTool(Arc::clone(&self.inner)))
        } else {
            Arc::clone(&self.inner) as Arc<dyn Tool>
        };
        let started = self.jobs.start(
            tool,
            AGENT_TOOL,
            &agent.name,
            arguments,
            context,
            turn,
            None,
        )?;
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
            description: "Wait for a background agent job (the `job` handle an agent call with \
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
            .wait(&args.job, limit, &context.cancellation, &context.call_id)
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
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: StatusArgs = parse_args(&arguments)?;
        let value = self.jobs.describe(args.job.as_deref(), &context.call_id)?;
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
            description: "Stop a running background agent job (the `job` handle an agent call \
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
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: CancelArgs = parse_args(&arguments)?;
        let value = self.jobs.cancel(&args.job, &context.call_id).await?;
        Ok(ToolOutput::success(value.to_string()))
    }
}

#[cfg(test)]
mod tests;
