//! Unit tests for `src/approval.rs`.

use std::path::PathBuf;

use super::*;

#[tokio::test]
async fn background_requests_get_only_the_unattended_answer() {
    let request = |risk| ApprovalRequest {
        call_id: "job-1".into(),
        name: "agent".into(),
        risk,
        cwd: PathBuf::from("/"),
        summary: "nested".into(),
    };
    let decide = |policy, client_approves_all, risk| async move {
        UnattendedGate {
            policy,
            client_approves_all,
        }
        .approve(request(risk), CancellationToken::new())
        .await
        .unwrap()
    };
    use ApprovalPolicy::{Always, Never, OnRisk};
    use ToolRisk::{Process, ReadOnly};
    // An owner chat session's client approves everything, so background
    // requests get that answer, within the policy.
    assert!(decide(OnRisk, true, Process).await);
    assert!(decide(Always, true, Process).await);
    assert!(decide(Always, true, ReadOnly).await);
    assert!(
        !decide(Never, true, Process).await,
        "never beyond the policy"
    );
    assert!(decide(Never, true, ReadOnly).await);
    // A client that asks a person (the TUI) or a tool-free guest: only
    // what the policy grants on its own.
    assert!(!decide(OnRisk, false, Process).await);
    assert!(decide(OnRisk, false, ReadOnly).await);
    assert!(!decide(Always, false, ReadOnly).await);
    assert!(!decide(Never, false, Process).await);
}
