//! Unit tests for `src/project_environment.rs`.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

struct Fixture {
    root: tempfile::TempDir,
    project: PathBuf,
    inherited: Vec<(OsString, OsString)>,
    adapter: Vec<(OsString, OsString)>,
}

fn script(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{text}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project with spaces");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\nrust-version='1.88'").unwrap();
        let host = root.path().join("user");
        let private = root.path().join("agent");
        let system = root.path().join("system");
        script(&system.join("rustc"), "echo 'rustc 1.75.0'");
        script(&system.join("cargo"), "echo 'cargo 1.75.0'");
        for version in ["1.75.0", "1.99.0", "1.100.0"] {
            for tool in ["rustc", "cargo", "rustdoc"] {
                script(
                    &host.join(format!(".rustup/toolchains/{version}/bin/{tool}")),
                    &format!("echo '{tool} {version}'"),
                );
            }
        }
        script(
            &host.join(".cargo/bin/rustup"),
            r#"
case "$1 $2" in
  'show active-toolchain')
    if [ -n "$RUSTUP_TOOLCHAIN" ]; then
      printf '%s (environment override)\n' "$RUSTUP_TOOLCHAIN"
    elif [ -f rust-toolchain.toml ] || [ -f rust-toolchain ]; then
      printf '%s (toolchain file)\n' "${FAKE_PIN:-1.100.0}"
    else
      printf '%s (default)\n' "${FAKE_DEFAULT:-1.100.0}"
    fi;;
  'toolchain list') printf '1.75.0\n1.99.0\n1.100.0\n';;
  'which rustc'|'which cargo')
    if [ -n "$RUSTUP_TOOLCHAIN" ]; then
      selected="$RUSTUP_TOOLCHAIN"
    elif [ -f rust-toolchain.toml ] || [ -f rust-toolchain ]; then
      selected="${FAKE_PIN:-1.100.0}"
    else
      selected="${FAKE_DEFAULT:-1.100.0}"
    fi
    if [ -x "$RUSTUP_HOME/toolchains/$selected/bin/$2" ]; then
      printf '%s/toolchains/%s/bin/%s\n' "$RUSTUP_HOME" "$selected" "$2"
    else
      echo 'toolchain is not installed' >&2; exit 1
    fi;;
  'which --toolchain')
    if [ -x "$RUSTUP_HOME/toolchains/$3/bin/$4" ]; then
      printf '%s/toolchains/%s/bin/%s\n' "$RUSTUP_HOME" "$3" "$4"
    else
      echo 'toolchain is not installed' >&2; exit 1
    fi;;
  *) exit 9;;
esac
"#,
        );
        let inherited = vec![("HOME".into(), host.into()), ("PATH".into(), system.into())];
        let adapter = vec![
            ("HOME".into(), private.clone().into()),
            ("CODEX_HOME".into(), private.into()),
        ];
        Self {
            root,
            project,
            inherited,
            adapter,
        }
    }

    async fn resolve(&self) -> Result<ProjectEnvironment> {
        resolve_from(
            &self.project,
            &self.adapter,
            self.inherited.clone(),
            CancellationToken::new(),
        )
        .await
    }

    fn launch_environment(&self) -> Vec<(OsString, OsString)> {
        let mut environment = self.adapter.clone();
        environment.extend([
            (
                "CARGO_HOME".into(),
                self.root.path().join("user/.cargo").into(),
            ),
            (
                "RUSTUP_HOME".into(),
                self.root.path().join("user/.rustup").into(),
            ),
            ("RUSTUP_TOOLCHAIN".into(), "1.100.0".into()),
            ("PATH".into(), self.root.path().join("system").into()),
        ]);
        environment
    }
}

#[tokio::test]
async fn finds_rustup_outside_service_path_without_reusing_cargo_credentials() {
    let fixture = Fixture::new();
    let result = fixture.resolve().await.unwrap();
    let env: std::collections::HashMap<_, _> = result.environment.iter().cloned().collect();
    assert_eq!(
        PathBuf::from(&env[OsStr::new("CARGO_HOME")]),
        fixture.root.path().join("agent/.cargo")
    );
    assert_eq!(
        PathBuf::from(&env[OsStr::new("RUSTUP_HOME")]),
        fixture.root.path().join("user/.rustup")
    );
    let first = std::env::split_paths(&env[OsStr::new("PATH")])
        .next()
        .unwrap();
    assert!(first.ends_with("1.100.0/bin"));
    assert!(!env.contains_key(OsStr::new("HOME")));
    assert!(!env.contains_key(OsStr::new("CODEX_HOME")));
    // Execute the tools through the environment actually injected into agents.
    let mut command = std::process::Command::new("/bin/sh");
    crate::apply_agent_environment(&mut command, &fixture.adapter);
    let output = command
        .envs(result.environment)
        .arg("-c")
        .arg("rustc --version; cargo --version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "rustc 1.100.0\ncargo 1.100.0\n"
    );
}

