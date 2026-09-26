//! [`AcpAgentTool`]: the `agent_*` tool for an agent reached over ACP.

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{
    AcpAgentLaunch, AgentAdapterConfig, DelegationContext,
    args::{Timeouts, bounded, parse_args, timeout_schema, validate_process_args},
    delegate::{
        conversation::{Attachment, ConversationStore},
        output::RunStatus,
        progress::redact,
        records,
        request::{AgentArgs, resolve_agent_cwd, valid_model_name, validate_agent_cwd},
    },
};

use super::{AcpChild, TurnEnd};

pub(crate) struct AcpAgentTool {
    pub(super) name: String,
    /// The adapter name, such as `claude`: conversation handles and results.
    pub(super) agent: String,
    pub(super) launch: AcpAgentLaunch,
    pub(super) resolved: Option<PathBuf>,
    pub(super) environment: Vec<(OsString, OsString)>,
    /// `permissions = "full"`.
    pub(super) full: bool,
    pub(super) model_hint: String,
    pub(super) timeouts: Timeouts,
    pub(super) output_limit: usize,
    pub(super) delegation: Option<DelegationContext>,
    pub(super) conversations: Arc<ConversationStore>,
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

    pub(super) fn validate(&self, args: &AgentArgs) -> Result<(), ToolError> {
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

    pub(super) fn owner_depth(&self) -> u32 {
        self.delegation
            .as_ref()
            .map_or_else(records::current_depth, DelegationContext::owner_depth)
    }
}

#[async_trait]
impl Tool for AcpAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Runs as a nested coding agent over the Agent Client Protocol (not \
                 sandboxed). Delegate substantial work here rather than doing it step by step \
                 with bash: research and web lookups, multi-file coding, and running tools, \
                 builds, and tests. Give it a self-contained brief, since it does not see this \
                 conversation, and set cwd to the project the work is in so it follows that \
                 project's instructions and skills. Its permission requests come back to \
                 this session for approval. Each result carries a `session` handle: pass it \
                 back to follow up on the same work (answers, fixes, next steps) instead of \
                 repeating the context."
                .into(),
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
                            "{} Set when the user asks, or when the work matches a configured \
                             use_for default; omit to use the agent's configured default. An \
                             unoffered value fails with the list of choices.",
                            self.model_hint
                        )
                    },
                    "effort":{
                        "type":"string",
                        "description":"Reasoning effort, such as low, medium, or high. Set when the user \
                            asks, or when the work matches a configured use_for default; omit to use \
                            the agent's configured default."
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
