mod common;

use common::Isolated;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt as _,
    process::{Command, Stdio},
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};

fn session_model(home: &std::path::Path, workspace: &std::path::Path) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home)
        .args(["--scv-home"])
        .arg(home)
        .args(["server", "--stdio"])
        .env("OPENAI_API_KEY", "test-only")
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    for message in [
        ClientMessage::Initialize {
            request_id: "init".into(),
            protocol_version: PROTOCOL_VERSION,
            client: PeerInfo {
                name: "isolation-test".into(),
                version: "0".into(),
            },
        },
        ClientMessage::SessionStart {
            request_id: "start".into(),
            cwd: workspace.display().to_string(),
            provider: None,
            model: None,
            base_url: None,
            no_tools: Some(true),
            delegation_depth: None,
        },
    ] {
        writeln!(input, "{}", serde_json::to_string(&message).unwrap()).unwrap();
    }
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    let event: ServerEvent = serde_json::from_str(&line).unwrap();
    let model = match event {
        ServerEvent::SessionStarted { model, .. } => model,
        other => panic!("unexpected event: {other:?}"),
    };
    child.kill().unwrap();
    child.wait().unwrap();
    model
}

fn write_private(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn provider(model: &str) -> String {
    format!(
        "[provider]\nmodel = \"{model}\"\nbase_url = \"https://provider.invalid/v1\"\napi_key_env = \"OPENAI_API_KEY\"\n"
    )
}

#[test]
fn scv_home_inside_the_workspace_is_not_a_project_layer() {
    // Running from `~` makes `~/.scv/config.toml` both the user configuration
    // and the workspace's `.scv/config.toml`; it must load once, as the user's.
    let workspace = tempfile::tempdir().unwrap();
    let home = workspace.path().join(".scv");
    std::fs::create_dir(&home).unwrap();
    write_private(&home.join("config.toml"), &provider("model-home"));
    assert_eq!(session_model(&home, workspace.path()), "model-home");
}

#[test]
fn agent_sign_in_ignores_project_configuration() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // A fake `claude` whose status names the account, as the real one does.
    let fake = home.path().join("fake-claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\n[ \"$*\" = \"auth status\" ] || exit 2\n\
         echo '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"owner@example.com\",\"subscriptionType\":\"max\",\"orgName\":\"owner@example.com Org\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &home.path().join("config.toml"),
        &format!(
            "{}[agents.claude]\ncommand = {:?}\n",
            provider("model"),
            fake.display().to_string()
        ),
    );
    // A project layer that would be rejected must not block agent sign-in.
    std::fs::create_dir(workspace.path().join(".scv")).unwrap();
    std::fs::write(
        workspace.path().join(".scv/config.toml"),
        "[provider]\nmodel = \"project\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home.path())
        .args(["--scv-home"])
        .arg(home.path())
        .args(["agents", "status", "claude"])
        .env("OPENAI_API_KEY", "test-only")
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Signed-in state and method only: never the account email.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "claude:\n  signed in (Claude account, max)\n"
    );
    assert!(home.path().join("adapters/claude").is_dir());
}

#[test]
fn isolated_instances_select_independent_provider_models() {
    let workspace = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for (home, model) in [
        (first.path(), "model-first"),
        (second.path(), "model-second"),
    ] {
        write_private(&home.join("config.toml"), &provider(model));
    }
    assert_eq!(session_model(first.path(), workspace.path()), "model-first");
    assert_eq!(
        session_model(second.path(), workspace.path()),
        "model-second"
    );
}

#[test]
fn codex_import_copies_into_the_instance_adapter_home() {
    let home = tempfile::tempdir().unwrap();
    let codex = tempfile::tempdir().unwrap();
    write_private(&home.path().join("config.toml"), &provider("model"));
    std::fs::write(
        codex.path().join("config.toml"),
        "model_provider = \"relay\"\n[model_providers.relay]\nbase_url = \"https://relay.invalid\"\n",
    )
    .unwrap();
    std::fs::write(
        codex.path().join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test-secret"}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home.path())
        .args(["--scv-home"])
        .arg(home.path())
        .args(["agents", "import", "codex", "--from"])
        .arg(codex.path())
        .env("OPENAI_API_KEY", "test-only")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#"model_provider "relay""#));
    assert!(!stdout.contains("sk-test-secret"));
    let adapter = home.path().join("adapters/codex");
    assert!(
        std::fs::read_to_string(adapter.join("auth.json"))
            .unwrap()
            .contains("sk-test-secret")
    );
    assert!(adapter.join("config.toml").is_file());
}

fn scv(home: &std::path::Path, args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home)
        .args(["--scv-home"])
        .arg(home)
        .args(args)
        .env("OPENAI_API_KEY", "sk-env-secret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    for stream in [&output.stdout, &output.stderr] {
        assert!(
            !String::from_utf8_lossy(stream).contains("secret"),
            "{}",
            String::from_utf8_lossy(stream)
        );
    }
    output
}

