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
