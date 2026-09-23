use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    io::Write,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use scv_core::{AgentConfig as CoreAgentConfig, ContextConfig, HistoryLimits};
use scv_provider_openai::ProviderLimits;
use scv_tools::{AgentAdapterConfig, ToolsConfig};
use serde::{Deserialize, Serialize};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// A day: long enough for any delegated job, short enough that deadline
/// arithmetic never overflows.
const MAX_TOOL_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;
/// Retries multiply provider load and turn latency, so they stay small.
const MAX_PROVIDER_RETRIES: usize = 10;

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
    pub provider_limits: ProviderLimitsFile,
    pub skills: SkillsConfig,
    pub agents: AgentsConfig,
    /// The process-owned root used for sockets, credentials, skills, and adapters.
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Optional Cargo registry index URL used by `scv update`.
    pub index_url: Option<String>,
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
        }
    }
}

impl Config {
    pub fn init_user_config() -> Result<PathBuf> {
        let path = user_config_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine user config path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create config directory")?;
            ensure_private_dir(parent)?;
        }
        let content = "[provider]\nactive = \"openai\"\n\n[providers.openai]\nkind = \"openai-compatible\"\nmodel = \"gpt-4.1-mini\"\nbase_url = \"https://api.openai.com/v1\"\napi_key_env = \"OPENAI_API_KEY\"\n";
        if !path.exists() {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("configuration path has no parent"))?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)
                .context("create temporary example configuration")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temporary
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .context("secure temporary configuration")?;
            }
            temporary
                .write_all(content.as_bytes())
                .context("write example configuration")?;
            temporary
                .as_file()
                .sync_all()
                .context("sync example configuration")?;
            match temporary.persist(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error).context("install example configuration"),
            }
        }
        Ok(path)
    }
    pub fn active_provider(&self) -> Result<ProviderConfig> {
        if let Some(name) = self
            .provider_active
            .as_deref()
            .or(self.provider.active.as_deref())
        {
            return self
                .providers
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("active provider profile {name:?} was not found"));
        }
        Ok(self.provider.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub max_steps: usize,
    pub system_prompt: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 128,
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

impl ApprovalPolicy {
    fn strictness(self) -> u8 {
        match self {
            Self::OnRisk => 1,
            Self::Always => 2,
            Self::Never => 3,
        }
    }
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

impl Config {
    pub fn load(workspace: &std::path::Path, overrides: ConfigOverrides) -> Result<Self> {
        Self::load_layers(Some(workspace), overrides)
    }

    /// Load without a project layer, for settings that project configuration
    /// can never set (such as `[agents]`), so the caller's directory is irrelevant.
    pub fn load_user(overrides: ConfigOverrides) -> Result<Self> {
        Self::load_layers(None, overrides)
    }

    fn load_layers(
        workspace: Option<&std::path::Path>,
        overrides: ConfigOverrides,
    ) -> Result<Self> {
        let instance_home = user_home_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine SCV instance home"))?;
        std::fs::create_dir_all(&instance_home).context("create SCV instance home")?;
        ensure_private_dir(&instance_home)?;
        let mut value: toml::Value = toml::from_str(
            &toml::to_string(&Self::default()).context("serialize default configuration")?,
        )?;

        if let Some(user_path) = user_config_path()
            && user_path.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&user_path)?.permissions().mode() & 0o077 != 0 {
                    bail!("user configuration is readable by group or others; run chmod 600");
                }
            }
            merge(&mut value, read_layer(&user_path)?);
        }
        let user_baseline: Self = value
            .clone()
            .try_into()
            .context("parse user configuration")?;

        if let Some(workspace) = workspace {
            let project_path = workspace.join(".scv/config.toml");
            // A workspace whose `.scv` is the SCV home (such as running from
            // `~`) has no project layer: that file is the user configuration,
            // already applied above at full trust.
            let user_file = user_config_path().and_then(|path| std::fs::canonicalize(path).ok());
            if project_path.is_file() {
                let canonical_project = std::fs::canonicalize(&project_path)
                    .with_context(|| format!("resolve configuration {}", project_path.display()))?;
                if user_file.as_ref() != Some(&canonical_project) {
                    if !canonical_project.starts_with(workspace) {
                        bail!("project configuration escaped workspace");
                    }
                    let project = read_layer(&canonical_project)?;
                    validate_project_keys(&project)?;
                    let mut candidate_value = value.clone();
                    merge(&mut candidate_value, project);
                    let candidate: Self = candidate_value
                        .clone()
                        .try_into()
                        .context("parse project configuration")?;
                    validate_project_not_weaker(&user_baseline, &candidate)?;
                    value = candidate_value;
                }
            }
        }

        if let Some(explicit) = std::env::var_os("SCV_CONFIG") {
            let path = PathBuf::from(explicit);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                    bail!("explicit configuration is readable by group or others; run chmod 600");
                }
            }
            merge(&mut value, read_layer(&path)?);
        }
        let mut config: Self = value.try_into().context("parse merged configuration")?;
        if let Some(name) = overrides.provider.as_deref() {
            config.provider_active = Some(name.to_owned());
        }
        let selected = config.active_provider()?;
        config.provider = selected;
        if let Ok(model) = std::env::var("SCV_MODEL") {
            config.provider.model = model;
        }
        if let Ok(base_url) = std::env::var("SCV_BASE_URL") {
            config.provider.base_url = base_url;
        }
        if let Ok(api_key_env) = std::env::var("SCV_API_KEY_ENV") {
            config.provider.api_key_env = Some(api_key_env);
        }
        if let Some(model) = overrides.model {
            config.provider.model = model;
        }
        if let Some(base_url) = overrides.base_url {
            config.provider.base_url = base_url;
        }
        if let Some(policy) = overrides.approval_policy {
            config.tools.approval_policy = policy;
        }
        if config.skills.user_dir == std::path::Path::new("~/.scv/skills")
            && let Some(home) = std::env::var_os("SCV_HOME")
        {
            config.skills.user_dir = PathBuf::from(home).join("skills");
        }
        config.skills.user_dir = expand_home(&config.skills.user_dir);
        config.instance_home = instance_home;
        config.validate()?;
        Ok(config)
    }

    pub fn core_agent(&self, system_prompt: String) -> CoreAgentConfig {
        CoreAgentConfig {
            system_prompt,
            max_steps: self.agent.max_steps,
            history_limits: HistoryLimits {
                max_bytes: self.session.max_history_bytes,
                max_messages: self.session.max_messages,
                note_max_chars: self.context.summary_max_chars,
            },
        }
    }

    pub fn tools(&self) -> ToolsConfig {
        ToolsConfig {
            command_timeout: Duration::from_secs(self.tools.command_timeout_seconds),
            agent_timeout: Duration::from_secs(self.tools.agent_timeout_seconds),
            max_timeout: Duration::from_secs(self.tools.max_timeout_seconds),
            output_limit_bytes: self.tools.output_limit_bytes,
            max_read_bytes: self.tools.max_read_bytes,
            max_write_bytes: self.tools.max_write_bytes,
        }
    }

    pub fn provider_limits(&self) -> ProviderLimits {
        ProviderLimits {
            max_sse_event_bytes: self.provider_limits.max_sse_event_bytes,
            max_response_bytes: self.provider_limits.max_response_bytes,
            max_assistant_bytes: self.provider_limits.max_assistant_bytes,
            max_tool_calls: self.provider_limits.max_tool_calls,
            max_tool_arguments_bytes: self.provider_limits.max_tool_arguments_bytes,
            max_retries: self.provider_limits.max_retries,
            ..ProviderLimits::default()
        }
    }

    pub fn adapters(&self) -> HashMap<String, AgentAdapterConfig> {
        let user_home = dirs::home_dir();
        self.agents
            .0
            .iter()
            .filter_map(|(name, config)| {
                let descriptor = scv_tools::adapters::adapter(name)?;
                let adapter_home = self.instance_home.join("adapters").join(name);
                let mut environment = vec![
                    (OsString::from("SCV_HOME"), adapter_home.clone().into()),
                    (OsString::from("HOME"), adapter_home.clone().into()),
                    (
                        OsString::from("XDG_CONFIG_HOME"),
                        adapter_home.join("config").into(),
                    ),
                    (
                        OsString::from("XDG_DATA_HOME"),
                        adapter_home.join("data").into(),
                    ),
                    (
                        OsString::from("XDG_STATE_HOME"),
                        adapter_home.join("state").into(),
                    ),
                ];
                for (variable, relative) in descriptor.home_environment {
                    let path = if relative.is_empty() {
                        adapter_home.clone()
                    } else {
                        adapter_home.join(relative)
                    };
                    environment.push((OsString::from(variable), path.into()));
                }
                let full = config.permissions == AgentPermissions::Full;
                environment.extend(
                    descriptor
                        .fixed_environment
                        .iter()
                        .chain(
                            descriptor
                                .full_permission_environment
                                .iter()
                                .filter(|_| full),
                        )
                        .map(|(variable, value)| (OsString::from(variable), OsString::from(value))),
                );
                Some((
                    format!("agent_{name}"),
                    AgentAdapterConfig {
                        command: config.command.clone(),
                        args: config.args.clone(),
                        prompt_args: config.prompt_args.clone(),
                        full_permission_args: full.then(|| {
                            descriptor
                                .full_permission_args
                                .iter()
                                .map(|arg| (*arg).to_owned())
                                .collect()
                        }),
                        model_args: config.model_args.clone(),
                        effort_args: config.effort_args.clone(),
                        model_hint: descriptor.model_hint.into(),
                        environment,
                        search_dirs: user_home
                            .as_deref()
                            .map(|home| scv_tools::adapters::adapter_search_dirs(descriptor, home))
                            .unwrap_or_default(),
                    },
                ))
            })
            .collect()
    }

    pub fn prepare_adapter_homes(&self) -> Result<()> {
        for name in self.agents.0.keys() {
            let path = self.instance_home.join("adapters").join(name);
            std::fs::create_dir_all(&path)
                .with_context(|| format!("create isolated {name} adapter home"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("secure isolated {name} adapter home"))?;
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.provider.kind != "openai-compatible" {
            bail!("provider.kind must be openai-compatible in v0.1");
        }
        if self.provider.model.trim().is_empty()
            || self.provider.base_url.trim().is_empty()
            || self
                .provider
                .api_key
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
                && self
                    .provider
                    .api_key_env
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
        {
            bail!(
                "provider model and base_url must be non-empty; configure api_key or api_key_env"
            );
        }
        for (agent, adapter) in &self.agents.0 {
            if scv_tools::adapters::adapter(agent).is_none() {
                let known: Vec<_> = scv_tools::adapters::ADAPTERS
                    .iter()
                    .map(|adapter| adapter.name)
                    .collect();
                bail!(
                    "unknown agent [agents.{agent}]; known agents are {}",
                    known.join(", ")
                );
            }
            let name = format!("agents.{agent}.command");
            if adapter.command.trim().is_empty() {
                bail!("{name} must be non-empty");
            }
            for (field, template, placeholder) in [
                ("model_args", &adapter.model_args, "{model}"),
                ("effort_args", &adapter.effort_args, "{effort}"),
            ] {
                if !template.is_empty() && !template.iter().any(|arg| arg.contains(placeholder)) {
                    let adapter = name.trim_end_matches(".command");
                    bail!("{adapter}.{field} must contain {placeholder} or be empty");
                }
            }
            let adapter_bytes = adapter.command.len()
                + [
                    &adapter.args,
                    &adapter.prompt_args,
                    &adapter.model_args,
                    &adapter.effort_args,
                ]
                .into_iter()
                .flatten()
                .map(String::len)
                .sum::<usize>();
            if adapter_bytes > 16 * 1024 {
                bail!("{name} and its fixed arguments exceed 16384 bytes");
            }
        }
        let positives = [
            (
                "provider.timeout_seconds",
                usize::try_from(self.provider.timeout_seconds).unwrap_or(usize::MAX),
            ),
            ("agent.max_steps", self.agent.max_steps),
            ("session.max_history_bytes", self.session.max_history_bytes),
            ("session.max_messages", self.session.max_messages),
            ("context.max_tokens", self.context.max_tokens),
            ("context.bytes_per_token", self.context.bytes_per_token),
            ("context.summary_max_chars", self.context.summary_max_chars),
            (
                "tools.command_timeout_seconds",
                usize::try_from(self.tools.command_timeout_seconds).unwrap_or(usize::MAX),
            ),
            (
                "tools.agent_timeout_seconds",
                usize::try_from(self.tools.agent_timeout_seconds).unwrap_or(usize::MAX),
            ),
            (
                "tools.max_timeout_seconds",
                usize::try_from(self.tools.max_timeout_seconds).unwrap_or(usize::MAX),
            ),
            ("tools.output_limit_bytes", self.tools.output_limit_bytes),
            ("tools.max_read_bytes", self.tools.max_read_bytes),
            ("tools.max_write_bytes", self.tools.max_write_bytes),
            (
                "protocol.max_client_frame_bytes",
                self.protocol.max_client_frame_bytes,
            ),
            (
                "protocol.max_server_frame_bytes",
                self.protocol.max_server_frame_bytes,
            ),
            ("tui.max_transcript_bytes", self.tui.max_transcript_bytes),
            ("tui.max_transcript_items", self.tui.max_transcript_items),
            (
                "tui.max_prompt_history_bytes",
                self.tui.max_prompt_history_bytes,
            ),
            (
                "tui.max_prompt_history_items",
                self.tui.max_prompt_history_items,
            ),
            (
                "provider_limits.max_sse_event_bytes",
                self.provider_limits.max_sse_event_bytes,
            ),
            (
                "provider_limits.max_response_bytes",
                self.provider_limits.max_response_bytes,
            ),
            (
                "provider_limits.max_assistant_bytes",
                self.provider_limits.max_assistant_bytes,
            ),
            (
                "provider_limits.max_tool_calls",
                self.provider_limits.max_tool_calls,
            ),
            (
                "provider_limits.max_tool_arguments_bytes",
                self.provider_limits.max_tool_arguments_bytes,
            ),
            ("skills.max_skills", self.skills.max_skills),
            ("skills.max_skill_bytes", self.skills.max_skill_bytes),
        ];
        if let Some((name, _)) = positives.into_iter().find(|(_, value)| *value == 0) {
            bail!("{name} must be positive");
        }
        for (name, value) in [
            (
                "tools.command_timeout_seconds",
                self.tools.command_timeout_seconds,
            ),
            (
                "tools.agent_timeout_seconds",
                self.tools.agent_timeout_seconds,
            ),
        ] {
            if value > self.tools.max_timeout_seconds {
                bail!("{name} exceeds tools.max_timeout_seconds");
            }
        }
        if self.tools.max_timeout_seconds > MAX_TOOL_TIMEOUT_SECONDS {
            bail!("tools.max_timeout_seconds must be at most {MAX_TOOL_TIMEOUT_SECONDS}");
        }
        if self
            .context
            .reserve_output_tokens
            .saturating_add(self.context.safety_margin_tokens)
            >= self.context.max_tokens
        {
            bail!("context reserve and safety margin consume max_tokens");
        }
        let worst_assistant_frame = self
            .provider_limits
            .max_assistant_bytes
            .saturating_mul(6)
            .saturating_add(64 * 1024);
        if worst_assistant_frame > self.protocol.max_server_frame_bytes {
            bail!(
                "provider_limits.max_assistant_bytes can exceed protocol.max_server_frame_bytes after JSON escaping"
            );
        }
        if self.provider_limits.max_tool_arguments_bytes > self.provider_limits.max_response_bytes {
            bail!("tool argument limit exceeds provider response limit");
        }
        if self.provider_limits.max_sse_event_bytes > self.provider_limits.max_response_bytes {
            bail!("provider SSE event limit exceeds provider response limit");
        }
        if self.provider_limits.max_retries > MAX_PROVIDER_RETRIES {
            bail!("provider_limits.max_retries must be at most {MAX_PROVIDER_RETRIES}");
        }
        if self.protocol.max_client_frame_bytes < 4096 {
            bail!("protocol.max_client_frame_bytes must be at least 4096");
        }
        if self.protocol.max_server_frame_bytes < 64 * 1024 {
            bail!("protocol.max_server_frame_bytes must be at least 65536");
        }
        let worst_tool_frame = self
            .tools
            .output_limit_bytes
            .max(self.tools.max_read_bytes)
            .saturating_mul(12)
            .saturating_add(64 * 1024);
        let worst_skill_frame = self
            .skills
            .max_skill_bytes
            .saturating_mul(6)
            .saturating_add(64 * 1024);
        let worst_arguments_frame = self
            .provider_limits
            .max_tool_arguments_bytes
            .saturating_mul(6)
            .saturating_add(64 * 1024);
        if worst_tool_frame
            .max(worst_skill_frame)
            .max(worst_arguments_frame)
            > self.protocol.max_server_frame_bytes
        {
            bail!(
                "tool or skill limits can exceed protocol.max_server_frame_bytes after JSON escaping"
            );
        }
        if self.skills.project_dir.is_absolute()
            || self
                .skills
                .project_dir
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            bail!("skills.project_dir must be a contained relative path");
        }
        Ok(())
    }
}

