//! [`AgentTool`]: the `agent` tool, the model's one way to hand work to
//! another agent. Which agent runs is an argument. The tool checks the call
//! against what that agent takes and passes it to the agent's backend: one
//! CLI process per turn ([`native`](super::native)), an Agent Client
//! Protocol server ([`acp`](super::acp)), or a nested SCV
//! ([`scv`](super::scv)).
//!
//! The agent is, in order: the one whose conversation a `session` handle
//! names; the `agent` argument; the first agent in the user's `[agent]
//! prefer` that the session offers. Without that default the schema requires
//! `agent`, so SCV itself never picks one.
//!
//! A call that names no `model` or `effort` gets the user's
//! `[agents.<name>]` defaults, filled in here so every backend, background
//! job, and approval sees the values that run. A continued conversation
//! keeps what its session holds instead.

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolRisk, ToolSpec};
use serde_json::{Map, Value, json};

use crate::{
    AgentDefaults, BusyBehavior, BusyConfig,
    args::{Timeouts, bounded, parse_args, timeout_schema},
    delegate::{
        choice, conversation,
        options::AgentOptions,
        request::{AGENT_EFFORTS, AgentArgs},
    },
};

/// The delegation tool's name.
pub(crate) const AGENT_TOOL: &str = "agent";

/// Runs one agent's calls. The [`AgentTool`] has chosen the agent and
/// checked the call's options against its [`Accepts`] before a backend sees
/// the call.
#[async_trait]
pub(crate) trait Backend: Send + Sync {
    /// Validate the call without running anything. The risk is always
    /// [`ToolRisk::Delegate`].
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError>;

    /// What the user approves: what starts or continues, where, and for how long.
    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError>;

    /// Whether the conversation the call continues is running a turn.
    fn busy(&self, _arguments: &Value) -> Result<bool, ToolError> {
        Ok(false)
    }

    /// Add the call's prompt to the running turn of the conversation it
    /// continues. `None` when that turn cannot take it, so the caller's
    /// `steer_fallback` applies.
    async fn steer(
        &self,
        _arguments: Value,
        _context: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    /// Whether a conversation of this agent can be continued from the
    /// agent's own session ID alone, with no process kept from before, as
    /// resuming an interrupted job does: a CLI that resumes by ID, or an ACP
    /// server that loads saved sessions.
    fn adoptable(&self) -> bool {
        false
    }

    /// Wait until the conversation the call continues runs no turn.
    async fn wait_idle(
        &self,
        _arguments: &Value,
        _cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

/// The optional arguments an agent takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Accepts {
    /// `model`: agents with `model_args`, and the nested SCV.
    pub(crate) model: bool,
    /// `effort`: agents with `effort_args`.
    pub(crate) effort: bool,
    /// `session`: agents that can continue a conversation.
    pub(crate) session: bool,
}

impl Accepts {
    fn takes(self, option: &str) -> bool {
        match option {
            "model" => self.model,
            "effort" => self.effort,
            "session" => self.session,
            _ => false,
        }
    }
}

/// One agent this session offers.
pub(crate) struct Offered {
    /// The adapter name, such as `codex`: the `agent` argument's value.
    pub(crate) name: String,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) accepts: Accepts,
    /// Describes the `model` values this agent takes.
    pub(crate) model_hint: String,
    /// The model and effort values its ACP server offered recently, which
    /// the description lists in place of the hint.
    pub(crate) offered: Option<AgentOptions>,
    /// The user's `[agents.<name>] use_for` note.
    pub(crate) use_for: Option<String>,
    /// The user's `[agents.<name>]` model and efforts.
    pub(crate) defaults: AgentDefaults,
    /// Whether a continued conversation keeps the model and effort its
    /// session was given, as an ACP server or a nested SCV does. A CLI run
    /// once per turn does not, so the defaults fill in on every turn.
    pub(crate) holds_settings: bool,
    /// How a call to one of its busy conversations is handled.
    pub(crate) busy: BusyConfig,
}

impl Offered {
    /// `arguments` as this agent's backend receives them: a `model` or
    /// `effort` the call leaves out (or blank) is the user's default, when
    /// the agent takes it. A turn of a conversation whose session holds its
    /// settings gets none, so it keeps what it was started with.
    fn with_defaults(&self, arguments: &Value, args: &AgentArgs) -> Value {
        let mut arguments = arguments.clone();
        if args.session.is_some() && self.holds_settings {
            return arguments;
        }
        let Some(object) = arguments.as_object_mut() else {
            return arguments;
        };
        for (option, given, default, taken) in [
            (
                "model",
                &args.model,
                &self.defaults.model,
                self.accepts.model,
            ),
            (
                "effort",
                &args.effort,
                &self.defaults.effort,
                self.accepts.effort,
            ),
        ] {
            if let Some(default) = default.as_ref().filter(|_| given.is_none() && taken) {
                object.insert(option.into(), default.clone().into());
            }
        }
        arguments
    }

