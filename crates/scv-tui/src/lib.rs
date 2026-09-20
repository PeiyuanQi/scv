//! SCV's terminal client.

use std::{
    collections::VecDeque,
    io::{self, Write as _, stdout},
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, QueueEntry, ServerEvent};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    process::{Child, ChildStdin, ChildStdout, Command},
};
use uuid::Uuid;

const DEFAULT_SERVER_FRAME_LIMIT: usize = 8 * 1024 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
// The server may spend three seconds stopping an active turn and three more
// draining its writer after stdin EOF. Keep the client grace strictly longer.
const SERVER_EXIT_GRACE: Duration = Duration::from_secs(7);

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub approval_policy: Option<String>,
}

pub async fn run_tui(cwd: &Path, options: LaunchOptions) -> Result<()> {
    let (mut client, session) = Client::connect(cwd, &options).await?;
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(session);
    let result = run_event_loop(&mut terminal.terminal, &mut client, &mut app, cwd, &options).await;
    client.shutdown().await;
    result
}

pub async fn run_exec(
    cwd: &Path,
    prompt: String,
    approve_risky: bool,
    options: LaunchOptions,
) -> Result<()> {
    let (mut client, session) = Client::spawn(cwd, &options).await?;
    let result: Result<()> = async {
        let request_id = new_id();
        client
            .send(&ClientMessage::TurnStart {
                request_id,
                session_id: session.id.clone(),
                prompt,
            })
            .await?;
        let mut printed_delta = false;
        loop {
            let event = client
                .read_event()
                .await?
                .ok_or_else(|| anyhow!("server disconnected before turn completion"))?;
            match event {
                ServerEvent::AssistantDelta { content, .. } => {
                    print!("{content}");
                    stdout().flush()?;
                    printed_delta = true;
                }
                ServerEvent::ApprovalRequested {
                    approval_id,
                    name,
                    summary,
                    ..
                } => {
                    if !printed_delta {
                        eprintln!("tool approval: {name}: {summary}");
                    }
                    client
                        .send(&ClientMessage::ApprovalResolve {
                            request_id: new_id(),
                            session_id: session.id.clone(),
                            approval_id,
                            approved: approve_risky,
                        })
                        .await?;
                }
                ServerEvent::ToolCompleted {
                    name,
                    success,
                    output,
                    ..
                } if !success => eprintln!("\n{name} failed: {output}"),
                ServerEvent::TurnCompleted { .. } => {
                    if printed_delta {
                        println!();
                    }
                    break;
                }
                ServerEvent::TurnCancelled { .. } => bail!("turn cancelled"),
                ServerEvent::TurnFailed { code, message, .. } => {
                    bail!("turn failed ({code}): {message}")
                }
                ServerEvent::Error { code, message, .. } => {
                    bail!("server error ({code}): {message}")
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    client.shutdown().await;
    result
}

struct SessionInfo {
    id: String,
    cwd: String,
    model: String,
    context_max_tokens: usize,
    max_server_frame_bytes: usize,
    max_transcript_bytes: usize,
    max_transcript_items: usize,
    max_prompt_history_bytes: usize,
    max_prompt_history_items: usize,
}

struct Client {
    child: Option<Child>,
    stdin: ClientOutput,
    stdout: ClientInput,
    max_server_frame: usize,
    frame: Vec<u8>,
}

enum ClientOutput {
    Child(ChildStdin),
    Socket(OwnedWriteHalf),
}

enum ClientInput {
    Child(BufReader<ChildStdout>),
    Socket(BufReader<OwnedReadHalf>),
}

impl Client {
    async fn connect(cwd: &Path, options: &LaunchOptions) -> Result<(Self, SessionInfo)> {
        let path = scv_client::default_socket_path()?;
        Self::connect_at(&path, cwd, options).await
    }

    async fn connect_at(
        path: &Path,
        cwd: &Path,
        options: &LaunchOptions,
    ) -> Result<(Self, SessionInfo)> {
        tokio::time::timeout(SOCKET_TIMEOUT, async {
            let stream = UnixStream::connect(path).await.with_context(|| {
                format!(
                    "SCV server not started or not found at {}. Start it with `scv start --workspace {}` or run `scv run` in another terminal",
                    path.display(),
                    cwd.display()
                )
            })?;
            let (reader, writer) = stream.into_split();
            let client = Self {
                child: None,
                stdin: ClientOutput::Socket(writer),
                stdout: ClientInput::Socket(BufReader::new(reader)),
                max_server_frame: DEFAULT_SERVER_FRAME_LIMIT,
                frame: Vec::new(),
            };
            client.initialize(cwd, options).await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "server connection timed out"))?
    }

    async fn spawn(cwd: &Path, options: &LaunchOptions) -> Result<(Self, SessionInfo)> {
        let executable = std::env::current_exe().context("locate scv executable")?;
        let mut command = Command::new(executable);
        if let Some(model) = &options.model {
            command.args(["--model", model]);
        }
        if let Some(provider) = &options.provider {
            command.args(["--provider", provider]);
        }
        if let Some(base_url) = &options.base_url {
            command.args(["--base-url", base_url]);
        }
        if let Some(policy) = &options.approval_policy {
            command.args(["--approval-policy", policy]);
        }
        command
            .args(["server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().context("launch scv server")?;
        let stdin = child.stdin.take().context("server stdin unavailable")?;
        let stdout = child.stdout.take().context("server stdout unavailable")?;
        let client = Self {
            child: Some(child),
            stdin: ClientOutput::Child(stdin),
            stdout: ClientInput::Child(BufReader::new(stdout)),
            max_server_frame: DEFAULT_SERVER_FRAME_LIMIT,
            frame: Vec::new(),
        };
        client.initialize(cwd, options).await
    }

    async fn initialize(
        mut self,
        cwd: &Path,
        options: &LaunchOptions,
    ) -> Result<(Self, SessionInfo)> {
        self.send(&ClientMessage::Initialize {
            request_id: "initialize".into(),
            protocol_version: PROTOCOL_VERSION,
            client: PeerInfo {
                name: "scv-tui".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        })
        .await?;
        match self.read_event().await? {
            Some(ServerEvent::Initialized { .. }) => {}
            Some(ServerEvent::Error { code, message, .. }) => {
                bail!("server initialization failed ({code}): {message}")
            }
            other => bail!("unexpected server initialization response: {other:?}"),
        }
        self.send(&ClientMessage::SessionStart {
            request_id: "session-start".into(),
            cwd: cwd.display().to_string(),
            provider: options.provider.clone(),
            model: options.model.clone(),
            base_url: options.base_url.clone(),
            no_tools: None,
        })
        .await?;
        let session = match self.read_event().await? {
            Some(ServerEvent::SessionStarted {
                session_id,
                cwd,
                model,
                context_max_tokens,
                max_server_frame_bytes,
                max_transcript_bytes,
                max_transcript_items,
                max_prompt_history_bytes,
                max_prompt_history_items,
                ..
            }) => SessionInfo {
                id: session_id,
                cwd,
                model,
                context_max_tokens,
                max_server_frame_bytes,
                max_transcript_bytes,
                max_transcript_items,
                max_prompt_history_bytes,
                max_prompt_history_items,
            },
            Some(ServerEvent::Error { code, message, .. }) => {
                bail!("session startup failed ({code}): {message}")
            }
            other => bail!("unexpected session startup response: {other:?}"),
        };
        self.max_server_frame = session.max_server_frame_bytes;
        Ok((self, session))
    }

    async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        let bytes = serde_json::to_vec(message).context("encode client message")?;
        match &mut self.stdin {
            ClientOutput::Child(writer) => {
                writer.write_all(&bytes).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
            ClientOutput::Socket(writer) => {
                // A partial write has an unknown outcome. Drop the connection on
                // timeout; retrying the message could execute a prompt twice.
                tokio::time::timeout(SOCKET_TIMEOUT, async {
                    writer.write_all(&bytes).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await
                })
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "server write timed out"))??;
            }
        }
        Ok(())
    }

    async fn read_event(&mut self) -> Result<Option<ServerEvent>> {
        let frame = match &mut self.stdout {
            ClientInput::Child(reader) => {
                read_bounded_frame(reader, &mut self.frame, self.max_server_frame).await?
            }
            ClientInput::Socket(reader) => {
                read_bounded_frame(reader, &mut self.frame, self.max_server_frame).await?
            }
        };
        let Some(frame) = frame else {
            return Ok(None);
        };
        Ok(Some(
            serde_json::from_slice(&frame).context("decode server event")?,
        ))
    }

    async fn shutdown(&mut self) {
        match &mut self.stdin {
            ClientOutput::Child(writer) => {
                let _ = writer.shutdown().await;
            }
            ClientOutput::Socket(writer) => {
                let _ = writer.shutdown().await;
            }
        }
        if let Some(child) = &mut self.child
            && tokio::time::timeout(SERVER_EXIT_GRACE, child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

async fn reconnect_client(
    path: &Path,
    cwd: &Path,
    options: &LaunchOptions,
) -> (Client, SessionInfo) {
    loop {
        if let Ok(connection) = Client::connect_at(path, cwd, options).await {
            return connection;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

fn is_transport_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::NotConnected
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::TimedOut
        )
    })
}

async fn read_bounded_frame<R>(
    reader: &mut R,
    frame: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    // Keep partial frames on Client: select! cancels reads on every UI tick.
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server disconnected during a frame",
            )
            .into());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(count) > max_bytes.saturating_add(2) {
            bail!("server frame exceeded configured client limit");
        }
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            break;
        }
    }
    while matches!(frame.last(), Some(b'\n' | b'\r')) {
        frame.pop();
    }
    if frame.len() > max_bytes {
        bail!("server frame exceeded configured client limit");
    }
    Ok(Some(std::mem::take(frame)))
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, EnableBracketedPaste) {
            restore_terminal_state();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal_state();
                return Err(error.into());
            }
        };
        Ok(Self { terminal })
    }
}

