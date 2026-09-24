//! Unit tests for `src/state.rs`.

use super::*;

/// Credentials identified by `id` alone, so a new `secret` rotates them.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Test {
    id: String,
    secret: String,
}

impl Credentials for Test {
    fn fingerprint(&self) -> Result<String> {
        Ok(format!("id:{}", self.id))
    }
}

fn credentials(id: &str) -> Test {
    Test {
        id: id.into(),
        secret: "secret".into(),
    }
}

fn store(directory: &Path) -> Store<Test> {
    Store::new(&Layout::new(directory), "test")
}

fn mode(path: impl AsRef<Path>) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn transaction_contention_fails_promptly_without_partial_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_owned();
    let store = store(&root);
    let original = credentials("a");
    store.save_account("default", &original).unwrap();
    let transaction = store.transaction("default").unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let store = self::store(&root);
        let results = [
            store
                .save_account(
                    "default",
                    &Test {
                        secret: "rotated".into(),
                        ..credentials("a")
                    },
                )
                .err()
                .unwrap(),
            store.remove("default").err().unwrap(),
            store.account_snapshot("default").err().unwrap(),
            store
                .save_settings(
                    "default",
                    &AccountSettings {
                        enabled: false,
                        ..Default::default()
                    },
                )
                .err()
                .unwrap(),
        ];
        let kinds = results.map(|error| error.downcast_ref::<std::io::Error>().unwrap().kind());
        send.send(kinds).unwrap();
    });
    let result = receive.recv_timeout(std::time::Duration::from_secs(1));
    drop(transaction);
    worker.join().unwrap();
    assert_eq!(result.unwrap(), [std::io::ErrorKind::WouldBlock; 4]);
    assert!(store.account("default").unwrap().unwrap() == original);
    assert!(store.settings("default").unwrap().enabled);
    store.remove("default").unwrap();
    assert!(store.account_snapshot("default").unwrap().0.is_none());
    store.save_account("default", &credentials("a")).unwrap();
}

#[test]
fn pending_state_reads_legacy_shape() {
    let state: BridgeState = serde_json::from_str(
        r#"{"cursor":"c","seen":["m"],"pending":{"to_user_id":"u","context_token":"x","reply":"hello"}}"#,
    ).unwrap();
    assert!(state.in_flight.is_empty());
    let [pending] = state.pending.as_slice() else {
        panic!("one legacy pending reply")
    };
    assert!(pending.client_ids.is_empty());
    assert_eq!(pending.next_chunk, 0);
    assert!(pending.key.is_empty() && pending.carried.is_empty() && !pending.transient);
    let claim: BridgeState = serde_json::from_str(
        r#"{"cursor":"c","seen":[],"pending":null,"in_flight":{"message_id":"m","to_user_id":"u","context_token":"x"}}"#,
    )
    .unwrap();
    assert_eq!(claim.in_flight.len(), 1);
    assert!(claim.pending.is_empty());
}

#[test]
fn single_entries_keep_the_legacy_shape_and_many_become_lists() {
    let claim = |id: &str| InFlight {
        message_id: id.into(),
        to_user_id: "u".into(),
        context_token: "x".into(),
        key: String::new(),
    };
    let mut state = BridgeState::default();
    let value = serde_json::to_value(&state).unwrap();
    assert!(value["pending"].is_null() && value["in_flight"].is_null());
    assert!(value.get("held").is_none());
    state.in_flight.push(claim("a"));
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value["in_flight"]["message_id"], "a");
    assert!(value["in_flight"].get("key").is_none());
    state.in_flight.push(claim("b"));
    let restored: BridgeState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    let ids: Vec<_> = restored
        .in_flight
        .iter()
        .map(|c| c.message_id.as_str())
        .collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn settings_live_in_config_toml_and_keep_the_rest_of_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let store = store(directory.path());
    assert!(store.settings("default").unwrap() == AccountSettings::default());
    let empty: AccountSettings = serde_json::from_str("{}").unwrap();
    assert!(empty.enabled);
    assert!(empty.workspace.is_none());
    assert_eq!(empty.remote_tools, RemoteTools::None);

    let config = directory.path().join("config.toml");
    let original = "# my provider\n[provider]\nactive = \"openai\" # keep this\n";
    atomic_write(&config, original).unwrap();
    let settings = AccountSettings {
        enabled: false,
        workspace: Some(directory.path().join("workspace")),
        remote_tools: RemoteTools::Owner,
        media: MediaSettings::default(),
    };
    store.save_settings("default", &settings).unwrap();
    assert!(store.settings("default").unwrap() == settings);
    // Default media limits are not written; changed ones round-trip.
    assert!(!std::fs::read_to_string(&config).unwrap().contains("media"));
    let limited = AccountSettings {
        media: MediaSettings {
            owner_max_mib: 10,
            others_image_max_mib: 0,
            keep_days: 2,
        },
        ..settings.clone()
    };
    store.save_settings("default", &limited).unwrap();
    assert!(store.settings("default").unwrap() == limited);
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("owner_max_mib = 10"), "{text}");
    store.save_settings("default", &settings).unwrap();

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with(original), "{text}");
    assert!(
        text.contains("[channels.test.default]\nenabled = false\n"),
        "{text}"
    );
    assert!(!text.contains("[channels]\n"), "{text}");
    assert_eq!(mode(&config), 0o600);

    // A person's edit is read back, and their own keys and comments survive
    // SCV's next change.
    let edited = text.replace("enabled = false", "enabled = true # on again");
    atomic_write(&config, &edited).unwrap();
    assert!(store.settings("default").unwrap().enabled);
    store
        .save_settings(
            "default",
            &AccountSettings {
                workspace: None,
                ..settings.clone()
            },
        )
        .unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("# keep this") && !text.contains("workspace"),
        "{text}"
    );

    for invalid in [
        "[channels.test.default]\nenabeld = false\n",
        "[channels.test.default]\nremote_tools = \"everyone\"\n",
        "[channels]\ntest = 1\n",
    ] {
        atomic_write(&config, invalid).unwrap();
        assert!(store.settings("default").is_err(), "{invalid}");
    }
    // Parse errors name the line but never quote the file, which holds keys.
    atomic_write(&config, "api_key = \"sk-secret\"\nbroken =\n").unwrap();
    let error = store.settings("default").unwrap_err().to_string();
    assert!(
        error.contains("line 2") && !error.contains("sk-secret"),
        "{error}"
    );
    assert!(store.save_settings("default", &settings).is_err());
    assert!(store.save_settings("../escape", &settings).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        atomic_write(&config, "").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.settings("default").is_err());
    }
}

