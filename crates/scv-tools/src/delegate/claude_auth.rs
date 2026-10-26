//! Programmatic Claude Code settings that pin SCV's billing identity and route.
//!
//! The API-key agent is pinned to `https://allrouter.cc`. The subscription
//! agent is pinned to `https://api.anthropic.com`.

use std::path::Path;

use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

use super::adapters::AgentAuth;

/// `ANTHROPIC_BASE_URL` for the API-key agent. Claude Code's client joins this
/// origin with `/v1/messages` (the same shape as `https://api.anthropic.com`).
const CLAUDE_API_GATEWAY: &str = "https://allrouter.cc";

/// `ANTHROPIC_BASE_URL` for the subscription agent.
const ANTHROPIC_API_ORIGIN: &str = "https://api.anthropic.com";

/// Variables cleared in both Claude settings objects so a parent environment
/// cannot replace the pin or select another credential.
const CLEARED_ENVIRONMENT: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", ""),
    ("ANTHROPIC_AUTH_TOKEN", ""),
    ("ANTHROPIC_CUSTOM_HEADERS", ""),
    ("ANTHROPIC_UNIX_SOCKET", ""),
    ("ANTHROPIC_PROFILE", ""),
    ("CLAUDE_CODE_OAUTH_TOKEN", ""),
    ("CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR", ""),
    ("CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR", ""),
    ("CLAUDE_CODE_USE_BEDROCK", ""),
    ("CLAUDE_CODE_USE_VERTEX", ""),
    ("CLAUDE_CODE_USE_FOUNDRY", ""),
    ("CLAUDE_CODE_USE_ANTHROPIC_AWS", ""),
    ("CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD", ""),
    ("CLAUDE_CODE_USE_MANTLE", ""),
    ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", ""),
];

/// Build the settings object passed at the programmatic settings tier.
pub fn claude_settings(auth: AgentAuth, home: &Path) -> Result<Option<Value>> {
    let (base_url, login, helper) = match auth {
        AgentAuth::ClaudeSubscription => (ANTHROPIC_API_ORIGIN, "claudeai", String::new()),
        AgentAuth::ClaudeApiKey => {
            let path = home.join(".anthropic-api-key");
            let path = path
                .to_str()
                .ok_or_else(|| anyhow!("Claude agent home is not valid UTF-8"))?;
            (
                CLAUDE_API_GATEWAY,
                "console",
                format!("cat -- {}", shell_quote(path)),
            )
        }
        AgentAuth::None => return Ok(None),
    };
    let mut env = Map::new();
    env.insert("ANTHROPIC_BASE_URL".into(), base_url.into());
    for (name, value) in CLEARED_ENVIRONMENT {
        env.insert((*name).into(), (*value).into());
    }
    let mut settings = Map::new();
    settings.insert("forceLoginMethod".into(), login.into());
    settings.insert("apiKeyHelper".into(), helper.into());
    settings.insert("env".into(), Value::Object(env));
    Ok(Some(Value::Object(settings)))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests;
