//! Programmatic Claude Code settings that pin SCV's billing identity and route.

use std::path::Path;

use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

use super::adapters::AgentAuth;

const PINNED_ENVIRONMENT: &[(&str, &str)] = &[
    ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
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
    let (AgentAuth::ClaudeApiKey | AgentAuth::ClaudeSubscription) = auth else {
        return Ok(None);
    };
    let mut env = Map::new();
    for (name, value) in PINNED_ENVIRONMENT {
        env.insert((*name).into(), (*value).into());
    }
    let mut settings = Map::new();
    match auth {
        AgentAuth::ClaudeSubscription => {
            settings.insert("forceLoginMethod".into(), "claudeai".into());
            settings.insert("apiKeyHelper".into(), "".into());
        }
        AgentAuth::ClaudeApiKey => {
            settings.insert("forceLoginMethod".into(), "console".into());
            let path = home
                .join(".anthropic-api-key")
                .canonicalize()
                .unwrap_or_else(|_| home.join(".anthropic-api-key"));
            let path = path
                .to_str()
                .ok_or_else(|| anyhow!("Claude agent home is not valid UTF-8"))?;
            settings.insert(
                "apiKeyHelper".into(),
                format!("cat -- {}", shell_quote(path)).into(),
            );
        }
        AgentAuth::None => unreachable!(),
    }
    settings.insert("env".into(), Value::Object(env));
    Ok(Some(Value::Object(settings)))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests;
