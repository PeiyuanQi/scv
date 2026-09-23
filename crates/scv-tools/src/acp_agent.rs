//! Delegation over the Agent Client Protocol (ACP): JSON-RPC 2.0 over stdio.
//!
//! Each conversation runs one ACP server ([`LiveChild`]): the agent itself
//! (`grok agent stdio`, `dsh --profile acp`) or its official adapter
//! (`claude-agent-acp`, `codex-acp`). The first turn performs `initialize`
//! and `session/new`; every turn is a `session/prompt` on that session.
//! `session/update` notifications become progress, the agent's
//! `session/request_permission` goes through the calling session's approval
//! gate, and a cancelled or timed-out call sends `session/cancel`. SCV offers
//! no client file-system or terminal capabilities, so every other request the
//! agent makes is refused as an unknown method.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::{
    AcpAgentLaunch, AgentAdapterConfig, AgentArgs, DelegationContext, Timeouts, add_sign_in_hint,
    agent_output::{AgentResult, AgentUsage, RunStatus},
    agent_progress::redact,
    bounded,
    conversation::{Attachment, ConversationStore, TurnGuard},
    delegation, is_secret_like,
    live::{LiveChild, LiveLine, LiveSpec},
    parse_args, resolve_agent_cwd,
    scv_agent::{LineProgress, Reply},
    timeout_schema, valid_model_name, validate_agent_cwd, validate_process_args,
};

/// The ACP major version SCV speaks.
const PROTOCOL_VERSION: u64 = 1;
/// Longest JSON-RPC line accepted from an agent.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// How long a cancelled or timed-out prompt may take to settle before the
/// agent is shut down.
const SETTLE_GRACE: Duration = Duration::from_secs(2);
/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// Config option IDs agents use for reasoning effort.
const EFFORT_OPTIONS: [&str; 3] = ["effort", "reasoning_effort", "thought_level"];

/// A JSON-RPC connection to an ACP server.
#[derive(Debug)]
struct Rpc {
    live: Arc<LiveChild>,
    next_id: AtomicU64,
}

/// One message from the agent.
enum Incoming {
    Response {
        id: Value,
        outcome: Result<Value, Value>,
    },
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// Why waiting for the agent stopped early.
enum Interrupt {
    TimedOut,
    Cancelled,
    /// The agent exited or broke the protocol.
    Lost(String),
}

impl Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<u64, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.live
            .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ToolError> {
        self.live
            .send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn respond(&self, id: Value, result: Value) -> Result<(), ToolError> {
        self.live
            .send(&json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }

    async fn refuse(&self, id: Value, method: &str) -> Result<(), ToolError> {
        self.live
            .send(&json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{"code":METHOD_NOT_FOUND,"message":format!("SCV does not provide {method}")}
            }))
            .await
    }

