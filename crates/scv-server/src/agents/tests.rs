//! Unit tests for `src/agents.rs`.

use super::*;

const CONFIG: &str = r#"model_provider = "relay"
model = "gpt-test"
sandbox_mode = "danger-full-access"
approval_policy = "never"

[model_providers.relay]
base_url = "https://relay.invalid"
wire_api = "responses"
requires_openai_auth = true
experimental_bearer_token = "sk-bearer-secret"
"#;

fn homes() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn copies_config_and_api_key_privately_without_printing_secrets() {
    let (source, destination) = homes();
    std::fs::write(source.path().join("config.toml"), CONFIG).unwrap();
    let auth = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-auth-secret"}"#;
    std::fs::write(source.path().join("auth.json"), auth).unwrap();
    let notes = import_codex(source.path(), destination.path()).unwrap();
    for file in ["config.toml", "auth.json"] {
        let copied = destination.path().join(file);
        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            std::fs::read_to_string(source.path().join(file)).unwrap()
        );
        assert_eq!(mode(&copied), 0o600);
    }
    let notes = notes.join("\n");
    assert!(notes.contains(r#"model_provider "relay", model "gpt-test""#));
    assert!(notes.contains(r#"sandbox_mode "danger-full-access" and approval_policy "never""#));
    assert!(notes.contains("API-key sign-in"));
    assert!(!notes.contains("secret"));
}

#[test]
fn never_copies_a_chatgpt_session() {
    let (source, destination) = homes();
    std::fs::write(source.path().join("config.toml"), "model = \"m\"\n").unwrap();
    std::fs::write(
        source.path().join("auth.json"),
        r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"refresh_token":"rt"}}"#,
    )
    .unwrap();
    let notes = import_codex(source.path(), destination.path()).unwrap();
    assert!(destination.path().join("config.toml").is_file());
    assert!(!destination.path().join("auth.json").exists());
    assert!(notes.join("\n").contains("scv agents login codex"));
}

#[test]
fn env_key_providers_are_flagged() {
    let (source, destination) = homes();
    std::fs::write(
        source.path().join("config.toml"),
        "[model_providers.a]\nenv_key = \"OPENAI_API_KEY\"\n\
             [model_providers.b]\nenv_key = \"RELAY_KEY\"\n",
    )
    .unwrap();
    let notes = import_codex(source.path(), destination.path())
        .unwrap()
        .join("\n");
    assert!(notes.contains("$OPENAI_API_KEY, which SCV removes"));
    assert!(notes.contains("$RELAY_KEY; the SCV daemon's environment must provide it"));
}

const DSH: KeyStore = KeyStore::DshRefs {
    path: ".dsh/.credentials.yaml",
    variable: "DEEPSEEK_API_KEY",
};
const PI: KeyStore = KeyStore::Pi { dir: ".pi/agent" };
const GROK: KeyStore = KeyStore::Grok {
    auth: ".grok/auth.json",
    config: ".grok/config.toml",
};

const GROK_CONFIG: &str = r#"[cli]
installer = "internal"

# The relay profile.
[model.relay]
model = "grok-4.5"
base_url = "https://relay.invalid"
api_key = "xai-profile-secret"
api_backend = "responses"

[model."relay-4.7"]
model = "grok-4.7"
base_url = "https://relay.invalid"
api_key = "xai-profile-secret"

[models]
default = "relay-4.7"
"#;

fn grok_status_of(config: &str) -> (bool, String) {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".grok")).unwrap();
    std::fs::write(home.path().join(".grok/config.toml"), config).unwrap();
    let StoredStatus { ready, lines } = stored_status(GROK, home.path()).unwrap();
    (ready, lines.join("\n"))
}

