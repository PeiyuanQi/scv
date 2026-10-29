//! The workbench's daemon side: the sessions in which it runs the owner's
//! own jobs (one per workspace, created on first use), the `daemon.control`
//! actions behind the dashboard and `scv jobs`, and the maintenance that
//! keeps its records tidy.
//!
//! What the workbench shows and does is `scv_tools::workbench`; this
//! module only decides where an owner's job runs and answers the control
//! actions.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use scv_core::{ApprovalGate, ToolRegistry};
use scv_protocol::{DaemonCommand, DaemonStatus, DelegationSummary, WorktreeInfo};
use scv_tools::{
    DelegationContext, SkillMap,
    background::{BackgroundJobs, ledger},
    builtin_registry,
    delegation::DelegationRegistry,
    workbench::{SessionHandle, Workbench},
};
use tokio::sync::Mutex;

use crate::{
    approval::{PolicyFirstGate, UnattendedGate},
    config::{ConfigOverrides, Instance},
    session::{build::offered_adapters, persist},
};

/// A workbench session with no job running or waiting is dropped after
/// this long unused, with its conversations.
const HOST_IDLE: Duration = Duration::from_secs(3600);
/// How long shutting the workbench down waits for its jobs' agents to stop.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// The session label of the workbench's own sessions.
pub(crate) const LABEL: &str = "workbench";

/// The daemon's workbench and the sessions it runs the owner's jobs in.
pub(crate) struct Hosts {
    instance: Instance,
    registry: Arc<DelegationRegistry>,
    workbench: Arc<Workbench>,
    hosts: Mutex<HashMap<PathBuf, Host>>,
}

/// The workbench's session for one workspace.
struct Host {
    session: String,
    jobs: Arc<BackgroundJobs>,
    /// Keeps the session's `agent` tool, conversations, and job store alive.
    _tools: Arc<ToolRegistry>,
    _registration: SessionHandle,
    /// In the restart activity table while this session lives, so
    /// `scv restart --when-idle` waits for its jobs as for any other session.
    _activity: crate::restart::SessionTracker,
    last_used: Instant,
}

impl Hosts {
    pub(crate) fn new(instance: Instance, registry: Arc<DelegationRegistry>) -> Arc<Self> {
        let workbench = Workbench::new(&instance.layout, Arc::clone(&registry));
        Arc::new(Self {
            instance,
            registry,
            workbench,
            hosts: Mutex::default(),
        })
    }

    /// The daemon's workbench.
    pub(crate) fn workbench(&self) -> &Arc<Workbench> {
        &self.workbench
    }

    /// The workbench's session for `workspace`, an existing absolute
    /// directory, created on first use.
    pub(crate) async fn session_for(self: &Arc<Self>, workspace: &str) -> Result<String> {
        let path = Path::new(workspace);
        if !path.is_absolute() {
            bail!("the workspace must be an absolute directory");
        }
        let workspace = std::fs::canonicalize(path)
            .with_context(|| format!("resolve workspace {workspace}"))?;
        if !workspace.is_dir() {
            bail!("the workspace is not a directory");
        }
        let mut hosts = self.hosts.lock().await;
        if let Some(host) = hosts.get_mut(&workspace) {
            host.last_used = Instant::now();
            return Ok(host.session.clone());
        }
        // Loading the configuration and building the tools reads files.
        let this = Arc::clone(self);
        let path = workspace.clone();
        let host = tokio::task::spawn_blocking(move || this.build(&path))
            .await
            .context("build the workbench session")??;
        let session = host.session.clone();
        hosts.insert(workspace, host);
        Ok(session)
    }