    /// The next message, waiting until `deadline` or cancellation.
    async fn next(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Incoming, Interrupt> {
        let line = tokio::select! {
            line = timeout_at(deadline, self.live.recv()) => match line {
                Ok(line) => line,
                Err(_) => return Err(Interrupt::TimedOut),
            },
            () = cancellation.cancelled() => return Err(Interrupt::Cancelled),
        };
        let bytes = match line {
            Some(LiveLine::Line(bytes)) => bytes,
            Some(LiveLine::TooLong) => {
                return Err(Interrupt::Lost(
                    "the agent sent a JSON-RPC message larger than SCV accepts".into(),
                ));
            }
            None => return Err(Interrupt::Lost("the agent exited".into())),
        };
        let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(&bytes) else {
            return Err(Interrupt::Lost(
                "the agent sent a line that is not a JSON-RPC message".into(),
            ));
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let params = message.remove("params").unwrap_or(Value::Null);
        Ok(match (message.remove("id"), method) {
            (Some(id), Some(method)) => Incoming::Request { id, method, params },
            (None, Some(method)) => Incoming::Notification { method, params },
            (Some(id), None) => Incoming::Response {
                id,
                outcome: match message.remove("error") {
                    Some(error) => Err(error),
                    None => Ok(message.remove("result").unwrap_or(Value::Null)),
                },
            },
            (None, None) => {
                return Err(Interrupt::Lost(
                    "the agent sent a JSON-RPC message with neither id nor method".into(),
                ));
            }
        })
    }

    /// Call `method` outside a prompt turn and wait for its result. The agent
    /// may not ask for anything meanwhile: permission requests are cancelled
    /// and other requests refused.
    async fn call(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Value, CallError> {
        let id = self.request(method, params).await.map_err(CallError::Io)?;
        loop {
            match self.next(deadline, cancellation).await {
                Ok(Incoming::Response { id: reply, outcome }) if reply == json!(id) => {
                    return outcome.map_err(CallError::Rpc);
                }
                Ok(Incoming::Request { id, method, .. }) => {
                    let sent = if method == "session/request_permission" {
                        self.respond(id, json!({"outcome":{"outcome":"cancelled"}}))
                            .await
                    } else {
                        self.refuse(id, &method).await
                    };
                    sent.map_err(CallError::Io)?;
                }
                Ok(_) => {}
                Err(interrupt) => return Err(CallError::Interrupted(interrupt)),
            }
        }
    }
}

enum CallError {
    Io(ToolError),
    Rpc(Value),
    Interrupted(Interrupt),
}

/// A conversation's ACP server and its session.
#[derive(Debug)]
struct AcpChild {
    rpc: Rpc,
    session_id: String,
    /// The session's config options (`model`, `effort`, ...) and their
    /// allowed values; an empty list accepts any value.
    options: StdMutex<HashMap<String, Vec<String>>>,
}

impl AcpChild {
    fn remember_options(&self, result: &Value) {
        if let Some(options) = parse_config_options(result) {
            *self
                .options
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = options;
        }
    }
}

fn parse_config_options(result: &Value) -> Option<HashMap<String, Vec<String>>> {
    let options = result.get("configOptions")?.as_array()?;
    Some(
        options
            .iter()
            .filter_map(|option| {
                let id = option.get("id")?.as_str()?.to_owned();
                let values = option
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.get("value")?.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                Some((id, values))
            })
            .collect(),
    )
}

/// How a prompt turn ended.
enum TurnEnd {
    Ended {
        stop_reason: String,
        reply: (String, bool),
        usage: Option<AgentUsage>,
    },
    Failed {
        reply: (String, bool),
        error: String,
    },
    TimedOut {
        reply: (String, bool),
        /// Whether the agent answered `session/cancel` in time.
        settled: bool,
    },
    Cancelled {
        settled: bool,
    },
    /// The agent exited or broke the protocol.
    Lost(String),
}

pub(crate) struct AcpAgentTool {
    name: String,
    /// The adapter name, such as `claude`: conversation handles and results.
    agent: String,
    launch: AcpAgentLaunch,
    resolved: Option<PathBuf>,
    environment: Vec<(OsString, OsString)>,
    /// `permissions = "full"`.
    full: bool,
    model_hint: String,
    timeouts: Timeouts,
    output_limit: usize,
    delegation: Option<DelegationContext>,
    conversations: Arc<ConversationStore>,
}

impl AcpAgentTool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        name: String,
        adapter: &AgentAdapterConfig,
        launch: AcpAgentLaunch,
        resolved: Option<PathBuf>,
        timeouts: Timeouts,
        output_limit: usize,
        delegation: Option<DelegationContext>,
        conversations: Arc<ConversationStore>,
    ) -> Self {
        Self {
            agent: name.trim_start_matches("agent_").to_owned(),
            name,
            launch,
            resolved,
            environment: adapter.environment.clone(),
            full: adapter.full_permission_args.is_some(),
            model_hint: adapter.model_hint.clone(),
            timeouts,
            output_limit,
            delegation,
            conversations,
        }
    }

    fn validate(&self, args: &AgentArgs) -> Result<(), ToolError> {
        validate_process_args(&args.prompt)?;
        self.timeouts.resolve(args.timeout_seconds)?;
        if let Some(cwd) = &args.cwd {
            validate_agent_cwd(cwd)?;
        }
        if let Some(session) = &args.session
            && !crate::conversation::is_handle(session)
        {
            return Err(ToolError(format!(
                "session {:?} is not a conversation handle; pass the `session` value an \
                 earlier {} call returned, or omit it to start a new conversation",
                bounded(session, 80),
                self.name
            )));
        }
        if let Some(model) = &args.model
            && !valid_model_name(model)
        {
            return Err(ToolError(format!("invalid model {model:?}")));
        }
        if let Some(effort) = &args.effort
            && (effort.is_empty()
                || effort.len() > 32
                || !effort
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            return Err(ToolError(format!("invalid effort {effort:?}")));
        }
        Ok(())
    }

    fn owner_depth(&self) -> u32 {
        self.delegation
            .as_ref()
            .map_or_else(delegation::current_depth, DelegationContext::owner_depth)
    }

    /// Start the ACP server for a new conversation and open its session.
    async fn start(
        &self,
        turn: &TurnGuard,
        cwd: &Path,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<AcpChild, String> {
        let executable = self.resolved.as_ref().ok_or_else(|| {
            format!(
                "{} ACP server {:?} was not found on PATH or in the user's install directories",
                self.name, self.launch.command
            )
        })?;
        let owner_depth = self.owner_depth();
        let pending = self.delegation.as_ref().map(|delegation| {
            delegation.registry.begin_at(
                owner_depth,
                &self.agent,
                &delegation.session,
                cwd,
                Some((turn.handle.as_str(), turn.turn)),
            )
        });
        let mut environment = self.environment.clone();
        match &pending {
            Some(pending) => environment.extend(pending.environment.iter().cloned()),
            None => environment.push((
                delegation::DEPTH_VARIABLE.into(),
                owner_depth.saturating_add(1).to_string().into(),
            )),
        }
        let registration = self
            .delegation
            .as_ref()
            .map(|delegation| Arc::clone(&delegation.registry))
            .zip(pending);
        let live = LiveChild::spawn(
            LiveSpec {
                executable: executable.as_os_str().to_owned(),
                args: self.launch.args.iter().map(OsString::from).collect(),
                cwd: cwd.to_owned(),
                environment,
                max_line_bytes: MAX_LINE_BYTES,
            },
            registration,
        )
        .map_err(|error| error.0)?;
        let rpc = Rpc {
            live,
            next_id: AtomicU64::new(1),
        };
        match self.handshake(rpc, cwd, deadline, cancellation).await {
            Ok(child) => Ok(child),
            Err((rpc, error)) => {
                rpc.live.close().await;
                Err(error)
            }
        }
    }

    async fn handshake(
        &self,
        rpc: Rpc,
        cwd: &Path,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<AcpChild, (Rpc, String)> {
        let initialize = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": "scv", "version": env!("CARGO_PKG_VERSION")}
        });
        let initialized = match rpc
            .call("initialize", initialize, deadline, cancellation)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let message = self.call_error(&rpc, "initialize", error).await;
                return Err((rpc, message));
            }
        };
        let version = initialized.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(PROTOCOL_VERSION) {
            let message = format!(
                "the {} ACP server speaks protocol version {}, not {PROTOCOL_VERSION}",
                self.agent,
                version.map_or_else(|| "unknown".into(), |version| version.to_string())
            );
            return Err((rpc, message));
        }
        let session = match rpc
            .call(
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
                deadline,
                cancellation,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let message = self.call_error(&rpc, "session/new", error).await;
                return Err((rpc, message));
            }
        };
        let Some(session_id) = session
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Err((rpc, "the agent's session/new returned no sessionId".into()));
        };
        let child = AcpChild {
            rpc,
            session_id,
            options: StdMutex::new(parse_config_options(&session).unwrap_or_default()),
        };
        if let Some(mode) = &self.launch.full_mode
            && let Err(error) = self
                .select_mode(&child, &session, mode, deadline, cancellation)
                .await
        {
            return Err((child.rpc, error));
        }
        Ok(child)
    }

    /// Select the session mode that grants full permissions: through
    /// `session/set_mode` when the session lists it among its modes,
    /// otherwise through a `mode` config option.
    async fn select_mode(
        &self,
        child: &AcpChild,
        session: &Value,
        mode: &str,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let listed = session
            .pointer("/modes/availableModes")
            .and_then(Value::as_array)
            .is_some_and(|modes| {
                modes
                    .iter()
                    .any(|entry| entry.get("id").and_then(Value::as_str) == Some(mode))
            });
        let result = if listed {
            child
                .rpc
                .call(
                    "session/set_mode",
                    json!({"sessionId": child.session_id, "modeId": mode}),
                    deadline,
                    cancellation,
                )
                .await
        } else if child
            .options
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get("mode")
            .is_some_and(|values| values.iter().any(|value| value == mode))
        {
            child
                .rpc
                .call(
                    "session/set_config_option",
                    json!({"sessionId": child.session_id, "configId": "mode", "value": mode}),
                    deadline,
                    cancellation,
                )
                .await
        } else {
            return Err(format!(
                "permissions = \"full\" needs the {} ACP session mode {mode:?}, which this \
                 agent does not offer",
                self.agent
            ));
        };
        match result {
            Ok(result) => {
                child.remember_options(&result);
                Ok(())
            }
            Err(error) => Err(self.call_error(&child.rpc, "set mode", error).await),
        }
    }

    /// Apply the call's model and effort through `session/set_config_option`.
    async fn configure(
        &self,
        child: &AcpChild,
        args: &AgentArgs,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        for (label, value, ids) in [
            ("model", args.model.as_deref(), &["model"][..]),
            ("effort", args.effort.as_deref(), &EFFORT_OPTIONS[..]),
        ] {
            let Some(value) = value else {
                continue;
            };
            let chosen = {
                let options = child
                    .options
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                ids.iter().find_map(|id| {
                    options
                        .get(*id)
                        .map(|values| ((*id).to_owned(), values.clone()))
                })
            };
            let Some((id, values)) = chosen else {
                return Err(format!(
                    "{} does not offer a {label} choice over ACP; omit {label}",
                    self.name
                ));
            };
            if !values.is_empty() && !values.iter().any(|allowed| allowed == value) {
                return Err(format!(
                    "{label} {value:?} is not offered by {}; choose one of: {}",
                    self.name,
                    values.join(", ")
                ));
            }
            match child
                .rpc
                .call(
                    "session/set_config_option",
                    json!({"sessionId": child.session_id, "configId": id, "value": value}),
                    deadline,
                    cancellation,
                )
                .await
            {
                Ok(result) => child.remember_options(&result),
                Err(error) => {
                    return Err(self
                        .call_error(&child.rpc, &format!("set {label}"), error)
                        .await);
                }
            }
        }
        Ok(())
    }

    async fn call_error(&self, rpc: &Rpc, what: &str, error: CallError) -> String {
        match error {
            CallError::Io(error) => format!("{what}: {}", error.0),
            CallError::Rpc(error) => format!("{what} failed: {}", describe_rpc_error(&error)),
            CallError::Interrupted(Interrupt::TimedOut) => format!("{what}: timed out"),
            CallError::Interrupted(Interrupt::Cancelled) => format!("{what}: cancelled"),
            CallError::Interrupted(Interrupt::Lost(reason)) => {
                let tail = rpc.live.stderr_tail().await;
                if tail.is_empty() {
                    format!("{what}: {reason}")
                } else {
                    format!("{what}: {reason}: {}", bounded(&redact(&tail), 1000))
                }
            }
        }
    }

    /// Run one prompt turn on `child`, relaying permission requests and
    /// reporting progress.
    async fn run_turn(
        &self,
        child: &AcpChild,
        handle: &str,
        cwd: &Path,
        prompt: String,
        context: &ToolContext,
        deadline: Instant,
    ) -> TurnEnd {
        let prompt_id = match child
            .rpc
            .request(
                "session/prompt",
                json!({
                    "sessionId": child.session_id,
                    "prompt": [{"type": "text", "text": prompt}]
                }),
            )
            .await
        {
            Ok(id) => json!(id),
            Err(error) => return TurnEnd::Lost(error.0),
        };
        let label = format!("[{handle} acp]");
        let mut reply = Reply::new(self.output_limit);
        let mut progress = Progress::default();
        loop {
            let message = match child.rpc.next(deadline, &context.cancellation).await {
                Ok(message) => message,
                Err(Interrupt::Lost(reason)) => return TurnEnd::Lost(reason),
                Err(interrupt) => {
                    let settled = cancel_prompt(child, &prompt_id).await;
                    return match interrupt {
                        Interrupt::TimedOut => TurnEnd::TimedOut {
                            reply: reply.finish(),
                            settled,
                        },
                        _ => TurnEnd::Cancelled { settled },
                    };
                }
            };
            match message {
                Incoming::Response { id, outcome } if id == prompt_id => {
                    progress.lines.flush(&context.progress);
                    return match outcome {
                        Ok(result) => TurnEnd::Ended {
                            stop_reason: result
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn")
                                .to_owned(),
                            reply: reply.finish(),
                            usage: parse_usage(&result),
                        },
                        Err(error) => TurnEnd::Failed {
                            reply: reply.finish(),
                            error: describe_rpc_error(&error),
                        },
                    };
                }
                Incoming::Response { .. } => {}
                Incoming::Notification { method, params } => {
                    if method == "session/update" {
                        progress.update(&params, &mut reply, &context.progress);
                    }
                }
                Incoming::Request { id, method, params } => {
                    if method != "session/request_permission" {
                        if let Err(error) = child.rpc.refuse(id, &method).await {
                            return TurnEnd::Lost(error.0);
                        }
                        continue;
                    }
                    progress.lines.flush(&context.progress);
                    let (risk, summary) = describe_permission(&params, &label);
                    let request = context.approvals.request(
                        self.name.clone(),
                        risk,
                        cwd.to_owned(),
                        summary,
                        context.cancellation.child_token(),
                    );
                    let approved = tokio::select! {
                        result = request => result.ok(),
                        () = tokio::time::sleep_until(deadline) => {
                            let _ = child.rpc.respond(id, json!({"outcome":{"outcome":"cancelled"}})).await;
                            let settled = cancel_prompt(child, &prompt_id).await;
                            return TurnEnd::TimedOut { reply: reply.finish(), settled };
                        }
                        () = context.cancellation.cancelled() => None,
                    };
                    let Some(approved) = approved else {
                        let _ = child
                            .rpc
                            .respond(id, json!({"outcome":{"outcome":"cancelled"}}))
                            .await;
                        let settled = cancel_prompt(child, &prompt_id).await;
                        return TurnEnd::Cancelled { settled };
                    };
                    let outcome = choose_option(params.get("options"), approved);
                    if let Err(error) = child.rpc.respond(id, json!({"outcome": outcome})).await {
                        return TurnEnd::Lost(error.0);
                    }
                }
            }
        }
    }

    fn result(
        &self,
        status: RunStatus,
        (reply, cut): (String, bool),
        usage: Option<AgentUsage>,
        error: Option<String>,
        conversation: Option<(&str, u32)>,
    ) -> ToolOutput {
        let reply = match error {
            Some(error) if reply.is_empty() => error,
            Some(error) => format!("{reply}\n{error}"),
            None => reply,
        };
        let result = AgentResult {
            status,
            reply,
            usage,
            truncated: cut,
            session: None,
        };
        let (content, truncated) =
            result.to_json(&self.agent, conversation, None, "", self.output_limit);
        let mut output = ToolOutput {
            content,
            is_error: status != RunStatus::Completed,
            truncated,
        };
        if output.is_error {
            add_sign_in_hint(&mut output, &self.agent);
        }
        output
    }
}

