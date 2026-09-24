//! The configuration schema: every table of `config.toml`, with defaults.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use scv_channels::state::AccountSettings;
use scv_core::ContextConfig;
use scv_provider_openai::ProviderLimits;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub provider: ProviderConfig,
    /// Named provider profiles. When non-empty, `provider.active` selects one.
    pub providers: HashMap<String, ProviderConfig>,
    pub provider_active: Option<String>,
    pub agent: AgentConfig,
    pub session: SessionConfig,
    pub context: ContextConfigFile,
    pub tools: ToolConfig,
    pub protocol: ProtocolConfig,
    pub tui: TuiConfig,
    pub update: UpdateConfig,
    pub notify: NotifyConfig,
    pub provider_limits: ProviderLimitsFile,
    pub skills: SkillsConfig,
    pub agents: AgentsConfig,
    pub web: WebConfig,
    /// `[channels.<channel>.<account>]`: each chat account's settings. SCV's
    /// channel store reads and edits them in the instance's `config.toml`;
    /// here they are only validated.
    pub channels: BTreeMap<String, BTreeMap<String, AccountSettings>>,
    /// The process-owned root: see [`Layout`] for what it holds.
    #[serde(skip)]
    pub instance_home: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub active: Option<String>,
    pub kind: String,
    pub wire_api: String,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub timeout_seconds: u64,
    pub headers: HashMap<String, String>,
    /// Show images users attach to the model as image input. Turn it off
    /// for a model without vision; SCV also stops for the session after the
    /// provider rejects an image.
    pub image_input: bool,
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
pub struct NotifyConfig {
    /// Accounts as `<channel>:<account>`, such as `feishu:default`. A notice
    /// goes to the owner of the first one that is connected, on that one
    /// account only. Empty: the chat the owner last wrote from.
    pub owner: Vec<String>,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub max_steps: usize,
    pub system_prompt: String,
    /// `agent_*` tools are offered only while this SCV's own delegation depth
    /// is below this, so delegation chains stay bounded. 0 disables them.
    pub max_delegation_depth: u32,
    /// Delegated conversations a session remembers; starting another forgets
    /// the least recently used idle one.
    pub max_conversations: usize,
    /// A delegated conversation unused this long is forgotten.
    pub conversation_idle_seconds: u64,
    /// Background agent jobs (`background: true`) a session may run at once;
    /// 0 turns background delegation off.
    pub max_background: usize,
    /// Agents the user prefers, in order (such as `["codex", "claude"]`);
    /// the system prompt names the installed ones. Empty states no preference.
    pub prefer: Vec<String>,
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
            prefer: Vec::new(),
            system_prompt: "You are SCV, a concise and careful coding agent. Use tools to inspect, change, and verify the workspace.".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub max_history_bytes: usize,
    pub max_messages: usize,
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
pub struct ContextConfigFile {
    pub max_tokens: usize,
    pub reserve_output_tokens: usize,
    pub safety_margin_tokens: usize,
    pub bytes_per_token: usize,
    pub summary_max_chars: usize,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    OnRisk,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolConfig {
    pub approval_policy: ApprovalPolicy,
    /// `bash` timeout when a call does not choose one.
    pub command_timeout_seconds: u64,
    /// Native-agent timeout when a call does not choose one.
    pub agent_timeout_seconds: u64,
    /// The longest timeout a single tool call may request.
    pub max_timeout_seconds: u64,
    pub output_limit_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            approval_policy: ApprovalPolicy::OnRisk,
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
pub struct ProtocolConfig {
    pub max_client_frame_bytes: usize,
    pub max_server_frame_bytes: usize,
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
pub struct TuiConfig {
    pub max_transcript_bytes: usize,
    pub max_transcript_items: usize,
    pub max_prompt_history_bytes: usize,
    pub max_prompt_history_items: usize,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            max_transcript_bytes: 8 * 1024 * 1024,
            max_transcript_items: 10_000,
            max_prompt_history_bytes: 1024 * 1024,
            max_prompt_history_items: 200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderLimitsFile {
    pub max_sse_event_bytes: usize,
    pub max_response_bytes: usize,
    pub max_assistant_bytes: usize,
    pub max_tool_calls: usize,
    pub max_tool_arguments_bytes: usize,
    pub max_retries: usize,
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
pub struct SkillsConfig {
    pub user_dir: PathBuf,
    pub project_dir: PathBuf,
    /// List the agent skills (`.agents/skills`, `.claude/skills`) of the
    /// workspace and its immediate child projects in tool-enabled sessions.
    pub scan_projects: bool,
    pub max_skills: usize,
    pub max_skill_bytes: usize,
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
pub enum WebSearchMode {
    Off,
    /// The provider endpoint's hosted Responses `web_search` tool.
    Provider,
    Searxng,
    Brave,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Offer `web_fetch` (and search, when configured) to tool-enabled sessions.
    pub enabled: bool,
    pub fetch_max_bytes: usize,
    pub fetch_timeout_seconds: u64,
    pub max_redirects: usize,
    /// HTTPS hosts `web_fetch` may read without approval.
    pub auto_approve_domains: Vec<String>,
    /// Let `web_fetch` reach loopback, private, and link-local addresses.
    pub allow_private_addresses: bool,
    pub search: WebSearchMode,
    pub searxng_url: Option<String>,
    pub brave_url: String,
    pub brave_api_key: Option<String>,
    pub brave_api_key_env: Option<String>,
    pub max_search_results: usize,
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
pub struct AdapterConfig {
    pub command: String,
    pub args: Vec<String>,
    /// `full` adds the CLI's own switches for unprompted, unsandboxed work.
    pub permissions: AgentPermissions,
    /// Placed immediately before the prompt (`grok -p <prompt>`).
    pub prompt_args: Vec<String>,
    /// Appended when a call selects a model; `{model}` is substituted.
    pub model_args: Vec<String>,
    /// Appended when a call selects an effort; `{effort}` is substituted.
    pub effort_args: Vec<String>,
    /// How SCV talks to the agent: its ACP server or one process per turn.
    pub transport: AgentTransport,
    /// When to choose this agent, in the user's words; added to its tool
    /// description so the model can pick between agents.
    pub use_for: Option<String>,
}

/// How SCV talks to a delegated agent that has an ACP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTransport {
    /// The agent's ACP server when it is installed, else one process per turn.
    #[default]
    Auto,
    /// Only its ACP server; the agent is not offered while it is missing.
    Acp,
    /// One CLI process per turn, continued through the CLI's own resume.
    Resume,
}

/// How much a delegated CLI may do without its own prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPermissions {
    /// Add nothing: the CLI's own configuration decides.
    #[default]
    Default,
    /// Add the CLI's full-autonomy switches: no approval prompts, no sandbox,
    /// and web search where the CLI gates it. An explicit user opt-in.
    Full,
}

/// `[agents.<name>]` for every adapter in [`scv_tools::adapters::ADAPTERS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentsConfig(pub BTreeMap<String, AdapterConfig>);

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
                            permissions: AgentPermissions::Default,
                            prompt_args: strings(adapter.prompt_args),
                            model_args: strings(adapter.model_args),
                            effort_args: strings(adapter.effort_args),
                            transport: AgentTransport::Auto,
                            use_for: None,
                        },
                    )
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub no_tools: bool,
}
