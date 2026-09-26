//! Planned restarts into a newly installed release, and the notices around
//! them.
//!
//! `scv restart --when-idle` (run by the feature-flow deploy script after
//! `cargo install`) asks the daemon to restart into the binary now at its own
//! path. The daemon checks that binary runs, then waits until the delegation
//! that asked has finished and its report is stored, and no owner message is
//! being answered, or until the request's deadline. It then records a plan,
//! keeps a copy of its own binary for rollback, and starts a watchdog outside
//! its own cgroup (`systemd-run`). The watchdog restarts the unit, checks that
//! the new release comes up with the channels that were connected before,
//! and otherwise puts the previous binary back when both releases share a
//! config layout. The daemon that starts next announces the outcome in the
//! chat that asked, or through the notify list.
//!
//! The same notifier tells the owner about restarts after a crash and about
//! accounts that stay disconnected.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex as SyncMutex, PoisonError, Weak, atomic::AtomicBool},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use scv_channels::hub::{Hub, Origin, Restart};
use scv_client::Layout;
use scv_protocol::{ComponentState, DaemonCommand, RestartInfo};
use scv_tools::{background::BackgroundJobs, delegation::DelegationRegistry};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::components::Components;
use crate::config::Instance;

/// Where configuration and state files live and how they are shaped. Bump it
/// when a release reads or writes them in a way the previous release cannot:
/// a rollback between releases with different layouts is refused.
pub const CONFIG_LAYOUT: u32 = 1;

const DEFAULT_MAX_WAIT: u64 = 10 * 60;
const MAX_WAIT_LIMIT: u64 = 60 * 60;
/// How long the watchdog gives a new release to report its version and
/// reconnect the channels that were connected before.
const VERIFY_SECONDS: u64 = 180;
/// How long the watchdog waits for a rolled-back release to come back.
const ROLLBACK_SECONDS: u64 = 90;
/// Checks a restart must pass in a row before it goes ahead, a second apart,
/// so a job that just finished has time to start its report.
const CLEAR_CHECKS: u32 = 2;
/// An account disconnected this long gets a notice through another account.
const DOWN_NOTICE_AFTER: Duration = Duration::from_secs(10 * 60);
const MONITOR_INTERVAL: Duration = Duration::from_secs(30);
/// A plan restarted this long ago no longer explains interrupted work.
const RESTART_CONTEXT_MAX_AGE: u64 = 60 * 60;

/// What a binary reports about itself for a planned restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub version: String,
    pub config_layout: u32,
}

/// This binary's build information, printed by `scv build-info`.
pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        config_layout: CONFIG_LAYOUT,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// Waiting for the requesting work to end.
    Waiting,
    /// The watchdog is restarting the unit and checking the new release.
    Restarting,
    /// The new release came up with its channels.
    Verified,
    /// The new release failed and the previous binary was put back.
    RolledBack,
    /// The new release failed and was not rolled back, or the restart could
    /// not start.
    Failed,
}

/// The delegation that asked for a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requester {
    pub handle: String,
    pub session: String,
}

/// A planned restart, saved in `<home>/state/update.json` (mode 0600) and
/// shared by the daemon that plans it, the watchdog, and the next daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub state: PlanState,
    pub from_version: String,
    pub to_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub from_layout: u32,
    pub to_layout: u32,
    pub unit: String,
    /// The daemon's executable, where the new release was installed.
    pub binary: PathBuf,
    /// A copy of the release the daemon ran, for rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<Requester>,
    /// The chat that asked, which hears the outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// Accounts connected when the restart went ahead; the new release
    /// must reconnect them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected: Vec<String>,
    pub requested_unix: u64,
    pub deadline_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_unix: Option<u64>,
    /// The restart went ahead at the deadline while work still ran.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waited_out: bool,
    /// Why the new release failed, for the announcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// How long the watchdog gives the new release.
    #[serde(default = "default_verify_seconds")]
    pub verify_seconds: u64,
}

fn default_verify_seconds() -> u64 {
    VERIFY_SECONDS
}

impl Plan {
    fn info(&self, waiting_for: Option<String>) -> RestartInfo {
        RestartInfo {
            to_version: self.to_version.clone(),
            waiting_for,
            requester: self.requester.as_ref().map(|r| r.handle.clone()),
            origin: self.origin.as_ref().map(|origin| origin.component.clone()),
            deadline_unix_seconds: self.deadline_unix,
        }
    }

