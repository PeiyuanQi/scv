use std::path::Path;

use serde_json::json;

use super::*;

#[test]
fn api_key_settings_quote_the_private_path() {
    let value = claude_settings(AgentAuth::ClaudeApiKey, Path::new("/tmp/home with 'quote'"))
        .unwrap()
        .unwrap();
    assert_eq!(
        value["apiKeyHelper"],
        "cat -- '/tmp/home with '\\''quote'\\''/.anthropic-api-key'"
    );
    assert_eq!(
        value["env"]["ANTHROPIC_BASE_URL"],
        "https://api.anthropic.com"
    );
    assert_eq!(value["env"]["ANTHROPIC_API_KEY"], "");
}

#[test]
fn subscription_settings_pin_claude_ai() {
    assert_eq!(
        claude_settings(AgentAuth::ClaudeSubscription, Path::new("/tmp/home"))
            .unwrap()
            .unwrap(),
        json!({
            "apiKeyHelper": "",
            "env": {
                "ANTHROPIC_API_KEY": "",
                "ANTHROPIC_AUTH_TOKEN": "",
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_CUSTOM_HEADERS": "",
                "ANTHROPIC_PROFILE": "",
                "ANTHROPIC_UNIX_SOCKET": "",
                "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR": "",
                "CLAUDE_CODE_OAUTH_TOKEN": "",
                "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR": "",
                "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST": "",
                "CLAUDE_CODE_USE_ANTHROPIC_AWS": "",
                "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD": "",
                "CLAUDE_CODE_USE_BEDROCK": "",
                "CLAUDE_CODE_USE_FOUNDRY": "",
                "CLAUDE_CODE_USE_MANTLE": "",
                "CLAUDE_CODE_USE_VERTEX": "",
            },
            "forceLoginMethod": "claudeai"
        })
    );
}
