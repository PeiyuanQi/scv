//! The configuration schema: every table of `config.toml`, with defaults.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use scv_channels::state::AccountSettings;
use scv_client::Secret;
use scv_core::ContextConfig;
use scv_provider_openai::ProviderLimits;
use scv_tools::BusyBehavior;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub provider: ProviderConfig,
    /// Named provider profiles. When non-empty, `provider.active` selects one.
    pub(crate) providers: HashMap<String, ProviderConfig>,
    pub(crate) provider_active: Option<String>,
    pub(crate) agent: AgentConfig,
    pub(crate) session: SessionConfig,
    pub(crate) context: ContextConfigFile,
    /// `[policy]`: what runs without a person, for SCV's own tools and every
    /// delegated agent.
    pub(crate) policy: PolicyConfig,
    pub(crate) tools: ToolConfig,
    pub(crate) protocol: ProtocolConfig,
    pub(crate) tui: TuiConfig,
    pub update: UpdateConfig,
    pub(crate) notify: NotifyConfig,
    pub(crate) history: HistoryConfig,
    pub(crate) provider_limits: ProviderLimitsFile,
    pub(crate) skills: SkillsConfig,
    pub(crate) agents: AgentsConfig,
    pub(crate) web: WebConfig,
    /// `[channels.<channel>.<account>]`: each chat account's settings. SCV's
    /// channel store reads and edits them in the instance's `config.toml`;
    /// here they are only validated.
    pub(crate) channels: BTreeMap<String, BTreeMap<String, AccountSettings>>,
    /// The process-owned root: see [`Layout`] for what it holds.
    #[serde(skip)]
    pub(crate) instance_home: PathBuf,
    /// The deprecated keys the layers set, each read as its replacement.
    #[serde(skip)]
    pub(crate) deprecated: Vec<super::Deprecated>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub(crate) active: Option<String>,
    pub kind: String,
    pub wire_api: String,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<Secret>,
    pub api_key_env: Option<String>,
    pub timeout_seconds: u64,
    /// Extra request headers; their values may carry credentials.
    pub headers: HashMap<String, Secret>,
    /// Show images users attach to the model as image input. Turn it off
    /// for a model without vision; SCV also stops for the session after the
    /// provider rejects an image.
    pub(crate) image_input: bool,
    /// The reasoning effort SCV's own requests ask this provider's model for
    /// (Responses `reasoning.effort`), such as `high`. Unset, requests name
    /// none and the model uses its default.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Optional Cargo registry index URL used by `scv update`.
    pub index_url: Option<String>,
}

/// Where SCV sends notices nobody asked for: an update started from a
/// terminal, a rollback, a restart after a crash, or a disconnected account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NotifyConfig {
    /// Accounts as `<channel>:<account>`, such as `feishu:default`. A notice
    /// goes to the owner of the first one that is connected, on that one
    /// account only. Empty: the chat the owner last wrote from.
    pub(crate) owner: Vec<String>,
}

