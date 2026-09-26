//! Server-owned lifecycle for every long-running integration.

use anyhow::{Result, bail};
use async_trait::async_trait;
use scv_channels::state::{self, AccountSettings};
use scv_channels::{Accounts, ChannelCredentials, ChannelKind};
use scv_protocol::{
    ComponentHealth, ComponentState, DaemonCommand, DaemonStatus, RemoteTools, Senders,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Instance;

const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long an operator command waits out a bridge's state commit.
const BUSY_RETRY: Duration = Duration::from_secs(5);

/// Components must observe cancellation and must not detach child tasks.
/// Return on failure; the supervisor owns retries and bounded shutdown.
#[async_trait]
pub trait Component: Send + Sync + 'static {
    async fn run(&self, cancellation: CancellationToken, health: HealthReporter) -> Result<()>;
}

#[derive(Clone)]
pub struct HealthReporter(Arc<Mutex<ComponentHealth>>);

impl HealthReporter {
    pub fn contact(&self, connected: bool) {
        let mut health = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            health.state,
            ComponentState::Stopping | ComponentState::Stopped
        ) {
            return;
        }
        health.state = if connected {
            ComponentState::Connected
        } else {
            ComponentState::Disconnected
        };
        health.error = (!connected).then(|| "Component contact failed".into());
        if connected {
            health.last_success_unix_seconds = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            );
        }
    }

    fn transition(&self, state: ComponentState, error: Option<&str>) {
        let mut health = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        health.state = state;
        health.error = error.map(str::to_owned);
    }

    fn snapshot(&self) -> ComponentHealth {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub struct Supervisor {
    tasks: BTreeMap<String, RunningComponent>,
    grace: Duration,
    initial_backoff: Duration,
}

struct RunningComponent {
    cancellation: CancellationToken,
    task: JoinHandle<()>,
    health: HealthReporter,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self {
            tasks: BTreeMap::new(),
            grace: STOP_GRACE,
            initial_backoff: Duration::from_secs(1),
        }
    }
}

impl Supervisor {
    /// Idempotent start: replacement must first stop and join the old instance.
    pub fn start(&mut self, component: Arc<dyn Component>, health: ComponentHealth) {
        if self.tasks.contains_key(&health.id) {
            return;
        }
        let id = health.id.clone();
        let health = HealthReporter(Arc::new(Mutex::new(health)));
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let report = health.clone();
        let initial_backoff = self.initial_backoff;
        let grace = self.grace;
        let task = tokio::spawn(async move {
            let mut delay = initial_backoff;
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                report.transition(ComponentState::Starting, None);
                let started = Instant::now();
                // Catch task panics without letting them take down the daemon or skip retries.
                let instance = component.clone();
                let child_cancel = cancel.clone();
                let child_report = report.clone();
                let mut child =
                    tokio::spawn(async move { instance.run(child_cancel, child_report).await });
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        report.transition(ComponentState::Stopping, None);
                        if tokio::time::timeout(grace, &mut child).await.is_err() {
                            child.abort();
                            let _ = child.await;
                        }
                        break;
                    }
                    _ = &mut child => {}
                }
                report.transition(
                    ComponentState::Backoff,
                    Some("Component stopped unexpectedly; retrying"),
                );
                report
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .restarts += 1;
                if started.elapsed() >= Duration::from_secs(60) {
                    delay = initial_backoff;
                }
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(60));
            }
            report.transition(ComponentState::Stopped, None);
        });
        self.tasks.insert(
            id,
            RunningComponent {
                cancellation,
                task,
                health,
            },
        );
    }

    pub fn health(&self) -> Vec<ComponentHealth> {
        self.tasks
            .values()
            .map(|task| task.health.snapshot())
            .collect()
    }

    pub async fn stop(&mut self, id: &str) {
        if let Some(running) = self.tasks.get_mut(id) {
            running.cancellation.cancel();
            // The runner owns abort/join of its child, so never abort the runner first.
            let _ = (&mut running.task).await;
        }
        self.tasks.remove(id);
    }

    pub async fn shutdown(&mut self) {
        for task in self.tasks.values() {
            task.cancellation.cancel();
        }
        for id in self.tasks.keys().cloned().collect::<Vec<_>>() {
            self.stop(&id).await;
        }
    }
}

/// Account discovery from saved credentials.
fn account_names(accounts: &Accounts) -> std::result::Result<Vec<String>, &'static str> {
    const DISCOVERY: &str =
        "Account discovery failed; components stopped until configuration is readable";
    accounts.names().map_err(|_| DISCOVERY)
}

