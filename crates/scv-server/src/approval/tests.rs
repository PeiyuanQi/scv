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

/// A gate that records being asked and approves.
struct Asked(std::sync::atomic::AtomicUsize);

#[async_trait]
impl ApprovalGate for Asked {
    async fn approve(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn the_owner_is_only_asked_what_the_policy_leaves_to_a_person() {
    let request = |risk| ApprovalRequest {
        call_id: "job-1".into(),
        name: "agent".into(),
        risk,
        cwd: PathBuf::from("/"),
        summary: "nested".into(),
    };
    use ApprovalPolicy::{Always, Never, OnRisk};
    use ToolRisk::{Process, ReadOnly};
    for (policy, risk, expected, asked) in [
        // `never` denies side effects outright: no owner may grant them.
        (Never, Process, false, false),
        (Never, ReadOnly, true, false),
        (OnRisk, ReadOnly, true, false),
        // What the foreground would put to a person goes to the owner.
        (OnRisk, Process, true, true),
        (Always, ReadOnly, true, true),
    ] {
        let attended = Arc::new(Asked(std::sync::atomic::AtomicUsize::new(0)));
        let gate = PolicyFirstGate {
            policy,
            attended: Arc::clone(&attended) as Arc<dyn ApprovalGate>,
        };
        let answer = gate
            .approve(request(risk), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer, expected, "{policy:?} {risk:?}");
        assert_eq!(
            attended.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
            asked,
            "{policy:?} {risk:?}"
        );
    }
}
