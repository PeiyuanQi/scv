//! SCV's authoritative stdio server.

mod agents;
pub mod components;
mod config;
pub mod imports;
pub mod overview;

use std::{
    collections::{HashMap, VecDeque},
    io::Read as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use config::Config;
pub use config::{ApprovalPolicy, ConfigOverrides, user_home_path};
use sha2::{Digest, Sha256};
pub fn init_user_config() -> anyhow::Result<std::path::PathBuf> {
    config::Config::init_user_config()
}
pub fn update_index_url(workspace: &std::path::Path) -> anyhow::Result<Option<String>> {
    Ok(config::Config::load(workspace, ConfigOverrides::default())?
        .update
        .index_url)
}

pub use agents::{Endpoint, PI_PROVIDER, WireApi, read_secret};
pub use scv_tools::{adapters, delegation};

/// Build a command for a native agent's CLI with the same private home and
/// cleaned environment the daemon's `agent_<name>` tool uses, so the agent's
/// own sign-in stores credentials where delegated runs will find them.
/// Project configuration cannot set `[agents]`, so none is read.
pub fn agent_command(agent: &str) -> Result<std::process::Command> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let adapter = config
        .adapters()
        .remove(&format!("agent_{agent}"))
        .ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    let executable =
        scv_tools::adapters::resolve_agent_executable(&adapter.command, &adapter.search_dirs)
            .ok_or_else(|| {
                anyhow!(
                    "{agent} is not installed: {:?} was not found on PATH or in ~/.local/bin",
                    adapter.command
                )
            })?;
    let mut command = std::process::Command::new(executable);
    command.current_dir(config.layout().agent_home(agent));
    scv_tools::apply_agent_environment(&mut command, &adapter.environment);
    Ok(command)
}

/// Where `agent`'s executable resolves, as the daemon would find it.
pub fn agent_executable(agent: &str) -> Result<Option<PathBuf>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let adapter = config
        .adapters()
        .remove(&format!("agent_{agent}"))
        .ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    Ok(scv_tools::adapters::resolve_agent_executable(
        &adapter.command,
        &adapter.search_dirs,
    ))
}

/// The prepared private agent home for `agent`.
pub fn agent_home(agent: &str) -> Result<PathBuf> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let home = config.layout().agent_home(agent);
    if !home.is_dir() {
        return Err(anyhow!("unknown agent {agent}"));
    }
    Ok(home)
}

/// Remove delegated-conversation transcripts older than `older_than` from
/// the agent homes (`agent`, or every agent that keeps them), keeping any a
/// live conversation still uses. Returns each agent's report.
pub fn collect_agent_garbage(
    agent: Option<&str>,
    older_than: std::time::Duration,
    dry_run: bool,
) -> Result<Vec<(&'static str, scv_tools::conversation::GcReport)>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let markers = config.layout().conversations();
    let mut reports = Vec::new();
    for adapter in adapters::ADAPTERS {
        if agent.is_some_and(|agent| agent != adapter.name) {
            continue;
        }
        let Some(files) = adapter.conversation_files else {
            continue;
        };
        let home = config.layout().agent_home(adapter.name);
        if !home.is_dir() {
            continue;
        }
        let report =
            scv_tools::conversation::collect_garbage(&home, files, &markers, older_than, dry_run)
                .with_context(|| format!("clean {} transcripts", adapter.name))?;
        reports.push((adapter.name, report));
    }
    Ok(reports)
}

/// Parse a `scv agents gc --older-than` age such as `30d`.
pub fn conversation_age(value: &str) -> std::result::Result<std::time::Duration, String> {
    scv_tools::conversation::parse_age(value)
}

fn key_store_home(agent: &str) -> Result<PathBuf> {
    scv_tools::adapters::adapter(agent).ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    agent_home(agent)
}

/// Store `key` in the agent's native credential file inside its agent home.
pub fn store_agent_key(agent: &str, store: adapters::KeyStore, key: &str) -> Result<Vec<String>> {
    agents::store_key(store, &key_store_home(agent)?, key)
}

/// Whether the agent's stored credentials exist, with display lines that
/// never contain secrets.
pub fn agent_stored_status(agent: &str, store: adapters::KeyStore) -> Result<(bool, Vec<String>)> {
    agents::stored_status(store, &key_store_home(agent)?)
}

/// Remove the agent's stored credentials from its agent home.
pub fn remove_agent_credentials(agent: &str, store: adapters::KeyStore) -> Result<Vec<String>> {
    agents::remove_stored(store, &key_store_home(agent)?)
}

/// Point SCV's pi at an OpenAI-compatible endpoint.
pub fn configure_pi_endpoint(endpoint: &Endpoint, key: &str) -> Result<Vec<String>> {
    agents::configure_pi_endpoint(&pi_agent_dir()?, endpoint, key)
}

/// Point SCV's pi at SCV's own active provider: its base URL, model, and key
/// (read from `api_key`, or from the `api_key_env` variable now, since
/// delegated agents never inherit key variables).
pub fn import_pi_from_scv_provider() -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let provider = &config.provider;
    let key = scv_provider_key(provider)?;
    let endpoint = Endpoint {
        base_url: provider.base_url.clone(),
        api: WireApi::Responses,
        model: provider.model.clone(),
    };
    let mut notes = agents::configure_pi_endpoint(&pi_agent_dir()?, &endpoint, &key)?;
    imports::record(
        &config.layout(),
        "pi",
        imports::Source::ScvProvider,
        provider_digest("pi", &config, &key)?,
    )?;
    if !provider.headers.is_empty() {
        notes.push(
            "Note: SCV's provider sends extra headers, which were not copied; add them to \
             pi's models.json if the endpoint needs them"
                .into(),
        );
    }
    Ok(notes)
}

/// The key of SCV's own provider: `api_key`, or the `api_key_env` variable
/// read now, since delegated agents never inherit key variables.
fn scv_provider_key(provider: &config::ProviderConfig) -> Result<String> {
    if provider.kind != "openai-compatible" {
        return Err(anyhow!("SCV's provider is not openai-compatible"));
    }
    match (&provider.api_key, &provider.api_key_env) {
        (Some(key), _) if !key.trim().is_empty() => Ok(key.trim().to_owned()),
        (_, Some(variable)) => std::env::var(variable)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("SCV's provider reads its key from ${variable}, which is not set here")
            }),
        _ => Err(anyhow!("SCV's provider has no API key configured")),
    }
}

/// Digest of what a provider import copies to `agent`: SCV's provider
/// settings and key, as each agent receives them.
fn provider_digest(agent: &str, config: &Config, key: &str) -> Result<String> {
    let provider = &config.provider;
    let mut headers: Vec<_> = provider.headers.iter().collect();
    headers.sort();
    match agent {
        "pi" => imports::digest_value(&(&provider.base_url, &provider.model, key)),
        _ => imports::digest_value(&(
            &provider.base_url,
            &provider.model,
            key,
            &provider.wire_api,
            provider.timeout_seconds,
            headers,
            config.hosted_web_search(),
        )),
    }
}

/// How `agent`'s imported copy compares with its source now, as one display
/// line without secrets; `None` when nothing was imported.
pub fn agent_import_status(agent: &str) -> Result<Option<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let status = imports::check(&config.layout(), agent, || {
        let key = scv_provider_key(&config.provider).ok()?;
        provider_digest(agent, &config, &key).ok()
    })?;
    Ok(status.map(|status| status.describe(agent, imports::now())))
}

/// Give the nested SCV (`agent_scv`) its own copy of SCV's active provider,
/// in `$SCV_HOME/agents/scv/config.toml` (mode 0600).
pub fn import_scv_from_scv_provider() -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let provider = &config.provider;
    let key = scv_provider_key(provider)?;
    let digest = provider_digest("scv", &config, &key)?;
    let notes = agents::configure_scv_child(
        &agent_home("scv")?,
        &agents::ScvChildProvider {
            wire_api: &provider.wire_api,
            model: &provider.model,
            base_url: &provider.base_url,
            timeout_seconds: provider.timeout_seconds,
            headers: &provider.headers,
            hosted_web_search: config.hosted_web_search(),
        },
        &key,
    )?;
    imports::record(
        &config.layout(),
        "scv",
        imports::Source::ScvProvider,
        digest,
    )?;
    Ok(notes)
}

fn pi_agent_dir() -> Result<PathBuf> {
    let descriptor =
        scv_tools::adapters::adapter("pi").ok_or_else(|| anyhow!("unknown agent pi"))?;
    let scv_tools::adapters::Status::Stored(scv_tools::adapters::KeyStore::Pi { dir }) =
        descriptor.status
    else {
        return Err(anyhow!("pi has no SCV-managed store"));
    };
    Ok(agent_home("pi")?.join(dir))
}

/// Copy the user's own Codex setup from `source` into SCV's private Codex
/// agent home: `config.toml`, and `auth.json` only when it holds an API key.
/// Returns display lines that never contain secret values.
pub fn import_codex(source: &Path) -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let layout = config.layout();
    let notes = agents::import_codex(source, &layout.agent_home("codex"))?;
    record_file_import(&layout, "codex", source, agents::codex_copied_files(source))?;
    Ok(notes)
}

/// Remember which files an import copied from `source`, so a later change
/// there shows up as a stale copy.
fn record_file_import(
    layout: &scv_client::Layout,
    agent: &str,
    source: &Path,
    files: Vec<String>,
) -> Result<()> {
    let dir = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_owned());
    let digest = imports::digest_files(&dir, &files)?;
    imports::record(layout, agent, imports::Source::Files { dir, files }, digest)
}

/// Copy the user's own Grok `config.toml` from `source` (a Grok home) into
/// SCV's private Grok home, keeping settings only SCV's copy has. Returns
/// display lines that never contain secret values.
pub fn import_grok(source: &Path) -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let descriptor =
        scv_tools::adapters::adapter("grok").ok_or_else(|| anyhow!("unknown agent grok"))?;
    let grok_home = descriptor
        .home_environment
        .iter()
        .find(|(variable, _)| *variable == "GROK_HOME")
        .map(|(_, relative)| *relative)
        .ok_or_else(|| anyhow!("grok has no GROK_HOME in its agent home"))?;
    let layout = config.layout();
    let notes = agents::import_grok(source, &layout.agent_home("grok").join(grok_home))?;
    record_file_import(&layout, "grok", source, vec!["config.toml".into()])?;
    Ok(notes)
}

/// Return the user service name for the selected SCV instance.
pub fn service_name() -> anyhow::Result<String> {
    if std::env::var_os("SCV_HOME").is_none() {
        return Ok("scv.service".into());
    }
    let home = user_home_path().ok_or_else(|| anyhow!("cannot determine SCV instance home"))?;
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("scv-{suffix}.service"))
}

pub fn service_unit_path() -> anyhow::Result<std::path::PathBuf> {
    let config =
        dirs::config_dir().ok_or_else(|| anyhow!("cannot determine XDG config directory"))?;
    Ok(config.join("systemd/user").join(service_name()?))
}
use scv_core::{
    AgentError, AgentRuntime, ApprovalGate, ApprovalRequest, BudgetContextPolicy, CoreEvent,
    EventSink, Message, ToolRegistry, ToolRisk,
};
use scv_protocol::{
    ClientMessage, DaemonCommand, DaemonStatus, DelegationInfo, DelegationSummary,
    ORIGIN_BACKGROUND, PROTOCOL_VERSION, PeerInfo, QueueEntry, ServerEvent, TurnOrigin, Usage,
};
use scv_provider_openai::OpenAiProvider;
use scv_tools::{
    DelegationContext, SkillMap,
    background::{self, BackgroundJobs},
    builtin_registry,
    delegation::{self as delegations, DelegationRegistry},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

const PROMPT_LIMIT_BYTES: usize = 256 * 1024;
const OUTPUT_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_MIN_BYTES: usize = 16 * 1024 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const MAX_QUEUE_ITEMS: usize = 64;
const MAX_QUEUE_BYTES: usize = 4 * 1024 * 1024;

pub async fn run_stdio(overrides: ConfigOverrides) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let tasks = TaskTracker::new();
    let registry = instance_delegations()?;
    // Without a daemon, a later `scv exec` is what cleans up after an earlier
    // one that was killed; this runs alongside the session.
    tokio::spawn(reconcile_delegations(Arc::clone(&registry)));
    let result = run_managed(
        stdin,
        stdout,
        overrides,
        None,
        registry,
        CancellationToken::new(),
        tasks.clone(),
    )
    .await;
    tasks.close();
    tasks.wait().await;
    result
}

