//! Process-level lifecycle tests use an isolated home and no external services.
mod common;

use common::Isolated;
use scv_protocol::{
    ClientMessage, ComponentState, DaemonCommand, DaemonStatus, PROTOCOL_VERSION, PeerInfo,
    RemoteTools, ServerEvent,
};
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
};

fn start(home: &Path, workspace: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home)
        .args(["run", "--workspace"])
        .arg(workspace)
        .env("OPENAI_API_KEY", "test-only")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn status(home: &Path) -> DaemonStatus {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(status) =
                scv_client::control(&home.join("server.sock"), DaemonCommand::Status).await
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

async fn terminate(child: &mut Child) {
    let result = Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(result.success());
    assert!(
        tokio::time::timeout(Duration::from_secs(12), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

/// Takes an account's transaction lock, as a bridge state commit does, and
/// releases it from another thread after `duration`.
fn hold_transaction(home: &Path, account: &str, duration: Duration) -> std::thread::JoinHandle<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.join(format!("clawbot/transactions/{account}.json")))
        .unwrap();
    // SAFETY: the descriptor stays valid until the thread drops `file`.
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        drop(file);
    })
}

async fn session(
    home: &Path,
    workspace: &Path,
) -> (
    String,
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(home.join("server.sock")).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    for message in [
        ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
            client: PeerInfo {
                name: "test".into(),
                version: "0".into(),
            },
        },
        ClientMessage::SessionStart {
            request_id: "start".into(),
            cwd: workspace.display().to_string(),
            provider: None,
            model: None,
            base_url: None,
            no_tools: Some(true),
        },
    ] {
        write
            .write_all(&serde_json::to_vec(&message).unwrap())
            .await
            .unwrap();
        write.write_all(b"\n").await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        match serde_json::from_str::<ServerEvent>(&line).unwrap() {
            ServerEvent::Initialized { .. } => {}
            ServerEvent::SessionStarted { session_id, .. } => return (session_id, reader, write),
            event => panic!("unexpected {event:?}"),
        }
    }
    unreachable!()
}

#[tokio::test]
async fn daemon_restores_enabled_accounts_and_connected_clients_get_fresh_sessions() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let accounts = home.path().join("clawbot/accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    let account = accounts.join("test.json");
    std::fs::write(&account, r#"{"token":"test-secret-never-in-status","base_url":"https://127.0.0.1:1","bot_id":"bot-test","user_id":"user-test"}"#).unwrap();
    std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = start(home.path(), workspace.path());
    let socket = home.path().join("server.sock");
    let first = status(home.path()).await;
    let loaded = scv_client::control(&socket, DaemonCommand::Reload)
        .await
        .unwrap();
    assert_eq!(loaded.components.len(), 1);
    assert_eq!(loaded.components[0].bot_id.as_deref(), Some("bot-test"));
    assert_ne!(loaded.components[0].state, ComponentState::Connected);
    assert!(loaded.components[0].last_success_unix_seconds.is_none());
    assert!(
        !serde_json::to_string(&loaded)
            .unwrap()
            .contains("test-secret")
    );
    // A running bridge holds the transaction lock while it commits state.
    // Operator commands wait for the commit instead of failing.
    let commit = hold_transaction(home.path(), "test", Duration::from_millis(300));
    for _ in 0..2 {
        let running = scv_client::control(
            &socket,
            DaemonCommand::ClawbotSet {
                account: "test".into(),
                enabled: true,
                workspace: None,
                remote_tools: Some(RemoteTools::Owner),
            },
        )
        .await
        .unwrap();
        assert_eq!(running.components.len(), 1);
        assert_eq!(running.components[0].remote_tools, RemoteTools::Owner);
    }
    commit.join().unwrap();
    let mut duplicate = start(home.path(), workspace.path());
    assert!(
        !tokio::time::timeout(Duration::from_secs(3), duplicate.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(status(home.path()).await.pid, first.pid);
    let (old_session, mut reader, _writer) = session(home.path(), workspace.path()).await;
    terminate(&mut child).await;
    let mut line = String::new();
    while tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap()
        != 0
    {
        line.clear();
    }
    assert!(!socket.exists());
    assert!(
        scv_client::control(&socket, DaemonCommand::Status)
            .await
            .is_err()
    );
    let mut child = start(home.path(), workspace.path());
    let second = status(home.path()).await;
    assert_ne!(first.pid, second.pid);
    let restored = scv_client::control(&socket, DaemonCommand::Reload)
        .await
        .unwrap();
    assert_eq!(restored.components.len(), 1);
    assert!(restored.components[0].enabled);
    let (new_session, _, _) = session(home.path(), workspace.path()).await;
    assert_ne!(old_session, new_session);
    let disabled = scv_client::control(
        &socket,
        DaemonCommand::ClawbotSet {
            account: "test".into(),
            enabled: false,
            workspace: None,
            remote_tools: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(disabled.components[0].state, ComponentState::Disabled);
    // An omitted mode keeps the saved grant.
    assert_eq!(disabled.components[0].remote_tools, RemoteTools::Owner);
    terminate(&mut child).await;
    let mut child = start(home.path(), workspace.path());
    status(home.path()).await;
    let restored = scv_client::control(&socket, DaemonCommand::Reload)
        .await
        .unwrap();
    assert_eq!(restored.components[0].state, ComponentState::Disabled);
    let removed = scv_client::control(
        &socket,
        DaemonCommand::ClawbotLogout {
            account: "test".into(),
        },
    )
    .await
    .unwrap();
    assert!(removed.components.is_empty());
    assert!(!account.exists());
    terminate(&mut child).await;
}

#[tokio::test]
async fn invalid_credentials_report_sanitized_failure_without_disabling_daemon() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let accounts = home.path().join("clawbot/accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    let account = accounts.join("bad.json");
    std::fs::write(&account, "private-malformed-token").unwrap();
    std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = start(home.path(), workspace.path());
    status(home.path()).await;
    let loaded = scv_client::control(&home.path().join("server.sock"), DaemonCommand::Reload)
        .await
        .unwrap();
    assert_eq!(loaded.components[0].state, ComponentState::Failed);
    assert!(
        !serde_json::to_string(&loaded)
            .unwrap()
            .contains("private-malformed")
    );
    terminate(&mut child).await;
}
