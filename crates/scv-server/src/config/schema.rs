//! The configuration schema: every table of `config.toml`, with defaults.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use scv_channels::state::AccountSettings;
use scv_client::Secret;
use scv_core::ContextConfig;
use scv_provider_openai::ProviderLimits;
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
    pub(crate) tools: ToolConfig,
    pub(crate) protocol: ProtocolConfig,
    pub(crate) tui: TuiConfig,
    pub update: UpdateConfig,
    pub(crate) notify: NotifyConfig,
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
pub(crate) struct AgentConfig {
    pub(crate) max_steps: usize,
    pub(crate) system_prompt: String,
    /// `agent_*` tools are offered only while this SCV's own delegation depth
    /// is below this, so delegation chains stay bounded. 0 disables them.
    pub(crate) max_delegation_depth: u32,
    /// Delegated conversations a session remembers; starting another forgets
    /// the least recently used idle one.
    pub(crate) max_conversations: usize,
    /// A delegated conversation unused this long is forgotten.
    pub(crate) conversation_idle_seconds: u64,
    /// Background agent jobs (`background: true`) a session may run at once;
    /// 0 turns background delegation off.
    pub(crate) max_background: usize,
    /// Agents the user prefers, in order (such as `["codex", "claude"]`);
    /// the system prompt names the installed ones. Empty states no preference.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    OnRisk,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ToolConfig {
    pub(crate) approval_policy: ApprovalPolicy,
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
    /// `full` adds the CLI's own switches for unprompted, unsandboxed work.
    pub(crate) permissions: AgentPermissions,
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
    /// Model to pass when the work matches `use_for`. Without `use_for`, pass
    /// it whenever this agent is called, unless the user asks for another.
    pub(crate) model: Option<String>,
    /// Effort to pass the same way as `model`.
    pub(crate) effort: Option<String>,
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

/// How much a delegated CLI may do without its own prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentPermissions {
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
                            permissions: AgentPermissions::Default,
                            prompt_args: strings(adapter.prompt_args),
                            model_args: strings(adapter.model_args),
                            effort_args: strings(adapter.effort_args),
                            transport: AgentTransport::Auto,
                            use_for: None,
                            model: None,
                            effort: None,
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
    pub approval_policy: Option<ApprovalPolicy>,
    pub no_tools: bool,
    /// An explicit configuration layer (`--config`, or `SCV_CONFIG` as the
    /// process received it), applied after the user and project files.
    pub config_file: Option<PathBuf>,
}