/// Return the local Unix socket used by the SCV daemon and TUI.
pub fn default_socket_path() -> Result<PathBuf> {
    scv_client::default_socket_path()
}

/// Run the authoritative server on the local Unix socket.
pub async fn run_socket(path: &Path, overrides: ConfigOverrides) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("create SCV socket directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .context("secure SCV socket directory")?;
        }
    }
    let _lock = SocketLock::acquire(path)?;
    // Nothing reads an older release's files; say so once rather than let
    // them look like live configuration.
    if let Ok(strays) = scv_client::Layout::from_env().and_then(|layout| layout.strays()) {
        for stray in strays.into_iter().filter(|stray| stray.legacy) {
            tracing::warn!(
                "{} is from an older SCV layout and is not used; see `scv config show`",
                stray.path.display()
            );
        }
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(anyhow!(
                "SCV server is already running at {}",
                path.display()
            ));
        }
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
            return Err(anyhow!(
                "refusing to remove a non-socket at SCV socket path"
            ));
        }
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("remove stale SCV socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind SCV server socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("secure SCV socket")?;
    }
    let components = Arc::new(Mutex::new(components::Components::new(
        path.to_owned(),
        std::env::current_dir()?,
    )));
    let registry = instance_delegations()?;
    // Descendants a delegated agent leaves behind reparent to the daemon, not init.
    if !delegations::become_child_subreaper() {
        tracing::debug!("SCV daemon is not a child subreaper on this platform");
    }
    let cancellation = CancellationToken::new();
    let delegation_registry = Arc::clone(&registry);
    let delegation_cancel = cancellation.clone();
    let mut delegation_task = tokio::spawn(async move {
        // The first tick is immediate: orphans from before a restart go first.
        let mut interval = tokio::time::interval(DELEGATION_RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                biased;
                _ = delegation_cancel.cancelled() => break,
                _ = interval.tick() => {
                    reconcile_delegations(Arc::clone(&delegation_registry)).await;
                    let zombies = delegations::reap_orphaned_zombies();
                    if zombies > 0 {
                        tracing::debug!("Reaped {zombies} exited orphan processes");
                    }
                }
            }
        }
    });
    let _delegation_abort = AbortGuard(delegation_task.abort_handle());
    let tasks = TaskTracker::new();
    let mut clients = tokio::task::JoinSet::new();
    let refresh_components = components.clone();
    let refresh_cancel = cancellation.clone();
    let mut refresh_task = tokio::spawn(async move {
        let mut refresh = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                biased;
                _ = refresh_cancel.cancelled() => break,
                _ = refresh.tick() => {
                    tokio::select! {
                        biased;
                        _ = refresh_cancel.cancelled() => break,
                        result = async { refresh_components.lock().await.reconcile().await } => {
                            if result.is_err() { tracing::warn!("Component account discovery failed"); }
                        }
                    }
                }
            }
        }
    });
    let _refresh_abort = AbortGuard(refresh_task.abort_handle());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                let child_overrides = overrides.clone();
                let components = components.clone();
                let registry = Arc::clone(&registry);
                let cancellation = cancellation.clone();
                let tasks = tasks.clone();
                clients.spawn(async move {
                    let (reader, writer) = stream.into_split();
                    if run_managed(reader, writer, child_overrides, Some(components), registry, cancellation, tasks).await.is_err() {
                        tracing::warn!("SCV socket client stopped");
                    }
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {},
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
        }
    };
    drop(listener);
    cancellation.cancel();
    let _ = (&mut refresh_task).await;
    let _ = (&mut delegation_task).await;
    components.lock().await.shutdown().await;
    if tokio::time::timeout(Duration::from_secs(8), async {
        while clients.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    }
    tasks.close();
    tasks.wait().await;
    let _ = tokio::fs::remove_file(path).await;
    result
}

const DELEGATION_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// The delegation registry for this process's SCV instance.
fn instance_delegations() -> Result<Arc<DelegationRegistry>> {
    let home =
        config::user_home_path().ok_or_else(|| anyhow!("cannot determine SCV instance home"))?;
    Ok(Arc::new(DelegationRegistry::new(&home)))
}

/// Stop orphaned delegations of this instance and log what was stopped.
async fn reconcile_delegations(registry: Arc<DelegationRegistry>) {
    let report = registry.reconcile().await;
    if !report.reaped.is_empty() {
        tracing::info!(
            "Reaped {} orphaned delegations: {}",
            report.reaped.len(),
            report.reaped.join(", ")
        );
    }
    if report.removed > 0 {
        tracing::debug!(
            "Removed {} delegation records whose processes had exited",
            report.removed
        );
    }
    if report.stale_markers > 0 {
        tracing::debug!(
            "Removed {} conversation markers whose SCV process had exited",
            report.stale_markers
        );
    }
}

/// Why a daemon control request failed.
enum ControlFailure {
    /// A delegation request the client can correct; the message is safe to show.
    Delegation(String),
    Component,
}

/// Apply a daemon control command, adding the instance's delegations.
async fn daemon_control(
    components: &Arc<Mutex<components::Components>>,
    registry: &DelegationRegistry,
    command: DaemonCommand,
) -> std::result::Result<DaemonStatus, ControlFailure> {
    let mut killed = Vec::new();
    let listing = match &command {
        DaemonCommand::Delegations { all } => Some(*all),
        DaemonCommand::DelegationKill { handle, orphans } => {
            if handle.is_none() && !orphans {
                return Err(ControlFailure::Delegation(
                    "name a delegation handle or ask for orphans".into(),
                ));
            }
            if *orphans {
                let report = registry.reconcile().await;
                killed.extend(report.reaped);
            }
            if let Some(handle) = handle {
                registry
                    .kill(handle)
                    .await
                    .map_err(ControlFailure::Delegation)?;
                killed.push(handle.clone());
            }
            Some(true)
        }
        _ => None,
    };
    let mut status = components
        .lock()
        .await
        .control(command)
        .await
        .map_err(|_| ControlFailure::Component)?;
    let running = registry.list(false);
    status.delegations = DelegationSummary {
        active: running.len() as u64,
        reaped: registry.reaped_total(),
        entries: match listing {
            Some(true) => registry.list(true),
            Some(false) => running,
            None => Vec::new(),
        }
        .into_iter()
        .map(|entry| DelegationInfo {
            handle: entry.record.handle,
            agent: entry.record.agent,
            session: entry.record.session,
            depth: entry.record.depth,
            pid: entry.record.process.pid,
            owner_pid: entry.record.owner.pid,
            processes: u32::try_from(entry.processes).unwrap_or(u32::MAX),
            cwd: entry.record.cwd.display().to_string(),
            started_unix_seconds: entry.record.started_unix,
            orphaned: entry.orphaned,
            conversation: entry.record.conversation,
            turn: entry.record.turn,
        })
        .collect(),
        killed,
    };
    Ok(status)
}

