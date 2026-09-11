//! SCV's authoritative stdio server.

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
pub use config::{ApprovalPolicy, ConfigOverrides};
pub fn init_user_config() -> anyhow::Result<std::path::PathBuf> { config::Config::init_user_config() }
use scv_core::{
    AgentError, AgentRuntime, ApprovalGate, ApprovalRequest, BudgetContextPolicy, CoreEvent,
    EventSink, Message, ToolRisk,
};
use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, QueueEntry, ServerEvent, Usage};
use scv_provider_openai::OpenAiProvider;
use scv_tools::{SkillMap, builtin_registry};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
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
    run(stdin, stdout, overrides).await
}

async fn run<R, W>(reader: R, writer: W, overrides: ConfigOverrides) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let initial_output_bytes =
        output_queue_bytes(Config::default().protocol.max_server_frame_bytes)?;
    let (output_tx, mut output_rx) = outbound_channel(initial_output_bytes);
    let mut writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(frame) = output_rx.recv().await {
            writer.write_all(&frame.bytes).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });
    let (done_tx, mut done_rx) = mpsc::channel::<TurnDone>(4);
    let approvals = Arc::new(ApprovalBroker::default());
    let mut reader = BufReader::new(reader);
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
            read = read_bounded_frame(&mut reader, frame_limit) => {
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
                    ClientMessage::SessionStart { request_id, cwd } => {
                        if session.is_some() {
                            send_error(&output_tx, &request_id, "invalid_request", "this connection already has a session", false, server_frame_limit(&session)).await?;
                            continue;
                        }
                        match build_session(&cwd, overrides.clone()).await {
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
                        let cancellation = CancellationToken::new();
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
                        let task = tokio::spawn(async move {
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
                            Err(code) => send_error(&output_tx, &request_id, &code, "queue entry was not found or revision is stale", false, server_frame_limit(&session)).await?,
                        }
                    }
                    ClientMessage::QueueMove { request_id, session_id, queue_id, revision, before_queue_id } => {
                        let Some(current) = session.as_ref() else { send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?; continue; };
                        match current.move_queue(&session_id, &queue_id, revision, before_queue_id).await {
                            Ok((id, rev, pos)) => send_event(&output_tx, ServerEvent::QueueMoved { request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), queue_id: id, position: pos, revision: rev }, current.config.protocol.max_server_frame_bytes).await?,
                            Err(code) => send_error(&output_tx, &request_id, &code, "queue entry was not found or revision is stale", false, server_frame_limit(&session)).await?,
                        }
                    }
                    ClientMessage::QueueRemove { request_id, session_id, queue_id, revision } => {
                        let Some(current) = session.as_ref() else { send_error(&output_tx, &request_id, "session_not_found", "start a session first", false, server_frame_limit(&session)).await?; continue; };
                        match current.remove_queue(&session_id, &queue_id, revision).await {
                            Ok((id, rev)) => send_event(&output_tx, ServerEvent::QueueRemoved { request_id, session_id: current.id.clone(), seq: next_seq(&current.seq), queue_id: id, revision: rev }, current.config.protocol.max_server_frame_bytes).await?,
                            Err(code) => send_error(&output_tx, &request_id, &code, "queue entry was not found or revision is stale", false, server_frame_limit(&session)).await?,
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
                    if let Some(active) = active.take() {
                        let _ = active.task.await;
                    }
                    if let Some(current) = session.as_ref()
                        && !current.paused.load(Ordering::Acquire)
                        && let Some(entry) = current.queue.lock().await.pop_front()
                    {
                        let turn_id = Uuid::new_v4().to_string();
                        let cancellation = CancellationToken::new();
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
                        let task = tokio::spawn(async move {
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

async fn read_bounded_frame<R>(reader: &mut R, max_bytes: usize) -> std::io::Result<FrameRead>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::with_capacity(max_bytes.min(8192));
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(2);
    let mut limited = reader.take(read_limit);
    let read = limited.read_until(b'\n', &mut frame).await?;
    drop(limited);
    if read == 0 {
        return Ok(FrameRead::Eof);
    }
    let ended_with_newline = frame.last() == Some(&b'\n');
    while matches!(frame.last(), Some(b'\n' | b'\r')) {
        frame.pop();
    }
    if frame.len() <= max_bytes {
        return Ok(FrameRead::Frame(frame));
    }
    if !ended_with_newline {
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                break;
            }
            if let Some(end) = available.iter().position(|byte| *byte == b'\n') {
                reader.consume(end + 1);
                break;
            }
            let consumed = available.len();
            reader.consume(consumed);
        }
    }
    Ok(FrameRead::TooLarge)
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
    async fn enqueue(&self, prompt: String, submitter: String) -> std::result::Result<QueueEntry, &'static str> {
        let entry = QueueEntry { queue_id: Uuid::new_v4().to_string(), revision: 1, prompt, submitter };
        let mut queue = self.queue.lock().await;
        let bytes: usize = queue.iter().map(|item| item.prompt.len()).sum();
        if queue.len() >= MAX_QUEUE_ITEMS || bytes.saturating_add(entry.prompt.len()) > MAX_QUEUE_BYTES {
            return Err("queue_limit");
        }
        queue.push_back(entry.clone());
        Ok(entry)
    }

    async fn update_queue(&self, id: &str, revision: u64, prompt: String) -> std::result::Result<QueueEntry, &'static str> {
        let mut queue = self.queue.lock().await;
        let bytes: usize = queue.iter().map(|item| item.prompt.len()).sum();
        let entry = queue.iter_mut().find(|entry| entry.queue_id == id).ok_or("queue_not_found")?;
        if entry.revision != revision { return Err("queue_conflict"); }
        if bytes.saturating_sub(entry.prompt.len()).saturating_add(prompt.len()) > MAX_QUEUE_BYTES { return Err("queue_limit"); }
        entry.prompt = prompt;
        entry.revision += 1;
        Ok(entry.clone())
    }

    async fn move_queue(&self, session_id: &str, id: &str, revision: u64, before: Option<String>) -> std::result::Result<(String, u64, usize), &'static str> {
        if self.id != session_id { return Err("session_not_found"); }
        let mut queue = self.queue.lock().await;
        let index = queue.iter().position(|entry| entry.queue_id == id).ok_or("queue_not_found")?;
        if queue[index].revision != revision { return Err("queue_conflict"); }
        // Validate the destination while the source is still present. This keeps
        // the operation atomic and handles a self move as a no-op reorder.
        let target_index = match before.as_deref() {
            Some(target) if target == id => return Ok((id.to_string(), revision, index)),
            Some(target) => Some(queue.iter().position(|item| item.queue_id == target).ok_or("queue_not_found")?),
            None => None,
        };
        let mut entry = queue.remove(index).expect("queue index exists");
        let target = target_index.map_or(queue.len(), |target| target.saturating_sub(usize::from(target > index)));
        let pos = target.min(queue.len());
        let id = entry.queue_id.clone();
        let rev = entry.revision + 1;
        entry.revision = rev;
        queue.insert(pos, entry);
        Ok((id, rev, pos))
    }

    async fn remove_queue(&self, session_id: &str, id: &str, revision: u64) -> std::result::Result<(String, u64), &'static str> {
        if self.id != session_id { return Err("session_not_found"); }
        let mut queue = self.queue.lock().await;
        let index = queue.iter().position(|entry| entry.queue_id == id).ok_or("queue_not_found")?;
        if queue[index].revision != revision { return Err("queue_conflict"); }
        let entry = queue.remove(index).expect("queue index exists");
        Ok((entry.queue_id, entry.revision))
    }
}

