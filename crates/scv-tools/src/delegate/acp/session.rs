//! One conversation's ACP session: start the server, negotiate the
//! protocol, select modes and options, and run a prompt turn to its end.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::Path,
    sync::{Arc, Mutex as StdMutex, atomic::AtomicU64},
    time::Duration,
};

use scv_core::{AgentError, ToolContext, ToolFailure, ToolOutput};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentAuthPlan,
    args::bounded,
    delegate::{
        activity::{RunActivity, RunHeader, RunTransport},
        adapters,
        agent::AGENT_TOOL,
        conversation::TurnGuard,
        live::{LiveChild, LiveSpec},
        mode,
        options::{self, AgentOptions, EFFORT_OPTIONS, Listed, SelectValue},
        output::{AgentResult, AgentUsage, RunStatus, add_sign_in_hint},
        progress::redact,
        records,
        request::AgentArgs,
        scv::Reply,
    },
    process,
};

use super::{
    AcpAgentTool, CallError, Incoming, Interrupt, Progress, Rpc, choose_option,
    describe_permission, describe_rpc_error, permission_command,
};
use crate::sync::lock;

/// Claude Code's own plan mode, which it leaves for the mode it entered it
/// from, so a session may move to it and back after the check.
const CLAUDE_PLAN_MODE: &str = "plan";

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
    /// The session mode SCV checked against the policy, for an agent with a
    /// full mode; `None` for one it cannot verify.
    pub(super) verified_mode: Option<String>,
}

/// The session mode an ACP agent reports: `modes.currentModeId` (else the
/// `mode` config option's `currentValue`) when the session opens, then every
/// `current_mode_update`, plus any `session/set_config_option` answer or
/// `config_option_update` listing the `mode` option, whichever reader receives
/// it. Once the policy's check passed, the session holds the modes it may be in,
/// and the first move outside them is kept. Boxed, so the [`Rpc`] a failed
/// handshake hands back stays small.
#[derive(Debug, Default)]
pub(super) struct ModeWatch(Box<StdMutex<ModeState>>);

#[derive(Debug, Default)]
struct ModeState {
    /// The mode it last reported.
    current: Option<String>,
    /// How many reports arrived since the session opened.
    reports: u64,
    /// The modes the check allows, empty until it passed.
    allowed: Vec<String>,
    /// The first mode it moved to outside them.
    left: Option<String>,
}

impl ModeState {
    fn report(&mut self, mode: &str) {
        self.current = Some(mode.to_owned());
        if !self.allowed.is_empty()
            && self.left.is_none()
            && !self.allowed.iter().any(|allowed| allowed == mode)
        {
            self.left = Some(mode.to_owned());
        }
    }
}

impl ModeWatch {
    /// Note `params` of a `session/update`, when it reports the mode.
    pub(super) fn update(&self, params: &Value) {
        let Some(update) = params.get("update") else {
            return;
        };
        let mode = match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("current_mode_update") => update
                .get("currentModeId")
                .or_else(|| update.get("modeId"))
                .and_then(Value::as_str),
            Some("config_option_update") => configured_mode(update),
            _ => None,
        };
        if let Some(mode) = mode {
            self.reported(mode);
        }
    }

    /// Note the mode an answer's `configOptions` lists, if any.
    fn answered(&self, result: &Value) {
        if let Some(mode) = configured_mode(result) {
            self.reported(mode);
        }
    }

    fn reported(&self, mode: &str) {
        let mut state = lock(&self.0);
        state.reports += 1;
        state.report(mode);
    }

    /// Note the mode a session reports when it opens.
    pub(super) fn opened(&self, session: &Value) {
        if let Some(mode) = session
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .or_else(|| configured_mode(session))
        {
            lock(&self.0).report(mode);
        }
    }

    /// The mode it last reported, if any.
    pub(super) fn current(&self) -> Option<String> {
        lock(&self.0).current.clone()
    }

    fn reports(&self) -> u64 {
        lock(&self.0).reports
    }

    /// The mode it last reported, if a report arrived after `reports`.
    fn reported_since(&self, reports: u64) -> Option<String> {
        let state = lock(&self.0);
        (state.reports > reports)
            .then(|| state.current.clone())
            .flatten()
    }

    /// From now on, a move to a mode outside `allowed` fails the call.
    fn hold(&self, allowed: Vec<String>) {
        lock(&self.0).allowed = allowed;
    }

    /// The first mode it moved to outside the ones the check allows.
    pub(super) fn left(&self) -> Option<String> {
        lock(&self.0).left.clone()
    }
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
    /// Remember the options and any mode a config option answer reports.
    pub(super) fn remember_options(&self, result: &Value) {
        self.rpc.mode.answered(result);
        if let Some(options) = parse_config_options(result) {
            *lock(&self.options) = options;
        }
    }
}

