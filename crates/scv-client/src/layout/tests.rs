//! Unit tests for `src/layout.rs`.

use super::*;

#[test]
fn every_path_lives_under_one_of_the_top_level_entries() {
    let layout = Layout::new("/h");
    for path in [
        layout.config(),
        layout.channel_credentials("wechat"),
        layout.agent_home("codex"),
        layout.skills(),
        layout.socket(),
        layout.delegations(),
        layout.conversations(),
        layout.imports(),
        layout.channel_state("feishu"),
        layout.config_lock(),
    ] {
        let top = path
            .strip_prefix("/h")
            .unwrap()
            .components()
            .next()
            .unwrap();
        let top = top.as_os_str().to_str().unwrap();
        assert!(ENTRIES.contains(&top), "{}", path.display());
    }
    assert_eq!(layout.socket(), Path::new("/h/state/server.sock"));
}

#[test]
fn strays_name_old_layout_paths_and_unknown_files() {
    let home = tempfile::tempdir().unwrap();
    for directory in ["state", "agents", "adapters", "notes"] {
        std::fs::create_dir(home.path().join(directory)).unwrap();
    }
    for file in ["config.toml", "server.sock", "config.toml.bak"] {
        std::fs::write(home.path().join(file), "").unwrap();
    }
    let strays = Layout::new(home.path()).strays().unwrap();
    let named: Vec<(String, bool)> = strays
        .iter()
        .map(|stray| {
            (
                stray
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                stray.legacy,
            )
        })
        .collect();
    assert_eq!(
        named,
        [
            ("adapters".to_owned(), true),
            ("config.toml.bak".to_owned(), false),
            ("notes".to_owned(), false),
            ("server.sock".to_owned(), true),
        ]
    );
    assert!(
        Layout::new(home.path().join("missing"))
            .strays()
            .unwrap()
            .is_empty()
    );
}