#[tokio::test]
async fn upgrades_only_an_unpinned_old_default_using_numeric_versions() {
    let mut fixture = Fixture::new();
    fixture
        .inherited
        .push(("FAKE_DEFAULT".into(), "1.75.0".into()));
    let result = fixture.resolve().await.unwrap();
    assert!(
        result
            .diagnostics
            .iter()
            .any(|line| line.contains("compatible rustup toolchain: 1.100.0"))
    );
    std::fs::write(
        fixture.project.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='1.75.0'",
    )
    .unwrap();
    fixture.inherited.push(("FAKE_PIN".into(), "1.75.0".into()));
    assert!(
        fixture
            .resolve()
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit toolchain")
    );
}

#[tokio::test]
async fn respects_explicit_overrides_and_never_falls_back_from_missing_pins() {
    let mut fixture = Fixture::new();
    fixture
        .inherited
        .push(("RUSTUP_TOOLCHAIN".into(), "1.99.0".into()));
    assert!(
        fixture
            .resolve()
            .await
            .unwrap()
            .diagnostics
            .iter()
            .any(|line| line.contains("rustc 1.99.0"))
    );
    fixture
        .inherited
        .push(("RUSTUP_TOOLCHAIN".into(), "1.75.0".into()));
    assert!(
        fixture
            .resolve()
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit toolchain")
    );
    fixture
        .inherited
        .push(("RUSTUP_TOOLCHAIN".into(), "missing".into()));
    assert!(
        fixture
            .resolve()
            .await
            .unwrap_err()
            .to_string()
            .contains("not installed")
    );
}

#[tokio::test]
async fn custom_install_locations_and_adapter_path_are_respected() {
    let mut fixture = Fixture::new();
    let custom = fixture.root.path().join("custom");
    std::fs::rename(fixture.root.path().join("user/.cargo"), &custom).unwrap();
    fixture.inherited.push(("CARGO_HOME".into(), custom.into()));
    let toolchains = fixture.root.path().join("toolchains elsewhere");
    std::fs::rename(fixture.root.path().join("user/.rustup"), &toolchains).unwrap();
    fixture
        .inherited
        .push(("RUSTUP_HOME".into(), toolchains.into()));
    assert!(fixture.resolve().await.is_ok());
}

#[tokio::test]
async fn a_separate_cargo_cache_does_not_hide_the_original_rustup_install() {
    let mut fixture = Fixture::new();
    fixture.inherited.push((
        "CARGO_HOME".into(),
        fixture.root.path().join("cache-only").into(),
    ));
    assert!(fixture.resolve().await.is_ok());
}

#[tokio::test]
async fn rejects_old_system_tools_but_accepts_sufficient_ones_without_rustup() {
    let mut fixture = Fixture::new();
    fixture
        .inherited
        .push(("HOME".into(), fixture.root.path().join("no-rustup").into()));
    assert!(
        fixture
            .resolve()
            .await
            .unwrap_err()
            .to_string()
            .contains("obsolete tools")
    );
    std::fs::write(
        fixture.project.join("Cargo.toml"),
        "[package]\nrust-version='1.60'",
    )
    .unwrap();
    assert!(fixture.resolve().await.is_ok());
    std::fs::write(fixture.project.join("rust-toolchain"), "stable").unwrap();
    assert!(
        fixture
            .resolve()
            .await
            .unwrap_err()
            .to_string()
            .contains("needs rustup")
    );
}

#[tokio::test]
async fn system_tools_from_different_directories_keep_their_path_precedence() {
    let mut fixture = Fixture::new();
    fixture
        .inherited
        .push(("HOME".into(), fixture.root.path().join("no-rustup").into()));
    let cargo_bin = fixture.root.path().join("cargo-only");
    let rustc_bin = fixture.root.path().join("system");
    script(&cargo_bin.join("cargo"), "echo 'cargo 1.100.0'");
    script(&rustc_bin.join("rustc"), "echo 'rustc 1.100.0'");
    // The older Cargo in the compiler directory must stay shadowed.
    let path = std::env::join_paths([cargo_bin, rustc_bin]).unwrap();
    fixture.inherited.push(("PATH".into(), path.clone()));
    let result = fixture.resolve().await.unwrap();
    assert!(result.environment.contains(&("PATH".into(), path)));
}

#[tokio::test]
async fn non_rust_project_does_not_probe_or_change_environment() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.project.join("Cargo.toml")).unwrap();
    assert!(
        resolve_from(
            &fixture.project,
            &[],
            vec![("PATH".into(), "/nonexistent".into())],
            CancellationToken::new()
        )
        .await
        .unwrap()
        .environment
        .is_empty()
    );
}

