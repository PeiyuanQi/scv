//! Unit tests for `src/delegate/options.rs`.

use std::{
    os::unix::fs::PermissionsExt as _,
    path::Path,
    time::{Duration, SystemTime},
};

use serde_json::json;

use super::*;

/// A `session/new` result shaped like claude-agent-acp's.
fn session() -> Value {
    json!({
        "sessionId": "s1",
        "configOptions": [
            {"id": "mode", "currentValue": "default", "options": [{"value": "default"}, {"value": "bypassPermissions"}]},
            {"id": "model", "currentValue": "default", "options": [
                {"value": "default"}, {"value": "opus[1m]"}, {"value": "sonnet"},
                {"value": "--dangerous"}, {"value": "@file"}, {"value": "two words"}
            ]},
            {"id": "effort", "currentValue": "default", "options": [
                {"value": "default"}, {"value": "low"}, {"value": "xhigh"}, {"value": "x; rm"}, {"value": "-x"}
            ]}
        ]
    })
}

fn executable(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("agent-acp");
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    path
}

#[test]
fn a_session_lists_the_model_and_effort_values_that_can_be_passed() {
    let options = AgentOptions::from_acp(&session()).unwrap();
    let model = options.model.unwrap();
    assert_eq!(model.values, ["default", "opus[1m]", "sonnet"]);
    assert_eq!(model.shown(), ["opus[1m]", "sonnet"]);
    assert_eq!(model.current.as_deref(), Some("default"));
    assert_eq!(model.named_default(), None, "`default` names no model");
    assert!(model.offers("opus[1m]") && model.offers("default"));
    assert!(!model.offers("opus"));
    let effort = options.effort.unwrap();
    assert_eq!(effort.values, ["default", "low", "xhigh"]);
}

#[test]
fn codex_style_ids_and_named_defaults_are_read() {
    let options = AgentOptions::from_acp(&json!({"configOptions": [
        {"id": "model", "currentValue": "gpt-5.6-sol", "options": [{"value": "gpt-5.6-sol"}, {"value": "gpt-5.5"}]},
        {"id": "reasoning_effort", "currentValue": "xhigh", "options": [{"value": "high"}, {"value": "xhigh"}, {"value": "ultra"}]}
    ]}))
    .unwrap();
    assert_eq!(options.model.unwrap().named_default(), Some("gpt-5.6-sol"));
    assert_eq!(options.effort.unwrap().shown(), ["high", "xhigh", "ultra"]);
}

#[test]
fn a_session_without_model_or_effort_choices_offers_nothing() {
    assert_eq!(AgentOptions::from_acp(&json!({"sessionId": "s1"})), None);
    assert_eq!(
        AgentOptions::from_acp(&json!({"configOptions": [
            {"id": "mode", "options": [{"value": "default"}]},
            {"id": "model", "options": [{"value": "--bad"}]}
        ]})),
        None
    );
}

#[test]
fn saved_options_load_only_for_the_same_agent_install_and_while_recent() {
    let dir = tempfile::tempdir().unwrap();
    let server = executable(dir.path());
    let file = dir.path().join("state/agent-options/claude.json");
    let options = AgentOptions::from_acp(&session()).unwrap();
    save(&file, "claude", &server, &options).unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);

    let now = SystemTime::now();
    let listed = load(&file, "claude", &server, now).unwrap();
    assert_eq!(listed.options, options);
    assert!(listed.fresh(now));
    assert!(!listed.fresh(now + MAX_AGE + Duration::from_secs(60)));
    assert_eq!(load(&file, "codex", &server, now), None, "another agent");
    assert_eq!(
        load(
            &file,
            "claude",
            &server,
            now + MAX_AGE + Duration::from_secs(60)
        ),
        None,
        "too old"
    );
    let (read, seen) = read_saved(&file, "claude").unwrap();
    assert_eq!(read, options);
    assert!(seen > 0);

    // An update changes the server's file: the values may have changed too.
    std::fs::write(&server, "#!/bin/sh\n# a newer release\n").unwrap();
    assert_eq!(load(&file, "claude", &server, now), None, "another install");
}

#[test]
fn a_saved_file_is_checked_again_when_read() {
    let dir = tempfile::tempdir().unwrap();
    let server = executable(dir.path());
    // The writer keeps the directory and its parent (`state/`) private.
    let file = dir.path().join("state/agent-options/claude.json");
    save(&file, "claude", &server, &AgentOptions::default()).unwrap();
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    saved["model"] = json!({"values": ["sonnet", "--flag", "@x"], "current": "--flag"});
    std::fs::write(&file, serde_json::to_vec(&saved).unwrap()).unwrap();
    let model = load(&file, "claude", &server, SystemTime::now())
        .unwrap()
        .options
        .model
        .unwrap();
    assert_eq!(model.values, ["sonnet"]);
    assert_eq!(model.current, None);

    std::fs::write(&file, vec![b' '; 70 * 1024]).unwrap();
    assert_eq!(read_saved(&file, "claude"), None, "oversized");
    std::fs::write(&file, "not json").unwrap();
    assert_eq!(read_saved(&file, "claude"), None, "malformed");
}

/// A `session/new` result as DeepSeek Harness 0.1.7-rc.1 sends it, with a
/// gateway route beside the official one: models grouped by provider, each
/// value a JSON `["provider","model"]` pair.
fn dsh_session() -> Value {
    json!({
        "sessionId": "s1",
        "configOptions": [
            {"id": "model", "name": "Model", "category": "model", "type": "select",
             "currentValue": "[\"deepseek-official\",\"deepseek-v4-flash\"]",
             "options": [
                {"group": "deepseek-official", "name": "DeepSeek", "options": [
                    {"value": "[\"deepseek-official\",\"deepseek-v4-flash\"]", "name": "deepseek-v4-flash"},
                    {"value": "[\"deepseek-official\",\"deepseek-v4-pro\"]", "name": "DeepSeek-V4-Pro"}
                ]},
                {"group": "xubao", "name": "Xubao", "options": [
                    {"value": "[\"xubao\",\"glm-5.3\"]", "name": "GLM 5.3 (Xubao)"}
                ]}
             ]},
            {"id": "reasoning_effort", "name": "Reasoning effort", "category": "thought_level",
             "type": "select", "currentValue": "high", "options": [
                {"value": "off"}, {"value": "low"}, {"value": "high"}, {"value": "max"}
            ]}
        ]
    })
}

#[test]
fn grouped_values_and_provider_model_pairs_are_listed_as_provider_slash_model() {
    let options = AgentOptions::from_acp(&dsh_session()).unwrap();
    let model = options.model.unwrap();
    assert_eq!(
        model.values,
        [
            "deepseek-official/deepseek-v4-flash",
            "deepseek-official/deepseek-v4-pro",
            "xubao/glm-5.3"
        ]
    );
    assert_eq!(
        model.named_default(),
        Some("deepseek-official/deepseek-v4-flash")
    );
    assert_eq!(
        options.effort.unwrap().shown(),
        ["off", "low", "high", "max"]
    );
    // Each shown value keeps the agent's own spelling to send.
    let values = select_values(&dsh_session()["configOptions"][0]);
    assert_eq!(values[2].shown, "xubao/glm-5.3");
    assert_eq!(values[2].value, r#"["xubao","glm-5.3"]"#);
    // Anything else is shown as the agent spells it.
    for plain in ["opus[1m]", "[]", "[\"\"]", "[1,2]", "not json ["] {
        assert_eq!(shown(plain), plain);
    }
}
