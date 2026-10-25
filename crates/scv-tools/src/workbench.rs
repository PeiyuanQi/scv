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
            BackgroundCapable, BackgroundJobs, JobMeta, LiveJob, OwnerCall,
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
    worktrees: PathBuf,
    delegations: Arc<DelegationRegistry>,
    inner: Mutex<Inner>,
    watched_until: Mutex<Option<Instant>>,
    /// Jobs being resumed now, `<session>/<job>`: each is resumed once.
    resuming: Mutex<HashSet<String>>,
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
            worktrees: layout.worktrees(),
            delegations,
            inner: Mutex::default(),
            watched_until: Mutex::new(None),
            resuming: Mutex::default(),
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
        // An answer that lands as the deadline fires is still the answer.
        // Closing the receiver and taking anything already sent stops the
        // waiter from reporting a denial after `approve` has sent `true`.
        let mut receiver = receiver;
        let answer = tokio::select! {
            biased;
            answer = &mut receiver => Ok(answer.unwrap_or(false)),
            () = tokio::time::sleep(ATTENDED_WAIT) => {
                receiver.close();
                Ok(receiver.try_recv().unwrap_or(false))
            }
            () = cancellation.cancelled() => {
                receiver.close();
                match receiver.try_recv() {
                    Ok(approved) => Ok(approved),
                    Err(_) => Err(AgentError::Cancelled),
                }
            }
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
        let live_handles = self.live_handles();
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
                        agent: slot.agent.as_ref().and_then(Weak::upgrade),
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

        // A conversation handle is shown only while the live session that
        // made it still has it, so the owner can send it a prompt: one an
        // earlier daemon, or an earlier session under an ID that came back
        // (a workbench session), left names nothing now. Such a session
        // numbers its conversations after the ones its records name, so a
        // handle never comes to mean another conversation.
        let me = ProcessIdentity::current();
        let held = |owner: &ProcessIdentity, session: &str, handle: &str| {
            me.as_ref() == Some(owner)
                && slots.iter().any(|slot| {
                    slot.id == session
                        && slot.agent.as_ref().is_some_and(|agent| agent.knows(handle))
                })
        };
        let mut states = activity::list_states(&self.runs);
        states
            .sort_by_key(|state| std::cmp::Reverse((state.last_activity_unix, state.started_unix)));
        let mut runs: Vec<WorkbenchRun> = Vec::new();
        let mut job_runs: HashMap<(String, String), &RunState> = HashMap::new();
        for state in &states {
            for job in state.jobs.iter().chain(&state.job) {
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
                conversation: state
                    .conversation
                    .clone()
                    .filter(|handle| held(&state.owner, &state.session, handle)),
                turn: state.turn,
                job: state.job.clone(),
                jobs: state.jobs.clone(),
                cwd: state.cwd.display().to_string(),
                state: current,
                detail,
                started_unix_seconds: state.started_unix,
                last_activity_unix_seconds: state.last_activity_unix,
                live: !state.ended && live_handles.contains(&state.handle),
                steerable: state.steerable,
            });
        }

        let all = ledger::list_all(&self.jobs);
        let mut records: Vec<&JobRecord> = all.iter().filter(|record| !record.dismissed).collect();
        records.sort_by_key(|record| std::cmp::Reverse((record.created_unix, record.job.clone())));
        let mut jobs = Vec::new();
        let mut finished_listed = 0;
        // What each job was resumed as, by the record of the job resuming
        // it too, which is written first: a dismissed one's included, or
        // the job would be resumable again.
        let successors: HashMap<&str, String> = all
            .iter()
            .filter_map(|record| {
                let from = record.resumed_from.as_deref()?;
                Some((from, format!("{}/{}", record.session, record.job)))
            })
            .collect();
        for &record in &records {
            let key = (record.session.clone(), record.job.clone());
            let resumed_as = record.resumed_as.clone().or_else(|| {
                successors
                    .get(format!("{}/{}", record.session, record.job).as_str())
                    .cloned()
            });
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
            // An interrupted job the owner can still resume stays listed
            // after the cap of other finished jobs. Counting it as finished
            // would hide it once newer completions filled the snapshot.
            let resumable_interrupted =
                state == AgentState::Interrupted && resumed_as.is_none() && !record.reviewed;
            if state.is_final() && !resumable_interrupted {
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
                    .filter(|handle| held(&record.owner, &record.session, handle)),
                run: run.map(|run| run.handle.clone()),
                worktree: record.worktree.clone(),
                reviewed: record.reviewed,
                resumable: state == AgentState::Interrupted
                    && !record.reviewed
                    && resumed_as.is_none(),
                resumed_as,
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
        for &record in &records {
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

    /// The runs whose processes still run, in any SCV process of the
    /// instance.
    fn live_handles(&self) -> HashSet<String> {
        self.delegations
            .list(true)
            .into_iter()
            .filter(|entry| entry.processes > 0)
            .map(|entry| entry.record.handle)
            .collect()
    }

    /// Refuse to remove the worktree holding `path` while anything still
    /// works in it ([`Self::worktree_users`]).
    pub fn check_worktree_unused(&self, path: &Path) -> Result<(), String> {
        let users = self.worktree_users(path);
        if users.is_empty() {
            return Ok(());
        }
        Err(format!(
            "{} still work in {}; stop them before removing it",
            users.join(", "),
            path.display()
        ))
    }

    /// What still runs in the SCV worktree holding `path` (its top, or any
    /// directory in it): jobs not yet finished that recorded it, and
    /// delegated runs whose processes work inside it.
    pub fn worktree_users(&self, path: &Path) -> Vec<String> {
        let path = crate::delegate::worktree::worktree_path(&self.worktrees, path)
            .or_else(|| std::fs::canonicalize(path).ok())
            .unwrap_or_else(|| path.to_owned());
        let mut users: Vec<String> = ledger::list(&self.jobs)
            .into_iter()
            .filter(|record| {
                !record.state.is_final()
                    && record.owner.is_alive()
                    && record
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| Path::new(&worktree.path) == path)
            })
            .map(|record| format!("{}/{}", record.session, record.job))
            .collect();
        users.extend(
            self.delegations
                .list(true)
                .into_iter()
                .filter(|entry| entry.processes > 0 && entry.record.cwd.starts_with(&path))
                .map(|entry| entry.record.handle),
        );
        users
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
                JobMeta::default(),
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
        self.start_with(session, arguments, JobMeta::default())
            .await
    }

    /// [`Self::start`], with what the job's record holds from the start.
    async fn start_with(
        &self,
        session: &str,
        arguments: Value,
        meta: JobMeta,
    ) -> Result<String, String> {
        let (agent, workspace) = self.agent(session)?;
        match agent
            .owner_call(arguments, workspace, SendMode::Queue, meta)
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
    /// left changes. A queued follow-up has no run of its own: it uses the
    /// conversation's directory and worktree, continues that agent session
    /// only after the interrupted job the run served has been resumed (and
    /// then behind that conversation), and otherwise starts in a new
    /// worktree of the checkout that job recorded. Each job is resumed
    /// once, and not while an agent process of its run still goes. Returns
    /// `<host>/<new job>`.
    pub async fn resume(&self, host: &str, session: &str, job: &str) -> Result<String, String> {
        let key = format!("{session}/{job}");
        if !lock(&self.resuming).insert(key.clone()) {
            return Err(format!("{job} is being resumed already"));
        }
        // Released however this ends, a dropped request included.
        let _claim = Claim {
            claims: &self.resuming,
            key: &key,
        };
        self.resume_once(host, session, job, &key).await
    }

    async fn resume_once(
        &self,
        host: &str,
        session: &str,
        job: &str,
        key: &str,
    ) -> Result<String, String> {
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
        // Reading every record, run state, and the process table takes a
        // while.
        let (successor, run, live, blocking) = {
            let (jobs, runs) = (self.jobs.clone(), self.runs.clone());
            let delegations = Arc::clone(&self.delegations);
            let continued = record
                .arguments
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let (session, job, key) = (session.to_owned(), job.to_owned(), key.to_owned());
            tokio::task::spawn_blocking(move || {
                // A job that names it as what it resumes settles it, should
                // SCV have stopped before this record said so, even once
                // the owner dismissed that job.
                let successor = ledger::list_all(&jobs)
                    .into_iter()
                    .find(|record| record.resumed_from.as_deref() == Some(key.as_str()))
                    .map(|record| format!("{}/{}", record.session, record.job));
                let states = activity::list_states(&runs);
                let served = |state: &activity::RunState| {
                    state.session == session
                        && (state.job.as_deref() == Some(job.as_str()) || state.jobs.contains(&job))
                };
                let own = states
                    .iter()
                    .filter(|state| served(state))
                    .max_by_key(|state| state.started_unix)
                    .cloned();
                // A queued follow-up has no run of its own. The conversation
                // it continues does, and that run has the directory and the
                // agent's session.
                let fallback = if own.is_none() {
                    continued.as_deref().and_then(|handle| {
                        states
                            .iter()
                            .filter(|state| {
                                state.session == session
                                    && state.conversation.as_deref() == Some(handle)
                            })
                            .max_by_key(|state| state.started_unix)
                            .cloned()
                    })
                } else {
                    None
                };
                let run = own.or_else(|| fallback.clone());
                // Adopting while the job that run served is still interrupted
                // would start a second process in its agent session. The
                // follow-up waits until that job is resumed, then queues
                // behind the conversation that resume adopted.
                let blocking = match &fallback {
                    Some(found) if found.vendor.is_some() && found.cwd.is_dir() => {
                        unresumed_served(&jobs, found, &job)
                    }
                    _ => Vec::new(),
                };
                let live = run.as_ref().is_some_and(|run| {
                    delegations
                        .list(true)
                        .iter()
                        .any(|entry| entry.record.handle == run.handle && entry.processes > 0)
                });
                (successor, run, live, blocking)
            })
            .await
            .map_err(|error| error.to_string())?
        };
        if let Some(resumed) = successor {
            return Err(format!("{job} was already resumed as {resumed}"));
        }
        if live {
            return Err(format!(
                "{job}'s agent ({}) still runs; SCV stops what a stopped SCV process left \
                 within a minute (or `scv agents kill` it now). Resume it after that",
                run.as_ref().map_or("", |run| run.handle.as_str())
            ));
        }
        if !blocking.is_empty() {
            let ahead = if blocking.len() == 1 { "it" } else { "them" };
            return Err(format!(
                "{job} was queued behind {}; resume {ahead} first. This follow-up then waits \
                 behind the same conversation, so one agent session is not continued twice",
                blocking.join(", ")
            ));
        }
        let mut record = record;
        let cwd_live = run.as_ref().is_some_and(|found| found.cwd.is_dir());
        if let Some(found) = &run
            && cwd_live
        {
            remember_cwd(&mut record, &found.cwd);
            if record.worktree.is_none()
                && let Some(worktree) =
                    crate::delegate::worktree::worktree_ref(&self.worktrees, &found.cwd)
            {
                record.worktree = Some(worktree);
            }
        }
        // The directory is gone (the owner removed the worktree; an
        // interrupted job does not hold it). The job that run served still
        // records the checkout, dismissed records included, which is what a
        // new worktree is made from. The run's path still names the
        // subdirectory inside it.
        if record.worktree.is_none()
            && let Some(found) = &run
            && let Some(worktree) = served_worktree(&self.jobs, found)
        {
            record.worktree = Some(worktree);
        }
        if !cwd_live
            && let Some(found) = &run
            && let Some(worktree) = &record.worktree
            && found.cwd.starts_with(Path::new(&worktree.path))
        {
            remember_cwd(&mut record, &found.cwd);
        }
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
        // The new job's record says what it resumes from its first write.
        let meta = JobMeta {
            resumed_from: Some(key.to_owned()),
            worktree: record
                .worktree
                .clone()
                .filter(|worktree| Path::new(&worktree.path).is_dir()),
            ..JobMeta::default()
        };
        let resumed = self.start_with(host, arguments, meta).await?;
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
        let jobs = lock(&self.inner)
            .sessions
            .get(session)
            .and_then(|slot| slot.jobs.as_ref())
            .and_then(Weak::upgrade);
        let running = jobs.as_ref().is_some_and(|jobs| {
            jobs.live()
                .iter()
                .any(|live| live.id == job && live.running)
        });
        if running || (!record.state.is_final() && record.owner.is_alive()) {
            return Err(format!("{job} is still running; stop it first"));
        }
        // A session that has yet to report the result writes its record
        // again when it does: this daemon's says so itself, and one of
        // another live SCV process by its record.
        let elsewhere =
            record.owner.is_alive() && ProcessIdentity::current().as_ref() != Some(&record.owner);
        if jobs.is_some_and(|jobs| jobs.unreported(job)) || (elsewhere && !record.reported) {
            return Err(format!(
                "{job}'s result has not reached its session yet; dismiss it once it has"
            ));
        }
        ledger::dismiss(&self.jobs, session, job).map_err(|error| error.to_string())?;
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

/// A job claimed for resuming, released when dropped.
struct Claim<'a> {
    claims: &'a Mutex<HashSet<String>>,
    key: &'a str,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        lock(self.claims).remove(self.key);
    }
}

struct SlotView {
    id: String,
    label: String,
    workspace: PathBuf,
    state: AgentState,
    detail: Option<String>,
    agent: Option<Arc<BackgroundCapable>>,
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

/// Jobs whose turns `run` served, the latest one first.
fn served_job_names(run: &RunState) -> Vec<&str> {
    let mut names = Vec::new();
    if let Some(job) = run.job.as_deref() {
        names.push(job);
    }
    for job in &run.jobs {
        if !names.contains(&job.as_str()) {
            names.push(job.as_str());
        }
    }
    names
}

/// Interrupted jobs `run` served that the owner has not resumed, as
/// `session/job`. `skip` is the follow-up being resumed.
fn unresumed_served(root: &Path, run: &RunState, skip: &str) -> Vec<String> {
    served_job_names(run)
        .into_iter()
        .filter(|name| *name != skip)
        .filter_map(|name| {
            let record = ledger::load(root, &run.session, name)?;
            record
                .resumable()
                .then(|| format!("{}/{}", run.session, name))
        })
        .collect()
}

/// The worktree recorded on a job `run` served. A follow-up has none of
/// its own, and the run's directory may already have been removed. A
/// dismissed record still carries the worktree.
fn served_worktree(root: &Path, run: &RunState) -> Option<scv_protocol::WorktreeRef> {
    served_job_names(run).into_iter().find_map(|name| {
        ledger::load_including_dismissed(root, &run.session, name)
            .and_then(|record| record.worktree)
    })
}

/// Record `cwd` on `record` when the call did not name one.
fn remember_cwd(record: &mut ledger::JobRecord, cwd: &Path) {
    let has_cwd = record
        .arguments
        .get("cwd")
        .and_then(Value::as_str)
        .is_some_and(|cwd| !cwd.is_empty());
    if !has_cwd && let Some(object) = record.arguments.as_object_mut() {
        object.insert("cwd".into(), cwd.display().to_string().into());
    }
}

/// The `agent` call that runs interrupted job `record` again from its
/// prompt in a new conversation: its options but those the host agent does
/// not `take`, and a note that an earlier attempt may have left changes. A
/// job that ran in a worktree runs in the same directory of it while the
/// worktree is there, and in a new worktree of its repository once it was
/// removed, so its work never lands in the repository's own checkout.
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
        if let Some(worktree) = &record.worktree {
            let top = Path::new(&worktree.path);
            // The call's directory inside the worktree, as it was recorded.
            let inside = object
                .get("cwd")
                .and_then(Value::as_str)
                .and_then(|cwd| Path::new(cwd).strip_prefix(top).ok())
                .map(Path::to_path_buf)
                .unwrap_or_default();
            if top.is_dir() {
                let cwd = Some(top.join(&inside))
                    .filter(|cwd| cwd.is_dir())
                    .unwrap_or_else(|| top.to_owned());
                object.insert("cwd".into(), cwd.display().to_string().into());
            } else {
                let repo = Path::new(&worktree.repo);
                let cwd = Some(repo.join(&inside))
                    .filter(|cwd| cwd.is_dir())
                    .unwrap_or_else(|| repo.to_owned());
                object.insert("cwd".into(), cwd.display().to_string().into());
                object.insert("worktree".into(), true.into());
            }
        }
    }
    arguments
}

#[cfg(test)]
mod tests;
