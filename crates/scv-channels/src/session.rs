//! A channel conversation's session on the single SCV Unix-socket daemon.

use anyhow::{Context, Result, anyhow, bail};
use scv_protocol::{
    Attachment, ClientMessage, PROTOCOL_VERSION, PeerInfo, ReplyAttachment, ServerEvent,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{
    UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
};
use uuid::Uuid;

/// A turn's answer: its text and the files the model attached to it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub files: Vec<ReplyAttachment>,
}

impl Reply {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            files: Vec::new(),
        }
    }
}

pub struct Session {
    stdin: OwnedWriteHalf,
    stdout: BufReader<OwnedReadHalf>,
    /// The part of a frame read so far, kept here so a read can be abandoned
    /// (while waiting for either a message or a background report) and
    /// resumed without losing bytes.
    partial: Vec<u8>,
    pub session_id: String,
    pub last_used: Instant,
    /// Owner sessions run with tools and approve their requests; others have
    /// no tools and deny any approval request.
    pub tools: bool,
    /// This client's turn in progress: its request ID and, once started, its
    /// turn ID.
    current: Option<(String, Option<String>)>,
    /// Turns the server started itself (background reports), by request ID,
    /// with the answer so far.
    server_turns: HashMap<String, Reply>,
    /// This client's abandoned turns, whose late events are ignored.
    stale: HashSet<String>,
    /// Finished server-started turns' answers, waiting to be sent.
    reports: VecDeque<Reply>,
    /// Files the model attached during this client's current turn.
    files: Vec<ReplyAttachment>,
    /// Background jobs this session started that have not been reported.
    background: HashMap<String, JobInfo>,
    /// What this turn's agent calls were asked to do, by call ID, until they
    /// return.
    tasks: HashMap<String, String>,
    /// A read or write failed, so the session cannot be reused.
    broken: bool,
}

impl Session {
    pub async fn spawn(workspace: &Path) -> Result<Self> {
        Self::connect(&scv_client::default_socket_path()?, workspace, false, None).await
    }

