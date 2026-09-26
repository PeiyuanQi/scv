//! The `scv` agent: delegate to a nested SCV over the SCV protocol.
//!
//! Each conversation runs one `scv server --stdio` in SCV's private adapter
//! home ([`LiveChild`]). The first turn performs the handshake (`initialize`,
//! then `session.start` at the next delegation depth); every turn is a
//! `turn.start` on that session. The nested SCV's events become progress, its
//! `approval.requested` goes through the calling session's approval gate, and
//! a cancelled or timed-out call sends `turn.cancel`.
//!
//! The nested session may run background jobs of its own, which outlive the
//! call that started them and are reported in turns the nested SCV starts
//! itself. Between calls a watcher keeps reading the child's events, so those
//! turns never stall on a full pipe; it denies their approval requests, since
//! no call is there to carry them to a person. From the events of both, the
//! child's delegation record counts the jobs that still run or wait to be
//! reported (`background_jobs`), and a planned restart waits for them.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use scv_core::{ToolContext, ToolError, ToolOutput, ToolRisk};
use scv_protocol::{ClientMessage, PROTOCOL_VERSION, ServerEvent};
use serde_json::Value;
use tokio::{
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{
    DelegationContext,
    args::{Timeouts, bounded, parse_args, validate_process_args},
    delegate::{
        agent::{Accepts, Backend},
        choice,
        conversation::{Attachment, ConversationStore, TurnGuard},
        live::{LiveChild, LiveLine, LiveSpec, LiveTurn},
        output::{AgentResult, AgentUsage, RunStatus, truncate_utf8},
        records,
        request::{AgentArgs, resolve_agent_cwd, valid_model_name, validate_agent_cwd},
    },
    sync::lock,
};

const AGENT: &str = "scv";
/// Longest protocol line accepted from the nested SCV: its largest frame.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024 + 1024;
/// How long a cancelled or timed-out turn may take to settle before the
/// nested SCV is shut down.
const SETTLE_GRACE: Duration = Duration::from_secs(2);
/// How long the watcher waits for an event before it waits again.
const WATCH_WAIT: Duration = Duration::from_secs(3600);
/// Most background jobs, and report turns, followed for one nested SCV, so a
/// misbehaving child cannot grow the table without bound.
const MAX_TRACKED: usize = 64;

/// A conversation's nested SCV, and the watcher that reads its events while
/// no call does.
#[derive(Debug)]
struct ScvChild {
    nested: Arc<Nested>,
    depth: u32,
    watcher: StdMutex<Option<Watcher>>,
}

/// The nested SCV process, its protocol session, and its background work.
#[derive(Debug)]
struct Nested {
    live: Arc<LiveChild>,
    session_id: String,
    work: StdMutex<BackgroundWork>,
}

/// The task reading a nested SCV's events between calls.
#[derive(Debug)]
struct Watcher {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

/// A nested SCV's own background jobs, followed through its events the way a
/// chat session follows its own: a job counts from the call that started it
/// (`tool.completed.jobs`) until its model has seen the result, through a
/// later call or a turn the nested SCV started to report it (`origin.jobs`),
/// which counts until it ends.
#[derive(Debug, Default)]
struct BackgroundWork {
    jobs: HashSet<String>,
    /// Report turns running, by request ID, with how many jobs each reports.
    reports: HashMap<String, usize>,
}

impl BackgroundWork {
    /// Follow `event`, returning the new count when it changed.
    fn observe(&mut self, event: &ServerEvent) -> Option<usize> {
        let before = self.count();
        match event {
            ServerEvent::ToolCompleted { jobs, .. } => {
                for change in jobs {
                    if !change.started() {
                        self.jobs.remove(&change.job);
                    } else if self.jobs.len() < MAX_TRACKED {
                        self.jobs.insert(change.job.clone());
                    }
                }
            }
            ServerEvent::TurnStarted {
                request_id,
                origin: Some(origin),
                ..
            } => {
                for job in &origin.jobs {
                    self.jobs.remove(job);
                }
                if self.reports.len() < MAX_TRACKED {
                    self.reports
                        .insert(request_id.clone(), origin.jobs.len().max(1));
                }
            }
            ServerEvent::TurnCompleted {
                request_id,
                origin: Some(_),
                ..
            }
            | ServerEvent::TurnCancelled {
                request_id,
                origin: Some(_),
                ..
            }
            | ServerEvent::TurnFailed {
                request_id,
                origin: Some(_),
                ..
            } => {
                self.reports.remove(request_id);
            }
            _ => {}
        }
        let after = self.count();
        (after != before).then_some(after)
    }

    fn count(&self) -> usize {
        self.jobs.len() + self.reports.values().sum::<usize>()
    }
}

impl Nested {
    /// The next event from the nested SCV. Every event updates its
    /// background work, and an approval request of a turn other than
    /// `current` (one the nested SCV started itself) is denied, since no call
    /// carries it to a person.
    async fn next_event(
        &self,
        current: Option<&str>,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<ServerEvent, Interrupt> {
        loop {
            let event = next_event(&self.live, deadline, cancellation).await?;
            if let Some(jobs) = lock(&self.work).observe(&event) {
                self.live.set_background_jobs(jobs);
            }
            match event {
                ServerEvent::ApprovalRequested {
                    request_id,
                    approval_id,
                    ..
                } if current != Some(request_id.as_str()) => {
                    self.resolve(approval_id, false)
                        .await
                        .map_err(|error| Interrupt::Lost(error.message))?;
                }
                event => return Ok(event),
            }
        }
    }

    async fn resolve(&self, approval_id: String, approved: bool) -> Result<(), ToolError> {
        self.live
            .send(&ClientMessage::ApprovalResolve {
                request_id: uuid::Uuid::new_v4().to_string(),
                session_id: self.session_id.clone(),
                approval_id,
                approved,
            })
            .await
    }

    /// Read the nested SCV's events while no call does, until `stop`. A
    /// child that exits or breaks the protocol is shut down, so the next
    /// call finds its conversation ended.
    async fn watch(self: Arc<Self>, stop: CancellationToken) {
        loop {
            match self
                .next_event(None, Instant::now() + WATCH_WAIT, &stop)
                .await
            {
                Ok(_) | Err(Interrupt::TimedOut) => {}
                Err(Interrupt::Cancelled) => return,
                Err(Interrupt::Lost(_)) => {
                    self.live.close().await;
                    return;
                }
            }
        }
    }
}

impl ScvChild {
    /// Take the nested SCV's events over for a call serving `turn`: stop the
    /// watcher, and record the child at work until the returned guard drops.
    async fn serve(&self, turn: u32) -> Serving<'_> {
        let watcher = lock(&self.watcher).take();
        if let Some(watcher) = watcher {
            watcher.stop.cancel();
            let _ = watcher.task.await;
        }
        Serving {
            child: self,
            _turn: self.nested.live.begin_turn(turn),
        }
    }
}

impl Drop for ScvChild {
    fn drop(&mut self) {
        // The watcher holds the process too: stopping it lets the process go
        // with its conversation.
        let watcher = self
            .watcher
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(watcher) = watcher {
            watcher.stop.cancel();
        }
    }
}

/// A call serving a turn on a nested SCV, from [`ScvChild::serve`]. However
/// the call ends, dropping this records the child idle and hands its events
/// back to the watcher while it still runs.
#[must_use = "the call's turn ends when this guard drops"]
struct Serving<'a> {
    child: &'a ScvChild,
    _turn: LiveTurn<'a>,
}

impl Drop for Serving<'_> {
    fn drop(&mut self) {
        let child = self.child;
        if !child.nested.live.is_running() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let stop = CancellationToken::new();
        let task = runtime.spawn(Arc::clone(&child.nested).watch(stop.clone()));
        *lock(&child.watcher) = Some(Watcher { stop, task });
    }
}

