//! Server-owned lifecycle for every long-running integration.

use anyhow::{Result, bail};
use async_trait::async_trait;
use scv_clawbot::state::{self, Account, AccountSettings};
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

struct ClawBot {
    account: String,
    credentials: Account,
    workspace: PathBuf,
    socket: PathBuf,
    tool_owner: Option<String>,
}

#[async_trait]
impl Component for ClawBot {
    async fn run(&self, cancellation: CancellationToken, health: HealthReporter) -> Result<()> {
        let tool_owner = self.tool_owner.clone().map(|user_id| {
            let turn_timeout = scv_clawbot::owner_turn_timeout(max_tool_timeout(&self.workspace));
            tracing::info!(
                "ClawBot {} owner turns may run up to {} seconds",
                self.account,
                turn_timeout.as_secs()
            );
            scv_clawbot::ToolOwner {
                user_id,
                turn_timeout,
            }
        });
        scv_clawbot::run_supervised(
            &self.credentials.token,
            &self.credentials.base_url,
            &self.account,
            &self.workspace,
            &self.socket,
            tool_owner.as_ref(),
            cancellation,
            Arc::new(move |connected| health.contact(connected)),
        )
        .await
    }
}

pub(crate) struct Components {
    supervisor: Supervisor,
    desired: BTreeMap<String, (Account, AccountSettings)>,
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
        }
    }

    pub async fn reconcile(&mut self) -> Result<()> {
        let names = match state::account_names() {
            Ok(names) => names,
            Err(_) => {
                self.supervisor.shutdown().await;
                self.desired.clear();
                self.inactive.clear();
                let mut health = initial_health("discovery", None, false);
                health.id = "clawbot:discovery-error".into();
                health.state = ComponentState::Failed;
                health.error = Some(
                    "Account discovery failed; components stopped until configuration is readable"
                        .into(),
                );
                self.inactive.insert("discovery-error".into(), health);
                bail!("Account discovery failed");
            }
        };
        for name in self
            .desired
            .keys()
            .chain(self.inactive.keys())
            .cloned()
            .collect::<Vec<_>>()
        {
            if !names.contains(&name) {
                self.supervisor.stop(&format!("clawbot:{name}")).await;
                self.desired.remove(&name);
                self.inactive.remove(&name);
            }
        }
        for name in names {
            let loaded = (|| -> Result<_> {
                let (account, settings) = state::account_snapshot(&name)?;
                Ok((
                    account.ok_or_else(|| anyhow::anyhow!("missing account"))?,
                    settings,
                ))
            })();
            let (credentials, settings) = match loaded {
                Ok(value) => value,
                Err(error) => {
                    self.account_error(name, error).await;
                    continue;
                }
            };
            if self.desired.get(&name) == Some(&(credentials.clone(), settings.clone())) {
                continue;
            }
            self.supervisor.stop(&format!("clawbot:{name}")).await;
            self.inactive.remove(&name);
            let mut health = initial_health(&name, Some(&credentials), settings.enabled);
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
                    self.inactive.insert(name.clone(), health);
                    self.desired.remove(&name);
                    continue;
                }
                self.supervisor.start(
                    Arc::new(ClawBot {
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
                self.inactive.insert(name.clone(), health);
            }
            self.desired.insert(name, (credentials, settings));
        }
        Ok(())
    }

    async fn account_error(&mut self, name: String, error: anyhow::Error) {
        // A bridge state commit briefly holds this same lock. Retry next refresh
        // rather than interrupting healthy work for ordinary lock contention.
        if is_busy(&error) {
            return;
        }
        self.supervisor.stop(&format!("clawbot:{name}")).await;
        self.desired.remove(&name);
        let mut health = initial_health(&name, None, true);
        health.state = ComponentState::Failed;
        health.error = Some("Invalid or inaccessible account/settings".into());
        self.inactive.insert(name, health);
    }

    pub async fn control(&mut self, command: DaemonCommand) -> Result<DaemonStatus> {
        match command {
            DaemonCommand::Status => return Ok(self.status()),
            DaemonCommand::Reload => {}
            DaemonCommand::ClawbotSet {
                account,
                enabled,
                workspace,
                remote_tools,
            } => {
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
                    if state::account(&account)?.is_none() {
                        bail!("Account is not logged in");
                    }
                    let mut settings = state::settings(&account)?;
                    settings.enabled = enabled;
                    if let Some(path) = &workspace {
                        settings.workspace = Some(path.clone());
                    }
                    if let Some(mode) = remote_tools {
                        settings.remote_tools = mode;
                    }
                    state::save_settings(&account, &settings)
                })
                .await?;
            }
            DaemonCommand::ClawbotLogout { account } => {
                state::validate_name(&account)?;
                // Persist disabled and tool-free first, so failed deletion can
                // neither resurrect a live account nor hand a later login the grant.
                retry_while_busy(|| {
                    let mut settings = state::settings(&account)?;
                    settings.enabled = false;
                    settings.remote_tools = RemoteTools::None;
                    state::save_settings(&account, &settings)
                })
                .await?;
                self.supervisor.stop(&format!("clawbot:{account}")).await;
                self.desired.remove(&account);
                self.inactive.remove(&account);
                retry_while_busy(|| state::remove(&account)).await?;
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
            tracing::warn!("ClawBot uses the default tool timeout ceiling: {error:#}");
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
fn tool_owner(credentials: &Account, settings: &AccountSettings) -> Option<String> {
    (settings.remote_tools == RemoteTools::Owner)
        .then(|| credentials.user_id.clone())
        .flatten()
        .filter(|owner| !owner.is_empty())
}

fn initial_health(account: &str, credentials: Option<&Account>, enabled: bool) -> ComponentHealth {
    ComponentHealth {
        id: format!("clawbot:{account}"),
        account: account.into(),
        bot_id: credentials.and_then(|a| a.bot_id.clone()),
        user_id: credentials.and_then(|a| a.user_id.clone()),
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
        supervisor.start(fake.clone(), initial_health("test", None, true));
        supervisor.start(fake.clone(), initial_health("test", None, true));
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
        supervisor.start(fake, initial_health("test", None, true));
        tokio::time::sleep(Duration::from_millis(20)).await;
        supervisor.shutdown().await;
        assert_eq!(starts.load(Ordering::SeqCst), 3);
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn credentials_are_not_connection_evidence() {
        let health = initial_health("saved", None, true);
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
            initial_health("test", None, true),
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
                "test".into(),
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
            .account_error("test".into(), anyhow::anyhow!("invalid settings"))
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
        supervisor.start(Arc::new(Stubborn), initial_health("stubborn", None, true));
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
        supervisor.start(fake, initial_health("backoff", None, true));
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
        let account = |user_id: Option<&str>| Account {
            token: "token".into(),
            base_url: "https://example.invalid".into(),
            bot_id: Some("bot".into()),
            user_id: user_id.map(Into::into),
        };
        let owner = AccountSettings {
            remote_tools: RemoteTools::Owner,
            ..Default::default()
        };
        assert_eq!(
            tool_owner(&account(Some("owner@im.wechat")), &owner).as_deref(),
            Some("owner@im.wechat")
        );
        assert_eq!(tool_owner(&account(None), &owner), None);
        assert_eq!(tool_owner(&account(Some("")), &owner), None);
        assert_eq!(
            tool_owner(
                &account(Some("owner@im.wechat")),
                &AccountSettings::default()
            ),
            None
        );
    }
}