struct ChannelAccount {
    kind: ChannelKind,
    account: String,
    credentials: ChannelCredentials,
    settings: AccountSettings,
    /// The daemon's instance, whose socket the account's sessions use.
    instance: Instance,
    workspace: PathBuf,
    /// The account owner holds remote tools.
    tools: bool,
    link: scv_channels::hub::Link,
}

#[async_trait]
impl Component for ChannelAccount {
    async fn run(&self, cancellation: CancellationToken, health: HealthReporter) -> Result<()> {
        let tool_turn_timeout = self.tools.then(|| {
            let turn_timeout =
                scv_channels::owner_turn_timeout(max_tool_timeout(&self.instance, &self.workspace));
            tracing::info!(
                "{} {} owner turns may run up to {} seconds",
                self.kind.title(),
                self.account,
                turn_timeout.as_secs()
            );
            turn_timeout
        });
        let socket = self.instance.layout.socket();
        let report = move |connected| health.contact(connected);
        let run = scv_channels::AccountRun {
            layout: &self.instance.layout,
            account: &self.account,
            credentials: &self.credentials,
            settings: &self.settings,
            owner: self.credentials.owner().filter(|owner| !owner.is_empty()),
            tool_turn_timeout,
            workspace: &self.workspace,
            socket: &socket,
            link: &self.link,
            health: &report,
        };
        // Cancellation drops the run's I/O and sessions; it spawns no tasks.
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Ok(()),
            result = scv_channels::run(run) => result,
        }
    }
}

pub(crate) struct Components {
    supervisor: Supervisor,
    /// Running or disabled accounts by component ID, `<channel>:<account>`.
    desired: BTreeMap<String, (ChannelCredentials, AccountSettings)>,
    inactive: BTreeMap<String, ComponentHealth>,
    /// The daemon's instance: where accounts are saved and the socket their
    /// sessions use.
    instance: Instance,
    workspace: PathBuf,
    /// What the daemon shares with the channel bridges it runs.
    hub: Arc<scv_channels::hub::Hub>,
    /// Plans the daemon's own restarts; set only in the socket daemon.
    restarter: Option<Arc<crate::restart::Restarter>>,
    /// Asks the owner yes/no questions; set only in the socket daemon.
    confirmer: Option<Arc<crate::confirm::Confirmer>>,
}

impl Components {
    #[cfg(test)]
    pub fn new(instance: Instance, workspace: PathBuf) -> Self {
        Self::with_hub(instance, workspace, scv_channels::hub::Hub::new(None))
    }

    pub fn with_hub(
        instance: Instance,
        workspace: PathBuf,
        hub: Arc<scv_channels::hub::Hub>,
    ) -> Self {
        Self {
            supervisor: Supervisor::default(),
            desired: BTreeMap::new(),
            inactive: BTreeMap::new(),
            instance,
            workspace,
            hub,
            restarter: None,
            confirmer: None,
        }
    }

    /// One channel's saved accounts in the daemon's instance.
    fn accounts(&self, kind: ChannelKind) -> Accounts {
        kind.accounts(&self.instance.layout)
    }

    pub(crate) fn set_restarter(&mut self, restarter: Arc<crate::restart::Restarter>) {
        self.restarter = Some(restarter);
    }

    pub(crate) fn restarter(&self) -> Option<Arc<crate::restart::Restarter>> {
        self.restarter.clone()
    }

    pub(crate) fn set_confirmer(&mut self, confirmer: Arc<crate::confirm::Confirmer>) {
        self.confirmer = Some(confirmer);
    }

    pub(crate) fn confirmer(&self) -> Option<Arc<crate::confirm::Confirmer>> {
        self.confirmer.clone()
    }

