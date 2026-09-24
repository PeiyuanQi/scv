//! Unit tests for `src/delegate/native.rs`.

use std::{os::unix::fs::symlink, time::Duration};

use super::*;
use crate::{
    ToolsConfig,
    builtin::shell::BashTool,
    delegate::{adapters::Transport, choice, request::MAX_AGENT_CWD_BYTES},
};

fn test_conversations() -> Arc<ConversationStore> {
    Arc::new(ConversationStore::new(
        ToolsConfig::default().conversations,
        None,
    ))
}

/// Fake agents run through `bash` so no test ever executes a file that a
/// concurrently forked test process may still hold open for writing
/// (which fails spawning with ETXTBSY).
fn fake_agent(
    workspace: &Path,
    name: &str,
    script: &str,
    args: &[&str],
    environment: Vec<(OsString, OsString)>,
) -> NativeAgentTool {
    fake_agent_with_prompt_args(workspace, name, script, args, &[], environment)
}

fn fake_agent_with_prompt_args(
    workspace: &Path,
    name: &str,
    script: &str,
    args: &[&str],
    prompt_args: &[&str],
    environment: Vec<(OsString, OsString)>,
) -> NativeAgentTool {
    let script_path = workspace.join("fake-agent.sh");
    std::fs::write(&script_path, script).unwrap();
    let mut fixed = vec![script_path.display().to_string()];
    fixed.extend(args.iter().map(|arg| arg.to_string()));
    NativeAgentTool::new(
        name.into(),
        AgentAdapterConfig {
            command: "bash".into(),
            args: fixed,
            prompt_args: prompt_args.iter().map(|arg| arg.to_string()).collect(),
            full_permission_args: None,
            model_args: vec!["--model".into(), "{model}".into()],
            effort_args: vec!["--effort".into(), "{effort}".into()],
            model_hint: adapters::adapter(name.trim_start_matches("agent_"))
                .map_or(
                    "Model ID in the form this agent's CLI accepts.",
                    |adapter| adapter.model_hint,
                )
                .into(),
            environment,
            search_dirs: Vec::new(),
            output: OutputFormat::Text,
            resume: Resume::Unsupported,
            home: None,
            transport: Transport::Process,
            acp: None,
            use_for: None,
        },
        Timeouts {
            default: Duration::from_secs(2),
            max: Duration::from_secs(5),
        },
        1024,
        None,
        test_conversations(),
    )
}

fn context(workspace: &Path) -> ToolContext {
    ToolContext::new(
        workspace.canonicalize().unwrap(),
        tokio_util::sync::CancellationToken::new(),
    )
}

#[tokio::test]
async fn native_agent_preserves_argument_boundaries() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = fake_agent(
        workspace.path(),
        "agent_fake",
        "pwd\nprintf '%s\\n' \"$@\"\n",
        &["--fixed"],
        Vec::new(),
    );
    let output = tool
        .execute(
            json!({"prompt":"hello; echo unsafe"}),
            context(workspace.path()),
        )
        .await
        .unwrap();
    assert!(output.content.contains("--fixed"));
    assert!(output.content.contains("hello; echo unsafe"));
    assert!(
        output
            .content
            .contains(&workspace.path().display().to_string())
    );
}

#[tokio::test]
async fn native_agent_maps_model_and_effort_to_adapter_flags() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = fake_agent(
        workspace.path(),
        "agent_claude",
        "printf '%s\\n' \"$@\"\n",
        &["-p"],
        Vec::new(),
    );
    let properties = &tool.spec().parameters["properties"];
    assert_eq!(properties["effort"]["enum"], json!(AGENT_EFFORTS));
    assert_eq!(properties["model"]["type"], "string");
    let arguments = json!({"prompt":"hi","model":"sonnet","effort":"medium"});
    assert!(
        tool.approval_summary(&arguments)
            .unwrap()
            .contains(r#""--model", "sonnet", "--effort", "medium""#)
    );
    let output = tool
        .execute(arguments, context(workspace.path()))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["reply"], "-p\n--model\nsonnet\n--effort\nmedium\nhi");
    for invalid in [
        json!({"prompt":"hi","model":"--dangerously-skip-permissions"}),
        json!({"prompt":"hi","model":"sonnet medium"}),
        json!({"prompt":"hi","effort":"extreme"}),
        json!({"prompt":"hi","model":"@/etc/passwd"}),
        json!({"prompt":"--resume"}),
    ] {
        assert!(tool.risk(&invalid).is_err());
    }
    let fixed_only = NativeAgentTool::new(
        "agent_pi".into(),
        AgentAdapterConfig {
            command: "pi".into(),
            args: vec!["-p".into()],
            prompt_args: Vec::new(),
            full_permission_args: None,
            model_args: Vec::new(),
            effort_args: Vec::new(),
            model_hint: String::new(),
            environment: Vec::new(),
            search_dirs: Vec::new(),
            output: OutputFormat::Text,
            resume: Resume::Unsupported,
            home: None,
            transport: Transport::Process,
            acp: None,
            use_for: None,
        },
        Timeouts {
            default: Duration::from_secs(2),
            max: Duration::from_secs(2),
        },
        1024,
        None,
        test_conversations(),
    );
    assert!(
        fixed_only.spec().parameters["properties"]
            .get("model")
            .is_none()
    );
    let error = fixed_only
        .risk(&json!({"prompt":"hi","model":"sonnet"}))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support selecting a model")
    );
}

