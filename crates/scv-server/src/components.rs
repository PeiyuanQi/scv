//! Server-owned lifecycle for every long-running integration.

use anyhow::{Result, bail};
use async_trait::async_trait;
use scv_channels::state::{self, AccountSettings};
use scv_protocol::{ComponentHealth, ComponentState, DaemonCommand, DaemonStatus, RemoteTools};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
        let mut health = self.0.lock().unwrap();
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
        let mut health = self.0.lock().unwrap();
        health.state = state;
        health.error = error.map(str::to_owned);
    }

    fn snapshot(&self) -> ComponentHealth {
        self.0.lock().unwrap().clone()
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
                    _ = cancel.cancelled() => {
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
                report.0.lock().unwrap().restarts += 1;
                if started.elapsed() >= Duration::from_secs(60) {
                    delay = initial_backoff;
                }
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
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

/// A chat channel whose accounts the daemon supervises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Channel {
    Wechat,
    Feishu,
}

const CHANNELS: [Channel; 2] = [Channel::Wechat, Channel::Feishu];

/// A channel account's saved credentials.
#[derive(Clone, PartialEq)]
enum Credentials {
    Wechat(scv_clawbot::state::Account),
    Feishu(scv_feishu::state::Account),
}

impl Channel {
    fn parse(name: &str) -> Result<Self> {
        CHANNELS
            .into_iter()
            .find(|channel| channel.name() == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown channel {name:?}"))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Wechat => scv_clawbot::CHANNEL,
            Self::Feishu => scv_feishu::CHANNEL,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Wechat => "WeChat",
            Self::Feishu => "Feishu",
        }
    }

    /// Account discovery. WeChat state saved before channels moves first;
    /// the user resolves a failed move by hand, and `scv channels status`
    /// names the files.
    fn account_names(self) -> std::result::Result<Vec<String>, &'static str> {
        const DISCOVERY: &str =
            "Account discovery failed; components stopped until configuration is readable";
        match self {
            Self::Wechat => scv_clawbot::state::migrate()
                .map_err(|_| {
                    "Saved WeChat state could not move to channels/wechat; run `scv channels status` for details"
                })
                .and_then(|_| scv_clawbot::state::account_names().map_err(|_| DISCOVERY)),
            Self::Feishu => scv_feishu::state::account_names().map_err(|_| DISCOVERY),
        }
    }

    fn snapshot(self, account: &str) -> Result<(Option<Credentials>, AccountSettings)> {
        Ok(match self {
            Self::Wechat => {
                let (credentials, settings) = scv_clawbot::state::account_snapshot(account)?;
                (credentials.map(Credentials::Wechat), settings)
            }
            Self::Feishu => {
                let (credentials, settings) = scv_feishu::state::account_snapshot(account)?;
                (credentials.map(Credentials::Feishu), settings)
            }
        })
    }

    fn signed_in(self, account: &str) -> Result<bool> {
        Ok(match self {
            Self::Wechat => scv_clawbot::state::account(account)?.is_some(),
            Self::Feishu => scv_feishu::state::account(account)?.is_some(),
        })
    }

    fn settings(self, account: &str) -> Result<AccountSettings> {
        match self {
            Self::Wechat => scv_clawbot::state::settings(account),
            Self::Feishu => scv_feishu::state::settings(account),
        }
    }

    fn save_settings(self, account: &str, settings: &AccountSettings) -> Result<()> {
        match self {
            Self::Wechat => scv_clawbot::state::save_settings(account, settings),
            Self::Feishu => scv_feishu::state::save_settings(account, settings),
        }
    }

    fn remove(self, account: &str) -> Result<()> {
        match self {
            Self::Wechat => scv_clawbot::state::remove(account),
            Self::Feishu => scv_feishu::state::remove(account),
        }
    }
}

impl Credentials {
    /// The authenticated owner, the only sender remote tools may reach.
    fn owner(&self) -> Option<&str> {
        match self {
            Self::Wechat(account) => account.user_id.as_deref(),
            Self::Feishu(account) => account.owner_open_id.as_deref(),
        }
    }

    /// The bot's identity shown in status: the iLink bot or the Feishu app.
    fn bot_id(&self) -> Option<String> {
        match self {
            Self::Wechat(account) => account.bot_id.clone(),
            Self::Feishu(account) => Some(account.app_id.clone()),
        }
    }
}