pub(crate) struct ScvAgentTool {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) resolved: Option<PathBuf>,
    pub(crate) args: Vec<String>,
    pub(crate) environment: Vec<(OsString, OsString)>,
    pub(crate) timeouts: Timeouts,
    pub(crate) output_limit: usize,
    pub(crate) delegation: Option<DelegationContext>,
    pub(crate) conversations: Arc<ConversationStore>,
}

/// Why waiting for the nested SCV stopped early.
enum Interrupt {
    TimedOut,
    Cancelled,
    /// The nested SCV ended or broke the protocol.
    Lost(String),
}

impl ScvAgentTool {
    /// What a nested SCV takes: `model`, for a new conversation, and
    /// `session`; its provider has no per-call effort.
    pub(crate) const ACCEPTS: Accepts = Accepts {
        model: true,
        effort: false,
        session: true,
    };

    fn validate(&self, args: &AgentArgs) -> Result<(), ToolError> {
        validate_process_args(&args.prompt)?;
        self.timeouts.resolve(args.timeout_seconds)?;
        if let Some(cwd) = &args.cwd {
            validate_agent_cwd(cwd)?;
        }
        if let Some(session) = &args.session
            && !crate::delegate::conversation::is_handle(session)
        {
            return Err(ToolError::invalid_arguments(format!(
                "session {:?} is not a conversation handle; pass the `session` value an \
                 earlier {} call returned, or omit it to start a new conversation",
                bounded(session, 80),
                self.name
            )));
        }
        if args.effort.is_some() {
            return Err(ToolError::invalid_arguments(format!(
                "{} does not support selecting an effort",
                self.name
            )));
        }
        if let Some(model) = &args.model {
            if args.session.is_some() {
                return Err(ToolError::invalid_arguments(
                    "model applies to a new conversation only; omit it when continuing",
                ));
            }
            if !valid_model_name(model) {
                return Err(ToolError::invalid_arguments(format!(
                    "invalid model {model:?}"
                )));
            }
        }
        Ok(())
    }