#[test]
fn native_agent_model_hints_name_the_adapter_family_and_default() {
    let workspace = tempfile::tempdir().unwrap();
    let description = |name: &str, field: &str| {
        fake_agent(workspace.path(), name, "", &[], Vec::new())
            .spec()
            .parameters["properties"][field]["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let claude = description("agent_claude", "model");
    let codex = description("agent_codex", "model");
    let other = description("agent_other", "model");
    assert!(claude.contains("sonnet or opus"));
    for text in [&codex, &other] {
        assert!(!text.contains("sonnet"), "{text}");
    }
    assert!(codex.contains("not a Claude alias"));
    for text in [claude, codex, other, description("agent_codex", "effort")] {
        assert!(
            text.contains("omit to use the agent's configured default"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn signed_out_dsh_failure_names_the_host_login_command() {
    let workspace = tempfile::tempdir().unwrap();
    // DeepSeek Harness 0.1.7-rc.1's startup error without a key.
    let tool = fake_agent(
        workspace.path(),
        "agent_dsh",
        "echo 'dsh: MISSING_CREDENTIAL: llm-deepseek: no API key for provider route \"deepseek-official\"' >&2\nexit 1\n",
        &[],
        Vec::new(),
    );
    let output = tool
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(output.is_error);
    let content: Value = serde_json::from_str(&output.content).unwrap();
    assert!(
        content["hint"]
            .as_str()
            .unwrap()
            .ends_with("scv agents login dsh"),
        "{content}"
    );
}

#[tokio::test]
async fn signed_out_agent_failure_names_the_host_login_command() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = fake_agent(
        workspace.path(),
        "agent_claude",
        "echo 'Not logged in · Please run /login'\nexit 1\n",
        &[],
        Vec::new(),
    );
    let output = tool
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(output.is_error);
    let content: Value = serde_json::from_str(&output.content).unwrap();
    assert!(
        content["hint"]
            .as_str()
            .unwrap()
            .ends_with("scv agents login claude")
    );
    let other = fake_agent(
        workspace.path(),
        "agent_claude",
        "echo 'disk full'\nexit 1\n",
        &[],
        Vec::new(),
    );
    let output = other
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(output.is_error);
    assert!(!output.content.contains("hint"));
}

#[tokio::test]
async fn native_agent_uses_instance_private_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let home = workspace.path().join("private-home");
    let tool = fake_agent(
        workspace.path(),
        "agent_codex",
        "printf 'HOME=%s\\nSCV_HOME=%s\\nCODEX_HOME=%s\\nSCV_CONFIG=%s\\nOPENAI_API_KEY=%s\\nCODEX_API_KEY=%s\\n' \"$HOME\" \"$SCV_HOME\" \"$CODEX_HOME\" \"${SCV_CONFIG-unset}\" \"${OPENAI_API_KEY-unset}\" \"${CODEX_API_KEY-unset}\"\n",
        &[],
        vec![
            ("HOME".into(), home.clone().into()),
            ("SCV_HOME".into(), home.clone().into()),
            ("CODEX_HOME".into(), home.join("codex").into()),
        ],
    );
    let output = tool
        .execute(
            json!({"prompt":"print environment"}),
            context(workspace.path()),
        )
        .await
        .unwrap();
    assert!(output.content.contains(&format!("HOME={}", home.display())));
    assert!(
        output
            .content
            .contains(&format!("CODEX_HOME={}/codex", home.display()))
    );
    assert!(output.content.contains("SCV_CONFIG=unset"));
    assert!(output.content.contains("OPENAI_API_KEY=unset"));
    assert!(output.content.contains("CODEX_API_KEY=unset"));
}

#[tokio::test]
async fn native_agent_places_prompt_flags_just_before_the_prompt() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = fake_agent_with_prompt_args(
        workspace.path(),
        "agent_grok",
        "printf '%s\\n' \"$@\"\n",
        &[],
        &["-p"],
        Vec::new(),
    );
    let arguments = json!({"prompt":"hi","model":"grok-4","effort":"high"});
    assert!(
        tool.approval_summary(&arguments)
            .unwrap()
            .contains(r#""--model", "grok-4", "--effort", "high", "-p""#)
    );
    let output = tool
        .execute(arguments, context(workspace.path()))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["reply"], "--model\ngrok-4\n--effort\nhigh\n-p\nhi");
}

#[tokio::test]
async fn full_permissions_follow_the_fixed_arguments_and_are_announced() {
    let workspace = tempfile::tempdir().unwrap();
    let mut tool = fake_agent(
        workspace.path(),
        "agent_claude",
        "printf '%s\\n' \"$@\"\n",
        &["-p"],
        Vec::new(),
    );
    let arguments = json!({"prompt":"hi","model":"opus"});
    assert!(!tool.approval_summary(&arguments).unwrap().contains("FULL"));
    tool.full_permission_args = Some(vec!["--permission-mode".into(), "bypassPermissions".into()]);
    let summary = tool.approval_summary(&arguments).unwrap();
    assert!(summary.contains("FULL PERMISSIONS"), "{summary}");
    assert!(
        summary.contains(r#""-p", "--permission-mode", "bypassPermissions", "--model", "opus""#)
    );
    let output = tool
        .execute(arguments, context(workspace.path()))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(
        output["reply"],
        "-p\n--permission-mode\nbypassPermissions\n--model\nopus\nhi"
    );
}

#[tokio::test]
async fn native_agent_runs_in_a_contained_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("project")).unwrap();
    std::fs::write(root.join("notes.txt"), "not a directory").unwrap();
    symlink(outside.path(), root.join("escape")).unwrap();
    symlink(root.join("project"), root.join("inner-link")).unwrap();
    let tool = fake_agent(&root, "agent_codex", "pwd\n", &[], Vec::new());
    let run = |arguments: Value| tool.execute(arguments, context(&root));

    for arguments in [
        json!({"prompt":"hi"}),
        json!({"prompt":"hi","cwd":""}),
        json!({"prompt":"hi","cwd":"  ","model":"","effort":" "}),
    ] {
        let output = run(arguments.clone()).await.unwrap();
        let output: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(output["reply"], root.display().to_string(), "{arguments}");
    }
    for cwd in [
        "project".to_owned(),
        "project/".to_owned(),
        "inner-link".to_owned(),
        root.join("project").display().to_string(),
    ] {
        let output = run(json!({"prompt":"hi","cwd":cwd})).await.unwrap();
        let output: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(
            output["reply"],
            root.join("project").display().to_string(),
            "{cwd}"
        );
    }
    for (cwd, error) in [
        ("..", "outside the workspace"),
        ("escape", "outside the workspace"),
        ("/", "outside the workspace"),
        ("notes.txt", "not a directory"),
        ("missing", "No such file"),
    ] {
        let result = run(json!({"prompt":"hi","cwd":cwd})).await;
        assert!(
            result.as_ref().unwrap_err().to_string().contains(error),
            "{cwd}: {result:?}"
        );
    }
    assert!(tool.risk(&json!({"prompt":"hi","cwd":"a\0b"})).is_err());
    assert!(
        tool.risk(&json!({"prompt":"hi","cwd":"x".repeat(MAX_AGENT_CWD_BYTES + 1)}))
            .is_err()
    );
    let summary = tool
        .approval_summary(&json!({"prompt":"hi","cwd":"project","timeout_seconds":4}))
        .unwrap();
    assert!(summary.contains(r#"in "project" (inside the workspace) for up to 4 seconds"#));
    assert!(
        tool.approval_summary(&json!({"prompt":"hi"}))
            .unwrap()
            .contains("in the workspace root for up to 2 seconds")
    );
    let description = tool.spec().parameters["properties"]["cwd"]["description"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(description.contains("AGENTS.md"));
}

#[tokio::test]
async fn per_call_timeouts_may_rise_to_the_ceiling_but_not_past_it() {
    let timeouts = Timeouts {
        default: Duration::from_secs(120),
        max: Duration::from_secs(1800),
    };
    assert_eq!(timeouts.resolve(None).unwrap(), Duration::from_secs(120));
    assert_eq!(timeouts.resolve(Some(30)).unwrap(), Duration::from_secs(30));
    assert_eq!(
        timeouts.resolve(Some(1800)).unwrap(),
        Duration::from_secs(1800)
    );
    assert!(timeouts.resolve(Some(0)).is_err());
    assert!(
        timeouts
            .resolve(Some(1801))
            .unwrap_err()
            .to_string()
            .contains("maximum of 1800 seconds (tools.max_timeout_seconds)")
    );

    let workspace = tempfile::tempdir().unwrap();
    let agent = fake_agent(
        workspace.path(),
        "agent_codex",
        "echo ran\n",
        &[],
        Vec::new(),
    );
    let schema = &agent.spec().parameters["properties"]["timeout_seconds"];
    assert_eq!(schema["maximum"], 5);
    assert!(
        schema["description"]
            .as_str()
            .unwrap()
            .contains("Defaults to 2; at most 5")
    );
    assert!(
        agent
            .risk(&json!({"prompt":"hi","timeout_seconds":5}))
            .is_ok()
    );
    assert!(
        agent
            .risk(&json!({"prompt":"hi","timeout_seconds":6}))
            .is_err()
    );
    assert!(
        agent
            .execute(
                json!({"prompt":"hi","timeout_seconds":6}),
                context(workspace.path())
            )
            .await
            .is_err()
    );

    let bash = BashTool {
        timeout: Duration::from_secs(1),
        max_timeout: Duration::from_secs(3),
        output_limit: 100,
    };
    assert_eq!(
        bash.spec().parameters["properties"]["timeout_seconds"]["maximum"],
        3
    );
    assert!(
        bash.risk(&json!({"command":"true","timeout_seconds":3}))
            .is_ok()
    );
    assert!(
        bash.risk(&json!({"command":"true","timeout_seconds":4}))
            .unwrap_err()
            .to_string()
            .contains("tools.max_timeout_seconds")
    );
}

/// A fake agent CLI in `format`, run through `bash script`, optionally
/// recorded in `delegation`.
fn structured_agent(
    workspace: &Path,
    name: &str,
    format: OutputFormat,
    script: &str,
    home: Option<PathBuf>,
    delegation: Option<DelegationContext>,
    timeout: Duration,
) -> NativeAgentTool {
    conversing_agent(
        workspace,
        name,
        format,
        Resume::Unsupported,
        script,
        home,
        delegation,
        timeout,
        test_conversations(),
    )
}

/// Like [`structured_agent`], continuing conversations as `resume` says,
/// in `conversations` (shared by one session's tools).
#[allow(clippy::too_many_arguments)]
fn conversing_agent(
    workspace: &Path,
    name: &str,
    format: OutputFormat,
    resume: Resume,
    script: &str,
    home: Option<PathBuf>,
    delegation: Option<DelegationContext>,
    timeout: Duration,
    conversations: Arc<ConversationStore>,
) -> NativeAgentTool {
    let script_path = workspace.join(format!("fake-{name}.sh"));
    std::fs::write(&script_path, script).unwrap();
    NativeAgentTool::new(
        name.into(),
        AgentAdapterConfig {
            command: "bash".into(),
            args: vec![script_path.display().to_string()],
            prompt_args: Vec::new(),
            full_permission_args: None,
            model_args: Vec::new(),
            effort_args: Vec::new(),
            model_hint: String::new(),
            environment: Vec::new(),
            search_dirs: Vec::new(),
            output: format,
            resume,
            home,
            transport: Transport::Process,
            acp: None,
            use_for: None,
        },
        Timeouts {
            default: timeout,
            max: Duration::from_secs(30),
        },
        64 * 1024,
        delegation,
        conversations,
    )
}

fn delegation_context(home: &Path) -> DelegationContext {
    DelegationContext {
        registry: Arc::new(DelegationRegistry::new(home)),
        session: "session-1".into(),
        depth: 0,
    }
}

#[tokio::test]
async fn claude_stream_json_becomes_a_structured_result() {
    let workspace = tempfile::tempdir().unwrap();
    let args_file = workspace.path().join("args.txt");
    let script = format!(
        r#"printf '%s\n' "$@" > {args}
printf '%s\n' "$SCV_PARENT" "$SCV_DELEGATION_DEPTH" >> {args}
echo '{{"type":"system","subtype":"init","session_id":"x","unknown":[1,2]}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"thinking"}}]}}}}'
echo 'stray diagnostic' >&2
echo '{{"type":"result","subtype":"success","is_error":false,"result":"all done","usage":{{"input_tokens":12,"output_tokens":3}}}}'
"#,
        args = args_file.display()
    );
    let home = tempfile::tempdir().unwrap();
    let context_home = delegation_context(home.path());
    let tool = conversing_agent(
        workspace.path(),
        "agent_claude",
        OutputFormat::ClaudeStreamJson,
        adapters::adapter("claude").unwrap().resume,
        &script,
        None,
        Some(context_home.clone()),
        Duration::from_secs(10),
        test_conversations(),
    );
    let output = tool
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["agent"], "claude");
    assert_eq!(
        (value["session"].as_str(), value["turn"].as_u64()),
        (Some("claude-1"), Some(1))
    );
    assert_eq!(value["status"], "completed");
    assert_eq!(value["reply"], "all done");
    assert_eq!(value["usage"]["input_tokens"], 12);
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["stderr_tail"], "stray diagnostic");
    assert_eq!(value["truncated"], false);
    // No event log reaches the parent.
    assert!(!output.content.contains("thinking"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        &lines[..4],
        [
            "--output-format",
            "stream-json",
            "--verbose",
            "--session-id"
        ]
    );
    assert!(uuid::Uuid::parse_str(lines[4]).is_ok());
    assert_eq!(lines[5], "hi");
    let chain = lines[6];
    assert!(chain.contains("/session-1/claude-"), "{chain}");
    assert_eq!(lines[7], "1");
    // The run's record is gone once it ends.
    assert!(context_home.registry.list(true).is_empty());
}

/// A fake Codex that records each call's arguments, reports thread
/// `th-1`, and answers with the prompt it was given. With `slow_start`,
/// a first (non-resume) turn hangs after reporting its thread.
fn fake_codex(workspace: &Path, slow_start: bool) -> String {
    let log = workspace.join("calls.txt");
    format!(
        r#"printf '%s\n' "$@" '--' >> {log}
case " $* " in *" resume "*) ;; *) echo '{{"type":"thread.started","thread_id":"th-1"}}'; {hang} ;; esac
for last; do :; done
echo "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"echo: $last\"}}}}"
echo '{{"type":"turn.completed","usage":{{"input_tokens":1,"output_tokens":1}}}}'
"#,
        log = log.display(),
        hang = if slow_start { "sleep 30" } else { ":" }
    )
}

fn calls(workspace: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(workspace.join("calls.txt"))
        .unwrap()
        .split("--\n")
        .filter(|call| !call.is_empty())
        .map(|call| call.lines().map(str::to_owned).collect())
        .collect()
}

#[tokio::test]
async fn conversations_continue_the_cli_session_in_the_same_cwd() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("sub")).unwrap();
    let codex_resume = adapters::adapter("codex").unwrap().resume;
    let store = test_conversations();
    let tool = conversing_agent(
        workspace.path(),
        "agent_codex",
        OutputFormat::CodexJsonl,
        codex_resume,
        &fake_codex(workspace.path(), false),
        None,
        None,
        Duration::from_secs(10),
        Arc::clone(&store),
    );
    let first = tool
        .execute(
            json!({"prompt":"remember heron"}),
            context(workspace.path()),
        )
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&first.content).unwrap();
    assert_eq!(value["status"], "completed", "{value}");
    assert_eq!(
        (value["session"].as_str(), value["turn"].as_u64()),
        (Some("codex-1"), Some(1))
    );
    let second = tool
        .execute(
            json!({"prompt":"what word?","session":"codex-1"}),
            context(workspace.path()),
        )
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&second.content).unwrap();
    assert_eq!(value["reply"], "echo: what word?");
    assert_eq!(
        (value["session"].as_str(), value["turn"].as_u64()),
        (Some("codex-1"), Some(2))
    );
    // The script path is the only fixed argument, so `$@` starts after it:
    // `resume` comes right after the fixed arguments, and the CLI's thread
    // ID sits just before the prompt.
    let recorded = calls(workspace.path());
    assert_eq!(recorded[0], ["--json", "remember heron"]);
    assert_eq!(recorded[1], ["resume", "--json", "th-1", "what word?"]);

    // A conversation stays in its cwd.
    let moved = tool
        .execute(
            json!({"prompt":"x","session":"codex-1","cwd":"sub"}),
            context(workspace.path()),
        )
        .await
        .unwrap_err();
    assert!(moved.0.contains("runs in"), "{}", moved.0);
    // Another session's tools do not know this session's handles.
    let other_session = conversing_agent(
        workspace.path(),
        "agent_codex",
        OutputFormat::CodexJsonl,
        codex_resume,
        &fake_codex(workspace.path(), false),
        None,
        None,
        Duration::from_secs(10),
        test_conversations(),
    );
    let unknown = other_session
        .execute(
            json!({"prompt":"x","session":"codex-1"}),
            context(workspace.path()),
        )
        .await
        .unwrap_err();
    assert!(
        unknown.0.contains("unknown in this session"),
        "{}",
        unknown.0
    );
    assert_eq!(
        calls(workspace.path()).len(),
        2,
        "rejected turns never launch the CLI"
    );
    // The CLI's own ID is never accepted in place of a handle.
    let vendor = json!({"prompt":"x","session":"01a0cd5a-7195-7b31-a503-e235d5da7b45"});
    assert!(
        tool.risk(&vendor)
            .unwrap_err()
            .0
            .contains("not a conversation handle")
    );
    assert!(
        tool.spec().parameters["properties"]
            .get("session")
            .is_some()
    );
}

