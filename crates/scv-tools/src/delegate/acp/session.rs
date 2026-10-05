//! One conversation's ACP session: start the server, negotiate the
//! protocol, select modes and options, and run a prompt turn to its end.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::Path,
    sync::{Arc, Mutex as StdMutex, atomic::AtomicU64},
    time::Duration,
};

use scv_core::{ToolContext, ToolOutput};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    args::bounded,
    delegate::{
        agent::AGENT_TOOL,
        conversation::TurnGuard,
        live::{LiveChild, LiveSpec},
        options::{self, AgentOptions, EFFORT_OPTIONS, Listed, SelectValue},
        output::{AgentResult, AgentUsage, RunStatus, add_sign_in_hint},
        progress::redact,
        records,
        request::AgentArgs,
        scv::Reply,
    },
};

use super::{
    AcpAgentTool, CallError, Incoming, Interrupt, Progress, Rpc, choose_option,
    describe_permission, describe_rpc_error,
};
use crate::sync::lock;

/// The ACP major version SCV speaks.
pub(super) const PROTOCOL_VERSION: u64 = 1;
/// Longest JSON-RPC line accepted from an agent.
pub(super) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// How long a cancelled or timed-out prompt may take to settle before the
/// agent is shut down.
pub(super) const SETTLE_GRACE: Duration = Duration::from_secs(2);

/// A conversation's ACP server and its session.
#[derive(Debug)]
pub(super) struct AcpChild {
    pub(super) rpc: Rpc,
    pub(super) session_id: String,
    /// The session's config options (`model`, `effort`, ...) and their
    /// allowed values; an empty list accepts any value as given.
    pub(super) options: StdMutex<HashMap<String, Vec<SelectValue>>>,
    /// The server advertised `_meta.steering.supported` at `initialize`.
    pub(super) steerable: bool,
    /// Steering requests sent into its running prompt turn.
    pub(super) steering: Steering,
}

/// Steering requests sent into a prompt turn, whose answers only that
/// turn's reader receives.
#[derive(Debug, Default)]
pub(super) struct Steering {
    state: StdMutex<SteeringState>,
    /// Woken when an answer arrives or the turn ends.
    changed: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct SteeringState {
    /// A prompt turn is reading the agent's messages.
    turn: bool,
    /// Requests sent, by ID, and their answers once they come.
    pending: HashMap<u64, Option<Result<Value, Value>>>,
}

/// A prompt turn that receives steering answers until it drops.
pub(super) struct SteeringTurn<'a>(&'a Steering);

/// A steering request whose answer is expected, until it drops.
pub(super) struct Expected<'a> {
    steering: &'a Steering,
    id: u64,
}

/// What became of a steering request.
pub(super) enum Steered {
    /// The agent took the prompt into its running turn.
    Accepted,
    /// It refused, or its turn ended first, so the prompt went nowhere.
    Refused,
    /// No answer came in time; the agent may still take the prompt.
    Unanswered,
    /// The steering call was cancelled.
    Cancelled,
}

impl Steering {
    /// Receive answers while a prompt turn runs, until the guard drops.
    pub(super) fn turn(&self) -> SteeringTurn<'_> {
        lock(&self.state).turn = true;
        SteeringTurn(self)
    }

    /// Expect the answer to request `id`. `None` when no turn runs to
    /// receive it, so there is nothing to steer.
    pub(super) fn expect(&self, id: u64) -> Option<Expected<'_>> {
        let mut state = lock(&self.state);
        state.turn.then(|| {
            state.pending.insert(id, None);
            Expected { steering: self, id }
        })
    }

    /// Deliver the agent's answer to request `id`, when one is expected.
    pub(super) fn resolve(&self, id: u64, outcome: Result<Value, Value>) {
        if let Some(answer) = lock(&self.state).pending.get_mut(&id) {
            *answer = Some(outcome);
            self.changed.notify_waiters();
        }
    }
}

impl Expected<'_> {
    /// Wait up to `limit` for the agent's answer.
    pub(super) async fn answer(
        &self,
        limit: Duration,
        cancellation: &CancellationToken,
    ) -> Steered {
        let deadline = Instant::now() + limit;
        loop {
            let changed = self.steering.changed.notified();
            let answer = lock(&self.steering.state)
                .pending
                .get_mut(&self.id)
                .and_then(Option::take);
            if let Some(answer) = answer {
                // `{"accepted": false}` refuses as an error would.
                let accepted = answer.is_ok_and(|result| {
                    result.get("accepted").and_then(Value::as_bool) != Some(false)
                });
                return if accepted {
                    Steered::Accepted
                } else {
                    Steered::Refused
                };
            }
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep_until(deadline) => return Steered::Unanswered,
                () = cancellation.cancelled() => return Steered::Cancelled,
            }
        }
    }
}

impl Drop for Expected<'_> {
    fn drop(&mut self) {
        // A later answer is ignored.
        lock(&self.steering.state).pending.remove(&self.id);
    }
}

impl Drop for SteeringTurn<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.turn = false;
        // Nothing reads the agent's answers until the next turn, and a
        // prompt the turn did not take before it ended went nowhere.
        for answer in state.pending.values_mut().filter(|answer| answer.is_none()) {
            *answer = Some(Err(json!({"message": "the turn ended first"})));
        }
        drop(state);
        self.0.changed.notify_waiters();
    }
}

impl AcpChild {
    pub(super) fn remember_options(&self, result: &Value) {
        if let Some(options) = parse_config_options(result) {
            *lock(&self.options) = options;
        }
    }
}