fn restore_terminal_state() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone)]
enum TranscriptItem {
    User(String),
    Assistant {
        content: String,
        streaming: bool,
    },
    Tool {
        call_id: String,
        name: String,
        status: ToolStatus,
        arguments: String,
        output: String,
        expanded: bool,
    },
    System(String),
    Error(String),
}

impl TranscriptItem {
    fn size(&self) -> usize {
        match self {
            Self::User(value) | Self::System(value) | Self::Error(value) => value.len(),
            Self::Assistant { content, .. } => content.len(),
            Self::Tool {
                call_id,
                name,
                arguments,
                output,
                ..
            } => call_id.len() + name.len() + arguments.len() + output.len(),
        }
    }
}

#[derive(Clone, Copy)]
enum ToolStatus {
    Proposed,
    Approval,
    Running,
    Success,
    Denied,
    Cancelled,
    Failed,
}

struct PendingApproval {
    id: String,
    name: String,
    risk: String,
    cwd: String,
    summary: String,
}

struct App {
    session_id: String,
    cwd: String,
    model: String,
    context_max_tokens: usize,
    context_after_tokens: Option<usize>,
    history_bytes: Option<usize>,
    items: VecDeque<TranscriptItem>,
    items_bytes: usize,
    max_items: usize,
    max_items_bytes: usize,
    prompt_history: VecDeque<String>,
    prompt_history_bytes: usize,
    max_prompt_history_items: usize,
    max_prompt_history_bytes: usize,
    history_index: Option<usize>,
    input: String,
    cursor: usize,
    scroll: u16,
    follow_output: bool,
    running: bool,
    active_turn: Option<String>,
    started_at: Option<Instant>,
    pending_approval: Option<PendingApproval>,
    queue: VecDeque<QueueEntry>,
    queue_paused: bool,
    queue_editing: Option<QueueEntry>,
    queue_selected: Option<usize>,
    last_seq: u64,
    connected: bool,
    quit: bool,
}

impl App {
    fn new(session: SessionInfo) -> Self {
        Self {
            session_id: session.id,
            cwd: session.cwd,
            model: session.model,
            context_max_tokens: session.context_max_tokens,
            context_after_tokens: None,
            history_bytes: None,
            items: VecDeque::new(),
            items_bytes: 0,
            max_items: session.max_transcript_items,
            max_items_bytes: session.max_transcript_bytes,
            prompt_history: VecDeque::new(),
            prompt_history_bytes: 0,
            max_prompt_history_items: session.max_prompt_history_items,
            max_prompt_history_bytes: session.max_prompt_history_bytes,
            history_index: None,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow_output: true,
            running: false,
            active_turn: None,
            started_at: None,
            pending_approval: None,
            queue: VecDeque::new(),
            queue_paused: false,
            queue_editing: None,
            queue_selected: None,
            last_seq: 0,
            connected: true,
            quit: false,
        }
    }

    fn push_item(&mut self, item: TranscriptItem) {
        self.items_bytes = self.items_bytes.saturating_add(item.size());
        self.items.push_back(item);
        let mut trimmed = false;
        while self.items.len() > self.max_items || self.items_bytes > self.max_items_bytes {
            if let Some(removed) = self.items.pop_front() {
                self.items_bytes = self.items_bytes.saturating_sub(removed.size());
                trimmed = true;
            } else {
                break;
            }
        }
        if trimmed
            && !matches!(self.items.front(), Some(TranscriptItem::System(value)) if value.starts_with("Display history trimmed"))
        {
            let marker =
                TranscriptItem::System("Display history trimmed at configured limit.".into());
            self.items_bytes += marker.size();
            self.items.push_front(marker);
        }
        self.follow_output = true;
    }

