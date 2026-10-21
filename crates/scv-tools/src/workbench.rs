//! The daemon's workbench: one place that knows every delegated agent, background
//! job, and session of the instance, and lets the owner act on them.
//!
//! What it shows comes from the durable records every SCV process of the
//! instance writes (the job ledger, each run's activity log and state, and
//! the delegation records), so jobs and runs of `scv exec` servers, and of
//! processes that stopped, are listed too. What it does needs the live
//! objects, so only sessions of this process can be acted on: each
//! registers with [`Workbench::register_session`], and its `agent` tool
//! attaches its job store when the registry builds it.
//!
//! The owner acts on:
//!
//! - a conversation: [`Workbench::send`] gives it the owner's prompt as a
//!   new turn, queued behind a running one, or steered into it;
//! - a job: [`Workbench::cancel`] stops it, [`Workbench::resume`] runs an
//!   interrupted one again, [`Workbench::dismiss`] forgets a finished one;
//! - an approval request a background job relayed while the owner watches
//!   ([`Workbench::watch`]): it waits for [`Workbench::approve`] up to
//!   [`ATTENDED_WAIT`], then gets the session's unattended answer.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use scv_client::Layout;
use scv_core::{AgentError, ApprovalGate, ApprovalRequest};
use scv_protocol::{
    AgentState, RunLogPage, SendMode, WorkbenchApproval, WorkbenchJob, WorkbenchRun,
    WorkbenchSession, WorkbenchSnapshot,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    delegate::{
        activity::{self, RunState},
        background::{
            BackgroundCapable, BackgroundJobs, LiveJob, OwnerCall,
            ledger::{self, JobRecord},
        },
        records::{DelegationRegistry, ProcessIdentity},
    },
    sync::lock,
};

/// How long a background job's approval request waits for the owner while
/// the owner watches, before it gets the session's unattended answer.
pub const ATTENDED_WAIT: Duration = Duration::from_secs(120);
/// How long one `watch` keeps the owner counted as watching.
pub const WATCH_TTL: Duration = Duration::from_secs(15);
/// Jobs and runs a snapshot lists at most: every unfinished one, then the
/// newest finished ones.
const MAX_LISTED: usize = 200;
/// Finished jobs and ended runs are kept this long.
pub const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// Ended runs kept at most.
pub const MAX_RUNS: usize = 500;

/// The daemon's workbench.
pub struct Workbench {
    runs: PathBuf,
    jobs: PathBuf,
    delegations: Arc<DelegationRegistry>,
    inner: Mutex<Inner>,
    watched_until: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for Workbench {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Workbench").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Slot>,
    approvals: BTreeMap<String, Pending>,
    next: u64,
}

/// A live session of this process.
struct Slot {
    token: u64,
    label: String,
    workspace: PathBuf,
    state: AgentState,
    detail: Option<String>,
    agent: Option<Weak<BackgroundCapable>>,
    jobs: Option<Weak<BackgroundJobs>>,
}

struct Pending {
    info: WorkbenchApproval,
    answer: Option<oneshot::Sender<bool>>,
}

/// A session's place in the workbench while it lives; dropping it removes
/// the session.
pub struct SessionHandle {
    workbench: Weak<Workbench>,
    id: String,
    token: u64,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("id", &self.id)
            .finish()
    }
}

impl SessionHandle {
    /// What the session is doing: a turn runs, an approval waits for its
    /// client, or it is idle.
    pub fn set_state(&self, state: AgentState, detail: Option<String>) {
        let Some(workbench) = self.workbench.upgrade() else {
            return;
        };
        if let Some(slot) = lock(&workbench.inner).sessions.get_mut(&self.id)
            && slot.token == self.token
        {
            slot.state = state;
            slot.detail = detail;
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let Some(workbench) = self.workbench.upgrade() else {
            return;
        };
        let mut inner = lock(&workbench.inner);
        if inner
            .sessions
            .get(&self.id)
            .is_some_and(|slot| slot.token == self.token)
        {
            inner.sessions.remove(&self.id);
        }
    }
}

/// What [`Workbench::send`] did with the owner's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// A background job of the session runs it: `<session>/<job>`.
    Job(String),
    /// It went into the conversation's running turn.
    Steered,
}

