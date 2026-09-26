//! `scv confirm`: its exit status against an isolated daemon, a daemon too
//! old for it, and a scripted daemon that reports each answer.

use crate::daemon::{start, status, terminate};
use crate::support::Isolated;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

fn confirm(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scv"));
    command.isolated(home).arg("confirm");
    command
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A `daemon.status` reply about question `q1` in `state`.
fn question(state: &str) -> Value {
    json!({"type":"daemon.status","request_id":"control","status":{
        "version":"0.3.0","pid":1,"components":[],
        "confirm":{"id":"q1","state":state,"chat":"wechat:default","deadline_unix_seconds":unix_now() + 60}
    }})
}

/// Answer one management connection per reply in `replies`, recording each
/// `daemon.control` command, then stop listening and remove the socket as
/// a stopped daemon does.
fn scripted_daemon(home: &Path, replies: Vec<Value>) -> Arc<Mutex<Vec<Value>>> {
    std::fs::create_dir_all(home.join("state")).unwrap();
    let socket = home.join("state/server.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&commands);
    tokio::spawn(async move {
        for reply in replies {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let init: Value = serde_json::from_str(&line).unwrap();
            let initialized = json!({"type":"initialized","request_id":init["request_id"],"protocol_version":init["protocol_version"],"server":{"name":"scv-server","version":"0.3.0"}});
            stream
                .get_mut()
                .write_all(format!("{initialized}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            seen.lock().unwrap().push(request["command"].clone());
            stream
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(listener);
        let _ = std::fs::remove_file(socket);
    });
    commands
}

#[tokio::test]
async fn the_answer_is_the_exit_status() {
    let mut scenarios = tokio::task::JoinSet::new();
    for (last, code, said) in [
        (question("yes"), 0, "The owner said yes."),
        (question("no"), 1, "The owner said no."),
        (question("expired"), 1, "No answer in time"),
        (question("failed"), 2, ""),
        (question("withdrawn"), 2, ""),
        // The daemon restarted: the new one does not know the question.
        (
            json!({"type":"error","request_id":"control","code":"confirm_error","message":"no question q1 is known here; the daemon may have restarted since it was asked","fatal":false}),
            2,
            "",
        ),
    ] {
        scenarios.spawn(async move {
            let home = tempfile::tempdir().unwrap();
            let commands = scripted_daemon(
                home.path(),
                vec![question("pending"), question("pending"), last.clone()],
            );
            let output = confirm(home.path())
                .args(["--timeout", "90", "Publish SCV 0.3.0?"])
                .env("SCV_PARENT", "0a1b2c3d/session/codex-3f9a2c")
                .output()
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(code), "{last}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(said),
                "{output:?}"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("Asked the owner on wechat:default; waiting up to 2 minutes"),
                "{output:?}"
            );
            let commands = commands.lock().unwrap().clone();
            assert_eq!(
                commands[0],
                json!({"action":"confirm_ask","question":"Publish SCV 0.3.0?","parent":"0a1b2c3d/session/codex-3f9a2c","timeout_seconds":90})
            );
            assert_eq!(
                commands[1..],
                [
                    json!({"action":"confirm_status","id":"q1"}),
                    json!({"action":"confirm_status","id":"q1"})
                ]
            );
        });
    }
    while let Some(scenario) = scenarios.join_next().await {
        scenario.unwrap();
    }
}

#[tokio::test]
async fn a_daemon_that_stops_while_waiting_leaves_the_answer_unknown() {
    let home = tempfile::tempdir().unwrap();
    scripted_daemon(home.path(), vec![question("pending")]);
    let output = confirm(home.path()).arg("Publish?").output().await.unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("lost the question while waiting"),
        "{output:?}"
    );
}

#[tokio::test]
async fn nothing_is_asked_without_a_daemon_that_can_ask() {
    let home = tempfile::tempdir().unwrap();
    let output = confirm(home.path()).arg("Publish?").output().await.unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no SCV daemon is running"),
        "{output:?}"
    );
    // A daemon from before `confirm_ask` cannot parse it.
    scripted_daemon(
        home.path(),
        vec![
            json!({"type":"error","code":"invalid_json","message":"invalid protocol JSON: unknown variant `confirm_ask`","fatal":false}),
        ],
    );
    let output = confirm(home.path()).arg("Publish?").output().await.unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("too old to ask the owner"),
        "{output:?}"
    );
    // Waits outside 1 second to 4 hours are refused before asking.
    for timeout in ["0", "14401"] {
        let output = confirm(home.path())
            .args(["--timeout", timeout, "Publish?"])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{timeout}: {output:?}");
    }
}

#[tokio::test]
async fn a_daemon_with_no_owner_chat_to_ask_in_asks_nothing() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut daemon = start(home.path(), workspace.path());
    status(home.path()).await;
    // A delegated agent may ask; its chain names no delegation of this
    // daemon, so the notify target would be asked, and there is none.
    let output = confirm(home.path())
        .arg("Publish SCV 0.3.0?")
        .env("SCV_DELEGATION_DEPTH", "1")
        .env("SCV_PARENT", "0a1b2c3d/session/codex-3f9a2c")
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no owner chat to ask in"), "{stderr}");
    assert!(!stderr.contains("refused"), "{stderr}");
    terminate(&mut daemon).await;
}
