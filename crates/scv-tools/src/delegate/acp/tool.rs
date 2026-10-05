//! [`AcpAgentTool`]: the backend of an agent reached over ACP.

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use scv_core::{ToolContext, ToolError, ToolOutput, ToolRisk};
use serde_json::Value;
use tokio::time::Instant;

use crate::{
    AcpAgentLaunch, AgentAdapterConfig, DelegationContext,
    args::{Timeouts, bounded, parse_args, validate_process_args},
    delegate::{
        agent::{Accepts, Backend},
        conversation::{Attachment, ConversationStore},
        options::{self, AgentOptions, Choice, Listed},
        output::RunStatus,
        progress::redact,
        records,
        request::{
            AgentArgs, resolve_agent_cwd, valid_effort, valid_model_name, validate_agent_cwd,
        },
    },
};

use super::{AcpChild, Steered, TurnEnd};
use crate::sync::lock;

/// How long a steer waits for the agent to answer whether it took the prompt.
const STEER_ANSWER_WAIT: Duration = Duration::from_secs(30);

pub(crate) struct AcpAgentTool {
    /// The adapter name, such as `claude`: conversation handles and results.
    pub(super) name: String,
    pub(super) launch: AcpAgentLaunch,
    pub(super) resolved: Option<PathBuf>,
    pub(super) environment: Vec<(OsString, OsString)>,
    /// `permissions = "full"`.
    pub(super) full: bool,
    /// `model` and `effort` as the adapter offers them, and always
    /// `session`, since an ACP session continues.
    accepts: Accepts,
    pub(super) timeouts: Timeouts,
    pub(super) output_limit: usize,
    pub(super) delegation: Option<DelegationContext>,
    pub(super) conversations: Arc<ConversationStore>,
    /// Where this agent's offered model and effort values are saved.
    pub(super) options_file: Option<PathBuf>,
    /// Those values, when they are recent and from the installed server;
    /// each new ACP session of this tool replaces them.
    pub(super) offered: StdMutex<Option<Listed>>,
    /// Refuse a model the saved values do not list before starting. Off for
    /// `scv agents check`, where the agent's own list decides.
    precheck: bool,
    /// `[agents.<name>] model`, named when that is the model refused.
    default_model: Option<String>,
}

impl AcpAgentTool {
    #[allow(clippy::too_many_arguments, reason = "one field per adapter setting")]
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
        let offered = adapter
            .options_file
            .as_deref()
            .zip(resolved.as_deref())
            .and_then(|(file, executable)| {
                options::load(file, &name, executable, SystemTime::now())
            });
        let session_options = launch.session_options;
        Self {
            name,
            launch,
            resolved,
            environment: adapter.environment.clone(),
            full: adapter.full_permission_args.is_some(),
            accepts: Accepts {
                model: !adapter.model_args.is_empty() || session_options,
                effort: !adapter.effort_args.is_empty() || session_options,
                session: true,
            },
            timeouts,
            output_limit,
            delegation,
            conversations,
            options_file: adapter.options_file.clone(),
            offered: StdMutex::new(offered),
            precheck: true,
            default_model: adapter.defaults.model.clone(),
        }
    }

    /// Whether a model the saved values do not list is refused before the
    /// call starts.
    pub(crate) fn with_precheck(mut self, precheck: bool) -> Self {
        self.precheck = precheck;
        self
    }

    /// What this agent takes.
    pub(crate) fn accepts(&self) -> Accepts {
        self.accepts
    }

    /// The model and effort values this agent's server offered recently.
    pub(crate) fn offered(&self) -> Option<AgentOptions> {
        lock(&self.offered)
            .as_ref()
            .filter(|listed| listed.fresh(SystemTime::now()))
            .map(|listed| listed.options.clone())
    }

    /// Refuse a model the agent did not list, before anything starts. The
    /// saved file is read again first, since another session or `scv agents
    /// check` may have saved a newer list. Effort levels are left to the
    /// agent: they depend on the model, and a session's model is not always
    /// known here.
    fn check_offered(&self, args: &AgentArgs) -> Result<(), ToolError> {
        let Some(model) = args.model.as_deref().filter(|_| self.precheck) else {
            return Ok(());
        };
        let refusing = |listed: Option<&Listed>| -> Option<Choice> {
            listed
                .and_then(|listed| listed.options.model.clone())
                .filter(|choice| !choice.offers(model))
        };
        let now = SystemTime::now();
        let current = lock(&self.offered)
            .clone()
            .filter(|listed| listed.fresh(now));
        if refusing(current.as_ref()).is_none() {
            return Ok(());
        }
        let reloaded = self
            .options_file
            .as_deref()
            .zip(self.resolved.as_deref())
            .and_then(|(file, executable)| options::load(file, &self.name, executable, now));
        let refused = refusing(reloaded.as_ref());
        if reloaded.is_some() {
            *lock(&self.offered) = reloaded;
        }
        let Some(choice) = refused else {
            return Ok(());
        };
        let (name, values) = (&self.name, choice.shown().join(", "));
        let refresh = format!(
            "This list is from {name}'s last session; if the user named a newer one, run `scv \
             agents check {name}` to refresh it"
        );
        // Omitting the model would bring the same default back.
        if self.default_model.as_deref() == Some(model) {
            return Err(ToolError::invalid_arguments(format!(
                "model {:?} is the user's default for {name} ([agents.{name}] model), but not \
                 one {name} offers; pass one of: {values}, and tell the user so they can change \
                 that setting. {refresh}",
                bounded(model, 80)
            )));
        }
        let omit = self.default_model.as_deref().map_or_else(
            || "its default".to_owned(),
            |default| format!("the user's default, {default}"),
        );
        Err(ToolError::invalid_arguments(format!(
            "model {:?} is not one {name} offers; choose one of: {values}, or omit model for \
             {omit}. {refresh}",
            bounded(model, 80)
        )))
    }

    pub(super) fn validate(&self, args: &AgentArgs) -> Result<(), ToolError> {
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
        if let Some(model) = &args.model
            && !valid_model_name(model)
        {
            return Err(ToolError::invalid_arguments(format!(
                "invalid model {model:?}"
            )));
        }
        if let Some(effort) = &args.effort
            && !valid_effort(effort)
        {
            return Err(ToolError::invalid_arguments(format!(
                "invalid effort {effort:?}"
            )));
        }
        self.check_offered(args)
    }

    pub(super) fn owner_depth(&self) -> u32 {
        self.delegation
            .as_ref()
            .map_or_else(records::current_depth, DelegationContext::owner_depth)
    }
}