    fn owner_depth(&self) -> u32 {
        self.delegation
            .as_ref()
            .map_or_else(records::current_depth, DelegationContext::owner_depth)
    }

    /// Start a nested SCV for a new conversation and open its session.
    async fn start(
        &self,
        turn: &TurnGuard,
        cwd: &Path,
        model: Option<&str>,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<ScvChild, ToolError> {
        let executable = self.resolved.as_ref().ok_or_else(|| {
            ToolError::unavailable(format!(
                "{} executable {:?} was not found on PATH or in the user's install directories",
                self.name, self.command
            ))
        })?;
        let owner_depth = self.owner_depth();
        let depth = owner_depth.saturating_add(1);
        let pending = self.delegation.as_ref().map(|delegation| {
            delegation.registry.begin_at(
                owner_depth,
                AGENT,
                &delegation.session,
                cwd,
                Some((turn.handle.as_str(), turn.turn)),
            )
        });
        let mut environment = self.environment.clone();
        match &pending {
            Some(pending) => environment.extend(pending.environment.iter().cloned()),
            None => environment.push((records::DEPTH_VARIABLE.into(), depth.to_string().into())),
        }
        let registration = self
            .delegation
            .as_ref()
            .map(|delegation| Arc::clone(&delegation.registry))
            .zip(pending);
        let live = LiveChild::spawn(
            LiveSpec {
                executable: executable.as_os_str().to_owned(),
                args: self.args.iter().map(OsString::from).collect(),
                cwd: cwd.to_owned(),
                environment,
                max_line_bytes: MAX_FRAME_BYTES,
            },
            registration,
        )?;
        let handshake = async {
            live.send(&ClientMessage::initialize("scv-agent-init", "scv-agent"))
                .await?;
            loop {
                match next_event(&live, deadline, cancellation).await {
                    Ok(ServerEvent::Initialized {
                        protocol_version, ..
                    }) if protocol_version == PROTOCOL_VERSION => break,
                    Ok(ServerEvent::Initialized {
                        protocol_version, ..
                    }) => {
                        return Err(ToolError::failed(format!(
                            "the nested SCV speaks protocol {protocol_version}, not {PROTOCOL_VERSION}; \
                             install the same SCV version"
                        )));
                    }
                    Ok(ServerEvent::Error { message, .. }) => {
                        return Err(reported_error(format!(
                            "the nested SCV refused the handshake: {message}"
                        )));
                    }
                    Ok(_) => {}
                    Err(interrupt) => return Err(interrupt_error(&live, interrupt).await),
                }
            }
            live.send(&ClientMessage::SessionStart {
                request_id: "scv-agent-session".into(),
                cwd: cwd.display().to_string(),
                provider: None,
                model: model.map(str::to_owned),
                base_url: None,
                no_tools: None,
                delegation_depth: Some(depth),
                channel: None,
                auto_approve: None,
            })
            .await?;
            loop {
                match next_event(&live, deadline, cancellation).await {
                    Ok(ServerEvent::SessionStarted { session_id, .. }) => return Ok(session_id),
                    Ok(ServerEvent::Error { message, .. }) => {
                        return Err(reported_error(format!(
                            "the nested SCV could not start a session: {message}"
                        )));
                    }
                    Ok(_) => {}
                    Err(interrupt) => return Err(interrupt_error(&live, interrupt).await),
                }
            }
        };
        match handshake.await {
            Ok(session_id) => Ok(ScvChild {
                nested: Arc::new(Nested {
                    live,
                    session_id,
                    work: StdMutex::default(),
                }),
                depth,
                watcher: StdMutex::default(),
            }),
            Err(error) => {
                live.close().await;
                Err(error)
            }
        }
    }

    /// Run one turn on `child`, relaying approvals and reporting progress.
    async fn run_turn(
        &self,
        child: &ScvChild,
        handle: &str,
        prompt: String,
        context: &ToolContext,
        deadline: Instant,
    ) -> TurnEnd {
        let nested = &child.nested;
        let request_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = nested
            .live
            .send(&ClientMessage::TurnStart {
                request_id: request_id.clone(),
                session_id: nested.session_id.clone(),
                prompt,
                attachments: Vec::new(),
            })
            .await
        {
            return TurnEnd::Lost(error.message);
        }
        let label = format!("[{handle} depth {}]", child.depth);
        let mut turn_id: Option<String> = None;
        let mut reply = Reply::new(self.output_limit);
        let mut progress = LineProgress::default();
        loop {
            let event = match nested
                .next_event(Some(&request_id), deadline, &context.cancellation)
                .await
            {
                Ok(event) => event,
                Err(Interrupt::Lost(reason)) => return TurnEnd::Lost(reason),
                Err(interrupt) => {
                    let settled = cancel_turn(nested, turn_id.as_deref(), &request_id).await;
                    return match interrupt {
                        Interrupt::TimedOut => TurnEnd::TimedOut {
                            reply: reply.finish(),
                            settled,
                        },
                        _ => TurnEnd::Cancelled { settled },
                    };
                }
            };
            if !belongs_to(&event, &request_id) {
                continue;
            }
            match event {
                ServerEvent::TurnStarted { turn_id: id, .. }
                | ServerEvent::QueueDequeued { turn_id: id, .. } => turn_id = Some(id),
                ServerEvent::AssistantDelta { content, .. } => {
                    reply.delta(&content);
                    progress.push(&content, &context.progress);
                }
                ServerEvent::AssistantCompleted { content, .. } => {
                    reply.completed(content);
                    progress.flush(&context.progress);
                }
                ServerEvent::ToolStarted { name, .. } => {
                    progress.flush(&context.progress);
                    context.progress.report(&format!("{name} …"));
                }
                ServerEvent::ToolProgress { text, .. } => {
                    for line in text.lines() {
                        context.progress.report(line);
                    }
                }
                ServerEvent::ToolCompleted { name, success, .. } => {
                    context.progress.report(&format!(
                        "{name} {}",
                        if success { "done" } else { "failed" }
                    ));
                }
                ServerEvent::ApprovalRequested {
                    approval_id,
                    name,
                    risk,
                    cwd,
                    summary,
                    ..
                } => {
                    let request = context.approvals.request(
                        name,
                        ToolRisk::parse(&risk).unwrap_or(ToolRisk::Delegate),
                        PathBuf::from(cwd),
                        format!("{label} {summary}"),
                        context.cancellation.child_token(),
                    );
                    let approved = tokio::select! {
                        result = request => result,
                        () = tokio::time::sleep_until(deadline) => {
                            let settled = cancel_turn(nested, turn_id.as_deref(), &request_id).await;
                            return TurnEnd::TimedOut { reply: reply.finish(), settled };
                        }
                        () = context.cancellation.cancelled() => {
                            let settled = cancel_turn(nested, turn_id.as_deref(), &request_id).await;
                            return TurnEnd::Cancelled { settled };
                        }
                    };
                    let Ok(approved) = approved else {
                        let settled = cancel_turn(nested, turn_id.as_deref(), &request_id).await;
                        return TurnEnd::Cancelled { settled };
                    };
                    if let Err(error) = nested.resolve(approval_id, approved).await {
                        return TurnEnd::Lost(error.message);
                    }
                }
                ServerEvent::TurnCompleted { usage, .. } => {
                    progress.flush(&context.progress);
                    return TurnEnd::Completed {
                        reply: reply.finish(),
                        usage: AgentUsage {
                            input_tokens: usage.input_tokens.unwrap_or(0),
                            output_tokens: usage.output_tokens.unwrap_or(0),
                        },
                    };
                }
                ServerEvent::TurnFailed { code, message, .. } => {
                    return TurnEnd::Failed {
                        reply: reply.finish(),
                        error: format!("the nested SCV turn failed ({code}): {message}"),
                    };
                }
                ServerEvent::TurnCancelled { .. } => return TurnEnd::Cancelled { settled: true },
                ServerEvent::Error { message, fatal, .. } => {
                    let error = format!("the nested SCV reported an error: {message}");
                    return if fatal {
                        TurnEnd::Lost(error)
                    } else {
                        TurnEnd::Failed {
                            reply: reply.finish(),
                            error,
                        }
                    };
                }
                _ => {}
            }
        }
    }
}

/// How a turn on the nested SCV ended.
enum TurnEnd {
    Completed {
        reply: (String, bool),
        usage: AgentUsage,
    },
    Failed {
        reply: (String, bool),
        error: String,
    },
    TimedOut {
        reply: (String, bool),
        /// Whether the nested SCV acknowledged the cancellation in time.
        settled: bool,
    },
    Cancelled {
        settled: bool,
    },
    /// The nested SCV exited or broke the protocol.
    Lost(String),
}

/// Whether `event` belongs to the turn started with `request_id`. Session
/// events without a request (such as a fatal error) always do.
fn belongs_to(event: &ServerEvent, request_id: &str) -> bool {
    let value = serde_json::to_value(event).unwrap_or(Value::Null);
    match value.get("request_id") {
        Some(Value::String(id)) => id == request_id,
        _ => true,
    }
}

/// Send `turn.cancel` and wait up to [`SETTLE_GRACE`] for the turn to end.
async fn cancel_turn(nested: &Nested, turn_id: Option<&str>, request_id: &str) -> bool {
    let Some(turn_id) = turn_id else {
        return false;
    };
    if nested
        .live
        .send(&ClientMessage::TurnCancel {
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: nested.session_id.clone(),
            turn_id: turn_id.to_owned(),
        })
        .await
        .is_err()
    {
        return false;
    }
    let deadline = Instant::now() + SETTLE_GRACE;
    let never = CancellationToken::new();
    loop {
        match nested.next_event(Some(request_id), deadline, &never).await {
            Ok(event) if belongs_to(&event, request_id) => match event {
                ServerEvent::TurnCancelled { .. }
                | ServerEvent::TurnFailed { .. }
                | ServerEvent::TurnCompleted { .. } => return true,
                ServerEvent::ApprovalRequested { approval_id, .. } => {
                    // Deny anything the cancelled turn still asks for.
                    let _ = nested.resolve(approval_id, false).await;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// The next protocol event from the nested SCV.
async fn next_event(
    live: &LiveChild,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ServerEvent, Interrupt> {
    let line = tokio::select! {
        line = timeout_at(deadline, live.recv()) => match line {
            Ok(line) => line,
            Err(_) => return Err(Interrupt::TimedOut),
        },
        () = cancellation.cancelled() => return Err(Interrupt::Cancelled),
    };
    match line {
        Some(LiveLine::Line(bytes)) => serde_json::from_slice(&bytes).map_err(|error| {
            Interrupt::Lost(format!("the nested SCV sent an invalid event: {error}"))
        }),
        Some(LiveLine::TooLong) => Err(Interrupt::Lost(
            "the nested SCV sent an event larger than its frame limit".into(),
        )),
        None => Err(Interrupt::Lost("the nested SCV exited".into())),
    }
}

async fn interrupt_error(live: &LiveChild, interrupt: Interrupt) -> ToolError {
    match interrupt {
        Interrupt::TimedOut => ToolError::limit("the nested SCV did not start in time"),
        Interrupt::Cancelled => ToolError::cancelled("nested SCV start cancelled"),
        Interrupt::Lost(reason) => {
            let tail = live.stderr_tail().await;
            let detail = if tail.is_empty() {
                reason
            } else {
                format!("{reason}: {}", bounded(&tail, 1000))
            };
            reported_error(format!(
                "{detail}. If the nested SCV has no provider configured, the host owner can \
                 copy SCV's own with: scv agents import scv"
            ))
        }
    }
}

/// A delegated agent's reply: the last completed assistant message, or the
/// streamed text of one still in progress, bounded to the output limit.
pub(crate) struct Reply {
    completed: Option<String>,
    streaming: String,
    limit: usize,
}

impl Reply {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            completed: None,
            streaming: String::new(),
            limit,
        }
    }

    pub(crate) fn delta(&mut self, content: &str) {
        if self.streaming.len() <= self.limit {
            self.streaming.push_str(content);
        }
    }

    pub(crate) fn completed(&mut self, content: String) {
        self.streaming.clear();
        if !content.trim().is_empty() {
            self.completed = Some(content);
        }
    }

    /// The reply and whether it was cut.
    pub(crate) fn finish(self) -> (String, bool) {
        let text = match self.completed {
            Some(text) if self.streaming.is_empty() => text,
            Some(text) => format!("{text}\n{}", self.streaming),
            None => self.streaming,
        };
        let (text, cut) = truncate_utf8(text.trim(), self.limit);
        (text.to_owned(), cut)
    }
}

/// Streamed assistant text reported as whole lines.
#[derive(Default)]
pub(crate) struct LineProgress {
    partial: String,
}

impl LineProgress {
    pub(crate) fn push(&mut self, content: &str, sink: &scv_core::ProgressSink) {
        self.partial.push_str(content);
        while let Some(index) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=index).collect();
            if !line.trim().is_empty() {
                sink.report(&line);
            }
        }
        // A runaway line without breaks is reported in pieces.
        if self.partial.len() > scv_core::MAX_PROGRESS_LINE_BYTES * 4 {
            self.flush(sink);
        }
    }

    pub(crate) fn flush(&mut self, sink: &scv_core::ProgressSink) {
        if !self.partial.trim().is_empty() {
            sink.report(&self.partial);
        }
        self.partial.clear();
    }
}

#[async_trait]
impl Backend for ScvAgentTool {
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        self.validate(&args)?;
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        self.validate(&args)?;
        let executable = self
            .resolved
            .as_ref()
            .map_or_else(|| self.command.clone(), |path| path.display().to_string());
        let directory = args.cwd.as_deref().map_or_else(
            || "the workspace root".to_owned(),
            |cwd| format!("{:?} (inside the workspace)", bounded(cwd, 200)),
        );
        let session = args.session.as_deref().map_or_else(
            || format!("a new nested SCV ({executable} {})", self.args.join(" ")),
            |session| format!("nested SCV conversation {session}"),
        );
        let timeout = self.timeouts.resolve(args.timeout_seconds)?;
        Ok(format!(
            "Send prompt {:?} to {session} in {directory} for up to {} seconds. It runs as your \
             user in SCV's private home; its own tool calls come back here for approval.",
            bounded(&args.prompt, 2000),
            timeout.as_secs()
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: AgentArgs = parse_args(&arguments)?;
        self.validate(&args)?;
        let cwd = resolve_agent_cwd(&context.workspace, args.cwd.as_deref())?;
        let limit = self.timeouts.resolve(args.timeout_seconds)?;
        let deadline = Instant::now() + limit;
        let mut turn = self
            .conversations
            .begin(AGENT, args.session.as_deref(), &cwd, false)?;
        let child: Arc<ScvChild> = if let Some(Attachment(attachment)) = turn.attachment() {
            let Ok(child) = Arc::clone(attachment).downcast::<ScvChild>() else {
                turn.forget();
                return Err(ToolError::failed("conversation has no nested SCV"));
            };
            if !child.nested.live.is_running() {
                let handle = turn.handle.clone();
                turn.forget();
                return Err(ToolError::unavailable(format!(
                    "conversation {handle} ended: its nested SCV exited; omit session to \
                     start a new one"
                )));
            }
            child
        } else {
            let child = self
                .start(
                    &turn,
                    &cwd,
                    args.model.as_deref(),
                    deadline,
                    &context.cancellation,
                )
                .await?;
            let child = Arc::new(child);
            turn.attach(Attachment(
                Arc::clone(&child) as Arc<dyn std::any::Any + Send + Sync>
            ));
            child
        };
        // Recorded at work until the call is done with the child's events,
        // however it ends. A child the call keeps is recorded idle and
        // watched again before its conversation is free for the next call.
        let serving = child.serve(turn.turn).await;
        let handle = turn.handle.clone();
        let number = turn.turn;
        let end = self
            .run_turn(&child, &handle, args.prompt, &context, deadline)
            .await;
        let live = &child.nested.live;
        let (status, (reply, cut), usage, error) = match end {
            TurnEnd::Completed { reply, usage } => (RunStatus::Completed, reply, Some(usage), None),
            TurnEnd::Failed { reply, error } => (RunStatus::Failed, reply, None, Some(error)),
            TurnEnd::TimedOut { reply, settled } => {
                if !settled {
                    live.close().await;
                    turn.forget();
                    return Ok(result(
                        RunStatus::Timeout,
                        reply,
                        None,
                        Some(format!(
                            "timed out after {} seconds; the nested SCV did not stop in time and \
                             was shut down",
                            limit.as_secs()
                        )),
                        None,
                        self.output_limit,
                    ));
                }
                (
                    RunStatus::Timeout,
                    reply,
                    None,
                    Some(format!("timed out after {} seconds", limit.as_secs())),
                )
            }
            TurnEnd::Cancelled { settled } => {
                if settled {
                    drop(serving);
                    drop(turn.finish(Some(child.nested.session_id.clone()), false));
                } else {
                    live.close().await;
                    turn.forget();
                }
                return Err(ToolError::cancelled("nested SCV turn cancelled"));
            }
            TurnEnd::Lost(reason) => {
                let tail = live.stderr_tail().await;
                live.close().await;
                turn.forget();
                let mut output = result(
                    RunStatus::Failed,
                    (String::new(), false),
                    None,
                    Some(reason),
                    None,
                    self.output_limit,
                );
                if !tail.is_empty()
                    && let Ok(Value::Object(mut value)) = serde_json::from_str(&output.content)
                {
                    value.insert("stderr_tail".into(), bounded(&tail, 2000).into());
                    output.content = Value::Object(value).to_string();
                }
                return Ok(output);
            }
        };
        drop(serving);
        let conversation = turn
            .finish(
                Some(child.nested.session_id.clone()),
                status == RunStatus::Completed,
            )
            .map(|handle| (handle, number));
        let output = result(
            status,
            (reply, cut),
            usage,
            error,
            conversation
                .as_ref()
                .map(|(handle, turn)| (handle.as_str(), *turn)),
            self.output_limit,
        );
        Ok(output)
    }
}

fn result(
    status: RunStatus,
    (reply, cut): (String, bool),
    usage: Option<AgentUsage>,
    error: Option<String>,
    conversation: Option<(&str, u32)>,
    limit: usize,
) -> ToolOutput {
    let reply = match &error {
        Some(error) if reply.is_empty() => error.clone(),
        Some(error) => format!("{reply}\n{error}"),
        None => reply,
    };
    let result = AgentResult {
        status,
        reply,
        error,
        usage,
        truncated: cut,
        session: None,
    };
    let (content, truncated) = result.to_json(AGENT, conversation, None, "", limit);
    ToolOutput {
        content,
        failure: status.failure(),
        truncated,
    }
}

/// An error the nested SCV reported before its session started: unavailable
/// when it reads like a missing or signed-out provider, as an agent's own
/// reported error would.
fn reported_error(message: String) -> ToolError {
    if choice::reports_unavailable(&message) {
        ToolError::unavailable(message)
    } else {
        ToolError::failed(message)
    }
}

#[cfg(test)]
mod tests;
