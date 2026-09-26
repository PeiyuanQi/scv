//! Unit tests for `src/registry.rs`.

use std::{collections::HashMap, path::Path};

use super::*;
use crate::delegate::{
    adapters::{OutputFormat, Resume},
    records::DelegationRegistry,
};

fn delegation_context(home: &Path) -> DelegationContext {
    DelegationContext {
        registry: Arc::new(DelegationRegistry::new(&scv_client::Layout::new(home))),
        session: "session-1".into(),
        depth: 0,
    }
}

#[test]
fn uninstalled_agents_are_not_offered() {
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
        model: None,
        effort: None,
    };
    let registry = builtin_registry(
        ToolsConfig::default(),
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
        ToolsConfig::default(),
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
        model: None,
        effort: None,
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
        model: None,
        effort: None,
    }
}

#[test]
fn one_agent_tool_offers_every_installed_agent_with_the_preferred_one_as_default() {
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
