//! Unit tests for `src/control.rs`.

use std::path::PathBuf;

use scv_tools::delegation as delegations;

use super::*;

#[tokio::test]
async fn daemon_control_lists_and_stops_delegations() {
    use std::os::unix::process::CommandExt as _;
    let home = tempfile::tempdir().unwrap();
    let registry = DelegationRegistry::new(&scv_client::Layout::new(home.path()));
    let components = Arc::new(Mutex::new(components::Components::new(
        crate::test_support::test_instance("/unused"),
        PathBuf::from("/"),
    )));
    // A run owned by another live SCV process of the same instance.
    let mut owner = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let mut agent = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let identity = |pid| delegations::ProcessIdentity::of(pid).unwrap();
    let record = delegations::DelegationRecord {
        handle: "codex-a1b2c3".into(),
        agent: "codex".into(),
        instance: registry.instance().into(),
        session: "session".into(),
        owner: identity(owner.id()),
        process: identity(agent.id()),
        pgid: agent.id(),
        cwd: "/work/project\u{7}".into(),
        started_unix: 1,
        depth: 1,
        conversation: Some("codex-2".into()),
        turn: Some(3),
        idle_since_unix: None,
        background_jobs: None,
    };
    std::fs::create_dir_all(registry.record_dir()).unwrap();
    let save = |record: &delegations::DelegationRecord| {
        std::fs::write(
            registry.record_dir().join("codex-a1b2c3.json"),
            serde_json::to_vec(record).unwrap(),
        )
        .unwrap();
    };
    save(&record);
    let control = |command| daemon_control(&components, &registry, command);
    let Ok(status) = control(DaemonCommand::Status).await else {
        panic!("status failed");
    };
    assert_eq!(status.delegations.active, 1);
    assert_eq!(status.delegations.idle, Some(0));
    assert!(status.delegations.entries.is_empty());
    // Between turns it still counts as live, and as idle.
    save(&delegations::DelegationRecord {
        idle_since_unix: Some(2),
        ..record.clone()
    });
    let Ok(status) = control(DaemonCommand::Status).await else {
        panic!("status failed");
    };
    assert_eq!(status.delegations.active, 1);
    assert_eq!(status.delegations.idle, Some(1));
    // A nested SCV between turns whose own background job still runs is at
    // work, not idle, and its listing says why.
    save(&delegations::DelegationRecord {
        idle_since_unix: Some(2),
        background_jobs: Some(1),
        ..record.clone()
    });
    let Ok(status) = control(DaemonCommand::Status).await else {
        panic!("status failed");
    };
    assert_eq!(status.delegations.active, 1);
    assert_eq!(status.delegations.idle, Some(0));
    let Ok(status) = control(DaemonCommand::Delegations { all: false }).await else {
        panic!("listing failed");
    };
    assert_eq!(status.delegations.entries[0].background_jobs, Some(1));
    save(&record);
    let Ok(status) = control(DaemonCommand::Delegations { all: false }).await else {
        panic!("listing failed");
    };
    let [entry] = status.delegations.entries.as_slice() else {
        panic!("{:?}", status.delegations);
    };
    assert_eq!(entry.handle, "codex-a1b2c3");
    assert_eq!(entry.conversation.as_deref(), Some("codex-2"));
    assert_eq!(entry.turn, Some(3));
    assert_eq!(entry.pid, agent.id());
    assert_eq!(entry.owner_pid, owner.id());
    assert!(!entry.orphaned);
    assert_eq!(entry.processes, 1);
    for command in [
        DaemonCommand::DelegationKill {
            handle: Some("codex-nosuch".into()),
            orphans: false,
        },
        DaemonCommand::DelegationKill {
            handle: None,
            orphans: false,
        },
    ] {
        assert!(matches!(
            control(command).await,
            Err(ControlFailure::Delegation(_))
        ));
    }
    let Ok(status) = control(DaemonCommand::DelegationKill {
        handle: Some("codex-a1b2c3".into()),
        orphans: false,
    })
    .await
    else {
        panic!("kill failed");
    };
    assert_eq!(status.delegations.killed, ["codex-a1b2c3"]);
    assert!(agent.wait().unwrap().code().is_none());
    // Its live owner removes the record itself; once the owner is gone
    // the record is an orphan that an orphan sweep removes.
    owner.kill().unwrap();
    owner.wait().unwrap();
    let Ok(status) = control(DaemonCommand::Delegations { all: true }).await else {
        panic!("listing failed");
    };
    assert!(status.delegations.entries[0].orphaned);
    assert_eq!(status.delegations.active, 0);
    // An orphan between turns is neither live nor idle.
    save(&delegations::DelegationRecord {
        idle_since_unix: Some(2),
        ..record
    });
    let Ok(status) = control(DaemonCommand::Status).await else {
        panic!("status failed");
    };
    assert_eq!(status.delegations.active, 0);
    assert_eq!(status.delegations.idle, Some(0));
    let Ok(_) = control(DaemonCommand::DelegationKill {
        handle: None,
        orphans: true,
    })
    .await
    else {
        panic!("orphan sweep failed");
    };
    assert!(registry.list(true).is_empty());
}

