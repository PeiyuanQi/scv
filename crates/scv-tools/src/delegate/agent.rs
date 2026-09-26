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

use std::sync::Arc;

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolRisk, ToolSpec};
use serde_json::{Map, Value, json};

use crate::{
    args::{Timeouts, bounded, parse_args, timeout_schema},
    delegate::{
        choice, conversation,
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
    /// The user's `[agents.<name>] use_for` note.
    pub(crate) use_for: Option<String>,
    /// `[agents.<name>] model`, passed when the work matches `use_for`.
    pub(crate) model: Option<String>,
    /// `[agents.<name>] effort`, passed the same way as `model`.
    pub(crate) effort: Option<String>,
}

/// The `agent` tool.
pub(crate) struct AgentTool {
    /// Sorted by name.
    agents: Vec<Offered>,
    /// The first offered agent in `[agent] prefer`.
    default: Option<usize>,
    timeouts: Timeouts,
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
        }
    }

    fn find(&self, name: &str) -> Option<&Offered> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    fn names(&self) -> Vec<&str> {
        self.agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect()
    }

    /// The agent that runs a call, once the call's options are checked
    /// against what that agent takes. Nothing has launched when this fails.
    pub(crate) fn route(&self, arguments: &Value) -> Result<&Offered, ToolError> {
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
        Ok(agent)
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
        }
        if self.agents.iter().any(|agent| agent.accepts.model) {
            properties.insert(
                "model".into(),
                json!({
                    "type":"string",
                    "description":"Model for the chosen agent, in the form its line under agent \
                        names. Set when the user asks, or when the work matches a configured use_for \
                        default; omit to use the agent's configured default."
                }),
            );
        }
        if self.agents.iter().any(|agent| agent.accepts.effort) {
            properties.insert(
                "effort".into(),
                json!({
                    "type":"string",
                    "enum":AGENT_EFFORTS,
                    "description":"Reasoning effort. Set when the user asks, or when the work \
                        matches a configured use_for default; omit to use the agent's configured \
                        default."
                }),
            );
        }
        properties.insert("timeout_seconds".into(), timeout_schema(self.timeouts));
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
        self.route(arguments)?.backend.risk(arguments)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let agent = self.route(arguments)?;
        Ok(format!(
            "agent {}: {}",
            agent.name,
            agent.backend.approval_summary(arguments)?
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let agent = self.route(&arguments)?;
        let result = agent.backend.execute(arguments, context).await;
        let others: Vec<&str> = self
            .names()
            .into_iter()
            .filter(|name| *name != agent.name)
            .collect();
        choice::settle(result, &others)
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
            use_for: None,
            model: None,
            effort: None,
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