/// The chat log of the owner's direct chats and the files kept from them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct HistoryConfig {
    /// Minutes without a message after which the next one starts a new
    /// episode, which a new session no longer reloads.
    pub(crate) episode_gap_minutes: u64,
    /// The share of each disk holding the chat log, chat media, or kept
    /// files that must stay free; below it the owner is told and SCV stops
    /// saving new files from chat. 0 turns the check off.
    pub(crate) min_free_percent: u8,
    /// Where files the owner asks to keep go, under
    /// `<channel>/<account>/<conversation>/files/`; the history directory
    /// when unset.
    pub(crate) archive_dir: Option<PathBuf>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            episode_gap_minutes: 120,
            min_free_percent: 20,
            archive_dir: None,
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            active: None,
            kind: "openai-compatible".into(),
            wire_api: "responses".into(),
            model: "gpt-4.1-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            api_key_env: Some("OPENAI_API_KEY".into()),
            timeout_seconds: 600,
            headers: HashMap::new(),
            image_input: true,
            reasoning_effort: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AgentConfig {
    pub(crate) max_steps: usize,
    pub(crate) system_prompt: String,
    /// The `agent` tool is offered only while this SCV's own delegation depth
    /// is below this, so delegation chains stay bounded. 0 disables it.
    pub(crate) max_delegation_depth: u32,
    /// Delegated conversations a session remembers; starting another forgets
    /// the least recently used idle one.
    pub(crate) max_conversations: usize,
    /// A delegated conversation unused this long is forgotten.
    pub(crate) conversation_idle_seconds: u64,
    /// Background agent jobs (`background: true`) a session may run at once;
    /// 0 turns background delegation off.
    pub(crate) max_background: usize,
    pub(crate) on_busy: BusyBehavior,
    pub(crate) steer_fallback: BusyBehavior,
    pub(crate) max_queued_turns: usize,
    /// Agents the user prefers, in order (such as `["codex", "claude"]`);
    /// the system prompt names the offered ones, and the first of them runs
    /// an `agent` call that names none. Empty states no preference.
    pub(crate) prefer: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 128,
            max_delegation_depth: 2,
            max_conversations: 8,
            conversation_idle_seconds: 86400,
            // The main agent hands most work to background jobs and stays
            // available, so a few may run at once.
            max_background: 4,
            on_busy: BusyBehavior::Queue,
            steer_fallback: BusyBehavior::Queue,
            max_queued_turns: 4,
            prefer: Vec::new(),
            system_prompt: "You are SCV, a concise and careful agent. Use tools to inspect, change, and verify.".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SessionConfig {
    pub(crate) max_history_bytes: usize,
    pub(crate) max_messages: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_history_bytes: 16 * 1024 * 1024,
            max_messages: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ContextConfigFile {
    pub(crate) max_tokens: usize,
    pub(crate) reserve_output_tokens: usize,
    pub(crate) safety_margin_tokens: usize,
    pub(crate) bytes_per_token: usize,
    pub(crate) summary_max_chars: usize,
}

impl Default for ContextConfigFile {
    fn default() -> Self {
        let value = ContextConfig::default();
        Self {
            max_tokens: value.max_tokens,
            reserve_output_tokens: value.reserve_output_tokens,
            safety_margin_tokens: value.safety_margin_tokens,
            bytes_per_token: value.bytes_per_token,
            summary_max_chars: value.summary_max_chars,
        }
    }
}

impl From<&ContextConfigFile> for ContextConfig {
    fn from(value: &ContextConfigFile) -> Self {
        Self {
            max_tokens: value.max_tokens,
            reserve_output_tokens: value.reserve_output_tokens,
            safety_margin_tokens: value.safety_margin_tokens,
            bytes_per_token: value.bytes_per_token,
            summary_max_chars: value.summary_max_chars,
        }
    }
}

/// What runs without a person: `[policy] mode`, or one agent's
/// `[agents.<name>] mode`. Strictness runs `auto` < `on-risk` < `always` <
/// `read-only`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyMode {
    /// SCV's own tools and the requests relayed to it are approved, `agent`
    /// calls included, and every delegated agent starts in its full mode.
    #[default]
    Auto,
    /// Read-only calls are approved and the rest ask; delegated agents start
    /// with no switches, so their own configuration decides.
    OnRisk,
    /// Every call asks; delegated agents start with no switches.
    Always,
    /// Everything but read-only calls is denied, `agent` calls included.
    ReadOnly,
}

impl PolicyMode {
    /// The mode as configuration writes it, such as `on-risk`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OnRisk => "on-risk",
            Self::Always => "always",
            Self::ReadOnly => "read-only",
        }
    }
}