#[tokio::test]
async fn a_timed_out_turn_stays_resumable_and_unsupported_agents_refuse_sessions() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = conversing_agent(
        workspace.path(),
        "agent_codex",
        OutputFormat::CodexJsonl,
        adapters::adapter("codex").unwrap().resume,
        &fake_codex(workspace.path(), true),
        None,
        None,
        Duration::from_secs(1),
        test_conversations(),
    );
    let first = tool
        .execute(json!({"prompt":"start"}), context(workspace.path()))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&first.content).unwrap();
    assert_eq!(value["status"], "timeout", "{value}");
    assert_eq!(value["session"], "codex-1");
    let resumed = tool
        .execute(
            json!({"prompt":"continue where you left off","session":"codex-1"}),
            context(workspace.path()),
        )
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&resumed.content).unwrap();
    assert_eq!(value["status"], "completed", "{value}");
    assert_eq!(value["turn"], 2);

    let plain = structured_agent(
        workspace.path(),
        "agent_grok",
        OutputFormat::Text,
        "echo hi\n",
        None,
        None,
        Duration::from_secs(5),
    );
    let refused = plain
        .risk(&json!({"prompt":"x","session":"grok-1"}))
        .unwrap_err();
    assert!(
        refused.0.contains("cannot continue a conversation"),
        "{}",
        refused.0
    );
    assert!(
        plain.spec().parameters["properties"]
            .get("session")
            .is_none()
    );
    let output = plain
        .execute(json!({"prompt":"x"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(
        !output.content.contains("\"session\""),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn codex_json_reads_the_last_message_file_and_removes_it() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = r#"while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then printf 'final from file\n' > "$2"; echo "$2" > last-path.txt; fi
  shift
done
echo '{"type":"thread.started","thread_id":"t"}'
echo '{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":1}}'
"#;
    let tool = structured_agent(
        workspace.path(),
        "agent_codex",
        OutputFormat::CodexJsonl,
        script,
        Some(home.path().to_owned()),
        None,
        Duration::from_secs(10),
    );
    let output = tool
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["status"], "completed", "{value}");
    assert_eq!(value["reply"], "final from file");
    let path = std::fs::read_to_string(workspace.path().join("last-path.txt")).unwrap();
    let path = PathBuf::from(path.trim());
    assert!(path.starts_with(home.path().join("tmp")));
    assert!(!path.exists(), "the last-message file is removed");
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(home.path().join("tmp"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn pi_json_and_signed_out_claude_results() {
    let workspace = tempfile::tempdir().unwrap();
    let pi = structured_agent(
        workspace.path(),
        "agent_pi",
        OutputFormat::PiJson,
        r#"echo '{"type":"session","id":"p"}'
echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"pi ok"}],"usage":{"input":7,"output":2}}}'
"#,
        None,
        None,
        Duration::from_secs(10),
    );
    let output = pi
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["reply"], "pi ok");
    assert_eq!(value["usage"]["output_tokens"], 2);

    let claude = structured_agent(
        workspace.path(),
        "agent_claude",
        OutputFormat::ClaudeStreamJson,
        r#"echo '{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}'
exit 1
"#,
        None,
        None,
        Duration::from_secs(10),
    );
    let output = claude
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(output.is_error);
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["status"], "failed");
    assert_eq!(value["exit_code"], 1);
    assert!(
        value["hint"]
            .as_str()
            .unwrap()
            .contains("scv agents login claude")
    );
}