async fn run_managed<R, W>(
    reader: R,
    writer: W,
    overrides: ConfigOverrides,
    components: Option<Arc<Mutex<components::Components>>>,
    registry: Arc<DelegationRegistry>,
    cancellation: CancellationToken,
    tasks: TaskTracker,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let initial_output_bytes =
        output_queue_bytes(Config::default().protocol.max_server_frame_bytes)?;
    let (output_tx, mut output_rx) = outbound_channel(initial_output_bytes);
    let mut writer_task = tasks.spawn(async move {
        let mut writer = writer;
        while let Some(frame) = output_rx.recv().await {
            writer.write_all(&frame.bytes).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });
    let _writer_abort = AbortGuard(writer_task.abort_handle());
    let (done_tx, mut done_rx) = mpsc::channel::<TurnDone>(4);
    let approvals = Arc::new(ApprovalBroker::default());
    let mut reader = BufReader::new(reader);
    let mut frames = FrameBuffer::default();
    let mut initialized = false;
    let mut session: Option<Session> = None;
    let mut active: Option<ActiveTurn> = None;
    // Woken when a background job finishes; set until its report turn starts.
    let mut background_rx: Option<mpsc::UnboundedReceiver<()>> = None;
    let mut background_ready = false;
    let mut fatal = false;
    let mut writer_finished = false;

    let loop_result: Result<()> = async {
        loop {
        let frame_limit = session.as_ref().map_or_else(
            || Config::default().protocol.max_client_frame_bytes,
            |value| value.config.protocol.max_client_frame_bytes,
        );
        tokio::select! {
            _ = cancellation.cancelled() => break,
            read = frames.read(&mut reader, frame_limit) => {
                let frame = match read.context("read protocol input")? {
                    FrameRead::Eof => {
                        if let Some(active) = &active { active.cancellation.cancel(); }
                        break;
                    }
                    FrameRead::TooLarge => {
                        send_error(&output_tx, "", "invalid_request", "client frame exceeds configured limit", false, server_frame_limit(&session)).await?;
                        continue;
                    }
                    FrameRead::Frame(frame) => frame,
                };
                if frame.is_empty() {
                    send_error(&output_tx, "", "invalid_json", "protocol frame is empty", false, server_frame_limit(&session)).await?;
                    continue;
                }
                let message = match serde_json::from_slice::<ClientMessage>(&frame) {
                    Ok(message) => message,
                    Err(error) => {
                        send_error(&output_tx, "", "invalid_json", &format!("invalid protocol JSON: {error}"), false, server_frame_limit(&session)).await?;
                        continue;
                    }
                };
                match message {
                    ClientMessage::Initialize { request_id, protocol_version, .. } => {
                        if initialized {
                            send_error(&output_tx, &request_id, "invalid_request", "connection is already initialized", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        if protocol_version != PROTOCOL_VERSION {
                            send_error(&output_tx, &request_id, "version_mismatch", &format!("server supports protocol {PROTOCOL_VERSION}"), true, server_frame_limit(&session)).await?;
                            fatal = true;
                            break;
                        }
                        initialized = true;
                        send_event(&output_tx, ServerEvent::Initialized {
                            request_id,
                            protocol_version: PROTOCOL_VERSION,
                            server: PeerInfo { name: "scv-server".into(), version: env!("CARGO_PKG_VERSION").into() },
                        }, Config::default().protocol.max_server_frame_bytes).await?;
                    }
                    other if !initialized => {
                        send_error(&output_tx, other.request_id(), "not_initialized", "initialize must be the first message", false, server_frame_limit(&session)).await?;
                    }
                    ClientMessage::DaemonControl { request_id, command } => {
                        if let Some(components) = &components {
                            let result = tokio::select! {
                                biased;
                                _ = cancellation.cancelled() => break,
                                result = daemon_control(components, &registry, command) => result,
                            };
                            match result {
                                Ok(status) => send_event(&output_tx, ServerEvent::DaemonStatus { request_id, status }, server_frame_limit(&session)).await?,
                                Err(ControlFailure::Delegation(message)) => send_error(&output_tx, &request_id, "delegation_error", &message, false, server_frame_limit(&session)).await?,
                                Err(ControlFailure::Component) => send_error(&output_tx, &request_id, "component_error", "Component operation failed; check account credentials, private file permissions and absolute workspace", false, server_frame_limit(&session)).await?,
                            }
                        } else {
                            send_error(&output_tx, &request_id, "unsupported", "Component management requires the daemon socket", false, server_frame_limit(&session)).await?;
                        }
                    }
                    ClientMessage::SessionStart { request_id, cwd, provider, model, base_url, no_tools, delegation_depth, channel, auto_approve } => {
                        if session.is_some() {
                            send_error(&output_tx, &request_id, "invalid_request", "this connection already has a session", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        if channel.as_deref().is_some_and(|name| !valid_channel_name(name)) {
                            send_error(&output_tx, &request_id, "invalid_request", "channel must be a short name without control characters", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        let client = SessionClient { channel, auto_approve: auto_approve.unwrap_or(false) };
                        let session_overrides = ConfigOverrides {
                            provider: provider.or_else(|| overrides.provider.clone()),
                            model: model.or_else(|| overrides.model.clone()),
                            base_url: base_url.or_else(|| overrides.base_url.clone()),
                            approval_policy: overrides.approval_policy,
                            no_tools: no_tools.unwrap_or(overrides.no_tools),
                        };
                        match build_session(&cwd, session_overrides, delegation_depth.unwrap_or(0), &registry, client).await {
                            Ok((new_session, finished)) => {
                                background_rx = finished;
                                output_tx.ensure_capacity(output_queue_bytes(
                                    new_session.config.protocol.max_server_frame_bytes,
                                )?)?;
                                let event = ServerEvent::SessionStarted {
                                    request_id,
                                    session_id: new_session.id.clone(),
                                    cwd: new_session.workspace.display().to_string(),
                                    model: new_session.runtime.model().to_owned(),
                                    context_max_tokens: new_session.config.context.max_tokens,
                                    max_server_frame_bytes: new_session.config.protocol.max_server_frame_bytes,
                                    max_transcript_bytes: new_session.config.tui.max_transcript_bytes,
                                    max_transcript_items: new_session.config.tui.max_transcript_items,
                                    max_prompt_history_bytes: new_session.config.tui.max_prompt_history_bytes,
                                    max_prompt_history_items: new_session.config.tui.max_prompt_history_items,
                                };
                                send_event(&output_tx, event, new_session.config.protocol.max_server_frame_bytes).await?;
                                send_event(&output_tx, ServerEvent::QueueSnapshot {
                                    request_id: None,
                                    session_id: new_session.id.clone(),
                                    seq: next_seq(&new_session.seq),
                                    entries: new_session.queue.lock().await.iter().cloned().collect(),
                                    paused: new_session.paused.load(Ordering::Acquire),
                                }, new_session.config.protocol.max_server_frame_bytes).await?;
                                session = Some(new_session);
                            }
                            Err(error) => {
                                send_error(&output_tx, &request_id, "invalid_request", &error.to_string(), false, server_frame_limit(&session)).await?;
                            }
                        }
                    }
                    ClientMessage::SessionAttach { request_id, .. } => {
                        send_error(&output_tx, &request_id, "unsupported", "session attach requires the shared socket server", false, server_frame_limit(&session)).await?;
                    }
                    ClientMessage::TurnStart { request_id, session_id, prompt } => {
                        let Some(current) = session.as_ref() else {
                            send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?;
                            continue;
                        };
                        if current.id != session_id {
                            send_error(&output_tx, &request_id, "session_not_found", "session id does not match", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        if prompt.trim().is_empty() || prompt.len() > PROMPT_LIMIT_BYTES {
                            send_error(&output_tx, &request_id, "invalid_request", "prompt must be non-empty and no larger than 256 KiB", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        if active.is_some() {
                            let entry = match current.enqueue(prompt, request_id.clone()).await {
                                Ok(entry) => entry,
                                Err(code) => { send_error(&output_tx, &request_id, code, "session queue limit reached", false, server_frame_limit(&session)).await?; continue; }
                            };
                            let position = current.queue.lock().await.len().saturating_sub(1);
                            send_event(&output_tx, ServerEvent::QueueEnqueued {
                                request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), entry, position,
                            }, current.config.protocol.max_server_frame_bytes).await?;
                            continue;
                        }
                        let starter = TurnStarter { output: &output_tx, approvals: &approvals, done: &done_tx, tasks: &tasks, cancellation: &cancellation };
                        active = Some(starter.start(current, Uuid::new_v4().to_string(), request_id, prompt, None).await?);
                    }
                    ClientMessage::QueueUpdate { request_id, session_id, queue_id, revision, prompt } => {
                        let Some(current) = session.as_ref() else { send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?; continue; };
                        if current.id != session_id { send_error(&output_tx, &request_id, "session_not_found", "session id does not match", false, server_frame_limit(&session)).await?; continue; }
                        if prompt.trim().is_empty() || prompt.len() > PROMPT_LIMIT_BYTES { send_error(&output_tx, &request_id, "invalid_request", "prompt must be non-empty and no larger than 256 KiB", false, server_frame_limit(&session)).await?; continue; }
                        match current.update_queue(&queue_id, revision, prompt).await {
                            Ok(entry) => send_event(&output_tx, ServerEvent::QueueUpdated { request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), entry }, current.config.protocol.max_server_frame_bytes).await?,
                            Err(code) => send_error(&output_tx, &request_id, code, "queue entry was not found or revision is stale", false, server_frame_limit(&session)).await?,
                        }
                    }
                    ClientMessage::QueueMove { request_id, session_id, queue_id, revision, before_queue_id } => {
                        let Some(current) = session.as_ref() else { send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?; continue; };
                        match current.move_queue(&session_id, &queue_id, revision, before_queue_id).await {
                            Ok((id, rev, pos)) => send_event(&output_tx, ServerEvent::QueueMoved { request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), queue_id: id, position: pos, revision: rev }, current.config.protocol.max_server_frame_bytes).await?,
                            Err(code) => send_error(&output_tx, &request_id, code, "queue entry was not found or revision is stale", false, server_frame_limit(&session)).await?,
                        }
                    }
                    ClientMessage::QueueRemove { request_id, session_id, queue_id, revision } => {
                        let Some(current) = session.as_ref() else { send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?; continue; };
                        match current.remove_queue(&session_id, &queue_id, revision).await {
                            Ok((id, rev)) => send_event(&output_tx, ServerEvent::QueueRemoved { request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), queue_id: id, revision: rev }, current.config.protocol.max_server_frame_bytes).await?,
                            Err(code) => send_error(&output_tx, &request_id, code, "queue entry was not found or revision is stale", false, server_frame_limit(&session)).await?,
                        }
                    }
                    ClientMessage::SessionPause { request_id, session_id, paused } => {
                        let Some(current) = session.as_ref() else { send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?; continue; };
                        if current.id != session_id { send_error(&output_tx, &request_id, "session_not_found", "session id does not match", false, server_frame_limit(&session)).await?; continue; }
                        current.paused.store(paused, Ordering::Release);
                        send_event(&output_tx, ServerEvent::SessionPaused { request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), paused }, current.config.protocol.max_server_frame_bytes).await?;
                    }
                    ClientMessage::TurnCancel { request_id, session_id, turn_id } => {
                        match (&session, &active) {
                            (Some(current), Some(running)) if current.id == session_id && running.turn_id == turn_id => running.cancellation.cancel(),
                            _ => send_error(&output_tx, &request_id, "turn_not_found", "active turn was not found", false, server_frame_limit(&session)).await?,
                        }
                    }
                    ClientMessage::ApprovalResolve { request_id, session_id, approval_id, approved } => {
                        if session.as_ref().is_none_or(|current| current.id != session_id) {
                            send_error(&output_tx, &request_id, "session_not_found", "session id does not match", false, server_frame_limit(&session)).await?;
                        } else if !approvals.resolve(&approval_id, approved).await {
                            send_error(&output_tx, &request_id, "approval_not_found", "approval was not found or already resolved", false, server_frame_limit(&session)).await?;
                        }
                    }
                    ClientMessage::SessionClear { request_id, session_id } => {
                        let Some(current) = session.as_ref() else {
                            send_error(&output_tx, &request_id, "session_not_found", "session was not found", false, server_frame_limit(&session)).await?;
                            continue;
                        };
                        if current.id != session_id {
                            send_error(&output_tx, &request_id, "session_not_found", "session id does not match", false, server_frame_limit(&session)).await?;
                        } else if active.is_some() {
                            send_error(&output_tx, &request_id, "turn_active", "cancel the active turn before clearing", false, server_frame_limit(&session)).await?;
                        } else {
                            current.history.lock().await.clear();
                            current.queue.lock().await.clear();
                            send_event(&output_tx, ServerEvent::SessionCleared {
                                request_id,
                                session_id: current.id.clone(),
                                seq: next_seq(&current.seq),
                            }, current.config.protocol.max_server_frame_bytes).await?;
                            send_event(&output_tx, ServerEvent::QueueSnapshot { request_id: None, session_id: current.id.clone(), seq: next_seq(&current.seq), entries: Vec::new(), paused: current.paused.load(Ordering::Acquire) }, current.config.protocol.max_server_frame_bytes).await?;
                        }
                    }
                }
            }
            writer = &mut writer_task => {
                writer_finished = true;
                writer.context("join protocol writer")??;
                break;
            }
            Some(()) = recv_background(&mut background_rx) => {
                background_ready = true;
                if active.is_none()
                    && let Some(current) = session.as_ref()
                    && !current.paused.load(Ordering::Acquire)
                    && current.queue.lock().await.is_empty()
                {
                    let starter = TurnStarter { output: &output_tx, approvals: &approvals, done: &done_tx, tasks: &tasks, cancellation: &cancellation };
                    active = starter.report_background(current).await?;
                    background_ready = active.is_some();
                }
            }
            done = done_rx.recv(), if active.is_some() => {
                if let Some(done) = done {
                    if let Some(current) = session.as_ref() {
                        let seq = next_seq(&current.seq);
                        let event = match done.result {
                            Ok(outcome) => ServerEvent::TurnCompleted {
                                request_id: done.request_id,
                                session_id: done.session_id,
                                turn_id: done.turn_id,
                                seq,
                                steps: outcome.steps,
                                usage: Usage { input_tokens: outcome.usage.input_tokens, output_tokens: outcome.usage.output_tokens },
                                origin: done.origin,
                            },
                            Err(AgentError::Cancelled) => ServerEvent::TurnCancelled {
                                request_id: done.request_id,
                                session_id: done.session_id,
                                turn_id: done.turn_id,
                                seq,
                                origin: done.origin,
                            },
                            Err(error) => ServerEvent::TurnFailed {
                                request_id: done.request_id,
                                session_id: done.session_id,
                                turn_id: done.turn_id,
                                seq,
                                code: error.code().into(),
                                message: error.to_string(),
                                origin: done.origin,
                            },
                        };
                        send_event(&output_tx, event, current.config.protocol.max_server_frame_bytes).await?;
                    }
                    if let Some(mut active) = active.take() {
                        let _ = (&mut active.task).await;
                    }
                    if let Some(current) = session.as_ref()
                        && !current.paused.load(Ordering::Acquire)
                        && let Some(entry) = current.queue.lock().await.pop_front()
                    {
                        let turn_id = Uuid::new_v4().to_string();
                        send_event(&output_tx, ServerEvent::QueueDequeued {
                            request_id: entry.submitter.clone(),
                            session_id: current.id.clone(),
                            seq: next_seq(&current.seq),
                            queue_id: entry.queue_id,
                            turn_id: turn_id.clone(),
                        }, current.config.protocol.max_server_frame_bytes).await?;
                        let starter = TurnStarter { output: &output_tx, approvals: &approvals, done: &done_tx, tasks: &tasks, cancellation: &cancellation };
                        active = Some(starter.start(current, turn_id, entry.submitter, entry.prompt, None).await?);
                    }
                    // Report finished background jobs once the user's own work is done.
                    if active.is_none() && background_ready
                        && let Some(current) = session.as_ref()
                    {
                        let starter = TurnStarter { output: &output_tx, approvals: &approvals, done: &done_tx, tasks: &tasks, cancellation: &cancellation };
                        active = starter.report_background(current).await?;
                        background_ready = active.is_some();
                    }
                }
            }
        }
        }
        Ok(())
    }
    .await;

    if let Some(active) = active.take() {
        shutdown_active_turn(active, SHUTDOWN_GRACE).await;
    }
    drop(output_tx);
    let writer_result = if writer_finished {
        Ok(())
    } else {
        shutdown_writer(writer_task, SHUTDOWN_GRACE).await
    };
    loop_result?;
    writer_result?;
    if fatal {
        return Err(anyhow!("protocol version mismatch"));
    }
    Ok(())
}

enum FrameRead {
    Eof,
    Frame(Vec<u8>),
    TooLarge,
}

struct OutboundFrame {
    bytes: Vec<u8>,
    _byte_permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct OutboundSender {
    frames: mpsc::Sender<OutboundFrame>,
    budget: Arc<Semaphore>,
    capacity: Arc<AtomicUsize>,
}

#[derive(Debug, PartialEq, Eq)]
enum OutboundSendError {
    Cancelled,
    Closed,
    TimedOut,
    FrameExceedsQueue { frame_bytes: usize, capacity: usize },
}

impl std::fmt::Display for OutboundSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("outbound send cancelled"),
            Self::Closed => formatter.write_str("protocol client disconnected"),
            Self::TimedOut => formatter.write_str("outbound send timed out under backpressure"),
            Self::FrameExceedsQueue {
                frame_bytes,
                capacity,
            } => write!(
                formatter,
                "outbound frame uses {frame_bytes} bytes but queue capacity is {capacity} bytes"
            ),
        }
    }
}

impl std::error::Error for OutboundSendError {}

fn outbound_channel(capacity: usize) -> (OutboundSender, mpsc::Receiver<OutboundFrame>) {
    let (frames, receiver) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
    (
        OutboundSender {
            frames,
            budget: Arc::new(Semaphore::new(capacity)),
            capacity: Arc::new(AtomicUsize::new(capacity)),
        },
        receiver,
    )
}

impl OutboundSender {
    fn ensure_capacity(&self, required: usize) -> Result<()> {
        if required > Semaphore::MAX_PERMITS {
            return Err(anyhow!(
                "outbound queue capacity {required} exceeds runtime limit {}",
                Semaphore::MAX_PERMITS
            ));
        }
        let current = self.capacity.load(Ordering::Acquire);
        if required > current {
            self.budget.add_permits(required - current);
            self.capacity.store(required, Ordering::Release);
        }
        Ok(())
    }

    async fn send(
        &self,
        bytes: Vec<u8>,
        cancellation: Option<&CancellationToken>,
    ) -> std::result::Result<(), OutboundSendError> {
        self.send_with_timeout(bytes, cancellation, SHUTDOWN_GRACE)
            .await
    }

    async fn send_with_timeout(
        &self,
        bytes: Vec<u8>,
        cancellation: Option<&CancellationToken>,
        control_timeout: Duration,
    ) -> std::result::Result<(), OutboundSendError> {
        let frame_bytes =
            bytes
                .len()
                .checked_add(1)
                .ok_or(OutboundSendError::FrameExceedsQueue {
                    frame_bytes: usize::MAX,
                    capacity: self.capacity.load(Ordering::Acquire),
                })?;
        let capacity = self.capacity.load(Ordering::Acquire);
        let permits =
            u32::try_from(frame_bytes).map_err(|_| OutboundSendError::FrameExceedsQueue {
                frame_bytes,
                capacity,
            })?;
        if frame_bytes > capacity {
            return Err(OutboundSendError::FrameExceedsQueue {
                frame_bytes,
                capacity,
            });
        }

        let control_deadline = tokio::time::Instant::now() + control_timeout;
        let acquire = Arc::clone(&self.budget).acquire_many_owned(permits);
        let permit = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(OutboundSendError::Cancelled),
                permit = acquire => permit.map_err(|_| OutboundSendError::Closed)?,
            }
        } else {
            tokio::time::timeout_at(control_deadline, acquire)
                .await
                .map_err(|_| OutboundSendError::TimedOut)?
                .map_err(|_| OutboundSendError::Closed)?
        };
        let frame = OutboundFrame {
            bytes,
            _byte_permit: permit,
        };
        if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(OutboundSendError::Cancelled),
                result = self.frames.send(frame) => result.map_err(|_| OutboundSendError::Closed),
            }
        } else {
            tokio::time::timeout_at(control_deadline, self.frames.send(frame))
                .await
                .map_err(|_| OutboundSendError::TimedOut)?
                .map_err(|_| OutboundSendError::Closed)
        }
    }
}