    /// What a call to one of this agent's busy conversations does: the
    /// call's own `on_busy`, else the agent's.
    pub(crate) fn on_busy(&self, routed: &Value) -> Result<BusyBehavior, ToolError> {
        Ok(parse_args::<AgentArgs>(routed)?
            .on_busy
            .unwrap_or(self.busy.behavior))
    }
}

/// The `agent` tool.
pub(crate) struct AgentTool {
    /// Sorted by name.
    agents: Vec<Offered>,
    /// The first offered agent in `[agent] prefer`.
    default: Option<usize>,
    timeouts: Timeouts,
    /// Whether calls may set `no_timeout`: the owner allows it
    /// (`tools.allow_no_timeout`) and the session runs background jobs.
    no_timeout: bool,
    /// Where calls with `worktree: true` get their git worktrees; `None`
    /// leaves `worktree` out.
    worktrees: Option<std::path::PathBuf>,
}

impl AgentTool {
    /// Offer `agents`; the first of them named in `prefer` runs a call that
    /// names none.
    pub(crate) fn new(mut agents: Vec<Offered>, prefer: &[String], timeouts: Timeouts) -> Self {
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        let default = prefer
            .iter()
            .find_map(|name| agents.iter().position(|agent| agent.name == *name));
        Self {
            agents,
            default,
            timeouts,
            no_timeout: false,
            worktrees: None,
        }
    }

    /// Offer `worktree`, creating worktrees under `root`.
    pub(crate) fn with_worktrees(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.worktrees = root;
        self
    }

    /// When the call asks for `worktree`, create it and point the call's
    /// `cwd` at the same directory inside it: the worktree it runs in.
    /// Nothing happens for a call that does not ask.
    pub(crate) async fn prepare_worktree(
        &self,
        arguments: &mut Value,
        context: &ToolContext,
    ) -> Result<Option<scv_protocol::WorktreeRef>, ToolError> {
        let args = parse_args::<AgentArgs>(arguments)?;
        if !args.worktree {
            return Ok(None);
        }
        let root = self.worktrees.as_deref().ok_or_else(|| {
            ToolError::invalid_arguments("worktree is not available in this session")
        })?;
        let (agent, _) = self.route(arguments)?;
        let cwd =
            crate::delegate::request::resolve_agent_cwd(&context.workspace, args.cwd.as_deref())?;
        let created =
            crate::delegate::worktree::create(root, &cwd, &agent.name, &context.cancellation)
                .await?;
        context.progress.report(&format!(
            "worktree {} on branch {}",
            created.worktree.path, created.worktree.branch
        ));
        if let Some(object) = arguments.as_object_mut() {
            object.remove("worktree");
            object.insert("cwd".into(), created.cwd.display().to_string().into());
        }
        Ok(Some(created.worktree))
    }

    /// Let background jobs' calls set `no_timeout` when `offered`.
    pub(crate) fn with_no_timeout(mut self, offered: bool) -> Self {
        self.no_timeout = offered;
        self
    }

