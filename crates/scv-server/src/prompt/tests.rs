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
    let agents = ["codex".to_owned(), "grok".to_owned()];
    let prompt = prompt_for(
        &config,
        &PromptContext {
            agents: &agents,
            background: true,
            channel: None,
        },
    );
    assert!(prompt.starts_with(&config.agent.system_prompt), "{prompt}");
    // One tool, with the agents named by their `agent` values.
    assert!(
        prompt.contains(
            "You can hand work to other agents with the agent tool, naming one in its agent \
             argument: codex (Codex), grok (Grok Build)."
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("agent_codex"), "{prompt}");
    // Preferences name only offered agents, in the user's order, and the
    // first of them is the default.
    assert!(
        prompt.contains("The user prefers codex, grok, in that order"),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "A call that names no agent goes to codex, so name one whenever the work calls \
             for another."
        ),
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
                 the user what it said and call the agent tool with agent grok and the same \
                 request. A refusal from another agent is not a reason to skip Grok."
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "If grok itself declines, tell the user; don't pass the request to another agent \
                 on your own unless the user names one."
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
    assert!(foreground.contains("call the agent tool with agent grok and the same request"));
    assert!(!foreground.contains("background set to true"));
    assert!(!foreground.contains("prefers"));
    assert!(!foreground.contains("names no agent"));

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
    let agents = ["claude".to_owned(), "grok".to_owned()];
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
        prompt.contains("For coding, prefer claude with model opus-5.5 and effort xhigh."),
        "{prompt}"
    );
    assert!(
        prompt.contains("For current events, and anything that needs posts on X, prefer grok."),
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
fn defaults_without_a_note_apply_whenever_that_agent_runs() {
    let mut config = Config::default();
    config.agents.0.get_mut("codex").unwrap().model = Some("gpt-5.5".into());
    // A preference for an agent this session does not offer is left out.
    config.agent.prefer = vec!["claude".into()];
    let agents = ["codex".to_owned()];
    let prompt = prompt_for(
        &config,
        &PromptContext {
            agents: &agents,
            background: true,
            channel: None,
        },
    );
    assert!(
        prompt.contains(
            "When delegating to codex, pass model gpt-5.5 unless the user asks for another."
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("prefers"), "{prompt}");
    assert!(!prompt.contains("omit model and effort"), "{prompt}");
}

#[test]
fn chat_sessions_are_told_their_channel_and_how_replies_are_read() {
    let agents = ["claude".to_owned()];
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
    assert!(!owner.contains("agent grok"), "{owner}");
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