fn output_queue_bytes(max_frame_bytes: usize) -> Result<usize> {
    let required = max_frame_bytes
        .checked_add(1)
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| anyhow!("configured server frame limit is too large"))?
        .max(OUTPUT_QUEUE_MIN_BYTES);
    if required > Semaphore::MAX_PERMITS {
        return Err(anyhow!(
            "configured server frame limit requires an outbound queue larger than the runtime supports"
        ));
    }
    Ok(required)
}

/// A persistent decoder keeps consumed partial bytes across select cancellation.
#[derive(Default)]
struct FrameBuffer {
    bytes: Vec<u8>,
    oversized: bool,
}

impl FrameBuffer {
    async fn read<R>(&mut self, reader: &mut R, max_bytes: usize) -> std::io::Result<FrameRead>
    where
        R: AsyncBufRead + Unpin,
    {
        loop {
            let available = reader.fill_buf().await?;
            let eof = available.is_empty();
            let end = available.iter().position(|b| *b == b'\n');
            let take = end.map_or(available.len(), |n| n + 1);
            if !self.oversized {
                if self.bytes.len().saturating_add(take) > max_bytes.saturating_add(2) {
                    self.oversized = true;
                    self.bytes.clear();
                } else {
                    self.bytes.extend_from_slice(&available[..take]);
                }
            }
            reader.consume(take);
            if end.is_some() || eof {
                if std::mem::take(&mut self.oversized) {
                    return Ok(FrameRead::TooLarge);
                }
                if eof && self.bytes.is_empty() {
                    return Ok(FrameRead::Eof);
                }
                let mut bytes = std::mem::take(&mut self.bytes);
                while matches!(bytes.last(), Some(b'\n' | b'\r')) {
                    bytes.pop();
                }
                return Ok(if bytes.len() > max_bytes {
                    FrameRead::TooLarge
                } else {
                    FrameRead::Frame(bytes)
                });
            }
        }
    }
}

#[cfg(test)]
async fn read_bounded_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<FrameRead> {
    FrameBuffer::default().read(reader, max_bytes).await
}

/// A persistent advisory lock closes the stale-socket unlink/bind race.
struct SocketLock(std::fs::File);
impl SocketLock {
    fn acquire(socket: &Path) -> Result<Self> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(socket.with_extension("lock"))?;
        // SAFETY: flock operates on this owned, live file descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(anyhow!("SCV daemon already owns this socket"));
        }
        Ok(Self(file))
    }
}
impl Drop for SocketLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: the descriptor remains live until this drop returns.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn server_frame_limit(session: &Option<Session>) -> usize {
    session.as_ref().map_or_else(
        || Config::default().protocol.max_server_frame_bytes,
        |value| value.config.protocol.max_server_frame_bytes,
    )
}

struct Session {
    id: String,
    workspace: PathBuf,
    config: Config,
    runtime: Arc<AgentRuntime>,
    history: Arc<Mutex<Vec<Message>>>,
    seq: Arc<AtomicU64>,
    queue: Arc<Mutex<VecDeque<QueueEntry>>>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    /// Background agent jobs, shared with the session's tools.
    background: Option<Arc<BackgroundJobs>>,
}

impl Session {
    async fn enqueue(
        &self,
        prompt: String,
        submitter: String,
    ) -> std::result::Result<QueueEntry, &'static str> {
        let entry = QueueEntry {
            queue_id: Uuid::new_v4().to_string(),
            revision: 1,
            prompt,
            submitter,
        };
        let mut queue = self.queue.lock().await;
        let bytes: usize = queue.iter().map(|item| item.prompt.len()).sum();
        if queue.len() >= MAX_QUEUE_ITEMS
            || bytes.saturating_add(entry.prompt.len()) > MAX_QUEUE_BYTES
        {
            return Err("queue_limit");
        }
        queue.push_back(entry.clone());
        Ok(entry)
    }

    async fn update_queue(
        &self,
        id: &str,
        revision: u64,
        prompt: String,
    ) -> std::result::Result<QueueEntry, &'static str> {
        let mut queue = self.queue.lock().await;
        let bytes: usize = queue.iter().map(|item| item.prompt.len()).sum();
        let entry = queue
            .iter_mut()
            .find(|entry| entry.queue_id == id)
            .ok_or("queue_not_found")?;
        if entry.revision != revision {
            return Err("queue_conflict");
        }
        if bytes
            .saturating_sub(entry.prompt.len())
            .saturating_add(prompt.len())
            > MAX_QUEUE_BYTES
        {
            return Err("queue_limit");
        }
        entry.prompt = prompt;
        entry.revision += 1;
        Ok(entry.clone())
    }

    async fn move_queue(
        &self,
        session_id: &str,
        id: &str,
        revision: u64,
        before: Option<String>,
    ) -> std::result::Result<(String, u64, usize), &'static str> {
        if self.id != session_id {
            return Err("session_not_found");
        }
        let mut queue = self.queue.lock().await;
        let index = queue
            .iter()
            .position(|entry| entry.queue_id == id)
            .ok_or("queue_not_found")?;
        if queue[index].revision != revision {
            return Err("queue_conflict");
        }
        // Validate the destination while the source is still present. This keeps
        // the operation atomic and handles a self move as a no-op reorder.
        let target_index = match before.as_deref() {
            Some(target) if target == id => return Ok((id.to_string(), revision, index)),
            Some(target) => Some(
                queue
                    .iter()
                    .position(|item| item.queue_id == target)
                    .ok_or("queue_not_found")?,
            ),
            None => None,
        };
        let mut entry = queue.remove(index).expect("queue index exists");
        let target = target_index.map_or(queue.len(), |target| {
            target.saturating_sub(usize::from(target > index))
        });
        let pos = target.min(queue.len());
        let id = entry.queue_id.clone();
        let rev = entry.revision + 1;
        entry.revision = rev;
        queue.insert(pos, entry);
        Ok((id, rev, pos))
    }

    async fn remove_queue(
        &self,
        session_id: &str,
        id: &str,
        revision: u64,
    ) -> std::result::Result<(String, u64), &'static str> {
        if self.id != session_id {
            return Err("session_not_found");
        }
        let mut queue = self.queue.lock().await;
        let index = queue
            .iter()
            .position(|entry| entry.queue_id == id)
            .ok_or("queue_not_found")?;
        if queue[index].revision != revision {
            return Err("queue_conflict");
        }
        let entry = queue.remove(index).expect("queue index exists");
        Ok((entry.queue_id, entry.revision))
    }
}

struct ActiveTurn {
    turn_id: String,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

struct AbortGuard(tokio::task::AbortHandle);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn shutdown_active_turn(mut active: ActiveTurn, grace: Duration) -> bool {
    active.cancellation.cancel();
    if tokio::time::timeout(grace, &mut active.task).await.is_ok() {
        true
    } else {
        active.task.abort();
        let _ = (&mut active.task).await;
        false
    }
}

async fn shutdown_writer(
    mut writer: JoinHandle<std::io::Result<()>>,
    grace: Duration,
) -> Result<()> {
    match tokio::time::timeout(grace, &mut writer).await {
        Ok(result) => {
            result.context("join protocol writer")??;
            Ok(())
        }
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            Err(anyhow!("protocol writer shutdown timed out"))
        }
    }
}

struct TurnDone {
    request_id: String,
    session_id: String,
    turn_id: String,
    origin: Option<TurnOrigin>,
    result: Result<scv_core::TurnOutcome, AgentError>,
}

/// What a connection needs to start a turn in its session.
struct TurnStarter<'a> {
    output: &'a OutboundSender,
    approvals: &'a Arc<ApprovalBroker>,
    done: &'a mpsc::Sender<TurnDone>,
    tasks: &'a TaskTracker,
    cancellation: &'a CancellationToken,
}

