//! Unit tests for `src/registry.rs`.

use std::{collections::HashMap, path::Path};

use super::*;
use crate::{
    AgentDefaults,
    delegate::{
        adapters::{OutputFormat, Resume},
        records::DelegationRegistry,
    },
};

fn delegation_context(home: &Path) -> DelegationContext {
    DelegationContext {
        registry: Arc::new(DelegationRegistry::for_test(&scv_client::Layout::new(home))),
        session: "session-1".into(),
        depth: 0,
    }
}

#[test]
fn uninstalled_agents_are_not_offered() {
    let home = tempfile::tempdir().unwrap();
    let config = ToolsConfig {
        delegation: Some(delegation_context(home.path())),
        ..ToolsConfig::default()
    };
    let adapter = |command: &str| AgentAdapterConfig {
        command: command.into(),
        args: Vec::new(),
        prompt_args: Vec::new(),
        full_permission_args: None,
        model_args: Vec::new(),
        effort_args: Vec::new(),
        model_hint: String::new(),
        environment: Vec::new(),
        search_dirs: Vec::new(),
        output: OutputFormat::Text,
        resume: Resume::Unsupported,
        home: None,
        transport: Transport::Process,
        acp: None,
        use_for: None,
        defaults: AgentDefaults::default(),
        options_file: None,
        busy: crate::BusyConfig::default(),
    };
    let registry = builtin_registry(
        config.clone(),
        SkillMap::new(),
        Vec::new(),
        1024,
        HashMap::from([
            ("present".to_owned(), adapter("bash")),
            (
                "missing".to_owned(),
                adapter("scv-test-agent-that-is-not-installed"),
            ),
        ]),
    )
    .unwrap();
    assert_eq!(crate::offered_agents(&registry), ["present"]);
    // No agent installed: no agent tool and no job tools.
    let registry = builtin_registry(
        config,
        SkillMap::new(),
        Vec::new(),
        1024,
        HashMap::from([(
            "missing".to_owned(),
            adapter("scv-test-agent-that-is-not-installed"),
        )]),
    )
    .unwrap();
    for tool in ["agent", "agent_wait", "agent_status", "agent_cancel"] {
        assert!(registry.get(tool).is_none(), "{tool} offered");
    }
    assert!(registry.get("bash").is_some());
}

#[test]
fn agents_are_not_offered_at_the_delegation_depth_limit() {
    let adapter = AgentAdapterConfig {
        command: "bash".into(),
        args: Vec::new(),
        prompt_args: Vec::new(),
        full_permission_args: None,
        model_args: Vec::new(),
        effort_args: Vec::new(),
        model_hint: String::new(),
        environment: Vec::new(),
        search_dirs: Vec::new(),
        output: OutputFormat::Text,
        resume: Resume::Unsupported,
        home: None,
        transport: Transport::Process,
        acp: None,
        use_for: None,
        defaults: AgentDefaults::default(),
        options_file: None,
        busy: crate::BusyConfig::default(),
    };
    let home = tempfile::tempdir().unwrap();
    for (max_depth, offered) in [(0, false), (1, true)] {
        let registry = builtin_registry(
            ToolsConfig {
                max_delegation_depth: max_depth,
                delegation: Some(delegation_context(home.path())),
                ..ToolsConfig::default()
            },
            SkillMap::new(),
            Vec::new(),
            1024,
            HashMap::from([("claude".to_owned(), adapter.clone())]),
        )
        .unwrap();
        assert_eq!(registry.get("agent").is_some(), offered);
        assert!(registry.get("bash").is_some());
    }
    // A client that is itself delegated (`session.start.delegation_depth`)
    // counts too, even though this process is not delegated.
    for (declared, max_depth, offered) in [(1, 1, false), (1, 2, true), (5, 2, false)] {
        let registry = builtin_registry(
            ToolsConfig {
                max_delegation_depth: max_depth,
                delegation: Some(DelegationContext {
                    depth: declared,
                    ..delegation_context(home.path())
                }),
                ..ToolsConfig::default()
            },
            SkillMap::new(),
            Vec::new(),
            1024,
            HashMap::from([("claude".to_owned(), adapter.clone())]),
        )
        .unwrap();
        assert_eq!(
            registry.get("agent").is_some(),
            offered,
            "declared {declared}, limit {max_depth}"
        );
    }
}

fn installed(model_args: bool) -> AgentAdapterConfig {
    AgentAdapterConfig {
        command: "bash".into(),
        args: Vec::new(),
        prompt_args: Vec::new(),
        full_permission_args: None,
        model_args: if model_args {
            vec!["--model".into(), "{model}".into()]
        } else {
            Vec::new()
        },
        effort_args: Vec::new(),
        model_hint: "Model ID.".into(),
        environment: Vec::new(),
        search_dirs: Vec::new(),
        output: OutputFormat::Text,
        resume: Resume::Unsupported,
        home: None,
        transport: Transport::Process,
        acp: None,
        use_for: None,
        defaults: AgentDefaults::default(),
        options_file: None,
        busy: crate::BusyConfig::default(),
    }
}