    /// Start a session for a conversation on `channel` (its user-facing
    /// name, such as `WeChat`), so the model knows it is answering a chat
    /// and, with tools, may attach files to its replies.
    pub async fn connect(
        socket: &Path,
        workspace: &Path,
        tools: bool,
        channel: Option<&str>,
    ) -> Result<Self> {
        let stream = UnixStream::connect(socket).await.with_context(|| {
            format!(
                "SCV server not started or not found at {}. Start it with `scv start` or `scv run`",
                socket.display()
            )
        })?;
        let (reader, writer) = stream.into_split();
        let mut session = Self {
            stdin: writer,
            stdout: BufReader::new(reader),
            partial: Vec::new(),
            session_id: String::new(),
            last_used: Instant::now(),
            tools,
            current: None,
            server_turns: HashMap::new(),
            stale: HashSet::new(),
            reports: VecDeque::new(),
            files: Vec::new(),
            background: HashMap::new(),
            tasks: HashMap::new(),
            broken: false,
        };
        write(
            &mut session.stdin,
            &ClientMessage::Initialize {
                request_id: "channel-init".into(),
                protocol_version: PROTOCOL_VERSION,
                client: PeerInfo {
                    name: "scv-channels".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            },
        )
        .await?;
        match session.event().await? {
            ServerEvent::Initialized {
                protocol_version, ..
            } if protocol_version == PROTOCOL_VERSION => {}
            ServerEvent::Error { message, .. } => {
                bail!("channel protocol initialization failed: {message}")
            }
            _ => bail!("channel protocol initialization failed"),
        }
        write(
            &mut session.stdin,
            &ClientMessage::SessionStart {
                request_id: "channel-session".into(),
                cwd: workspace.display().to_string(),
                provider: None,
                model: None,
                base_url: None,
                no_tools: Some(!tools),
                delegation_depth: None,
                channel: channel.map(str::to_owned),
                // This client approves every request of an owner session
                // (and a tool-free one makes none), so background jobs may
                // get the same answer without a turn to carry it.
                auto_approve: Some(tools),
            },
        )
        .await?;
        loop {
            match session.event().await? {
                ServerEvent::SessionStarted { session_id, .. } => {
                    session.session_id = session_id;
                    break;
                }
                ServerEvent::Error { message, .. } => {
                    bail!("channel protocol session failed: {message}")
                }
                _ => {}
            }
        }
        Ok(session)
    }

    /// Read the next event. Cancel-safe: a partly read frame stays in
    /// `partial` for the next call.
    async fn event(&mut self) -> Result<ServerEvent> {
        let result = self.read_event().await;
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    async fn read_event(&mut self) -> Result<ServerEvent> {
        loop {
            let buf = self.stdout.fill_buf().await?;
            if buf.is_empty() {
                bail!("SCV server closed the channel session")
            }
            let take = buf
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buf.len(), |index| index + 1);
            if self.partial.len() + take > 8 * 1024 * 1024 {
                bail!("channel protocol frame exceeds limit")
            }
            self.partial.extend_from_slice(&buf[..take]);
            self.stdout.consume(take);
            if self.partial.last() == Some(&b'\n') {
                break;
            }
        }
        let line = std::mem::take(&mut self.partial);
        serde_json::from_slice(&line).context("decode SCV protocol event")
    }

    async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        let result = write(&mut self.stdin, message).await;
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    /// Whether a read or write failed, so the session must be replaced.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Background jobs started here and not yet reported.
    pub fn background_jobs(&self) -> usize {
        self.background.len()
    }

    /// Whether finished background reports are waiting to be sent.
    pub fn has_reports(&self) -> bool {
        !self.reports.is_empty()
    }

    /// Whether a turn the server started (a background report) is running.
    pub fn reporting(&self) -> bool {
        !self.server_turns.is_empty()
    }

    /// Background work whose report has not been handed over yet: running
    /// or unreported jobs, report turns, and finished reports.
    pub fn pending_work(&self) -> usize {
        self.background.len() + self.server_turns.len() + self.reports.len()
    }

    /// The background jobs this session runs and has not reported, by handle.
    pub fn jobs(&self) -> Vec<(String, JobInfo)> {
        let mut jobs: Vec<_> = self
            .background
            .iter()
            .map(|(job, info)| (job.clone(), info.clone()))
            .collect();
        jobs.sort();
        jobs
    }

    /// Answers of background reports that finished during `turn`.
    pub fn take_reports(&mut self) -> Vec<Reply> {
        self.reports.drain(..).collect()
    }

    /// Wait for the next background report while no turn of ours runs.
    /// Cancel-safe.
    pub async fn next_report(&mut self) -> Result<Reply> {
        loop {
            if let Some(report) = self.reports.pop_front() {
                return Ok(report);
            }
            let event = self.event().await?;
            self.observe(&event);
            self.absorb(event).await?;
        }
    }

    /// Track background jobs and server-started turns in any event.
    fn observe(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::ToolProposed {
                call_id,
                name,
                arguments,
                ..
            } if name.starts_with("agent_") => {
                let prompt = arguments
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if self.tasks.len() < MAX_TRACKED_CALLS {
                    self.tasks.insert(call_id.clone(), task_line(prompt));
                }
            }
            ServerEvent::ToolCompleted {
                request_id,
                call_id,
                name,
                success,
                output,
                ..
            } => {
                let task = self.tasks.remove(call_id).unwrap_or_default();
                let update = scv_protocol::background_job_update(output);
                for job in update.started {
                    self.background.insert(
                        job,
                        JobInfo {
                            tool: name.clone(),
                            task: task.clone(),
                        },
                    );
                }
                for job in update.settled {
                    self.background.remove(&job);
                }
                if let Some(file) = scv_protocol::reply_attachment(name, *success, output) {
                    // Like its text, a file belongs to a server-started turn,
                    // an abandoned one (dropped), or else the current turn.
                    if let Some(report) = self.server_turns.get_mut(request_id) {
                        report.files.push(file);
                    } else if !self.stale.contains(request_id) && self.current.is_some() {
                        self.files.push(file);
                    }
                }
            }
            ServerEvent::TurnStarted {
                request_id,
                origin: Some(origin),
                ..
            } => {
                self.server_turns
                    .insert(request_id.clone(), Reply::default());
                for job in &origin.jobs {
                    self.background.remove(job);
                }
            }
            ServerEvent::TurnStarted {
                request_id,
                turn_id,
                origin: None,
                ..
            } => {
                if let Some((current, started)) = &mut self.current
                    && current == request_id
                {
                    *started = Some(turn_id.clone());
                }
            }
            _ => {}
        }
    }