pub(super) fn parse_config_options(result: &Value) -> Option<HashMap<String, Vec<SelectValue>>> {
    let options = result.get("configOptions")?.as_array()?;
    Some(
        options
            .iter()
            .filter_map(|option| {
                let id = option.get("id")?.as_str()?.to_owned();
                Some((id, options::select_values(option)))
            })
            .collect(),
    )
}

/// How a prompt turn ended.
pub(super) enum TurnEnd {
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

impl AcpAgentTool {
    /// Start the ACP server for a new conversation and open its session.
    pub(super) async fn start(
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
                &self.name,
                &delegation.session,
                cwd,
                Some((turn.handle.as_str(), turn.turn)),
            )
        });
        let mut environment = self.environment.clone();
        environment.extend(self.launch.environment.iter().cloned());
        match &pending {
            Some(pending) => environment.extend(pending.environment.iter().cloned()),
            None => environment.push((
                records::DEPTH_VARIABLE.into(),
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
            cancellation.clone(),
        )
        .await
        .map_err(|error| error.message)?;
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

    pub(super) async fn handshake(
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
                self.name,
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
        self.remember_offered(&session);
        let child = AcpChild {
            rpc,
            session_id,
            options: StdMutex::new(parse_config_options(&session).unwrap_or_default()),
            steerable: initialized
                .pointer("/_meta/steering/supported")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            steering: Steering::default(),
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

    /// Keep what a new session offers for `model` and `effort`, and save it
    /// so later sessions list those values before calling this agent. A
    /// session that offers neither leaves the known values alone.
    fn remember_offered(&self, session: &Value) {
        let Some(offered) = AgentOptions::from_acp(session) else {
            return;
        };
        if let (Some(file), Some(executable)) =
            (self.options_file.as_deref(), self.resolved.as_deref())
            && let Err(error) = options::save(file, &self.name, executable, &offered)
        {
            tracing::debug!(agent = %self.name, %error, "could not save the agent's options");
        }
        *lock(&self.offered) = Some(Listed {
            options: offered,
            seen: std::time::SystemTime::now(),
        });
    }

    /// Select the session mode that grants full permissions: through
    /// `session/set_mode` when the session lists it among its modes,
    /// otherwise through a `mode` config option.
    pub(super) async fn select_mode(
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
        } else if lock(&child.options)
            .get("mode")
            .is_some_and(|values| values.iter().any(|value| value.value == mode))
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
                self.name
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
    pub(super) async fn configure(
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
                let options = lock(&child.options);
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
            // The value as shown, or the agent's own spelling of it.
            let listed = values
                .iter()
                .find(|allowed| allowed.shown == value || allowed.value == value);
            let value = match listed {
                Some(allowed) => allowed.value.clone(),
                None if values.is_empty() => value.to_owned(),
                None => {
                    let shown: Vec<&str> = values
                        .iter()
                        .map(|allowed| allowed.shown.as_str())
                        .collect();
                    return Err(format!(
                        "{label} {value:?} is not offered by {}; choose one of: {}",
                        self.name,
                        shown.join(", ")
                    ));
                }
            };
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

    pub(super) async fn call_error(&self, rpc: &Rpc, what: &str, error: CallError) -> String {
        match error {
            CallError::Io(error) => format!("{what}: {}", error.message),
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
    pub(super) async fn run_turn(
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
            Err(error) => return TurnEnd::Lost(error.message),
        };
        // Only now, so the agent never gets a steer before its prompt.
        let _steering = child.steering.turn();
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
                Incoming::Response { id, outcome } if id != prompt_id => {
                    if let Some(id) = id.as_u64() {
                        child.steering.resolve(id, outcome);
                    }
                }
                Incoming::Response { outcome, .. } => {
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
                Incoming::Notification { method, params } => {
                    if method == "session/update" {
                        progress.update(&params, &mut reply, &context.progress);
                    }
                }
                Incoming::Request { id, method, params } => {
                    if method != "session/request_permission" {
                        if let Err(error) = child.rpc.refuse(id, &method).await {
                            return TurnEnd::Lost(error.message);
                        }
                        continue;
                    }
                    progress.lines.flush(&context.progress);
                    let (risk, summary) = describe_permission(&params, &label);
                    let request = context.approvals.request(
                        AGENT_TOOL,
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
                        return TurnEnd::Lost(error.message);
                    }
                }
            }
        }
    }

    pub(super) fn result(
        &self,
        status: RunStatus,
        (reply, cut): (String, bool),
        usage: Option<AgentUsage>,
        error: Option<String>,
        conversation: Option<(&str, u32)>,
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
        let (content, truncated) =
            result.to_json(&self.name, conversation, None, "", self.output_limit);
        let mut output = ToolOutput {
            content,
            failure: status.failure(),
            truncated,
        };
        if output.is_error() {
            add_sign_in_hint(&mut output, &self.name);
        }
        output
    }
}

/// Send `session/cancel` and wait up to [`SETTLE_GRACE`] for the prompt to end.
pub(super) async fn cancel_prompt(child: &AcpChild, prompt_id: &Value) -> bool {
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

/// Token usage from a prompt response, when the agent reports it.
pub(super) fn parse_usage(result: &Value) -> Option<AgentUsage> {
    let usage = result.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64);
    let input = count("inputTokens").or_else(|| count("input_tokens"));
    let output = count("outputTokens").or_else(|| count("output_tokens"));
    (input.is_some() || output.is_some()).then(|| AgentUsage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    })
}
