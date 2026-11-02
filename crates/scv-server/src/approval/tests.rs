//! Unit tests for `src/approval.rs`.

use std::path::PathBuf;

use super::*;

fn request(risk: ToolRisk, command: Option<&str>) -> ApprovalRequest {
    ApprovalRequest {
        call_id: "job-1".into(),
        name: "agent".into(),
        risk,
        cwd: PathBuf::from("/"),
        summary: "nested".into(),
        command: command.map(str::to_owned),
    }
}

fn policy(mode: PolicyMode) -> Policy {
    Policy { mode }
}

/// Nobody can be asked: a stdio server, which has no `Confirmer`.
fn nobody() -> OwnerQuestions {
    OwnerQuestions {
        confirmer: None,
        session: "session-1".into(),
    }
}

#[test]
fn the_mode_decides_after_the_gates() {
    use Decision::{Allow, AskClient, Deny};
    use PolicyMode::{Always, Auto, OnRisk, ReadOnly};
    use ToolRisk::{Delegate, Process, ReadOnly as Read};
    for (mode, risk, decision) in [
        (Auto, Process, Allow),
        (Auto, Delegate, Allow),
        (Auto, Read, Allow),
        (OnRisk, Read, Allow),
        (OnRisk, Process, AskClient),
        (OnRisk, Delegate, AskClient),
        (Always, Read, AskClient),
        (Always, Process, AskClient),
        (ReadOnly, Read, Allow),
        (ReadOnly, Process, Deny),
        // `agent` calls are delegate risk, so no agent starts.
        (ReadOnly, Delegate, Deny),
    ] {
        assert_eq!(
            decide(policy(mode), &request(risk, Some("cargo test"))),
            decision,
            "{mode:?} {risk:?}"
        );
    }
}

#[test]
fn force_pushing_main_is_denied_and_a_direct_publish_asks_the_owner_in_every_mode() {
    for mode in [
        PolicyMode::Auto,
        PolicyMode::OnRisk,
        PolicyMode::Always,
        PolicyMode::ReadOnly,
    ] {
        let decide =
            |command: &str| decide(policy(mode), &request(ToolRisk::Process, Some(command)));
        for command in [
            "git push --force origin main",
            "git push -f origin main",
            "git push origin main --force",
            "git push --force-with-lease origin HEAD:main",
            "git push --force-with-lease=main:abc123 origin main",
            "git push origin +main",
            "git push origin +HEAD:refs/heads/main",
            "git -C /work/scv push -fu origin main",
            "cd /work && git push -f origin main && echo done",
            "scripts/host.sh git push --force origin main",
            "GIT_TRACE=1 /usr/bin/git push 'origin' \"main\" -f",
        ] {
            assert_eq!(decide(command), Decision::Deny, "{mode:?}: {command}");
        }
        for command in [
            "cargo publish -p scv-core",
            "cargo +1.88.0 publish --locked",
            "cargo --locked publish",
            "scripts/host.sh cargo publish -p scv-cli",
            "cargo build && cargo publish",
        ] {
            assert_eq!(decide(command), Decision::AskOwner, "{mode:?}: {command}");
        }
    }
    // What feature-flow does stays the mode's to decide.
    for command in [
        "git push origin HEAD:main",
        "git push -u origin HEAD",
        "git push --force-with-lease origin feat/topic",
        "git push --force-with-lease",
        "git push origin +feat/topic",
        "git commit -m 'never git push -f origin main'",
        "cargo build --release && scripts/publish.sh",
        "cargo test -p scv-server publish",
    ] {
        assert_eq!(
            decide(
                policy(PolicyMode::Auto),
                &request(ToolRisk::Process, Some(command))
            ),
            Decision::Allow,
            "{command}"
        );
    }
    // Without a command, only the mode decides.
    assert_eq!(
        decide(policy(PolicyMode::Auto), &request(ToolRisk::Process, None)),
        Decision::Allow
    );
}