    fn label(&self) -> String {
        match &self.commit {
            Some(commit) => format!("v{} ({commit})", self.to_version),
            None => format!("v{}", self.to_version),
        }
    }
}

pub(crate) fn load_plan(path: &Path) -> Result<Option<Plan>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parse restart plan {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub(crate) fn save_plan(path: &Path, plan: &Plan) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(plan)?)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    scv_client::fs::replace_private(path, bytes)
        .with_context(|| format!("write {}", path.display()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The daemon's executable path. Linux names an executable that was
/// replaced on disk `<path> (deleted)`; the path is where the new one is.
fn own_executable() -> Result<PathBuf> {
    std::env::current_exe()
        .map(strip_deleted)
        .context("locate the daemon's executable")
}

fn strip_deleted(path: PathBuf) -> PathBuf {
    match path
        .to_str()
        .and_then(|text| text.strip_suffix(" (deleted)"))
    {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

/// Whether this process runs in `unit`'s cgroup.
fn runs_as_unit(unit: &str) -> bool {
    let suffix = format!("/{unit}");
    std::fs::read_to_string("/proc/self/cgroup")
        .is_ok_and(|text| text.lines().any(|line| line.ends_with(&suffix)))
}

/// Run `binary build-info` and parse what it reports.
async fn probe(binary: &Path) -> Result<BuildInfo> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .arg("build-info")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("it did not answer within 10 seconds"))??;
    if !output.status.success() {
        bail!("it exited with {}", output.status);
    }
    serde_json::from_slice(&output.stdout).context("it printed no build information")
}

// ---------------------------------------------------------------------------
// Sessions' own activity, which a restart waits out for the session that
// asked (its report turn, or a turn the TUI started).

struct SessionActivity {
    busy: AtomicBool,
    background: Option<Weak<BackgroundJobs>>,
}

static SESSIONS: LazyLock<SyncMutex<HashMap<String, Arc<SessionActivity>>>> =
    LazyLock::new(Default::default);

/// A daemon session's entry in the activity table while it lives.
pub(crate) struct SessionTracker {
    id: String,
    activity: Arc<SessionActivity>,
}

impl SessionTracker {
    pub(crate) fn new(id: &str, background: Option<&Arc<BackgroundJobs>>) -> Self {
        let activity = Arc::new(SessionActivity {
            busy: AtomicBool::new(false),
            background: background.map(Arc::downgrade),
        });
        SESSIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned(), Arc::clone(&activity));
        Self {
            id: id.to_owned(),
            activity,
        }
    }

    /// A turn runs, or a finished background job waits for its report turn.
    pub(crate) fn set_busy(&self, busy: bool) {
        self.activity
            .busy
            .store(busy, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for SessionTracker {
    fn drop(&mut self) {
        SESSIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

fn session_busy(id: &str) -> bool {
    let activity = SESSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(id)
        .cloned();
    activity.is_some_and(|activity| {
        activity.busy.load(std::sync::atomic::Ordering::Acquire)
            || activity
                .background
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|jobs| jobs.running() > 0)
    })
}

// ---------------------------------------------------------------------------
// Notices: where a message nobody asked for goes.

/// A place a notice may go: an account, and the chat partner there, or the
/// account's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    component: String,
    peer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Send {
        component: String,
        peer: String,
    },
    /// An earlier candidate may still connect.
    Wait,
    Nothing,
}

/// The first candidate that is connected and whose peer is known. Until the
/// grace period is over, a candidate that may still connect keeps its place
/// ahead of later ones.
fn pick(
    candidates: &[Candidate],
    states: &HashMap<String, ComponentState>,
    owner: &dyn Fn(&str) -> Option<Option<String>>,
    exclude: Option<&str>,
    grace_over: bool,
) -> Pick {
    for candidate in candidates {
        if exclude == Some(candidate.component.as_str()) {
            continue;
        }
        let registered = owner(&candidate.component);
        let peer = candidate
            .peer
            .clone()
            .or_else(|| registered.clone().flatten());
        match (states.get(&candidate.component), &registered, peer) {
            (Some(ComponentState::Connected), Some(_), Some(peer)) => {
                return Pick::Send {
                    component: candidate.component.clone(),
                    peer,
                };
            }
            (
                Some(
                    ComponentState::Starting
                    | ComponentState::Connected
                    | ComponentState::Disconnected
                    | ComponentState::Backoff,
                ),
                _,
                _,
            ) if !grace_over => return Pick::Wait,
            _ => {}
        }
    }
    Pick::Nothing
}

/// The human name of a component's channel.
fn channel_title(component: &str) -> &str {
    match component.split(':').next() {
        Some("wechat") => "WeChat",
        Some("feishu") => "Feishu",
        Some(other) => other,
        None => component,
    }
}

/// Where component states come from.
#[derive(Clone)]
enum States {
    Components(Weak<Mutex<Components>>),
    #[cfg(test)]
    Fixed(Arc<SyncMutex<HashMap<String, ComponentState>>>),
}

impl States {
    async fn get(&self) -> HashMap<String, ComponentState> {
        match self {
            Self::Components(components) => match components.upgrade() {
                Some(components) => components
                    .lock()
                    .await
                    .status()
                    .components
                    .into_iter()
                    .filter(|health| health.enabled)
                    .map(|health| (health.id, health.state))
                    .collect(),
                None => HashMap::new(),
            },
            #[cfg(test)]
            Self::Fixed(states) => states.lock().unwrap().clone(),
        }
    }
}

/// Sends notices to the owner through the hub.
#[derive(Clone)]
pub(crate) struct Notifier {
    hub: Arc<Hub>,
    states: States,
    /// How long an account ahead in line may take to connect.
    grace: Duration,
    /// When an undeliverable notice is dropped.
    give_up: Duration,
    poll: Duration,
    /// Where the notify list is configured.
    instance: Instance,
    /// The notify list; `None` reads it from the user configuration.
    #[cfg(test)]
    list: Option<Vec<String>>,
}

impl Notifier {
    pub(crate) fn new(
        instance: Instance,
        hub: Arc<Hub>,
        components: Weak<Mutex<Components>>,
    ) -> Self {
        Self {
            hub,
            instance,
            states: States::Components(components),
            grace: Duration::from_secs(120),
            give_up: Duration::from_secs(15 * 60),
            poll: Duration::from_secs(2),
            #[cfg(test)]
            list: None,
        }
    }

    fn notify_list(&self) -> Vec<String> {
        #[cfg(test)]
        if let Some(list) = &self.list {
            return list.clone();
        }
        self.instance.load_user().map_or_else(
            |error| {
                tracing::warn!(
                    "Notices use the owner's last chat; configuration failed: {error:#}"
                );
                Vec::new()
            },
            |config| config.notify.owner,
        )
    }

    /// The notify list, or else the chat the owner last wrote from.
    fn candidates(&self) -> Vec<Candidate> {
        let list = self.notify_list();
        if !list.is_empty() {
            return list
                .into_iter()
                .map(|component| Candidate {
                    component,
                    peer: None,
                })
                .collect();
        }
        self.hub
            .last_owner()
            .map(|last| Candidate {
                component: last.component,
                peer: Some(last.peer),
            })
            .into_iter()
            .collect()
    }

    /// Store `text` for the `origin` chat, or, when it is not given or does
    /// not connect in time, for the first reachable notify target other than
    /// `exclude`. Returns where it went.
    pub(crate) async fn deliver(
        &self,
        origin: Option<&Origin>,
        text: &str,
        exclude: Option<&str>,
        cancel: &CancellationToken,
    ) -> Option<String> {
        let started = tokio::time::Instant::now();
        let fallback = self.candidates();
        // The asking chat alone first; the notify targets once its grace is
        // over, saying why the answer comes there.
        let mut phase = match origin {
            Some(origin) => (
                vec![Candidate {
                    component: origin.component.clone(),
                    peer: Some(origin.peer.clone()),
                }],
                None,
                text.to_owned(),
            ),
            None => (fallback.clone(), exclude, text.to_owned()),
        };
        let mut phase_started = started;
        loop {
            let states = self.states.get().await;
            let grace_over = phase_started.elapsed() >= self.grace;
            if let Some(origin) = origin
                && grace_over
                && phase.1.is_none()
            {
                phase = (
                    fallback.clone(),
                    Some(origin.component.as_str()),
                    format!(
                        "(You asked on {}, which is not connected, so this comes here.) {text}",
                        channel_title(&origin.component)
                    ),
                );
                phase_started = tokio::time::Instant::now();
                continue;
            }
            let (candidates, exclude, text) = &phase;
            let owner = |component: &str| self.hub.owner(component);
            match pick(candidates, &states, &owner, *exclude, grace_over) {
                Pick::Send { component, peer } => {
                    match self.hub.notify(&component, &peer, text).await {
                        Ok(()) => return Some(component),
                        Err(error) => tracing::warn!("Notice to {component} not stored: {error}"),
                    }
                }
                Pick::Nothing if grace_over => {
                    tracing::warn!("No connected account can take this notice: {text}");
                    return None;
                }
                Pick::Wait | Pick::Nothing => {}
            }
            if started.elapsed() >= self.give_up {
                tracing::warn!("Gave up delivering a notice: {text}");
                return None;
            }
            tokio::select! {
                () = cancel.cancelled() => return None,
                () = tokio::time::sleep(self.poll) => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The daemon side: requests, waiting, and handing over to the watchdog.

/// How the restart is carried out once it may go ahead.
enum Launcher {
    /// A watchdog unit started with `systemd-run`.
    Systemd,
    /// Tests record the plan instead.
    #[cfg(test)]
    Record(Arc<SyncMutex<Vec<Plan>>>),
}

/// Plans restarts for the daemon.
pub(crate) struct Restarter {
    launcher: Launcher,
    instance: Instance,
    hub: Arc<Hub>,
    registry: Arc<DelegationRegistry>,
    notifier: Notifier,
    components: Weak<Mutex<Components>>,
    cancel: CancellationToken,
    /// The plan being waited on or carried out, and what it waits for.
    current: SyncMutex<Option<(Plan, Option<String>)>>,
}

impl Restarter {
    pub(crate) fn new(
        instance: Instance,
        hub: Arc<Hub>,
        registry: Arc<DelegationRegistry>,
        components: &Arc<Mutex<Components>>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            launcher: Launcher::Systemd,
            notifier: Notifier::new(
                instance.clone(),
                Arc::clone(&hub),
                Arc::downgrade(components),
            ),
            instance,
            hub,
            registry,
            components: Arc::downgrade(components),
            cancel,
            current: SyncMutex::new(None),
        })
    }

    pub(crate) fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// The scheduled restart, for status replies.
    pub(crate) fn info(&self) -> Option<RestartInfo> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(plan, waiting)| plan.info(waiting.clone()))
    }

    /// Handle `restart_when_idle`. The error is shown to the caller.
    pub(crate) async fn request(
        self: &Arc<Self>,
        command: DaemonCommand,
    ) -> std::result::Result<RestartInfo, String> {
        let DaemonCommand::RestartWhenIdle {
            version,
            commit,
            parent,
            max_wait_seconds,
        } = command
        else {
            return Err("not a restart request".into());
        };
        if let Some(info) = self.info() {
            return if version.as_deref().is_none_or(|v| v == info.to_version) {
                Ok(info)
            } else {
                Err(format!(
                    "a restart into v{} is already scheduled",
                    info.to_version
                ))
            };
        }
        let unit = self.instance.layout.service_name();
        if !runs_as_unit(&unit) {
            return Err(format!(
                "this daemon does not run as {unit}, so it cannot restart itself; \
                 restart it yourself"
            ));
        }
        let binary = own_executable().map_err(|error| format!("{error:#}"))?;
        let installed = probe(&binary).await.map_err(|error| {
            format!(
                "the binary at {} does not run ({error:#}); not restarting",
                binary.display()
            )
        })?;
        if let Some(version) = &version
            && version != &installed.version
        {
            return Err(format!(
                "{} reports v{}, not v{version}; not restarting",
                binary.display(),
                installed.version
            ));
        }
        let requester = parent.as_deref().and_then(|chain| self.requester(chain));
        let now = unix_now();
        let wait = max_wait_seconds
            .unwrap_or(DEFAULT_MAX_WAIT)
            .min(MAX_WAIT_LIMIT);
        let plan = Plan {
            id: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
            state: PlanState::Waiting,
            from_version: env!("CARGO_PKG_VERSION").into(),
            to_version: installed.version,
            commit: commit.filter(|commit| !commit.trim().is_empty()),
            from_layout: CONFIG_LAYOUT,
            to_layout: installed.config_layout,
            unit,
            previous: None,
            binary,
            requester,
            origin: None,
            expected: Vec::new(),
            requested_unix: now,
            deadline_unix: now + wait,
            restart_unix: None,
            waited_out: false,
            detail: None,
            verify_seconds: VERIFY_SECONDS,
        };
        // An owner confirmation step would go here, before the plan is armed.
        self.arm(plan)
    }

    /// Save `plan` and wait for it in the background.
    fn arm(self: &Arc<Self>, mut plan: Plan) -> std::result::Result<RestartInfo, String> {
        plan.origin = plan
            .requester
            .as_ref()
            .and_then(|requester| self.hub.origin(&requester.session));
        save_plan(&self.instance.layout.update_plan(), &plan)
            .map_err(|error| format!("{error:#}"))?;
        let waiting = self.waiting_for(&plan);
        let info = plan.info(waiting.clone());
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((plan.clone(), waiting));
        tracing::info!(
            "Restart into v{} scheduled; waiting at most {} seconds",
            plan.to_version,
            plan.deadline_unix.saturating_sub(plan.requested_unix)
        );
        let restarter = Arc::clone(self);
        tokio::spawn(async move { restarter.wait_and_restart(plan).await });
        Ok(info)
    }

    /// The delegation of this daemon named in a `SCV_PARENT` chain.
    fn requester(&self, chain: &str) -> Option<Requester> {
        let own = std::process::id();
        let entries = self.registry.list(true);
        chain.split(';').find_map(|entry| {
            let mut parts = entry.splitn(3, '/');
            let (instance, session, handle) = (parts.next()?, parts.next()?, parts.next()?);
            if instance != self.registry.instance() {
                return None;
            }
            entries
                .iter()
                .find(|running| running.record.handle == handle && running.record.owner.pid == own)
                .map(|_| Requester {
                    handle: handle.to_owned(),
                    session: session.to_owned(),
                })
        })
    }

    /// What the restart still waits for, or `None` when it may go ahead.
    fn waiting_for(&self, plan: &Plan) -> Option<String> {
        if let Some(requester) = &plan.requester {
            let running = self
                .registry
                .list(true)
                .into_iter()
                .any(|entry| entry.record.handle == requester.handle && entry.processes > 0);
            if running {
                return Some(format!("{} to finish", requester.handle));
            }
            if session_busy(&requester.session) || self.hub.session_work(&requester.session) > 0 {
                return Some(format!("{}'s report", requester.handle));
            }
        }
        if self.hub.owner_claims() > 0 {
            return Some("an owner message to be answered".into());
        }
        None
    }

    async fn wait_and_restart(self: Arc<Self>, mut plan: Plan) {
        let mut clear = 0;
        loop {
            tokio::select! {
                // The daemon is stopping: the next one finds the plan waiting.
                () = self.cancel.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let waiting = self.waiting_for(&plan);
            clear = if waiting.is_none() { clear + 1 } else { 0 };
            if let Some((_, current)) = self
                .current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
            {
                current.clone_from(&waiting);
            }
            if clear >= CLEAR_CHECKS {
                break;
            }
            if unix_now() >= plan.deadline_unix {
                tracing::warn!(
                    "Restarting into v{} at its deadline while waiting for {}",
                    plan.to_version,
                    waiting.as_deref().unwrap_or("work")
                );
                plan.waited_out = true;
                break;
            }
        }
        if let Err(error) = self.hand_over(&mut plan).await {
            tracing::error!("Restart into v{} did not start: {error:#}", plan.to_version);
            plan.state = PlanState::Failed;
            plan.detail = Some(format!("the restart did not start: {error:#}"));
            let _ = save_plan(&self.instance.layout.update_plan(), &plan);
            *self.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
            let text = announcement(&plan, env!("CARGO_PKG_VERSION"));
            self.notifier
                .deliver(plan.origin.as_ref(), &text, None, &self.cancel)
                .await;
            let _ = std::fs::remove_file(self.instance.layout.update_plan());
        }
    }

    /// Record the plan as restarting, keep this release's binary, and start
    /// the watchdog that restarts the unit.
    async fn hand_over(&self, plan: &mut Plan) -> Result<()> {
        plan.state = PlanState::Restarting;
        plan.restart_unix = Some(unix_now());
        if let Some(components) = self.components.upgrade() {
            plan.expected = components
                .lock()
                .await
                .status()
                .components
                .into_iter()
                .filter(|health| health.enabled && health.state == ComponentState::Connected)
                .map(|health| health.id)
                .collect();
        }
        match &self.launcher {
            Launcher::Systemd => {}
            #[cfg(test)]
            Launcher::Record(plans) => {
                save_plan(&self.instance.layout.update_plan(), plan)?;
                plans.lock().unwrap().push(plan.clone());
                return Ok(());
            }
        }
        plan.previous = match keep_previous(&plan.binary) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!("No rollback copy of this release: {error:#}");
                None
            }
        };
        let path = self.instance.layout.update_plan();
        save_plan(&path, plan)?;
        // The watchdog runs the release known to work: this one.
        let watchdog = plan.previous.clone().unwrap_or_else(|| plan.binary.clone());
        let mut command = std::process::Command::new("systemd-run");
        command.args([
            "--user",
            "--quiet",
            "--collect",
            &format!("--unit=scv-update-{}", plan.id),
        ]);
        // The watchdog selects the same instance and configuration.
        let layout = &self.instance.layout;
        let home = (!layout.is_default()).then(|| layout.home());
        let config = self.instance.overrides.config_file.as_deref();
        for (variable, value) in [("SCV_HOME", home), ("SCV_CONFIG", config)] {
            if let Some(value) = value {
                let mut setting = std::ffi::OsString::from(format!("--setenv={variable}="));
                setting.push(value);
                command.arg(setting);
            }
        }
        command
            .arg(watchdog)
            .arg("restart-watchdog")
            .arg("--plan")
            .arg(&path)
            .stdin(std::process::Stdio::null());
        let status = tokio::task::spawn_blocking(move || command.status())
            .await?
            .context("run systemd-run")?;
        if !status.success() {
            bail!("systemd-run exited with {status}");
        }
        tracing::info!(
            "Handed the restart into v{} to unit scv-update-{}",
            plan.to_version,
            plan.id
        );
        Ok(())
    }
}

/// Copy the running executable (still readable through `/proc/self/exe`
/// after it was replaced on disk) next to `binary` as `<binary>.prev`.
fn keep_previous(binary: &Path) -> Result<PathBuf> {
    let previous = binary.with_file_name(format!(
        "{}.prev",
        binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("scv")
    ));
    install_copy(Path::new("/proc/self/exe"), &previous)?;
    Ok(previous)
}

/// Copy `source` to `target` through a temporary file beside it, executable.
fn install_copy(source: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", target.display()))?;
    let temporary = tempfile::Builder::new()
        .prefix(".scv-install")
        .tempfile_in(parent)?;
    std::fs::copy(source, temporary.path())
        .with_context(|| format!("copy {} to {}", source.display(), target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    temporary
        .persist(target)
        .map_err(|error| error.error)
        .with_context(|| format!("install {}", target.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The watchdog, run by `scv restart-watchdog` outside the daemon.

/// Restart the unit, check the new release, and roll back when it fails and
/// the releases share a config layout. Records the outcome in the plan.
pub async fn watchdog(layout: &Layout, plan_path: &Path) -> Result<()> {
    let mut plan = load_plan(plan_path)?.context("no restart plan")?;
    if plan.state != PlanState::Restarting {
        bail!("the restart plan is {:?}, not restarting", plan.state);
    }
    let socket = layout.socket();
    eprintln!("Restarting {} into v{}", plan.unit, plan.to_version);
    systemctl_restart(&plan.unit);
    let outcome = verify(
        &socket,
        &plan.to_version,
        &plan.expected,
        plan.verify_seconds,
    )
    .await;
    match outcome {
        Ok(()) => {
            eprintln!("v{} is up with its channels", plan.to_version);
            plan.state = PlanState::Verified;
        }
        Err(reason) => {
            eprintln!("v{} failed: {reason}", plan.to_version);
            match rollback_refusal(&plan) {
                None => {
                    let previous = plan.previous.clone().expect("checked by rollback_refusal");
                    let detail = match install_copy(&previous, &plan.binary) {
                        Ok(()) => {
                            systemctl_restart(&plan.unit);
                            let seconds = plan.verify_seconds.min(ROLLBACK_SECONDS);
                            match verify(&socket, &plan.from_version, &[], seconds).await {
                                Ok(()) => reason,
                                Err(again) => format!(
                                    "{reason}; after the rollback v{} did not come back either ({again})",
                                    plan.from_version
                                ),
                            }
                        }
                        Err(error) => format!(
                            "{reason}; putting v{} back failed: {error:#}",
                            plan.from_version
                        ),
                    };
                    plan.state = PlanState::RolledBack;
                    plan.detail = Some(detail);
                }
                Some(refusal) => {
                    plan.state = PlanState::Failed;
                    plan.detail = Some(format!("{reason}; not rolled back: {refusal}"));
                }
            }
        }
    }
    save_plan(plan_path, &plan)?;
    Ok(())
}

fn systemctl_restart(unit: &str) {
    match std::process::Command::new("systemctl")
        .args(["--user", "restart", unit])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("systemctl --user restart {unit} exited with {status}"),
        Err(error) => eprintln!("could not run systemctl: {error}"),
    }
}

/// Why the previous binary may not be put back, or `None` when it may.
fn rollback_refusal(plan: &Plan) -> Option<String> {
    if plan.to_layout != plan.from_layout {
        return Some(format!(
            "v{} uses config layout {} and v{} uses {}, so the older binary cannot read the \
             current configuration",
            plan.to_version, plan.to_layout, plan.from_version, plan.from_layout
        ));
    }
    match &plan.previous {
        Some(previous) if previous.is_file() => None,
        _ => Some(format!("no copy of v{} was kept", plan.from_version)),
    }
}

/// Wait until the daemon reports `version` and every `expected` account is
/// connected, or explain what was missing when `seconds` run out.
async fn verify(
    socket: &Path,
    version: &str,
    expected: &[String],
    seconds: u64,
) -> std::result::Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut last = format!("v{version} did not start");
    loop {
        match scv_client::control(socket, DaemonCommand::Status).await {
            Ok(status) if status.version == version => {
                let missing: Vec<_> = expected
                    .iter()
                    .filter(|id| {
                        !status.components.iter().any(|health| {
                            &health.id == *id && health.state == ComponentState::Connected
                        })
                    })
                    .map(String::as_str)
                    .collect();
                if missing.is_empty() {
                    return Ok(());
                }
                last = format!(
                    "v{version} started, but {} did not reconnect",
                    missing.join(" and ")
                );
            }
            Ok(status) => last = format!("SCV still reports v{}", status.version),
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(last);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// Startup: explain the previous run, then announce.

/// What the daemon found at startup about how its predecessor ended.
pub(crate) struct Startup {
    plan: Option<Plan>,
    /// The previous daemon stopped without shutting down: its version and
    /// start time.
    unclean: Option<(String, u64)>,
}

#[derive(Serialize, Deserialize)]
struct Marker {
    pid: u32,
    version: String,
    started_unix: u64,
}

/// Read the restart plan and the running marker, tell the hub whether this
/// start is a planned restart (before any bridge recovers), and mark this
/// daemon running until [`clean_shutdown`].
pub(crate) fn startup(layout: &Layout, hub: &Hub) -> Startup {
    let plan = load_plan(&layout.update_plan()).unwrap_or_else(|error| {
        tracing::warn!("Ignoring an unreadable restart plan: {error:#}");
        let _ = std::fs::remove_file(layout.update_plan());
        None
    });
    let planned = plan.as_ref().filter(|plan| {
        plan.state != PlanState::Waiting
            && plan
                .restart_unix
                .is_some_and(|at| unix_now().saturating_sub(at) < RESTART_CONTEXT_MAX_AGE)
    });
    hub.set_restart(planned.map(|plan| Restart {
        to_version: plan.to_version.clone(),
    }));
    let marker = layout.daemon_marker();
    let unclean = std::fs::read(&marker)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
        .filter(|previous| previous.pid != std::process::id())
        .map(|previous| (previous.version, previous.started_unix));
    let current = Marker {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        started_unix: unix_now(),
    };
    if let Err(error) = serde_json::to_vec(&current)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| write_private(&marker, &bytes))
    {
        tracing::warn!("Could not record the running daemon: {error:#}");
    }
    Startup { plan, unclean }
}

/// The daemon stopped on request: the next one will not report a crash.
pub(crate) fn clean_shutdown(layout: &Layout) {
    let _ = std::fs::remove_file(layout.daemon_marker());
}

/// What the next daemon should say about a plan, given its own version.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Say(String),
    /// The watchdog is still deciding.
    Wait,
    Drop,
}

fn decide(plan: &Plan, own: &str, watchdog_overdue: bool) -> Decision {
    match plan.state {
        PlanState::Waiting => Decision::Say(if own == plan.to_version {
            format!(
                "SCV is now running {}. It stopped before the planned restart, so work that \
                 was running then was stopped.",
                plan.label()
            )
        } else {
            format!(
                "SCV stopped before it could restart into v{}; it is running v{own}. Deploy \
                 again to finish the update.",
                plan.to_version
            )
        }),
        PlanState::Restarting if !watchdog_overdue => Decision::Wait,
        PlanState::Restarting if own == plan.to_version => Decision::Say(format!(
            "SCV is now running {}; the update watchdog did not report back.",
            plan.label()
        )),
        PlanState::Restarting if own == plan.from_version => Decision::Say(format!(
            "The update to v{} did not take effect; SCV is still running v{own}.",
            plan.to_version
        )),
        PlanState::Restarting => Decision::Drop,
        PlanState::Verified | PlanState::RolledBack | PlanState::Failed => {
            Decision::Say(announcement(plan, own))
        }
    }
}

/// The announcement of a finished plan.
fn announcement(plan: &Plan, own: &str) -> String {
    let detail = plan.detail.as_deref().unwrap_or("it did not come up");
    let mut text = match plan.state {
        PlanState::Verified => format!("SCV updated: now running {}.", plan.label()),
        PlanState::RolledBack => format!(
            "The update to v{} failed: {detail}. SCV rolled back to v{}.",
            plan.to_version, plan.from_version
        ),
        PlanState::Failed if own == plan.to_version => {
            format!("SCV is running {}, but {detail}.", plan.label())
        }
        _ => format!("The update to v{} failed: {detail}.", plan.to_version),
    };
    if plan.waited_out {
        let minutes = plan
            .deadline_unix
            .saturating_sub(plan.requested_unix)
            .div_ceil(60);
        text.push_str(&format!(
            " It waited {minutes} minutes for running work, then restarted anyway; work \
             still running then was stopped."
        ));
    }
    text
}

/// Announce how the previous run ended, once the accounts can take it.
pub(crate) async fn announce(
    layout: Layout,
    startup: Startup,
    notifier: Notifier,
    cancel: CancellationToken,
) {
    let own = env!("CARGO_PKG_VERSION");
    if let Some(mut plan) = startup.plan {
        let path = layout.update_plan();
        let overdue_at = plan.restart_unix.unwrap_or(plan.requested_unix)
            + plan.verify_seconds
            + ROLLBACK_SECONDS
            + 60;
        let text = loop {
            match decide(&plan, own, unix_now() >= overdue_at) {
                Decision::Say(text) => break Some(text),
                Decision::Drop => break None,
                Decision::Wait => {}
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            match load_plan(&path) {
                Ok(Some(reloaded)) if reloaded.id == plan.id => plan = reloaded,
                _ => break None,
            }
        };
        if let Some(text) = text {
            tracing::info!("{text}");
            notifier
                .deliver(plan.origin.as_ref(), &text, None, &cancel)
                .await;
        }
        if !cancel.is_cancelled() {
            let _ = std::fs::remove_file(&path);
        }
    } else if let Some((version, started)) = startup.unclean {
        let text = format!(
            "SCV started again after an unexpected stop (a crash or a host restart); it had run \
             v{version} since {}. Work in progress then was stopped.",
            format_time(started)
        );
        tracing::warn!("{text}");
        notifier.deliver(None, &text, None, &cancel).await;
    }
}

fn format_time(unix: u64) -> String {
    let age = unix_now().saturating_sub(unix);
    match age {
        0..=119 => "moments before".into(),
        120..=7199 => format!("{} minutes before", age / 60),
        7200..=172_799 => format!("{} hours before", age / 3600),
        _ => format!("{} days before", age / 86_400),
    }
}

// ---------------------------------------------------------------------------
// Accounts that stay disconnected.

/// Tell the owner, through another account, when an enabled account stays
/// disconnected for [`DOWN_NOTICE_AFTER`]; once per outage.
pub(crate) async fn monitor(notifier: Notifier, cancel: CancellationToken) {
    let mut down: HashMap<String, (tokio::time::Instant, bool)> = HashMap::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(MONITOR_INTERVAL) => {}
        }
        let states = notifier.states.get().await;
        down.retain(|id, _| {
            states
                .get(id)
                .is_some_and(|state| *state != ComponentState::Connected)
        });
        for (id, state) in &states {
            if *state == ComponentState::Connected {
                continue;
            }
            let (since, told) = down
                .entry(id.clone())
                .or_insert((tokio::time::Instant::now(), false));
            if *told || since.elapsed() < DOWN_NOTICE_AFTER {
                continue;
            }
            *told = true;
            let (channel, account) = id.split_once(':').unwrap_or((id, "default"));
            let text = format!(
                "SCV's {} account {account} has been disconnected for {} minutes; its sign-in \
                 may have expired. On the host, check `scv channels status {channel}` and sign \
                 in again with `scv channels login {channel}` if needed.",
                channel_title(id),
                since.elapsed().as_secs() / 60
            );
            tracing::warn!("{text}");
            let notifier = notifier.clone();
            let cancel = cancel.clone();
            let id = id.clone();
            tokio::spawn(async move { notifier.deliver(None, &text, Some(&id), &cancel).await });
        }
    }
}

#[cfg(test)]
mod tests;