    fn reconnect(&mut self, session: SessionInfo) {
        self.session_id = session.id;
        self.cwd = session.cwd;
        self.model = session.model;
        self.context_max_tokens = session.context_max_tokens;
        self.max_items = session.max_transcript_items;
        self.max_items_bytes = session.max_transcript_bytes;
        self.max_prompt_history_items = session.max_prompt_history_items;
        self.max_prompt_history_bytes = session.max_prompt_history_bytes;
        self.context_after_tokens = None;
        self.history_bytes = None;
        self.running = false;
        self.active_turn = None;
        self.started_at = None;
        self.pending_approval = None;
        self.queue.clear();
        self.queue_paused = false;
        self.queue_editing = None;
        self.queue_selected = None;
        self.last_seq = 0;
        self.connected = true;
        self.history_index = None;
        while self.prompt_history.len() > self.max_prompt_history_items
            || self.prompt_history_bytes > self.max_prompt_history_bytes
        {
            if let Some(removed) = self.prompt_history.pop_front() {
                self.prompt_history_bytes = self.prompt_history_bytes.saturating_sub(removed.len());
            } else {
                break;
            }
        }
        self.push_item(TranscriptItem::System(
            "Reconnected with a fresh session. Prior transcript is display-only; server history was not restored. Interrupted prompts and queued work were not replayed.".into(),
        ));
        self.enforce_item_limits();
    }

    fn disconnect(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        self.running = false;
        self.active_turn = None;
        self.started_at = None;
        self.pending_approval = None;
        self.queue.clear();
        self.queue_paused = false;
        self.queue_selected = None;
        if self.queue_editing.take().is_some() {
            clear_input(self);
        }
        self.context_after_tokens = None;
        self.history_bytes = None;
        self.last_seq = 0;
        self.history_index = None;
        self.finish_pending_tools(ToolStatus::Failed);
        for item in &mut self.items {
            if let TranscriptItem::Assistant { streaming, .. } = item {
                *streaming = false;
            }
        }
        self.push_item(TranscriptItem::Error(
            "Server disconnected; reconnecting. Pending work has an unknown outcome and will not be replayed. Press Ctrl+C to exit.".into(),
        ));
        self.enforce_item_limits();
    }

    fn handle_connection_error(&mut self, error: anyhow::Error) -> Result<()> {
        if !is_transport_error(&error) {
            return Err(error);
        }
        self.disconnect();
        Ok(())
    }

    fn handle_server_result(&mut self, result: Result<Option<ServerEvent>>) -> Result<()> {
        match result {
            Ok(Some(event)) => self.handle_server_event(event),
            Ok(None) => self.disconnect(),
            Err(error) => self.handle_connection_error(error)?,
        }
        Ok(())
    }

    fn add_prompt_history(&mut self, prompt: String) {
        if prompt.len() > self.max_prompt_history_bytes {
            return;
        }
        self.prompt_history_bytes += prompt.len();
        self.prompt_history.push_back(prompt);
        while self.prompt_history.len() > self.max_prompt_history_items
            || self.prompt_history_bytes > self.max_prompt_history_bytes
        {
            if let Some(removed) = self.prompt_history.pop_front() {
                self.prompt_history_bytes = self.prompt_history_bytes.saturating_sub(removed.len());
            }
        }
        self.history_index = None;
    }

    fn update_seq(&mut self, event: &ServerEvent) {
        let Some(seq) = event_seq(event) else { return };
        if self.last_seq != 0 && seq != self.last_seq + 1 {
            self.push_item(TranscriptItem::Error(format!(
                "Protocol event sequence jumped from {} to {seq}",
                self.last_seq
            )));
        }
        self.last_seq = seq;
    }