    /// Refuse a call's `no_timeout` unless this session offers it, the call
    /// runs as a background job, and it sets no `timeout_seconds`.
    fn check_no_timeout(&self, arguments: &Value, background_job: bool) -> Result<(), ToolError> {
        let args = parse_args::<AgentArgs>(arguments)?;
        if args.no_timeout && !self.no_timeout {
            return Err(ToolError::invalid_arguments(
                "no_timeout is not available in this session: it needs the owner's \
                 tools.allow_no_timeout and background jobs (agent.max_background above 0)",
            ));
        }
        self.timeouts
            .deadline(args.timeout_seconds, args.no_timeout, background_job)
            .map(drop)
    }

    /// [`Tool::risk`] for a call that runs as a background job when
    /// `background_job`, which alone may set `no_timeout`.
    pub(crate) fn risk_for(
        &self,
        arguments: &Value,
        background_job: bool,
    ) -> Result<ToolRisk, ToolError> {
        self.check_no_timeout(arguments, background_job)?;
        let (agent, arguments) = self.route(arguments)?;
        agent.backend.risk(&arguments)
    }

    /// [`Tool::approval_summary`] for a call that runs as a background job
    /// when `background_job`.
    pub(crate) fn approval_summary_for(
        &self,
        arguments: &Value,
        background_job: bool,
    ) -> Result<String, ToolError> {
        self.check_no_timeout(arguments, background_job)?;
        let (agent, arguments) = self.route(arguments)?;
        let worktree = if parse_args::<AgentArgs>(&arguments)?.worktree {
            " It runs in a new git worktree of the repository holding that directory, on a new \
             branch, under SCV's worktrees directory."
        } else {
            ""
        };
        Ok(format!(
            "agent {}{}: {}{worktree}",
            agent.name,
            crate::adapters::adapter(&agent.name)
                .and_then(|descriptor| match descriptor.auth {
                    crate::adapters::AgentAuth::ClaudeApiKey => Some(" (API key billing)"),
                    crate::adapters::AgentAuth::ClaudeSubscription =>
                        Some(" (Claude subscription)"),
                    crate::adapters::AgentAuth::None => None,
                })
                .unwrap_or_default(),
            agent.backend.approval_summary(&arguments)?
        ))
    }

    /// The offered agent `name`.
    pub(crate) fn find(&self, name: &str) -> Option<&Offered> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// Every offered agent's name, sorted.
    pub(crate) fn names(&self) -> Vec<&str> {
        self.agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect()
    }

    /// The offered agents that can continue a conversation.
    pub(crate) fn continuing(&self) -> Vec<&str> {
        self.agents
            .iter()
            .filter(|agent| agent.accepts.session)
            .map(|agent| agent.name.as_str())
            .collect()
    }

    /// A call's default and longest timeout.
    pub(crate) fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// The agent that runs a call, once the call's options are checked
    /// against what that agent takes, and the arguments its backend gets,
    /// with the user's defaults filled in. Nothing has launched when this
    /// fails.
    pub(crate) fn route(&self, arguments: &Value) -> Result<(&Offered, Value), ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        let agent = self.choose(&args)?;
        for (option, given) in [
            ("model", args.model.is_some()),
            ("effort", args.effort.is_some()),
            ("session", args.session.is_some()),
        ] {
            if given && !agent.accepts.takes(option) {
                return Err(self.not_taken(agent, option));
            }
        }
        if args.worktree {
            if self.worktrees.is_none() {
                return Err(ToolError::invalid_arguments(
                    "worktree is not available in this session",
                ));
            }
            if args.session.is_some() {
                return Err(ToolError::invalid_arguments(
                    "worktree starts a new conversation in a new worktree; a conversation you \
                     continue keeps its own directory, so omit worktree or session",
                ));
            }
        }
        Ok((agent, agent.with_defaults(arguments, &args)))
    }

    /// Run the call once its conversation runs no turn, for a call whose
    /// turn has come in the conversation's lane, so no busy policy applies.
    pub(crate) async fn execute_when_idle(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let (agent, arguments) = self.route(&arguments)?;
        self.check_no_timeout(&arguments, context.background_job)?;
        agent
            .backend
            .wait_idle(&arguments, &context.cancellation)
            .await?;
        self.run(agent, arguments, context).await
    }

