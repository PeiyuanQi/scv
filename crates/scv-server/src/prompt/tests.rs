//! Unit tests for `src/prompt/mod.rs`.

use super::*;
use crate::session::valid_channel_name;

fn prompt_for(config: &Config, context: &PromptContext<'_>) -> String {
    let workspace = tempfile::tempdir().unwrap();
    let listings = SkillListings {
        listing: String::new(),
        project_listing: String::new(),
    };
    build_system_prompt(workspace.path(), config, &listings, context).unwrap()
}

#[test]
fn the_prompt_teaches_delegate_first_only_when_agents_can_run_in_the_background() {
    let mut config = Config::default();
    config.agent.prefer = vec!["pi".into(), "codex".into(), "grok".into()];
    let agents = ["agent_codex".to_owned(), "agent_grok".to_owned()];
    let prompt = prompt_for(
        &config,
        &PromptContext {
            agents: &agents,
            background: true,
            channel: None,
        },
    );
    assert!(prompt.starts_with(&config.agent.system_prompt), "{prompt}");
    assert!(
        prompt.contains("agent_codex (Codex), agent_grok (Grok Build)"),
        "{prompt}"
    );
    // Preferences name only offered agents, in the user's order.
    assert!(
        prompt.contains("The user prefers agent_codex, agent_grok, in that order"),
        "{prompt}"
    );
    assert!(prompt.contains("background set to true"), "{prompt}");
    assert!(prompt.contains("job handle"), "{prompt}");
    assert!(prompt.contains("agent_cancel"), "{prompt}");
    assert!(prompt.contains("[SCV background report]"), "{prompt}");
    assert!(!prompt.contains("# Chat channel"), "{prompt}");
    // Grok is offered, so a refusal is not a reason to skip it.
    assert!(
        prompt.contains(
            "If an agent declines a request, including a safety or guardrail refusal, tell \
                 the user what it said and call agent_grok with the same request. A refusal \
                 from another agent is not a reason to skip Grok."
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "If agent_grok itself declines, tell the user; don't pass the request to another \
                 agent on your own unless the user names one."
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("a preferred one is unavailable"),
        "{prompt}"
    );
    // Calm guidance: no shouted rules.
    for loud in ["CRITICAL", "MUST", "IMPORTANT", "NEVER"] {
        assert!(!prompt.contains(loud), "{loud} in {prompt}");
    }

    let foreground = prompt_for(
        &Config::default(),
        &PromptContext {
            agents: &agents,
            background: false,
            channel: None,
        },
    );
    assert!(foreground.contains("Hand substantial work to an agent"));
    assert!(foreground.contains("call agent_grok with the same request"));
    assert!(!foreground.contains("background set to true"));
    assert!(!foreground.contains("prefers"));

    let tool_free = prompt_for(
        &Config::default(),
        &PromptContext {
            agents: &[],
            background: false,
            channel: None,
        },
    );
    assert!(!tool_free.contains("# Delegating work"), "{tool_free}");
}

#[test]
fn the_prompt_names_per_agent_task_defaults() {
    let mut config = Config::default();
    config.agents.0.get_mut("claude").unwrap().use_for = Some("coding".into());
    config.agents.0.get_mut("claude").unwrap().model = Some("opus-5.5".into());
    config.agents.0.get_mut("claude").unwrap().effort = Some("xhigh".into());
    config.agents.0.get_mut("grok").unwrap().use_for =
        Some("current events, and anything that needs posts on X".into());
    let agents = ["agent_claude".to_owned(), "agent_grok".to_owned()];
    let prompt = prompt_for(
        &config,
        &PromptContext {
            agents: &agents,
            background: false,
            channel: None,
        },
    );
    assert!(
        prompt.starts_with("You are SCV, a concise and careful agent."),
        "{prompt}"
    );
    assert!(
        prompt.contains("For coding, prefer agent_claude with model opus-5.5 and effort xhigh."),
        "{prompt}"
    );
    assert!(
        prompt
            .contains("For current events, and anything that needs posts on X, prefer agent_grok."),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "When the work does not match a note, omit model and effort so the agent uses \
             its own default."
        ),
        "{prompt}"
    );
}

#[test]
fn chat_sessions_are_told_their_channel_and_how_replies_are_read() {
    let agents = ["agent_claude".to_owned()];
    let owner = prompt_for(
        &Config::default(),
        &PromptContext {
            agents: &agents,
            background: true,
            channel: Some("WeChat"),
        },
    );
    assert!(owner.contains("# Chat channel"), "{owner}");
    assert!(owner.contains("takes place on WeChat"), "{owner}");
    assert!(owner.contains("plain text"), "{owner}");
    assert!(owner.contains("never see your tool calls"), "{owner}");
    assert!(owner.contains("# Delegating work"), "{owner}");
    // Claude only: a refusal goes back to the user.
    assert!(
        owner.contains(
            "If an agent declines a request, tell the user what it said; don't pass the \
                 request to another agent on your own."
        ),
        "{owner}"
    );
    assert!(!owner.contains("call agent_grok"), "{owner}");
    // A tool-free chat session still learns how its replies are read.
    let guest = prompt_for(
        &Config::default(),
        &PromptContext {
            agents: &[],
            background: false,
            channel: Some("Feishu"),
        },
    );
    assert!(guest.contains("takes place on Feishu"), "{guest}");
    assert!(!guest.contains("# Delegating work"), "{guest}");
    assert!(valid_channel_name("Lark"));
    for bad in [
        "",
        "  ",
        "We\nChat",
        &"x".repeat(scv_protocol::MAX_CHANNEL_NAME_BYTES + 1),
    ] {
        assert!(!valid_channel_name(bad), "{bad:?}");
    }
}