impl TurnStarter<'_> {
    /// Announce and run one turn of `current` for `prompt`.
    async fn start(
        &self,
        current: &Session,
        turn_id: String,
        request_id: String,
        prompt: String,
        origin: Option<TurnOrigin>,
    ) -> Result<ActiveTurn> {
        let cancellation = self.cancellation.child_token();
        send_event(
            self.output,
            ServerEvent::TurnStarted {
                request_id: request_id.clone(),
                session_id: current.id.clone(),
                turn_id: turn_id.clone(),
                seq: next_seq(&current.seq),
                origin: origin.clone(),
            },
            current.config.protocol.max_server_frame_bytes,
        )
        .await?;
        let meta = TurnMeta {
            request_id: request_id.clone(),
            session_id: current.id.clone(),
            turn_id: turn_id.clone(),
            seq: Arc::clone(&current.seq),
            max_server_frame: current.config.protocol.max_server_frame_bytes,
        };
        let sink: Arc<dyn EventSink> = Arc::new(ProtocolSink {
            meta: meta.clone(),
            output: self.output.clone(),
            cancellation: cancellation.clone(),
        });
        let gate: Arc<dyn ApprovalGate> = Arc::new(ProtocolApprovalGate {
            policy: current.config.tools.approval_policy,
            broker: Arc::clone(self.approvals),
            meta,
            output: self.output.clone(),
        });
        let runtime = Arc::clone(&current.runtime);
        let history = Arc::clone(&current.history);
        let done = self.done.clone();
        let session_id = current.id.clone();
        let task_turn = turn_id.clone();
        let task_cancel = cancellation.clone();
        let task = self.tasks.spawn(async move {
            let mut history = history.lock().await;
            let result = runtime
                .run_turn(&mut history, prompt, sink, gate, task_cancel)
                .await;
            let _ = done
                .send(TurnDone {
                    request_id,
                    session_id,
                    turn_id: task_turn,
                    origin,
                    result,
                })
                .await;
        });
        Ok(ActiveTurn {
            turn_id,
            cancellation,
            task,
        })
    }

    /// Start a turn reporting background jobs the model has not seen yet,
    /// or `None` when every finished job was already seen.
    async fn report_background(&self, current: &Session) -> Result<Option<ActiveTurn>> {
        let Some(jobs) = &current.background else {
            return Ok(None);
        };
        let reports = jobs.take_unreported();
        if reports.is_empty() {
            return Ok(None);
        }
        let origin = TurnOrigin {
            kind: ORIGIN_BACKGROUND.into(),
            jobs: reports.iter().map(|report| report.job.clone()).collect(),
        };
        let prompt = background::report_prompt(&reports);
        let request_id = format!("background:{}", Uuid::new_v4());
        self.start(
            current,
            Uuid::new_v4().to_string(),
            request_id,
            prompt,
            Some(origin),
        )
        .await
        .map(Some)
    }
}

/// The next background-job wake-up, or never without a receiver.
async fn recv_background(receiver: &mut Option<mpsc::UnboundedReceiver<()>>) -> Option<()> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

#[derive(Clone)]
struct TurnMeta {
    request_id: String,
    session_id: String,
    turn_id: String,
    seq: Arc<AtomicU64>,
    max_server_frame: usize,
}

fn next_seq(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed) + 1
}

/// What a client declared about itself in `session.start`.
#[derive(Debug, Default)]
struct SessionClient {
    /// The chat channel the session answers on, such as `WeChat`.
    channel: Option<String>,
    /// The client approves every approval request without asking anyone.
    auto_approve: bool,
}

fn valid_channel_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= scv_protocol::MAX_CHANNEL_NAME_BYTES
        && !name.chars().any(char::is_control)
}

/// The configured agents to offer: signed-out ones are left out when their
/// sign-in state is a local file SCV can check cheaply; the rest are offered
/// and fail with a sign-in hint if they turn out to be signed out.
fn offered_adapters(config: &Config) -> HashMap<String, scv_tools::AgentAdapterConfig> {
    let mut adapters = config.adapters();
    adapters.retain(|tool, adapter| {
        let descriptor = tool
            .strip_prefix("agent_")
            .and_then(scv_tools::adapters::adapter);
        match (
            descriptor.map(|descriptor| descriptor.status),
            &adapter.home,
        ) {
            (Some(scv_tools::adapters::Status::Stored(store)), Some(home)) => {
                !matches!(agents::stored_status(store, home), Ok((false, _)))
            }
            _ => true,
        }
    });
    adapters
}

/// Agent tools that delegate work, as opposed to observing or stopping jobs.
fn agent_tool_names(tools: &ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = tools
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .filter(|name| {
            name.starts_with("agent_")
                && !["agent_wait", "agent_status", "agent_cancel"].contains(&name.as_str())
        })
        .collect();
    names.sort();
    names
}

/// `delegation_depth` is the depth the client declared in `session.start`
/// (0 for a direct client); the session's delegated runs count from it.
async fn build_session(
    cwd: &str,
    overrides: ConfigOverrides,
    delegation_depth: u32,
    registry: &Arc<DelegationRegistry>,
    client: SessionClient,
) -> Result<(Session, Option<mpsc::UnboundedReceiver<()>>)> {
    let id = Uuid::new_v4().to_string();
    let workspace = std::fs::canonicalize(cwd).with_context(|| format!("resolve cwd {cwd}"))?;
    if !workspace.is_dir() {
        return Err(anyhow!("cwd is not a directory"));
    }
    let no_tools = overrides.no_tools;
    let config = Config::load(&workspace, overrides)?;
    if !no_tools {
        config.prepare_adapter_homes()?;
    }
    let provider_config = config.provider.clone();
    let api_key = provider_config.api_key.clone().or_else(|| {
        provider_config.api_key_env.as_deref().and_then(|name| std::env::var(name).ok())
    }).filter(|key| !key.trim().is_empty()).ok_or_else(|| anyhow!("provider credential is not configured; set provider.api_key or provider.api_key_env"))?;
    let skills = discover_skills(&workspace, &config, !no_tools)?;
    let listings = SkillListings {
        listing: skills.listing,
        project_listing: skills.project_listing,
    };
    let mut provider = OpenAiProvider::new(
        provider_config.model.clone(),
        provider_config.base_url.clone(),
        api_key,
        Duration::from_secs(provider_config.timeout_seconds),
        config.provider_limits(),
        provider_config.headers.clone(),
    )?;
    if !no_tools && config.hosted_web_search() {
        provider = provider.with_web_search();
    }
    let provider = Arc::new(provider);
    let (background, finished) = if !no_tools && config.agent.max_background > 0 {
        let (finished_tx, finished_rx) = mpsc::unbounded_channel();
        let unattended: Arc<dyn ApprovalGate> = Arc::new(UnattendedGate {
            policy: config.tools.approval_policy,
            client_approves_all: client.auto_approve,
        });
        (
            Some(Arc::new(
                BackgroundJobs::new(config.agent.max_background, Some(finished_tx))
                    .with_approvals(unattended),
            )),
            Some(finished_rx),
        )
    } else {
        (None, None)
    };
    let tools = if no_tools {
        Arc::new(ToolRegistry::default())
    } else {
        let mut tools = config.tools();
        tools.delegation = Some(DelegationContext {
            registry: Arc::clone(registry),
            session: id.clone(),
            depth: delegation_depth,
        });
        tools.background = background.clone();
        let mut registry = builtin_registry(
            tools,
            skills.map,
            skills.roots,
            config.skills.max_skill_bytes,
            offered_adapters(&config),
        )?;
        if let Some(web) = config.web_tools() {
            scv_tools::web::register(&mut registry, web)?;
        }
        Arc::new(registry)
    };
    let agents = agent_tool_names(&tools);
    let system_prompt = build_system_prompt(
        &workspace,
        &config,
        &listings,
        &PromptContext {
            agents: &agents,
            background: tools.get("agent_status").is_some(),
            channel: client.channel.as_deref(),
        },
    )?;
    let context = Arc::new(BudgetContextPolicy::new((&config.context).into())?);
    let runtime = Arc::new(AgentRuntime::new(
        provider,
        tools,
        context,
        config.core_agent(system_prompt),
        workspace.clone(),
    ));
    Ok((
        Session {
            id,
            workspace,
            config,
            runtime,
            history: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicU64::new(0)),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            background,
        },
        finished,
    ))
}

/// The skill listings a session's system prompt carries.
struct SkillListings {
    listing: String,
    project_listing: String,
}

/// What the system prompt tells the model about its situation.
struct PromptContext<'a> {
    /// Agent tools this session offers, such as `agent_codex`, sorted.
    agents: &'a [String],
    /// Whether agent calls can run in the background.
    background: bool,
    /// The chat channel the session answers on.
    channel: Option<&'a str>,
}