struct ActiveTurn {
    turn_id: String,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

async fn shutdown_active_turn(mut active: ActiveTurn, grace: Duration) -> bool {
    active.cancellation.cancel();
    if tokio::time::timeout(grace, &mut active.task).await.is_ok() {
        true
    } else {
        active.task.abort();
        let _ = active.task.await;
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
    let config = Config::load(&workspace, overrides)?;
    let provider_config = config.provider.clone();
    let api_key = provider_config.api_key.clone().or_else(|| {
        provider_config.api_key_env.as_deref().and_then(|name| std::env::var(name).ok())
    }).filter(|key| !key.trim().is_empty()).ok_or_else(|| anyhow!("provider credential is not configured; set provider.api_key or provider.api_key_env"))?;
    let (skills, skill_roots, skill_prompt) = discover_skills(&workspace, &config)?;
    let system_prompt = build_system_prompt(&workspace, &config, &skill_prompt)?;
    let provider = Arc::new(OpenAiProvider::new(
        provider_config.model.clone(),
        provider_config.base_url.clone(),
        api_key,
        Duration::from_secs(provider_config.timeout_seconds),
        config.provider_limits(),
        provider_config.headers.clone(),
    )?);
    let tools = Arc::new(builtin_registry(
        config.tools(),
        skills,
        skill_roots,
        config.skills.max_skill_bytes,
        config.adapters(),
    )?);
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

fn build_system_prompt(workspace: &Path, config: &Config, skills: &str) -> Result<String> {
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
    if !skills.is_empty() {
        prompt.push_str("\n# Available skills\n");
        prompt.push_str(skills);
        prompt.push_str("\nUse read_skill with a skill name when its workflow applies.\n");
    }
    Ok(prompt)
}

fn discover_skills(workspace: &Path, config: &Config) -> Result<(SkillMap, Vec<PathBuf>, String)> {
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
    Ok((skills, roots, listing))
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
}