struct ChannelAccount {
    channel: Channel,
    account: String,
    credentials: Credentials,
    workspace: PathBuf,
    socket: PathBuf,
    tool_owner: Option<String>,
}

#[async_trait]
impl Component for ChannelAccount {
    async fn run(&self, cancellation: CancellationToken, health: HealthReporter) -> Result<()> {
        let tool_owner = self.tool_owner.clone().map(|user_id| {
            let turn_timeout = scv_channels::owner_turn_timeout(max_tool_timeout(&self.workspace));
            tracing::info!(
                "{} {} owner turns may run up to {} seconds",
                self.channel.title(),
                self.account,
                turn_timeout.as_secs()
            );
            scv_channels::ToolOwner {
                user_id,
                turn_timeout,
            }
        });
        let report = Arc::new(move |connected| health.contact(connected));
        match &self.credentials {
            Credentials::Wechat(credentials) => {
                scv_clawbot::run_supervised(
                    &credentials.token,
                    &credentials.base_url,
                    &self.account,
                    &self.workspace,
                    &self.socket,
                    tool_owner.as_ref(),
                    cancellation,
                    report,
                )
                .await
            }
            Credentials::Feishu(credentials) => {
                scv_feishu::run_supervised(
                    credentials,
                    &self.account,
                    &self.workspace,
                    &self.socket,
                    tool_owner.as_ref(),
                    cancellation,
                    report,
                )
                .await
            }
        }
    }
}

pub(crate) struct Components {
    supervisor: Supervisor,
    /// Running or disabled accounts by component ID, `<channel>:<account>`.
    desired: BTreeMap<String, (Credentials, AccountSettings)>,
    inactive: BTreeMap<String, ComponentHealth>,
    socket: PathBuf,
    workspace: PathBuf,
}

impl Components {
    pub fn new(socket: PathBuf, workspace: PathBuf) -> Self {
        Self {
            supervisor: Supervisor::default(),
            desired: BTreeMap::new(),
            inactive: BTreeMap::new(),
            socket,
            workspace,
        }
    }