impl std::fmt::Display for PolicyMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// `[policy]`: one policy for SCV's own tools, the requests relayed to it,
/// and how delegated agents start. The gates in `approval.rs` apply on top
/// of every mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PolicyConfig {
    pub(crate) mode: PolicyMode,
    /// Versions of one crate that may reach crates.io in any 24 hours before
    /// publishing asks the owner. At 20 it is crates.io's own limit.
    pub(crate) publish_crate_versions_per_day: u64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            mode: PolicyMode::Auto,
            publish_crate_versions_per_day: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ToolConfig {
    /// Let background `agent` calls set `no_timeout` and run with no main
    /// deadline. Off unless the owner turns it on.
    pub(crate) allow_no_timeout: bool,
    /// `bash` timeout when a call does not choose one.
    pub(crate) command_timeout_seconds: u64,
    /// Native-agent timeout when a call does not choose one.
    pub(crate) agent_timeout_seconds: u64,
    /// The longest timeout a single tool call may request.
    pub(crate) max_timeout_seconds: u64,
    pub(crate) output_limit_bytes: usize,
    pub(crate) max_read_bytes: usize,
    pub(crate) max_write_bytes: usize,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            allow_no_timeout: false,
            command_timeout_seconds: 600,
            agent_timeout_seconds: 3600,
            max_timeout_seconds: 14400,
            output_limit_bytes: 64 * 1024,
            max_read_bytes: 256 * 1024,
            max_write_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProtocolConfig {
    pub(crate) max_client_frame_bytes: usize,
    pub(crate) max_server_frame_bytes: usize,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            max_client_frame_bytes: 1024 * 1024,
            max_server_frame_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TuiConfig {
    pub(crate) max_transcript_bytes: usize,
    pub(crate) max_transcript_items: usize,
    pub(crate) max_prompt_history_bytes: usize,
    pub(crate) max_prompt_history_items: usize,
    /// The terminal UI's key bindings: action names to key specifications
    /// such as `alt+]` or `ctrl+t`, sent to the TUI with `session.started`,
    /// which checks the actions and keys and keeps its default for any it
    /// cannot use.
    pub(crate) keys: BTreeMap<String, String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            max_transcript_bytes: 8 * 1024 * 1024,
            max_transcript_items: 10_000,
            max_prompt_history_bytes: 1024 * 1024,
            max_prompt_history_items: 200,
            keys: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderLimitsFile {
    pub(crate) max_sse_event_bytes: usize,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_assistant_bytes: usize,
    pub(crate) max_tool_calls: usize,
    pub(crate) max_tool_arguments_bytes: usize,
    pub(crate) max_retries: usize,
}

impl Default for ProviderLimitsFile {
    fn default() -> Self {
        let value = ProviderLimits::default();
        Self {
            max_sse_event_bytes: value.max_sse_event_bytes,
            max_response_bytes: value.max_response_bytes,
            max_assistant_bytes: value.max_assistant_bytes,
            max_tool_calls: value.max_tool_calls,
            max_tool_arguments_bytes: value.max_tool_arguments_bytes,
            max_retries: value.max_retries,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SkillsConfig {
    pub(crate) user_dir: PathBuf,
    pub(crate) project_dir: PathBuf,
    /// List the agent skills (`.agents/skills`, `.claude/skills`) of the
    /// workspace and its immediate child projects in tool-enabled sessions.
    pub(crate) scan_projects: bool,
    pub(crate) max_skills: usize,
    pub(crate) max_skill_bytes: usize,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            user_dir: PathBuf::from("~/.scv/skills"),
            project_dir: PathBuf::from(".scv/skills"),
            scan_projects: true,
            max_skills: 128,
            max_skill_bytes: 256 * 1024,
        }
    }
}

/// Where `web_search` results come from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WebSearchMode {
    Off,
    /// The provider endpoint's hosted Responses `web_search` tool.
    Provider,
    Searxng,
    Brave,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WebConfig {
    /// Offer `web_fetch` (and search, when configured) to tool-enabled sessions.
    pub(crate) enabled: bool,
    pub(crate) fetch_max_bytes: usize,
    pub(crate) fetch_timeout_seconds: u64,
    pub(crate) max_redirects: usize,
    /// HTTPS hosts `web_fetch` may read without approval.
    pub(crate) auto_approve_domains: Vec<String>,
    /// Let `web_fetch` reach loopback, private, and link-local addresses.
    pub(crate) allow_private_addresses: bool,
    pub(crate) search: WebSearchMode,
    pub(crate) searxng_url: Option<String>,
    pub(crate) brave_url: String,
    pub(crate) brave_api_key: Option<Secret>,
    pub(crate) brave_api_key_env: Option<String>,
    pub(crate) max_search_results: usize,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fetch_max_bytes: 2 * 1024 * 1024,
            fetch_timeout_seconds: 30,
            max_redirects: 5,
            auto_approve_domains: [
                "docs.rs",
                "crates.io",
                "doc.rust-lang.org",
                "docs.python.org",
                "pypi.org",
                "developer.mozilla.org",
            ]
            .map(String::from)
            .to_vec(),
            allow_private_addresses: false,
            search: WebSearchMode::Off,
            searxng_url: None,
            brave_url: "https://api.search.brave.com/res/v1/web/search".into(),
            brave_api_key: None,
            brave_api_key_env: Some("BRAVE_SEARCH_API_KEY".into()),
            max_search_results: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AdapterConfig {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    /// This agent's policy mode instead of `[policy] mode`: under `auto` it
    /// starts with its CLI's own switches for unprompted, unsandboxed work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<PolicyMode>,
    /// Placed immediately before the prompt (`grok -p <prompt>`).
    pub(crate) prompt_args: Vec<String>,
    /// Appended when a call selects a model; `{model}` is substituted.
    pub(crate) model_args: Vec<String>,
    /// Appended when a call selects an effort; `{effort}` is substituted.
    pub(crate) effort_args: Vec<String>,
    /// How SCV talks to the agent: its ACP server or one process per turn.
    pub(crate) transport: AgentTransport,
    /// When to choose this agent, in the user's words; added to its tool
    /// description so the model can pick between agents.
    pub(crate) use_for: Option<String>,
    /// The model an `agent` call to this agent runs on when it names none.
    pub(crate) model: Option<String>,
    /// The effort an `agent` call runs at when it names none.
    pub(crate) effort: Option<String>,
    /// The effort the main agent is told to pass for a hard task.
    pub(crate) hard_task_effort: Option<String>,
    pub(crate) on_busy: Option<BusyBehavior>,
    pub(crate) steer_fallback: Option<BusyBehavior>,
    pub(crate) max_queued_turns: Option<usize>,
}

impl AdapterConfig {
    /// The user's model and efforts for this agent, as the tools take them.
    pub(crate) fn defaults(&self) -> scv_tools::AgentDefaults {
        scv_tools::AgentDefaults {
            model: self.model.clone(),
            effort: self.effort.clone(),
            hard_task_effort: self.hard_task_effort.clone(),
        }
    }

    /// The ACP server this agent runs on, when `descriptor` has one and the
    /// transport allows it: `acp`, or `auto` with the built-in command. A
    /// custom `command` points SCV at a specific CLI, which the ACP server
    /// would not run.
    pub(crate) fn acp_launch(
        &self,
        descriptor: &scv_tools::adapters::AdapterDescriptor,
    ) -> Option<scv_tools::adapters::AcpLaunch> {
        descriptor.acp.filter(|_| match self.transport {
            AgentTransport::Acp => true,
            AgentTransport::Resume => false,
            // An agent with no one-prompt mode has nothing else to run.
            AgentTransport::Auto => self.command == descriptor.command || !descriptor.cli_turns,
        })
    }
}

/// How SCV talks to a delegated agent that has an ACP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentTransport {
    /// The agent's ACP server when it is installed, else one process per turn.
    #[default]
    Auto,
    /// Only its ACP server; the agent is not offered while it is missing.
    Acp,
    /// One CLI process per turn, continued through the CLI's own resume.
    Resume,
}

/// `[agents.<name>]` for every adapter in [`scv_tools::adapters::ADAPTERS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct AgentsConfig(pub(crate) BTreeMap<String, AdapterConfig>);

impl Default for AgentsConfig {
    fn default() -> Self {
        let strings = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
        Self(
            scv_tools::adapters::ADAPTERS
                .iter()
                .map(|adapter| {
                    (
                        adapter.name.to_owned(),
                        AdapterConfig {
                            command: adapter.command.into(),
                            args: strings(adapter.args),
                            mode: None,
                            prompt_args: strings(adapter.prompt_args),
                            model_args: strings(adapter.model_args),
                            effort_args: strings(adapter.effort_args),
                            transport: AgentTransport::Auto,
                            use_for: None,
                            model: None,
                            effort: None,
                            hard_task_effort: None,
                            on_busy: None,
                            steer_fallback: None,
                            max_queued_turns: None,
                        },
                    )
                })
                .collect(),
        )
    }
}

/// What the command line, or a client's `session.start`, changes on top of
/// the configuration files.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// `--policy` (or its alias `--approval-policy`): `[policy] mode`.
    pub mode: Option<PolicyMode>,
    pub no_tools: bool,
    /// An explicit configuration layer (`--config`, or `SCV_CONFIG` as the
    /// process received it), applied after the user and project files.
    pub config_file: Option<PathBuf>,
}