    /// Whether `event` belongs to a turn other than this client's current one.
    fn is_other(&self, event: &ServerEvent) -> bool {
        event.turn_request_id().is_some_and(|request| {
            self.server_turns.contains_key(request) || self.stale.contains(request)
        })
    }

    /// Handle an event of a server-started or abandoned turn.
    async fn absorb(&mut self, event: ServerEvent) -> Result<()> {
        let Some(request) = event.turn_request_id().map(str::to_owned) else {
            return Ok(());
        };
        let stale = self.stale.contains(&request);
        match event {
            ServerEvent::ApprovalRequested { approval_id, .. } => {
                // Abandoned turns get nothing more; report turns follow the
                // session's own approval policy.
                let approved = self.tools && !stale;
                self.send(&ClientMessage::ApprovalResolve {
                    request_id: Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    approval_id,
                    approved,
                })
                .await?;
            }
            ServerEvent::AssistantCompleted { content, .. } => {
                if let Some(answer) = self.server_turns.get_mut(&request) {
                    answer.text.clear();
                    append_capped(&mut answer.text, &content, MAX_REPORT_BYTES);
                }
            }
            ServerEvent::AssistantDelta { content, .. } => {
                if let Some(answer) = self.server_turns.get_mut(&request) {
                    append_capped(&mut answer.text, &content, MAX_REPORT_BYTES);
                }
            }
            ServerEvent::TurnCompleted { .. } => {
                self.stale.remove(&request);
                if let Some(answer) = self.server_turns.remove(&request)
                    && (!answer.text.trim().is_empty() || !answer.files.is_empty())
                {
                    self.reports.push_back(answer);
                }
            }
            ServerEvent::TurnFailed { .. } => {
                self.stale.remove(&request);
                if self.server_turns.remove(&request).is_some() {
                    self.reports.push_back(Reply::text(REPORT_FAILURE));
                }
            }
            ServerEvent::TurnCancelled { .. } => {
                self.stale.remove(&request);
                self.server_turns.remove(&request);
            }
            _ => {}
        }
        Ok(())
    }

    /// Abandon the current turn (its time ran out): ask the server to cancel
    /// it and ignore its late events. `false` when it had not started, so
    /// it cannot be cancelled and the session should be replaced.
    pub async fn cancel_current(&mut self) -> Result<bool> {
        let Some((request, started)) = self.current.take() else {
            return Ok(true);
        };
        self.stale.insert(request);
        let Some(turn_id) = started else {
            return Ok(false);
        };
        self.send(&ClientMessage::TurnCancel {
            request_id: Uuid::new_v4().to_string(),
            session_id: self.session_id.clone(),
            turn_id,
        })
        .await?;
        Ok(true)
    }

