//! Repository guards that run with the tests: integration tests never read the
//! developer's real SCV home, and unit tests live where docs/quality.md
//! ("Test layout") puts them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[test]
fn every_spawned_scv_binary_is_isolated() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
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

/// Source files that still hold inline unit tests. They predate the test
/// layout rule and move to their `tests.rs` one crate at a time; the list may
/// only shrink.
const INLINE_TEST_ALLOWLIST: &[&str] = &[
    "crates/scv-channels/src/hub.rs",
    "crates/scv-channels/src/media.rs",
    "crates/scv-channels/src/session.rs",
    "crates/scv-channels/src/state.rs",
    "crates/scv-clawbot/src/bridge.rs",
    "crates/scv-clawbot/src/lib.rs",
    "crates/scv-clawbot/src/lifecycle_tests.rs",
    "crates/scv-clawbot/src/media.rs",
    "crates/scv-clawbot/src/state.rs",
    "crates/scv-core/src/lib.rs",
    "crates/scv-feishu/src/api.rs",
    "crates/scv-feishu/src/frame.rs",
    "crates/scv-feishu/src/inbound.rs",
    "crates/scv-feishu/src/state.rs",
    "crates/scv-protocol/src/lib.rs",
    "crates/scv-provider-openai/src/lib.rs",
    "crates/scv-server/src/agents.rs",
    "crates/scv-server/src/attachments.rs",
    "crates/scv-server/src/components.rs",
    "crates/scv-server/src/config.rs",
    "crates/scv-server/src/imports.rs",
    "crates/scv-server/src/lib.rs",
    "crates/scv-server/src/overview.rs",
    "crates/scv-server/src/restart.rs",
    "crates/scv-tools/src/acp_agent.rs",
    "crates/scv-tools/src/adapters.rs",
    "crates/scv-tools/src/agent_choice.rs",
    "crates/scv-tools/src/agent_output.rs",
    "crates/scv-tools/src/agent_progress.rs",
    "crates/scv-tools/src/background.rs",
    "crates/scv-tools/src/chat_attach.rs",
    "crates/scv-tools/src/conversation.rs",
    "crates/scv-tools/src/delegation.rs",
    "crates/scv-tools/src/lib.rs",
    "crates/scv-tools/src/live.rs",
    "crates/scv-tools/src/scv_agent.rs",
    "crates/scv-tools/src/web.rs",
    "crates/scv-tui/src/lib.rs",
];

#[test]
fn unit_tests_live_in_tests_rs_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    rust_files(&root.join("src"), &mut sources);
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        rust_files(&entry.unwrap().path().join("src"), &mut sources);
    }
    let mut misplaced = Vec::new();
    let mut unused = Vec::new();
    let mut allowed = BTreeSet::new();
    for path in &sources {
        let relative = relative(root, path);
        let source = std::fs::read_to_string(path).unwrap();
        if in_test_module(&relative) {
            if !included(path) {
                unused.push(relative);
            }
        } else if holds_tests(&source) {
            if INLINE_TEST_ALLOWLIST.contains(&relative.as_str()) {
                allowed.insert(relative);
            } else {
                misplaced.push(relative);
            }
        }
    }
    let stale: Vec<_> = INLINE_TEST_ALLOWLIST
        .iter()
        .filter(|path| !allowed.contains(**path))
        .collect();
    assert!(
        misplaced.is_empty(),
        "move these unit tests into the module's `tests.rs` (`#[cfg(test)] mod tests;`, see \
         docs/quality.md): {misplaced:?}"
    );
    assert!(
        unused.is_empty(),
        "these test files are not declared by their parent module, so they never run: {unused:?}"
    );
    assert!(
        stale.is_empty(),
        "these files no longer hold inline tests; remove them from INLINE_TEST_ALLOWLIST: {stale:?}"
    );
    assert!(
        INLINE_TEST_ALLOWLIST
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "keep INLINE_TEST_ALLOWLIST sorted and free of duplicates"
    );
}

fn rust_files(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// `path` relative to the repository root, with `/` separators.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A `tests.rs`, or a file under a `tests/` directory inside `src/`.
fn in_test_module(relative: &str) -> bool {
    let parts: Vec<&str> = relative.split('/').collect();
    let inside_src = parts
        .iter()
        .position(|part| *part == "src")
        .map_or(&parts[..0], |index| &parts[index + 1..parts.len() - 1]);
    parts.last() == Some(&"tests.rs") || inside_src.contains(&"tests")
}

fn holds_tests(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("#[test]") || line.starts_with("#[tokio::test")
    })
}

/// Whether the module that owns this test file declares it: `src/tests.rs`
/// by `lib.rs` or `main.rs`, `src/foo/tests.rs` by `src/foo.rs` or
/// `src/foo/mod.rs`, and `src/foo/tests/bar.rs` by `src/foo/tests.rs`.
fn included(path: &Path) -> bool {
    let directory = path.parent().unwrap();
    let stem = path.file_stem().unwrap().to_string_lossy();
    let parents: Vec<PathBuf> = if stem == "tests" {
        let mut parents = vec![directory.join("mod.rs")];
        if directory.file_name().is_some_and(|name| name == "src") {
            parents.extend([directory.join("lib.rs"), directory.join("main.rs")]);
        } else {
            parents.push(directory.with_extension("rs"));
        }
        parents
    } else {
        vec![directory.with_extension("rs"), directory.join("mod.rs")]
    };
    parents.iter().any(|parent| {
        std::fs::read_to_string(parent).is_ok_and(|source| {
            source
                .lines()
                .any(|line| line.trim() == format!("mod {stem};"))
        })
    })
}