    fn handle_server_event(&mut self, event: ServerEvent) {
        self.update_seq(&event);
        match event {
            ServerEvent::QueueSnapshot {
                entries, paused, ..
            } => {
                self.queue = entries.into();
                self.queue_paused = paused;
                self.queue_selected = None;
            }
            ServerEvent::QueueEnqueued {
                entry, position, ..
            } => {
                self.queue.insert(position.min(self.queue.len()), entry);
            }
            ServerEvent::QueueUpdated { entry, .. } => {
                if let Some(existing) = self
                    .queue
                    .iter_mut()
                    .find(|existing| existing.queue_id == entry.queue_id)
                {
                    *existing = entry;
                }
            }
            ServerEvent::QueueMoved {
                queue_id,
                position,
                revision,
                ..
            } => {
                if let Some(index) = self
                    .queue
                    .iter()
                    .position(|entry| entry.queue_id == queue_id)
                    && let Some(mut entry) = self.queue.remove(index)
                {
                    entry.revision = revision;
                    self.queue.insert(position.min(self.queue.len()), entry);
                }
            }
            ServerEvent::QueueRemoved { queue_id, .. }
            | ServerEvent::QueueDequeued { queue_id, .. } => {
                if let Some(index) = self
                    .queue
                    .iter()
                    .position(|entry| entry.queue_id == queue_id)
                {
                    self.queue.remove(index);
                    self.queue_selected = self.queue_selected.and_then(|selected| {
                        if self.queue.is_empty() {
                            None
                        } else {
                            Some(selected.min(self.queue.len() - 1))
                        }
                    });
                }
            }
            ServerEvent::SessionPaused { paused, .. } => self.queue_paused = paused,
            ServerEvent::TurnStarted { turn_id, .. } => {
                self.running = true;
                self.active_turn = Some(turn_id);
                self.started_at = Some(Instant::now());
            }
            ServerEvent::AssistantDelta { content, .. } => {
                if let Some(TranscriptItem::Assistant {
                    content: current,
                    streaming: true,
                }) = self.items.back_mut()
                {
                    self.items_bytes += content.len();
                    current.push_str(&content);
                } else {
                    self.push_item(TranscriptItem::Assistant {
                        content,
                        streaming: true,
                    });
                }
            }
            ServerEvent::AssistantCompleted { content, .. } => {
                if let Some(TranscriptItem::Assistant {
                    content: current,
                    streaming,
                }) = self.items.back_mut()
                {
                    self.items_bytes = self.items_bytes.saturating_sub(current.len());
                    *current = content;
                    self.items_bytes += current.len();
                    *streaming = false;
                } else if !content.is_empty() {
                    self.push_item(TranscriptItem::Assistant {
                        content,
                        streaming: false,
                    });
                }
            }
            ServerEvent::ToolProposed {
                call_id,
                name,
                arguments,
                ..
            } => self.push_item(TranscriptItem::Tool {
                call_id,
                name,
                status: ToolStatus::Proposed,
                arguments: arguments.to_string(),
                output: String::new(),
                expanded: false,
            }),
            ServerEvent::ApprovalRequested {
                call_id,
                approval_id,
                name,
                risk,
                cwd,
                summary,
                ..
            } => {
                self.set_tool_status(&call_id, ToolStatus::Approval, None);
                self.pending_approval = Some(PendingApproval {
                    id: approval_id,
                    name,
                    risk,
                    cwd,
                    summary,
                });
            }
            ServerEvent::ToolStarted { call_id, .. } => {
                self.pending_approval = None;
                self.set_tool_status(&call_id, ToolStatus::Running, None);
            }
            ServerEvent::ToolCompleted {
                call_id,
                success,
                output,
                ..
            } => {
                self.pending_approval = None;
                self.set_tool_status(
                    &call_id,
                    if success {
                        ToolStatus::Success
                    } else if output.contains("denied by policy or user") {
                        ToolStatus::Denied
                    } else if output.to_ascii_lowercase().contains("cancelled") {
                        ToolStatus::Cancelled
                    } else {
                        ToolStatus::Failed
                    },
                    Some(output),
                );
            }
            ServerEvent::ContextCompacted {
                after_tokens,
                removed_messages,
                ..
            } => {
                self.context_after_tokens = Some(after_tokens);
                self.push_item(TranscriptItem::System(format!(
                    "Context compacted: {after_tokens} estimated tokens, {removed_messages} messages omitted."
                )));
            }
            ServerEvent::SessionTrimmed {
                removed_messages,
                history_bytes,
                ..
            } => {
                self.history_bytes = Some(history_bytes);
                self.push_item(TranscriptItem::System(format!(
                    "Session history trimmed: {removed_messages} messages removed."
                )));
            }
            ServerEvent::TurnCompleted { steps, usage, .. } => {
                self.running = false;
                self.active_turn = None;
                self.started_at = None;
                self.push_item(TranscriptItem::System(format!(
                    "Completed in {steps} step(s){}.",
                    usage
                        .input_tokens
                        .zip(usage.output_tokens)
                        .map_or_else(String::new, |(input, output)| format!(
                            " · {input}↑ {output}↓"
                        ))
                )));
            }
            ServerEvent::TurnCancelled { .. } => {
                self.finish_pending_tools(ToolStatus::Cancelled);
                self.running = false;
                self.active_turn = None;
                self.started_at = None;
                self.pending_approval = None;
                self.push_item(TranscriptItem::System("Turn cancelled.".into()));
            }
            ServerEvent::TurnFailed { code, message, .. } => {
                self.finish_pending_tools(ToolStatus::Failed);
                self.running = false;
                self.active_turn = None;
                self.started_at = None;
                self.pending_approval = None;
                self.push_item(TranscriptItem::Error(format!("{code}: {message}")));
            }
            ServerEvent::SessionCleared { .. } => {
                self.items.clear();
                self.items_bytes = 0;
                self.prompt_history.clear();
                self.prompt_history_bytes = 0;
                self.queue.clear();
            }
            ServerEvent::Error { code, message, .. } => {
                self.push_item(TranscriptItem::Error(format!("{code}: {message}")));
            }
            ServerEvent::Initialized { .. }
            | ServerEvent::SessionStarted { .. }
            | ServerEvent::DaemonStatus { .. } => {}
        }
        self.enforce_item_limits();
    }

    fn enforce_item_limits(&mut self) {
        while self.items.len() > self.max_items || self.items_bytes > self.max_items_bytes {
            if let Some(removed) = self.items.pop_front() {
                self.items_bytes = self.items_bytes.saturating_sub(removed.size());
            } else {
                break;
            }
        }
    }

    fn set_tool_status(&mut self, call_id: &str, status: ToolStatus, output: Option<String>) {
        for item in self.items.iter_mut().rev() {
            if let TranscriptItem::Tool {
                call_id: current,
                status: current_status,
                output: current_output,
                ..
            } = item
                && current == call_id
            {
                *current_status = status;
                if let Some(output) = output {
                    self.items_bytes = self.items_bytes.saturating_sub(current_output.len());
                    *current_output = output;
                    self.items_bytes += current_output.len();
                }
                break;
            }
        }
    }

    fn finish_pending_tools(&mut self, status: ToolStatus) {
        for item in &mut self.items {
            if let TranscriptItem::Tool {
                status: current, ..
            } = item
                && matches!(
                    current,
                    ToolStatus::Proposed | ToolStatus::Approval | ToolStatus::Running
                )
            {
                *current = status;
            }
        }
    }
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    client: &mut Client,
    app: &mut App,
    cwd: &Path,
    options: &LaunchOptions,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let socket_path = scv_client::default_socket_path()?;
    let mut reconnect = Box::pin(reconnect_client(&socket_path, cwd, options));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        terminal.draw(|frame| render(frame, app))?;
        if app.quit {
            return Ok(());
        }
        tokio::select! {
            _ = tick.tick() => {}
            (new_client, session) = &mut reconnect, if !app.connected => {
                *client = new_client;
                app.reconnect(session);
                reconnect = Box::pin(reconnect_client(&socket_path, cwd, options));
            }
            _ = terminate.recv() => app.quit = true,
            _ = hangup.recv() => app.quit = true,
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(client, app, key).await?;
                        if !app.connected {
                            client.shutdown().await;
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {},
                    Some(Ok(_)) => {},
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
            server_event = client.read_event(), if app.connected => {
                app.handle_server_result(server_event)?;
                if !app.connected {
                    client.shutdown().await;
                }
            }
        }
    }
}

async fn handle_key(client: &mut Client, app: &mut App, key: KeyEvent) -> Result<()> {
    if !app.connected {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            app.quit = true;
        }
        return Ok(());
    }
    if let Err(error) = handle_connected_key(client, app, key).await {
        if is_transport_error(&error) {
            clear_input(app);
        }
        app.handle_connection_error(error)?;
    }
    Ok(())
}

