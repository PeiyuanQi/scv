//! Checking configuration: each value on its own, values against each
//! other, and what a project layer may change.

use anyhow::{Context, Result, bail};

use super::{AgentTransport, ApprovalPolicy, Config, WebSearchMode};

/// A day: long enough for any delegated job, short enough that deadline
/// arithmetic never overflows.
pub(super) const MAX_TOOL_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;
/// Retries multiply provider load and turn latency, so they stay small.
pub(super) const MAX_PROVIDER_RETRIES: usize = 10;
/// The most background agent jobs `agent.max_background` may allow.
pub(super) const MAX_BACKGROUND_JOBS: usize = 16;
/// Longest `[agents.<name>] use_for` note.
pub(super) const MAX_USE_FOR_BYTES: usize = 500;

impl ApprovalPolicy {
    fn strictness(self) -> u8 {
        match self {
            Self::OnRisk => 1,
            Self::Always => 2,
            Self::Never => 3,
        }
    }
}

/// A numeric limit. A project layer may lower it but never raise it, and
/// most limits must also be positive.
pub(super) struct Limit {
    pub(super) name: &'static str,
    pub(super) value: u64,
    pub(super) must_be_positive: bool,
}

/// How many numeric limits [`Config::limits`] lists.
const LIMITS: usize = 35;

impl Config {
    /// Every numeric limit, each named once, so validation and the project
    /// rules check the same list.
    pub(super) fn limits(&self) -> [Limit; LIMITS] {
        fn limit(name: &'static str, value: impl TryInto<u64>) -> Limit {
            Limit {
                name,
                value: value.try_into().unwrap_or(u64::MAX),
                must_be_positive: false,
            }
        }
        fn positive(name: &'static str, value: impl TryInto<u64>) -> Limit {
            Limit {
                must_be_positive: true,
                ..limit(name, value)
            }
        }
        [
            positive("provider.timeout_seconds", self.provider.timeout_seconds),
            positive("agent.max_steps", self.agent.max_steps),
            limit(
                "agent.max_delegation_depth",
                self.agent.max_delegation_depth,
            ),
            positive("agent.max_conversations", self.agent.max_conversations),
            positive(
                "agent.conversation_idle_seconds",
                self.agent.conversation_idle_seconds,
            ),
            limit("agent.max_background", self.agent.max_background),
            positive("session.max_history_bytes", self.session.max_history_bytes),
            positive("session.max_messages", self.session.max_messages),
            positive("context.max_tokens", self.context.max_tokens),
            positive("context.bytes_per_token", self.context.bytes_per_token),
            positive("context.summary_max_chars", self.context.summary_max_chars),
            positive(
                "tools.command_timeout_seconds",
                self.tools.command_timeout_seconds,
            ),
            positive(
                "tools.agent_timeout_seconds",
                self.tools.agent_timeout_seconds,
            ),
            positive("tools.max_timeout_seconds", self.tools.max_timeout_seconds),
            positive("tools.output_limit_bytes", self.tools.output_limit_bytes),
            positive("tools.max_read_bytes", self.tools.max_read_bytes),
            positive("tools.max_write_bytes", self.tools.max_write_bytes),
            positive(
                "protocol.max_client_frame_bytes",
                self.protocol.max_client_frame_bytes,
            ),
            positive(
                "protocol.max_server_frame_bytes",
                self.protocol.max_server_frame_bytes,
            ),
            positive("tui.max_transcript_bytes", self.tui.max_transcript_bytes),
            positive("tui.max_transcript_items", self.tui.max_transcript_items),
            positive(
                "tui.max_prompt_history_bytes",
                self.tui.max_prompt_history_bytes,
            ),
            positive(
                "tui.max_prompt_history_items",
                self.tui.max_prompt_history_items,
            ),
            positive(
                "provider_limits.max_sse_event_bytes",
                self.provider_limits.max_sse_event_bytes,
            ),
            positive(
                "provider_limits.max_response_bytes",
                self.provider_limits.max_response_bytes,
            ),
            positive(
                "provider_limits.max_assistant_bytes",
                self.provider_limits.max_assistant_bytes,
            ),
            positive(
                "provider_limits.max_tool_calls",
                self.provider_limits.max_tool_calls,
            ),
            positive(
                "provider_limits.max_tool_arguments_bytes",
                self.provider_limits.max_tool_arguments_bytes,
            ),
            limit(
                "provider_limits.max_retries",
                self.provider_limits.max_retries,
            ),
            positive("skills.max_skills", self.skills.max_skills),
            positive("skills.max_skill_bytes", self.skills.max_skill_bytes),
            positive("web.fetch_max_bytes", self.web.fetch_max_bytes),
            positive("web.fetch_timeout_seconds", self.web.fetch_timeout_seconds),
            limit("web.max_redirects", self.web.max_redirects),
            positive("web.max_search_results", self.web.max_search_results),
        ]
    }
}

impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        for account in &self.notify.owner {
            let valid = account.split_once(':').is_some_and(|(channel, name)| {
                !channel.is_empty()
                    && !name.is_empty()
                    && account
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
            });
            if !valid {
                bail!(
                    "notify.owner entries must look like \"feishu:default\" (<channel>:<account>)"
                );
            }
        }
        if self.provider.kind != "openai-compatible" {
            bail!("provider.kind must be openai-compatible");
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
        for (channel, accounts) in &self.channels {
            let known = [scv_clawbot::CHANNEL, scv_feishu::CHANNEL];
            if !known.contains(&channel.as_str()) {
                bail!(
                    "unknown channel [channels.{channel}]; known channels are {}",
                    known.join(", ")
                );
            }
            for (account, settings) in accounts {
                scv_channels::state::validate_name(account).with_context(|| {
                    format!("[channels.{channel}.{account}] has an invalid account name")
                })?;
                if settings
                    .workspace
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
                {
                    bail!("channels.{channel}.{account}.workspace must be an absolute path");
                }
            }
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
            if let Some(use_for) = &adapter.use_for
                && (use_for.trim().is_empty()
                    || use_for.len() > MAX_USE_FOR_BYTES
                    || use_for.chars().any(char::is_control))
            {
                bail!(
                    "agents.{agent}.use_for must be one non-empty line of at most \
                     {MAX_USE_FOR_BYTES} bytes"
                );
            }
            if let Some(model) = &adapter.model {
                if !scv_tools::valid_model_name(model) {
                    bail!("agents.{agent}.model is not a valid model name");
                }
                if adapter.model_args.is_empty() && agent != "scv" {
                    bail!("agents.{agent}.model is set but {agent} does not offer model selection");
                }
            }
            if let Some(effort) = &adapter.effort {
                if !scv_tools::valid_effort(effort) {
                    bail!(
                        "agents.{agent}.effort must be one of {}",
                        scv_tools::AGENT_EFFORTS.join(", ")
                    );
                }
                if adapter.effort_args.is_empty() {
                    bail!(
                        "agents.{agent}.effort is set but {agent} does not offer effort selection"
                    );
                }
            }
            if adapter.transport == AgentTransport::Acp
                && scv_tools::adapters::adapter(agent)
                    .is_some_and(|descriptor| descriptor.acp.is_none())
            {
                bail!(
                    "agents.{agent}.transport = \"acp\" but {agent} has no verified ACP server; \
                     use \"auto\" or \"resume\""
                );
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
        if let Some(limit) = self
            .limits()
            .into_iter()
            .find(|limit| limit.must_be_positive && limit.value == 0)
        {
            bail!("{} must be positive", limit.name);
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
        if self.agent.max_background > MAX_BACKGROUND_JOBS {
            bail!("agent.max_background must be at most {MAX_BACKGROUND_JOBS}");
        }
        for agent in &self.agent.prefer {
            if scv_tools::adapters::adapter(agent).is_none() {
                bail!("agent.prefer names unknown agent {agent:?}");
            }
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
        self.validate_web()?;
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

impl Config {
    fn validate_web(&self) -> Result<()> {
        let web = &self.web;
        if web.fetch_max_bytes > 64 * 1024 * 1024 {
            bail!("web.fetch_max_bytes must be at most 67108864");
        }
        if web.fetch_timeout_seconds > self.tools.max_timeout_seconds {
            bail!("web.fetch_timeout_seconds exceeds tools.max_timeout_seconds");
        }
        if web.max_redirects > 10 {
            bail!("web.max_redirects must be at most 10");
        }
        if web.max_search_results > 20 {
            bail!("web.max_search_results must be at most 20");
        }
        if web.auto_approve_domains.len() > 256 {
            bail!("web.auto_approve_domains may list at most 256 hosts");
        }
        if let Some(entry) = web
            .auto_approve_domains
            .iter()
            .find(|entry| !valid_domain_pattern(entry))
        {
            bail!(
                "web.auto_approve_domains entry {entry:?} must be a host name such as docs.rs or *.example.com"
            );
        }
        let http_url = |value: &str| value.starts_with("https://") || value.starts_with("http://");
        if !http_url(&web.brave_url) {
            bail!("web.brave_url must be an http or https URL");
        }
        match web.search {
            WebSearchMode::Searxng if !web.searxng_url.as_deref().is_some_and(http_url) => {
                bail!("web.search = \"searxng\" requires web.searxng_url (an http or https URL)");
            }
            WebSearchMode::Brave
                if web.brave_api_key.as_deref().unwrap_or("").trim().is_empty()
                    && web
                        .brave_api_key_env
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .is_empty() =>
            {
                bail!("web.search = \"brave\" requires web.brave_api_key or web.brave_api_key_env");
            }
            _ => {}
        }
        Ok(())
    }
}

/// A host name, optionally prefixed with `*.` to match its subdomains.
pub(super) fn valid_domain_pattern(entry: &str) -> bool {
    let host = entry.strip_prefix("*.").unwrap_or(entry);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

pub(super) fn validate_project_keys(value: &toml::Value) -> Result<()> {
    let Some(table) = value.as_table() else {
        bail!("project configuration must be a TOML table");
    };
    for forbidden in [
        "provider",
        "providers",
        "provider_active",
        "agents",
        "update",
        "channels",
        "notify",
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
    if table
        .get("agent")
        .and_then(toml::Value::as_table)
        .is_some_and(|agent| agent.contains_key("prefer"))
    {
        bail!("project configuration cannot set agent.prefer");
    }
    if let Some(web) = table.get("web").and_then(toml::Value::as_table) {
        for key in [
            "auto_approve_domains",
            "allow_private_addresses",
            "searxng_url",
            "brave_url",
            "brave_api_key",
            "brave_api_key_env",
        ] {
            if web.contains_key(key) {
                bail!("project configuration cannot set web.{key}");
            }
        }
    }
    Ok(())
}

pub(super) fn validate_project_not_weaker(user: &Config, project: &Config) -> Result<()> {
    for (user_limit, project_limit) in user.limits().into_iter().zip(project.limits()) {
        if project_limit.value > user_limit.value {
            bail!("project configuration cannot raise {}", user_limit.name);
        }
    }
    if project.context.reserve_output_tokens < user.context.reserve_output_tokens
        || project.context.safety_margin_tokens < user.context.safety_margin_tokens
    {
        bail!("project configuration cannot lower context reserves");
    }
    if project.tools.approval_policy.strictness() < user.tools.approval_policy.strictness() {
        bail!("project configuration cannot weaken tools.approval_policy");
    }
    if project.skills.scan_projects && !user.skills.scan_projects {
        bail!("project configuration cannot enable skills.scan_projects");
    }
    if project.web.enabled && !user.web.enabled {
        bail!("project configuration cannot enable web");
    }
    if project.web.search != user.web.search && project.web.search != WebSearchMode::Off {
        bail!("project configuration can only turn web.search off");
    }
    Ok(())
}
