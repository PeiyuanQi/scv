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
        layout.outbox(),
        layout.update_plan(),
        layout.last_owner(),
        layout.daemon_marker(),
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

/// Files an earlier release wrote, which a planned restart and its rollback
/// read across releases: their paths are part of the on-disk layout.
#[test]
fn restart_and_media_files_keep_their_paths() {
    let layout = Layout::new("/h");
    assert_eq!(layout.update_plan(), Path::new("/h/state/update.json"));
    assert_eq!(layout.last_owner(), Path::new("/h/state/last-owner.json"));
    assert_eq!(layout.daemon_marker(), Path::new("/h/state/daemon.json"));
    assert_eq!(layout.outbox(), Path::new("/h/state/media/outbox"));
    assert!(layout.outbox().starts_with(layout.media()));
}

/// The unit name is how `scv restart`, `scv update`, and the planned-restart
/// watchdog find the unit an earlier release wrote, so its hash must never
/// change.
#[test]
fn service_names_are_stable() {
    assert_eq!(
        Layout::new("/srv/scv").service_name(),
        "scv-8973cbc5732311b1.service"
    );
    let default = Layout {
        home: PathBuf::from("/home/user/.scv"),
        default: true,
    };
    assert!(default.is_default());
    assert_eq!(default.service_name(), "scv.service");
    assert!(!Layout::new("/home/user/.scv").is_default());
}

#[test]
fn homes_resolve_to_the_canonical_path_when_they_exist() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        resolve(link).unwrap(),
        std::fs::canonicalize(&real).unwrap()
    );
    let missing = root.path().join("missing");
    assert_eq!(resolve(missing.clone()).unwrap(), missing);
    assert_eq!(
        resolve(PathBuf::from("relative-missing")).unwrap(),
        std::env::current_dir().unwrap().join("relative-missing")
    );
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
