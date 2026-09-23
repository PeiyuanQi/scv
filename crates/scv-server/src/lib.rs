//! SCV's authoritative stdio server.

mod agents;
pub mod components;
mod config;

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
    let mut command = std::process::Command::new(&adapter.command);
    command
        .current_dir(config.instance_home.join("adapters").join(agent))
        .envs(adapter.environment);
    for variable in scv_tools::AGENT_REMOVED_ENVIRONMENT {
        command.env_remove(variable);
    }
    Ok(command)
}

/// Copy the user's own Codex setup from `source` into SCV's private Codex
/// adapter home: `config.toml`, and `auth.json` only when it holds an API key.
/// Returns display lines that never contain secret values.
pub fn import_codex(source: &Path) -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    agents::import_codex(source, &config.instance_home.join("adapters").join("codex"))
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
use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, QueueEntry, ServerEvent, Usage};
use scv_provider_openai::OpenAiProvider;
use scv_tools::{SkillMap, builtin_registry};
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
    let result = run_managed(
        stdin,
        stdout,
        overrides,
        None,
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
    }
    let _lock = SocketLock::acquire(path)?;
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
    let cancellation = CancellationToken::new();
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
                let cancellation = cancellation.clone();
                let tasks = tasks.clone();
                clients.spawn(async move {
                    let (reader, writer) = stream.into_split();
                    if run_managed(reader, writer, child_overrides, Some(components), cancellation, tasks).await.is_err() {
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

async fn run_managed<R, W>(
    reader: R,
    writer: W,
    overrides: ConfigOverrides,
    components: Option<Arc<Mutex<components::Components>>>,
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
                                result = async { components.lock().await.control(command).await } => result,
                            };
                            match result {
                                Ok(status) => send_event(&output_tx, ServerEvent::DaemonStatus { request_id, status }, server_frame_limit(&session)).await?,
                                Err(_) => send_error(&output_tx, &request_id, "component_error", "Component operation failed; check account credentials, private file permissions and absolute workspace", false, server_frame_limit(&session)).await?,
                            }
                        } else {
                            send_error(&output_tx, &request_id, "unsupported", "Component management requires the daemon socket", false, server_frame_limit(&session)).await?;
                        }
                    }
                    ClientMessage::SessionStart { request_id, cwd, provider, model, base_url, no_tools } => {
                        if session.is_some() {
                            send_error(&output_tx, &request_id, "invalid_request", "this connection already has a session", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        let session_overrides = ConfigOverrides {
                            provider: provider.or_else(|| overrides.provider.clone()),
                            model: model.or_else(|| overrides.model.clone()),
                            base_url: base_url.or_else(|| overrides.base_url.clone()),
                            approval_policy: overrides.approval_policy,
                            no_tools: no_tools.unwrap_or(overrides.no_tools),
                        };
                        match build_session(&cwd, session_overrides).await {
                            Ok(new_session) => {
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
                        let turn_id = Uuid::new_v4().to_string();
                        let cancellation = cancellation.child_token();
                        let meta = TurnMeta {
                            request_id: request_id.clone(),
                            session_id: current.id.clone(),
                            turn_id: turn_id.clone(),
                            seq: Arc::clone(&current.seq),
                            max_server_frame: current.config.protocol.max_server_frame_bytes,
                        };
                        send_event(&output_tx, ServerEvent::TurnStarted {
                            request_id: request_id.clone(),
                            session_id: current.id.clone(),
                            turn_id: turn_id.clone(),
                            seq: next_seq(&current.seq),
                        }, current.config.protocol.max_server_frame_bytes).await?;
                        let runtime = Arc::clone(&current.runtime);
                        let history = Arc::clone(&current.history);
                        let sink: Arc<dyn EventSink> = Arc::new(ProtocolSink {
                            meta: meta.clone(),
                            output: output_tx.clone(),
                            cancellation: cancellation.clone(),
                        });
                        let gate: Arc<dyn ApprovalGate> = Arc::new(ProtocolApprovalGate {
                            policy: current.config.tools.approval_policy,
                            broker: Arc::clone(&approvals),
                            meta,
                            output: output_tx.clone(),
                        });
                        let task_cancel = cancellation.clone();
                        let task_done = done_tx.clone();
                        let task_request = request_id.clone();
                        let task_session = current.id.clone();
                        let task_turn = turn_id.clone();
                        let task = tasks.spawn(async move {
                            let mut history = history.lock().await;
                            let result = runtime.run_turn(&mut history, prompt, sink, gate, task_cancel).await;
                            let _ = task_done.send(TurnDone {
                                request_id: task_request,
                                session_id: task_session,
                                turn_id: task_turn,
                                result,
                            }).await;
                        });
                        active = Some(ActiveTurn { turn_id, cancellation, task });
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
                            },
                            Err(AgentError::Cancelled) => ServerEvent::TurnCancelled {
                                request_id: done.request_id,
                                session_id: done.session_id,
                                turn_id: done.turn_id,
                                seq,
                            },
                            Err(error) => ServerEvent::TurnFailed {
                                request_id: done.request_id,
                                session_id: done.session_id,
                                turn_id: done.turn_id,
                                seq,
                                code: error.code().into(),
                                message: error.to_string(),
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
                        let cancellation = cancellation.child_token();
                        send_event(&output_tx, ServerEvent::QueueDequeued {
                            request_id: entry.submitter.clone(),
                            session_id: current.id.clone(),
                            seq: next_seq(&current.seq),
                            queue_id: entry.queue_id,
                            turn_id: turn_id.clone(),
                        }, current.config.protocol.max_server_frame_bytes).await?;
                        send_event(&output_tx, ServerEvent::TurnStarted {
                            request_id: entry.submitter.clone(),
                            session_id: current.id.clone(),
                            turn_id: turn_id.clone(),
                            seq: next_seq(&current.seq),
                        }, current.config.protocol.max_server_frame_bytes).await?;
                        let meta = TurnMeta {
                            request_id: entry.submitter.clone(), session_id: current.id.clone(), turn_id: turn_id.clone(),
                            seq: Arc::clone(&current.seq), max_server_frame: current.config.protocol.max_server_frame_bytes,
                        };
                        let sink: Arc<dyn EventSink> = Arc::new(ProtocolSink { meta: meta.clone(), output: output_tx.clone(), cancellation: cancellation.clone() });
                        let gate: Arc<dyn ApprovalGate> = Arc::new(ProtocolApprovalGate { policy: current.config.tools.approval_policy, broker: Arc::clone(&approvals), meta, output: output_tx.clone() });
                        let runtime = Arc::clone(&current.runtime);
                        let history = Arc::clone(&current.history);
                        let task_done = done_tx.clone();
                        let task_request = entry.submitter;
                        let task_session = current.id.clone();
                        let task_turn = turn_id.clone();
                        let task_cancel = cancellation.clone();
                        let task = tasks.spawn(async move {
                            let mut history = history.lock().await;
                            let result = runtime.run_turn(&mut history, entry.prompt, sink, gate, task_cancel).await;
                            let _ = task_done.send(TurnDone { request_id: task_request, session_id: task_session, turn_id: task_turn, result }).await;
                        });
                        active = Some(ActiveTurn { turn_id, cancellation, task });
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
    result: Result<scv_core::TurnOutcome, AgentError>,
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

async fn build_session(cwd: &str, overrides: ConfigOverrides) -> Result<Session> {
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
    let system_prompt = build_system_prompt(&workspace, &config, &skills)?;
    let provider = Arc::new(OpenAiProvider::new(
        provider_config.model.clone(),
        provider_config.base_url.clone(),
        api_key,
        Duration::from_secs(provider_config.timeout_seconds),
        config.provider_limits(),
        provider_config.headers.clone(),
    )?);
    let tools = if no_tools {
        Arc::new(ToolRegistry::default())
    } else {
        Arc::new(builtin_registry(
            config.tools(),
            skills.map,
            skills.roots,
            config.skills.max_skill_bytes,
            config.adapters(),
        )?)
    };
    let context = Arc::new(BudgetContextPolicy::new((&config.context).into())?);
    let runtime = Arc::new(AgentRuntime::new(
        provider,
        tools,
        context,
        config.core_agent(system_prompt),
        workspace.clone(),
    ));
    Ok(Session {
        id: Uuid::new_v4().to_string(),
        workspace,
        config,
        runtime,
        history: Arc::new(Mutex::new(Vec::new())),
        seq: Arc::new(AtomicU64::new(0)),
        queue: Arc::new(Mutex::new(VecDeque::new())),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
}

fn build_system_prompt(
    workspace: &Path,
    config: &Config,
    skills: &DiscoveredSkills,
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
        prompt.push_str(
            "\nTo use one, delegate with an agent_* tool such as agent_codex or agent_claude, \
             set its cwd to the skill's project, and name the skill in the prompt: that agent \
             then loads the project's instructions and skills itself. read_skill loads a \
             skill for reference.\n",
        );
    }
    Ok(prompt)
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

struct ProtocolApprovalGate {
    policy: ApprovalPolicy,
    broker: Arc<ApprovalBroker>,
    meta: TurnMeta,
    output: OutboundSender,
}

#[async_trait]
impl ApprovalGate for ProtocolApprovalGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        match self.policy {
            ApprovalPolicy::OnRisk if request.risk == ToolRisk::ReadOnly => return Ok(true),
            ApprovalPolicy::Never => return Ok(request.risk == ToolRisk::ReadOnly),
            ApprovalPolicy::Always | ApprovalPolicy::OnRisk => {}
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

    use super::*;

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
        let prompt = build_system_prompt(&workspace, &config, &skills).unwrap();
        assert!(prompt.contains("# Project skills"));
        assert!(prompt.contains("set its cwd to the skill's project"));

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
                scv_core::ToolContext {
                    workspace: workspace.clone(),
                    cancellation: CancellationToken::new(),
                },
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
}