    /// Run the routed call on `agent`'s backend and settle its result.
    async fn run(
        &self,
        agent: &Offered,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let result = agent.backend.execute(arguments, context).await;
        let family = crate::adapters::adapter(&agent.name).map(|descriptor| descriptor.family);
        let others: Vec<&str> = self
            .names()
            .into_iter()
            .filter(|name| *name != agent.name)
            .filter(|name| {
                family.is_none_or(|family| {
                    crate::adapters::adapter(name)
                        .is_none_or(|descriptor| descriptor.family != family)
                })
            })
            .collect();
        choice::settle(result, &others)
    }

    fn choose(&self, args: &AgentArgs) -> Result<&Offered, ToolError> {
        let named = args.agent.as_deref();
        if let Some(session) = args.session.as_deref()
            && let Some(owner) = conversation::handle_agent(session)
        {
            let Some(agent) = self.find(owner) else {
                return Err(ToolError::invalid_arguments(format!(
                    "conversation {session} is unknown in this session; omit session to start \
                     a new conversation"
                )));
            };
            if let Some(named) = named.filter(|named| *named != owner) {
                let named = bounded(named, 40);
                return Err(ToolError::invalid_arguments(format!(
                    "conversation {session} belongs to {owner}, not {named}; omit agent to \
                     continue it, or omit session to start a new conversation with {named}"
                )));
            }
            return Ok(agent);
        }
        let names = self.names().join(", ");
        match named {
            Some(named) => self.find(named).ok_or_else(|| {
                ToolError::invalid_arguments(format!(
                    "agent {:?} is not offered in this session; choose one of: {names}",
                    bounded(named, 40)
                ))
            }),
            None => self
                .default
                .map(|index| &self.agents[index])
                .ok_or_else(|| {
                    ToolError::invalid_arguments(format!(
                        "name the agent: the user prefers none of the agents this session \
                         offers ([agent] prefer); choose one of: {names}"
                    ))
                }),
        }
    }

    /// The error for `option` given to `agent`, which does not take it.
    fn not_taken(&self, agent: &Offered, option: &str) -> ToolError {
        let takers: Vec<&str> = self
            .agents
            .iter()
            .filter(|other| other.accepts.takes(option))
            .map(|other| other.name.as_str())
            .collect();
        let reason = if option == "session" {
            " (it starts a new conversation on every call)"
        } else {
            ""
        };
        ToolError::invalid_arguments(if takers.is_empty() {
            format!(
                "{} does not take {option}{reason}, and no agent in this session does; omit it",
                agent.name
            )
        } else {
            format!(
                "{} does not take {option}{reason}; these agents do: {}. Omit {option}, or call \
                 one of them",
                agent.name,
                takers.join(", ")
            )
        })
    }

    /// The `effort` values the schema allows: SCV's own, then any other that
    /// an offered agent's ACP server listed or the user configured for it,
    /// for all tasks or for hard ones.
    fn efforts(&self) -> Vec<&str> {
        let mut efforts: Vec<&str> = AGENT_EFFORTS.to_vec();
        for agent in self.agents.iter().filter(|agent| agent.accepts.effort) {
            let listed = agent
                .offered
                .as_ref()
                .and_then(|offered| offered.effort.as_ref())
                .map(|choice| choice.shown())
                .unwrap_or_default();
            let configured = [&agent.defaults.effort, &agent.defaults.hard_task_effort]
                .into_iter()
                .filter_map(Option::as_deref);
            for effort in listed.into_iter().chain(configured) {
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
            }
        }
        efforts
    }

