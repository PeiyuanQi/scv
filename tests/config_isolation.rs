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

#[test]
fn isolated_instances_select_independent_provider_models() {
    let workspace = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for (home, model) in [
        (first.path(), "model-first"),
        (second.path(), "model-second"),
    ] {
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"{model}\"\nbase_url = \"https://provider.invalid/v1\"\napi_key_env = \"OPENAI_API_KEY\"\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                home.join("config.toml"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
    }
    assert_eq!(session_model(first.path(), workspace.path()), "model-first");
    assert_eq!(
        session_model(second.path(), workspace.path()),
        "model-second"
    );
}
