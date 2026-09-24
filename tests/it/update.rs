use crate::support::Isolated;
use std::{os::unix::fs::PermissionsExt, process::Command};

fn update(install_succeeds: bool) -> (std::process::Output, String) {
    let temp = tempfile::tempdir().unwrap();
    let cargo = temp.path().join("cargo");
    let systemctl = temp.path().join("systemctl");
    let log = temp.path().join("commands");
    std::fs::write(
        &cargo,
        format!(
            "#!/bin/sh\nprintf 'cargo %s\\n' \"$*\" >> \"$SCV_TEST_LOG\"\nexit {}\n",
            if install_succeeds { 0 } else { 1 }
        ),
    )
    .unwrap();
    std::fs::write(
        &systemctl,
        "#!/bin/sh\nprintf 'systemctl %s\\n' \"$*\" >> \"$SCV_TEST_LOG\"\nexit 0\n",
    )
    .unwrap();
    for path in [&cargo, &systemctl] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(temp.path())
        .args(["update", "--index-url", "https://example.invalid/index"])
        .env("PATH", temp.path())
        .env("SCV_TEST_LOG", &log)
        .current_dir(temp.path())
        .output()
        .unwrap();
    (output, std::fs::read_to_string(log).unwrap())
}

#[test]
fn successful_install_restarts_active_daemon_after_cargo_finishes() {
    let (output, log) = update(true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut lines = log.lines();
    assert_eq!(
        lines.next(),
        Some("cargo install --locked --force --index https://example.invalid/index scv-cli")
    );
    let active = lines.next().unwrap();
    let service = active
        .strip_prefix("systemctl --user is-active --quiet ")
        .unwrap();
    assert!(service.starts_with("scv-") && service.ends_with(".service"));
    assert_eq!(
        lines.next(),
        Some(format!("systemctl --user restart {service}").as_str())
    );
    assert!(lines.next().is_none());
}

#[test]
fn failed_install_never_restarts_daemon() {
    let (output, log) = update(false);
    assert!(!output.status.success());
    assert!(!log.contains("systemctl"));
}
