//! Unit tests for `src/cli/agents.rs`.

use super::*;

#[test]
fn a_live_agent_between_turns_is_listed_idle_unless_its_own_jobs_run() {
    let running = scv_protocol::DelegationInfo {
        handle: "scv-92f0c3".into(),
        agent: "scv".into(),
        session: "s".into(),
        depth: 1,
        pid: 4321,
        owner_pid: 1234,
        processes: 1,
        cwd: "/workspace/scv".into(),
        started_unix_seconds: 1_750_000_000,
        orphaned: false,
        conversation: Some("scv-1".into()),
        turn: Some(1),
        idle_since_unix_seconds: None,
        background_jobs: None,
    };
    assert_eq!(delegation_state(&running), "running");
    let idle = scv_protocol::DelegationInfo {
        idle_since_unix_seconds: Some(1_750_000_600),
        ..running.clone()
    };
    assert_eq!(delegation_state(&idle), "idle");
    // Its turn ended, but a background job of its own still runs.
    let background = scv_protocol::DelegationInfo {
        background_jobs: Some(1),
        ..idle.clone()
    };
    assert_eq!(delegation_state(&background), "background");
    let working = scv_protocol::DelegationInfo {
        background_jobs: Some(1),
        ..running
    };
    assert_eq!(delegation_state(&working), "running");
    let orphaned = scv_protocol::DelegationInfo {
        orphaned: true,
        ..background
    };
    assert_eq!(delegation_state(&orphaned), "orphaned");
}