#[tokio::test]
async fn background_requests_get_only_the_unattended_answer() {
    let decide = |mode, client_approves_all, risk| async move {
        UnattendedGate {
            policy: policy(mode),
            client_approves_all,
            owner: nobody(),
        }
        .approve(request(risk, None), CancellationToken::new())
        .await
        .unwrap()
    };
    use PolicyMode::{Always, Auto, OnRisk, ReadOnly};
    use ToolRisk::{Process, ReadOnly as Read};
    // An owner chat session's client approves everything, so background
    // requests get that answer, within the policy.
    assert!(decide(OnRisk, true, Process).await);
    assert!(decide(Always, true, Process).await);
    assert!(decide(Always, true, Read).await);
    assert!(
        !decide(ReadOnly, true, Process).await,
        "never beyond the policy"
    );
    assert!(decide(ReadOnly, true, Read).await);
    // A client that asks a person (the TUI) or a tool-free guest: only
    // what the policy grants on its own.
    assert!(!decide(OnRisk, false, Process).await);
    assert!(decide(OnRisk, false, Read).await);
    assert!(!decide(Always, false, Read).await);
    assert!(!decide(ReadOnly, false, Process).await);
    assert!(decide(Auto, false, Process).await);
}

#[tokio::test]
async fn a_blanket_answer_never_satisfies_a_request_only_the_owner_may_answer() {
    let publish = || request(ToolRisk::Process, Some("cargo publish -p scv-core"));
    // A chat client approves everything; this still needs the owner, who
    // cannot be asked here, so it is a no.
    let unattended = UnattendedGate {
        policy: policy(PolicyMode::Auto),
        client_approves_all: true,
        owner: nobody(),
    };
    assert!(
        !unattended
            .approve(publish(), CancellationToken::new())
            .await
            .unwrap()
    );
    let attended = Arc::new(Asked(std::sync::atomic::AtomicUsize::new(0)));
    let gate = PolicyFirstGate {
        policy: policy(PolicyMode::Auto),
        attended: Arc::clone(&attended) as Arc<dyn ApprovalGate>,
        owner: nobody(),
    };
    assert!(
        !gate
            .approve(publish(), CancellationToken::new())
            .await
            .unwrap()
    );
    assert_eq!(attended.0.load(std::sync::atomic::Ordering::SeqCst), 0);
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
    use PolicyMode::{Always, Auto, OnRisk, ReadOnly};
    use ToolRisk::{Process, ReadOnly as Read};
    for (mode, risk, expected, asked) in [
        // `read-only` denies side effects outright: no owner may grant them.
        (ReadOnly, Process, false, false),
        (ReadOnly, Read, true, false),
        (OnRisk, Read, true, false),
        (Auto, Process, true, false),
        // What the foreground would put to a person goes to the owner.
        (OnRisk, Process, true, true),
        (Always, Read, true, true),
    ] {
        let attended = Arc::new(Asked(std::sync::atomic::AtomicUsize::new(0)));
        let gate = PolicyFirstGate {
            policy: policy(mode),
            attended: Arc::clone(&attended) as Arc<dyn ApprovalGate>,
            owner: nobody(),
        };
        let answer = gate
            .approve(request(risk, None), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer, expected, "{mode:?} {risk:?}");
        assert_eq!(
            attended.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
            asked,
            "{mode:?} {risk:?}"
        );
    }
}

#[test]
fn the_owner_is_told_why_and_what() {
    let question = owner_question(&ApprovalRequest {
        summary: "Run with /bin/bash -lc: cargo publish -p scv-core".into(),
        ..request(ToolRisk::Process, Some("cargo publish -p scv-core"))
    });
    assert!(
        question.starts_with("A direct cargo publish needs the owner's yes"),
        "{question}"
    );
    assert!(question.contains("cargo publish -p scv-core"), "{question}");
}
