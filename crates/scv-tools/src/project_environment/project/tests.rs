//! Unit tests for `src/project_environment/project.rs`.

use super::*;

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn nearest_files_legacy_precedence_and_subdirectories() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "rust-toolchain.toml",
        "[toolchain]\nchannel='stable'",
    );
    write(root.path(), "nested/rust-toolchain", "nightly-2026-01-01");
    write(
        root.path(),
        "nested/rust-toolchain.toml",
        "[toolchain]\nchannel='beta'",
    );
    write(
        root.path(),
        "nested/Cargo.toml",
        "[package]\nrust-version='1.100'",
    );
    std::fs::create_dir(root.path().join("nested/src")).unwrap();
    let project = Project::detect(&root.path().join("nested/src")).unwrap();
    assert_eq!(
        project.toolchain.unwrap(),
        root.path().join("nested/rust-toolchain")
    );
    assert_eq!(project.minimum, Version([1, 100, 0]));
}

#[test]
fn workspace_inheritance_and_virtual_root() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[workspace.package]\nrust-version='1.88'\nedition='2024'",
    );
    write(
        root.path(),
        "member/Cargo.toml",
        "[package]\nrust-version.workspace=true\nedition.workspace=true",
    );
    for dir in [root.path().to_owned(), root.path().join("member")] {
        assert_eq!(Project::detect(&dir).unwrap().minimum, Version([1, 88, 0]));
    }
}

#[test]
fn explicit_workspace_and_edition_floor() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "common/Cargo.toml",
        "[workspace.package]\nrust-version='1.56'\nedition='2024'",
    );
    write(
        root.path(),
        "member/Cargo.toml",
        "[package]\nworkspace='../common'\nrust-version.workspace=true\nedition.workspace=true",
    );
    assert_eq!(
        Project::detect(&root.path().join("member"))
            .unwrap()
            .minimum,
        Version([1, 85, 0])
    );
}

#[test]
fn invalid_or_unresolved_requirements_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    for manifest in [
        "invalid toml",
        "[package]\nrust-version='>=1.88'",
        "[package]\nrust-version.workspace=true",
        "[package]\nedition='2030'",
        "[package]\nrust-version=1.88",
    ] {
        write(root.path(), "Cargo.toml", manifest);
        assert!(Project::detect(root.path()).is_err(), "{manifest}");
    }
    write(root.path(), "Cargo.toml", &" ".repeat(1024 * 1024 + 1));
    assert!(Project::detect(root.path()).is_err());
}

#[test]
fn rust_is_optional_and_a_minimum_is_not_hardcoded() {
    let root = tempfile::tempdir().unwrap();
    assert!(!Project::detect(root.path()).unwrap().is_rust());
    write(root.path(), "Cargo.toml", "[package]\nrust-version='1.60'");
    assert_eq!(
        Project::detect(root.path()).unwrap().minimum,
        Version([1, 60, 0])
    );
    assert!(Version::parse("2.0").unwrap() > Version::parse("1.999.9").unwrap());
    assert!(Version::parse("1.100").unwrap() > Version::parse("1.99").unwrap());
}