    fn build(&self, workspace: &Path) -> Result<Host> {
        let layout = &self.instance.layout;
        let config = crate::config::Config::load(
            layout,
            workspace,
            ConfigOverrides {
                approval_policy: self.instance.overrides.approval_policy,
                config_file: self.instance.overrides.config_file.clone(),
                ..ConfigOverrides::default()
            },
        )?;
        if config.agent.max_background == 0 {
            bail!("background jobs are off (agent.max_background = 0), so the workbench runs none");
        }
        config.prepare_adapter_homes()?;
        let session = scv_tools::workbench::host_session(workspace);
        let registration = self.workbench.register_session(&session, LABEL, workspace);
        // Nobody is asked for a job of the owner's own: what the policy
        // decides on its own stands, and while the owner watches, a request
        // it would put to a person waits for the owner instead of being
        // denied.
        let unattended: Arc<dyn ApprovalGate> = Arc::new(UnattendedGate {
            policy: config.tools.approval_policy,
            client_approves_all: false,
        });
        let gate: Arc<dyn ApprovalGate> = Arc::new(PolicyFirstGate {
            policy: config.tools.approval_policy,
            attended: self.workbench.attended_gate(&session, unattended),
        });
        let mut jobs = BackgroundJobs::new(config.agent.max_background, None).with_approvals(gate);
        if let Some(ledger) = ledger::JobLedger::new(&layout.jobs(), &session, LABEL, workspace) {
            jobs = jobs.with_ledger(ledger);
        }
        let jobs = Arc::new(jobs);
        let activity = crate::restart::SessionTracker::new(layout.home(), &session, Some(&jobs));
        let mut tools = config.tools();
        tools.delegation = Some(DelegationContext {
            registry: Arc::clone(&self.registry),
            session: session.clone(),
            depth: 0,
        });
        tools.background = Some(Arc::clone(&jobs));
        tools.reviews = Some(layout.reviews());
        tools.worktrees = Some(layout.worktrees());
        tools.workbench = Some(Arc::clone(&self.workbench));
        tools.label = LABEL.to_owned();
        tools.workspace = workspace.to_owned();
        let registry = builtin_registry(
            tools,
            SkillMap::new(),
            Vec::new(),
            0,
            offered_adapters(&config),
        )
        .map_err(|error| anyhow!(error.message))?;
        if registry.get("agent").is_none() {
            bail!("no delegated agent is installed and signed in for this workspace");
        }
        Ok(Host {
            session,
            jobs,
            _tools: Arc::new(registry),
            _registration: registration,
            _activity: activity,
            last_used: Instant::now(),
        })
    }

    /// Drop workbench sessions that have had nothing running or waiting,
    /// and no owner action, for [`HOST_IDLE`]. A session with a job going,
    /// or a conversation taken over, counts as used, so the hour starts
    /// when the last of them ends.
    pub(crate) async fn expire_idle(&self) {
        let taken_over = self.workbench.takeover_sessions();
        self.hosts.lock().await.retain(|_, host| {
            if host.jobs.pending() > 0 || taken_over.contains(&host.session) {
                host.last_used = Instant::now();
            }
            host.last_used.elapsed() < HOST_IDLE
        });
    }

    /// An owner action reached workbench session `session`: it counts as
    /// used now.
    pub(crate) async fn touch(&self, session: &str) {
        if let Some(host) = self
            .hosts
            .lock()
            .await
            .values_mut()
            .find(|host| host.session == session)
        {
            host.last_used = Instant::now();
        }
    }