#[test]
fn one_agent_tool_offers_every_installed_agent_with_the_preferred_one_as_default() {
    let home = tempfile::tempdir().unwrap();
    let agents = || {
        HashMap::from([
            ("claude".to_owned(), installed(true)),
            ("dsh".to_owned(), installed(false)),
        ])
    };
    let preferring = |prefer: &[&str], max_background| {
        builtin_registry(
            ToolsConfig {
                prefer: prefer.iter().map(|name| (*name).to_owned()).collect(),
                max_background,
                delegation: Some(delegation_context(home.path())),
                ..ToolsConfig::default()
            },
            SkillMap::new(),
            Vec::new(),
            1024,
            agents(),
        )
        .unwrap()
    };
    let registry = preferring(&["pi", "dsh"], 2);
    let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(
        names,
        [
            "agent",
            "agent_cancel",
            "agent_status",
            "agent_wait",
            "bash",
            "read",
            "read_skill",
            "write"
        ]
    );
    assert_eq!(crate::offered_agents(&registry), ["claude", "dsh"]);
    let spec = registry.get("agent").unwrap().spec();
    assert_eq!(spec.parameters["required"], serde_json::json!(["prompt"]));
    assert!(
        spec.parameters["properties"]["agent"]["description"]
            .as_str()
            .unwrap()
            .starts_with("Which agent runs the task. Defaults to dsh,")
    );
    // Only claude takes a model, so the schema offers it.
    assert!(spec.parameters["properties"]["model"].is_object());
    assert!(spec.parameters["properties"]["background"].is_object());
    // No preference offered here: the model must name the agent.
    let spec = preferring(&["pi"], 2).get("agent").unwrap().spec();
    assert_eq!(
        spec.parameters["required"],
        serde_json::json!(["agent", "prompt"])
    );
    // Without background jobs, the agent tool alone.
    let registry = preferring(&[], 0);
    assert!(registry.get("agent_status").is_none());
    let spec = registry.get("agent").unwrap().spec();
    assert!(spec.parameters["properties"].get("background").is_none());
}

#[test]
fn an_acp_agent_lists_the_values_its_server_offered_and_says_so_when_unknown() {
    use crate::delegate::options::{self, AgentOptions, Choice};

    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("claude-agent-acp");
    std::fs::write(&server, "#!/bin/sh\n").unwrap();
    let file = dir.path().join("state/agent-options/claude.json");
    let adapter = || {
        let mut adapter = installed(true);
        adapter.effort_args = vec!["--effort".into(), "{effort}".into()];
        adapter.acp = Some(crate::AcpAgentLaunch {
            command: server.display().to_string(),
            args: Vec::new(),
            full_mode: None,
            environment: Vec::new(),
            required: false,
            session_options: false,
        });
        adapter.options_file = Some(file.clone());
        adapter
    };
    let spec = |adapter: AgentAdapterConfig| {
        builtin_registry(
            ToolsConfig {
                max_background: 0,
                delegation: Some(delegation_context(dir.path())),
                ..ToolsConfig::default()
            },
            SkillMap::new(),
            Vec::new(),
            1024,
            HashMap::from([("claude".to_owned(), adapter)]),
        )
        .unwrap()
        .get("agent")
        .unwrap()
        .spec()
        .parameters
    };

    let unknown = spec(adapter());
    let line = unknown["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(line.contains("which SCV has not seen yet"), "{line}");
    assert!(
        !line.contains("Model ID"),
        "the CLI's hint does not apply: {line}"
    );
    // The user's configured efforts, for all tasks and for hard ones, are
    // always values the schema allows.
    let mut configured = adapter();
    configured.defaults.effort = Some("turbo".into());
    configured.defaults.hard_task_effort = Some("warp".into());
    let efforts = spec(configured)["properties"]["effort"]["enum"].clone();
    for effort in ["turbo", "warp"] {
        assert!(
            efforts.as_array().unwrap().contains(&effort.into()),
            "{efforts}"
        );
    }

    options::save(
        &file,
        "claude",
        &server,
        &AgentOptions {
            model: Some(Choice {
                values: vec!["default".into(), "opus[1m]".into(), "sonnet".into()],
                current: Some("default".into()),
            }),
            effort: Some(Choice {
                values: vec!["default".into(), "high".into(), "ultra".into()],
                current: Some("high".into()),
            }),
        },
    )
    .unwrap();
    let known = spec(adapter());
    let line = known["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(
        line.contains(
            "Takes model (one of: opus[1m], sonnet), effort (one of: high, ultra; its default \
             is high), and session."
        ),
        "{line}"
    );
    let efforts = known["properties"]["effort"]["enum"].as_array().unwrap();
    assert!(efforts.contains(&"ultra".into()), "{efforts:?}");
    assert!(efforts.contains(&"max".into()), "{efforts:?}");
    assert!(!efforts.contains(&"default".into()), "{efforts:?}");
    // With the user's own effort, the agent's default no longer runs, so
    // the line names only the user's.
    let mut configured = adapter();
    configured.defaults.effort = Some("ultra".into());
    let with_default = spec(configured);
    let line = with_default["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(
        line.contains(
            "Takes model (one of: opus[1m], sonnet), effort (one of: high, ultra), and \
             session. The user's default, used when a call leaves it out: effort ultra."
        ),
        "{line}"
    );

    // A server that lists efforts but no model: omit model.
    options::save(
        &file,
        "claude",
        &server,
        &AgentOptions {
            model: None,
            effort: Some(Choice {
                values: vec!["low".into()],
                current: None,
            }),
        },
    )
    .unwrap();
    let efforts_only = spec(adapter());
    let line = efforts_only["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(
        line.contains("model (its ACP server lists no model to choose, so omit model)"),
        "{line}"
    );
}