/// What a maintenance pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Maintenance {
    /// Jobs found interrupted: their SCV process stopped.
    pub interrupted_jobs: Vec<String>,
    /// Runs found interrupted.
    pub interrupted_runs: Vec<String>,
    /// Old records removed.
    pub pruned: usize,
}

impl Workbench {
    /// The workbench of the instance at `layout`, whose delegated runs
    /// `delegations` records.
    pub fn new(layout: &Layout, delegations: Arc<DelegationRegistry>) -> Arc<Self> {
        Arc::new(Self {
            runs: layout.runs(),
            jobs: layout.jobs(),
            delegations,
            inner: Mutex::default(),
            watched_until: Mutex::new(None),
        })
    }

    /// Where the job ledger is.
    pub fn jobs_dir(&self) -> &Path {
        &self.jobs
    }

    /// Register a live session; `label` says who it serves.
    pub fn register_session(
        self: &Arc<Self>,
        id: &str,
        label: &str,
        workspace: &Path,
    ) -> SessionHandle {
        let mut inner = lock(&self.inner);
        inner.next += 1;
        let token = inner.next;
        inner.sessions.insert(
            id.to_owned(),
            Slot {
                token,
                label: label.to_owned(),
                workspace: workspace.to_owned(),
                state: AgentState::Idle,
                detail: None,
                agent: None,
                jobs: None,
            },
        );
        SessionHandle {
            workbench: Arc::downgrade(self),
            id: id.to_owned(),
            token,
        }
    }

    /// The `agent` tool and job store of live session `session`.
    pub(crate) fn attach(
        &self,
        session: &str,
        agent: Weak<BackgroundCapable>,
        jobs: Weak<BackgroundJobs>,
    ) {
        if let Some(slot) = lock(&self.inner).sessions.get_mut(session) {
            slot.agent = Some(agent);
            slot.jobs = Some(jobs);
        }
    }

    /// The owner is watching: background approval requests wait for an
    /// answer for the next [`WATCH_TTL`].
    pub fn watch(&self) {
        *lock(&self.watched_until) = Some(Instant::now() + WATCH_TTL);
    }

    fn watched(&self) -> bool {
        lock(&self.watched_until).is_some_and(|until| Instant::now() < until)
    }

    /// The approval gate for background jobs of `session`: `unattended`
    /// decides, except that a request it would deny waits up to
    /// [`ATTENDED_WAIT`] for the owner while the owner watches. It never
    /// denies what `unattended` allows.
    pub fn attended_gate(
        self: &Arc<Self>,
        session: &str,
        unattended: Arc<dyn ApprovalGate>,
    ) -> Arc<dyn ApprovalGate> {
        Arc::new(AttendedGate {
            workbench: Arc::downgrade(self),
            session: session.to_owned(),
            unattended,
        })
    }

    /// Hold `request` of a job of `session` for the owner's answer.
    async fn ask(
        &self,
        session: &str,
        request: ApprovalRequest,
        cancellation: &CancellationToken,
    ) -> Result<bool, AgentError> {
        let (sender, receiver) = oneshot::channel();
        let now = unix_now();
        let id = format!("ap-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        lock(&self.inner).approvals.insert(
            id.clone(),
            Pending {
                info: WorkbenchApproval {
                    id: id.clone(),
                    session: session.to_owned(),
                    job: request.call_id.clone(),
                    name: request.name.clone(),
                    risk: request.risk.as_str().to_owned(),
                    cwd: request.cwd.display().to_string(),
                    summary: request.summary.clone(),
                    since_unix_seconds: now,
                    expires_unix_seconds: now + ATTENDED_WAIT.as_secs(),
                },
                answer: Some(sender),
            },
        );
        let answer = tokio::select! {
            answer = receiver => Ok(answer.unwrap_or(false)),
            () = tokio::time::sleep(ATTENDED_WAIT) => Ok(false),
            () = cancellation.cancelled() => Err(AgentError::Cancelled),
        };
        lock(&self.inner).approvals.remove(&id);
        answer
    }