async fn handle_connected_key(client: &mut Client, app: &mut App, key: KeyEvent) -> Result<()> {
    if let Some(approval) = &app.pending_approval {
        let approved = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        if let Some(approved) = approved {
            let id = approval.id.clone();
            client
                .send(&ClientMessage::ApprovalResolve {
                    request_id: new_id(),
                    session_id: app.session_id.clone(),
                    approval_id: id,
                    approved,
                })
                .await?;
            app.pending_approval = None;
        }
        return Ok(());
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                if !app.input.is_empty() {
                    app.input.clear();
                    app.cursor = 0;
                } else if app.running {
                    cancel_turn(client, app).await?;
                } else {
                    app.quit = true;
                }
                return Ok(());
            }
            KeyCode::Char('j') => {
                insert_char(app, '\n');
                return Ok(());
            }
            KeyCode::Char('o') => {
                for item in app.items.iter_mut().rev() {
                    if let TranscriptItem::Tool { expanded, .. } = item {
                        *expanded = !*expanded;
                        break;
                    }
                }
                return Ok(());
            }
            KeyCode::Char('x') if app.running && app.queue_selected.is_some() => {
                let index = app.queue_selected.unwrap();
                if let Some(entry) = app.queue.get(index).cloned() {
                    client
                        .send(&ClientMessage::QueueRemove {
                            request_id: new_id(),
                            session_id: app.session_id.clone(),
                            queue_id: entry.queue_id,
                            revision: entry.revision,
                        })
                        .await?;
                }
                return Ok(());
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter => submit_input(client, app).await?,
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) && app.running => {
            let index = app
                .queue_selected
                .unwrap_or_else(|| app.queue.len().saturating_sub(1));
            if let Some(entry) = app.queue.get(index).cloned() {
                app.input = entry.prompt.clone();
                app.cursor = app.input.chars().count();
                app.queue_editing = Some(entry);
                app.queue_selected = Some(index);
            }
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) && app.running => {
            if !app.queue.is_empty() {
                let index = app
                    .queue_selected
                    .map_or(0, |index| (index + 1).min(app.queue.len() - 1));
                app.queue_selected = Some(index);
                if let Some(entry) = app.queue.get(index).cloned() {
                    app.input = entry.prompt.clone();
                    app.cursor = app.input.chars().count();
                    app.queue_editing = Some(entry);
                }
            }
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::ALT) && app.running => {
            client
                .send(&ClientMessage::SessionPause {
                    request_id: new_id(),
                    session_id: app.session_id.clone(),
                    paused: !app.queue_paused,
                })
                .await?;
        }
        KeyCode::Char(character) => insert_char(app, character),
        KeyCode::Backspace => backspace(app),
        KeyCode::Delete => delete(app),
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.chars().count()),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => {
            app.cursor = app.input.chars().count();
            app.scroll = 0;
            app.follow_output = true;
        }
        KeyCode::Up if !app.input.contains('\n') => recall_history(app, true),
        KeyCode::Down if !app.input.contains('\n') => recall_history(app, false),
        KeyCode::PageUp => {
            app.scroll = app.scroll.saturating_add(10);
            app.follow_output = false;
        }
        KeyCode::PageDown => {
            app.scroll = app.scroll.saturating_sub(10);
            app.follow_output = app.scroll == 0;
        }
        KeyCode::Esc if app.running => cancel_turn(client, app).await?,
        _ => {}
    }
    Ok(())
}

async fn submit_input(client: &mut Client, app: &mut App) -> Result<()> {
    let prompt = app.input.trim().to_owned();
    if prompt.is_empty() {
        return Ok(());
    }
    match prompt.as_str() {
        "/quit" => {
            app.quit = true;
            return Ok(());
        }
        "/help" => {
            app.push_item(TranscriptItem::System(
                "Enter send · Ctrl+J newline · Esc cancel · Ctrl+O tool details · PageUp/PageDown scroll · /clear · /context · /quit".into(),
            ));
            clear_input(app);
            return Ok(());
        }
        "/context" => {
            let selected = app.context_after_tokens.map_or_else(
                || "not compacted".into(),
                |value| format!("~{value} selected"),
            );
            let history = app
                .history_bytes
                .map_or_else(|| "within limit".into(), |value| format!("{value} bytes"));
            app.push_item(TranscriptItem::System(format!(
                "Context: {selected} / {} tokens · history {history}",
                app.context_max_tokens
            )));
            clear_input(app);
            return Ok(());
        }
        "/clear" => {
            client
                .send(&ClientMessage::SessionClear {
                    request_id: new_id(),
                    session_id: app.session_id.clone(),
                })
                .await?;
            clear_input(app);
            return Ok(());
        }
        _ => {}
    }
    if let Some(entry) = app.queue_editing.take() {
        client
            .send(&ClientMessage::QueueUpdate {
                request_id: new_id(),
                session_id: app.session_id.clone(),
                queue_id: entry.queue_id,
                revision: entry.revision,
                prompt,
            })
            .await?;
    } else {
        client
            .send(&ClientMessage::TurnStart {
                request_id: new_id(),
                session_id: app.session_id.clone(),
                prompt: prompt.clone(),
            })
            .await?;
        if !app.running {
            app.push_item(TranscriptItem::User(prompt.clone()));
            app.add_prompt_history(prompt);
            app.running = true;
            app.started_at = Some(Instant::now());
        }
        app.queue_selected = None;
    }
    clear_input(app);
    Ok(())
}

async fn cancel_turn(client: &mut Client, app: &mut App) -> Result<()> {
    if let Some(turn_id) = &app.active_turn {
        client
            .send(&ClientMessage::TurnCancel {
                request_id: new_id(),
                session_id: app.session_id.clone(),
                turn_id: turn_id.clone(),
            })
            .await?;
    }
    Ok(())
}

fn clear_input(app: &mut App) {
    app.input.clear();
    app.cursor = 0;
    app.history_index = None;
}

fn insert_char(app: &mut App, character: char) {
    let index = byte_index(&app.input, app.cursor);
    app.input.insert(index, character);
    app.cursor += 1;
    app.history_index = None;
}

fn backspace(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    let end = byte_index(&app.input, app.cursor);
    let start = byte_index(&app.input, app.cursor - 1);
    app.input.replace_range(start..end, "");
    app.cursor -= 1;
}

fn delete(app: &mut App) {
    if app.cursor >= app.input.chars().count() {
        return;
    }
    let start = byte_index(&app.input, app.cursor);
    let end = byte_index(&app.input, app.cursor + 1);
    app.input.replace_range(start..end, "");
}

fn byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map_or(value.len(), |(index, _)| index)
}