    /// Run one turn and return its answer, at most `max_bytes` (a longer
    /// answer is cut with a note), and the files the model attached.
    /// Background reports finishing meanwhile wait in `take_reports`.
    pub async fn turn(
        &mut self,
        prompt: &str,
        attachments: Vec<Attachment>,
        max_bytes: usize,
    ) -> Result<Reply> {
        self.last_used = Instant::now();
        let request_id = Uuid::new_v4().to_string();
        self.current = Some((request_id.clone(), None));
        self.files.clear();
        self.send(&ClientMessage::TurnStart {
            request_id,
            session_id: self.session_id.clone(),
            prompt: prompt.to_owned(),
            attachments,
        })
        .await?;
        let mut answer = String::new();
        loop {
            let event = self.event().await?;
            self.observe(&event);
            if self.is_other(&event) {
                self.absorb(event).await?;
                continue;
            }
            match event {
                ServerEvent::AssistantCompleted { content, .. } => {
                    answer.clear();
                    append_capped(&mut answer, &content, max_bytes);
                }
                ServerEvent::AssistantDelta { content, .. } => {
                    append_capped(&mut answer, &content, max_bytes)
                }
                ServerEvent::ApprovalRequested { approval_id, .. } => {
                    self.send(&ClientMessage::ApprovalResolve {
                        request_id: Uuid::new_v4().to_string(),
                        session_id: self.session_id.clone(),
                        approval_id,
                        approved: self.tools,
                    })
                    .await?;
                }
                ServerEvent::TurnCompleted { .. } => {
                    self.current = None;
                    self.tasks.clear();
                    // Idle expiry counts from the end of long turns too.
                    self.last_used = Instant::now();
                    return Ok(Reply {
                        text: answer,
                        files: std::mem::take(&mut self.files),
                    });
                }
                ServerEvent::TurnFailed { message, .. } => {
                    self.current = None;
                    bail!("{message}")
                }
                // Errors answering other requests (a late approval or cancel)
                // do not end this turn.
                ServerEvent::Error {
                    request_id,
                    message,
                    ..
                } if request_id.is_none()
                    || request_id.as_deref()
                        == self.current.as_ref().map(|(own, _)| own.as_str()) =>
                {
                    self.current = None;
                    bail!("{message}")
                }
                ServerEvent::TurnCancelled { .. } => {
                    self.current = None;
                    bail!("turn cancelled")
                }
                // Tool status lines are for local displays; channels get only
                // the final answer.
                ServerEvent::ToolProgress { .. } => {}
                _ => {}
            }
        }
    }
}

/// A background job this session started, as a restart describes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobInfo {
    /// The delegating tool, such as `agent_codex`.
    pub tool: String,
    /// The first line of the delegated prompt, shortened.
    pub task: String,
}

/// Agent calls of one turn whose task line is kept until they return.
const MAX_TRACKED_CALLS: usize = 64;
/// Characters of a delegated prompt's first line kept as its task.
const TASK_CHARS: usize = 80;

/// The first non-empty line of `prompt`, at most [`TASK_CHARS`] characters.
fn task_line(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut task: String = line
        .chars()
        .filter(|character| !character.is_control())
        .take(TASK_CHARS)
        .collect();
    if line.chars().count() > TASK_CHARS {
        task.push('…');
    }
    task
}

/// Answers from a background report turn, like any reply, are bounded.
const MAX_REPORT_BYTES: usize = 64 * 1024;
const REPORT_FAILURE: &str = "SCV could not report a finished background job.";
const TRUNCATED_NOTE: &str = "\n[reply truncated]";

/// Append `content` to `answer` up to `max_bytes`; the first cut adds a note.
fn append_capped(answer: &mut String, content: &str, max_bytes: usize) {
    if answer.ends_with(TRUNCATED_NOTE) {
        return;
    }
    let room = max_bytes.saturating_sub(answer.len());
    if content.len() <= room {
        answer.push_str(content);
        return;
    }
    let mut end = room.saturating_sub(TRUNCATED_NOTE.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    answer.push_str(&content[..end]);
    answer.push_str(TRUNCATED_NOTE);
}

async fn write(writer: &mut OwnedWriteHalf, message: &ClientMessage) -> Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(|error| anyhow!(error))?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