    /// Answer approval request `id`.
    pub fn approve(&self, id: &str, approved: bool) -> Result<String, String> {
        let pending = lock(&self.inner).approvals.get_mut(id).and_then(|pending| {
            pending
                .answer
                .take()
                .map(|answer| (answer, pending.info.clone()))
        });
        let Some((answer, info)) = pending else {
            return Err(format!(
                "no approval request {id:?} waits; it may have been answered or timed out"
            ));
        };
        if answer.send(approved).is_err() {
            return Err(format!("{} no longer waits for this answer", info.job));
        }
        Ok(format!(
            "{} {}: {}",
            if approved { "Approved" } else { "Denied" },
            info.job,
            bounded(&info.summary, 120)
        ))
    }

    /// Everything the workbench shows now. `restorable` is left for the
    /// server, which keeps session queues.
    pub fn snapshot(&self) -> WorkbenchSnapshot {
        let now = unix_now();
        let live_handles: HashSet<String> = self
            .delegations
            .list(true)
            .into_iter()
            .filter(|entry| entry.processes > 0)
            .map(|entry| entry.record.handle)
            .collect();
        let (slots, approvals): (Vec<_>, Vec<_>) = {
            let inner = lock(&self.inner);
            (
                inner
                    .sessions
                    .iter()
                    .map(|(id, slot)| SlotView {
                        id: id.clone(),
                        label: slot.label.clone(),
                        workspace: slot.workspace.clone(),
                        state: slot.state,
                        detail: slot.detail.clone(),
                        jobs: slot.jobs.as_ref().and_then(Weak::upgrade),
                    })
                    .collect(),
                inner
                    .approvals
                    .values()
                    .map(|pending| pending.info.clone())
                    .collect(),
            )
        };
        let live_jobs: HashMap<(String, String), LiveJob> = slots
            .iter()
            .filter_map(|slot| {
                slot.jobs
                    .as_ref()
                    .map(|jobs| (slot.id.clone(), jobs.live()))
            })
            .flat_map(|(session, jobs)| {
                jobs.into_iter()
                    .map(move |job| ((session.clone(), job.id.clone()), job))
            })
            .collect();
        let live_sessions: HashSet<&str> = slots.iter().map(|slot| slot.id.as_str()).collect();
        let awaiting: HashMap<(String, String), &WorkbenchApproval> = approvals
            .iter()
            .map(|approval| ((approval.session.clone(), approval.job.clone()), approval))
            .collect();

        // Conversation handles belong to the process that made them: one an
        // earlier daemon left names nothing (or another conversation) now,
        // even under a session ID that came back.
        let me = ProcessIdentity::current();
        let ours = |owner: &ProcessIdentity| me.as_ref() == Some(owner);
        let mut states = activity::list_states(&self.runs);
        states
            .sort_by_key(|state| std::cmp::Reverse((state.last_activity_unix, state.started_unix)));
        let mut runs: Vec<WorkbenchRun> = Vec::new();
        let mut job_runs: HashMap<(String, String), &RunState> = HashMap::new();
        for state in &states {
            if let Some(job) = &state.job {
                job_runs
                    .entry((state.session.clone(), job.clone()))
                    .or_insert(state);
            }
        }
        let mut ended_listed = 0;
        for state in &states {
            if state.ended {
                if ended_listed >= MAX_LISTED {
                    continue;
                }
                ended_listed += 1;
            }
            let (mut current, mut detail) = activity::current(state, now);
            if let Some(job) = &state.job
                && let Some(approval) = awaiting.get(&(state.session.clone(), job.clone()))
                && !state.ended
            {
                current = AgentState::AwaitingInput;
                detail = Some(format!(
                    "waiting for your approval: {}",
                    bounded(&approval.summary, 200)
                ));
            }
            runs.push(WorkbenchRun {
                handle: state.handle.clone(),
                agent: state.agent.clone(),
                session: state.session.clone(),
                transport: state.transport.clone(),
                conversation: state.conversation.clone().filter(|_| ours(&state.owner)),
                turn: state.turn,
                job: state.job.clone(),
                cwd: state.cwd.display().to_string(),
                state: current,
                detail,
                started_unix_seconds: state.started_unix,
                last_activity_unix_seconds: state.last_activity_unix,
                live: !state.ended && live_handles.contains(&state.handle),
                steerable: state.steerable,
            });
        }

        let mut records = ledger::list(&self.jobs);
        records.sort_by_key(|record| std::cmp::Reverse((record.created_unix, record.job.clone())));
        let mut jobs = Vec::new();
        let mut finished_listed = 0;
        for record in &records {
            let key = (record.session.clone(), record.job.clone());
            let live = live_jobs.get(&key);
            let run = job_runs.get(&key).copied();
            let (state, detail) = job_state(
                record,
                live,
                run,
                awaiting.get(&key).copied(),
                live_sessions.contains(record.session.as_str()),
                now,
            );
            if state.is_final() {
                if finished_listed >= MAX_LISTED {
                    continue;
                }
                finished_listed += 1;
            }
            let cwd = run
                .map(|run| run.cwd.display().to_string())
                .or_else(|| {
                    record
                        .worktree
                        .as_ref()
                        .map(|worktree| worktree.path.clone())
                })
                .unwrap_or_else(|| {
                    let relative = record.arguments.get("cwd").and_then(Value::as_str);
                    relative.map_or_else(
                        || record.workspace.display().to_string(),
                        |cwd| record.workspace.join(cwd).display().to_string(),
                    )
                });
            jobs.push(WorkbenchJob {
                session: record.session.clone(),
                job: record.job.clone(),
                agent: record.agent.clone(),
                task: record.task.clone(),
                workspace: record.workspace.display().to_string(),
                cwd,
                state,
                detail,
                started_by: record.started_by,
                conversation: record
                    .conversation
                    .clone()
                    .or_else(|| run.and_then(|run| run.conversation.clone()))
                    .or_else(|| {
                        record
                            .arguments
                            .get("session")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .filter(|_| ours(&record.owner)),
                run: run.map(|run| run.handle.clone()),
                worktree: record.worktree.clone(),
                reviewed: record.reviewed,
                resumable: state == AgentState::Interrupted
                    && !record.reviewed
                    && record.resumed_as.is_none(),
                resumed_as: record.resumed_as.clone(),
                created_unix_seconds: record.created_unix,
                updated_unix_seconds: record.updated_unix,
            });
        }

        let mut sessions: Vec<WorkbenchSession> = slots
            .iter()
            .map(|slot| WorkbenchSession {
                id: slot.id.clone(),
                label: slot.label.clone(),
                workspace: slot.workspace.display().to_string(),
                live: true,
                state: slot.state,
                detail: slot.detail.clone(),
            })
            .collect();
        let mut seen: HashSet<String> = sessions.iter().map(|session| session.id.clone()).collect();
        for record in &records {
            if seen.insert(record.session.clone()) {
                let alive = record.owner.is_alive() && record.owner.pid != std::process::id();
                sessions.push(WorkbenchSession {
                    id: record.session.clone(),
                    label: record.label.clone(),
                    workspace: record.workspace.display().to_string(),
                    live: alive,
                    state: AgentState::Unknown,
                    detail: Some(if alive {
                        format!("runs in SCV process {}", record.owner.pid)
                    } else {
                        "ended".to_owned()
                    }),
                });
            }
        }
        sessions.sort_by(|a, b| b.live.cmp(&a.live).then_with(|| a.id.cmp(&b.id)));
        WorkbenchSnapshot {
            generated_unix_seconds: now,
            sessions,
            jobs,
            runs,
            approvals,
            restorable: Vec::new(),
        }
    }

    /// One page of run `handle`'s activity log after byte position `after`.
    pub fn run_log(&self, handle: &str, after: u64, limit: usize) -> Result<RunLogPage, String> {
        activity::read_page(&self.runs, handle, after, limit).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("no activity log for run {handle:?}")
            } else {
                format!("cannot read run {handle:?}: {error}")
            }
        })
    }

    /// The `agent` tool of live session `session`.
    fn agent(&self, session: &str) -> Result<(Arc<BackgroundCapable>, PathBuf), String> {
        let inner = lock(&self.inner);
        let slot = inner.sessions.get(session).ok_or_else(|| {
            format!("session {session} does not run in this daemon (it ended, or another SCV process runs it)")
        })?;
        let agent = slot
            .agent
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| format!("session {session} runs no background agent jobs"))?;
        Ok((agent, slot.workspace.clone()))
    }

    /// Give conversation `conversation` of live session `session` the
    /// owner's `prompt`.
    pub async fn send(
        &self,
        session: &str,
        conversation: &str,
        prompt: &str,
        mode: SendMode,
    ) -> Result<Sent, String> {
        let (agent, workspace) = self.agent(session)?;
        let call = agent
            .owner_call(
                json!({"session": conversation, "prompt": prompt}),
                workspace,
                mode,
            )
            .await
            .map_err(|error| error.message)?;
        Ok(match call {
            OwnerCall::Job(started) => Sent::Job(job_name(session, &started)),
            OwnerCall::Steered => Sent::Steered,
        })
    }

    /// Start the owner's own job in live session `session` with the `agent`
    /// call `arguments`. Returns `<session>/<job>`.
    pub async fn start(&self, session: &str, arguments: Value) -> Result<String, String> {
        let (agent, workspace) = self.agent(session)?;
        match agent
            .owner_call(arguments, workspace, SendMode::Queue)
            .await
            .map_err(|error| error.message)?
        {
            OwnerCall::Job(started) => Ok(job_name(session, &started)),
            OwnerCall::Steered => Err("the prompt went into a running turn".into()),
        }
    }

    /// Run interrupted job `job` of `session` again as the owner's job in
    /// live session `host`, which serves the job's workspace. It continues
    /// the agent's own conversation when the agent saved one and can resume
    /// it; otherwise it starts over, told that an earlier attempt may have
    /// left changes. Returns `<host>/<new job>`.
    pub async fn resume(&self, host: &str, session: &str, job: &str) -> Result<String, String> {
        let record = ledger::load(&self.jobs, session, job)
            .ok_or_else(|| format!("no record of {session}/{job}"))?;
        let session_live = lock(&self.inner).sessions.contains_key(session);
        let interrupted = record.state == AgentState::Interrupted
            || (!record.state.is_final()
                && (!record.owner.is_alive()
                    || (record.owner.pid == std::process::id() && !session_live)));
        if !interrupted {
            return Err(format!(
                "{job} was not interrupted ({}); only an interrupted job can be resumed",
                record.state
            ));
        }
        if record.reviewed {
            return Err(
                "a reviewed job is not resumed: its journal is its record. Start a new \
                 reviewed call whose brief restates the open findings and anything already \
                 landed"
                    .into(),
            );
        }
        if let Some(resumed) = &record.resumed_as {
            return Err(format!("{job} was already resumed as {resumed}"));
        }
        let (agent, _) = self.agent(host)?;
        let run = activity::list_states(&self.runs)
            .into_iter()
            .filter(|state| state.session == session && state.job.as_deref() == Some(job))
            .max_by_key(|state| state.started_unix);
        let cwd = run
            .as_ref()
            .map(|run| run.cwd.clone())
            .filter(|cwd| cwd.is_dir());
        let adopted = run
            .as_ref()
            .and_then(|run| run.vendor.as_deref().zip(cwd.as_deref()))
            .and_then(|(vendor, cwd)| agent.adopt(&record.agent, cwd, vendor));
        let mut arguments = if let Some(handle) = &adopted {
            json!({
                "session": handle,
                "prompt": format!(
                    "[SCV] SCV stopped before your last turn on this task finished (a restart \
                     or a crash), so it was cut off. Check where the work stands (files, git \
                     status, anything still running), then carry on. The task was:\n\n{}",
                    record.prompt
                ),
            })
        } else {
            rerun_arguments(&record, |option| agent.takes(&record.agent, option))
        };
        if let Some(object) = arguments.as_object_mut() {
            for key in ["timeout_seconds", "no_timeout"] {
                if let Some(value) = record.arguments.get(key) {
                    object.insert(key.into(), value.clone());
                }
            }
        }
        let resumed = self.start(host, arguments).await?;
        let (host_session, new_job) = resumed.split_once('/').unwrap_or((host, &resumed));
        ledger::update(&self.jobs, host_session, new_job, |new| {
            new.resumed_from = Some(format!("{session}/{job}"));
            if new.worktree.is_none() {
                new.worktree.clone_from(&record.worktree);
            }
        });
        ledger::update(&self.jobs, session, job, |old| {
            old.resumed_as = Some(resumed.clone());
            if !old.state.is_final() {
                ledger::interrupt(old, "its session ended before it finished");
            }
        });
        Ok(resumed)
    }

    /// Stop job `job` of live session `session`.
    pub fn cancel(&self, session: &str, job: &str) -> Result<String, String> {
        let jobs = {
            let inner = lock(&self.inner);
            inner
                .sessions
                .get(session)
                .and_then(|slot| slot.jobs.as_ref())
                .and_then(Weak::upgrade)
        };
        let Some(jobs) = jobs else {
            return Err(format!(
                "session {session} does not run in this daemon; a job whose session ended \
                 stopped with it, and `scv agents kill` stops another process's runs"
            ));
        };
        jobs.cancel_for_owner(job)?;
        Ok(format!(
            "Stopping {job}: its agent and every process it started are stopped; work it \
             already wrote stays."
        ))
    }

    /// Forget finished or interrupted job `job` of `session`.
    pub fn dismiss(&self, session: &str, job: &str) -> Result<String, String> {
        let record = ledger::load(&self.jobs, session, job)
            .ok_or_else(|| format!("no record of {session}/{job}"))?;
        let running = lock(&self.inner)
            .sessions
            .get(session)
            .and_then(|slot| slot.jobs.as_ref())
            .and_then(Weak::upgrade)
            .is_some_and(|jobs| {
                jobs.live()
                    .iter()
                    .any(|live| live.id == job && live.running)
            });
        if running || (!record.state.is_final() && record.owner.is_alive()) {
            return Err(format!("{job} is still running; stop it first"));
        }
        ledger::remove(&self.jobs, session, job).map_err(|error| error.to_string())?;
        Ok(format!("Forgot {session}/{job}."))
    }

    /// The ledger record of `job` in `session`.
    pub fn job_record(&self, session: &str, job: &str) -> Option<JobRecord> {
        ledger::load(&self.jobs, session, job)
    }

    /// Settle what processes that stopped left unfinished, and remove old
    /// records: the daemon runs this at startup and every minute.
    pub fn maintain(&self) -> Maintenance {
        let interrupted_jobs = ledger::settle_orphans(&self.jobs)
            .into_iter()
            .map(|record| format!("{}/{}", record.session, record.job))
            .collect();
        let interrupted_runs = activity::settle_orphans(&self.runs);
        let pruned = ledger::prune(&self.jobs, KEEP) + activity::prune(&self.runs, KEEP, MAX_RUNS);
        Maintenance {
            interrupted_jobs,
            interrupted_runs,
            pruned,
        }
    }
}