fn recall_history(app: &mut App, older: bool) {
    if app.prompt_history.is_empty() {
        return;
    }
    let next = match (app.history_index, older) {
        (None, true) => app.prompt_history.len() - 1,
        (Some(index), true) => index.saturating_sub(1),
        (Some(index), false) if index + 1 < app.prompt_history.len() => index + 1,
        (_, false) => {
            clear_input(app);
            return;
        }
    };
    app.history_index = Some(next);
    app.input = app.prompt_history[next].clone();
    app.cursor = app.input.chars().count();
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let input_lines = app.input.lines().count().max(1) as u16;
    let composer_height = (input_lines + 2).clamp(3, 8);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let connection = if app.connected {
        "connected"
    } else {
        "disconnected"
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " SCV ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(&app.model, Style::default().fg(Color::Cyan)),
        Span::raw("  ·  "),
        Span::raw(short_path(&app.cwd, 60)),
        Span::raw("  ·  "),
        Span::styled(
            connection,
            if app.connected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Red)
            },
        ),
    ]))
    .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, layout[0]);

    let transcript = transcript_text(app);
    let raw_lines = transcript.lines.len().min(usize::from(u16::MAX)) as u16;
    let visible = layout[1].height.saturating_sub(2);
    let bottom = raw_lines.saturating_sub(visible);
    let scroll = if app.follow_output {
        bottom
    } else {
        bottom.saturating_sub(app.scroll)
    };
    let transcript_widget = Paragraph::new(transcript)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(transcript_widget, layout[1]);

    let composer_title = if app.running {
        " working · Esc cancels "
    } else {
        " message "
    };
    let composer = Paragraph::new(app.input.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(composer_title)
                .border_style(if app.running {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Cyan)
                }),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(composer, layout[2]);
    if app.pending_approval.is_none() {
        let before_cursor: String = app.input.chars().take(app.cursor).collect();
        let cursor_row = before_cursor.matches('\n').count() as u16;
        let cursor_col = before_cursor
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .chars()
            .count() as u16;
        let x = layout[2].x + 1 + cursor_col.min(layout[2].width.saturating_sub(2));
        let y = layout[2].y + 1 + cursor_row.min(layout[2].height.saturating_sub(2));
        frame.set_cursor_position((x, y));
    }

    let elapsed = app
        .started_at
        .map(|start| format!(" · {:.1}s", start.elapsed().as_secs_f32()))
        .unwrap_or_default();
    let context = app.context_after_tokens.map_or_else(
        || format!("ctx ≤{}", app.context_max_tokens),
        |tokens| format!("ctx ~{tokens}/{}", app.context_max_tokens),
    );
    let footer = Paragraph::new(format!(
        " {}{}  ·  {}  ·  {} queue{}  ·  Enter send  Ctrl+J newline  /help",
        if app.running { "working" } else { "ready" },
        elapsed,
        context,
        app.queue.len(),
        if app.queue_paused { " paused" } else { "" },
    ))
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, layout[3]);

    if let Some(approval) = &app.pending_approval {
        render_approval(frame, approval);
    }
}

