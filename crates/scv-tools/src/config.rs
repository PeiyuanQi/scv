//! What a session's built-in tools are configured with: limits, the
//! delegation context, and each delegated agent's adapter settings.

use std::{collections::HashMap, ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    builtin::chat_attach,
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
    /// Agent tools are offered only below this delegation depth.
    pub max_delegation_depth: u32,
    /// How many delegated conversations a session remembers, and for how long.
    pub conversations: ConversationLimits,
    /// Records delegated runs for listing and cleanup; `None` runs them untracked.
    pub delegation: Option<DelegationContext>,
    /// Background jobs an agent call may start at once (`background: true`);
    /// 0 turns background calls and `agent_wait` / `agent_status` /
    /// `agent_cancel` off.
    pub max_background: usize,
    /// The session's background job store, when the server reports finished
    /// jobs; otherwise the registry makes its own.
    pub background: Option<Arc<background::BackgroundJobs>>,
    /// Offers `chat_attach` when the session answers on a chat channel.
    pub chat_attach: Option<chat_attach::ChatAttachConfig>,
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
            conversations: ConversationLimits {
                max: 8,
                idle: Duration::from_secs(86400),
            },
            delegation: None,
            max_background: 2,
            background: None,
            chat_attach: None,
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
    /// use_for`), added to its tool description.
    pub use_for: Option<String>,
    /// Default model to pass when the work matches `use_for` (or on every
    /// call to this agent, when `use_for` is unset).
    pub model: Option<String>,
    /// Default effort to pass the same way as `model`.
    pub effort: Option<String>,
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
}

pub type SkillMap = HashMap<String, PathBuf>;
