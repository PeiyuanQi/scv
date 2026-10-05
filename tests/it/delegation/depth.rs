//! Production delegation-depth and parent-chain inheritance, exercised in
//! separate processes so the test runner's environment stays untouched.

use std::{process::Command, sync::Arc, time::Duration};

use scv_tools::{
    AgentAdapterConfig, AgentDefaults, DelegationContext, SkillMap, ToolsConfig,
    adapters::{OutputFormat, Resume, Transport},
    builtin_registry,
    delegation::{DelegationRegistry, current_depth},
};

use crate::support::Isolated;

const CHILD: &str = "SCV_TEST_DEPTH_CHILD";
const PARENT: &str = "outer/session/codex-123456";

#[tokio::test]
async fn inherited_depth_gates_agents_and_extends_the_parent_chain() {
    let Ok(expected) = std::env::var(CHILD) else {
        for depth in [None, Some(1), Some(2), Some(5)] {
            let home = tempfile::tempdir().unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .isolated(home.path())
                .args([
                    "--exact",
                    "delegation::depth::inherited_depth_gates_agents_and_extends_the_parent_chain",
                    "--nocapture",
                ])
                .env(CHILD, depth.unwrap_or(0).to_string());
            if let Some(depth) = depth {
                child
                    .env("SCV_DELEGATION_DEPTH", depth.to_string())
                    .env("SCV_PARENT", PARENT);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "depth {depth:?}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let expected: u32 = expected.parse().unwrap();
    // This integration binary links scv-tools without cfg(test).
    assert_eq!(current_depth(), expected);
    let home = tempfile::tempdir().unwrap();
    let records = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
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
        busy: scv_tools::BusyConfig::default(),
    };
    // Cover both the ambient fallback and a registry-backed client. A
    // lower client declaration must never reduce the process's own depth.
    for declared in [None, Some(0), Some(1), Some(6)] {
        let owner = expected.max(declared.unwrap_or(0));
        for limit in [owner, owner + 1] {
            let config = ToolsConfig {
                max_delegation_depth: limit,
                max_background: 0,
                agent_timeout: Duration::from_secs(30),
                delegation: declared.map(|depth| DelegationContext {
                    registry: Arc::clone(&records),
                    session: "depth-test".into(),
                    depth,
                }),
                ..ToolsConfig::default()
            };
            let tools = builtin_registry(
                config,
                SkillMap::new(),
                Vec::new(),
                0,
                [("probe".into(), adapter.clone())],
            )
            .unwrap();
            assert_eq!(tools.get("agent").is_some(), owner < limit);
            assert!(tools.get("bash").is_some());
            // Existing nested-SCV integration coverage exercises emitted
            // environment propagation; this test focuses on the production
            // depth gate under inherited and client-declared selectors.
        }
    }
}