fn build_system_prompt(
    workspace: &Path,
    config: &Config,
    skills: &SkillListings,
    context: &PromptContext<'_>,
) -> Result<String> {
    let mut prompt = config.agent.system_prompt.clone();
    prompt.push_str(&format!(
        "\nCurrent working directory: {}\n",
        workspace.display()
    ));
    let agents_path = workspace.join("AGENTS.md");
    if agents_path.is_file() {
        let canonical = std::fs::canonicalize(&agents_path).context("resolve project AGENTS.md")?;
        if !canonical.starts_with(workspace) {
            return Err(anyhow!("project AGENTS.md escaped workspace"));
        }
        let (bytes, truncated) = read_prefix(&canonical, config.tools.max_read_bytes)
            .context("read project AGENTS.md")?;
        let instructions = std::str::from_utf8(&bytes).context("project AGENTS.md is not UTF-8")?;
        prompt.push_str("\n# Project instructions\n");
        prompt.push_str(instructions);
        if truncated {
            prompt.push_str("\n[AGENTS.md truncated by configured read limit]\n");
        }
    }
    if !skills.listing.is_empty() {
        prompt.push_str("\n# Available skills\n");
        prompt.push_str(&skills.listing);
        prompt.push_str("\nUse read_skill with a skill name when its workflow applies.\n");
    }
    if !skills.project_listing.is_empty() {
        prompt.push_str("\n# Project skills\n");
        prompt.push_str(
            "Projects in this workspace provide these skills to agents working in them:\n",
        );
        prompt.push_str(&skills.project_listing);
        match context.agents {
            [] => prompt.push_str("\nread_skill loads one for reference.\n"),
            agents => prompt.push_str(&format!(
                "\nTo use one, delegate with an agent tool such as {}, set its cwd to the \
                 skill's project, and name the skill in the prompt: that agent then loads the \
                 project's instructions and skills itself. read_skill loads a skill for \
                 reference.\n",
                agents
                    .iter()
                    .take(2)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
        }
    }
    if !context.agents.is_empty() {
        prompt.push_str(&delegation_guidance(config, context));
    }
    if let Some(channel) = context.channel {
        prompt.push_str(&format!(
            "\n# Chat channel\n\
             This conversation takes place on {channel}. The user reads your replies there as \
             chat messages, so keep them short and in plain text, without tables, headings, \
             or code blocks unless the user asks for them. Only the last message of each turn \
             reaches the user, and they never see your tool calls or their output, so put what \
             you did and what you found into that message in words.\n"
        ));
    }
    Ok(prompt)
}

/// How the main agent works with delegated agents. Written to explain why,
/// since the model follows guidance it understands more reliably.
fn delegation_guidance(config: &Config, context: &PromptContext<'_>) -> String {
    let named: Vec<String> = context
        .agents
        .iter()
        .map(|tool| format!("{tool} ({})", scv_tools::agent_choice::product(tool)))
        .collect();
    let mut text = format!(
        "\n# Delegating work\n\
         You can hand work to these agents: {}. Each tool's description says what that agent \
         offers.",
        named.join(", ")
    );
    let preferred: Vec<String> = config
        .agent
        .prefer
        .iter()
        .map(|agent| format!("agent_{agent}"))
        .filter(|tool| context.agents.contains(tool))
        .collect();
    if !preferred.is_empty() {
        text.push_str(&format!(
            " The user prefers {}, in that order; choose another when the work needs \
             something only it offers, or when a preferred one is unavailable.",
            preferred.join(", ")
        ));
    }
    if context.background {
        text.push_str(
            "\n\nStay available to the user: while one of your turns runs, they cannot reach \
             you. Handle quick things yourself, such as short reads, lookups, status checks, \
             and answers you can give in a step or two. Hand real work to an agent with \
             background set to true: changes to code or files, multi-step investigation, \
             builds, tests, releases, and anything else likely to take more than about a \
             minute. Then reply right away with what you started and its job handle.\n\n\
             The agent does not see this conversation, so write a brief that stands on its \
             own: the goal, the project directory (cwd), what you already know, constraints, \
             and what to report back.\n\n\
             When a job finishes, SCV starts a turn with an [SCV background report]; tell the \
             user what happened and the key result. agent_status shows how jobs are going, \
             and agent_cancel stops one the user no longer wants. agent_wait, foreground \
             agent calls, and long bash commands keep the user waiting, so use them only for \
             results you need within this turn that arrive quickly.\n",
        );
    } else {
        text.push_str(
            "\n\nHand substantial work to an agent rather than doing it step by step with \
             bash. The agent does not see this conversation, so write a brief that stands on \
             its own: the goal, the project directory (cwd), what you already know, \
             constraints, and what to report back.\n",
        );
    }
    // A refusal is the agent's own judgement, so it goes back to the user;
    // the user may still choose another agent, whose policies then apply.
    text.push_str(
        "\nIf an agent declines a request, tell the user what it said; don't pass the request \
         to another agent on your own. If the user then asks for a specific agent, use it.\n",
    );
    text
}

/// Skills found at session start: the names `read_skill` serves, the roots it
/// revalidates them against, and their system-prompt listings.
struct DiscoveredSkills {
    map: SkillMap,
    roots: Vec<PathBuf>,
    listing: String,
    project_listing: String,
}

/// Agent-native skill directories, relative to a project, that Codex and
/// Claude Code load from their working directory.
const PROJECT_SKILL_DIRS: [&str; 2] = [".agents/skills", ".claude/skills"];
/// Workspace entries and child projects inspected for project skills, so a
/// large workspace such as a home directory costs bounded lookups.
const MAX_WORKSPACE_ENTRIES: usize = 4096;
const MAX_SKILL_PROJECTS: usize = 256;
/// Bytes read to find a project skill's description, and its listed length.
const PROJECT_SKILL_HEADER_BYTES: usize = 16 * 1024;
const MAX_PROJECT_SKILL_DESCRIPTION: usize = 400;

fn discover_skills(workspace: &Path, config: &Config, tools: bool) -> Result<DiscoveredSkills> {
    let mut skills = SkillMap::new();
    let mut roots = Vec::new();
    let project_root = workspace.join(&config.skills.project_dir);
    for (root, must_be_workspace) in [(&project_root, true), (&config.skills.user_dir, false)] {
        if !root.is_dir() {
            continue;
        }
        let canonical = std::fs::canonicalize(root)
            .with_context(|| format!("resolve skill root {}", root.display()))?;
        if must_be_workspace && !canonical.starts_with(workspace) {
            return Err(anyhow!("project skill root escaped workspace"));
        }
        roots.push(canonical.clone());
        let mut entries: Vec<_> = std::fs::read_dir(&canonical)
            .with_context(|| format!("read skill root {}", canonical.display()))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if skills.len() >= config.skills.max_skills {
                break;
            }
            let path = entry.path().join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            let canonical_file = std::fs::canonicalize(&path)
                .with_context(|| format!("resolve skill {}", path.display()))?;
            if !canonical_file.starts_with(&canonical) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            skills.entry(name).or_insert(canonical_file);
        }
    }
    let mut names: Vec<_> = skills.keys().cloned().collect();
    names.sort();
    let mut listing = String::new();
    for name in names {
        let path = &skills[&name];
        let bytes = read_prefix(path, config.skills.max_skill_bytes)
            .map(|(bytes, _)| bytes)
            .unwrap_or_default();
        let content = String::from_utf8_lossy(&bytes);
        let description = skill_description(&content);
        listing.push_str(&format!("- {name}: {description}\n"));
    }
    // Project skills are only actionable by delegating, so tool-free sessions
    // neither list them nor learn the workspace's project names.
    let project_listing = if tools && config.skills.scan_projects {
        discover_project_skills(workspace, config, &mut skills, &mut roots)
    } else {
        String::new()
    };
    Ok(DiscoveredSkills {
        map: skills,
        roots,
        listing,
        project_listing,
    })
}

/// List the agent skills of the workspace and its immediate, non-hidden child
/// projects. A child's skills are named `<project>:<skill>`. Everything
/// resolves inside the workspace, SKILL.md files that resolve to the same file
/// (such as a `.claude/skills` link to `.agents/skills`) count once, and
/// unreadable entries are skipped so one broken project cannot stop a session.
fn discover_project_skills(
    workspace: &Path,
    config: &Config,
    skills: &mut SkillMap,
    roots: &mut Vec<PathBuf>,
) -> String {
    let mut projects = vec![(None, workspace.to_path_buf())];
    let mut names: Vec<_> = std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take(MAX_WORKSPACE_ENTRIES)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    for name in names {
        if projects.len() > MAX_SKILL_PROJECTS {
            break;
        }
        let Ok(directory) = std::fs::canonicalize(workspace.join(&name)) else {
            continue;
        };
        if directory.is_dir()
            && directory.starts_with(workspace)
            && !projects.iter().any(|(_, seen)| seen == &directory)
        {
            projects.push((Some(name), directory));
        }
    }
    let mut seen_files = std::collections::HashSet::new();
    let mut listing = String::new();
    'projects: for (project, directory) in projects {
        let project_roots: Vec<PathBuf> = PROJECT_SKILL_DIRS
            .iter()
            .filter_map(|relative| std::fs::canonicalize(directory.join(relative)).ok())
            .filter(|root| root.is_dir() && root.starts_with(workspace))
            .collect();
        for root in &project_roots {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        for root in &project_roots {
            let mut entries: Vec<_> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .take(MAX_WORKSPACE_ENTRIES)
                .collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                if skills.len() >= config.skills.max_skills {
                    break 'projects;
                }
                let Ok(file) = std::fs::canonicalize(entry.path().join("SKILL.md")) else {
                    continue;
                };
                if !file.is_file()
                    || !project_roots.iter().any(|root| file.starts_with(root))
                    || !seen_files.insert(file.clone())
                {
                    continue;
                }
                let skill = entry.file_name().to_string_lossy().into_owned();
                let (name, location) = match &project {
                    Some(project) => (format!("{project}:{skill}"), format!("project {project}")),
                    None => (skill, "workspace root".to_owned()),
                };
                // SCV's own and the user's skills keep their names.
                if skills.contains_key(&name) {
                    continue;
                }
                let header = read_prefix(
                    &file,
                    config
                        .skills
                        .max_skill_bytes
                        .min(PROJECT_SKILL_HEADER_BYTES),
                )
                .map(|(bytes, _)| bytes)
                .unwrap_or_default();
                let description: String = skill_description(&String::from_utf8_lossy(&header))
                    .chars()
                    .take(MAX_PROJECT_SKILL_DESCRIPTION)
                    .collect();
                listing.push_str(&format!("- {name} ({location}): {description}\n"));
                skills.insert(name, file);
            }
        }
    }
    listing
}

fn read_prefix(path: &Path, max_bytes: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    file.take(
        u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    Ok((bytes, truncated))
}

fn skill_description(content: &str) -> String {
    if let Some(frontmatter) = content.strip_prefix("---\n")
        && let Some((header, _)) = frontmatter.split_once("\n---")
    {
        for line in header.lines() {
            if let Some(description) = line.strip_prefix("description:") {
                return description.trim().trim_matches('"').to_owned();
            }
        }
    }
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("No description provided")
        .chars()
        .take(240)
        .collect()
}

/// Cut progress text to `MAX_PROGRESS_EVENT_BYTES` on a character boundary.
fn bounded_progress(mut text: String) -> String {
    let limit = scv_core::MAX_PROGRESS_EVENT_BYTES;
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

struct ProtocolSink {
    meta: TurnMeta,
    output: OutboundSender,
    cancellation: CancellationToken,
}

#[async_trait]
impl EventSink for ProtocolSink {
    async fn emit(&self, event: CoreEvent) -> Result<(), AgentError> {
        let seq = next_seq(&self.meta.seq);
        let event = match event {
            CoreEvent::AssistantDelta { content } => ServerEvent::AssistantDelta {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                content,
            },
            CoreEvent::AssistantCompleted { content } => ServerEvent::AssistantCompleted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                content,
            },
            CoreEvent::ToolProposed {
                call_id,
                name,
                arguments,
            } => ServerEvent::ToolProposed {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
                arguments,
            },
            CoreEvent::ToolStarted { call_id, name } => ServerEvent::ToolStarted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
            },
            CoreEvent::ToolProgress { call_id, text } => ServerEvent::ToolProgress {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                // Tools report through a bounded sink; bound again here so a
                // custom tool can never grow a frame past the documented size.
                text: bounded_progress(text),
            },
            CoreEvent::ToolCompleted {
                call_id,
                name,
                output,
            } => ServerEvent::ToolCompleted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                call_id,
                name,
                success: !output.is_error,
                output: output.content,
                truncated: output.truncated,
            },
            CoreEvent::ContextCompacted {
                before_tokens,
                after_tokens,
                removed_messages,
            } => ServerEvent::ContextCompacted {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                turn_id: self.meta.turn_id.clone(),
                seq,
                before_tokens,
                after_tokens,
                removed_messages,
            },
            CoreEvent::SessionTrimmed {
                removed_messages,
                history_bytes,
            } => ServerEvent::SessionTrimmed {
                request_id: self.meta.request_id.clone(),
                session_id: self.meta.session_id.clone(),
                seq,
                removed_messages,
                history_bytes,
            },
        };
        send_turn_event(
            &self.output,
            event,
            self.meta.max_server_frame,
            &self.cancellation,
        )
        .await
    }
}

#[derive(Default)]
struct ApprovalBroker {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl ApprovalBroker {
    async fn insert(&self, id: String, sender: oneshot::Sender<bool>) {
        self.pending.lock().await.insert(id, sender);
    }

    async fn remove(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }

    async fn resolve(&self, id: &str, approved: bool) -> bool {
        let sender = self.pending.lock().await.remove(id);
        sender.is_some_and(|sender| sender.send(approved).is_ok())
    }
}

/// The decision `policy` makes for `risk` on its own, or `None` when it
/// asks the client.
fn policy_decision(policy: ApprovalPolicy, risk: ToolRisk) -> Option<bool> {
    match policy {
        ApprovalPolicy::OnRisk if risk == ToolRisk::ReadOnly => Some(true),
        ApprovalPolicy::Never => Some(risk == ToolRisk::ReadOnly),
        ApprovalPolicy::Always | ApprovalPolicy::OnRisk => None,
    }
}

struct ProtocolApprovalGate {
    policy: ApprovalPolicy,
    broker: Arc<ApprovalBroker>,
    meta: TurnMeta,
    output: OutboundSender,
}

/// Decides a background job's nested approval requests, which outlive the
/// turn that could carry them to the client. Each gets the answer the
/// session would give without asking a person: the policy's own decision,
/// else the client's declared blanket answer (`auto_approve`), else a denial.
/// It never grants more than the same request would get in the foreground.
struct UnattendedGate {
    policy: ApprovalPolicy,
    client_approves_all: bool,
}

#[async_trait]
impl ApprovalGate for UnattendedGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        Ok(policy_decision(self.policy, request.risk).unwrap_or(self.client_approves_all))
    }
}

#[async_trait]
impl ApprovalGate for ProtocolApprovalGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        if let Some(decision) = policy_decision(self.policy, request.risk) {
            return Ok(decision);
        }
        let approval_id = Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        self.broker.insert(approval_id.clone(), sender).await;
        let event = ServerEvent::ApprovalRequested {
            request_id: self.meta.request_id.clone(),
            session_id: self.meta.session_id.clone(),
            turn_id: self.meta.turn_id.clone(),
            seq: next_seq(&self.meta.seq),
            approval_id: approval_id.clone(),
            call_id: request.call_id,
            name: request.name,
            risk: request.risk.as_str().into(),
            cwd: request.cwd.display().to_string(),
            summary: request.summary,
        };
        if let Err(error) = send_turn_event(
            &self.output,
            event,
            self.meta.max_server_frame,
            &cancellation,
        )
        .await
        {
            self.broker.remove(&approval_id).await;
            return Err(error);
        }
        tokio::select! {
            result = receiver => result.map_err(|_| AgentError::Cancelled),
            _ = cancellation.cancelled() => {
                self.broker.remove(&approval_id).await;
                Err(AgentError::Cancelled)
            }
        }
    }
}