    /// The `agent` argument's description: how the agent is chosen, then
    /// one line per agent.
    fn agent_description(&self) -> String {
        let mut text = match self.default {
            Some(index) => format!(
                "Which agent runs the task. Defaults to {}, the first of the user's preferred \
                 agents offered here; a session handle keeps its conversation's agent.",
                self.agents[index].name
            ),
            None => "Which agent runs the task; a session handle keeps its conversation's agent."
                .to_owned(),
        };
        for agent in &self.agents {
            text.push_str("\n- ");
            text.push_str(&choice::entry(agent));
        }
        text
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        let continuing: Vec<&str> = self
            .agents
            .iter()
            .filter(|agent| agent.accepts.session)
            .map(|agent| agent.name.as_str())
            .collect();
        let mut properties = Map::new();
        properties.insert(
            "agent".into(),
            json!({
                "type":"string",
                "enum":self.names(),
                "description":self.agent_description()
            }),
        );
        properties.insert("prompt".into(), json!({"type":"string"}));
        properties.insert(
            "cwd".into(),
            json!({
                "type":"string",
                "description":"Directory inside the workspace to run in, such as a project directory (\"scv\"). \
                    The agent loads that directory's AGENTS.md or CLAUDE.md and its project skills. \
                    Defaults to the workspace root."
            }),
        );
        if let Some(first) = continuing.first() {
            properties.insert(
                "session".into(),
                json!({
                    "type":"string",
                    "description":format!(
                        "The `session` handle an earlier agent call returned, such as \"{first}-1\", \
                         which names its agent. Pass it to continue that conversation: the agent \
                         keeps its context, in the same cwd. Omit it to start a new conversation \
                         for unrelated work."
                    )
                }),
            );
            properties.insert(
                "on_busy".into(),
                json!({
                    "type":"string",
                    "enum":["queue","wait","steer","fail"],
                    "description":"What a call continuing a conversation does while it runs a turn \
                        or other prompts wait for it: queue (returns a background job that runs \
                        after them), wait (this call waits for them), steer (adds the prompt to \
                        the running turn when the agent can take it), or fail. Omit it to use \
                        the user's setting."
                }),
            );
        }
        if self.agents.iter().any(|agent| agent.accepts.model) {
            properties.insert(
                "model".into(),
                json!({
                    "type":"string",
                    "description":"Model for the chosen agent, in the form its line under agent \
                        names. Set it when the user asks for one; omit it to use the user's default \
                        that line gives, or else the agent's own."
                }),
            );
        }
        if self.agents.iter().any(|agent| agent.accepts.effort) {
            properties.insert(
                "effort".into(),
                json!({
                    "type":"string",
                    "enum":self.efforts(),
                    "description":"Reasoning effort. Set it when the user asks for one, or to \
                        the hard-task effort the chosen agent's line gives when the task is hard; \
                        omit it to use the user's default that line gives, or else the agent's own."
                }),
            );
        }
        if self.worktrees.is_some() {
            properties.insert(
                "worktree".into(),
                json!({
                    "type":"boolean",
                    "description":"Run in a new git worktree of the repository holding cwd, on a \
                        new branch from its current HEAD, so the agent's changes stay apart from \
                        the repository's own checkout and from other jobs. The result names the \
                        worktree and branch. Only for a new conversation."
                }),
            );
        }
        properties.insert("timeout_seconds".into(), timeout_schema(self.timeouts));
        if self.no_timeout {
            properties.insert(
                "no_timeout".into(),
                json!({
                    "type":"boolean",
                    "description":"Run with no time limit, for a background call (background: true) \
                        that must run until it finishes however long it takes. Not together with \
                        timeout_seconds. Starting the agent and each approval it asks for stay \
                        time-limited."
                }),
            );
        }
        let required = if self.default.is_some() {
            json!(["prompt"])
        } else {
            json!(["agent", "prompt"])
        };
        let mut description = String::from(
            "Hands a task to another coding agent, which runs it with its own model and tools \
             (not sandboxed). Delegate substantial work here rather than doing it step by step \
             with bash: research and web lookups, multi-file coding, and running tools, builds, \
             and tests. Give it a self-contained brief, since it does not see this \
             conversation, and set cwd to the project the work is in so it follows that \
             project's instructions and skills.",
        );
        if !continuing.is_empty() {
            description.push_str(
                " A result's `session` handle continues that conversation: pass it back to \
                 follow up on the same work (answers, fixes, next steps) instead of repeating \
                 the context.",
            );
        }
        ToolSpec {
            name: AGENT_TOOL.into(),
            description,
            parameters: json!({
                "type":"object",
                "properties":properties,
                "required":required,
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        self.risk_for(arguments, false)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        self.approval_summary_for(arguments, false)
    }

    async fn execute(
        &self,
        mut arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.route(&arguments)?;
        let worktree = self.prepare_worktree(&mut arguments, &context).await?;
        let (agent, arguments) = self.route(&arguments)?;
        self.check_no_timeout(&arguments, context.background_job)?;
        if let Some(worktree) = worktree {
            let output = self.run(agent, arguments, context).await?;
            return Ok(with_worktree(output, &worktree));
        }
        // Without background jobs (agent.max_background = 0) nothing can
        // queue, so a busy conversation's call steers, waits, or fails.
        // With them, the background wrapper decides before this runs.
        if agent.backend.busy(&arguments)? {
            let behavior = match agent.on_busy(&arguments)? {
                BusyBehavior::Steer => {
                    if let Some(output) = agent
                        .backend
                        .steer(arguments.clone(), context.clone())
                        .await?
                    {
                        return Ok(output);
                    }
                    agent.busy.fallback()
                }
                behavior => behavior,
            };
            if behavior != BusyBehavior::Wait {
                let handle = parse_args::<AgentArgs>(&arguments)?.session;
                return Err(ToolError::failed(format!(
                    "session busy: conversation {} is still running a turn",
                    handle.as_deref().unwrap_or_default()
                )));
            }
            agent
                .backend
                .wait_idle(&arguments, &context.cancellation)
                .await?;
        }
        self.run(agent, arguments, context).await
    }
}

#[cfg(test)]
impl AgentTool {
    /// A dispatcher offering `backend` as `name`, its default, beside
    /// `others`, agents that fail if called: for tests of one backend's
    /// results as the model sees them.
    pub(crate) fn beside(
        name: &str,
        backend: Arc<dyn Backend>,
        accepts: Accepts,
        others: &[&str],
    ) -> Self {
        struct Unused;
        #[async_trait]
        impl Backend for Unused {
            fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
                Ok(ToolRisk::Delegate)
            }

            fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
                Ok(String::new())
            }

            async fn execute(
                &self,
                _arguments: Value,
                _context: ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                Err(ToolError::failed("this test calls another agent"))
            }
        }
        let offered = |name: &str, backend: Arc<dyn Backend>, accepts| Offered {
            name: name.to_owned(),
            backend,
            accepts,
            model_hint: String::new(),
            offered: None,
            use_for: None,
            defaults: AgentDefaults::default(),
            holds_settings: true,
            busy: BusyConfig::default(),
        };
        let mut agents = vec![offered(name, backend, accepts)];
        agents.extend(
            others
                .iter()
                .map(|other| offered(other, Arc::new(Unused), Accepts::default())),
        );
        let timeouts = Timeouts {
            default: std::time::Duration::from_secs(3600),
            max: std::time::Duration::from_secs(14400),
        };
        Self::new(agents, &[name.to_owned()], timeouts)
    }
}

/// `output` with the worktree the call ran in added to its result.
pub(crate) fn with_worktree(
    mut output: ToolOutput,
    worktree: &scv_protocol::WorktreeRef,
) -> ToolOutput {
    if let Ok(Value::Object(mut value)) = serde_json::from_str::<Value>(&output.content) {
        value.insert(
            "worktree".into(),
            serde_json::to_value(worktree).unwrap_or(Value::Null),
        );
        output.content = Value::Object(value).to_string();
    }
    output
}

/// The agents a session's `agent` tool offers, sorted: the values its
/// schema lists. Empty when the session offers none.
pub fn offered_agents(tools: &ToolRegistry) -> Vec<String> {
    tools
        .get(AGENT_TOOL)
        .and_then(|tool| {
            tool.spec()
                .parameters
                .pointer("/properties/agent/enum")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
