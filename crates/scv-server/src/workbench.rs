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
    approval::UnattendedGate,
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
    pub(crate) async fn session_for(&self, workspace: &str) -> Result<String> {
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
        let host = self.build(&workspace)?;
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
        // Nobody is asked for a job of the owner's own; while the owner
        // watches, a request the policy would deny waits for the owner.
        let unattended: Arc<dyn ApprovalGate> = Arc::new(UnattendedGate {
            policy: config.tools.approval_policy,
            client_approves_all: false,
        });
        let mut jobs = BackgroundJobs::new(config.agent.max_background, None)
            .with_approvals(self.workbench.attended_gate(&session, unattended));
        if let Some(ledger) = ledger::JobLedger::new(&layout.jobs(), &session, LABEL, workspace) {
            jobs = jobs.with_ledger(ledger);
        }
        let jobs = Arc::new(jobs);
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
            last_used: Instant::now(),
        })
    }

    /// Drop workbench sessions with nothing running or waiting that were
    /// unused for [`HOST_IDLE`].
    pub(crate) async fn expire_idle(&self) {
        self.hosts
            .lock()
            .await
            .retain(|_, host| host.jobs.pending() > 0 || host.last_used.elapsed() < HOST_IDLE);
    }

    /// Drop every workbench session, which stops their jobs, and wait a
    /// little for their agents to go.
    pub(crate) async fn shutdown(&self) {
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
            | DaemonCommand::Worktrees
            | DaemonCommand::WorktreeRemove { .. }
    )
}

/// Answer a workbench control action. A refusal's message is safe to show.
pub(crate) async fn control(
    hosts: &Arc<Hosts>,
    command: DaemonCommand,
) -> std::result::Result<DaemonStatus, String> {
    let workbench = hosts.workbench();
    let mut status = base_status(&hosts.registry);
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
        } => match workbench
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
        },
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
            status.workbench_note = Some(workbench.dismiss(&session, &job)?);
        }
        DaemonCommand::Worktrees => {
            status.worktrees = worktrees(hosts).await;
            snapshot = false;
        }
        DaemonCommand::WorktreeRemove { path, force } => {
            let root = hosts.instance.layout.worktrees();
            status.workbench_note =
                Some(scv_tools::worktree::remove(&root, Path::new(&path), force).await?);
            status.worktrees = worktrees(hosts).await;
            snapshot = false;
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
    let records = ledger::list(&hosts.instance.layout.jobs());
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
    DaemonStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        components: Vec::new(),
        delegations: DelegationSummary {
            active: registry.list(false).len() as u64,
            idle: None,
            reaped: registry.reaped_total(),
            entries: Vec::new(),
            killed: Vec::new(),
        },
        restart: None,
        confirm: None,
        project: None,
        mail_actions: Vec::new(),
        mail_note: None,
        workbench: None,
        run_log: None,
        workbench_note: None,
        workbench_job: None,
        worktrees: Vec::new(),
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