/// A mail action's ID: `a` and 32 hex digits.
fn action_id() -> String {
    format!("a{}", "0123456789abcdef".repeat(2))
}

/// The safe message of a mail control failure.
fn mail_failure(result: std::result::Result<DaemonStatus, ControlFailure>) -> String {
    let Err(ControlFailure::Mail(message)) = result else {
        panic!("expected a mail failure");
    };
    message
}

#[tokio::test]
async fn mail_status_and_mail_cancel_reject_an_unknown_or_stopped_account() {
    let home = tempfile::tempdir().unwrap();
    let registry = DelegationRegistry::new(&scv_client::Layout::new(home.path()));
    let hub = Hub::new(None);
    // Registered, but not taking actions.
    let _idle = hub.register_mail("email:work", vec!["feishu:mail".into()]);
    let components = Arc::new(Mutex::new(
        components::Components::with_hub(
            crate::test_support::test_instance(home.path()),
            PathBuf::from("/"),
            Arc::clone(&hub),
        )
        .unwrap(),
    ));
    let control = |command| daemon_control(&components, &registry, command);

    let not_running = "that email account is not running with mail actions on";
    let not_a_name = "not an email account name";
    let name_one = "name one mail action's ID, or ask for all of them";
    let not_an_id = "that is not a mail action's ID";

    assert_eq!(
        mail_failure(
            control(DaemonCommand::MailStatus {
                account: Some("secret token".into())
            })
            .await
        ),
        not_a_name,
        "an account name that is not one is refused without echoing it"
    );
    assert_eq!(
        mail_failure(
            control(DaemonCommand::MailStatus {
                account: Some("work".into())
            })
            .await
        ),
        not_running
    );
    let Ok(status) = control(DaemonCommand::MailStatus { account: None }).await else {
        panic!("status of every account failed");
    };
    assert!(
        status.mail_actions.is_empty(),
        "an account that is not taking actions is not asked"
    );
    assert_eq!(status.mail_note, None);

    for (id, all) in [(None, false), (Some(action_id()), true)] {
        assert_eq!(
            mail_failure(
                control(DaemonCommand::MailCancel {
                    account: "work".into(),
                    id,
                    all,
                })
                .await
            ),
            name_one,
            "cancel needs exactly one of an ID and all"
        );
    }
    for id in [
        "abc",
        "a",
        &"g".repeat(32),
        &format!("b{}", "ab".repeat(16)),
    ] {
        let message = mail_failure(
            control(DaemonCommand::MailCancel {
                account: "secret token".into(),
                id: Some(id.to_owned()),
                all: false,
            })
            .await,
        );
        assert_eq!(message, not_an_id, "rejected {id}");
        assert!(
            !message.contains("secret"),
            "the refusal must not carry the account text"
        );
    }
    assert_eq!(
        mail_failure(
            control(DaemonCommand::MailCancel {
                account: "not a name".into(),
                id: Some(action_id()),
                all: false,
            })
            .await
        ),
        not_a_name
    );
    for command in [
        DaemonCommand::MailCancel {
            account: "work".into(),
            id: Some(action_id()),
            all: false,
        },
        DaemonCommand::MailCancel {
            account: "work".into(),
            id: None,
            all: true,
        },
    ] {
        assert_eq!(mail_failure(control(command).await), not_running);
    }
}