struct SlotView {
    id: String,
    label: String,
    workspace: PathBuf,
    state: AgentState,
    detail: Option<String>,
    jobs: Option<Arc<BackgroundJobs>>,
}

/// What a job is doing, from its record, its live entry when its session
/// runs here, the run serving it, and an approval it waits on.
fn job_state(
    record: &JobRecord,
    live: Option<&LiveJob>,
    run: Option<&RunState>,
    awaiting: Option<&WorkbenchApproval>,
    session_live: bool,
    now: u64,
) -> (AgentState, Option<String>) {
    if record.state.is_final() {
        let detail = match record.state {
            AgentState::Interrupted => record.interrupted.clone(),
            AgentState::Completed => None,
            _ => record
                .error
                .as_deref()
                .and_then(|error| error.lines().find(|line| !line.trim().is_empty()))
                .map(|line| bounded(line, 200)),
        };
        return (record.state, detail);
    }
    if let Some(approval) = awaiting {
        return (
            AgentState::AwaitingInput,
            Some(format!(
                "waiting for your approval: {}",
                bounded(&approval.summary, 200)
            )),
        );
    }
    if let Some(live) = live {
        if live.waiting {
            let behind = record
                .arguments
                .get("session")
                .and_then(Value::as_str)
                .map_or_else(String::new, |handle| format!(" {handle}'s"));
            return (
                AgentState::Queued,
                Some(format!("waits for conversation{behind} running turn")),
            );
        }
        if live.running {
            if let Some(run) = run.filter(|run| !run.ended) {
                return activity::current(run, now);
            }
            return (
                AgentState::Working,
                live.progress
                    .clone()
                    .or_else(|| Some(format!("running for {} s", live.elapsed.as_secs()))),
            );
        }
    }
    if !record.owner.is_alive() {
        return (
            AgentState::Interrupted,
            Some("the SCV process that ran it stopped".into()),
        );
    }
    if record.owner.pid == std::process::id() && !session_live {
        return (
            AgentState::Interrupted,
            Some("its session ended before it finished".into()),
        );
    }
    match run {
        Some(run) if !run.ended => activity::current(run, now),
        _ => (record.state, None),
    }
}