#[test]
fn grok_config_keys_count_as_signed_in_without_printing_them() {
    let (ready, lines) = grok_status_of(GROK_CONFIG);
    assert!(ready);
    assert_eq!(lines, r#"signed in (API key in config, model "relay-4.7")"#);
    // The default may name a model id instead of a catalog key.
    let (ready, _) =
        grok_status_of(&GROK_CONFIG.replace(r#"default = "relay-4.7""#, r#"default = "grok-4.5""#));
    assert!(ready);
    // A default without a key, an unknown default, and no default at all.
    let keyless = "[model.m]\nmodel = \"grok-4.7\"\n[models]\ndefault = \"m\"\n";
    let (ready, lines) = grok_status_of(keyless);
    assert!(!ready);
    assert!(lines.contains(r#"default model "m" has no api_key in config"#));
    assert!(!grok_status_of("[models]\ndefault = \"grok-9\"\n").0);
    assert!(!grok_status_of("[cli]\ninstaller = \"internal\"\n").0);
    // A key variable SCV removes from delegated agents does not count.
    let removed = "[model.m]\nenv_key = [\"XAI_API_KEY\"]\n[models]\ndefault = \"m\"\n";
    let (ready, lines) = grok_status_of(removed);
    assert!(!ready);
    assert!(lines.contains("$XAI_API_KEY"), "{lines}");
    // An unparsable config is an error that never echoes its content.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".grok")).unwrap();
    std::fs::write(
        home.path().join(".grok/config.toml"),
        "api_key = xai-secret",
    )
    .unwrap();
    let error = stored_status(GROK, home.path()).unwrap_err().to_string();
    assert!(!error.contains("secret"), "{error}");
}

#[test]
fn grok_import_merges_by_table_privately_without_printing_keys() {
    let (source, scv) = homes();
    let destination = scv.path().join(".grok");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(source.path().join("config.toml"), GROK_CONFIG).unwrap();
    std::fs::write(
        source.path().join("auth.json"),
        r#"{"a":{"key":"xai-token-secret"}}"#,
    )
    .unwrap();
    // Grok's own state in SCV's copy survives; a stale user table does not.
    std::fs::write(
        destination.join("config.toml"),
        "[marketplace]\ndefault_skills_installs_purged = true\n\n[cli]\ninstaller = \"old\"\n",
    )
    .unwrap();
    let notes = import_grok(source.path(), &destination).unwrap().join("\n");
    let copied = destination.join("config.toml");
    assert_eq!(mode(&copied), 0o600);
    let text = std::fs::read_to_string(&copied).unwrap();
    assert!(text.starts_with(GROK_CONFIG), "user formatting is kept");
    let table: toml::Table = text.parse().unwrap();
    assert_eq!(table["cli"]["installer"].as_str(), Some("internal"));
    assert_eq!(
        table["marketplace"]["default_skills_installs_purged"].as_bool(),
        Some(true)
    );
    assert_eq!(table["models"]["default"].as_str(), Some("relay-4.7"));
    assert!(!destination.join("auth.json").exists());
    assert!(notes.contains(r#"default model "relay-4.7"; profiles "relay", "relay-4.7""#));
    assert!(notes.contains(r#"Kept SCV-only settings: "marketplace""#));
    assert!(notes.contains("Skipped auth.json"));
    assert!(!notes.contains("secret"), "{notes}");
    assert!(stored_status(GROK, scv.path()).unwrap().ready);
}

#[test]
fn grok_import_writes_nothing_when_either_file_is_invalid() {
    let (source, scv) = homes();
    let destination = scv.path().join(".grok");
    std::fs::create_dir(&destination).unwrap();
    let existing = "[marketplace]\nkept = true\n";
    std::fs::write(destination.join("config.toml"), existing).unwrap();
    std::fs::write(source.path().join("config.toml"), "api_key = xai-secret").unwrap();
    let error = import_grok(source.path(), &destination)
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret"), "{error}");
    assert_eq!(
        std::fs::read_to_string(destination.join("config.toml")).unwrap(),
        existing
    );
    std::fs::write(source.path().join("config.toml"), GROK_CONFIG).unwrap();
    std::fs::write(destination.join("config.toml"), "broken = [").unwrap();
    assert!(import_grok(source.path(), &destination).is_err());
    assert_eq!(
        std::fs::read_to_string(destination.join("config.toml")).unwrap(),
        "broken = ["
    );
    assert!(import_grok(&destination, &destination).is_err());
    let empty = tempfile::tempdir().unwrap();
    assert!(import_grok(empty.path(), &destination).is_err());
}

#[test]
fn grok_import_keeps_top_level_values_outside_the_users_tables() {
    let (source, scv) = homes();
    let destination = scv.path().join(".grok");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(source.path().join("config.toml"), GROK_CONFIG).unwrap();
    std::fs::write(destination.join("config.toml"), "version = 3\n").unwrap();
    import_grok(source.path(), &destination).unwrap();
    let table: toml::Table = std::fs::read_to_string(destination.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(table["version"].as_integer(), Some(3));
    assert!(table["models"].get("version").is_none());
    assert_eq!(table["models"]["default"].as_str(), Some("relay-4.7"));
}

fn endpoint() -> Endpoint {
    Endpoint {
        base_url: "https://relay.invalid/v1/".into(),
        api: WireApi::Responses,
        model: "gpt-test".into(),
    }
}

#[test]
fn dsh_keys_are_stored_privately_in_its_native_file() {
    let home = tempfile::tempdir().unwrap();
    assert!(!stored_status(DSH, home.path()).unwrap().ready);
    let notes = store_key(DSH, home.path(), "sk-dsh-secret").unwrap();
    assert!(!notes.join("\n").contains("secret"));
    let path = home.path().join(".dsh/.credentials.yaml");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "version: 1\n\nrefs:\n  DEEPSEEK_API_KEY: \"sk-dsh-secret\"\n"
    );
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&home.path().join(".dsh")), 0o700);
    let StoredStatus { ready, lines } = stored_status(DSH, home.path()).unwrap();
    assert!(ready);
    assert!(!lines.join("\n").contains("secret"));
    for invalid in ["", "two words", "quote\"d", "line\nbreak"] {
        assert!(store_key(DSH, home.path(), invalid).is_err(), "{invalid:?}");
    }
    assert_eq!(
        remove_stored(DSH, home.path()).unwrap(),
        ["Removed .dsh/.credentials.yaml"]
    );
    assert!(!path.exists());
    assert!(!stored_status(DSH, home.path()).unwrap().ready);
}

#[test]
fn grok_login_entries_report_sign_in_without_values() {
    let home = tempfile::tempdir().unwrap();
    let store = GROK;
    assert!(!stored_status(store, home.path()).unwrap().ready);
    std::fs::create_dir(home.path().join(".grok")).unwrap();
    std::fs::write(home.path().join(".grok/auth.json"), "{}").unwrap();
    assert!(!stored_status(store, home.path()).unwrap().ready);
    std::fs::write(
        home.path().join(".grok/auth.json"),
        r#"{"https://auth.x.ai::id":{"key":"xai-token-secret"}}"#,
    )
    .unwrap();
    let StoredStatus { ready, lines } = stored_status(store, home.path()).unwrap();
    assert!(ready);
    assert!(!lines.join("\n").contains("secret"));
    std::fs::write(home.path().join(".grok/auth.json"), "xai-token-secret").unwrap();
    let error = stored_status(store, home.path()).unwrap_err().to_string();
    assert!(!error.contains("secret"), "{error}");
}

#[test]
fn pi_endpoints_merge_into_pi_files_as_the_private_default() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".pi/agent");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("models.json"),
            r#"{"providers":{"ollama":{"baseUrl":"http://localhost:11434/v1","api":"openai-completions","models":[{"id":"q"}]}}}"#,
        )
        .unwrap();
    std::fs::write(dir.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
    let notes = configure_pi_endpoint(&dir, &endpoint(), "sk-pi-secret").unwrap();
    let notes = notes.join("\n");
    assert!(
        notes.contains("openai-responses at relay.invalid"),
        "{notes}"
    );
    assert!(!notes.contains("secret"));

    let read = |file: &str| -> serde_json::Value {
        let path = dir.join(file);
        assert_eq!(mode(&path), 0o600, "{file}");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    let models = read("models.json");
    assert_eq!(
        models["providers"]["scv"],
        serde_json::json!({
            "baseUrl": "https://relay.invalid/v1",
            "api": "openai-responses",
            "models": [{"id": "gpt-test"}],
            "compat": {"sessionAffinityFormat": "openai-nosession"},
        })
    );
    assert_eq!(models["providers"]["ollama"]["models"][0]["id"], "q");
    assert_eq!(
        read("auth.json")["scv"],
        serde_json::json!({"type": "api_key", "key": "sk-pi-secret"})
    );
    let settings = read("settings.json");
    assert_eq!(settings["defaultProvider"], "scv");
    assert_eq!(settings["defaultModel"], "gpt-test");
    assert_eq!(settings["theme"], "dark");

    let StoredStatus { ready, lines } = stored_status(PI, home.path()).unwrap();
    let lines = lines.join("\n");
    assert!(ready);
    assert!(
        lines.contains(
            r#"default provider "scv" (openai-responses at relay.invalid), model "gpt-test""#
        ),
        "{lines}"
    );
    assert!(!lines.contains("secret"));

    let removed = remove_stored(PI, home.path()).unwrap().join("\n");
    assert!(removed.contains("auth.json"), "{removed}");
    assert!(!dir.join("auth.json").exists());
    let models = read("models.json");
    assert!(models["providers"].get("scv").is_none());
    assert!(models["providers"].get("ollama").is_some());
    let settings = read("settings.json");
    assert!(settings.get("defaultProvider").is_none());
    assert_eq!(settings["theme"], "dark");
    assert!(!stored_status(PI, home.path()).unwrap().ready);
}

#[test]
fn pi_endpoint_input_is_validated_before_any_write() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".pi/agent");
    for base_url in [
        "ftp://relay.invalid",
        "https://user:pass@relay.invalid",
        "https://relay.invalid/v1?key=x",
        "https://",
    ] {
        let endpoint = Endpoint {
            base_url: base_url.into(),
            ..endpoint()
        };
        assert!(
            configure_pi_endpoint(&dir, &endpoint, "sk").is_err(),
            "{base_url}"
        );
    }
    let endpoint = Endpoint {
        model: "--flag".into(),
        ..endpoint()
    };
    assert!(configure_pi_endpoint(&dir, &endpoint, "sk").is_err());
    assert!(configure_pi_endpoint(&dir, &super::tests::endpoint(), "").is_err());
    assert!(!dir.exists());

    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("models.json"), "not json").unwrap();
    assert!(configure_pi_endpoint(&dir, &super::tests::endpoint(), "sk").is_err());
    assert!(!dir.join("auth.json").exists());
    assert!(!dir.join("settings.json").exists());
}

#[test]
fn invalid_input_writes_nothing() {
    let (source, destination) = homes();
    std::fs::write(source.path().join("config.toml"), "model = \"m\"\n").unwrap();
    std::fs::write(source.path().join("auth.json"), "not json").unwrap();
    assert!(import_codex(source.path(), destination.path()).is_err());
    std::fs::write(source.path().join("config.toml"), "model = ").unwrap();
    std::fs::remove_file(source.path().join("auth.json")).unwrap();
    assert!(import_codex(source.path(), destination.path()).is_err());
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);

    let empty = tempfile::tempdir().unwrap();
    assert!(import_codex(empty.path(), destination.path()).is_err());
    assert!(import_codex(destination.path(), destination.path()).is_err());
}

#[test]
fn the_nested_scv_gets_a_private_copy_of_the_provider() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    std::fs::write(&path, "[agent]\nmax_steps = 7\n").unwrap();
    let headers = std::collections::HashMap::from([("X-Team".to_owned(), "core".to_owned())]);
    let key = "sk-nested-secret-0123456789";
    let lines = configure_scv_child(
        home.path(),
        &ScvChildProvider {
            wire_api: "responses",
            model: "gpt-test",
            base_url: "https://relay.invalid/v1/",
            timeout_seconds: 600,
            headers: &headers,
            hosted_web_search: true,
        },
        key,
    )
    .unwrap();
    assert!(lines.iter().all(|line| !line.contains(key)), "{lines:?}");
    assert!(lines[0].contains("gpt-test") && lines[0].contains("relay.invalid"));
    assert_eq!(mode(&path), 0o600);
    let table: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    assert_eq!(
        table["agent"]["max_steps"].as_integer(),
        Some(7),
        "other settings kept"
    );
    assert_eq!(
        table["provider"]["active"].as_str(),
        Some(SCV_CHILD_PROVIDER)
    );
    let profile = &table["providers"][SCV_CHILD_PROVIDER];
    assert_eq!(
        profile["base_url"].as_str(),
        Some("https://relay.invalid/v1")
    );
    assert_eq!(profile["api_key"].as_str(), Some(key));
    assert_eq!(profile["headers"]["X-Team"].as_str(), Some("core"));
    assert_eq!(table["web"]["search"].as_str(), Some("provider"));

    let store = KeyStore::Scv {
        config: "config.toml",
    };
    let StoredStatus {
        ready,
        lines: status,
    } = stored_status(store, home.path()).unwrap();
    assert!(ready);
    assert!(status.iter().all(|line| !line.contains(key)), "{status:?}");
    assert!(status[0].contains("API key stored"), "{status:?}");
    assert!(store_key(store, home.path(), key).is_err());
    remove_stored(store, home.path()).unwrap();
    let StoredStatus {
        ready,
        lines: status,
    } = stored_status(store, home.path()).unwrap();
    assert!(!ready);
    assert!(status[0].contains("scv agents import scv"), "{status:?}");
}

#[test]
fn a_broken_nested_scv_config_is_not_overwritten() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    std::fs::write(&path, "not = [valid").unwrap();
    let headers = std::collections::HashMap::new();
    let provider = ScvChildProvider {
        wire_api: "responses",
        model: "gpt-test",
        base_url: "https://relay.invalid",
        timeout_seconds: 60,
        headers: &headers,
        hosted_web_search: false,
    };
    assert!(configure_scv_child(home.path(), &provider, "sk-key-0123456789").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid");
    let bad_url = ScvChildProvider {
        base_url: "ftp://relay.invalid",
        ..provider
    };
    std::fs::remove_file(&path).unwrap();
    assert!(configure_scv_child(home.path(), &bad_url, "sk-key-0123456789").is_err());
    assert!(!path.exists());
}
