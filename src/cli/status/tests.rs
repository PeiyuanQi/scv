//! Unit tests for `src/cli/status.rs`.

use super::*;

#[test]
fn live_agents_between_turns_are_idle_not_running() {
    let delegations = DelegationSummary {
        active: 3,
        idle: Some(2),
        reaped: 4,
        ..DelegationSummary::default()
    };
    assert_eq!(
        describe_delegations(&delegations),
        "Delegations: 1 running, 2 idle, 4 orphaned runs stopped since the daemon started"
    );
    let quiet = DelegationSummary {
        idle: Some(0),
        ..DelegationSummary::default()
    };
    assert_eq!(
        describe_delegations(&quiet),
        "Delegations: 0 running, 0 idle, 0 orphaned runs stopped since the daemon started"
    );
}

#[test]
fn a_daemon_without_idle_counts_shows_every_live_run_running() {
    // As 0.3.0 reports two live runs, one of them perhaps between turns.
    let delegations = DelegationSummary {
        active: 2,
        reaped: 1,
        ..DelegationSummary::default()
    };
    assert_eq!(
        describe_delegations(&delegations),
        "Delegations: 2 running, 1 orphaned runs stopped since the daemon started"
    );
}