    pub fn status(&self) -> DaemonStatus {
        let mut components = self.supervisor.health();
        components.extend(self.inactive.values().cloned());
        components.sort_by(|a, b| a.id.cmp(&b.id));
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            components,
            delegations: scv_protocol::DelegationSummary::default(),
            restart: None,
            confirm: None,
        }
    }

    /// Match running components to every channel's saved accounts. A channel
    /// whose accounts cannot be discovered stops its own components and
    /// reports why; other channels keep running.
    pub async fn reconcile(&mut self) -> Result<()> {
        let mut failed = false;
        for &channel in ChannelKind::ALL {
            match account_names(&self.accounts(channel)) {
                Ok(names) => self.reconcile_channel(channel, names).await,
                Err(message) => {
                    failed = true;
                    for id in self.ids(channel) {
                        self.supervisor.stop(&id).await;
                        self.desired.remove(&id);
                        self.inactive.remove(&id);
                    }
                    let mut health = initial_health(channel, "discovery", None, false);
                    health.id = component_id(channel, "discovery-error");
                    health.state = ComponentState::Failed;
                    health.error = Some(message.into());
                    self.inactive.insert(health.id.clone(), health);
                }
            }
        }
        if failed {
            bail!("Account discovery failed");
        }
        Ok(())
    }

    /// Component IDs of one channel, running or not.
    fn ids(&self, channel: ChannelKind) -> Vec<String> {
        let prefix = format!("{}:", channel.name());
        self.desired
            .keys()
            .chain(self.inactive.keys())
            .filter(|id| id.starts_with(&prefix))
            .cloned()
            .collect()
    }

    async fn reconcile_channel(&mut self, channel: ChannelKind, names: Vec<String>) {
        let wanted: Vec<String> = names
            .iter()
            .map(|name| component_id(channel, name))
            .collect();
        for id in self.ids(channel) {
            if !wanted.contains(&id) {
                self.supervisor.stop(&id).await;
                self.desired.remove(&id);
                self.inactive.remove(&id);
            }
        }
        for name in names {
            let id = component_id(channel, &name);
            let loaded = (|| -> Result<_> {
                let (account, settings) = self.accounts(channel).snapshot(&name)?;
                Ok((
                    account.ok_or_else(|| anyhow::anyhow!("missing account"))?,
                    settings,
                ))
            })();
            let (credentials, settings) = match loaded {
                Ok(value) => value,
                Err(error) => {
                    self.account_error(channel, &name, error).await;
                    continue;
                }
            };
            if self.desired.get(&id) == Some(&(credentials.clone(), settings.clone())) {
                continue;
            }
            self.supervisor.stop(&id).await;
            self.inactive.remove(&id);
            let mut health = initial_health(channel, &name, Some(&credentials), settings.enabled);
            let tools = tool_owner(&credentials, &settings).is_some();
            if tools {
                health.remote_tools = RemoteTools::Owner;
            }
            health.senders = Some(settings.senders);
            if settings.enabled {
                let workspace = settings
                    .workspace
                    .clone()
                    .unwrap_or_else(|| self.workspace.clone());
                if !workspace.is_absolute() || !workspace.is_dir() {
                    health.state = ComponentState::Failed;
                    health.error =
                        Some("Component workspace must be an existing absolute directory".into());
                    self.inactive.insert(id.clone(), health);
                    self.desired.remove(&id);
                    continue;
                }
                let link = scv_channels::hub::Link::new(
                    Arc::clone(&self.hub),
                    id.clone(),
                    credentials.owner().map(str::to_owned),
                );
                self.supervisor.start(
                    Arc::new(ChannelAccount {
                        kind: channel,
                        account: name.clone(),
                        credentials: credentials.clone(),
                        settings: settings.clone(),
                        instance: self.instance.clone(),
                        workspace,
                        tools,
                        link,
                    }),
                    health,
                );
            } else {
                health.state = ComponentState::Disabled;
                self.inactive.insert(id.clone(), health);
            }
            self.desired.insert(id, (credentials, settings));
        }
    }

    async fn account_error(&mut self, channel: ChannelKind, name: &str, error: anyhow::Error) {
        // A bridge state commit briefly holds this same lock. Retry next refresh
        // rather than interrupting healthy work for ordinary lock contention.
        if is_busy(&error) {
            return;
        }
        let id = component_id(channel, name);
        self.supervisor.stop(&id).await;
        self.desired.remove(&id);
        let mut health = initial_health(channel, name, None, true);
        health.state = ComponentState::Failed;
        health.error = Some(
            "Invalid or inaccessible account or settings; `scv config show` says which".into(),
        );
        self.inactive.insert(id, health);
    }

    pub async fn control(&mut self, command: DaemonCommand) -> Result<DaemonStatus> {
        match command {
            // Delegations, restarts, and questions belong to the connection
            // handler, which adds them.
            DaemonCommand::Status
            | DaemonCommand::Delegations { .. }
            | DaemonCommand::DelegationKill { .. }
            | DaemonCommand::RestartWhenIdle { .. }
            | DaemonCommand::ConfirmAsk { .. }
            | DaemonCommand::ConfirmStatus { .. } => return Ok(self.status()),
            DaemonCommand::Reload => {}
            DaemonCommand::ChannelSet {
                channel,
                account,
                enabled,
                workspace,
                remote_tools,
                senders,
            } => {
                let channel = ChannelKind::parse(&channel)?;
                state::validate_name(&account)?;
                let workspace = match workspace {
                    Some(path) => {
                        let path = PathBuf::from(path);
                        if !path.is_absolute() || !path.is_dir() {
                            bail!("Invalid component workspace");
                        }
                        Some(std::fs::canonicalize(path)?)
                    }
                    None => None,
                };
                let accounts = self.accounts(channel);
                retry_while_busy(|| {
                    if !accounts.signed_in(&account)? {
                        bail!("Account is not logged in");
                    }
                    let mut settings = accounts.settings(&account)?;
                    settings.enabled = enabled;
                    if let Some(path) = &workspace {
                        settings.workspace = Some(path.clone());
                    }
                    if let Some(mode) = remote_tools {
                        settings.remote_tools = mode;
                    }
                    if let Some(senders) = senders {
                        settings.senders = senders;
                    }
                    accounts.save_settings(&account, &settings)
                })
                .await?;
            }
            DaemonCommand::ChannelLogout { channel, account } => {
                let channel = ChannelKind::parse(&channel)?;
                state::validate_name(&account)?;
                // Persist disabled, tool-free, and owner-only first, so failed
                // deletion can neither resurrect a live account nor hand a
                // later login the grant or other senders.
                let accounts = self.accounts(channel);
                retry_while_busy(|| {
                    let mut settings = accounts.settings(&account)?;
                    settings.enabled = false;
                    settings.remote_tools = RemoteTools::None;
                    settings.senders = Senders::Owner;
                    accounts.save_settings(&account, &settings)
                })
                .await?;
                let id = component_id(channel, &account);
                self.supervisor.stop(&id).await;
                self.desired.remove(&id);
                self.inactive.remove(&id);
                retry_while_busy(|| accounts.remove(&account)).await?;
            }
        }
        self.reconcile().await?;
        Ok(self.status())
    }

    pub async fn shutdown(&mut self) {
        self.supervisor.shutdown().await;
    }
}