fn user_config_path() -> Option<PathBuf> {
    user_home_path().map(|path| path.join("config.toml"))
}

pub fn user_home_path() -> Option<PathBuf> {
    let path = std::env::var_os("SCV_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".scv")))?;
    if path.exists() {
        Some(std::fs::canonicalize(path.clone()).unwrap_or(path))
    } else if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure directory {}", path.display()))?;
    }
    Ok(())
}

fn read_layer(path: &std::path::Path) -> Result<toml::Value> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("stat configuration {}", path.display()))?
        .len();
    if size > MAX_CONFIG_BYTES {
        bail!("configuration {} exceeds 1 MiB", path.display());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read configuration {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parse configuration {}", path.display()))
}

fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn validate_project_keys(value: &toml::Value) -> Result<()> {
    let Some(table) = value.as_table() else {
        bail!("project configuration must be a TOML table");
    };
    for forbidden in [
        "provider",
        "providers",
        "provider_active",
        "agents",
        "update",
    ] {
        if table.contains_key(forbidden) {
            bail!("project configuration cannot set [{forbidden}]");
        }
    }
    if table
        .get("skills")
        .and_then(toml::Value::as_table)
        .is_some_and(|skills| skills.contains_key("user_dir"))
    {
        bail!("project configuration cannot set skills.user_dir");
    }
    if table
        .get("agent")
        .and_then(toml::Value::as_table)
        .is_some_and(|agent| agent.contains_key("system_prompt"))
    {
        bail!("project configuration cannot replace agent.system_prompt");
    }
    Ok(())
}

fn validate_project_not_weaker(user: &Config, project: &Config) -> Result<()> {
    macro_rules! no_larger {
        ($field:expr, $name:literal) => {
            if $field.1 > $field.0 {
                bail!(concat!("project configuration cannot raise ", $name));
            }
        };
    }
    no_larger!(
        (user.agent.max_steps, project.agent.max_steps),
        "agent.max_steps"
    );
    no_larger!(
        (
            user.session.max_history_bytes,
            project.session.max_history_bytes
        ),
        "session.max_history_bytes"
    );
    no_larger!(
        (user.session.max_messages, project.session.max_messages),
        "session.max_messages"
    );
    no_larger!(
        (user.context.max_tokens, project.context.max_tokens),
        "context.max_tokens"
    );
    no_larger!(
        (
            user.context.summary_max_chars,
            project.context.summary_max_chars
        ),
        "context.summary_max_chars"
    );
    no_larger!(
        (
            user.tools.command_timeout_seconds,
            project.tools.command_timeout_seconds
        ),
        "tools.command_timeout_seconds"
    );
    no_larger!(
        (
            user.tools.agent_timeout_seconds,
            project.tools.agent_timeout_seconds
        ),
        "tools.agent_timeout_seconds"
    );
    no_larger!(
        (
            user.tools.max_timeout_seconds,
            project.tools.max_timeout_seconds
        ),
        "tools.max_timeout_seconds"
    );
    no_larger!(
        (
            user.tools.output_limit_bytes,
            project.tools.output_limit_bytes
        ),
        "tools.output_limit_bytes"
    );
    no_larger!(
        (user.tools.max_read_bytes, project.tools.max_read_bytes),
        "tools.max_read_bytes"
    );
    no_larger!(
        (user.tools.max_write_bytes, project.tools.max_write_bytes),
        "tools.max_write_bytes"
    );
    no_larger!(
        (
            user.protocol.max_client_frame_bytes,
            project.protocol.max_client_frame_bytes
        ),
        "protocol.max_client_frame_bytes"
    );
    no_larger!(
        (
            user.protocol.max_server_frame_bytes,
            project.protocol.max_server_frame_bytes
        ),
        "protocol.max_server_frame_bytes"
    );
    no_larger!(
        (
            user.provider_limits.max_response_bytes,
            project.provider_limits.max_response_bytes
        ),
        "provider_limits.max_response_bytes"
    );
    no_larger!(
        (
            user.provider_limits.max_sse_event_bytes,
            project.provider_limits.max_sse_event_bytes
        ),
        "provider_limits.max_sse_event_bytes"
    );
    no_larger!(
        (
            user.provider_limits.max_assistant_bytes,
            project.provider_limits.max_assistant_bytes
        ),
        "provider_limits.max_assistant_bytes"
    );
    no_larger!(
        (
            user.provider_limits.max_tool_calls,
            project.provider_limits.max_tool_calls
        ),
        "provider_limits.max_tool_calls"
    );
    no_larger!(
        (
            user.provider_limits.max_tool_arguments_bytes,
            project.provider_limits.max_tool_arguments_bytes
        ),
        "provider_limits.max_tool_arguments_bytes"
    );
    no_larger!(
        (
            user.provider_limits.max_retries,
            project.provider_limits.max_retries
        ),
        "provider_limits.max_retries"
    );
    no_larger!(
        (
            user.tui.max_transcript_bytes,
            project.tui.max_transcript_bytes
        ),
        "tui.max_transcript_bytes"
    );
    no_larger!(
        (
            user.tui.max_transcript_items,
            project.tui.max_transcript_items
        ),
        "tui.max_transcript_items"
    );
    no_larger!(
        (
            user.tui.max_prompt_history_bytes,
            project.tui.max_prompt_history_bytes
        ),
        "tui.max_prompt_history_bytes"
    );
    no_larger!(
        (
            user.tui.max_prompt_history_items,
            project.tui.max_prompt_history_items
        ),
        "tui.max_prompt_history_items"
    );
    no_larger!(
        (user.skills.max_skills, project.skills.max_skills),
        "skills.max_skills"
    );
    no_larger!(
        (user.skills.max_skill_bytes, project.skills.max_skill_bytes),
        "skills.max_skill_bytes"
    );
    if project.context.reserve_output_tokens < user.context.reserve_output_tokens
        || project.context.safety_margin_tokens < user.context.safety_margin_tokens
    {
        bail!("project configuration cannot lower context reserves");
    }
    if project.context.bytes_per_token > user.context.bytes_per_token {
        bail!("project configuration cannot raise context.bytes_per_token");
    }
    if project.tools.approval_policy.strictness() < user.tools.approval_policy.strictness() {
        bail!("project configuration cannot weaken tools.approval_policy");
    }
    if project.skills.scan_projects && !user.skills.scan_projects {
        bail!("project configuration cannot enable skills.scan_projects");
    }
    Ok(())
}

fn expand_home(path: &std::path::Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_cannot_redirect_provider_or_agent() {
        let provider: toml::Value = toml::from_str(
            r#"[provider]
base_url = "https://attacker.invalid"
"#,
        )
        .unwrap();
        assert!(validate_project_keys(&provider).is_err());

        let agent: toml::Value = toml::from_str(
            r#"[agents.codex]
command = "/tmp/fake"
"#,
        )
        .unwrap();
        assert!(validate_project_keys(&agent).is_err());
    }

    #[test]
    fn project_may_tighten_but_not_weaken_limits() {
        let user = Config::default();
        let mut tighter = user.clone();
        tighter.tools.output_limit_bytes /= 2;
        tighter.tools.approval_policy = ApprovalPolicy::Always;
        assert!(validate_project_not_weaker(&user, &tighter).is_ok());

        let mut weaker = user.clone();
        weaker.tools.output_limit_bytes *= 2;
        assert!(validate_project_not_weaker(&user, &weaker).is_err());
    }

    #[test]
    fn timeouts_default_below_a_ceiling_that_projects_may_only_lower() {
        let user = Config::default();
        assert_eq!(
            (
                user.tools.command_timeout_seconds,
                user.tools.agent_timeout_seconds,
                user.tools.max_timeout_seconds
            ),
            (600, 3600, 14400)
        );
        assert_eq!(user.agent.max_steps, 128);
        assert_eq!(user.provider.timeout_seconds, 600);
        let tools = user.tools();
        assert_eq!(tools.command_timeout, Duration::from_secs(600));
        assert_eq!(tools.agent_timeout, Duration::from_secs(3600));
        assert_eq!(tools.max_timeout, Duration::from_secs(14400));
        // A ClawBot owner turn outlasts the ceiling by five minutes: 4h05m.
        assert_eq!(
            scv_clawbot::owner_turn_timeout(tools.max_timeout),
            Duration::from_secs(4 * 3600 + 5 * 60)
        );

        for (field, name) in [
            (0, "tools.command_timeout_seconds"),
            (1, "tools.agent_timeout_seconds"),
        ] {
            let mut config = Config::default();
            let value = if field == 0 {
                &mut config.tools.command_timeout_seconds
            } else {
                &mut config.tools.agent_timeout_seconds
            };
            *value = config.tools.max_timeout_seconds + 1;
            assert_eq!(
                config.validate().unwrap_err().to_string(),
                format!("{name} exceeds tools.max_timeout_seconds")
            );
        }
        let mut unbounded = Config::default();
        unbounded.tools.max_timeout_seconds = MAX_TOOL_TIMEOUT_SECONDS + 1;
        assert!(unbounded.validate().is_err());
        let mut zero = Config::default();
        zero.tools.agent_timeout_seconds = 0;
        assert!(zero.validate().is_err());

        let mut lower = user.clone();
        lower.tools.max_timeout_seconds = 900;
        lower.tools.agent_timeout_seconds = 300;
        assert!(validate_project_not_weaker(&user, &lower).is_ok());
        for raise in [
            |config: &mut Config| config.tools.max_timeout_seconds += 1,
            |config: &mut Config| config.tools.agent_timeout_seconds += 1,
        ] {
            let mut higher = user.clone();
            raise(&mut higher);
            assert!(validate_project_not_weaker(&user, &higher).is_err());
        }
    }

    #[test]
    fn provider_retries_are_bounded_and_projects_may_only_lower_them() {
        let user = Config::default();
        assert_eq!(user.provider_limits.max_retries, 2);
        assert_eq!(user.provider_limits().max_retries, 2);
        let mut none = user.clone();
        none.provider_limits.max_retries = 0;
        assert!(none.validate().is_ok());
        assert!(validate_project_not_weaker(&user, &none).is_ok());
        assert!(validate_project_not_weaker(&none, &user).is_err());
        let mut excessive = user.clone();
        excessive.provider_limits.max_retries = MAX_PROVIDER_RETRIES + 1;
        assert_eq!(
            excessive.validate().unwrap_err().to_string(),
            format!("provider_limits.max_retries must be at most {MAX_PROVIDER_RETRIES}")
        );
    }

    #[test]
    fn projects_may_disable_but_not_enable_project_skill_scanning() {
        let user = Config::default();
        let mut disabled = user.clone();
        disabled.skills.scan_projects = false;
        assert!(validate_project_not_weaker(&user, &disabled).is_ok());
        assert!(validate_project_not_weaker(&disabled, &user).is_err());
    }

    #[test]
    fn cross_field_validation_accounts_for_json_escaping() {
        let mut config = Config::default();
        config.protocol.max_server_frame_bytes = config.provider_limits.max_assistant_bytes;
        assert!(config.validate().is_err());
    }

    #[test]
    fn adapter_selection_templates_survive_partial_overrides_and_validate() {
        let mut value: toml::Value =
            toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        merge(
            &mut value,
            toml::from_str(
                r#"[agents.claude]
args = ["-p", "--permission-mode", "acceptEdits"]
"#,
            )
            .unwrap(),
        );
        let config: Config = value.try_into().unwrap();
        let claude = &config.agents.0["claude"];
        assert_eq!(claude.args.len(), 3);
        assert_eq!(claude.model_args, ["--model", "{model}"]);
        assert_eq!(claude.effort_args, ["--effort", "{effort}"]);
        assert_eq!(
            config.agents.0["pi"].effort_args,
            ["--thinking", "{effort}"]
        );
        assert_eq!(config.agents.0["grok"].prompt_args, ["-p"]);

        let mut invalid = Config::default();
        invalid.agents.0.get_mut("claude").unwrap().effort_args = vec!["--effort".into()];
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("agents.claude.effort_args must contain {effort}")
        );
    }

    #[test]
    fn adapters_are_bound_to_the_instance_home() {
        let config = Config {
            instance_home: PathBuf::from("/tmp/scv-instance"),
            ..Config::default()
        };
        let adapters = config.adapters();
        let codex = &adapters["agent_codex"];
        assert!(codex.environment.contains(&(
            OsString::from("CODEX_HOME"),
            OsString::from("/tmp/scv-instance/adapters/codex")
        )));
        assert!(codex.environment.contains(&(
            OsString::from("SCV_HOME"),
            OsString::from("/tmp/scv-instance/adapters/codex")
        )));
        for (agent, variable, path) in [
            ("grok", "GROK_HOME", "/tmp/scv-instance/adapters/grok/.grok"),
            ("dsh", "DSH_HOME", "/tmp/scv-instance/adapters/dsh/.dsh"),
            (
                "pi",
                "PI_CODING_AGENT_DIR",
                "/tmp/scv-instance/adapters/pi/.pi/agent",
            ),
        ] {
            let adapter = &adapters[&format!("agent_{agent}")];
            assert!(
                adapter
                    .environment
                    .contains(&(OsString::from(variable), OsString::from(path))),
                "{agent}"
            );
            assert!(adapter.environment.contains(&(
                OsString::from("HOME"),
                OsString::from(format!("/tmp/scv-instance/adapters/{agent}"))
            )));
        }
        assert!(adapters["agent_grok"].environment.contains(&(
            OsString::from("GROK_DISABLE_AUTOUPDATER"),
            OsString::from("1")
        )));
        assert_eq!(adapters["agent_grok"].prompt_args, ["-p"]);
        assert!(adapters["agent_pi"].model_hint.contains("provider scv"));
    }

    #[test]
    fn full_permissions_are_opt_in_per_agent_and_combine_with_args() {
        let defaults = Config::default().adapters();
        for adapter in defaults.values() {
            assert_eq!(adapter.full_permission_args, None);
        }
        let mut value: toml::Value =
            toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        merge(
            &mut value,
            toml::from_str(
                "[agents.claude]\npermissions = \"full\"\n\n\
                 [agents.codex]\nargs = [\"exec\", \"--skip-git-repo-check\"]\npermissions = \"full\"\n\n\
                 [agents.grok]\npermissions = \"full\"\n\n\
                 [agents.dsh]\npermissions = \"full\"\n\n\
                 [agents.pi]\npermissions = \"full\"\n",
            )
            .unwrap(),
        );
        let config: Config = value.try_into().unwrap();
        config.validate().unwrap();
        let adapters = config.adapters();
        let full = |agent: &str| {
            adapters[&format!("agent_{agent}")]
                .full_permission_args
                .clone()
                .unwrap()
        };
        assert_eq!(full("claude"), ["--permission-mode", "bypassPermissions"]);
        assert_eq!(
            full("codex"),
            [
                "--dangerously-bypass-approvals-and-sandbox",
                "-c",
                "web_search=\"live\""
            ]
        );
        assert_eq!(
            adapters["agent_codex"].args,
            ["exec", "--skip-git-repo-check"]
        );
        assert_eq!(full("grok"), ["--always-approve"]);
        assert!(full("dsh").is_empty());
        assert!(adapters["agent_dsh"].environment.contains(&(
            OsString::from("DSH_PERMISSION_MODE"),
            OsString::from("danger-full-access")
        )));
        assert!(
            !defaults["agent_dsh"]
                .environment
                .iter()
                .any(|(variable, _)| variable == "DSH_PERMISSION_MODE")
        );
        // pi has no permission system: `full` is accepted and adds nothing.
        assert!(full("pi").is_empty());

        let mut invalid: toml::Value =
            toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        merge(
            &mut invalid,
            toml::from_str("[agents.claude]\npermissions = \"yolo\"\n").unwrap(),
        );
        assert!(invalid.try_into::<Config>().is_err());
    }

    #[test]
    fn user_agent_overrides_merge_over_every_built_in_and_unknown_agents_fail() {
        let mut value: toml::Value =
            toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        merge(
            &mut value,
            toml::from_str(
                "[agents.pi]
model_args = []

[agents.grok]
args = [\"--always-approve\"]
",
            )
            .unwrap(),
        );
        let config: Config = value.clone().try_into().unwrap();
        assert!(config.agents.0["pi"].model_args.is_empty());
        assert_eq!(config.agents.0["pi"].args, ["-p"]);
        assert_eq!(config.agents.0["grok"].args, ["--always-approve"]);
        assert_eq!(config.agents.0["grok"].prompt_args, ["-p"]);
        assert_eq!(
            config.agents.0.keys().collect::<Vec<_>>(),
            ["claude", "codex", "dsh", "grok", "pi"]
        );

        merge(
            &mut value,
            toml::from_str(
                "[agents.zcode]
command = \"zcode\"
",
            )
            .unwrap(),
        );
        let unknown: Config = value.try_into().unwrap();
        let error = unknown.validate().unwrap_err().to_string();
        assert!(error.contains("unknown agent [agents.zcode]"), "{error}");
    }
}