fn success(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn dsh_login_stores_a_piped_key_in_its_private_home() {
    let home = tempfile::tempdir().unwrap();
    write_private(&home.path().join("config.toml"), &provider("model"));
    let stdout = success(&scv(
        home.path(),
        &["agents", "login", "dsh"],
        "sk-dsh-secret\n",
    ));
    assert!(
        stdout.contains("Stored the API key as DEEPSEEK_API_KEY"),
        "{stdout}"
    );
    let credentials = home.path().join("adapters/dsh/.dsh/.credentials.yaml");
    assert!(
        std::fs::read_to_string(&credentials)
            .unwrap()
            .contains("\"sk-dsh-secret\"")
    );
    let status = success(&scv(home.path(), &["agents", "status", "dsh"], ""));
    assert!(
        status.contains("API key stored as DEEPSEEK_API_KEY"),
        "{status}"
    );
    success(&scv(home.path(), &["agents", "logout", "dsh"], ""));
    assert!(!credentials.exists());
    assert!(
        success(&scv(home.path(), &["agents", "status", "dsh"], ""))
            .contains("scv agents login dsh")
    );
}

#[test]
fn pi_imports_the_scv_provider_as_its_default_endpoint() {
    let home = tempfile::tempdir().unwrap();
    // The key comes from the provider's api_key_env, read at import time.
    write_private(&home.path().join("config.toml"), &provider("gpt-relay"));
    let stdout = success(&scv(
        home.path(),
        &["agents", "import", "pi", "--from-scv-provider"],
        "",
    ));
    assert!(
        stdout.contains(r#"openai-responses at provider.invalid, model "gpt-relay""#),
        "{stdout}"
    );
    let dir = home.path().join("adapters/pi/.pi/agent");
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["scv"]["key"], "sk-env-secret");
    let models: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap()).unwrap();
    assert_eq!(
        models["providers"]["scv"]["baseUrl"],
        "https://provider.invalid/v1"
    );
    let status = success(&scv(home.path(), &["agents", "status", "pi"], ""));
    assert!(status.contains(r#"default provider "scv""#), "{status}");

    // An explicit endpoint replaces it, reading only the key from stdin.
    success(&scv(
        home.path(),
        &[
            "agents",
            "login",
            "pi",
            "--openai-compatible",
            "--base-url",
            "https://chat.invalid/v1",
            "--wire-api",
            "chat",
            "--model",
            "chat-model",
        ],
        "sk-chat-secret\n",
    ));
    let models: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap()).unwrap();
    assert_eq!(models["providers"]["scv"]["api"], "openai-completions");
    assert!(models["providers"]["scv"].get("compat").is_none());
    assert_eq!(models["providers"]["scv"]["models"][0]["id"], "chat-model");
    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains("sk-chat-secret")
    );
    assert!(
        !scv(
            home.path(),
            &["agents", "import", "codex", "--from-scv-provider"],
            ""
        )
        .status
        .success()
    );
}

#[test]
fn a_delegated_run_may_not_manage_daemons() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["start"][..],
        &["stop"],
        &["restart"],
        &["run"],
        &["update"],
        &["channels", "status"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_scv"))
            .isolated(home.path())
            .arg("--scv-home")
            .arg(home.path())
            .args(args)
            .env("SCV_DELEGATION_DEPTH", "1")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("refused inside a delegated agent run"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn codex_status_on_stderr_is_summarized_without_the_key() {
    let home = tempfile::tempdir().unwrap();
    let fake = home.path().join("fake-codex");
    std::fs::write(
        &fake,
        "#!/bin/sh\n[ \"$*\" = \"login status\" ] || exit 2\n\
         echo 'Logged in using an API key - sk-proj-***wxyz' >&2\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private(
        &home.path().join("config.toml"),
        &format!(
            "{}[agents.codex]\ncommand = {:?}\n",
            provider("model"),
            fake.display().to_string()
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home.path())
        .arg("--scv-home")
        .arg(home.path())
        .args(["agents", "status", "codex"])
        .env("OPENAI_API_KEY", "test-only")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "codex:\n  signed in (API key)\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sk-"));
}