#[async_trait]
impl Backend for AcpAgentTool {
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

    fn busy(&self, arguments: &Value) -> Result<bool, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        Ok(args
            .session
            .as_deref()
            .is_some_and(|h| self.conversations.is_busy(h)))
    }

    async fn wait_idle(
        &self,
        arguments: &Value,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        if let Some(handle) = args.session.as_deref() {
            self.conversations.wait_idle(handle, cancellation).await?;
        }
        Ok(())
    }

    async fn steer(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let args: AgentArgs = parse_args(&arguments)?;
        let Some(handle) = args.session.as_deref() else {
            return Ok(None);
        };
        let Some(Attachment(attachment)) = self.conversations.attachment(handle) else {
            return Ok(None);
        };
        let Ok(child) = Arc::downcast::<AcpChild>(attachment) else {
            return Ok(None);
        };
        if !child.steerable {
            return Ok(None);
        }
        let request = child.rpc.reserve_request();
        let Some(expected) = child.steering.expect(request) else {
            return Ok(None);
        };
        let params = serde_json::json!({
            "sessionId": child.session_id,
            "prompt": [{"type": "text", "text": args.prompt}]
        });
        child
            .rpc
            .send_request(request, "_session/steering", params)
            .await?;
        match expected
            .answer(STEER_ANSWER_WAIT, &context.cancellation)
            .await
        {
            Steered::Accepted => Ok(Some(ToolOutput::success(
                serde_json::json!({
                    "agent": self.name,
                    "status": "steered",
                    "session": handle,
                    "note": "The agent took the prompt into its running turn; the result \
                             comes with that turn's."
                })
                .to_string(),
            ))),
            Steered::Refused => Ok(None),
            // Queueing it now could deliver the prompt twice.
            Steered::Unanswered => Err(ToolError::failed(format!(
                "{} did not answer the steering request for conversation {handle} within {} \
                 seconds; it may still take the prompt, so check that turn's result before \
                 sending it again",
                self.name,
                STEER_ANSWER_WAIT.as_secs()
            ))),
            Steered::Cancelled => Err(ToolError::cancelled("steering cancelled")),
        }
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
                .begin(&self.name, args.session.as_deref(), &cwd, false)?;
        let child: Arc<AcpChild> = match turn.attachment() {
            Some(Attachment(attachment)) => {
                let Ok(child) = Arc::clone(attachment).downcast::<AcpChild>() else {
                    turn.forget();
                    return Err(ToolError::failed("conversation has no ACP session"));
                };
                if !child.rpc.live.is_running() {
                    let handle = turn.handle.clone();
                    turn.forget();
                    return Err(ToolError::unavailable(format!(
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
                        return Err(ToolError::cancelled(format!(
                            "{} start cancelled",
                            self.name
                        )));
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
        // Recorded at work until this call returns, however it ends.
        let _serving = child.rpc.live.begin_turn(turn.turn);
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
                    return Err(ToolError::cancelled(format!(
                        "{} turn cancelled",
                        self.name
                    )));
                }
                "refusal" => (RunStatus::Declined, reply, usage, None),
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
                return Err(ToolError::cancelled(format!(
                    "{} turn cancelled",
                    self.name
                )));
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
