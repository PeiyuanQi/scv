use std::{collections::HashMap, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use peon_core::{AgentConfig as CoreAgentConfig, ContextConfig, HistoryLimits};
use peon_provider_openai::ProviderLimits;
use peon_tools::{AgentAdapterConfig, ToolsConfig};
use serde::{Deserialize, Serialize};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub provider: ProviderConfig,
    pub agent: AgentConfig,
    pub session: SessionConfig,
    pub context: ContextConfigFile,
    pub tools: ToolConfig,
    pub protocol: ProtocolConfig,
    pub tui: TuiConfig,
    pub provider_limits: ProviderLimitsFile,
    pub skills: SkillsConfig,
    pub agents: AgentsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub timeout_seconds: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "openai-compatible".into(),
            model: "gpt-4.1-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            timeout_seconds: 120,
        }
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
            max_steps: 32,
            system_prompt: "You are Peon, a concise and careful coding agent. Use tools to inspect, change, and verify the workspace.".into(),
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
    pub command_timeout_seconds: u64,
    pub output_limit_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            approval_policy: ApprovalPolicy::OnRisk,
            command_timeout_seconds: 120,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub user_dir: PathBuf,
    pub project_dir: PathBuf,
    pub max_skills: usize,
    pub max_skill_bytes: usize,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            user_dir: PathBuf::from("~/.config/peon/skills"),
            project_dir: PathBuf::from(".peon/skills"),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentsConfig {
    pub claude: AdapterConfig,
    pub codex: AdapterConfig,
    pub pi: AdapterConfig,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            claude: AdapterConfig {
                command: "claude".into(),
                args: vec!["-p".into()],
            },
            codex: AdapterConfig {
                command: "codex".into(),
                args: vec!["exec".into()],
            },
            pi: AdapterConfig {
                command: "pi".into(),
                args: vec!["-p".into()],
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub approval_policy: Option<ApprovalPolicy>,
}

impl Config {
    pub fn load(workspace: &std::path::Path, overrides: ConfigOverrides) -> Result<Self> {
        let mut value: toml::Value = toml::from_str(
            &toml::to_string(&Self::default()).context("serialize default configuration")?,
        )?;

        if let Some(user_path) = user_config_path()
            && user_path.is_file()
        {
            merge(&mut value, read_layer(&user_path)?);
        }
        let user_baseline: Self = value
            .clone()
            .try_into()
            .context("parse user configuration")?;

        let project_path = workspace.join(".peon/config.toml");
        if project_path.is_file() {
            let canonical_project = std::fs::canonicalize(&project_path)
                .with_context(|| format!("resolve configuration {}", project_path.display()))?;
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

        if let Some(explicit) = std::env::var_os("PEON_CONFIG") {
            let path = PathBuf::from(explicit);
            merge(&mut value, read_layer(&path)?);
        }
        let mut config: Self = value.try_into().context("parse merged configuration")?;
        if let Ok(model) = std::env::var("PEON_MODEL") {
            config.provider.model = model;
        }
        if let Ok(base_url) = std::env::var("PEON_BASE_URL") {
            config.provider.base_url = base_url;
        }
        if let Ok(api_key_env) = std::env::var("PEON_API_KEY_ENV") {
            config.provider.api_key_env = api_key_env;
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
        config.skills.user_dir = expand_home(&config.skills.user_dir);
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
        }
    }

    pub fn adapters(&self) -> HashMap<String, AgentAdapterConfig> {
        [
            ("agent_claude", &self.agents.claude),
            ("agent_codex", &self.agents.codex),
            ("agent_pi", &self.agents.pi),
        ]
        .into_iter()
        .map(|(name, config)| {
            (
                name.to_owned(),
                AgentAdapterConfig {
                    command: config.command.clone(),
                    args: config.args.clone(),
                },
            )
        })
        .collect()
    }

    fn validate(&self) -> Result<()> {
        if self.provider.kind != "openai-compatible" {
            bail!("provider.kind must be openai-compatible in v0.1");
        }
        if self.provider.model.trim().is_empty()
            || self.provider.base_url.trim().is_empty()
            || self.provider.api_key_env.trim().is_empty()
        {
            bail!("provider model, base_url, and api_key_env must be non-empty");
        }
        for (name, adapter) in [
            ("agents.claude.command", &self.agents.claude),
            ("agents.codex.command", &self.agents.codex),
            ("agents.pi.command", &self.agents.pi),
        ] {
            if adapter.command.trim().is_empty() {
                bail!("{name} must be non-empty");
            }
            let adapter_bytes =
                adapter.command.len() + adapter.args.iter().map(String::len).sum::<usize>();
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
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".config")))
        .map(|path| path.join("peon/config.toml"))
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
    for forbidden in ["provider", "agents"] {
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
    fn cross_field_validation_accounts_for_json_escaping() {
        let mut config = Config::default();
        config.protocol.max_server_frame_bytes = config.provider_limits.max_assistant_bytes;
        assert!(config.validate().is_err());
    }
}