    /// End every takeover, then drop every workbench session, which stops
    /// their jobs, and wait a little for their agents to go.
    pub(crate) async fn shutdown(&self) {
        self.workbench.end_takeovers().await;
        let sessions: Vec<String> = {
            let mut hosts = self.hosts.lock().await;
            let sessions = hosts.values().map(|host| host.session.clone()).collect();
            hosts.clear();
            sessions
        };
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        while Instant::now() < deadline
            && self
                .registry
                .list(false)
                .iter()
                .any(|entry| sessions.contains(&entry.record.session))
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Whether `command` is one of the workbench's control actions.
pub(crate) fn is_workbench_command(command: &DaemonCommand) -> bool {
    matches!(
        command,
        DaemonCommand::Workbench { .. }
            | DaemonCommand::RunLog { .. }
            | DaemonCommand::WorkbenchStart { .. }
            | DaemonCommand::WorkbenchSend { .. }
            | DaemonCommand::WorkbenchCancel { .. }
            | DaemonCommand::WorkbenchApprove { .. }
            | DaemonCommand::WorkbenchResume { .. }
            | DaemonCommand::WorkbenchDismiss { .. }
            | DaemonCommand::JobResult { .. }
            | DaemonCommand::Worktrees
            | DaemonCommand::WorktreeRemove { .. }
            | DaemonCommand::Attach { .. }
            | DaemonCommand::AttachIo { .. }
            | DaemonCommand::AttachDetach { .. }
            | DaemonCommand::AttachEnd { .. }
    )
}

/// Answer a workbench control action. A refusal's message is safe to show.
pub(crate) async fn control(
    hosts: &Arc<Hosts>,
    command: DaemonCommand,
) -> std::result::Result<DaemonStatus, String> {
    let workbench = hosts.workbench();
    // A taken-over terminal is read every 50 ms and at each key: those
    // replies leave the delegations, which take a process table scan, out.
    let mut status = if matches!(
        command,
        DaemonCommand::Attach { .. }
            | DaemonCommand::AttachIo { .. }
            | DaemonCommand::AttachDetach { .. }
    ) {
        status_with(DelegationSummary::default())
    } else {
        base_status(&hosts.registry)
    };
    let mut snapshot = true;
    match command {
        DaemonCommand::Workbench { watch } => {
            if watch {
                workbench.watch();
            }
        }
        DaemonCommand::RunLog {
            handle,
            after,
            limit,
        } => {
            let limit = limit.map_or(scv_tools::activity::MAX_PAGE_EVENTS, |limit| {
                usize::try_from(limit).unwrap_or(usize::MAX)
            });
            let workbench = Arc::clone(workbench);
            let page =
                tokio::task::spawn_blocking(move || workbench.run_log(&handle, after, limit))
                    .await
                    .map_err(|error| error.to_string())??;
            status.run_log = Some(page);
            snapshot = false;
        }
        DaemonCommand::WorkbenchStart {
            workspace,
            agent,
            prompt,
            cwd,
            model,
            effort,
            worktree,
        } => {
            let session = hosts
                .session_for(&workspace)
                .await
                .map_err(|error| format!("{error:#}"))?;
            let mut arguments = serde_json::json!({"agent": agent, "prompt": prompt});
            for (key, value) in [("cwd", cwd), ("model", model), ("effort", effort)] {
                if let Some(value) = value {
                    arguments[key] = value.into();
                }
            }
            if worktree {
                arguments["worktree"] = true.into();
            }
            let job = workbench.start(&session, arguments).await?;
            status.workbench_note = Some(format!("Started {job}."));
            status.workbench_job = Some(job);
        }
        DaemonCommand::WorkbenchSend {
            session,
            conversation,
            prompt,
            mode,
        } => {
            hosts.touch(&session).await;
            match workbench
                .send(&session, &conversation, &prompt, mode)
                .await?
            {
                scv_tools::workbench::Sent::Job(job) => {
                    status.workbench_note = Some(format!("Sent to {conversation} as {job}."));
                    status.workbench_job = Some(job);
                }
                scv_tools::workbench::Sent::Steered => {
                    status.workbench_note = Some(format!(
                        "{conversation} took the prompt into its running turn."
                    ));
                }
            }
        }
        DaemonCommand::WorkbenchCancel { session, job } => {
            status.workbench_note = Some(workbench.cancel(&session, &job)?);
        }
        DaemonCommand::WorkbenchApprove { id, approved } => {
            status.workbench_note = Some(workbench.approve(&id, approved)?);
        }
        DaemonCommand::WorkbenchResume { session, job } => {
            let record = workbench
                .job_record(&session, &job)
                .ok_or_else(|| format!("no record of {session}/{job}"))?;
            let host = hosts
                .session_for(&record.workspace.display().to_string())
                .await
                .map_err(|error| format!("{error:#}"))?;
            let resumed = workbench.resume(&host, &session, &job).await?;
            status.workbench_note = Some(format!("Resumed {session}/{job} as {resumed}."));
            status.workbench_job = Some(resumed);
        }
        DaemonCommand::WorkbenchDismiss { session, job } => {
            let workbench = Arc::clone(workbench);
            status.workbench_note = Some(
                tokio::task::spawn_blocking(move || workbench.dismiss(&session, &job))
                    .await
                    .map_err(|error| error.to_string())??,
            );
        }
        DaemonCommand::JobResult { session, job } => {
            let workbench = Arc::clone(workbench);
            status.job_result = Some(
                tokio::task::spawn_blocking(move || workbench.job_result(&session, &job))
                    .await
                    .map_err(|error| error.to_string())??,
            );
            snapshot = false;
        }
        DaemonCommand::Worktrees => {
            status.worktrees = worktrees(hosts).await;
            snapshot = false;
        }
        DaemonCommand::WorktreeRemove { path, force } => {
            {
                let workbench = Arc::clone(workbench);
                let path = PathBuf::from(&path);
                tokio::task::spawn_blocking(move || workbench.check_worktree_unused(&path))
                    .await
                    .map_err(|error| error.to_string())??;
            }
            let root = hosts.instance.layout.worktrees();
            status.workbench_note =
                Some(scv_tools::worktree::remove(&root, Path::new(&path), force).await?);
            status.worktrees = worktrees(hosts).await;
            snapshot = false;
        }
        DaemonCommand::Attach {
            session,
            conversation,
            cols,
            rows,
            interrupt,
            interrupt_job,
        } => {
            hosts.touch(&session).await;
            if interrupt {
                let (page, note) = workbench
                    .interrupt_and_attach(
                        &session,
                        &conversation,
                        cols,
                        rows,
                        interrupt_job.as_deref(),
                    )
                    .await?;
                status.attach = Some(page);
                status.workbench_note = note;
            } else {
                status.attach = Some(
                    workbench
                        .attach_terminal(&session, &conversation, cols, rows)
                        .await?,
                );
            }
            snapshot = false;
        }
        DaemonCommand::AttachIo {
            id,
            after,
            input,
            cols,
            rows,
        } => {
            status.attach = Some(workbench.attach_io(&id, after, &input, cols, rows).await?);
            snapshot = false;
        }
        DaemonCommand::AttachDetach { id } => {
            status.workbench_note = Some(workbench.detach(&id)?);
            snapshot = false;
        }
        DaemonCommand::AttachEnd {
            session,
            conversation,
        } => {
            hosts.touch(&session).await;
            let (note, page) = workbench.end_takeover(&session, &conversation).await?;
            status.workbench_note = Some(note);
            status.attach = Some(page);
        }
        _ => return Err("not a workbench action".into()),
    }
    if snapshot {
        let workbench = Arc::clone(workbench);
        let layout = hosts.instance.layout.clone();
        let mut taken = tokio::task::spawn_blocking(move || {
            let mut snapshot = workbench.snapshot();
            snapshot.restorable = persist::restorable(&layout);
            snapshot
        })
        .await
        .map_err(|error| error.to_string())?;
        taken.restorable.truncate(32);
        status.workbench = Some(taken);
    }
    Ok(status)
}

/// The worktrees SCV created, with the jobs recorded in each.
async fn worktrees(hosts: &Hosts) -> Vec<WorktreeInfo> {
    let mut found = scv_tools::worktree::list(&hosts.instance.layout.worktrees()).await;
    let jobs = hosts.instance.layout.jobs();
    let records = tokio::task::spawn_blocking(move || ledger::list(&jobs))
        .await
        .unwrap_or_default();
    for info in &mut found {
        info.jobs = records
            .iter()
            .filter(|record| {
                record
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.path == info.worktree.path)
            })
            .map(|record| format!("{}/{}", record.session, record.job))
            .collect();
    }
    found
}

/// A status naming this daemon and its delegations, without its
/// components, whose lock a dashboard's frequent requests never wait for.
fn base_status(registry: &DelegationRegistry) -> DaemonStatus {
    status_with(DelegationSummary {
        active: registry.list(false).len() as u64,
        idle: None,
        reaped: registry.reaped_total(),
        entries: Vec::new(),
        killed: Vec::new(),
    })
}

/// A status naming this daemon, with `delegations`.
fn status_with(delegations: DelegationSummary) -> DaemonStatus {
    DaemonStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        components: Vec::new(),
        delegations,
        restart: None,
        confirm: None,
        project: None,
        mail_actions: Vec::new(),
        mail_note: None,
        workbench: None,
        run_log: None,
        job_result: None,
        workbench_note: None,
        workbench_job: None,
        worktrees: Vec::new(),
        attach: None,
    }
}

/// Settle what stopped processes left unfinished (jobs and runs become
/// `interrupted`), remove old records, and drop idle workbench sessions.
/// The daemon runs it at startup and with its delegation reconcile, every
/// minute.
pub(crate) async fn maintain(hosts: &Hosts) {
    let workbench = Arc::clone(hosts.workbench());
    match tokio::task::spawn_blocking(move || workbench.maintain()).await {
        Ok(done) => {
            for job in &done.interrupted_jobs {
                tracing::info!(
                    "Background job {job} was interrupted: the SCV process that ran it stopped"
                );
            }
        }
        Err(error) => tracing::warn!(%error, "workbench maintenance failed"),
    }
    hosts.expire_idle().await;
}

#[cfg(test)]
mod tests;
