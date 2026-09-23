//! Keeps integration tests from reading the developer's real SCV home.

#[test]
fn every_spawned_scv_binary_is_isolated() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut unisolated = Vec::new();
    for entry in std::fs::read_dir(&tests).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "rs")
            || path
                .file_name()
                .is_some_and(|name| name == "isolation_guard.rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let mut rest = source.as_str();
        while let Some(start) = rest.find("CARGO_BIN_EXE_") {
            let spawn = &rest[start..];
            let end = ["spawn()", "output()", "status()"]
                .iter()
                .filter_map(|call| spawn.find(call))
                .min()
                .unwrap_or(spawn.len());
            if !spawn[..end].contains(".isolated(") {
                let line = source[..source.len() - spawn.len()].lines().count();
                unisolated.push(format!("{}:{line}", path.display()));
            }
            rest = &spawn["CARGO_BIN_EXE_".len()..];
        }
    }
    assert!(
        unisolated.is_empty(),
        "spawn SCV binaries with `common::Isolated::isolated(home)`: {unisolated:?}"
    );
}
