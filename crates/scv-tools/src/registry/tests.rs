//! Unit tests for `src/registry.rs`.

use std::path::Path;

use super::*;
use crate::delegate::{
    adapters::{OutputFormat, Resume},
    records::DelegationRegistry,
};

fn delegation_context(home: &Path) -> DelegationContext {
    DelegationContext {
        registry: Arc::new(DelegationRegistry::new(home)),
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
            ("agent_present".to_owned(), adapter("bash")),
            (
                "agent_missing".to_owned(),
                adapter("scv-test-agent-that-is-not-installed"),
            ),
        ]),
    )
    .unwrap();
    assert!(registry.get("agent_present").is_some());
    assert!(registry.get("agent_missing").is_none());
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
            HashMap::from([("agent_claude".to_owned(), adapter.clone())]),
        )
        .unwrap();
        assert_eq!(registry.get("agent_claude").is_some(), offered);
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
            HashMap::from([("agent_claude".to_owned(), adapter.clone())]),
        )
        .unwrap();
        assert_eq!(
            registry.get("agent_claude").is_some(),
            offered,
            "declared {declared}, limit {max_depth}"
        );
    }
}