async fn send_event(output: &OutboundSender, event: ServerEvent, max_bytes: usize) -> Result<()> {
    let bytes = encode_event(&event, max_bytes)?;
    output.send(bytes, None).await.map_err(anyhow::Error::new)
}

async fn send_turn_event(
    output: &OutboundSender,
    event: ServerEvent,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<(), AgentError> {
    let bytes = encode_event(&event, max_bytes)
        .map_err(|error| AgentError::ResponseLimit(error.to_string()))?;
    match output.send(bytes, Some(cancellation)).await {
        Ok(()) => Ok(()),
        Err(OutboundSendError::Cancelled) => Err(AgentError::Cancelled),
        Err(error) => Err(AgentError::Internal(error.to_string())),
    }
}

fn encode_event(event: &ServerEvent, max_bytes: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(event).context("serialize protocol event")?;
    if bytes.len() > max_bytes {
        return Err(anyhow!("server event exceeds configured frame limit"));
    }
    Ok(bytes)
}

async fn send_error(
    output: &OutboundSender,
    request_id: &str,
    code: &str,
    message: &str,
    fatal: bool,
    max_bytes: usize,
) -> Result<()> {
    send_event(
        output,
        ServerEvent::Error {
            request_id: (!request_id.is_empty()).then(|| request_id.to_owned()),
            code: code.into(),
            message: message.into(),
            fatal,
        },
        max_bytes,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        io::Cursor,
        sync::atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn delegation_records_live_where_the_layout_says() {
        let home = std::path::Path::new("/tmp/scv-layout-check");
        let registry = DelegationRegistry::new(home);
        let layout = scv_client::Layout::new(home);
        assert_eq!(registry.record_dir(), layout.delegations());
        assert_eq!(registry.conversation_dir(), layout.conversations());
    }

    /// A delegation registry in a private temporary instance home.
    fn test_registry() -> Arc<DelegationRegistry> {
        let home = tempfile::tempdir().unwrap().keep();
        Arc::new(DelegationRegistry::new(&home))
    }

    use super::*;

    #[test]
    fn clients_and_tools_agree_on_the_depth_variable() {
        assert_eq!(
            scv_client::DELEGATION_DEPTH_VARIABLE,
            scv_tools::delegation::DEPTH_VARIABLE
        );
    }

    #[test]
    fn progress_text_is_bounded_on_a_character_boundary() {
        let text = "é".repeat(400);
        let bounded = bounded_progress(text);
        assert!(bounded.len() <= scv_core::MAX_PROGRESS_EVENT_BYTES);
        assert!(bounded.chars().all(|character| character == 'é'));
        assert_eq!(bounded_progress("short".into()), "short");
    }

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn nonreading_management_client_does_not_hold_component_lock() {
        let (mut input, server_input) = tokio::io::duplex(65536);
        let (server_output, _blocked_output) = tokio::io::duplex(1);
        let tasks = TaskTracker::new();
        let components = Arc::new(Mutex::new(components::Components::new(
            PathBuf::from("/unused.sock"),
            PathBuf::from("/"),
        )));
        let cancel = CancellationToken::new();
        let handler = tokio::spawn(run_managed(
            server_input,
            server_output,
            ConfigOverrides::default(),
            Some(components.clone()),
            test_registry(),
            cancel.clone(),
            tasks.clone(),
        ));
        input.write_all(b"{\"type\":\"initialize\",\"request_id\":\"init\",\"protocol_version\":2,\"client\":{\"name\":\"test\",\"version\":\"0\"}}\n").await.unwrap();
        for _ in 0..300 {
            input.write_all(b"{\"type\":\"daemon.control\",\"request_id\":\"s\",\"command\":{\"action\":\"status\"}}\n").await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status = tokio::time::timeout(Duration::from_millis(100), async {
            components.lock().await.status()
        })
        .await
        .unwrap();
        assert_eq!(status.pid, std::process::id());
        cancel.cancel();
        handler.abort();
        let _ = handler.await;
        tasks.close();
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn forced_connection_abort_drops_and_joins_writer_descendants() {
        let (mut input, server_input) = tokio::io::duplex(512);
        let (server_output, _blocked_output) = tokio::io::duplex(1);
        let tasks = TaskTracker::new();
        let handler = tokio::spawn(run_managed(
            server_input,
            server_output,
            ConfigOverrides::default(),
            None,
            test_registry(),
            CancellationToken::new(),
            tasks.clone(),
        ));
        input.write_all(b"{\"type\":\"initialize\",\"request_id\":\"init\",\"protocol_version\":2,\"client\":{\"name\":\"test\",\"version\":\"0\"}}\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while tasks.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handler.abort();
        let _ = handler.await;
        tasks.close();
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn forced_handler_abort_cancels_and_joins_active_turn() {
        let tasks = TaskTracker::new();
        let cancellation = CancellationToken::new();
        let child_cancel = cancellation.child_token();
        let observed_cancel = child_cancel.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tasks.spawn({
            let dropped = dropped.clone();
            async move {
                let _guard = DropSignal(dropped);
                let _ = ready_tx.send(());
                pending::<()>().await;
            }
        });
        ready_rx.await.unwrap();
        let (owned_tx, owned_rx) = oneshot::channel();
        let handler = tokio::spawn(async move {
            let _active = ActiveTurn {
                turn_id: "test".into(),
                cancellation: child_cancel,
                task,
            };
            let _ = owned_tx.send(());
            pending::<()>().await;
        });
        owned_rx.await.unwrap();
        handler.abort();
        let _ = handler.await;
        tasks.close();
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .unwrap();
        assert!(observed_cancel.is_cancelled());
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn frame_buffer_preserves_partial_and_discard_state_across_cancellation() {
        let (mut input, output) = tokio::io::duplex(64);
        let mut reader = BufReader::new(output);
        let mut frames = FrameBuffer::default();
        input.write_all(b"12").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), frames.read(&mut reader, 4))
                .await
                .is_err()
        );
        input.write_all(b"34\n").await.unwrap();
        assert!(
            matches!(frames.read(&mut reader, 4).await.unwrap(), FrameRead::Frame(value) if value == b"1234")
        );
        input.write_all(b"123456789").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), frames.read(&mut reader, 4))
                .await
                .is_err()
        );
        input.write_all(b"\n{}\n").await.unwrap();
        assert!(matches!(
            frames.read(&mut reader, 4).await.unwrap(),
            FrameRead::TooLarge
        ));
        assert!(
            matches!(frames.read(&mut reader, 4).await.unwrap(), FrameRead::Frame(value) if value == b"{}")
        );
    }

    #[tokio::test]
    async fn bounded_reader_discards_an_oversized_line() {
        let input = format!("{}\n{{}}\n", "x".repeat(10));
        let mut reader = BufReader::new(Cursor::new(input.into_bytes()));
        assert!(matches!(
            read_bounded_frame(&mut reader, 4).await.unwrap(),
            FrameRead::TooLarge
        ));
        match read_bounded_frame(&mut reader, 4).await.unwrap() {
            FrameRead::Frame(frame) => assert_eq!(frame, b"{}"),
            _ => panic!("expected the frame following the oversized line"),
        }
    }

    #[tokio::test]
    async fn bounded_reader_accepts_exact_crlf_limit() {
        let mut reader = BufReader::new(Cursor::new(b"1234\r\n".to_vec()));
        match read_bounded_frame(&mut reader, 4).await.unwrap() {
            FrameRead::Frame(frame) => assert_eq!(frame, b"1234"),
            _ => panic!("expected an exact-limit frame"),
        }
    }

    #[tokio::test]
    async fn outbound_byte_backpressure_is_cancellation_aware() {
        let (output, mut receiver) = outbound_channel(5);
        output.send(vec![0; 4], None).await.unwrap();

        let cancellation = CancellationToken::new();
        let blocked = tokio::spawn({
            let output = output.clone();
            let cancellation = cancellation.clone();
            async move { output.send(vec![1; 4], Some(&cancellation)).await }
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        cancellation.cancel();
        assert_eq!(blocked.await.unwrap(), Err(OutboundSendError::Cancelled));

        drop(receiver.recv().await.unwrap());
        output.send(vec![2; 4], None).await.unwrap();
    }

    #[tokio::test]
    async fn outbound_control_send_times_out_under_byte_backpressure() {
        let (output, _receiver) = outbound_channel(5);
        output.send(vec![0; 4], None).await.unwrap();
        let result = output
            .send_with_timeout(vec![1; 4], None, Duration::from_millis(10))
            .await;
        assert_eq!(result, Err(OutboundSendError::TimedOut));
    }

    #[tokio::test]
    async fn active_turn_shutdown_aborts_after_grace_period() {
        let cancellation = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let task = tokio::spawn({
            let dropped = Arc::clone(&dropped);
            async move {
                let _signal = DropSignal(dropped);
                let _ = started_tx.send(());
                pending::<()>().await;
            }
        });
        started_rx.await.unwrap();

        let graceful = shutdown_active_turn(
            ActiveTurn {
                turn_id: "turn".into(),
                cancellation,
                task,
            },
            Duration::from_millis(10),
        )
        .await;

        assert!(!graceful);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn writer_shutdown_aborts_after_grace_period() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let writer = tokio::spawn({
            let dropped = Arc::clone(&dropped);
            async move {
                let _signal = DropSignal(dropped);
                let _ = started_tx.send(());
                pending::<std::io::Result<()>>().await
            }
        });
        started_rx.await.unwrap();

        let result = shutdown_writer(writer, Duration::from_millis(10)).await;

        assert!(result.is_err());
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn workspace_projects_list_their_agent_skills_for_delegation() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        let write_skill = |directory: &Path, description: &str| {
            std::fs::create_dir_all(directory).unwrap();
            std::fs::write(
                directory.join("SKILL.md"),
                format!(
                    "---\nname: skill\ndescription: {description}\n---\nBody of {description}\n"
                ),
            )
            .unwrap();
        };
        write_skill(
            &workspace.join("scv/.agents/skills/feature-flow"),
            "Land SCV",
        );
        std::fs::create_dir_all(workspace.join("scv/.claude/skills")).unwrap();
        symlink(
            "../../.agents/skills/feature-flow",
            workspace.join("scv/.claude/skills/feature-flow"),
        )
        .unwrap();
        write_skill(
            &workspace.join("web/.claude/skills/deploy"),
            "Deploy the site",
        );
        write_skill(&workspace.join(".agents/skills/triage"), "Root triage");
        write_skill(&workspace.join(".agents/skills/notes"), "Root notes");
        write_skill(&workspace.join(".scv/skills/triage"), "SCV triage");
        write_skill(&workspace.join(".hidden/.agents/skills/secret"), "Hidden");
        write_skill(&outside.join(".agents/skills/evil"), "Outside");
        symlink(&outside, workspace.join("escape")).unwrap();
        std::fs::create_dir_all(workspace.join("rogue/.agents")).unwrap();
        symlink(
            outside.join(".agents/skills"),
            workspace.join("rogue/.agents/skills"),
        )
        .unwrap();
        std::fs::create_dir_all(workspace.join("sneaky/.agents/skills/leak")).unwrap();
        symlink(
            outside.join(".agents/skills/evil/SKILL.md"),
            workspace.join("sneaky/.agents/skills/leak/SKILL.md"),
        )
        .unwrap();
        std::fs::write(workspace.join("file"), "not a project").unwrap();
        let mut config = Config::default();
        config.skills.user_dir = workspace.join("no-user-skills");

        let skills = discover_skills(&workspace, &config, true).unwrap();
        let mut names: Vec<_> = skills.map.keys().cloned().collect();
        names.sort();
        assert_eq!(names, ["notes", "scv:feature-flow", "triage", "web:deploy"]);
        assert_eq!(
            skills.map["triage"],
            workspace.join(".scv/skills/triage/SKILL.md")
        );
        assert_eq!(
            skills.project_listing,
            "- notes (workspace root): Root notes\n\
             - scv:feature-flow (project scv): Land SCV\n\
             - web:deploy (project web): Deploy the site\n"
        );
        let listings = SkillListings {
            listing: skills.listing.clone(),
            project_listing: skills.project_listing.clone(),
        };
        let agents = ["agent_claude".to_owned(), "agent_pi".to_owned()];
        let prompt = build_system_prompt(
            &workspace,
            &config,
            &listings,
            &PromptContext {
                agents: &agents,
                background: true,
                channel: None,
            },
        )
        .unwrap();
        assert!(prompt.contains("# Project skills"));
        assert!(
            prompt.contains("such as agent_claude or agent_pi, set its cwd to the skill's project")
        );
        // Only agents this session offers are named.
        assert!(!prompt.contains("agent_codex"), "{prompt}");
        let without_agents = build_system_prompt(
            &workspace,
            &config,
            &listings,
            &PromptContext {
                agents: &[],
                background: false,
                channel: None,
            },
        )
        .unwrap();
        assert!(
            !without_agents.contains("delegate with"),
            "{without_agents}"
        );
        assert!(without_agents.contains("read_skill loads one for reference"));

        let registry = builtin_registry(
            config.tools(),
            skills.map,
            skills.roots,
            config.skills.max_skill_bytes,
            HashMap::new(),
        )
        .unwrap();
        let read_skill = registry.get("read_skill").unwrap();
        let loaded = read_skill
            .execute(
                serde_json::json!({"name":"scv:feature-flow"}),
                scv_core::ToolContext::new(workspace.clone(), CancellationToken::new()),
            )
            .await
            .unwrap();
        assert!(loaded.content.contains("Body of Land SCV"));

        let tool_free = discover_skills(&workspace, &config, false).unwrap();
        assert!(tool_free.project_listing.is_empty());
        assert!(!tool_free.map.contains_key("scv:feature-flow"));
        config.skills.scan_projects = false;
        let disabled = discover_skills(&workspace, &config, true).unwrap();
        assert!(disabled.project_listing.is_empty());
        config.skills.scan_projects = true;
        config.skills.max_skills = 3;
        let capped = discover_skills(&workspace, &config, true).unwrap();
        assert_eq!(capped.map.len(), 3);
        assert!(capped.map.contains_key("scv:feature-flow"));
        assert!(!capped.map.contains_key("web:deploy"));
    }

    fn prompt_for(config: &Config, context: &PromptContext<'_>) -> String {
        let workspace = tempfile::tempdir().unwrap();
        let listings = SkillListings {
            listing: String::new(),
            project_listing: String::new(),
        };
        build_system_prompt(workspace.path(), config, &listings, context).unwrap()
    }

    #[test]
    fn the_prompt_teaches_delegate_first_only_when_agents_can_run_in_the_background() {
        let mut config = Config::default();
        config.agent.prefer = vec!["pi".into(), "codex".into(), "grok".into()];
        let agents = ["agent_codex".to_owned(), "agent_grok".to_owned()];
        let prompt = prompt_for(
            &config,
            &PromptContext {
                agents: &agents,
                background: true,
                channel: None,
            },
        );
        assert!(prompt.starts_with(&config.agent.system_prompt), "{prompt}");
        assert!(
            prompt.contains("agent_codex (Codex), agent_grok (Grok Build)"),
            "{prompt}"
        );
        // Preferences name only offered agents, in the user's order.
        assert!(
            prompt.contains("The user prefers agent_codex, agent_grok, in that order"),
            "{prompt}"
        );
        assert!(prompt.contains("background set to true"), "{prompt}");
        assert!(prompt.contains("job handle"), "{prompt}");
        assert!(prompt.contains("agent_cancel"), "{prompt}");
        assert!(prompt.contains("[SCV background report]"), "{prompt}");
        assert!(!prompt.contains("# Chat channel"), "{prompt}");
        // A declined request goes back to the user, who may pick an agent.
        assert!(
            prompt.contains(
                "If an agent declines a request, tell the user what it said; don't pass the \
                 request to another agent on your own. If the user then asks for a specific \
                 agent, use it."
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains("a preferred one is unavailable"),
            "{prompt}"
        );
        // Calm guidance: no shouted rules.
        for loud in ["CRITICAL", "MUST", "IMPORTANT", "NEVER"] {
            assert!(!prompt.contains(loud), "{loud} in {prompt}");
        }

        let foreground = prompt_for(
            &Config::default(),
            &PromptContext {
                agents: &agents,
                background: false,
                channel: None,
            },
        );
        assert!(foreground.contains("Hand substantial work to an agent"));
        assert!(foreground.contains("If an agent declines a request"));
        assert!(!foreground.contains("background set to true"));
        assert!(!foreground.contains("prefers"));

        let tool_free = prompt_for(
            &Config::default(),
            &PromptContext {
                agents: &[],
                background: false,
                channel: None,
            },
        );
        assert!(!tool_free.contains("# Delegating work"), "{tool_free}");
    }

    #[test]
    fn chat_sessions_are_told_their_channel_and_how_replies_are_read() {
        let agents = ["agent_claude".to_owned()];
        let owner = prompt_for(
            &Config::default(),
            &PromptContext {
                agents: &agents,
                background: true,
                channel: Some("WeChat"),
            },
        );
        assert!(owner.contains("# Chat channel"), "{owner}");
        assert!(owner.contains("takes place on WeChat"), "{owner}");
        assert!(owner.contains("plain text"), "{owner}");
        assert!(owner.contains("never see your tool calls"), "{owner}");
        assert!(owner.contains("# Delegating work"), "{owner}");
        // A tool-free chat session still learns how its replies are read.
        let guest = prompt_for(
            &Config::default(),
            &PromptContext {
                agents: &[],
                background: false,
                channel: Some("Feishu"),
            },
        );
        assert!(guest.contains("takes place on Feishu"), "{guest}");
        assert!(!guest.contains("# Delegating work"), "{guest}");
        assert!(valid_channel_name("Lark"));
        for bad in [
            "",
            "  ",
            "We\nChat",
            &"x".repeat(scv_protocol::MAX_CHANNEL_NAME_BYTES + 1),
        ] {
            assert!(!valid_channel_name(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn background_requests_get_only_the_unattended_answer() {
        let request = |risk| ApprovalRequest {
            call_id: "job-1".into(),
            name: "agent_codex".into(),
            risk,
            cwd: PathBuf::from("/"),
            summary: "nested".into(),
        };
        let decide = |policy, client_approves_all, risk| async move {
            UnattendedGate {
                policy,
                client_approves_all,
            }
            .approve(request(risk), CancellationToken::new())
            .await
            .unwrap()
        };
        use ApprovalPolicy::{Always, Never, OnRisk};
        use ToolRisk::{Process, ReadOnly};
        // An owner chat session's client approves everything, so background
        // requests get that answer, within the policy.
        assert!(decide(OnRisk, true, Process).await);
        assert!(decide(Always, true, Process).await);
        assert!(decide(Always, true, ReadOnly).await);
        assert!(
            !decide(Never, true, Process).await,
            "never beyond the policy"
        );
        assert!(decide(Never, true, ReadOnly).await);
        // A client that asks a person (the TUI) or a tool-free guest: only
        // what the policy grants on its own.
        assert!(!decide(OnRisk, false, Process).await);
        assert!(decide(OnRisk, false, ReadOnly).await);
        assert!(!decide(Always, false, ReadOnly).await);
        assert!(!decide(Never, false, Process).await);
    }

    #[test]
    fn signed_out_agents_with_a_local_sign_in_check_are_not_offered() {
        let home = tempfile::tempdir().unwrap();
        let config = Config {
            instance_home: home.path().to_owned(),
            ..Config::default()
        };
        let offered = offered_adapters(&config);
        // Nothing stored for dsh, pi, grok, or the nested SCV: all hidden.
        for hidden in ["agent_dsh", "agent_pi", "agent_grok", "agent_scv"] {
            assert!(!offered.contains_key(hidden), "{hidden} offered");
        }
        // Claude and Codex report sign-in through their own CLI, which is
        // too slow to run at every session start, so they stay offered.
        assert!(offered.contains_key("agent_claude"));
        assert!(offered.contains_key("agent_codex"));
        // A stored dsh key makes it available.
        let dsh = home.path().join("agents/dsh/.dsh");
        std::fs::create_dir_all(&dsh).unwrap();
        std::fs::write(
            dsh.join(".credentials.yaml"),
            "version: 1\n\nrefs:\n  DEEPSEEK_API_KEY: test-only\n",
        )
        .unwrap();
        assert!(offered_adapters(&config).contains_key("agent_dsh"));
    }

    #[tokio::test]
    async fn daemon_control_lists_and_stops_delegations() {
        use std::os::unix::process::CommandExt as _;
        let home = tempfile::tempdir().unwrap();
        let registry = DelegationRegistry::new(home.path());
        let components = Arc::new(Mutex::new(components::Components::new(
            PathBuf::from("/unused.sock"),
            PathBuf::from("/"),
        )));
        // A run owned by another live SCV process of the same instance.
        let mut owner = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut agent = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let identity = |pid| delegations::ProcessIdentity::of(pid).unwrap();
        let record = delegations::DelegationRecord {
            handle: "codex-a1b2c3".into(),
            agent: "codex".into(),
            instance: registry.instance().into(),
            session: "session".into(),
            owner: identity(owner.id()),
            process: identity(agent.id()),
            pgid: agent.id(),
            cwd: "/work/project\u{7}".into(),
            started_unix: 1,
            depth: 1,
            conversation: Some("codex-2".into()),
            turn: Some(3),
        };
        std::fs::create_dir_all(registry.record_dir()).unwrap();
        std::fs::write(
            registry.record_dir().join("codex-a1b2c3.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let control = |command| daemon_control(&components, &registry, command);
        let Ok(status) = control(DaemonCommand::Status).await else {
            panic!("status failed");
        };
        assert_eq!(status.delegations.active, 1);
        assert!(status.delegations.entries.is_empty());
        let Ok(status) = control(DaemonCommand::Delegations { all: false }).await else {
            panic!("listing failed");
        };
        let [entry] = status.delegations.entries.as_slice() else {
            panic!("{:?}", status.delegations);
        };
        assert_eq!(entry.handle, "codex-a1b2c3");
        assert_eq!(entry.conversation.as_deref(), Some("codex-2"));
        assert_eq!(entry.turn, Some(3));
        assert_eq!(entry.pid, agent.id());
        assert_eq!(entry.owner_pid, owner.id());
        assert!(!entry.orphaned);
        assert_eq!(entry.processes, 1);
        for command in [
            DaemonCommand::DelegationKill {
                handle: Some("codex-nosuch".into()),
                orphans: false,
            },
            DaemonCommand::DelegationKill {
                handle: None,
                orphans: false,
            },
        ] {
            assert!(matches!(
                control(command).await,
                Err(ControlFailure::Delegation(_))
            ));
        }
        let Ok(status) = control(DaemonCommand::DelegationKill {
            handle: Some("codex-a1b2c3".into()),
            orphans: false,
        })
        .await
        else {
            panic!("kill failed");
        };
        assert_eq!(status.delegations.killed, ["codex-a1b2c3"]);
        assert!(agent.wait().unwrap().code().is_none());
        // Its live owner removes the record itself; once the owner is gone
        // the record is an orphan that an orphan sweep removes.
        owner.kill().unwrap();
        owner.wait().unwrap();
        let Ok(status) = control(DaemonCommand::Delegations { all: true }).await else {
            panic!("listing failed");
        };
        assert!(status.delegations.entries[0].orphaned);
        assert_eq!(status.delegations.active, 0);
        let Ok(_) = control(DaemonCommand::DelegationKill {
            handle: None,
            orphans: true,
        })
        .await
        else {
            panic!("orphan sweep failed");
        };
        assert!(registry.list(true).is_empty());
    }
}
