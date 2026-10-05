//! What a session's built-in tools are configured with: limits, the
//! delegation context, and each delegated agent's adapter settings.

use std::{collections::HashMap, ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    builtin::{chat_attach, chat_history},
    delegate::{
        adapters::{OutputFormat, Resume, Transport},
        background,
        conversation::ConversationLimits,
        records::DelegationRegistry,
    },
};

/// Limits and shared state for one session's tools.
#[derive(Debug, Clone)]
pub struct ToolsConfig {
    /// Default `bash` timeout when a call does not choose one.
    pub command_timeout: Duration,
    /// Default native-agent timeout when a call does not choose one.
    pub agent_timeout: Duration,
    /// The longest timeout any single call may request.
    pub max_timeout: Duration,
    pub output_limit_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    /// The `agent` tool is offered only below this delegation depth.
    pub max_delegation_depth: u32,
    /// Agents the user prefers, in order (`[agent] prefer`); the first one
    /// offered runs an `agent` call that names none.
    pub prefer: Vec<String>,
    /// How many delegated conversations a session remembers, and for how long.
    pub conversations: ConversationLimits,
    /// Records delegated runs for listing and cleanup; `None` runs them untracked.
    pub delegation: Option<DelegationContext>,
    /// Background jobs `agent` calls may run at once (`background: true`);
    /// 0 turns background calls and `agent_wait` / `agent_status` /
    /// `agent_cancel` off.
    pub max_background: usize,
    /// The session's background job store, when the server reports finished
    /// jobs; otherwise the registry makes its own.
    pub background: Option<Arc<background::BackgroundJobs>>,
    /// Offers `chat_attach` when the session answers on a chat channel.
    pub chat_attach: Option<chat_attach::ChatAttachConfig>,
    /// Offers `chat_history` and `chat_keep` when the session answers a
    /// conversation that has a chat log.
    pub chat_history: Option<chat_history::ChatHistoryConfig>,
    /// Refuse a model an ACP agent's saved list lacks before the call starts.
    /// `scv agents check` turns it off so the agent's own list decides.
    pub precheck_agent_models: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(600),
            agent_timeout: Duration::from_secs(3600),
            max_timeout: Duration::from_secs(14400),
            output_limit_bytes: 64 * 1024,
            max_read_bytes: 256 * 1024,
            max_write_bytes: 1024 * 1024,
            max_delegation_depth: 2,
            prefer: Vec::new(),
            conversations: ConversationLimits {
                max: 8,
                idle: Duration::from_secs(86400),
            },
            delegation: None,
            max_background: 2,
            background: None,
            chat_attach: None,
            chat_history: None,
            precheck_agent_models: true,
        }
    }
}

/// What an `agent` call does when the conversation it continues is running
/// a turn, or has prompts queued (`agent.on_busy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusyBehavior {
    /// Queue the prompt as a background job that runs after the turns ahead.
    #[default]
    Queue,
    /// Keep the call waiting until the turns ahead end, then run it.
    Wait,
    /// Add the prompt to the running turn when the agent's ACP server can be
    /// steered, otherwise apply [`BusyConfig::steer_fallback`].
    Steer,
    /// Refuse the call as busy.
    Fail,
}

impl BusyBehavior {
    /// The behavior `value` names, or an error saying what may be named.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "queue" => Ok(Self::Queue),
            "wait" => Ok(Self::Wait),
            "steer" => Ok(Self::Steer),
            "fail" => Ok(Self::Fail),
            other => Err(format!(
                "invalid busy behavior {other:?}; use queue, wait, steer, or fail"
            )),
        }
    }
}

impl<'de> serde::Deserialize<'de> for BusyBehavior {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for BusyBehavior {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            Self::Queue => "queue",
            Self::Wait => "wait",
            Self::Steer => "steer",
            Self::Fail => "fail",
        })
    }
}

/// How one agent's busy conversations are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyConfig {
    /// What a call does by default (`on_busy`).
    pub behavior: BusyBehavior,
    /// What a steer does when the running turn cannot take the prompt
    /// (`steer_fallback`): queue, wait, or fail. `Steer` means queue.
    pub steer_fallback: BusyBehavior,
    /// Prompts that may wait per conversation (`max_queued_turns`).
    pub max_queued_turns: usize,
}

impl BusyConfig {
    /// What a steer that could not steer does instead.
    pub(crate) fn fallback(self) -> BusyBehavior {
        match self.steer_fallback {
            BusyBehavior::Steer => BusyBehavior::Queue,
            other => other,
        }
    }
}

