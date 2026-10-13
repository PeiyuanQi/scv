//! SCV's configuration: the schema of `config.toml` ([`schema`]), how the
//! layers are read and merged ([`load`]), what each value must satisfy and
//! what a project may change ([`validate`]), and the settings the runtime,
//! tools, and provider are built from ([`runtime`]).
//!
//! The `scv` command line reads it too: `scv config show` lists every
//! [`Setting`] with its origin, and `scv agents` finds the agent homes.

mod load;
mod runtime;
mod schema;
mod validate;

pub use load::{Setting, read_layer};
pub use schema::*;

use anyhow::Result;
use scv_client::Layout;

/// The instance a server process serves and the overrides its command line
/// chose. Every configuration the process loads starts from these.
#[derive(Debug, Clone)]
pub(crate) struct Instance {
    pub(crate) layout: Layout,
    pub(crate) overrides: ConfigOverrides,
}

impl Instance {
    /// The configuration files alone: the explicit layer, without the
    /// command line's provider, model, or policy flags.
    fn files(&self) -> ConfigOverrides {
        ConfigOverrides {
            config_file: self.overrides.config_file.clone(),
            ..ConfigOverrides::default()
        }
    }

    /// The configuration files as a session in `workspace` reads them.
    pub(crate) fn load(&self, workspace: &std::path::Path) -> Result<Config> {
        Config::load(&self.layout, workspace, self.files())
    }

    /// The user's configuration files, without a project layer.
    pub(crate) fn load_user(&self) -> Result<Config> {
        Config::load_user(&self.layout, self.files())
    }
}

/// Warnings for Claude Code billing migrations and machine-wide policy.
fn claude_warnings_for_layout(layout: &Layout) -> Vec<String> {
    let mut warnings = Vec::new();
    let agents = std::fs::read_to_string(layout.config())
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|value| value.get("agents").cloned())
        .and_then(|value| value.as_table().cloned());
    let explicit_claude = agents
        .as_ref()
        .is_some_and(|agents| agents.contains_key("claude"));
    let explicit_claude_ai = agents
        .as_ref()
        .is_some_and(|agents| agents.contains_key("claude-ai"));
    if explicit_claude
        && !explicit_claude_ai
        && scv_tools::stores::claude_status(
            scv_tools::adapters::AgentAuth::ClaudeSubscription,
            &layout.agent_home("claude"),
        )
        .is_ok_and(|status| status.ready)
    {
        warnings.push(
            "claude now bills an API key and claude-ai uses the Claude subscription. agents/claude holds a Claude subscription login, so claude will not run. To keep using the subscription, move agents/claude to agents/claude-ai and [agents.claude] to [agents.claude-ai] (see docs/configuration.md).".into(),
        );
    }
    if std::path::Path::new("/etc/claude-code/managed-settings.json").is_file() {
        warnings.push(
            "machine policy /etc/claude-code/managed-settings.json outranks SCV's pinned Claude Code credentials for claude and claude-ai".into(),
        );
    }
    warnings
}

impl Config {
    pub fn claude_warnings(&self) -> Vec<String> {
        claude_warnings_for_layout(&Layout::new(&self.instance_home))
    }
}

impl Instance {
    pub(crate) fn claude_warnings(&self) -> Vec<String> {
        claude_warnings_for_layout(&self.layout)
    }
}

#[cfg(test)]
mod tests;
