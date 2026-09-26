//! One client connection: bounded protocol frames in, one handler per
//! `ClientMessage`, and its session's turns run one at a time. Background
//! job reports start turns of their own once the user's work is done.

use std::sync::{Arc, atomic::Ordering};

use anyhow::{Context, Result, anyhow};
use scv_core::AgentError;
use scv_protocol::{
    Attachment, ClientMessage, DaemonCommand, ErrorCode, Frame, FrameDecoder, Overflow,
    PROTOCOL_VERSION, PeerInfo, ServerEvent, Usage, trim_line,
};
use scv_tools::delegation::DelegationRegistry;
use tokio::{
    io::{AsyncBufRead, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    approval::ApprovalBroker,
    attachments, components,
    config::{Config, ConfigOverrides, Instance},
    control::{ControlFailure, daemon_control},
    daemon::AbortGuard,
    events::error_code,
    outbound::{
        OutboundSender, SHUTDOWN_GRACE, outbound_channel, output_queue_bytes, send_error,
        send_event,
    },
    restart,
    session::{
        Session, SessionClient,
        build::build_session,
        next_seq,
        turn::{ActiveTurn, TurnDone, TurnStarter, shutdown_active_turn},
        valid_channel_name,
    },
};

const PROMPT_LIMIT_BYTES: usize = 256 * 1024;

/// Serve one client until it disconnects, the server shuts down, or it
/// speaks an unsupported protocol version.
#[tracing::instrument(name = "connection", skip_all, fields(session = tracing::field::Empty))]
pub(crate) async fn run_managed<R, W>(
    reader: R,
    writer: W,
    instance: Instance,
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
    let (output, mut output_rx) = outbound_channel(initial_output_bytes);
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
    let (done, mut done_rx) = mpsc::channel::<TurnDone>(4);
    let mut connection = Connection {
        turns: TurnStarter {
            output: output.clone(),
            approvals: Arc::new(ApprovalBroker::default()),
            done,
            tasks: tasks.clone(),
            cancellation: cancellation.clone(),
        },
        output,
        instance,
        components,
        registry,
        cancellation: cancellation.clone(),
        initialized: false,
        session: None,
        activity: None,
        active: None,
        background_rx: None,
        background_ready: false,
        fatal: false,
    };
    let mut reader = BufReader::new(reader);
    let mut frames = FrameBuffer::default();
    let mut writer_finished = false;

    let loop_result: Result<()> = async {
        loop {
            connection.track_activity();
            let frame_limit = connection.client_frame_limit();
            tokio::select! {
                () = cancellation.cancelled() => break,
                read = frames.read(&mut reader, frame_limit) => {
                    let read = read.context("read protocol input")?;
                    if let Flow::Stop = connection.on_read(read).await? {
                        break;
                    }
                }
                writer = &mut writer_task => {
                    writer_finished = true;
                    writer.context("join protocol writer")??;
                    break;
                }
                Some(()) = recv_background(&mut connection.background_rx) => {
                    connection.on_background().await?;
                }
                done = done_rx.recv(), if connection.active.is_some() => {
                    connection.on_turn_done(done).await?;
                }
            }
        }
        Ok(())
    }
    .await;

    if let Some(active) = connection.active.take() {
        shutdown_active_turn(active, SHUTDOWN_GRACE).await;
    }
    let fatal = connection.fatal;
    // The session outlives the writer, as it did before the split: only the
    // connection's own senders go now, so the writer can finish.
    let _session = connection.session.take();
    let _activity = connection.activity.take();
    drop(connection);
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

/// Whether the connection keeps reading after a message.
enum Flow {
    Continue,
    Stop,
}

/// `session.start`'s request fields, apart from its request id.
struct SessionStartRequest {
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    no_tools: Option<bool>,
    delegation_depth: Option<u32>,
    channel: Option<String>,
    auto_approve: Option<bool>,
}

/// One connection's state: its session, the turn it runs, and where its
/// turns and background jobs report.
struct Connection {
    output: OutboundSender,
    turns: TurnStarter,
    /// The server's instance and command-line overrides, which each
    /// session's own overrides fall back to.
    instance: Instance,
    components: Option<Arc<Mutex<components::Components>>>,
    registry: Arc<DelegationRegistry>,
    cancellation: CancellationToken,
    initialized: bool,
    session: Option<Session>,
    /// The session's turn and report activity, which a planned restart asked
    /// for from this session waits out.
    activity: Option<restart::SessionTracker>,
    active: Option<ActiveTurn>,
    /// Woken when a background job finishes; set until its report turn starts.
    background_rx: Option<mpsc::UnboundedReceiver<()>>,
    background_ready: bool,
    /// The client spoke an unsupported protocol version.
    fatal: bool,
}

impl Connection {
    fn track_activity(&self) {
        if let Some(activity) = &self.activity {
            activity.set_busy(self.active.is_some() || self.background_ready);
        }
    }

    fn client_frame_limit(&self) -> usize {
        self.session.as_ref().map_or_else(
            || Config::default().protocol.max_client_frame_bytes,
            |session| session.config.protocol.max_client_frame_bytes,
        )
    }

    fn server_frame_limit(&self) -> usize {
        self.session.as_ref().map_or_else(
            || Config::default().protocol.max_server_frame_bytes,
            |session| session.config.protocol.max_server_frame_bytes,
        )
    }

    /// Answer `request_id` with a non-fatal protocol error.
    async fn reject(&self, request_id: &str, code: ErrorCode, message: &str) -> Result<()> {
        send_error(
            &self.output,
            request_id,
            code,
            message,
            false,
            self.server_frame_limit(),
        )
        .await
    }

    /// The session a request names, or `None` after rejecting the request.
    async fn matching_session(
        &self,
        request_id: &str,
        session_id: &str,
    ) -> Result<Option<&Session>> {
        match &self.session {
            None => {
                self.reject(
                    request_id,
                    ErrorCode::SessionNotFound,
                    "start a session first",
                )
                .await?;
                Ok(None)
            }
            Some(current) if current.id != session_id => {
                self.reject(
                    request_id,
                    ErrorCode::SessionNotFound,
                    "session id does not match",
                )
                .await?;
                Ok(None)
            }
            Some(current) => Ok(Some(current)),
        }
    }

    async fn on_read(&mut self, read: FrameRead) -> Result<Flow> {
        let frame = match read {
            FrameRead::Eof => {
                if let Some(active) = &self.active {
                    active.cancellation.cancel();
                }
                return Ok(Flow::Stop);
            }
            FrameRead::TooLarge => {
                self.reject(
                    "",
                    ErrorCode::InvalidRequest,
                    "client frame exceeds configured limit",
                )
                .await?;
                return Ok(Flow::Continue);
            }
            FrameRead::Frame(frame) => frame,
        };
        if frame.is_empty() {
            self.reject("", ErrorCode::InvalidJson, "protocol frame is empty")
                .await?;
            return Ok(Flow::Continue);
        }
        match serde_json::from_slice::<ClientMessage>(&frame) {
            Ok(message) => self.handle(message).await,
            Err(error) => {
                self.reject(
                    "",
                    ErrorCode::InvalidJson,
                    &format!("invalid protocol JSON: {error}"),
                )
                .await?;
                Ok(Flow::Continue)
            }
        }
    }

    async fn handle(&mut self, message: ClientMessage) -> Result<Flow> {
        match message {
            ClientMessage::Initialize {
                request_id,
                protocol_version,
                ..
            } => return self.initialize(request_id, protocol_version).await,
            other if !self.initialized => {
                self.reject(
                    other.request_id(),
                    ErrorCode::NotInitialized,
                    "initialize must be the first message",
                )
                .await?;
            }
            ClientMessage::DaemonControl {
                request_id,
                command,
            } => return self.control(request_id, command).await,
            ClientMessage::SessionStart {
                request_id,
                cwd,
                provider,
                model,
                base_url,
                no_tools,
                delegation_depth,
                channel,
                auto_approve,
            } => {
                let request = SessionStartRequest {
                    cwd,
                    provider,
                    model,
                    base_url,
                    no_tools,
                    delegation_depth,
                    channel,
                    auto_approve,
                };
                self.session_start(request_id, request).await?;
            }
            ClientMessage::SessionAttach { request_id, .. } => {
                self.reject(
                    &request_id,
                    ErrorCode::Unsupported,
                    "session attach requires the shared socket server",
                )
                .await?;
            }
            ClientMessage::TurnStart {
                request_id,
                session_id,
                prompt,
                attachments,
            } => {
                self.turn_start(request_id, session_id, prompt, attachments)
                    .await?;
            }
            ClientMessage::QueueUpdate {
                request_id,
                session_id,
                queue_id,
                revision,
                prompt,
            } => {
                self.queue_update(request_id, session_id, queue_id, revision, prompt)
                    .await?;
            }
            ClientMessage::QueueMove {
                request_id,
                session_id,
                queue_id,
                revision,
                before_queue_id,
            } => {
                self.queue_move(request_id, session_id, queue_id, revision, before_queue_id)
                    .await?;
            }
            ClientMessage::QueueRemove {
                request_id,
                session_id,
                queue_id,
                revision,
            } => {
                self.queue_remove(request_id, session_id, queue_id, revision)
                    .await?;
            }
            ClientMessage::SessionPause {
                request_id,
                session_id,
                paused,
            } => self.session_pause(request_id, session_id, paused).await?,
            ClientMessage::TurnCancel {
                request_id,
                session_id,
                turn_id,
            } => self.turn_cancel(request_id, session_id, turn_id).await?,
            ClientMessage::ApprovalResolve {
                request_id,
                session_id,
                approval_id,
                approved,
            } => {
                self.approval_resolve(request_id, session_id, approval_id, approved)
                    .await?;
            }
            ClientMessage::SessionClear {
                request_id,
                session_id,
            } => self.session_clear(request_id, session_id).await?,
        }
        Ok(Flow::Continue)
    }

    async fn initialize(&mut self, request_id: String, protocol_version: u32) -> Result<Flow> {
        if self.initialized {
            self.reject(
                &request_id,
                ErrorCode::InvalidRequest,
                "connection is already initialized",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if protocol_version != PROTOCOL_VERSION {
            send_error(
                &self.output,
                &request_id,
                ErrorCode::VersionMismatch,
                &format!("server supports protocol {PROTOCOL_VERSION}"),
                true,
                self.server_frame_limit(),
            )
            .await?;
            self.fatal = true;
            return Ok(Flow::Stop);
        }
        self.initialized = true;
        send_event(
            &self.output,
            ServerEvent::Initialized {
                request_id,
                protocol_version: PROTOCOL_VERSION,
                server: PeerInfo {
                    name: "scv-server".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            },
            Config::default().protocol.max_server_frame_bytes,
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn control(&self, request_id: String, command: DaemonCommand) -> Result<Flow> {
        let Some(components) = &self.components else {
            self.reject(
                &request_id,
                ErrorCode::Unsupported,
                "Component management requires the daemon socket",
            )
            .await?;
            return Ok(Flow::Continue);
        };
        let result = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Ok(Flow::Stop),
            result = daemon_control(components, &self.registry, command) => result,
        };
        match result {
            Ok(status) => {
                send_event(
                    &self.output,
                    ServerEvent::DaemonStatus { request_id, status },
                    self.server_frame_limit(),
                )
                .await?;
            }
            Err(ControlFailure::Delegation(message)) => {
                self.reject(&request_id, ErrorCode::DelegationError, &message)
                    .await?;
            }
            Err(ControlFailure::Restart(message)) => {
                self.reject(&request_id, ErrorCode::RestartError, &message)
                    .await?;
            }
            Err(ControlFailure::Confirm(message)) => {
                self.reject(&request_id, ErrorCode::ConfirmError, &message)
                    .await?;
            }
            Err(ControlFailure::Component) => {
                self.reject(
                    &request_id,
                    ErrorCode::ComponentError,
                    "Component operation failed; check account credentials, private file \
                     permissions and absolute workspace",
                )
                .await?;
            }
        }
        Ok(Flow::Continue)
    }

    async fn session_start(
        &mut self,
        request_id: String,
        request: SessionStartRequest,
    ) -> Result<()> {
        if self.session.is_some() {
            self.reject(
                &request_id,
                ErrorCode::InvalidRequest,
                "this connection already has a session",
            )
            .await?;
            return Ok(());
        }
        if request
            .channel
            .as_deref()
            .is_some_and(|name| !valid_channel_name(name))
        {
            self.reject(
                &request_id,
                ErrorCode::InvalidRequest,
                "channel must be a short name without control characters",
            )
            .await?;
            return Ok(());
        }
        let client = SessionClient {
            channel: request.channel,
            auto_approve: request.auto_approve.unwrap_or(false),
        };
        let defaults = &self.instance.overrides;
        let overrides = ConfigOverrides {
            provider: request.provider.or_else(|| defaults.provider.clone()),
            model: request.model.or_else(|| defaults.model.clone()),
            base_url: request.base_url.or_else(|| defaults.base_url.clone()),
            approval_policy: defaults.approval_policy,
            no_tools: request.no_tools.unwrap_or(defaults.no_tools),
            config_file: defaults.config_file.clone(),
        };
        let built = build_session(
            &self.instance.layout,
            &request.cwd,
            overrides,
            request.delegation_depth.unwrap_or(0),
            &self.registry,
            client,
        )
        .await;
        let (session, finished) = match built {
            Ok(built) => built,
            Err(error) => {
                return self
                    .reject(&request_id, ErrorCode::InvalidRequest, &error.to_string())
                    .await;
            }
        };
        self.background_rx = finished;
        let max_bytes = session.config.protocol.max_server_frame_bytes;
        self.output
            .ensure_capacity(output_queue_bytes(max_bytes)?)?;
        let started = ServerEvent::SessionStarted {
            request_id,
            session_id: session.id.clone(),
            cwd: session.workspace.display().to_string(),
            model: session.runtime.model().to_owned(),
            context_max_tokens: session.config.context.max_tokens,
            max_server_frame_bytes: max_bytes,
            max_transcript_bytes: session.config.tui.max_transcript_bytes,
            max_transcript_items: session.config.tui.max_transcript_items,
            max_prompt_history_bytes: session.config.tui.max_prompt_history_bytes,
            max_prompt_history_items: session.config.tui.max_prompt_history_items,
        };
        send_event(&self.output, started, max_bytes).await?;
        let snapshot = ServerEvent::QueueSnapshot {
            request_id: None,
            session_id: session.id.clone(),
            seq: next_seq(&session.seq),
            entries: session.queue.lock().await.iter().cloned().collect(),
            paused: session.paused.load(Ordering::Acquire),
        };
        send_event(&self.output, snapshot, max_bytes).await?;
        self.activity = Some(restart::SessionTracker::new(
            &session.id,
            session.background.as_ref(),
        ));
        tracing::Span::current().record("session", session.id.as_str());
        self.session = Some(session);
        Ok(())
    }

    async fn turn_start(
        &mut self,
        request_id: String,
        session_id: String,
        prompt: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        let Some(current) = self.matching_session(&request_id, &session_id).await? else {
            return Ok(());
        };
        if (prompt.trim().is_empty() && attachments.is_empty()) || prompt.len() > PROMPT_LIMIT_BYTES
        {
            self.reject(
                &request_id,
                ErrorCode::InvalidRequest,
                "prompt must be non-empty and no larger than 256 KiB",
            )
            .await?;
            return Ok(());
        }
        if let Err(message) = attachments::validate(&attachments) {
            self.reject(&request_id, ErrorCode::InvalidRequest, &message)
                .await?;
            return Ok(());
        }
        if self.active.is_some() {
            let entry = match current
                .enqueue(prompt, request_id.clone(), attachments)
                .await
            {
                Ok(entry) => entry,
                Err(code) => {
                    return self
                        .reject(&request_id, code, "session queue limit reached")
                        .await;
                }
            };
            let position = current.queue.lock().await.len().saturating_sub(1);
            let event = ServerEvent::QueueEnqueued {
                request_id,
                session_id: current.id.clone(),
                seq: next_seq(&current.seq),
                entry,
                position,
            };
            return send_event(
                &self.output,
                event,
                current.config.protocol.max_server_frame_bytes,
            )
            .await;
        }
        let input = current.turn_input(&prompt, &attachments);
        let turn = self
            .turns
            .start(current, Uuid::new_v4().to_string(), request_id, input, None)
            .await?;
        self.active = Some(turn);
        Ok(())
    }

    async fn queue_update(
        &self,
        request_id: String,
        session_id: String,
        queue_id: String,
        revision: u64,
        prompt: String,
    ) -> Result<()> {
        let Some(current) = self.matching_session(&request_id, &session_id).await? else {
            return Ok(());
        };
        if prompt.trim().is_empty() || prompt.len() > PROMPT_LIMIT_BYTES {
            return self
                .reject(
                    &request_id,
                    ErrorCode::InvalidRequest,
                    "prompt must be non-empty and no larger than 256 KiB",
                )
                .await;
        }
        match current.update_queue(&queue_id, revision, prompt).await {
            Ok(entry) => {
                let event = ServerEvent::QueueUpdated {
                    request_id,
                    session_id: current.id.clone(),
                    seq: next_seq(&current.seq),
                    entry,
                };
                send_event(
                    &self.output,
                    event,
                    current.config.protocol.max_server_frame_bytes,
                )
                .await
            }
            Err(code) => {
                self.reject(
                    &request_id,
                    code,
                    "queue entry was not found or revision is stale",
                )
                .await
            }
        }
    }

    async fn queue_move(
        &self,
        request_id: String,
        session_id: String,
        queue_id: String,
        revision: u64,
        before_queue_id: Option<String>,
    ) -> Result<()> {
        let Some(current) = self.session.as_ref() else {
            return self
                .reject(
                    &request_id,
                    ErrorCode::SessionNotFound,
                    "start a session first",
                )
                .await;
        };
        match current
            .move_queue(&session_id, &queue_id, revision, before_queue_id)
            .await
        {
            Ok((queue_id, revision, position)) => {
                let event = ServerEvent::QueueMoved {
                    request_id,
                    session_id: current.id.clone(),
                    seq: next_seq(&current.seq),
                    queue_id,
                    position,
                    revision,
                };
                send_event(
                    &self.output,
                    event,
                    current.config.protocol.max_server_frame_bytes,
                )
                .await
            }
            Err(code) => {
                self.reject(
                    &request_id,
                    code,
                    "queue entry was not found or revision is stale",
                )
                .await
            }
        }
    }

    async fn queue_remove(
        &self,
        request_id: String,
        session_id: String,
        queue_id: String,
        revision: u64,
    ) -> Result<()> {
        let Some(current) = self.session.as_ref() else {
            return self
                .reject(
                    &request_id,
                    ErrorCode::SessionNotFound,
                    "start a session first",
                )
                .await;
        };
        match current.remove_queue(&session_id, &queue_id, revision).await {
            Ok((queue_id, revision)) => {
                let event = ServerEvent::QueueRemoved {
                    request_id,
                    session_id: current.id.clone(),
                    seq: next_seq(&current.seq),
                    queue_id,
                    revision,
                };
                send_event(
                    &self.output,
                    event,
                    current.config.protocol.max_server_frame_bytes,
                )
                .await
            }
            Err(code) => {
                self.reject(
                    &request_id,
                    code,
                    "queue entry was not found or revision is stale",
                )
                .await
            }
        }
    }

    async fn session_pause(
        &self,
        request_id: String,
        session_id: String,
        paused: bool,
    ) -> Result<()> {
        let Some(current) = self.matching_session(&request_id, &session_id).await? else {
            return Ok(());
        };
        current.paused.store(paused, Ordering::Release);
        let event = ServerEvent::SessionPaused {
            request_id,
            session_id: current.id.clone(),
            seq: next_seq(&current.seq),
            paused,
        };
        send_event(
            &self.output,
            event,
            current.config.protocol.max_server_frame_bytes,
        )
        .await
    }

    async fn turn_cancel(
        &self,
        request_id: String,
        session_id: String,
        turn_id: String,
    ) -> Result<()> {
        match (&self.session, &self.active) {
            (Some(current), Some(running))
                if current.id == session_id && running.turn_id == turn_id =>
            {
                running.cancellation.cancel();
                Ok(())
            }
            _ => {
                self.reject(
                    &request_id,
                    ErrorCode::TurnNotFound,
                    "active turn was not found",
                )
                .await
            }
        }
    }

    async fn approval_resolve(
        &self,
        request_id: String,
        session_id: String,
        approval_id: String,
        approved: bool,
    ) -> Result<()> {
        if self
            .session
            .as_ref()
            .is_none_or(|current| current.id != session_id)
        {
            self.reject(
                &request_id,
                ErrorCode::SessionNotFound,
                "session id does not match",
            )
            .await
        } else if !self.turns.approvals.resolve(&approval_id, approved).await {
            self.reject(
                &request_id,
                ErrorCode::ApprovalNotFound,
                "approval was not found or already resolved",
            )
            .await
        } else {
            Ok(())
        }
    }

    async fn session_clear(&self, request_id: String, session_id: String) -> Result<()> {
        let Some(current) = self.session.as_ref() else {
            return self
                .reject(
                    &request_id,
                    ErrorCode::SessionNotFound,
                    "session was not found",
                )
                .await;
        };
        if current.id != session_id {
            return self
                .reject(
                    &request_id,
                    ErrorCode::SessionNotFound,
                    "session id does not match",
                )
                .await;
        }
        if self.active.is_some() {
            return self
                .reject(
                    &request_id,
                    ErrorCode::TurnActive,
                    "cancel the active turn before clearing",
                )
                .await;
        }
        current.history.lock().await.clear();
        current.queue.lock().await.clear();
        let max_bytes = current.config.protocol.max_server_frame_bytes;
        let cleared = ServerEvent::SessionCleared {
            request_id,
            session_id: current.id.clone(),
            seq: next_seq(&current.seq),
        };
        send_event(&self.output, cleared, max_bytes).await?;
        let snapshot = ServerEvent::QueueSnapshot {
            request_id: None,
            session_id: current.id.clone(),
            seq: next_seq(&current.seq),
            entries: Vec::new(),
            paused: current.paused.load(Ordering::Acquire),
        };
        send_event(&self.output, snapshot, max_bytes).await
    }

    /// A background job finished: report it now when the session is idle,
    /// else once the user's own work is done.
    async fn on_background(&mut self) -> Result<()> {
        self.background_ready = true;
        if self.active.is_none()
            && let Some(current) = self.session.as_ref()
            && !current.paused.load(Ordering::Acquire)
            && current.queue.lock().await.is_empty()
        {
            let turn = self.turns.report_background(current).await?;
            self.background_ready = turn.is_some();
            self.active = turn;
        }
        Ok(())
    }

    /// A turn ended: announce how, then start the next queued prompt or a
    /// pending background report.
    async fn on_turn_done(&mut self, done: Option<TurnDone>) -> Result<()> {
        let Some(done) = done else {
            return Ok(());
        };
        if let Some(current) = self.session.as_ref() {
            let seq = next_seq(&current.seq);
            let event = match done.result {
                Ok(outcome) => ServerEvent::TurnCompleted {
                    request_id: done.request_id,
                    session_id: done.session_id,
                    turn_id: done.turn_id,
                    seq,
                    steps: outcome.steps,
                    usage: Usage {
                        input_tokens: outcome.usage.input_tokens,
                        output_tokens: outcome.usage.output_tokens,
                    },
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
                    code: error_code(&error),
                    message: error.to_string(),
                    origin: done.origin,
                },
            };
            send_event(
                &self.output,
                event,
                current.config.protocol.max_server_frame_bytes,
            )
            .await?;
        }
        if let Some(mut active) = self.active.take() {
            let _ = (&mut active.task).await;
        }
        if let Some(current) = self.session.as_ref()
            && !current.paused.load(Ordering::Acquire)
            && let Some(entry) = current.queue.lock().await.pop_front()
        {
            let turn_id = Uuid::new_v4().to_string();
            let dequeued = ServerEvent::QueueDequeued {
                request_id: entry.submitter.clone(),
                session_id: current.id.clone(),
                seq: next_seq(&current.seq),
                queue_id: entry.queue_id,
                turn_id: turn_id.clone(),
            };
            send_event(
                &self.output,
                dequeued,
                current.config.protocol.max_server_frame_bytes,
            )
            .await?;
            let input = current.turn_input(&entry.prompt, &entry.attachments);
            let turn = self
                .turns
                .start(current, turn_id, entry.submitter, input, None)
                .await?;
            self.active = Some(turn);
        }
        // Report finished background jobs once the user's own work is done.
        if self.active.is_none()
            && self.background_ready
            && let Some(current) = self.session.as_ref()
        {
            let turn = self.turns.report_background(current).await?;
            self.background_ready = turn.is_some();
            self.active = turn;
        }
        Ok(())
    }
}

pub(crate) enum FrameRead {
    Eof,
    Frame(Vec<u8>),
    TooLarge,
}

/// A persistent decoder keeps consumed partial bytes across select cancellation.
pub(crate) struct FrameBuffer {
    decoder: FrameDecoder,
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self {
            decoder: FrameDecoder::new(0, Overflow::Skip),
        }
    }
}

impl FrameBuffer {
    /// The next client frame of at most `max_bytes`, not counting its line
    /// ending (`\n` or `\r\n`). A frame over the limit is skipped whole and
    /// reported, so the connection can go on.
    pub(crate) async fn read<R>(
        &mut self,
        reader: &mut R,
        max_bytes: usize,
    ) -> std::io::Result<FrameRead>
    where
        R: AsyncBufRead + Unpin,
    {
        self.decoder.set_limit(max_bytes.saturating_add(2));
        Ok(
            match scv_client::read_frame(reader, &mut self.decoder).await? {
                Frame::End => FrameRead::Eof,
                Frame::TooLarge => FrameRead::TooLarge,
                Frame::Line(line) | Frame::Truncated(line) => {
                    trim_line(line, max_bytes).map_or(FrameRead::TooLarge, FrameRead::Frame)
                }
            },
        )
    }
}

async fn shutdown_writer(
    mut writer: JoinHandle<std::io::Result<()>>,
    grace: std::time::Duration,
) -> Result<()> {
    if let Ok(result) = tokio::time::timeout(grace, &mut writer).await {
        result.context("join protocol writer")??;
        Ok(())
    } else {
        writer.abort();
        let _ = writer.await;
        Err(anyhow!("protocol writer shutdown timed out"))
    }
}

/// The next background-job wake-up, or never without a receiver.
async fn recv_background(receiver: &mut Option<mpsc::UnboundedReceiver<()>>) -> Option<()> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
