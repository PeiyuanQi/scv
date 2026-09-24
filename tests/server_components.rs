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
                scv_client::control(&home.join("state/server.sock"), DaemonCommand::Status).await
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
        .open(home.join(format!("state/channels/wechat/{account}.transaction")))
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
    let stream = UnixStream::connect(home.join("state/server.sock"))
        .await
        .unwrap();
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
            delegation_depth: None,
            channel: None,
            auto_approve: None,
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
    let accounts = home.path().join("credentials/wechat");
    std::fs::create_dir_all(&accounts).unwrap();
    let account = accounts.join("test.json");
    std::fs::write(&account, r#"{"token":"test-secret-never-in-status","base_url":"https://127.0.0.1:1","bot_id":"bot-test","user_id":"user-test"}"#).unwrap();
    std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = start(home.path(), workspace.path());
    let socket = home.path().join("state/server.sock");
    let first = status(home.path()).await;
    let loaded = scv_client::control(&socket, DaemonCommand::Reload)
        .await
        .unwrap();
    assert_eq!(loaded.components.len(), 1);
    assert_eq!(loaded.components[0].id, "wechat:test");
    assert_eq!(loaded.components[0].channel, "wechat");
    assert_eq!(loaded.components[0].bot_id.as_deref(), Some("bot-test"));
    assert_ne!(loaded.components[0].state, ComponentState::Connected);
    assert!(loaded.components[0].last_success_unix_seconds.is_none());
    assert!(
        !serde_json::to_string(&loaded)
            .unwrap()
            .contains("test-secret")
    );
    // `scv status` summarizes connections, then shows each account as
    // indented JSON.
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home.path())
        .arg("status")
        .output()
        .await
        .unwrap();
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(
        shown.contains(
            "\nChannels: 0 of 1 enabled accounts connected\n{\n  \"id\": \"wechat:test\",\n"
        ),
        "{shown}"
    );
    assert!(!shown.contains("test-secret"), "{shown}");
    // A running bridge holds the transaction lock while it commits state.
    // Operator commands wait for the commit instead of failing.
    let commit = hold_transaction(home.path(), "test", Duration::from_millis(300));
    for _ in 0..2 {
        let running = scv_client::control(
            &socket,
            DaemonCommand::ChannelSet {
                channel: "wechat".into(),
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
        DaemonCommand::ChannelSet {
            channel: "wechat".into(),
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
        DaemonCommand::ChannelLogout {
            channel: "wechat".into(),
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
    let accounts = home.path().join("credentials/wechat");
    std::fs::create_dir_all(&accounts).unwrap();
    let account = accounts.join("bad.json");
    std::fs::write(&account, "private-malformed-token").unwrap();
    std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = start(home.path(), workspace.path());
    status(home.path()).await;
    let loaded = scv_client::control(
        &home.path().join("state/server.sock"),
        DaemonCommand::Reload,
    )
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

/// Writes the instance's private `config.toml`.
fn write_config(home: &Path, contents: &str) {
    let path = home.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Writes a private file two levels below the instance home, creating its
/// private parent directories.
fn write_private(path: &Path, contents: &str) {
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    for directory in [parent, parent.parent().unwrap()] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn account_settings_come_from_config_toml_and_old_layout_files_are_not_read() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let account = r#"{"token":"test-secret-never-in-status","base_url":"https://127.0.0.1:1","bot_id":"bot-test","user_id":"user-test"}"#;
    // An account saved in the layout before 0.2.0 is ignored, not moved.
    write_private(
        &home.path().join("channels/wechat/accounts/old.json"),
        account,
    );
    write_private(&home.path().join("credentials/wechat/test.json"), account);
    let config = home.path().join("config.toml");
    let text = "# Chat accounts\n[channels.wechat.test] # hand-written\nenabled = false\nremote_tools = \"owner\"\n";
    write_config(home.path(), text);
    let mut child = start(home.path(), workspace.path());
    status(home.path()).await;
    let socket = home.path().join("state/server.sock");
    let loaded = scv_client::control(&socket, DaemonCommand::Reload)
        .await
        .unwrap();
    let ids: Vec<_> = loaded.components.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["wechat:test"]);
    assert_eq!(loaded.components[0].state, ComponentState::Disabled);
    assert_eq!(loaded.components[0].remote_tools, RemoteTools::Owner);
    assert!(
        home.path()
            .join("channels/wechat/accounts/old.json")
            .exists()
    );

    // SCV's own change keeps the person's comments.
    scv_client::control(
        &socket,
        DaemonCommand::ChannelSet {
            channel: "wechat".into(),
            account: "test".into(),
            enabled: false,
            workspace: Some(workspace.path().display().to_string()),
            remote_tools: Some(RemoteTools::None),
        },
    )
    .await
    .unwrap();
    let edited = std::fs::read_to_string(&config).unwrap();
    assert!(edited.starts_with("# Chat accounts\n[channels.wechat.test] # hand-written\n"));
    assert!(edited.contains("remote_tools = \"none\""), "{edited}");
    assert!(edited.contains("workspace = "), "{edited}");

    // A person's own edit takes effect at the next reconciliation.
    std::fs::write(
        &config,
        edited.replace("remote_tools = \"none\"", "remote_tools = \"owner\""),
    )
    .unwrap();
    let reloaded = scv_client::control(&socket, DaemonCommand::Reload)
        .await
        .unwrap();
    assert_eq!(reloaded.components[0].remote_tools, RemoteTools::Owner);

    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home.path())
        .args(["config", "show"])
        .current_dir(workspace.path())
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(shown.contains("wechat:test"), "{shown}");
    assert!(
        shown.contains("credentials/wechat/test.json 0600"),
        "{shown}"
    );
    assert!(
        shown.contains("channels") && shown.contains("left by an older SCV layout"),
        "{shown}"
    );
    assert!(!shown.contains("test-secret"), "{shown}");
    terminate(&mut child).await;
}

#[tokio::test]
async fn feishu_accounts_run_beside_wechat_and_outlive_its_discovery_failure() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let account = r#"{"app_id":"cli_a1b2c3d4","app_secret":"test-secret-never-in-status","brand":"feishu","owner_open_id":"ou_owner"}"#;
    let settings = "[channels.feishu.default]\nenabled = false\nremote_tools = \"owner\"\n";
    write_private(
        &home.path().join("credentials/feishu/default.json"),
        account,
    );
    write_config(home.path(), settings);
    // Unreadable WeChat credentials fail WeChat discovery alone.
    write_private(&home.path().join("credentials/wechat"), "not a directory");
    let mut child = start(home.path(), workspace.path());
    status(home.path()).await;
    assert!(
        scv_client::control(
            &home.path().join("state/server.sock"),
            DaemonCommand::Reload
        )
        .await
        .is_err()
    );
    let status = status(home.path()).await;
    let ids: Vec<_> = status.components.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["feishu:default", "wechat:discovery-error"]);
    let feishu = &status.components[0];
    assert_eq!(feishu.channel, "feishu");
    assert_eq!(feishu.state, ComponentState::Disabled);
    assert_eq!(feishu.bot_id.as_deref(), Some("cli_a1b2c3d4"));
    assert_eq!(feishu.user_id.as_deref(), Some("ou_owner"));
    assert_eq!(feishu.remote_tools, RemoteTools::Owner);
    assert_eq!(status.components[1].state, ComponentState::Failed);
    assert!(
        !serde_json::to_string(&status)
            .unwrap()
            .contains("test-secret")
    );
    terminate(&mut child).await;
}

#[tokio::test]
async fn channel_login_options_stay_with_their_platform_and_ids_are_checked_first() {
    let home = tempfile::tempdir().unwrap();
    for (args, expected) in [
        (
            &["channels", "login", "wechat", "--app-id", "cli_1"][..],
            "Feishu options",
        ),
        (
            &[
                "channels",
                "login",
                "feishu",
                "--login-url",
                "https://x.test",
            ][..],
            "WeChat option",
        ),
        (
            &["channels", "login", "lark", "--app-id", "not-an-app"][..],
            "invalid Feishu app ID",
        ),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_scv"))
            .isolated(home.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The secret arrives on stdin, never as an argument.
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"test-secret-never-printed\n")
            .await
            .unwrap();
        drop(stdin);
        let output = child.wait_with_output().await.unwrap();
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!format!("{stdout}{stderr}").contains("test-secret"));
    }
    assert!(!home.path().join("credentials/feishu").exists());
}
