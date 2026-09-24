//! Turning configuration into what the runtime, tools, and provider take.

use std::{collections::HashMap, ffi::OsString, time::Duration};

use anyhow::{Context, Result};
use scv_client::Layout;
use scv_core::{AgentConfig as CoreAgentConfig, HistoryLimits};
use scv_provider_openai::ProviderLimits;
use scv_tools::{
    AgentAdapterConfig, ToolsConfig,
    conversation::ConversationLimits,
    web::{SearchBackend, WebToolsConfig},
};

use super::{AgentPermissions, AgentTransport, Config, WebSearchMode, load::ensure_private_dir};

impl Config {
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
            max_delegation_depth: self.agent.max_delegation_depth,
            conversations: ConversationLimits {
                max: self.agent.max_conversations,
                idle: Duration::from_secs(self.agent.conversation_idle_seconds),
            },
            delegation: None,
            max_background: self.agent.max_background,
            background: None,
            chat_attach: None,
        }
    }

    /// Web tool settings for a tool-enabled session, or `None` when disabled.
    /// A Brave backend without a key is left out rather than failing the session.
    pub fn web_tools(&self) -> Option<WebToolsConfig> {
        if !self.web.enabled {
            return None;
        }
        let search = match self.web.search {
            WebSearchMode::Off | WebSearchMode::Provider => None,
            WebSearchMode::Searxng => self
                .web
                .searxng_url
                .clone()
                .map(|url| SearchBackend::Searxng { url }),
            WebSearchMode::Brave => {
                let api_key = self
                    .web
                    .brave_api_key
                    .clone()
                    .or_else(|| {
                        self.web
                            .brave_api_key_env
                            .as_deref()
                            .and_then(|name| std::env::var(name).ok())
                    })
                    .filter(|key| !key.trim().is_empty());
                if api_key.is_none() {
                    tracing::warn!(
                        "web.search is \"brave\" but no Brave API key is configured; web_search is unavailable"
                    );
                }
                api_key.map(|api_key| SearchBackend::Brave {
                    url: self.web.brave_url.clone(),
                    api_key,
                })
            }
        };
        Some(WebToolsConfig {
            fetch_max_bytes: self.web.fetch_max_bytes,
            fetch_timeout: Duration::from_secs(self.web.fetch_timeout_seconds),
            max_redirects: self.web.max_redirects,
            auto_approve_domains: self.web.auto_approve_domains.clone(),
            allow_private_addresses: self.web.allow_private_addresses,
            search,
            max_search_results: self.web.max_search_results,
            output_limit: self.tools.output_limit_bytes,
        })
    }

    /// Whether to offer the provider's hosted web search to tool-enabled sessions.
    pub fn hosted_web_search(&self) -> bool {
        self.web.enabled && self.web.search == WebSearchMode::Provider
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
                let adapter_home = self.layout().agent_home(name);
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
                        output: descriptor.output,
                        resume: descriptor.resume,
                        home: Some(adapter_home),
                        transport: descriptor.transport,
                        acp: descriptor
                            .acp
                            .filter(|_| match config.transport {
                                AgentTransport::Acp => true,
                                AgentTransport::Resume => false,
                                // A custom `command` points SCV at a specific
                                // CLI, which the ACP server would not run.
                                AgentTransport::Auto => config.command == descriptor.command,
                            })
                            .map(|launch| scv_tools::AcpAgentLaunch {
                                command: launch.command.to_owned(),
                                args: scv_tools::adapters::acp_args(&launch, full),
                                full_mode: launch.full_mode.filter(|_| full).map(str::to_owned),
                                environment: launch
                                    .full_environment
                                    .iter()
                                    .filter(|_| full)
                                    .map(|(variable, value)| {
                                        (OsString::from(variable), OsString::from(value))
                                    })
                                    .collect(),
                                required: config.transport == AgentTransport::Acp,
                            }),
                        use_for: config.use_for.clone(),
                    },
                ))
            })
            .collect()
    }

    /// Where this instance keeps everything.
    pub fn layout(&self) -> Layout {
        Layout::new(&self.instance_home)
    }

    pub fn prepare_adapter_homes(&self) -> Result<()> {
        let layout = self.layout();
        std::fs::create_dir_all(layout.agents()).context("create the agent homes directory")?;
        ensure_private_dir(&layout.agents())?;
        for name in self.agents.0.keys() {
            let path = layout.agent_home(name);
            std::fs::create_dir_all(&path)
                .with_context(|| format!("create isolated {name} agent home"))?;
            ensure_private_dir(&path)
                .with_context(|| format!("secure isolated {name} agent home"))?;
        }
        Ok(())
    }
}