impl Default for BusyConfig {
    fn default() -> Self {
        Self {
            behavior: BusyBehavior::Queue,
            steer_fallback: BusyBehavior::Queue,
            max_queued_turns: 4,
        }
    }
}

/// The registry and parent session that delegated runs are recorded under.
#[derive(Debug, Clone)]
pub struct DelegationContext {
    pub registry: Arc<DelegationRegistry>,
    pub session: String,
    /// Delegation depth the session's client declared (0 for a direct
    /// client). Runs count from the larger of this and the process's own.
    pub depth: u32,
}

impl DelegationContext {
    /// The depth delegated runs of this session start from.
    pub(crate) fn owner_depth(&self) -> u32 {
        self.registry.depth().max(self.depth)
    }
}

#[derive(Debug, Clone)]
pub struct AgentAdapterConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Arguments placed immediately before the prompt, for CLIs that take the
    /// prompt as a flag value.
    pub prompt_args: Vec<String>,
    /// The CLI's own full-autonomy arguments, placed after `args`, when the
    /// user configured `permissions = "full"`; the approval summary says so.
    pub full_permission_args: Option<Vec<String>>,
    /// Arguments appended for a per-call model; `{model}` is substituted.
    /// Empty means the adapter does not offer model selection.
    pub model_args: Vec<String>,
    /// Arguments appended for a per-call effort; `{effort}` is substituted.
    /// Empty means the adapter does not offer effort selection.
    pub effort_args: Vec<String>,
    /// Describes the `model` argument for the calling model.
    pub model_hint: String,
    /// Environment for the nested process. SCV supplies an instance-private home.
    pub environment: Vec<(OsString, OsString)>,
    /// Per-user install directories searched when `command` is not on `PATH`.
    pub search_dirs: Vec<PathBuf>,
    /// What the CLI prints, and so how its reply is read.
    pub output: OutputFormat,
    /// How a conversation with the CLI is continued, if it can be.
    pub resume: Resume,
    /// SCV's private home for this agent, for files SCV hands the CLI.
    pub home: Option<PathBuf>,
    /// How SCV talks to the agent.
    pub transport: Transport,
    /// The agent's ACP server, when `[agents.<name>] transport` allows it and
    /// the adapter table has one.
    pub acp: Option<AcpAgentLaunch>,
    /// The user's note on when to choose this agent (`[agents.<name>]
    /// use_for`), added to its line in the `agent` tool's description.
    pub use_for: Option<String>,
    /// The user's model and effort for this agent.
    pub defaults: AgentDefaults,
    /// Where SCV keeps the model and effort values this agent's ACP server
    /// offers (`state/agent-options/<name>.json`); `None` keeps none.
    pub options_file: Option<PathBuf>,
    /// How a call to one of this agent's busy conversations is handled:
    /// `[agents.<name>]` over `[agent]`.
    pub busy: BusyConfig,
}

/// The user's model and effort for one agent: `[agents.<name>] model`,
/// `effort`, and `hard_task_effort`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDefaults {
    /// The model an `agent` call that names none runs on.
    pub model: Option<String>,
    /// The effort an `agent` call that names none runs at.
    pub effort: Option<String>,
    /// The effort the calling model is told to pass for a hard task. SCV
    /// never applies it by itself, since only the caller can tell a task is
    /// hard.
    pub hard_task_effort: Option<String>,
}

/// An agent's Agent Client Protocol server, resolved from its adapter-table
/// entry and `[agents.<name>] transport`.
#[derive(Debug, Clone)]
pub struct AcpAgentLaunch {
    pub command: String,
    /// Arguments with the `permissions = "full"` switches already applied.
    pub args: Vec<String>,
    /// The ACP session mode that grants full permissions, selected in every
    /// new session when `permissions = "full"`.
    pub full_mode: Option<String>,
    /// Extra environment for the ACP server, such as permission settings the
    /// server reads only from its environment.
    pub environment: Vec<(OsString, OsString)>,
    /// `transport = "acp"`: never fall back to one CLI process per turn, so
    /// the agent is not offered while its ACP server is missing.
    pub required: bool,
    /// The server takes `model` and `effort` as session config options even
    /// where the CLI takes neither as an argument (DeepSeek Harness).
    pub session_options: bool,
}

/// Where a skill's text comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skill {
    /// A `SKILL.md` file, read when loaded and only from inside one of the
    /// configured skill roots.
    File(PathBuf),
    /// Built into SCV.
    Builtin(&'static str),
}

/// A session's skills by name.
pub type SkillMap = HashMap<String, Skill>;