/// Send `session/cancel` and wait up to [`SETTLE_GRACE`] for the prompt to end.
async fn cancel_prompt(child: &AcpChild, prompt_id: &Value) -> bool {
    if child
        .rpc
        .notify("session/cancel", json!({"sessionId": child.session_id}))
        .await
        .is_err()
    {
        return false;
    }
    let deadline = Instant::now() + SETTLE_GRACE;
    let never = CancellationToken::new();
    loop {
        match child.rpc.next(deadline, &never).await {
            Ok(Incoming::Response { id, .. }) if &id == prompt_id => return true,
            Ok(Incoming::Request { id, method, .. }) => {
                // Refuse anything the cancelled turn still asks for.
                let _ = if method == "session/request_permission" {
                    child
                        .rpc
                        .respond(id, json!({"outcome":{"outcome":"cancelled"}}))
                        .await
                } else {
                    child.rpc.refuse(id, &method).await
                };
            }
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// The approval risk and summary for an agent's permission request. The
/// risk follows SCV's own tools: reads are read-only unless they touch a
/// secret-like path, edits are file-system work, and commands are processes.
fn describe_permission(params: &Value, label: &str) -> (ToolRisk, String) {
    let call = params.get("toolCall").unwrap_or(&Value::Null);
    let kind = call.get("kind").and_then(Value::as_str).unwrap_or("other");
    let title = call
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("an unnamed tool call");
    let paths: Vec<&str> = call
        .get("locations")
        .and_then(Value::as_array)
        .map(|locations| {
            locations
                .iter()
                .filter_map(|location| location.get("path").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let secret = paths.iter().any(|path| is_secret_like(Path::new(path)));
    let risk = match kind {
        "read" | "search" | "think" if !secret => ToolRisk::ReadOnly,
        "read" | "search" | "edit" | "delete" | "move" => ToolRisk::Filesystem,
        "execute" => ToolRisk::Process,
        "fetch" => ToolRisk::Network,
        _ => ToolRisk::Delegate,
    };
    let title = redact(&title.replace(['\n', '\r'], " "));
    let mut summary = format!("{label} {} ({kind})", bounded(&title, 500));
    let unnamed: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|path| !title.contains(path))
        .collect();
    if !unnamed.is_empty() {
        summary.push_str(&format!(" on {}", bounded(&unnamed.join(", "), 500)));
    }
    (risk, summary)
}

/// The permission outcome for SCV's decision: allow or reject once, falling
/// back to the "always" option, and cancelling when the agent offers neither.
fn choose_option(options: Option<&Value>, approved: bool) -> Value {
    let kinds: [&str; 2] = if approved {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    let options = options.and_then(Value::as_array);
    for kind in kinds {
        if let Some(option) = options
            .into_iter()
            .flatten()
            .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
            && let Some(id) = option.get("optionId")
        {
            return json!({"outcome": "selected", "optionId": id});
        }
    }
    json!({"outcome": "cancelled"})
}

/// A JSON-RPC error as one bounded, redacted line.
fn describe_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let detail = match error.get("data") {
        Some(Value::String(data)) => Some(data.clone()),
        Some(Value::Object(data)) => data
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    };
    let text = match detail {
        Some(detail) if !message.contains(detail.as_str()) => format!("{message}: {detail}"),
        _ => message.to_owned(),
    };
    bounded(&redact(&text.replace(['\n', '\r'], " ")), 1000)
}

/// Token usage from a prompt response, when the agent reports it.
fn parse_usage(result: &Value) -> Option<AgentUsage> {
    let usage = result.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64);
    let input = count("inputTokens").or_else(|| count("input_tokens"));
    let output = count("outputTokens").or_else(|| count("output_tokens"));
    (input.is_some() || output.is_some()).then(|| AgentUsage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    })
}

/// Turns `session/update` notifications into the reply and progress lines.
/// Tool output never becomes progress: only titles, kinds, and plan steps.
#[derive(Default)]
struct Progress {
    lines: LineProgress,
    titles: HashMap<String, String>,
    plan: Option<String>,
}

impl Progress {
    fn update(&mut self, params: &Value, reply: &mut Reply, sink: &scv_core::ProgressSink) {
        let Some(update) = params.get("update") else {
            return;
        };
        let text = |key: &str| update.get(key).and_then(Value::as_str);
        match text("sessionUpdate") {
            Some("agent_message_chunk") => {
                if update.pointer("/content/type").and_then(Value::as_str) == Some("text")
                    && let Some(chunk) = update.pointer("/content/text").and_then(Value::as_str)
                {
                    reply.delta(chunk);
                    self.lines.push(chunk, sink);
                }
            }
            Some("tool_call") => {
                self.lines.flush(sink);
                let title = text("title").unwrap_or_default();
                let kind = text("kind").unwrap_or("tool");
                if let Some(id) = text("toolCallId") {
                    self.titles.insert(id.to_owned(), title.to_owned());
                }
                sink.report(&line(kind, title));
            }
            Some("tool_call_update") => {
                if let (Some(id), Some(title)) = (text("toolCallId"), text("title")) {
                    self.titles.insert(id.to_owned(), title.to_owned());
                }
                if text("status") == Some("failed") {
                    let title = text("toolCallId")
                        .and_then(|id| self.titles.get(id))
                        .map_or("tool call", String::as_str);
                    sink.report(&format!("{} failed", line("", title)));
                }
            }
            Some("plan") => {
                let entries = update.get("entries").and_then(Value::as_array);
                let current = entries.and_then(|entries| {
                    entries
                        .iter()
                        .find(|entry| {
                            entry.get("status").and_then(Value::as_str) == Some("in_progress")
                        })
                        .or_else(|| {
                            entries.iter().find(|entry| {
                                entry.get("status").and_then(Value::as_str) == Some("pending")
                            })
                        })
                });
                if let Some(content) = current
                    .and_then(|entry| entry.get("content"))
                    .and_then(Value::as_str)
                {
                    let step = format!("plan: {}", line("", content));
                    if self.plan.as_deref() != Some(step.as_str()) {
                        sink.report(&step);
                        self.plan = Some(step);
                    }
                }
            }
            _ => {}
        }
    }
}

/// One short progress line: the redacted title, or the kind when untitled.
fn line(kind: &str, title: &str) -> String {
    let title = title.split(['\n', '\r']).next().unwrap_or_default().trim();
    if title.is_empty() {
        kind.to_owned()
    } else {
        bounded(&redact(title), 160)
    }
}

#[async_trait]
impl Tool for AcpAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "Launch the {} coding agent over the Agent Client Protocol (not sandboxed). \
                 Delegate substantial work here rather than doing it step by step with bash: \
                 research and web lookups, multi-file coding, and running tools, builds, and \
                 tests. Set cwd to the project the work is in so the agent follows that \
                 project's instructions and skills. Its permission requests come back to \
                 this session for approval. Each result carries a `session` handle: pass it \
                 back to follow up on the same work (answers, fixes, next steps) instead of \
                 repeating the context.",
                self.agent
            ),
            parameters: json!({
                "type":"object",
                "properties":{
                    "prompt":{"type":"string"},
                    "cwd":{
                        "type":"string",
                        "description":"Directory inside the workspace to run in, such as a project directory (\"scv\"). \
                            The agent loads that directory's AGENTS.md or CLAUDE.md and its project skills. \
                            Defaults to the workspace root."
                    },
                    "session":{
                        "type":"string",
                        "description":format!(
                            "The `session` handle an earlier call to this tool returned, such as \"{}-1\". \
                             Pass it to continue that conversation: the agent keeps its context, in the same cwd. \
                             Omit it to start a new conversation for unrelated work.",
                            self.agent
                        )
                    },
                    "model":{
                        "type":"string",
                        "description":format!(
                            "{} Set only when the user asks for a specific model; omit to use the \
                             agent's configured default. An unoffered value fails with the list of choices.",
                            self.model_hint
                        )
                    },
                    "effort":{
                        "type":"string",
                        "description":"Reasoning effort, such as low, medium, or high. Set only when the user \
                            asks for one; omit to use the agent's configured default."
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
        let executable = self.resolved.as_ref().map_or_else(
            || self.launch.command.clone(),
            |path| path.display().to_string(),
        );
        let directory = args.cwd.as_deref().map_or_else(
            || "the workspace root".to_owned(),
            |cwd| format!("{:?} (inside the workspace)", bounded(cwd, 200)),
        );
        let conversation = args.session.as_deref().map_or_else(
            || {
                format!(
                    "a new ACP session of {executable} {}",
                    self.launch.args.join(" ")
                )
            },
            |session| format!("ACP conversation {session}"),
        );
        let timeout = self.timeouts.resolve(args.timeout_seconds)?;
        let permissions = if self.full {
            " FULL PERMISSIONS (permissions = \"full\"): the agent's own approval prompts \
             and sandbox are off, so it edits files, runs commands, and uses the network \
             without asking."
        } else {
            " Its own permission requests come back here for approval."
        };
        Ok(format!(
            "Send prompt {:?} to {conversation} in {directory} for up to {} seconds. The \
             nested agent has your user permissions.{permissions}",
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
        let mut turn =
            self.conversations
                .begin(&self.agent, args.session.as_deref(), &cwd, false)?;
        let child: Arc<AcpChild> = match turn.attachment() {
            Some(Attachment(attachment)) => {
                let Ok(child) = Arc::clone(attachment).downcast::<AcpChild>() else {
                    turn.forget();
                    return Err(ToolError("conversation has no ACP session".into()));
                };
                if !child.rpc.live.is_running() {
                    let handle = turn.handle.clone();
                    turn.forget();
                    return Err(ToolError(format!(
                        "conversation {handle} ended: its ACP server exited; omit session to \
                         start a new one"
                    )));
                }
                child
            }
            None => match self
                .start(&turn, &cwd, deadline, &context.cancellation)
                .await
            {
                Ok(child) => {
                    let child = Arc::new(child);
                    turn.attach(Attachment(
                        Arc::clone(&child) as Arc<dyn std::any::Any + Send + Sync>
                    ));
                    child
                }
                Err(error) => {
                    turn.forget();
                    if context.cancellation.is_cancelled() {
                        return Err(ToolError(format!("{} start cancelled", self.name)));
                    }
                    return Ok(self.result(
                        RunStatus::Failed,
                        (String::new(), false),
                        None,
                        Some(error),
                        None,
                    ));
                }
            },
        };
        if let Err(error) = self
            .configure(&child, &args, deadline, &context.cancellation)
            .await
        {
            let handle = turn.handle.clone();
            let number = turn.turn;
            // The session is intact: a bad model or effort does not end it.
            let kept = turn.finish(Some(child.session_id.clone()), false);
            return Ok(self.result(
                RunStatus::Failed,
                (String::new(), false),
                None,
                Some(error),
                kept.as_deref().map(|_| (handle.as_str(), number)),
            ));
        }
        child.rpc.live.set_turn(turn.turn);
        let handle = turn.handle.clone();
        let number = turn.turn;
        let end = self
            .run_turn(&child, &handle, &cwd, args.prompt, &context, deadline)
            .await;
        let (status, reply, usage, error) = match end {
            TurnEnd::Ended {
                stop_reason,
                reply,
                usage,
            } => match stop_reason.as_str() {
                "end_turn" => (RunStatus::Completed, reply, usage, None),
                "cancelled" => {
                    drop(turn.finish(Some(child.session_id.clone()), false));
                    return Err(ToolError(format!("{} turn cancelled", self.name)));
                }
                "refusal" => (
                    RunStatus::Failed,
                    reply,
                    usage,
                    Some("the agent refused the request".to_owned()),
                ),
                other => (
                    RunStatus::Completed,
                    reply,
                    usage,
                    Some(format!("(stopped early: {})", bounded(other, 40))),
                ),
            },
            TurnEnd::Failed { reply, error } => (RunStatus::Failed, reply, None, Some(error)),
            TurnEnd::TimedOut { reply, settled } => {
                if !settled {
                    child.rpc.live.close().await;
                    turn.forget();
                    return Ok(self.result(
                        RunStatus::Timeout,
                        reply,
                        None,
                        Some(format!(
                            "timed out after {} seconds; the agent did not stop in time and \
                             was shut down",
                            limit.as_secs()
                        )),
                        None,
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
                    child.rpc.live.close().await;
                    turn.forget();
                }
                return Err(ToolError(format!("{} turn cancelled", self.name)));
            }
            TurnEnd::Lost(reason) => {
                let tail = child.rpc.live.stderr_tail().await;
                child.rpc.live.close().await;
                turn.forget();
                let detail = if tail.is_empty() {
                    reason
                } else {
                    format!("{reason}: {}", bounded(&redact(&tail), 1000))
                };
                return Ok(self.result(
                    RunStatus::Failed,
                    (String::new(), false),
                    None,
                    Some(detail),
                    None,
                ));
            }
        };
        let conversation = turn
            .finish(
                Some(child.session_id.clone()),
                status == RunStatus::Completed,
            )
            .map(|handle| (handle, number));
        Ok(self.result(
            status,
            reply,
            usage,
            error,
            conversation
                .as_ref()
                .map(|(handle, turn)| (handle.as_str(), *turn)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt as _, sync::Mutex as StdMutex, time::Duration};

    use async_trait::async_trait;
    use scv_core::{AgentError, ApprovalGate, ApprovalRequest, ToolApprovals};

    use super::*;
    use crate::{
        adapters::{OutputFormat, Resume, Transport},
        conversation::ConversationLimits,
        delegation::{DelegationRegistry, ProcessIdentity},
    };

    /// A Python stand-in for an ACP server. It records its PID, the
    /// `initialize` parameters, and every mode, option, and cancel call in
    /// `dir`, and acts on keywords in each prompt.
    const FAKE_AGENT: &str = r#"
import json, os, sys
DIR, MODE = sys.argv[1], sys.argv[2]
open(os.path.join(DIR, "pid"), "w").write(str(os.getpid()))
def log(line):
    with open(os.path.join(DIR, "calls"), "a") as f:
        f.write(line + "\n")
def send(message):
    message["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()
def update(session, body):
    send({"method": "session/update", "params": {"sessionId": session, "update": body}})
def chunk(session, text):
    update(session, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
OPTIONS = [{"id": "model", "options": [{"value": "m1"}, {"value": "m2"}]},
           {"id": "reasoning_effort", "options": [{"value": "low"}, {"value": "high"}]}]
memory, pending, refusals, sessions = "", None, [], 0
for line in sys.stdin:
    message = json.loads(line)
    method, params, rid = message.get("method"), message.get("params") or {}, message.get("id")
    if method is None:
        if rid == 900 and pending:
            outcome = message["result"]["outcome"]
            chunk(pending[1], "chose " + outcome.get("optionId", outcome["outcome"]))
            send({"id": pending[0], "result": {"stopReason": "end_turn"}})
            pending = None
        elif rid in (901, 902) and pending:
            refusals.append(str(message.get("error", {}).get("code")))
            if len(refusals) == 2:
                chunk(pending[1], "refused:" + ",".join(refusals))
                send({"id": pending[0], "result": {"stopReason": "end_turn"}})
                pending = None
        continue
    if method == "initialize":
        json.dump(params, open(os.path.join(DIR, "init.json"), "w"))
        send({"id": rid, "result": {"protocolVersion": 2 if MODE == "v2" else 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        sessions += 1
        log("session/new cwd=" + params["cwd"])
        if MODE == "auth":
            send({"id": rid, "error": {"code": -32000, "message": "Authentication required"}})
            continue
        send({"id": rid, "result": {"sessionId": "s%d" % sessions,
              "modes": {"currentModeId": "default", "availableModes": [{"id": "default", "name": "Default"}, {"id": "bypassPermissions", "name": "Bypass"}]},
              "configOptions": OPTIONS}})
    elif method == "session/set_mode":
        log("mode=" + params["modeId"])
        send({"id": rid, "result": {}})
    elif method == "session/set_config_option":
        log(params["configId"] + "=" + params["value"])
        send({"id": rid, "result": {"configOptions": OPTIONS}})
    elif method == "session/cancel":
        log("cancel")
        if pending and MODE != "stubborn":
            send({"id": pending[0], "result": {"stopReason": "cancelled"}})
            pending = None
    elif method == "session/prompt":
        session, text = params["sessionId"], params["prompt"][0]["text"]
        if text.startswith("remember "):
            memory = text.split(" ", 1)[1]
            chunk(session, "noted\n")
            update(session, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read notes.txt", "kind": "read", "status": "pending"})
            update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "PRIVATE-OUTPUT"}}]})
            update(session, {"sessionUpdate": "tool_call", "toolCallId": "t2", "title": "curl -H 'Authorization: Bearer sk-live-123456' x", "kind": "execute", "status": "pending"})
            update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "failed"})
            update(session, {"sessionUpdate": "plan", "entries": [{"content": "store the word", "status": "in_progress", "priority": "high"}]})
            update(session, {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "PRIVATE-THOUGHT"}})
            chunk(session, "stored " + memory)
            send({"id": rid, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 3, "outputTokens": 4}}})
        elif text == "recall":
            chunk(session, "the word is " + memory + " in " + session)
            send({"id": rid, "result": {"stopReason": "end_turn"}})
        elif text == "permission":
            pending = (rid, session)
            send({"id": 900, "method": "session/request_permission", "params": {"sessionId": session,
                  "toolCall": {"toolCallId": "t3", "title": "Write /tmp/scv-acp.txt", "kind": "edit", "locations": [{"path": "/tmp/scv-acp.txt"}]},
                  "options": [{"optionId": "allow-once", "name": "Allow", "kind": "allow_once"},
                              {"optionId": "always", "name": "Always", "kind": "allow_always"},
                              {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}})
        elif text == "client":
            pending, refusals = (rid, session), []
            send({"id": 901, "method": "fs/read_text_file", "params": {"sessionId": session, "path": "/etc/hosts"}})
            send({"id": 902, "method": "terminal/create", "params": {"sessionId": session, "command": "ls"}})
        elif text == "hang":
            pending = (rid, session)
        elif text == "die":
            sys.stderr.write("boom: out of memory\n"); sys.stderr.flush()
            sys.exit(3)
        elif text == "fail":
            send({"id": rid, "error": {"code": -32603, "message": "Internal error",
                  "data": {"message": "Unauthorized (401): Bearer sk-live-abcdef123456 expired"}}})
        elif text == "refuse":
            send({"id": rid, "result": {"stopReason": "refusal"}})
        else:
            chunk(session, "echo " + text)
            send({"id": rid, "result": {"stopReason": "end_turn"}})
    elif rid is not None:
        send({"id": rid, "error": {"code": -32601, "message": "Method not found"}})
"#;

    /// The fake agent as an executable in `dir`, run in `mode`.
    fn fake_agent(dir: &Path, mode: &str) -> PathBuf {
        let program = dir.join("fake_acp.py");
        std::fs::write(&program, FAKE_AGENT).unwrap();
        let path = dir.join(format!("fake-acp-{mode}"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nexec python3 -u \"{}\" \"{}\" {mode}\n",
                program.display(),
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn store(idle: Duration) -> Arc<ConversationStore> {
        Arc::new(ConversationStore::new(
            ConversationLimits { max: 8, idle },
            None,
        ))
    }

    fn adapter(full: bool) -> AgentAdapterConfig {
        AgentAdapterConfig {
            command: "unused".into(),
            args: Vec::new(),
            prompt_args: Vec::new(),
            full_permission_args: full.then(Vec::new),
            model_args: Vec::new(),
            effort_args: Vec::new(),
            model_hint: "Model ID.".into(),
            environment: Vec::new(),
            search_dirs: Vec::new(),
            output: OutputFormat::Text,
            resume: Resume::Unsupported,
            home: None,
            transport: Transport::Process,
            acp: None,
        }
    }

    fn acp_tool(
        script: &Path,
        conversations: Arc<ConversationStore>,
        delegation: Option<DelegationContext>,
        full_mode: Option<&str>,
    ) -> AcpAgentTool {
        AcpAgentTool::new(
            "agent_claude".into(),
            &adapter(full_mode.is_some()),
            AcpAgentLaunch {
                command: script.display().to_string(),
                args: Vec::new(),
                full_mode: full_mode.map(str::to_owned),
                required: false,
            },
            Some(script.to_owned()),
            Timeouts {
                default: Duration::from_secs(20),
                max: Duration::from_secs(30),
            },
            64 * 1024,
            delegation,
            conversations,
        )
    }

    /// Records relayed requests and answers them with `answer`.
    struct Gate {
        answer: bool,
        requests: StdMutex<Vec<ApprovalRequest>>,
    }

    #[async_trait]
    impl ApprovalGate for Gate {
        async fn approve(
            &self,
            request: ApprovalRequest,
            _cancellation: CancellationToken,
        ) -> Result<bool, AgentError> {
            self.requests.lock().unwrap().push(request);
            Ok(self.answer)
        }
    }

    fn context(workspace: &Path, gate: Option<Arc<Gate>>) -> ToolContext {
        let mut context =
            ToolContext::new(workspace.canonicalize().unwrap(), CancellationToken::new());
        if let Some(gate) = gate {
            context.approvals = ToolApprovals::new(gate, "call-1");
        }
        context.progress = scv_core::ProgressSink::buffered();
        context
    }

    fn json(output: &ToolOutput) -> Value {
        serde_json::from_str(&output.content).unwrap()
    }

    fn calls(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
    }

    fn pid(dir: &Path) -> u32 {
        std::fs::read_to_string(dir.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn alive(pid: u32) -> bool {
        ProcessIdentity::of(pid).is_some_and(|identity| identity.is_alive())
    }

    async fn wait_gone(pid: u32) {
        for _ in 0..100 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("ACP agent {pid} is still running");
    }

    #[tokio::test]
    async fn a_conversation_continues_one_session_with_bounded_progress() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let registry = Arc::new(DelegationRegistry::new(home.path()));
        let delegation = DelegationContext {
            registry: Arc::clone(&registry),
            session: "parent".into(),
            depth: 0,
        };
        let conversations = store(Duration::from_secs(3600));
        let tool = acp_tool(&script, Arc::clone(&conversations), Some(delegation), None);
        let context = context(dir.path(), None);

        let first = tool
            .execute(json!({"prompt":"remember heron"}), context.clone())
            .await
            .unwrap();
        let first_value = json(&first);
        assert_eq!(first_value["status"], "completed", "{first_value}");
        assert_eq!(first_value["agent"], "claude");
        assert_eq!(first_value["session"], "claude-1");
        assert_eq!(first_value["turn"], 1);
        assert!(
            first_value["reply"]
                .as_str()
                .unwrap()
                .contains("stored heron")
        );
        assert_eq!(first_value["usage"]["input_tokens"], 3);
        let progress = context.progress.take().unwrap_or_default();
        assert!(progress.contains("noted"), "{progress}");
        assert!(progress.contains("Read notes.txt"), "{progress}");
        assert!(progress.contains("failed"), "{progress}");
        assert!(progress.contains("plan: store the word"), "{progress}");
        for private in ["PRIVATE-OUTPUT", "PRIVATE-THOUGHT", "sk-live-123456"] {
            assert!(!progress.contains(private), "{private} leaked: {progress}");
        }
        // The client offers no file system or terminal.
        let init: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("init.json")).unwrap())
                .unwrap();
        assert_eq!(init["protocolVersion"], 1);
        assert_eq!(init["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(init["clientCapabilities"]["fs"]["writeTextFile"], false);
        assert_eq!(init["clientCapabilities"]["terminal"], false);
        let agent_pid = pid(dir.path());
        assert_eq!(registry.list(false).len(), 1, "the live agent is recorded");

        let second = tool
            .execute(
                json!({"prompt":"recall","session":"claude-1"}),
                context.clone(),
            )
            .await
            .unwrap();
        let second_value = json(&second);
        assert_eq!(second_value["turn"], 2, "{second_value}");
        assert!(
            second_value["reply"]
                .as_str()
                .unwrap()
                .contains("the word is heron in s1"),
            "{second_value}"
        );
        assert_eq!(
            pid(dir.path()),
            agent_pid,
            "one process serves the conversation"
        );
        assert_eq!(calls(dir.path()).matches("session/new").count(), 1);

        // Ending the calling session shuts the agent down.
        drop(tool);
        drop(conversations);
        wait_gone(agent_pid).await;
    }

    #[tokio::test]
    async fn permission_requests_go_through_the_callers_gate() {
        for (answer, chosen) in [(true, "chose allow-once"), (false, "chose reject")] {
            let dir = tempfile::tempdir().unwrap();
            let script = fake_agent(dir.path(), "normal");
            let gate = Arc::new(Gate {
                answer,
                requests: StdMutex::new(Vec::new()),
            });
            let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
            let output = tool
                .execute(
                    json!({"prompt":"permission"}),
                    context(dir.path(), Some(Arc::clone(&gate))),
                )
                .await
                .unwrap();
            let value = json(&output);
            assert!(value["reply"].as_str().unwrap().contains(chosen), "{value}");
            let requests = gate.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].name, "agent_claude");
            assert_eq!(requests[0].risk, ToolRisk::Filesystem);
            assert!(
                requests[0]
                    .summary
                    .starts_with("[claude-1 acp] Write /tmp/scv-acp.txt (edit)"),
                "{}",
                requests[0].summary
            );
        }
    }

    #[tokio::test]
    async fn without_a_gate_permission_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let output = tool
            .execute(json!({"prompt":"permission"}), context(dir.path(), None))
            .await
            .unwrap();
        assert!(
            json(&output)["reply"]
                .as_str()
                .unwrap()
                .contains("chose reject")
        );
    }

    #[tokio::test]
    async fn file_system_and_terminal_requests_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let output = tool
            .execute(json!({"prompt":"client"}), context(dir.path(), None))
            .await
            .unwrap();
        let value = json(&output);
        assert!(
            value["reply"]
                .as_str()
                .unwrap()
                .contains("refused:-32601,-32601"),
            "{value}"
        );
    }

    #[tokio::test]
    async fn a_timed_out_turn_is_cancelled_and_a_stubborn_agent_is_shut_down() {
        // An agent that honours session/cancel keeps its conversation.
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let conversations = store(Duration::from_secs(60));
        let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
        let output = tool
            .execute(
                json!({"prompt":"hang","timeout_seconds":1}),
                context(dir.path(), None),
            )
            .await
            .unwrap();
        let value = json(&output);
        assert_eq!(value["status"], "timeout", "{value}");
        assert_eq!(value["session"], "claude-1");
        assert!(calls(dir.path()).contains("cancel"));
        let resumed = tool
            .execute(
                json!({"prompt":"hello","session":"claude-1"}),
                context(dir.path(), None),
            )
            .await
            .unwrap();
        assert_eq!(json(&resumed)["status"], "completed");

        // One that ignores it is shut down and its conversation forgotten.
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "stubborn");
        let conversations = store(Duration::from_secs(60));
        let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
        let output = tool
            .execute(
                json!({"prompt":"hang","timeout_seconds":1}),
                context(dir.path(), None),
            )
            .await
            .unwrap();
        let value = json(&output);
        assert_eq!(value["status"], "timeout");
        assert!(value.get("session").is_none(), "{value}");
        wait_gone(pid(dir.path())).await;
        assert!(conversations.handles().is_empty());
    }

    #[tokio::test]
    async fn cancellation_sends_session_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let context = context(dir.path(), None);
        let cancel = context.cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            cancel.cancel();
        });
        let error = tool
            .execute(json!({"prompt":"hang"}), context)
            .await
            .unwrap_err();
        assert!(error.0.contains("cancelled"), "{}", error.0);
        assert!(calls(dir.path()).contains("cancel"));
    }

    #[tokio::test]
    async fn an_agent_dying_mid_turn_fails_with_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let conversations = store(Duration::from_secs(60));
        let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
        let output = tool
            .execute(json!({"prompt":"die"}), context(dir.path(), None))
            .await
            .unwrap();
        let value = json(&output);
        assert_eq!(value["status"], "failed");
        assert!(
            value["reply"]
                .as_str()
                .unwrap()
                .contains("boom: out of memory"),
            "{value}"
        );
        assert!(conversations.handles().is_empty());
    }

    #[tokio::test]
    async fn failures_are_structured_redacted_and_hint_at_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let output = tool
            .execute(json!({"prompt":"fail"}), context(dir.path(), None))
            .await
            .unwrap();
        let value = json(&output);
        assert_eq!(value["status"], "failed");
        let reply = value["reply"].as_str().unwrap();
        assert!(reply.contains("Unauthorized (401)"), "{reply}");
        assert!(!reply.contains("sk-live-abcdef123456"), "{reply}");
        assert!(
            value["hint"]
                .as_str()
                .unwrap()
                .contains("scv agents login claude"),
            "{value}"
        );

        let refused = tool
            .execute(json!({"prompt":"refuse"}), context(dir.path(), None))
            .await
            .unwrap();
        assert_eq!(json(&refused)["status"], "failed");

        let auth_dir = tempfile::tempdir().unwrap();
        let script = fake_agent(auth_dir.path(), "auth");
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let output = tool
            .execute(json!({"prompt":"hello"}), context(auth_dir.path(), None))
            .await
            .unwrap();
        let value = json(&output);
        assert_eq!(value["status"], "failed");
        assert!(
            value["reply"]
                .as_str()
                .unwrap()
                .contains("Authentication required")
        );
        assert!(value["hint"].is_string(), "{value}");
        wait_gone(pid(auth_dir.path())).await;

        let old_dir = tempfile::tempdir().unwrap();
        let script = fake_agent(old_dir.path(), "v2");
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let output = tool
            .execute(json!({"prompt":"hello"}), context(old_dir.path(), None))
            .await
            .unwrap();
        assert!(
            json(&output)["reply"]
                .as_str()
                .unwrap()
                .contains("protocol version 2")
        );
    }

    #[tokio::test]
    async fn full_permissions_select_the_mode_and_calls_choose_offered_options() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let tool = acp_tool(
            &script,
            store(Duration::from_secs(60)),
            None,
            Some("bypassPermissions"),
        );
        let summary = tool.approval_summary(&json!({"prompt":"x"})).unwrap();
        assert!(summary.contains("FULL PERMISSIONS"), "{summary}");
        let output = tool
            .execute(
                json!({"prompt":"hello","model":"m2","effort":"high"}),
                context(dir.path(), None),
            )
            .await
            .unwrap();
        assert_eq!(json(&output)["status"], "completed");
        let calls = calls(dir.path());
        assert!(calls.contains("mode=bypassPermissions"), "{calls}");
        assert!(calls.contains("model=m2"), "{calls}");
        assert!(calls.contains("reasoning_effort=high"), "{calls}");

        let unknown = tool
            .execute(
                json!({"prompt":"hello","session":"claude-1","model":"m9"}),
                context(dir.path(), None),
            )
            .await
            .unwrap();
        let value = json(&unknown);
        assert_eq!(value["status"], "failed");
        assert!(
            value["reply"]
                .as_str()
                .unwrap()
                .contains("choose one of: m1, m2"),
            "{value}"
        );
        assert_eq!(
            value["session"], "claude-1",
            "a bad model keeps the session"
        );
    }

    #[tokio::test]
    async fn idle_conversations_shut_their_agent_down() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let conversations = store(Duration::from_millis(100));
        let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
        tool.execute(json!({"prompt":"hello"}), context(dir.path(), None))
            .await
            .unwrap();
        let first = pid(dir.path());
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Starting another conversation expires the idle one.
        tool.execute(json!({"prompt":"hello"}), context(dir.path(), None))
            .await
            .unwrap();
        wait_gone(first).await;
    }

    #[test]
    fn permission_risks_follow_scvs_own_tools() {
        let risk = |kind: &str, path: &str| {
            describe_permission(
                &json!({"toolCall":{"kind":kind,"title":"t","locations":[{"path":path}]}}),
                "[x]",
            )
            .0
        };
        assert_eq!(risk("read", "src/lib.rs"), ToolRisk::ReadOnly);
        assert_eq!(risk("read", "/home/u/.env"), ToolRisk::Filesystem);
        assert_eq!(risk("edit", "a"), ToolRisk::Filesystem);
        assert_eq!(risk("execute", "a"), ToolRisk::Process);
        assert_eq!(risk("fetch", "a"), ToolRisk::Network);
        assert_eq!(risk("other", "a"), ToolRisk::Delegate);
        let (_, summary) = describe_permission(
            &json!({"toolCall":{"kind":"edit","title":"Edit file","locations":[{"path":"/tmp/a"}]}}),
            "[claude-1 acp]",
        );
        assert_eq!(summary, "[claude-1 acp] Edit file (edit) on /tmp/a");
        assert_eq!(
            choose_option(Some(&json!([{"optionId":"x","kind":"allow_always"}])), true),
            json!({"outcome":"selected","optionId":"x"})
        );
        assert_eq!(
            choose_option(Some(&json!([{"optionId":"x","kind":"allow_once"}])), false),
            json!({"outcome":"cancelled"})
        );
    }

    #[test]
    fn the_registry_prefers_an_installed_acp_server() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let with_acp = |command: &str, required: bool| {
            let mut adapter = adapter(false);
            adapter.command = "/bin/echo".into();
            adapter.acp = Some(AcpAgentLaunch {
                command: command.into(),
                args: Vec::new(),
                full_mode: None,
                required,
            });
            adapter
        };
        let description = |adapter: AgentAdapterConfig| {
            let registry = crate::builtin_registry(
                crate::ToolsConfig::default(),
                crate::SkillMap::new(),
                Vec::new(),
                1024,
                HashMap::from([("agent_claude".to_owned(), adapter)]),
            )
            .unwrap();
            registry
                .get("agent_claude")
                .map(|tool| tool.spec().description)
        };
        let installed = description(with_acp(&script.display().to_string(), false)).unwrap();
        assert!(installed.contains("Agent Client Protocol"), "{installed}");
        let missing = dir.path().join("no-such-acp-server").display().to_string();
        let fallback = description(with_acp(&missing, false)).unwrap();
        assert!(fallback.starts_with("Launch the configured"), "{fallback}");
        assert!(description(with_acp(&missing, true)).is_none());
    }
}