/// The longest tool call an owner session in `workspace` may make, from the
/// configuration its sessions load. Read at each (re)start of the component.
fn max_tool_timeout(instance: &Instance, workspace: &std::path::Path) -> std::time::Duration {
    let seconds = instance.load(workspace).map_or_else(
        |error| {
            tracing::warn!("Channel owner turns use the default tool timeout ceiling: {error:#}");
            crate::config::ToolConfig::default().max_timeout_seconds
        },
        |config| config.tools.max_timeout_seconds,
    );
    std::time::Duration::from_secs(seconds)
}

/// Account transactions fail fast while their lock is held, and a running
/// bridge holds it for every state commit. Operator commands retry through
/// that contention instead of failing whenever they coincide with a commit.
async fn retry_while_busy<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    let deadline = tokio::time::Instant::now() + BUSY_RETRY;
    loop {
        match operation() {
            Err(error) if is_busy(&error) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => return result,
        }
    }
}

fn is_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
}

/// Only the authenticated account owner may receive tools. Credentials without
/// a known owner ID grant tools to nobody, even when the setting asks for it.
fn tool_owner(credentials: &ChannelCredentials, settings: &AccountSettings) -> Option<String> {
    (settings.remote_tools == RemoteTools::Owner)
        .then(|| credentials.owner().map(str::to_owned))
        .flatten()
        .filter(|owner| !owner.is_empty())
}

fn component_id(channel: ChannelKind, account: &str) -> String {
    format!("{}:{account}", channel.name())
}

fn initial_health(
    channel: ChannelKind,
    account: &str,
    credentials: Option<&ChannelCredentials>,
    enabled: bool,
) -> ComponentHealth {
    ComponentHealth {
        id: component_id(channel, account),
        channel: channel.name().into(),
        account: account.into(),
        bot_id: credentials.and_then(ChannelCredentials::bot_id),
        user_id: credentials.and_then(|c| c.owner().map(str::to_owned)),
        enabled,
        state: ComponentState::Starting,
        last_success_unix_seconds: None,
        error: None,
        restarts: 0,
        remote_tools: RemoteTools::None,
        senders: None,
    }
}

#[cfg(test)]
mod tests;