fn transcript_text(app: &App) -> Text<'static> {
    let mut lines = Vec::new();
    if app.items.is_empty() {
        lines.push(Line::styled(
            "Ask SCV to inspect, change, or explain this workspace.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !app.queue.is_empty() {
        lines.push(Line::styled(
            format!(
                "queue ({}{})",
                app.queue.len(),
                if app.queue_paused { ", paused" } else { "" }
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        for (index, entry) in app.queue.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "  {}. {}",
                    index + 1,
                    bounded_text(&entry.prompt, 180).replace('\n', " ↵ ")
                ),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::raw(""));
    }
    for item in &app.items {
        match item {
            TranscriptItem::User(content) => push_content(
                &mut lines,
                "you",
                content,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TranscriptItem::Assistant { content, streaming } => {
                let label = if *streaming { "scv…" } else { "scv" };
                push_content(
                    &mut lines,
                    label,
                    content,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                );
            }
            TranscriptItem::Tool {
                name,
                status,
                arguments,
                output,
                expanded,
                ..
            } => {
                let (symbol, color) = match status {
                    ToolStatus::Proposed => ("○", Color::DarkGray),
                    ToolStatus::Approval => ("?", Color::Yellow),
                    ToolStatus::Running => ("●", Color::Yellow),
                    ToolStatus::Success => ("✓", Color::Green),
                    ToolStatus::Denied => ("⊘", Color::Yellow),
                    ToolStatus::Cancelled => ("■", Color::DarkGray),
                    ToolStatus::Failed => ("✗", Color::Red),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{symbol} {name}"),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", bounded_text(arguments, 160)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                if *expanded && !output.is_empty() {
                    for line in bounded_text(output, 4000).lines() {
                        lines.push(Line::styled(
                            format!("  {line}"),
                            Style::default().fg(Color::Gray),
                        ));
                    }
                }
            }
            TranscriptItem::System(content) => lines.push(Line::styled(
                format!("· {content}"),
                Style::default().fg(Color::DarkGray),
            )),
            TranscriptItem::Error(content) => lines.push(Line::styled(
                format!("! {content}"),
                Style::default().fg(Color::Red),
            )),
        }
        lines.push(Line::raw(""));
    }
    Text::from(lines)
}

fn push_content(lines: &mut Vec<Line<'static>>, label: &str, content: &str, style: Style) {
    lines.push(Line::styled(label.to_owned(), style));
    if content.is_empty() {
        lines.push(Line::raw(""));
    } else {
        lines.extend(content.lines().map(|line| Line::raw(line.to_owned())));
    }
}

fn render_approval(frame: &mut Frame<'_>, approval: &PendingApproval) {
    let area = centered_rect(80, 60, frame.area());
    frame.render_widget(Clear, area);
    let content = format!(
        "Tool: {}\nRisk: {}\nWorkspace: {}\n\n{}\n\n[y] allow once    [n/Esc] deny",
        approval.name,
        approval.risk,
        approval.cwd,
        bounded_text(&approval.summary, 4000)
    );
    let widget = Paragraph::new(content)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" approval required ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
    frame.render_widget(widget, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn event_seq(event: &ServerEvent) -> Option<u64> {
    match event {
        ServerEvent::TurnStarted { seq, .. }
        | ServerEvent::AssistantDelta { seq, .. }
        | ServerEvent::AssistantCompleted { seq, .. }
        | ServerEvent::ToolProposed { seq, .. }
        | ServerEvent::ApprovalRequested { seq, .. }
        | ServerEvent::ToolStarted { seq, .. }
        | ServerEvent::ToolCompleted { seq, .. }
        | ServerEvent::ContextCompacted { seq, .. }
        | ServerEvent::SessionTrimmed { seq, .. }
        | ServerEvent::SessionCleared { seq, .. }
        | ServerEvent::TurnCompleted { seq, .. }
        | ServerEvent::TurnCancelled { seq, .. }
        | ServerEvent::TurnFailed { seq, .. } => Some(*seq),
        ServerEvent::QueueSnapshot { seq, .. }
        | ServerEvent::QueueEnqueued { seq, .. }
        | ServerEvent::QueueUpdated { seq, .. }
        | ServerEvent::QueueMoved { seq, .. }
        | ServerEvent::QueueRemoved { seq, .. }
        | ServerEvent::QueueDequeued { seq, .. }
        | ServerEvent::SessionPaused { seq, .. } => Some(*seq),
        ServerEvent::Initialized { .. }
        | ServerEvent::SessionStarted { .. }
        | ServerEvent::DaemonStatus { .. }
        | ServerEvent::Error { .. } => None,
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        let mut output: String = value.chars().take(max_chars).collect();
        output.push('…');
        output
    }
}

fn short_path(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        let tail: String = value
            .chars()
            .rev()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use ratatui::backend::TestBackend;
    use tokio::net::UnixListener;

    struct SocketPath(PathBuf);

    impl SocketPath {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("scv-tui-{}.sock", new_id())))
        }
    }

    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn receive(peer: &mut BufReader<UnixStream>) -> ClientMessage {
        let mut line = String::new();
        let count = tokio::time::timeout(Duration::from_secs(2), peer.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(count, 0, "client closed before sending a message");
        serde_json::from_str(&line).unwrap()
    }

    async fn emit(peer: &mut BufReader<UnixStream>, event: ServerEvent) {
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n');
        peer.get_mut().write_all(&bytes).await.unwrap();
    }

    async fn accept_session(listener: &UnixListener, id: &str) -> BufReader<UnixStream> {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(6), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut peer = BufReader::new(stream);
        assert!(matches!(
            receive(&mut peer).await,
            ClientMessage::Initialize { .. }
        ));
        emit(
            &mut peer,
            ServerEvent::Initialized {
                request_id: "initialize".into(),
                protocol_version: PROTOCOL_VERSION,
                server: PeerInfo {
                    name: "test".into(),
                    version: "0".into(),
                },
            },
        )
        .await;
        assert!(matches!(
            receive(&mut peer).await,
            ClientMessage::SessionStart { .. }
        ));
        emit(
            &mut peer,
            ServerEvent::SessionStarted {
                request_id: "session-start".into(),
                session_id: id.into(),
                cwd: "/tmp".into(),
                model: "test".into(),
                context_max_tokens: 100,
                max_server_frame_bytes: 65536,
                max_transcript_bytes: 16384,
                max_transcript_items: 100,
                max_prompt_history_bytes: 1024,
                max_prompt_history_items: 10,
            },
        )
        .await;
        peer
    }

    async fn connected(
        path: &SocketPath,
        listener: &UnixListener,
    ) -> (Client, App, BufReader<UnixStream>) {
        let options = LaunchOptions::default();
        let (connection, peer) = tokio::join!(
            Client::connect_at(&path.0, Path::new("/tmp"), &options),
            accept_session(listener, "old-session"),
        );
        let (client, session) = connection.unwrap();
        (client, App::new(session), peer)
    }

    fn pending_work(app: &mut App) {
        app.running = true;
        app.active_turn = Some("old-turn".into());
        app.started_at = Some(Instant::now());
        app.pending_approval = Some(PendingApproval {
            id: "old-approval".into(),
            name: "test".into(),
            risk: "high".into(),
            cwd: "/tmp".into(),
            summary: "pending".into(),
        });
        app.queue.push_back(QueueEntry {
            queue_id: "old-queue".into(),
            revision: 1,
            prompt: "queued work".into(),
            submitter: "test".into(),
        });
        app.queue_paused = true;
        app.queue_selected = Some(0);
        app.queue_editing = app.queue.front().cloned();
        app.input = "edited queue prompt".into();
        app.cursor = app.input.len();
        app.context_after_tokens = Some(42);
        app.history_bytes = Some(100);
        app.last_seq = 9;
        app.push_item(TranscriptItem::Tool {
            call_id: "old-tool".into(),
            name: "test".into(),
            status: ToolStatus::Running,
            arguments: String::new(),
            output: String::new(),
            expanded: false,
        });
        app.push_item(TranscriptItem::Assistant {
            content: "partial answer".into(),
            streaming: true,
        });
    }

    fn assert_disconnected(app: &App) {
        assert!(!app.connected);
        assert!(!app.running);
        assert!(app.active_turn.is_none());
        assert!(app.started_at.is_none());
        assert!(app.pending_approval.is_none());
        assert!(app.queue.is_empty());
        assert!(!app.queue_paused);
        assert!(app.queue_editing.is_none());
        assert!(app.queue_selected.is_none());
        assert!(app.context_after_tokens.is_none());
        assert!(app.history_bytes.is_none());
        assert_eq!(app.last_seq, 0);
        assert!(!app.items.iter().any(|item| matches!(
            item,
            TranscriptItem::Assistant {
                streaming: true,
                ..
            } | TranscriptItem::Tool {
                status: ToolStatus::Running | ToolStatus::Approval | ToolStatus::Proposed,
                ..
            }
        )));
    }

    async fn assert_fresh_session_no_replay(
        client: &mut Client,
        app: &mut App,
        peer: &mut BufReader<UnixStream>,
    ) {
        assert!(app.connected);
        assert_eq!(app.session_id, "new-session");
        assert!(app.items.iter().any(|item| matches!(item,
            TranscriptItem::System(text) if text.contains("fresh session") && text.contains("history was not restored")
        )));
        handle_key(
            client,
            app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        app.input = "fresh prompt".into();
        handle_key(
            client,
            app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert!(
            matches!(receive(peer).await, ClientMessage::TurnStart { session_id, prompt, .. }
            if session_id == "new-session" && prompt == "fresh prompt")
        );
        let mut line = String::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), peer.read_line(&mut line))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connected_client_reconnects_after_daemon_restart_without_replaying_work() {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let (mut client, mut app, mut peer) = connected(&path, &listener).await;
        app.input = "interrupted prompt".into();
        handle_key(
            &mut client,
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert!(matches!(
            receive(&mut peer).await,
            ClientMessage::TurnStart { .. }
        ));
        pending_work(&mut app);
        drop(peer);
        drop(listener);
        std::fs::remove_file(&path.0).unwrap();
        app.handle_server_result(client.read_event().await).unwrap();
        assert_disconnected(&app);
        assert!(app.input.is_empty());
        assert_eq!(app.prompt_history.front().unwrap(), "interrupted prompt");
        handle_key(
            &mut client,
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();

        let options = LaunchOptions::default();
        let (connection, mut peer) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                reconnect_client(&path.0, Path::new("/tmp"), &options),
                async {
                    // The first retry sees a missing socket, as during a restart.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let listener = UnixListener::bind(&path.0).unwrap();
                    accept_session(&listener, "new-session").await
                }
            )
        })
        .await
        .unwrap();
        let (new_client, session) = connection;
        client = new_client;
        app.reconnect(session);
        assert_fresh_session_no_replay(&mut client, &mut app, &mut peer).await;
    }

    #[tokio::test]
    async fn socket_write_failures_clear_pending_commands_and_never_replay_them() {
        for action in ["prompt", "queue", "approval", "cancel", "clear"] {
            let path = SocketPath::new();
            let listener = UnixListener::bind(&path.0).unwrap();
            let (mut client, mut app, peer) = connected(&path, &listener).await;
            pending_work(&mut app);
            let key = match action {
                "approval" => KeyCode::Char('y'),
                "cancel" => {
                    app.pending_approval = None;
                    KeyCode::Esc
                }
                _ => {
                    app.pending_approval = None;
                    if action != "queue" {
                        app.queue_editing = None;
                    }
                    app.input = if action == "clear" {
                        "/clear"
                    } else {
                        "interrupted prompt"
                    }
                    .into();
                    KeyCode::Enter
                }
            };
            drop(peer);
            handle_key(
                &mut client,
                &mut app,
                KeyEvent::new(key, KeyModifiers::NONE),
            )
            .await
            .unwrap();
            assert_disconnected(&app);
            assert!(app.input.is_empty(), "{action}");
            let options = LaunchOptions::default();
            let ((new_client, session), mut peer) = tokio::join!(
                reconnect_client(&path.0, Path::new("/tmp"), &options),
                accept_session(&listener, "new-session"),
            );
            client = new_client;
            app.reconnect(session);
            assert_fresh_session_no_replay(&mut client, &mut app, &mut peer).await;
        }
    }

    #[tokio::test]
    async fn stalled_socket_write_times_out_without_replaying_a_partial_prompt() {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let (mut client, mut app, peer) = connected(&path, &listener).await;
        // The peer stays alive without draining its socket, forcing a partial write.
        app.input = "x".repeat(8 * 1024 * 1024);
        tokio::time::timeout(
            SOCKET_TIMEOUT + Duration::from_secs(2),
            handle_key(
                &mut client,
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_disconnected(&app);
        assert!(app.input.is_empty());
        client.shutdown().await;
        drop(peer);
        let options = LaunchOptions::default();
        let ((new_client, session), mut peer) = tokio::join!(
            reconnect_client(&path.0, Path::new("/tmp"), &options),
            accept_session(&listener, "new-session"),
        );
        client = new_client;
        app.reconnect(session);
        assert_fresh_session_no_replay(&mut client, &mut app, &mut peer).await;
    }

    #[tokio::test]
    async fn partial_socket_eof_is_recoverable_and_preserves_unsent_draft() {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let (mut client, mut app, mut peer) = connected(&path, &listener).await;
        app.input = "unsent draft".into();
        peer.get_mut().write_all(b"{\"type\":").await.unwrap();
        drop(peer);
        let error = client.read_event().await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        app.handle_server_result(Err(error)).unwrap();
        assert_disconnected(&app);
        assert_eq!(app.input, "unsent draft");
        handle_key(
            &mut client,
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        )
        .await
        .unwrap();
        assert!(app.quit);
    }

    #[tokio::test]
    async fn socket_frames_survive_cancelled_reads_and_protocol_errors_are_not_retried() {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let (mut client, mut app, mut peer) = connected(&path, &listener).await;
        let event = ServerEvent::QueueSnapshot {
            request_id: None,
            session_id: app.session_id.clone(),
            seq: 1,
            entries: vec![],
            paused: false,
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        let middle = bytes.len() / 2;
        peer.get_mut().write_all(&bytes[..middle]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), client.read_event())
                .await
                .is_err()
        );
        peer.get_mut().write_all(&bytes[middle..]).await.unwrap();
        peer.get_mut().write_all(b"\n").await.unwrap();
        assert_eq!(client.read_event().await.unwrap(), Some(event));
        peer.get_mut().write_all(b"not json\n").await.unwrap();
        assert!(app.handle_server_result(client.read_event().await).is_err());
        assert!(app.connected);
    }

    #[tokio::test]
    async fn reconnect_retries_a_stalled_handshake_with_a_bounded_wait() {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let options = LaunchOptions::default();
        let ((_, session), _peer) = tokio::time::timeout(
            SOCKET_TIMEOUT + RECONNECT_DELAY + Duration::from_secs(2),
            async {
                tokio::join!(
                    reconnect_client(&path.0, Path::new("/tmp"), &options),
                    async {
                        let (stalled, _) = listener.accept().await.unwrap();
                        let mut stalled = BufReader::new(stalled);
                        assert!(matches!(
                            receive(&mut stalled).await,
                            ClientMessage::Initialize { .. }
                        ));
                        let peer = accept_session(&listener, "new-session").await;
                        let mut line = String::new();
                        assert_eq!(stalled.read_line(&mut line).await.unwrap(), 0);
                        peer
                    }
                )
            },
        )
        .await
        .unwrap();
        assert_eq!(session.id, "new-session");
    }

    #[test]
    fn transient_io_errors_are_distinct_from_protocol_errors() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::TimedOut,
        ] {
            assert!(is_transport_error(
                &anyhow::Error::new(io::Error::from(kind)).context("socket")
            ));
        }
        assert!(!is_transport_error(&anyhow!("invalid protocol event")));
        assert!(!is_transport_error(
            &io::Error::from(io::ErrorKind::PermissionDenied).into()
        ));
    }

    fn app() -> App {
        App::new(SessionInfo {
            id: "s".into(),
            cwd: "/tmp".into(),
            model: "test".into(),
            context_max_tokens: 100,
            max_server_frame_bytes: DEFAULT_SERVER_FRAME_LIMIT,
            max_transcript_bytes: 20,
            max_transcript_items: 3,
            max_prompt_history_bytes: 10,
            max_prompt_history_items: 2,
        })
    }

    #[test]
    fn prompt_history_is_bounded() {
        let mut app = app();
        app.add_prompt_history("one".into());
        app.add_prompt_history("two".into());
        app.add_prompt_history("three".into());
        assert_eq!(app.prompt_history.len(), 2);
        assert_eq!(app.prompt_history.front().unwrap(), "two");
    }

    #[test]
    fn editor_handles_unicode_boundaries() {
        let mut app = app();
        insert_char(&mut app, '界');
        insert_char(&mut app, 'a');
        app.cursor = 1;
        backspace(&mut app);
        assert_eq!(app.input, "a");
    }

    #[test]
    fn renders_the_primary_terminal_regions() {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let app = app();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let contents = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(contents.contains("SCV"));
        assert!(contents.contains("connected"));
        assert!(contents.contains("message"));
        assert!(contents.contains("Enter send"));
    }

    #[tokio::test]
    async fn client_reader_rejects_frames_before_unbounded_allocation() {
        let mut reader = BufReader::new(Cursor::new(format!("{}\n", "x".repeat(32))));
        let error = read_bounded_frame(&mut reader, &mut Vec::new(), 8)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded"));
    }

    #[test]
    fn server_clear_is_authoritative_for_display_and_prompt_history() {
        let mut app = app();
        app.push_item(TranscriptItem::User("hello".into()));
        app.add_prompt_history("hello".into());
        app.handle_server_event(ServerEvent::SessionCleared {
            request_id: "clear".into(),
            session_id: "s".into(),
            seq: 1,
        });
        assert!(app.items.is_empty());
        assert!(app.prompt_history.is_empty());
        assert_eq!(app.last_seq, 1);
    }
}
