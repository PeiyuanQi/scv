//! `scv restart-watchdog` against a fake `systemctl` and a fake daemon that
//! reports the version written in the installed binary.

mod common;

use common::Isolated;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Setup {
    home: tempfile::TempDir,
    binary: PathBuf,
    previous: PathBuf,
    plan: PathBuf,
    log: PathBuf,
}

/// An instance home holding a restart plan from 0.1.36 to 0.1.37 whose
/// accounts must include a connected `wechat:default`.
fn setup(to_layout: u32) -> Setup {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let binary = bin.join("scv");
    let previous = bin.join("scv.prev");
    std::fs::write(&binary, "0.1.37").unwrap();
    std::fs::write(&previous, "0.1.36").unwrap();
    let log = home.path().join("systemctl.log");
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SCV_TEST_LOG\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plan = home.path().join("state/update.json");
    std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
    std::fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "id": "t0000001",
            "state": "restarting",
            "from_version": "0.1.36",
            "to_version": "0.1.37",
            "from_layout": 1,
            "to_layout": to_layout,
            "unit": "scv-test.service",
            "binary": binary,
            "previous": previous,
            "expected": ["wechat:default"],
            "requested_unix": 1,
            "deadline_unix": 2,
            "restart_unix": 3,
            "verify_seconds": 3,
        }))
        .unwrap(),
    )
    .unwrap();
    Setup {
        home,
        binary,
        previous,
        plan,
        log,
    }
}

/// Answer status requests on the instance socket with the version written in
/// `binary` and `wechat:default` connected or not.
fn fake_daemon(socket: &Path, binary: PathBuf, connected: Arc<AtomicBool>) {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let binary = binary.clone();
            let connected = Arc::clone(&connected);
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let init: Value = serde_json::from_str(&line).unwrap();
                let reply = json!({"type":"initialized","request_id":init["request_id"],"protocol_version":init["protocol_version"],"server":{"name":"fake","version":"0"}});
                stream
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                stream.read_line(&mut line).await.unwrap();
                let version = std::fs::read_to_string(&binary).unwrap();
                let state = if connected.load(Ordering::Acquire) {
                    "connected"
                } else {
                    "backoff"
                };
                let status = json!({"type":"daemon.status","request_id":"control","status":{
                    "version": version, "pid": 1,
                    "components": [{"id":"wechat:default","channel":"wechat","account":"default","bot_id":null,"user_id":null,"enabled":true,"state":state,"last_success_unix_seconds":null,"error":null,"restarts":0}]
                }});
                stream
                    .get_mut()
                    .write_all(format!("{status}\n").as_bytes())
                    .await
                    .unwrap();
            });
        }
    });
}

async fn run_watchdog(setup: &Setup, connected: bool) -> (std::process::Output, Value) {
    fake_daemon(
        &setup.home.path().join("state/server.sock"),
        setup.binary.clone(),
        Arc::new(AtomicBool::new(connected)),
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(setup.home.path())
        .args(["restart-watchdog", "--plan"])
        .arg(&setup.plan)
        .env("PATH", setup.binary.parent().unwrap())
        .env("SCV_TEST_LOG", &setup.log)
        .output()
        .await
        .unwrap();
    let plan = serde_json::from_slice(&std::fs::read(&setup.plan).unwrap()).unwrap();
    (output, plan)
}

fn restarts(setup: &Setup) -> Vec<String> {
    std::fs::read_to_string(&setup.log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_release_that_comes_up_with_its_channels_is_verified() {
    let setup = setup(1);
    let (output, plan) = run_watchdog(&setup, true).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(plan["state"], "verified");
    assert_eq!(restarts(&setup), ["--user restart scv-test.service"]);
    assert_eq!(std::fs::read_to_string(&setup.binary).unwrap(), "0.1.37");
}

#[tokio::test]
async fn a_release_whose_channels_stay_down_is_rolled_back() {
    let setup = setup(1);
    let (output, plan) = run_watchdog(&setup, false).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(plan["state"], "rolled_back");
    let detail = plan["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("v0.1.37 started, but wechat:default did not reconnect"),
        "{detail}"
    );
    // The previous binary is back, and the unit was restarted into it.
    assert_eq!(std::fs::read_to_string(&setup.binary).unwrap(), "0.1.36");
    assert!(setup.previous.is_file());
    assert_eq!(
        restarts(&setup),
        [
            "--user restart scv-test.service",
            "--user restart scv-test.service"
        ]
    );
}

#[tokio::test]
async fn a_release_with_another_config_layout_is_not_rolled_back() {
    let setup = setup(2);
    let (output, plan) = run_watchdog(&setup, false).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(plan["state"], "failed");
    let detail = plan["detail"].as_str().unwrap();
    assert!(detail.contains("not rolled back"), "{detail}");
    assert!(detail.contains("config layout 2"), "{detail}");
    assert_eq!(std::fs::read_to_string(&setup.binary).unwrap(), "0.1.37");
    assert_eq!(restarts(&setup), ["--user restart scv-test.service"]);
}