/// The `currentValue` of the `mode` option among `value`'s `configOptions`.
fn configured_mode(value: &Value) -> Option<&str> {
    value
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| option.get("id").and_then(Value::as_str) == Some("mode"))?
        .get("currentValue")?
        .as_str()
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
        /// A turn without a main deadline waited too long on one of its
        /// permission requests ([`process::APPROVAL_TIMEOUT`]).
        approval: bool,
    },
    Cancelled {
        settled: bool,
    },
    /// The agent exited or broke the protocol.
    Lost(String),
    /// The session moved to a permission mode the policy's check does not
    /// allow; the turn was cancelled.
    ModeChanged(String),
}

impl AcpAgentTool {
    /// Start the ACP server for a new conversation and open its session, or
    /// load the saved session `load` for a conversation resumed from it.
    pub(super) async fn start(
        &self,
        turn: &TurnGuard,
        cwd: &Path,
        load: Option<&str>,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<AcpChild, String> {
        let executable = self.resolved.as_ref().ok_or_else(|| {
            format!(
                "{} ACP server {:?} was not found on PATH or in the user's install directories",
                self.name, self.launch.command
            )
        })?;
        if let AgentAuthPlan::Claude { mode, home, .. } = &self.auth {
            let status =
                crate::stores::claude_status(*mode, home).map_err(|error| format!("{error:#}"))?;
            if !status.ready {
                let billing = match mode {
                    crate::adapters::AgentAuth::ClaudeApiKey => "API key billing",
                    crate::adapters::AgentAuth::ClaudeSubscription => "Claude subscription",
                    crate::adapters::AgentAuth::None => "unknown billing",
                };
                return Err(format!(
                    "{billing} identity for {} is unavailable: {}",
                    self.name,
                    status.lines.join("; ")
                ));
            }
        }
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
        if let AgentAuthPlan::Claude {
            mode: crate::adapters::AgentAuth::ClaudeApiKey,
            home,
            ..
        } = &self.auth
        {
            let key = std::fs::read_to_string(home.join(".anthropic-api-key"))
                .map_err(|error| format!("read Claude API key: {error}"))?;
            environment.push(("ANTHROPIC_API_KEY".into(), key.trim().into()));
        }
        environment.extend(self.launch.environment.iter().cloned());
        match &pending {
            Some(pending) => environment.extend(pending.environment.iter().cloned()),
            None => environment.push((
                records::DEPTH_VARIABLE.into(),
                owner_depth.saturating_add(1).to_string().into(),
            )),
        }
        let activity =
            self.delegation
                .as_ref()
                .zip(pending.as_ref())
                .and_then(|(delegation, pending)| {
                    delegation.registry.open_activity(RunHeader {
                        handle: pending.handle.clone(),
                        agent: self.name.clone(),
                        session: delegation.session.clone(),
                        transport: RunTransport::Acp,
                        cwd: cwd.to_owned(),
                        conversation: Some((turn.handle.clone(), turn.turn)),
                        streams: true,
                    })
                });
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
                activity,
            },
            registration,
            cancellation.clone(),
        )
        .await
        .map_err(|error| error.message)?;
        let rpc = Rpc {
            live,
            next_id: AtomicU64::new(1),
            mode: ModeWatch::default(),
        };
        match self.handshake(rpc, cwd, load, deadline, cancellation).await {
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
        load: Option<&str>,
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
        let mut session_params = json!({"cwd": cwd, "mcpServers": []});
        if let AgentAuthPlan::Claude { settings, .. } = &self.auth {
            session_params["_meta"] = json!({
                "claudeCode": {"options": {
                    "settings": settings,
                    "disallowedTools": adapters::CLAUDE_DENY_RULES,
                }}
            });
        }
        let method = if let Some(saved) = load {
            let loads = initialized
                .pointer("/agentCapabilities/loadSession")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !loads {
                return Err((
                    rpc,
                    format!(
                        "the {} ACP server cannot load a saved session (it does not offer \
                         loadSession), so this conversation cannot be resumed; start a new one",
                        self.name
                    ),
                ));
            }
            session_params["sessionId"] = saved.into();
            "session/load"
        } else {
            "session/new"
        };
        let session = match rpc
            .call(method, session_params, deadline, cancellation)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let message = self.call_error(&rpc, method, error).await;
                return Err((rpc, message));
            }
        };
        // `session/load` answers with the session's options, not its ID.
        let session_id = if let Some(saved) = load {
            saved.to_owned()
        } else {
            let Some(session_id) = session
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                return Err((rpc, "the agent's session/new returned no sessionId".into()));
            };
            session_id
        };
        self.remember_offered(&session);
        if let Some(activity) = rpc.live.activity() {
            activity.set_vendor(&session_id);
        }
        // What the session reports now outranks any update a loaded
        // session replayed.
        rpc.mode.opened(&session);
        let mut child = AcpChild {
            rpc,
            session_id,
            options: StdMutex::new(parse_config_options(&session).unwrap_or_default()),
            steerable: initialized
                .pointer("/_meta/steering/supported")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            steering: Steering::default(),
            verified_mode: None,
        };
        if let Some(full_mode) = self.launch.full_mode.clone() {
            match self
                .check_mode(&child, &session, &full_mode, deadline, cancellation)
                .await
            {
                Ok(confirmed) => child.verified_mode = Some(confirmed),
                Err(error) => return Err((child.rpc, error)),
            }
        }
        if let Some(activity) = child.rpc.live.activity() {
            activity.set_steerable(child.steerable);
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

    /// What the policy expects of the session mode of this agent, whose
    /// full mode is `full_mode`.
    pub(super) fn expected_mode(&self, full_mode: &str) -> mode::Expected {
        mode::Expected {
            agent: self.name.clone(),
            policy: self.policy.clone(),
            full_mode: full_mode.to_owned(),
            full: self.full,
        }
    }

    /// Check the session's mode against the policy before any prompt:
    /// under `auto`, select `full_mode` and confirm the agent reported
    /// taking it; otherwise, the mode the session opened in must not be
    /// `full_mode`. A missing report fails like a mismatch.
    /// From then on the session may move only back and forth to Claude's
    /// own `plan` mode. Returns the mode confirmed.
    pub(super) async fn check_mode(
        &self,
        child: &AcpChild,
        session: &Value,
        full_mode: &str,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<String, String> {
        let expected = self.expected_mode(full_mode);
        let confirmed = if self.full {
            self.select_mode(child, session, full_mode, deadline, cancellation)
                .await?
        } else {
            child.rpc.mode.current()
        }
        .ok_or_else(|| expected.missing())?;
        if !expected.accepts(&confirmed) {
            return Err(expected.mismatch(&confirmed));
        }
        let mut allowed = vec![confirmed.clone()];
        if matches!(self.auth, AgentAuthPlan::Claude { .. }) {
            allowed.push(CLAUDE_PLAN_MODE.to_owned());
        }
        child.rpc.mode.hold(allowed);
        Ok(confirmed)
    }

    /// Select the session mode that grants full permissions: through a
    /// `mode` config option when the session offers one, whose answer lists
    /// the mode the agent is in, otherwise through `session/set_mode` when
    /// the session lists it among its modes. An answer of `{}` says nothing:
    /// `claude-agent-acp` answers so even when it picks a different mode,
    /// and `codex-acp` answers so and reports nothing. Returns the mode the
    /// agent reported during the call, in that answer or in an update, and
    /// `None` when it reported none.
    pub(super) async fn select_mode(
        &self,
        child: &AcpChild,
        session: &Value,
        mode: &str,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, String> {
        let listed = session
            .pointer("/modes/availableModes")
            .and_then(Value::as_array)
            .is_some_and(|modes| {
                modes
                    .iter()
                    .any(|entry| entry.get("id").and_then(Value::as_str) == Some(mode))
            });
        let reports = child.rpc.mode.reports();
        let result = if lock(&child.options)
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
        } else if listed {
            child
                .rpc
                .call(
                    "session/set_mode",
                    json!({"sessionId": child.session_id, "modeId": mode}),
                    deadline,
                    cancellation,
                )
                .await
        } else {
            return Err(format!(
                "policy mode {} needs the {} ACP session mode {mode:?}, which this agent does not \
                 offer",
                self.policy, self.name
            ));
        };
        match result {
            Ok(result) => {
                child.remember_options(&result);
                Ok(child.rpc.mode.reported_since(reports))
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
            CallError::Rpc(error) => {
                if what == "session/new"
                    && matches!(
                        self.auth,
                        AgentAuthPlan::Claude {
                            mode: crate::adapters::AgentAuth::ClaudeApiKey,
                            ..
                        }
                    )
                    && error.pointer("/data/reason").and_then(Value::as_str)
                        == Some("claude_subscription_not_supported")
                {
                    "claude is API-key only but its home holds a Claude subscription login; move that login to agents/claude-ai, or run HOME=$SCV_HOME/agents/claude claude auth logout".into()
                } else if what == "session/new"
                    && matches!(
                        self.auth,
                        AgentAuthPlan::Claude {
                            mode: crate::adapters::AgentAuth::ClaudeApiKey,
                            ..
                        }
                    )
                    && error.get("data").is_none()
                    && error
                        .get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains("Authentication required"))
                {
                    "claude has no usable API key; run scv agents login claude".into()
                } else {
                    format!("{what} failed: {}", describe_rpc_error(&error))
                }
            }
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
        deadline: Option<Instant>,
    ) -> TurnEnd {
        // A move to another mode since the check fails the call before the
        // agent gets the prompt.
        if let Some(left) = child.rpc.mode.left() {
            return TurnEnd::ModeChanged(left);
        }
        // Handing over the prompt is part of starting the turn, which stays
        // bounded without a main deadline.
        let prompt_id = match child
            .rpc
            .request(
                "session/prompt",
                json!({
                    "sessionId": child.session_id,
                    "prompt": [{"type": "text", "text": prompt}]
                }),
                Some(process::startup_deadline(deadline)),
                &context.cancellation,
            )
            .await
        {
            Ok(id) => json!(id),
            // Not taken, or taken in part with the agent's input then
            // closed: either way there is no turn to settle.
            Err(error) if error.kind == ToolFailure::Cancelled => {
                return TurnEnd::Cancelled { settled: false };
            }
            Err(error) => return TurnEnd::Lost(error.message),
        };
        // Only now, so the agent never gets a steer before its prompt.
        let _steering = child.steering.turn();
        let label = format!("[{handle} acp]");
        let mut reply = Reply::new(self.output_limit);
        let mut progress = Progress::default();
        let activity: Option<&RunActivity> = child.rpc.live.activity();
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
                            approval: false,
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
                        progress.update(&params, &mut reply, &context.progress, activity);
                    }
                    if let Some(left) = child.rpc.mode.left() {
                        progress.lines.flush(&context.progress);
                        cancel_prompt(child, &prompt_id).await;
                        return TurnEnd::ModeChanged(left);
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
                    if let Some(activity) = activity {
                        activity.approval_requested(risk.as_str(), &summary);
                    }
                    let request = context.approvals.request_until(
                        AGENT_TOOL,
                        risk,
                        cwd.to_owned(),
                        summary,
                        permission_command(&params),
                        context.cancellation.child_token(),
                        Some(process::approval_deadline(deadline)),
                    );
                    let answer = request.await;
                    if let Some(activity) = activity {
                        activity.approval_resolved(matches!(answer, Ok(true)));
                    }
                    let approved = match answer {
                        Ok(approved) => approved,
                        Err(error) => {
                            let timed_out = matches!(error, AgentError::ToolLimit(_))
                                && !context.cancellation.is_cancelled();
                            // The request is owed an answer even now. `respond`
                            // sends it within its own grace, not racing it
                            // against the passed deadline or fired token that
                            // ended the wait, so the agent is never left
                            // waiting on it while `session/cancel` settles.
                            let _ = child
                                .rpc
                                .respond(id, json!({"outcome":{"outcome":"cancelled"}}))
                                .await;
                            let settled = cancel_prompt(child, &prompt_id).await;
                            return if timed_out {
                                TurnEnd::TimedOut {
                                    reply: reply.finish(),
                                    settled,
                                    approval: deadline.is_none(),
                                }
                            } else {
                                TurnEnd::Cancelled { settled }
                            };
                        }
                    };
                    let outcome = choose_option(params.get("options"), approved);
                    if let Err(error) = child.rpc.respond(id, json!({"outcome": outcome})).await {
                        return TurnEnd::Lost(error.message);
                    }
                }
            }
        }
    }

    /// What a result says about the mode of a turn on `child`: the mode
    /// SCV verified, or `unverified` with the mode it reports, if any, for
    /// an agent with a switch but no full mode to check.
    pub(super) fn mode_status(&self, child: &AcpChild) -> Option<String> {
        match &child.verified_mode {
            Some(verified) => Some(mode::verified(verified)),
            None => mode::unverified(
                if self.mode_switch {
                    mode::Check::Unverified
                } else {
                    mode::Check::NotApplicable
                },
                child.rpc.mode.current().as_deref(),
            ),
        }
    }

    pub(super) fn result(
        &self,
        status: RunStatus,
        (reply, cut): (String, bool),
        usage: Option<AgentUsage>,
        error: Option<String>,
        conversation: Option<(&str, u32)>,
        mode: Option<String>,
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
            mode,
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
        match child.rpc.next(Some(deadline), &never).await {
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