    pub fn status(&self) -> DaemonStatus {
        let mut components = self.supervisor.health();
        components.extend(self.inactive.values().cloned());
        components.sort_by(|a, b| a.id.cmp(&b.id));
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            components,
            delegations: Default::default(),
        }
    }

    /// Match running components to every channel's saved accounts. A channel
    /// whose accounts cannot be discovered stops its own components and
    /// reports why; other channels keep running.
    pub async fn reconcile(&mut self) -> Result<()> {
        let mut failed = false;
        for channel in CHANNELS {
            match channel.account_names() {
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
    fn ids(&self, channel: Channel) -> Vec<String> {
        let prefix = format!("{}:", channel.name());
        self.desired
            .keys()
            .chain(self.inactive.keys())
            .filter(|id| id.starts_with(&prefix))
            .cloned()
            .collect()
    }

    async fn reconcile_channel(&mut self, channel: Channel, names: Vec<String>) {
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
                let (account, settings) = channel.snapshot(&name)?;
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
            let tool_owner = tool_owner(&credentials, &settings);
            if tool_owner.is_some() {
                health.remote_tools = RemoteTools::Owner;
            }
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
                self.supervisor.start(
                    Arc::new(ChannelAccount {
                        channel,
                        account: name.clone(),
                        credentials: credentials.clone(),
                        workspace,
                        socket: self.socket.clone(),
                        tool_owner,
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

    async fn account_error(&mut self, channel: Channel, name: &str, error: anyhow::Error) {
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
        health.error = Some("Invalid or inaccessible account/settings".into());
        self.inactive.insert(id, health);
    }

    pub async fn control(&mut self, command: DaemonCommand) -> Result<DaemonStatus> {
        match command {
            // Delegations belong to the connection handler, which adds them.
            DaemonCommand::Status
            | DaemonCommand::Delegations { .. }
            | DaemonCommand::DelegationKill { .. } => return Ok(self.status()),
            DaemonCommand::Reload => {}
            DaemonCommand::ChannelSet {
                channel,
                account,
                enabled,
                workspace,
                remote_tools,
            } => {
                let channel = Channel::parse(&channel)?;
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
                retry_while_busy(|| {
                    if !channel.signed_in(&account)? {
                        bail!("Account is not logged in");
                    }
                    let mut settings = channel.settings(&account)?;
                    settings.enabled = enabled;
                    if let Some(path) = &workspace {
                        settings.workspace = Some(path.clone());
                    }
                    if let Some(mode) = remote_tools {
                        settings.remote_tools = mode;
                    }
                    channel.save_settings(&account, &settings)
                })
                .await?;
            }
            DaemonCommand::ChannelLogout { channel, account } => {
                let channel = Channel::parse(&channel)?;
                state::validate_name(&account)?;
                // Persist disabled and tool-free first, so failed deletion can
                // neither resurrect a live account nor hand a later login the grant.
                retry_while_busy(|| {
                    let mut settings = channel.settings(&account)?;
                    settings.enabled = false;
                    settings.remote_tools = RemoteTools::None;
                    channel.save_settings(&account, &settings)
                })
                .await?;
                let id = component_id(channel, &account);
                self.supervisor.stop(&id).await;
                self.desired.remove(&id);
                self.inactive.remove(&id);
                retry_while_busy(|| channel.remove(&account)).await?;
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
fn max_tool_timeout(workspace: &std::path::Path) -> std::time::Duration {
    let seconds = crate::Config::load(workspace, crate::ConfigOverrides::default())
        .map(|config| config.tools.max_timeout_seconds)
        .unwrap_or_else(|error| {
            tracing::warn!("Channel owner turns use the default tool timeout ceiling: {error:#}");
            crate::config::ToolConfig::default().max_timeout_seconds
        });
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
fn tool_owner(credentials: &Credentials, settings: &AccountSettings) -> Option<String> {
    (settings.remote_tools == RemoteTools::Owner)
        .then(|| credentials.owner().map(str::to_owned))
        .flatten()
        .filter(|owner| !owner.is_empty())
}

fn component_id(channel: Channel, account: &str) -> String {
    format!("{}:{account}", channel.name())
}

fn initial_health(
    channel: Channel,
    account: &str,
    credentials: Option<&Credentials>,
    enabled: bool,
) -> ComponentHealth {
    ComponentHealth {
        id: component_id(channel, account),
        channel: channel.name().into(),
        account: account.into(),
        bot_id: credentials.and_then(Credentials::bot_id),
        user_id: credentials.and_then(|c| c.owner().map(str::to_owned)),
        enabled,
        state: ComponentState::Starting,
        last_success_unix_seconds: None,
        error: None,
        restarts: 0,
        remote_tools: RemoteTools::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        fail_first: bool,
    }
    #[async_trait]
    impl Component for Fake {
        async fn run(&self, cancellation: CancellationToken, health: HealthReporter) -> Result<()> {
            let attempt = self.starts.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && attempt == 0 {
                bail!("secret error must never enter status");
            }
            health.contact(true);
            cancellation.cancelled().await;
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn starts_once_recovers_reports_contact_and_joins_before_restoration() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let fake = Arc::new(Fake {
            starts: starts.clone(),
            stops: stops.clone(),
            fail_first: true,
        });
        let mut supervisor = Supervisor {
            initial_backoff: Duration::from_millis(10),
            ..Supervisor::default()
        };
        supervisor.start(
            fake.clone(),
            initial_health(Channel::Wechat, "test", None, true),
        );
        supervisor.start(
            fake.clone(),
            initial_health(Channel::Wechat, "test", None, true),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if supervisor.health()[0].state == ComponentState::Connected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let health = &supervisor.health()[0];
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(health.restarts, 1);
        assert!(health.last_success_unix_seconds.is_some());
        assert!(health.error.is_none());
        supervisor.shutdown().await;
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        supervisor.start(fake, initial_health(Channel::Wechat, "test", None, true));
        tokio::time::sleep(Duration::from_millis(20)).await;
        supervisor.shutdown().await;
        assert_eq!(starts.load(Ordering::SeqCst), 3);
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn credentials_are_not_connection_evidence() {
        let health = initial_health(Channel::Wechat, "saved", None, true);
        assert_eq!(health.state, ComponentState::Starting);
        assert_eq!(health.last_success_unix_seconds, None);
    }

    #[tokio::test]
    async fn busy_account_snapshot_preserves_live_work_but_invalid_settings_stop_it() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let mut components = Components::new(PathBuf::from("/unused.sock"), PathBuf::from("/"));
        components.supervisor.start(
            Arc::new(Fake {
                starts: starts.clone(),
                stops: stops.clone(),
                fail_first: false,
            }),
            initial_health(Channel::Wechat, "test", None, true),
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        components
            .account_error(
                Channel::Wechat,
                "test",
                std::io::Error::from(std::io::ErrorKind::WouldBlock).into(),
            )
            .await;
        assert_eq!(
            components.status().components[0].state,
            ComponentState::Connected
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        components
            .account_error(Channel::Wechat, "test", anyhow::anyhow!("invalid settings"))
            .await;
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            components.status().components[0].state,
            ComponentState::Failed
        );
    }

    struct Stubborn;
    #[async_trait]
    impl Component for Stubborn {
        async fn run(&self, _: CancellationToken, _: HealthReporter) -> Result<()> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn bounded_stop_aborts_uncooperative_component_and_cancels_backoff() {
        let mut supervisor = Supervisor {
            grace: Duration::from_millis(20),
            ..Supervisor::default()
        };
        supervisor.start(
            Arc::new(Stubborn),
            initial_health(Channel::Wechat, "stubborn", None, true),
        );
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_secs(1), supervisor.shutdown())
            .await
            .unwrap();
        assert!(supervisor.health().is_empty());
        let fake = Arc::new(Fake {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
            fail_first: true,
        });
        supervisor.start(fake, initial_health(Channel::Wechat, "backoff", None, true));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(supervisor.health()[0].state, ComponentState::Backoff);
        assert_eq!(
            supervisor.health()[0].error.as_deref(),
            Some("Component stopped unexpectedly; retrying")
        );
        tokio::time::timeout(Duration::from_millis(100), supervisor.shutdown())
            .await
            .unwrap();
    }

    #[test]
    fn remote_tools_require_owner_mode_and_known_owner() {
        let wechat = |user_id: Option<&str>| {
            Credentials::Wechat(scv_clawbot::state::Account {
                token: "token".into(),
                base_url: "https://example.invalid".into(),
                bot_id: Some("bot".into()),
                user_id: user_id.map(Into::into),
            })
        };
        let feishu = |owner: Option<&str>| {
            Credentials::Feishu(scv_feishu::state::Account {
                app_id: "cli_a1b2".into(),
                app_secret: "secret".into(),
                brand: scv_feishu::state::Brand::Feishu,
                owner_open_id: owner.map(Into::into),
            })
        };
        let owner = AccountSettings {
            remote_tools: RemoteTools::Owner,
            ..Default::default()
        };
        assert_eq!(
            tool_owner(&wechat(Some("owner@im.wechat")), &owner).as_deref(),
            Some("owner@im.wechat")
        );
        assert_eq!(tool_owner(&wechat(None), &owner), None);
        assert_eq!(tool_owner(&wechat(Some("")), &owner), None);
        assert_eq!(
            tool_owner(
                &wechat(Some("owner@im.wechat")),
                &AccountSettings::default()
            ),
            None
        );
        assert_eq!(
            tool_owner(&feishu(Some("ou_owner")), &owner).as_deref(),
            Some("ou_owner")
        );
        assert_eq!(tool_owner(&feishu(None), &owner), None);
        assert_eq!(
            tool_owner(&feishu(Some("ou_owner")), &AccountSettings::default()),
            None
        );
    }

    #[test]
    fn channels_are_named_and_health_shows_the_app_and_owner() {
        assert_eq!(Channel::parse("wechat").unwrap(), Channel::Wechat);
        assert_eq!(Channel::parse("feishu").unwrap(), Channel::Feishu);
        assert!(Channel::parse("lark").is_err());
        let credentials = Credentials::Feishu(scv_feishu::state::Account {
            app_id: "cli_a1b2".into(),
            app_secret: "secret".into(),
            brand: scv_feishu::state::Brand::Feishu,
            owner_open_id: Some("ou_owner".into()),
        });
        let health = initial_health(Channel::Feishu, "default", Some(&credentials), true);
        assert_eq!(health.id, "feishu:default");
        assert_eq!(health.channel, "feishu");
        assert_eq!(health.bot_id.as_deref(), Some("cli_a1b2"));
        assert_eq!(health.user_id.as_deref(), Some("ou_owner"));
        assert!(!serde_json::to_string(&health).unwrap().contains("secret"));
    }
}