#[tokio::test]
async fn cli_refusals_are_declined_and_only_availability_failures_offer_other_agents() {
    let workspace = tempfile::tempdir().unwrap();
    let chosen = |tool: NativeAgentTool| choice::ChosenAgent {
        inner: Arc::new(tool),
        use_for: None,
        alternatives: vec!["agent_codex".into(), "agent_grok".into()],
    };
    // Claude Code relays the API's `refusal` stop reason; the reply
    // mentions authentication and a 403, and the run exits 0.
    let refusing = chosen(structured_agent(
        workspace.path(),
        "agent_claude",
        OutputFormat::ClaudeStreamJson,
        r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"I cannot help bypass authentication or the 403."}],"stop_reason":"refusal"}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"I cannot help bypass authentication or the 403."}'
"#,
        None,
        None,
        Duration::from_secs(10),
    ));
    let output = refusing
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    assert!(output.is_error);
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["status"], "declined");
    assert_eq!(value["note"], output::DECLINED_NOTE);
    for absent in ["fallback", "hint", "error"] {
        assert!(value.get(absent).is_none(), "{absent} in {value}");
    }
    // A signed-out CLI, reported on stderr: the other agents are named.
    let signed_out = chosen(fake_agent(
        workspace.path(),
        "agent_dsh",
        "echo 'dsh: MISSING_CREDENTIAL: no API key' >&2\nexit 1\n",
        &[],
        Vec::new(),
    ));
    let output = signed_out
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["status"], "failed");
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("MISSING_CREDENTIAL")
    );
    assert!(
        value["fallback"]
            .as_str()
            .unwrap()
            .ends_with("agent_codex, agent_grok."),
        "{value}"
    );
    // A rate-limited one too.
    let limited = chosen(structured_agent(
        workspace.path(),
        "agent_claude",
        OutputFormat::ClaudeStreamJson,
        r#"echo '{"type":"result","subtype":"success","is_error":true,"result":"API Error: 429 rate limit exceeded"}'
exit 1
"#,
        None,
        None,
        Duration::from_secs(10),
    ));
    let output = limited
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert!(value["fallback"].is_string(), "{value}");
    // A missing executable fails before running, in SCV's own words.
    let mut missing = fake_agent(workspace.path(), "agent_pi", "exit 0\n", &[], Vec::new());
    missing.resolved = None;
    let error = chosen(missing)
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap_err();
    assert!(error.0.ends_with("agent_codex, agent_grok."), "{error}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_timed_out_run_and_its_detached_descendants_are_stopped() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let delegation = delegation_context(home.path());
    let tool = structured_agent(
        workspace.path(),
        "agent_codex",
        OutputFormat::CodexJsonl,
        // The detached sleep leaves the agent's process group and session.
        "setsid sleep 60 &\necho \"$SCV_PARENT\" > chain.txt\nexec sleep 60\n",
        None,
        Some(delegation.clone()),
        Duration::from_secs(1),
    );
    let output = tool
        .execute(json!({"prompt":"hi"}), context(workspace.path()))
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["status"], "timeout");
    let chain = std::fs::read_to_string(workspace.path().join("chain.txt")).unwrap();
    let handle = chain.trim().rsplit('/').next().unwrap().to_owned();
    let tagged = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                std::fs::read(entry.path().join("environ")).is_ok_and(|environ| {
                    environ
                        .split(|byte| *byte == 0)
                        .any(|entry| entry == format!("SCV_PARENT={}", chain.trim()).as_bytes())
                })
            })
            .count()
    };
    let mut remaining = tagged();
    for _ in 0..100 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        remaining = tagged();
    }
    assert_eq!(remaining, 0, "tagged processes of {handle} survived");
    assert!(delegation.registry.list(true).is_empty());
}
