//! Project environment doctor: offline diagnostics and no instance writes.

use super::support::Isolated as _;
use std::{os::unix::fs::PermissionsExt as _, process::Command};

#[test]
fn doctor_needs_no_agent_or_rust_in_non_rust_projects_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let instance = root.path().join("instance");
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .env_clear()
        .isolated(&instance)
        .env("PATH", "/nonexistent")
        .args(["agents", "doctor", "--workspace"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("No Rust project detected"));
    assert!(!instance.exists());
}

#[test]
fn doctor_fails_before_agent_launch_when_system_rust_is_obsolete() {
    let root = tempfile::tempdir().unwrap();
    let instance = root.path().join("instance");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nrust-version='1.88'",
    )
    .unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for tool in ["rustc", "cargo"] {
        let path = bin.join(tool);
        std::fs::write(&path, format!("#!/bin/sh\necho '{tool} 1.75.0'\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .env_clear()
        .isolated(&instance)
        .env("PATH", &bin)
        .args(["agents", "doctor", "claude", "--workspace"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("obsolete tools"), "{error}");
    assert!(error.contains("1.88"), "{error}");
    assert!(!instance.exists());
}
