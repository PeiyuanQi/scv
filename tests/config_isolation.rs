use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, ServerEvent};

fn session_model(home: &std::path::Path, workspace: &std::path::Path) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv"))
        .args(["--scv-home"])
        .arg(home)
        .args(["server", "--stdio"])
        .env("OPENAI_API_KEY", "test-only")
        .env_remove("SCV_CONFIG")
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
    write_private(
        &home.path().join("config.toml"),
        &format!("{}[agents.claude]\ncommand = \"echo\"\n", provider("model")),
    );
    // A project layer that would be rejected must not block agent sign-in.
    std::fs::create_dir(workspace.path().join(".scv")).unwrap();
    std::fs::write(
        workspace.path().join(".scv/config.toml"),
        "[provider]\nmodel = \"project\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scv"))
        .args(["--scv-home"])
        .arg(home.path())
        .args(["agents", "status", "claude"])
        .env("OPENAI_API_KEY", "test-only")
        .env_remove("SCV_CONFIG")
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "claude:\nauth status --text\n"
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