/// The workbench's own session ID for `workspace`: stable, so the owner's
/// jobs in one workspace group under one session across daemon restarts.
pub fn host_session(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("workbench-{hex}")
}

/// `<session>/<job>` of a job an `agent` call started.
fn job_name(session: &str, started: &Value) -> String {
    format!(
        "{session}/{}",
        started.get("job").and_then(Value::as_str).unwrap_or("job")
    )
}

/// Decides a background job's approval request: the session's unattended
/// answer, or, for one it would deny while the owner watches, the owner's.
struct AttendedGate {
    workbench: Weak<Workbench>,
    session: String,
    unattended: Arc<dyn ApprovalGate>,
}

#[async_trait]
impl ApprovalGate for AttendedGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        if self
            .unattended
            .approve(request.clone(), cancellation.clone())
            .await?
        {
            return Ok(true);
        }
        match self.workbench.upgrade() {
            Some(workbench) if workbench.watched() => {
                workbench.ask(&self.session, request, &cancellation).await
            }
            _ => Ok(false),
        }
    }
}

fn bounded(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The `agent` call that runs interrupted job `record` again from its
/// prompt in a new conversation: its options but those the host agent does
/// not `take`, a note that an earlier attempt may have left changes, and
/// the worktree it ran in when that is still there.
fn rerun_arguments(record: &JobRecord, takes: impl Fn(&str) -> bool) -> Value {
    let mut arguments = record.arguments.clone();
    if let Some(object) = arguments.as_object_mut() {
        object.remove("session");
        object.remove("worktree");
        object.remove("on_busy");
        // What this session's agent does not take is left to its own
        // defaults.
        for option in ["model", "effort"] {
            if !takes(option) {
                object.remove(option);
            }
        }
        object.insert("agent".into(), record.agent.clone().into());
        object.insert(
            "prompt".into(),
            format!(
                "[SCV] An earlier attempt at this task was cut off when SCV stopped (a restart \
                 or a crash), and its conversation could not be resumed. It may have left \
                 partial changes here: check them first, then do the task:\n\n{}",
                record.prompt
            )
            .into(),
        );
        if let Some(worktree) = record
            .worktree
            .as_ref()
            .filter(|worktree| Path::new(&worktree.path).is_dir())
        {
            object.insert("cwd".into(), worktree.path.clone().into());
        }
    }
    arguments
}

#[cfg(test)]
mod tests;
