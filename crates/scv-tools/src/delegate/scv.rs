//! `agent_scv`: delegate to a nested SCV over the SCV protocol.
//!
//! Each conversation runs one `scv server --stdio` in SCV's private adapter
//! home ([`LiveChild`]). The first turn performs the handshake (`initialize`,
//! then `session.start` at the next delegation depth); every turn is a
//! `turn.start` on that session. The nested SCV's events become progress, its
//! `approval.requested` goes through the calling session's approval gate, and
//! a cancelled or timed-out call sends `turn.cancel`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use scv_protocol::{ClientMessage, PROTOCOL_VERSION, ServerEvent};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::{
    DelegationContext,
    args::{Timeouts, bounded, parse_args, timeout_schema, validate_process_args},
    delegate::{
        conversation::{Attachment, ConversationStore, TurnGuard},
        live::{LiveChild, LiveLine, LiveSpec},
        output::{AgentResult, AgentUsage, RunStatus, truncate_utf8},
        records,
        request::{AgentArgs, resolve_agent_cwd, valid_model_name, validate_agent_cwd},
    },
};

const AGENT: &str = "scv";
/// Longest protocol line accepted from the nested SCV: its largest frame.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024 + 1024;
/// How long a cancelled or timed-out turn may take to settle before the
/// nested SCV is shut down.
const SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A conversation's nested SCV: the process and its protocol session.
#[derive(Debug)]
struct ScvChild {
    live: Arc<LiveChild>,
    session_id: String,
    depth: u32,
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
    fn validate(&self, args: &AgentArgs) -> Result<(), ToolError> {
        validate_process_args(&args.prompt)?;
        self.timeouts.resolve(args.timeout_seconds)?;
        if let Some(cwd) = &args.cwd {
            validate_agent_cwd(cwd)?;
        }
        if let Some(session) = &args.session
            && !crate::delegate::conversation::is_handle(session)
        {
            return Err(ToolError(format!(
                "session {:?} is not a conversation handle; pass the `session` value an \
                 earlier {} call returned, or omit it to start a new conversation",
                bounded(session, 80),
                self.name
            )));
        }
        if args.effort.is_some() {
            return Err(ToolError(format!(
                "{} does not support selecting an effort",
                self.name
            )));
        }
        if let Some(model) = &args.model {
            if args.session.is_some() {
                return Err(ToolError(
                    "model applies to a new conversation only; omit it when continuing".into(),
                ));
            }
            if !valid_model_name(model) {
                return Err(ToolError(format!("invalid model {model:?}")));
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
            ToolError(format!(
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
                        return Err(ToolError(format!(
                            "the nested SCV speaks protocol {protocol_version}, not {PROTOCOL_VERSION}; \
                             install the same SCV version"
                        )));
                    }
                    Ok(ServerEvent::Error { message, .. }) => {
                        return Err(ToolError(format!(
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
                        return Err(ToolError(format!(
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
                live,
                session_id,
                depth,
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
        let request_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = child
            .live
            .send(&ClientMessage::TurnStart {
                request_id: request_id.clone(),
                session_id: child.session_id.clone(),
                prompt,
                attachments: Vec::new(),
            })
            .await
        {
            return TurnEnd::Lost(error.0);
        }
        let label = format!("[{handle} depth {}]", child.depth);
        let mut turn_id: Option<String> = None;
        let mut reply = Reply::new(self.output_limit);
        let mut progress = LineProgress::default();
        loop {
            let event = match next_event(&child.live, deadline, &context.cancellation).await {
                Ok(event) => event,
                Err(Interrupt::Lost(reason)) => return TurnEnd::Lost(reason),
                Err(interrupt) => {
                    let settled = cancel_turn(child, turn_id.as_deref(), &request_id).await;
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
                            let settled = cancel_turn(child, turn_id.as_deref(), &request_id).await;
                            return TurnEnd::TimedOut { reply: reply.finish(), settled };
                        }
                        () = context.cancellation.cancelled() => {
                            let settled = cancel_turn(child, turn_id.as_deref(), &request_id).await;
                            return TurnEnd::Cancelled { settled };
                        }
                    };
                    let Ok(approved) = approved else {
                        let settled = cancel_turn(child, turn_id.as_deref(), &request_id).await;
                        return TurnEnd::Cancelled { settled };
                    };
                    if let Err(error) = child
                        .live
                        .send(&ClientMessage::ApprovalResolve {
                            request_id: uuid::Uuid::new_v4().to_string(),
                            session_id: child.session_id.clone(),
                            approval_id,
                            approved,
                        })
                        .await
                    {
                        return TurnEnd::Lost(error.0);
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
async fn cancel_turn(child: &ScvChild, turn_id: Option<&str>, request_id: &str) -> bool {
    let Some(turn_id) = turn_id else {
        return false;
    };
    if child
        .live
        .send(&ClientMessage::TurnCancel {
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: child.session_id.clone(),
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
        match next_event(&child.live, deadline, &never).await {
            Ok(event) if belongs_to(&event, request_id) => match event {
                ServerEvent::TurnCancelled { .. }
                | ServerEvent::TurnFailed { .. }
                | ServerEvent::TurnCompleted { .. } => return true,
                ServerEvent::ApprovalRequested { approval_id, .. } => {
                    // Deny anything the cancelled turn still asks for.
                    let _ = child
                        .live
                        .send(&ClientMessage::ApprovalResolve {
                            request_id: uuid::Uuid::new_v4().to_string(),
                            session_id: child.session_id.clone(),
                            approval_id,
                            approved: false,
                        })
                        .await;
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
        Interrupt::TimedOut => ToolError("the nested SCV did not start in time".into()),
        Interrupt::Cancelled => ToolError("nested SCV start cancelled".into()),
        Interrupt::Lost(reason) => {
            let tail = live.stderr_tail().await;
            let detail = if tail.is_empty() {
                reason
            } else {
                format!("{reason}: {}", bounded(&tail, 1000))
            };
            ToolError(format!(
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
impl Tool for ScvAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Runs in its own private home, working with its own context and tools \
                while you keep yours, and does not see this conversation, so give it a \
                self-contained brief. It can also run SCV's own tools in another project \
                directory. Its tool approvals come \
                back to this session, so the same policy and user decide them. Each result \
                carries a `session` handle: pass it back to continue the same nested session \
                with its context."
                .into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "prompt":{"type":"string"},
                    "cwd":{
                        "type":"string",
                        "description":"Directory inside the workspace for the nested session, such as a project directory (\"scv\"). \
                            It loads that directory's AGENTS.md and skills. Defaults to the workspace root."
                    },
                    "session":{
                        "type":"string",
                        "description":"The `session` handle an earlier agent_scv call returned, such as \"scv-1\". \
                            Pass it to continue that nested session; omit it to start a new one for unrelated work."
                    },
                    "model":{
                        "type":"string",
                        "description":"Model for a new nested session. Set when the user asks, or when \
                            the work matches a configured use_for default; omit to use the nested \
                            SCV's configured model."
                    },
                    "timeout_seconds":timeout_schema(self.timeouts)
                },
                "required":["prompt"],
                "additionalProperties":false
            }),
        }
    }

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
                return Err(ToolError("conversation has no nested SCV".into()));
            };
            if !child.live.is_running() {
                let handle = turn.handle.clone();
                turn.forget();
                return Err(ToolError(format!(
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
        child.live.set_turn(turn.turn);
        let handle = turn.handle.clone();
        let number = turn.turn;
        let end = self
            .run_turn(&child, &handle, args.prompt, &context, deadline)
            .await;
        let (status, (reply, cut), usage, error) = match end {
            TurnEnd::Completed { reply, usage } => (RunStatus::Completed, reply, Some(usage), None),
            TurnEnd::Failed { reply, error } => (RunStatus::Failed, reply, None, Some(error)),
            TurnEnd::TimedOut { reply, settled } => {
                if !settled {
                    child.live.close().await;
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
                    drop(turn.finish(Some(child.session_id.clone()), false));
                } else {
                    child.live.close().await;
                    turn.forget();
                }
                return Err(ToolError("nested SCV turn cancelled".into()));
            }
            TurnEnd::Lost(reason) => {
                let tail = child.live.stderr_tail().await;
                child.live.close().await;
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
        let conversation = turn
            .finish(
                Some(child.session_id.clone()),
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
        is_error: status != RunStatus::Completed,
        truncated,
    }
}

#[cfg(test)]
mod tests;