#[test]
fn files_follow_the_instance_layout_privately() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    let store = store(home);
    store.save_account("default", &credentials("a")).unwrap();
    store
        .save_settings("default", &AccountSettings::default())
        .unwrap();
    let _lock = store.lock("default").unwrap();
    for (path, expected) in [
        (home.join("credentials"), 0o700),
        (home.join("credentials/test"), 0o700),
        (home.join("credentials/test/default.json"), 0o600),
        (home.join("state"), 0o700),
        (home.join("state/channels"), 0o700),
        (home.join("state/channels/test"), 0o700),
        (home.join("state/channels/test/default.json"), 0o600),
        (home.join("state/channels/test/default.lock"), 0o600),
        (home.join("state/channels/test/default.transaction"), 0o600),
        (home.join("config.toml"), 0o600),
    ] {
        assert_eq!(mode(&path), expected, "{}", path.display());
    }
}

#[test]
fn discovery_is_bounded_and_ignores_bad_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let store = store(directory.path());
    assert!(store.account_names().unwrap().is_empty());
    private_directory(directory.path(), &store.credentials).unwrap();
    atomic_write(&store.credentials_path("broken").unwrap(), "not JSON").unwrap();
    atomic_write(&store.credentials.join("invalid.name.json"), "{}").unwrap();
    assert_eq!(store.account_names().unwrap(), vec!["broken"]);
    assert!(store.account("broken").is_err());
    for index in 0..140 {
        atomic_write(
            &store.credentials_path(&format!("account-{index}")).unwrap(),
            "{}",
        )
        .unwrap();
    }
    assert!(store.account_names().is_err());
}

#[test]
fn removal_clears_one_account_and_preserves_others() {
    let directory = tempfile::tempdir().unwrap();
    let store = store(directory.path());
    let account = credentials("a");
    store.save_account("default", &account).unwrap();
    store.save_account("other", &account).unwrap();
    for name in ["default", "other"] {
        store
            .save_settings(name, &AccountSettings::default())
            .unwrap();
    }
    store
        .save_state("default", &BridgeState::default())
        .unwrap();
    store.remove("default").unwrap();
    assert!(store.account("default").unwrap().is_none());
    assert!(!store.credentials_path("default").unwrap().exists());
    assert!(!store.state_path("default").unwrap().exists());
    let config = std::fs::read_to_string(directory.path().join("config.toml")).unwrap();
    assert!(!config.contains("[channels.test.default]"), "{config}");
    assert!(config.contains("[channels.test.other]"), "{config}");
    assert!(store.account("other").unwrap().unwrap() == account);
    store.remove("other").unwrap();
    let config = std::fs::read_to_string(directory.path().join("config.toml")).unwrap();
    assert!(!config.contains("channels"), "{config}");
}

#[test]
fn account_lock_excludes_other_runs_and_removal() {
    let directory = tempfile::tempdir().unwrap();
    let store = store(directory.path());
    let lock = store.lock("default").unwrap();
    assert!(store.lock("default").is_err());
    assert!(store.remove("default").is_err());
    assert!(store.lock("other").is_ok());
    drop(lock);
    assert!(store.lock("default").is_ok());
    store.remove("default").unwrap();
}