#[tokio::test]
async fn missing_cargo_or_obsolete_cargo_fails_preflight() {
    let fixture = Fixture::new();
    let cargo = fixture
        .root
        .path()
        .join("user/.rustup/toolchains/1.100.0/bin/cargo");
    script(&cargo, "echo 'cargo 1.75.0'");
    assert!(
        fixture
            .resolve()
            .await
            .unwrap_err()
            .to_string()
            .contains("obsolete tools")
    );
    std::fs::remove_file(cargo).unwrap();
    assert!(fixture.resolve().await.is_err());
}

#[tokio::test]
async fn probe_is_cancellable_and_output_is_bounded() {
    let fixture = Fixture::new();
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        resolve_from(
            &fixture.project,
            &fixture.adapter,
            fixture.inherited.clone(),
            token
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled")
    );
    script(
        &fixture.root.path().join("user/.cargo/bin/rustup"),
        "i=0; while [ $i -lt 20000 ]; do printf x; i=$((i+1)); done",
    );
    assert!(fixture.resolve().await.is_err());
}

#[tokio::test]
async fn cancellation_joins_a_running_probe() {
    let fixture = Fixture::new();
    script(
        &fixture.root.path().join("user/.cargo/bin/rustup"),
        "exec /bin/sleep 30",
    );
    let cancellation = CancellationToken::new();
    let stop = cancellation.clone();
    let (result, ()) = tokio::join!(
        resolve_from(
            &fixture.project,
            &fixture.adapter,
            fixture.inherited.clone(),
            cancellation
        ),
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop.cancel();
        }
    );
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}

#[tokio::test]
async fn native_launch_receives_project_tools_and_keeps_private_home() {
    use crate::delegate::{
        adapters::{OutputFormat, Resume, Transport},
        agent::Backend as _,
        conversation::ConversationStore,
        native::NativeAgentTool,
    };
    let fixture = Fixture::new();
    let config = crate::AgentAdapterConfig {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "rustc --version; cargo --version; printf '%s\\n' \"$HOME\" \"$CODEX_HOME\" \"$CARGO_HOME\"".into()],
        prompt_args: Vec::new(), full_permission_args: None, model_args: Vec::new(),
        effort_args: Vec::new(), model_hint: String::new(), environment: fixture.launch_environment(),
        search_dirs: Vec::new(), output: OutputFormat::Text, resume: Resume::Unsupported,
        home: None, transport: Transport::Process, acp: None, use_for: None,
        defaults: crate::AgentDefaults::default(), options_file: None,
    };
    let tool = NativeAgentTool::new(
        "fake".into(),
        config,
        crate::args::Timeouts {
            default: Duration::from_secs(5),
            max: Duration::from_secs(5),
        },
        4096,
        None,
        std::sync::Arc::new(ConversationStore::new(
            crate::ToolsConfig::default().conversations,
            None,
        )),
    );
    let context = || scv_core::ToolContext::new(fixture.project.clone(), CancellationToken::new());
    let output = tool
        .execute(serde_json::json!({"prompt":"hello"}), context())
        .await
        .unwrap();
    assert!(!output.is_error(), "{}", output.content);
    let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(
        value["reply"],
        format!(
            "rustc 1.100.0\ncargo 1.100.0\n{0}/agent\n{0}/agent\n{0}/agent/.cargo",
            fixture.root.path().display()
        )
    );
    std::fs::write(
        fixture.project.join("Cargo.toml"),
        "[package]\nrust-version='1.101'",
    )
    .unwrap();
    assert!(
        tool.execute(serde_json::json!({"prompt":"hello"}), context())
            .await
            .unwrap_err()
            .message
            .contains("explicit toolchain")
    );
}

#[tokio::test]
async fn live_launch_used_by_acp_and_nested_scv_receives_project_tools() {
    use crate::delegate::live::{LiveChild, LiveLine, LiveSpec};
    let fixture = Fixture::new();
    let spec = || LiveSpec {
        executable: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "rustc --version; cargo --version; read ignored".into(),
        ],
        cwd: fixture.project.clone(),
        environment: fixture.launch_environment(),
        max_line_bytes: 1024,
    };
    let child = LiveChild::spawn(spec(), None, CancellationToken::new())
        .await
        .unwrap();
    for expected in ["rustc 1.100.0", "cargo 1.100.0"] {
        let line = child.recv().await;
        assert!(
            matches!(line, Some(LiveLine::Line(ref bytes)) if String::from_utf8_lossy(bytes).trim() == expected),
            "{line:?}"
        );
    }
    child.close().await;
    std::fs::write(
        fixture.project.join("Cargo.toml"),
        "[package]\nrust-version='1.101'",
    )
    .unwrap();
    assert!(
        LiveChild::spawn(spec(), None, CancellationToken::new())
            .await
            .unwrap_err()
            .message
            .contains("explicit toolchain")
    );
}
