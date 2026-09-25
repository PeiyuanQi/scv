//! Unit tests for src/fs.rs.

use super::*;

#[test]
fn replace_private_writes_a_user_only_file_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    replace_private(&path, b"first").unwrap();
    replace_private(&path, b"second").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"second");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
}

#[test]
fn replace_private_needs_an_existing_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(replace_private(&dir.path().join("missing/state.json"), b"x").is_err());
}
