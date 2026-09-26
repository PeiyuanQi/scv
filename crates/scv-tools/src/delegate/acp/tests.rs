//! Unit tests for `src/delegate/acp/mod.rs`.

use std::{os::unix::fs::PermissionsExt as _, sync::Mutex as StdMutex, time::Duration};

use async_trait::async_trait;
use scv_core::{AgentError, ApprovalGate, ApprovalRequest, ToolApprovals};

use super::*;
use crate::delegate::{
    adapters::{OutputFormat, Resume, Transport},
    conversation::ConversationLimits,
    records::{DelegationRegistry, ProcessIdentity},
};
use crate::{
    AcpAgentLaunch, AgentAdapterConfig, DelegationContext, args::Timeouts,
    delegate::conversation::ConversationStore,
};
use scv_core::{Tool, ToolContext, ToolOutput, ToolRisk};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// A Python stand-in for an ACP server, in `tests/fake_agent.py`; these
/// tests need `python3` on `PATH`. It records its PID, the
/// `initialize` parameters, and every mode, option, and cancel call in
/// `dir`, and acts on keywords in each prompt.
const FAKE_AGENT: &str = include_str!("tests/fake_agent.py");

/// The fake agent as an executable in `dir`, run in `mode`.
fn fake_agent(dir: &Path, mode: &str) -> PathBuf {
    let program = dir.join("fake_acp.py");
    std::fs::write(&program, FAKE_AGENT).unwrap();
    let path = dir.join(format!("fake-acp-{mode}"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec python3 -u \"{}\" \"{}\" {mode}\n",
            program.display(),
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn store(idle: Duration) -> Arc<ConversationStore> {
    Arc::new(ConversationStore::new(
        ConversationLimits { max: 8, idle },
        None,
    ))
}

fn adapter(full: bool) -> AgentAdapterConfig {
    AgentAdapterConfig {
        command: "unused".into(),
        args: Vec::new(),
        prompt_args: Vec::new(),
        full_permission_args: full.then(Vec::new),
        model_args: Vec::new(),
        effort_args: Vec::new(),
        model_hint: "Model ID.".into(),
        environment: Vec::new(),
        search_dirs: Vec::new(),
        output: OutputFormat::Text,
        resume: Resume::Unsupported,
        home: None,
        transport: Transport::Process,
        acp: None,
        use_for: None,
        model: None,
        effort: None,
    }
}

fn acp_tool(
    script: &Path,
    conversations: Arc<ConversationStore>,
    delegation: Option<DelegationContext>,
    full_mode: Option<&str>,
) -> AcpAgentTool {
    AcpAgentTool::new(
        "agent_claude".into(),
        &adapter(full_mode.is_some()),
        AcpAgentLaunch {
            command: script.display().to_string(),
            args: Vec::new(),
            full_mode: full_mode.map(str::to_owned),
            environment: Vec::new(),
            required: false,
        },
        Some(script.to_owned()),
        Timeouts {
            default: Duration::from_secs(20),
            max: Duration::from_secs(30),
        },
        64 * 1024,
        delegation,
        conversations,
    )
}

/// Records relayed requests and answers them with `answer`.
struct Gate {
    answer: bool,
    requests: StdMutex<Vec<ApprovalRequest>>,
}

#[async_trait]
impl ApprovalGate for Gate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        self.requests.lock().unwrap().push(request);
        Ok(self.answer)
    }
}

fn context(workspace: &Path, gate: Option<Arc<Gate>>) -> ToolContext {
    let mut context = ToolContext::new(workspace.canonicalize().unwrap(), CancellationToken::new());
    if let Some(gate) = gate {
        context.approvals = ToolApprovals::new(gate, "call-1");
    }
    context.progress = scv_core::ProgressSink::buffered();
    context
}

fn json(output: &ToolOutput) -> Value {
    serde_json::from_str(&output.content).unwrap()
}

fn calls(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
}

fn pid(dir: &Path) -> u32 {
    std::fs::read_to_string(dir.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn alive(pid: u32) -> bool {
    ProcessIdentity::of(pid).is_some_and(|identity| identity.is_alive())
}

async fn wait_gone(pid: u32) {
    for _ in 0..100 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ACP agent {pid} is still running");
}

#[tokio::test]
async fn a_conversation_continues_one_session_with_bounded_progress() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let registry = Arc::new(DelegationRegistry::new(home.path()));
    let delegation = DelegationContext {
        registry: Arc::clone(&registry),
        session: "parent".into(),
        depth: 0,
    };
    let conversations = store(Duration::from_secs(3600));
    let tool = acp_tool(&script, Arc::clone(&conversations), Some(delegation), None);
    let context = context(dir.path(), None);

    let first = tool
        .execute(json!({"prompt":"remember heron"}), context.clone())
        .await
        .unwrap();
    let first_value = json(&first);
    assert_eq!(first_value["status"], "completed", "{first_value}");
    assert_eq!(first_value["agent"], "claude");
    assert_eq!(first_value["session"], "claude-1");
    assert_eq!(first_value["turn"], 1);
    assert!(
        first_value["reply"]
            .as_str()
            .unwrap()
            .contains("stored heron")
    );
    assert_eq!(first_value["usage"]["input_tokens"], 3);
    let progress = context.progress.take().unwrap_or_default();
    assert!(progress.contains("noted"), "{progress}");
    assert!(progress.contains("Read notes.txt"), "{progress}");
    assert!(progress.contains("failed"), "{progress}");
    assert!(progress.contains("plan: store the word"), "{progress}");
    for private in ["PRIVATE-OUTPUT", "PRIVATE-THOUGHT", "sk-live-123456"] {
        assert!(!progress.contains(private), "{private} leaked: {progress}");
    }
    // The client offers no file system or terminal.
    let init: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("init.json")).unwrap())
            .unwrap();
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["clientCapabilities"]["fs"]["readTextFile"], false);
    assert_eq!(init["clientCapabilities"]["fs"]["writeTextFile"], false);
    assert_eq!(init["clientCapabilities"]["terminal"], false);
    let agent_pid = pid(dir.path());
    assert_eq!(registry.list(false).len(), 1, "the live agent is recorded");

    let second = tool
        .execute(
            json!({"prompt":"recall","session":"claude-1"}),
            context.clone(),
        )
        .await
        .unwrap();
    let second_value = json(&second);
    assert_eq!(second_value["turn"], 2, "{second_value}");
    assert!(
        second_value["reply"]
            .as_str()
            .unwrap()
            .contains("the word is heron in s1"),
        "{second_value}"
    );
    assert_eq!(
        pid(dir.path()),
        agent_pid,
        "one process serves the conversation"
    );
    assert_eq!(calls(dir.path()).matches("session/new").count(), 1);

    // Ending the calling session shuts the agent down.
    drop(tool);
    drop(conversations);
    wait_gone(agent_pid).await;
}

#[tokio::test]
async fn permission_requests_go_through_the_callers_gate() {
    for (answer, chosen) in [(true, "chose allow-once"), (false, "chose reject")] {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_agent(dir.path(), "normal");
        let gate = Arc::new(Gate {
            answer,
            requests: StdMutex::new(Vec::new()),
        });
        let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
        let output = tool
            .execute(
                json!({"prompt":"permission"}),
                context(dir.path(), Some(Arc::clone(&gate))),
            )
            .await
            .unwrap();
        let value = json(&output);
        assert!(value["reply"].as_str().unwrap().contains(chosen), "{value}");
        let requests = gate.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].name, "agent_claude");
        assert_eq!(requests[0].risk, ToolRisk::Filesystem);
        assert!(
            requests[0]
                .summary
                .starts_with("[claude-1 acp] Write /tmp/scv-acp.txt (edit)"),
            "{}",
            requests[0].summary
        );
    }
}

#[tokio::test]
async fn without_a_gate_permission_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
    let output = tool
        .execute(json!({"prompt":"permission"}), context(dir.path(), None))
        .await
        .unwrap();
    assert!(
        json(&output)["reply"]
            .as_str()
            .unwrap()
            .contains("chose reject")
    );
}

#[tokio::test]
async fn file_system_and_terminal_requests_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
    let output = tool
        .execute(json!({"prompt":"client"}), context(dir.path(), None))
        .await
        .unwrap();
    let value = json(&output);
    assert!(
        value["reply"]
            .as_str()
            .unwrap()
            .contains("refused:-32601,-32601"),
        "{value}"
    );
}

/// Start the agent with one untimed turn, and return the conversation's handle.
async fn open_conversation(tool: &AcpAgentTool, dir: &Path) -> String {
    let opened = tool
        .execute(json!({"prompt":"hello"}), context(dir, None))
        .await
        .unwrap();
    let value = json(&opened);
    assert_eq!(value["status"], "completed", "{value}");
    value["session"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_timed_out_turn_is_cancelled_and_a_stubborn_agent_is_shut_down() {
    // A call's timeout also covers starting the agent, which a loaded CI
    // host can spend over a second on, so each conversation is opened by an
    // untimed turn first and only the prompt runs against the one second.
    // An agent that honours session/cancel keeps its conversation.
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let conversations = store(Duration::from_secs(60));
    let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
    let session = open_conversation(&tool, dir.path()).await;
    let output = tool
        .execute(
            json!({"prompt":"hang","session":session,"timeout_seconds":1}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "timeout", "{value}");
    assert_eq!(value["session"], "claude-1");
    assert!(calls(dir.path()).contains("cancel"));
    let resumed = tool
        .execute(
            json!({"prompt":"hello","session":"claude-1"}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    assert_eq!(json(&resumed)["status"], "completed");

    // One that ignores it is shut down and its conversation forgotten.
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "stubborn");
    let conversations = store(Duration::from_secs(60));
    let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
    let session = open_conversation(&tool, dir.path()).await;
    let output = tool
        .execute(
            json!({"prompt":"hang","session":session,"timeout_seconds":1}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "timeout");
    assert!(value.get("session").is_none(), "{value}");
    wait_gone(pid(dir.path())).await;
    assert!(conversations.handles().is_empty());
}

#[tokio::test]
async fn cancellation_sends_session_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
    let context = context(dir.path(), None);
    let cancel = context.cancellation.clone();
    let log = dir.path().to_owned();
    tokio::spawn(async move {
        // Cancel once the agent is inside the prompt, however slowly it started.
        until(|| calls(&log).contains("hang")).await;
        cancel.cancel();
    });
    let error = tool
        .execute(json!({"prompt":"hang"}), context)
        .await
        .unwrap_err();
    assert!(error.0.contains("cancelled"), "{}", error.0);
    assert!(calls(dir.path()).contains("cancel"));
}

#[tokio::test]
async fn an_agent_dying_mid_turn_fails_with_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let conversations = store(Duration::from_secs(60));
    let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
    let output = tool
        .execute(json!({"prompt":"die"}), context(dir.path(), None))
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "failed");
    assert!(
        value["reply"]
            .as_str()
            .unwrap()
            .contains("boom: out of memory"),
        "{value}"
    );
    assert!(conversations.handles().is_empty());
}

#[tokio::test]
async fn failures_are_structured_redacted_and_hint_at_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
    let output = tool
        .execute(json!({"prompt":"fail"}), context(dir.path(), None))
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "failed");
    let reply = value["reply"].as_str().unwrap();
    assert!(reply.contains("Unauthorized (401)"), "{reply}");
    assert!(!reply.contains("sk-live-abcdef123456"), "{reply}");
    assert!(
        value["hint"]
            .as_str()
            .unwrap()
            .contains("scv agents login claude"),
        "{value}"
    );

    // A refusal is declined, whatever its reply mentions: no error, no
    // sign-in hint, and the note says to tell the user.
    let refused = tool
        .execute(json!({"prompt":"refuse"}), context(dir.path(), None))
        .await
        .unwrap();
    assert!(refused.is_error);
    let value = json(&refused);
    assert_eq!(value["status"], "declined");
    assert!(value["reply"].as_str().unwrap().contains("403"), "{value}");
    assert_eq!(value["note"], crate::delegate::output::DECLINED_NOTE);
    assert!(value.get("error").is_none(), "{value}");
    assert!(value.get("hint").is_none(), "{value}");
    // Chosen among several agents, Grok is named in the note, not as fallback.
    let chosen = crate::delegate::choice::ChosenAgent {
        inner: Arc::new(tool),
        use_for: None,
        model: None,
        effort: None,
        alternatives: vec!["agent_codex".into(), "agent_grok".into()],
    };
    let refused = chosen
        .execute(json!({"prompt":"refuse"}), context(dir.path(), None))
        .await
        .unwrap();
    let value = json(&refused);
    assert_eq!(value["status"], "declined");
    assert_eq!(
        value["note"],
        crate::delegate::output::DECLINED_NOTE_TRY_GROK
    );
    assert!(value.get("fallback").is_none(), "{value}");

    let auth_dir = tempfile::tempdir().unwrap();
    let script = fake_agent(auth_dir.path(), "auth");
    let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
    let output = tool
        .execute(json!({"prompt":"hello"}), context(auth_dir.path(), None))
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "failed");
    assert!(
        value["reply"]
            .as_str()
            .unwrap()
            .contains("Authentication required")
    );
    assert!(value["hint"].is_string(), "{value}");
    // A signed-out agent is a real availability failure: the others are named.
    let chosen = crate::delegate::choice::ChosenAgent {
        inner: Arc::new(tool),
        use_for: None,
        model: None,
        effort: None,
        alternatives: vec!["agent_codex".into()],
    };
    let output = chosen
        .execute(json!({"prompt":"hello"}), context(auth_dir.path(), None))
        .await
        .unwrap();
    assert!(
        json(&output)["fallback"]
            .as_str()
            .unwrap()
            .ends_with("available: agent_codex."),
        "{}",
        output.content
    );
    wait_gone(pid(auth_dir.path())).await;

    let old_dir = tempfile::tempdir().unwrap();
    let script = fake_agent(old_dir.path(), "v2");
    let tool = acp_tool(&script, store(Duration::from_secs(60)), None, None);
    let output = tool
        .execute(json!({"prompt":"hello"}), context(old_dir.path(), None))
        .await
        .unwrap();
    assert!(
        json(&output)["reply"]
            .as_str()
            .unwrap()
            .contains("protocol version 2")
    );
}

#[tokio::test]
async fn full_permissions_select_the_mode_and_calls_choose_offered_options() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let tool = acp_tool(
        &script,
        store(Duration::from_secs(60)),
        None,
        Some("bypassPermissions"),
    );
    let summary = tool.approval_summary(&json!({"prompt":"x"})).unwrap();
    assert!(summary.contains("FULL PERMISSIONS"), "{summary}");
    let output = tool
        .execute(
            json!({"prompt":"hello","model":"m2","effort":"high"}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    assert_eq!(json(&output)["status"], "completed");
    let calls = calls(dir.path());
    assert!(calls.contains("mode=bypassPermissions"), "{calls}");
    assert!(calls.contains("model=m2"), "{calls}");
    assert!(calls.contains("reasoning_effort=high"), "{calls}");

    let unknown = tool
        .execute(
            json!({"prompt":"hello","session":"claude-1","model":"m9"}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    let value = json(&unknown);
    assert_eq!(value["status"], "failed");
    assert!(
        value["reply"]
            .as_str()
            .unwrap()
            .contains("choose one of: m1, m2"),
        "{value}"
    );
    assert_eq!(
        value["session"], "claude-1",
        "a bad model keeps the session"
    );
}

#[tokio::test]
async fn the_launch_environment_reaches_the_acp_server() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let tool = |environment: Vec<(OsString, OsString)>| {
        AcpAgentTool::new(
            "agent_codex".into(),
            &adapter(true),
            AcpAgentLaunch {
                command: script.display().to_string(),
                args: Vec::new(),
                full_mode: None,
                environment,
                required: false,
            },
            Some(script.clone()),
            Timeouts {
                default: Duration::from_secs(20),
                max: Duration::from_secs(30),
            },
            64 * 1024,
            None,
            store(Duration::from_secs(60)),
        )
    };
    let workspace = dir.path().to_owned();
    let reply = |tool: AcpAgentTool| {
        let workspace = workspace.clone();
        async move {
            let output = tool
                .execute(json!({"prompt":"env"}), context(&workspace, None))
                .await
                .unwrap();
            json(&output)["reply"].as_str().unwrap().to_owned()
        }
    };
    let live = r#"{"web_search":"live"}"#;
    let with = reply(tool(vec![("CODEX_CONFIG".into(), live.into())])).await;
    assert!(with.contains(&format!("CODEX_CONFIG={live}")), "{with}");
    let without = reply(tool(Vec::new())).await;
    assert!(without.contains("CODEX_CONFIG=unset"), "{without}");
}

#[tokio::test]
async fn idle_conversations_shut_their_agent_down() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let conversations = store(Duration::from_millis(100));
    let tool = acp_tool(&script, Arc::clone(&conversations), None, None);
    tool.execute(json!({"prompt":"hello"}), context(dir.path(), None))
        .await
        .unwrap();
    let first = pid(dir.path());
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Starting another conversation expires the idle one.
    tool.execute(json!({"prompt":"hello"}), context(dir.path(), None))
        .await
        .unwrap();
    wait_gone(first).await;
}

#[test]
fn permission_risks_follow_scvs_own_tools() {
    let risk = |kind: &str, path: &str| {
        describe_permission(
            &json!({"toolCall":{"kind":kind,"title":"t","locations":[{"path":path}]}}),
            "[x]",
        )
        .0
    };
    assert_eq!(risk("read", "src/lib.rs"), ToolRisk::ReadOnly);
    assert_eq!(risk("read", "/home/u/.env"), ToolRisk::Filesystem);
    assert_eq!(risk("edit", "a"), ToolRisk::Filesystem);
    assert_eq!(risk("execute", "a"), ToolRisk::Process);
    assert_eq!(risk("fetch", "a"), ToolRisk::Network);
    assert_eq!(risk("other", "a"), ToolRisk::Delegate);
    let (_, summary) = describe_permission(
        &json!({"toolCall":{"kind":"edit","title":"Edit file","locations":[{"path":"/tmp/a"}]}}),
        "[claude-1 acp]",
    );
    assert_eq!(summary, "[claude-1 acp] Edit file (edit) on /tmp/a");
    assert_eq!(
        choose_option(Some(&json!([{"optionId":"x","kind":"allow_always"}])), true),
        json!({"outcome":"selected","optionId":"x"})
    );
    assert_eq!(
        choose_option(Some(&json!([{"optionId":"x","kind":"allow_once"}])), false),
        json!({"outcome":"cancelled"})
    );
}

#[test]
fn the_registry_prefers_an_installed_acp_server() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let with_acp = |command: &str, required: bool| {
        let mut adapter = adapter(false);
        adapter.command = "/bin/echo".into();
        adapter.acp = Some(AcpAgentLaunch {
            command: command.into(),
            args: Vec::new(),
            full_mode: None,
            environment: Vec::new(),
            required,
        });
        adapter
    };
    let description = |adapter: AgentAdapterConfig| {
        let registry = crate::builtin_registry(
            crate::ToolsConfig::default(),
            crate::SkillMap::new(),
            Vec::new(),
            1024,
            HashMap::from([("agent_claude".to_owned(), adapter)]),
        )
        .unwrap();
        registry
            .get("agent_claude")
            .map(|tool| tool.spec().description)
    };
    let installed = description(with_acp(&script.display().to_string(), false)).unwrap();
    assert!(installed.contains("Agent Client Protocol"), "{installed}");
    let missing = dir.path().join("no-such-acp-server").display().to_string();
    let fallback = description(with_acp(&missing, false)).unwrap();
    assert!(
        fallback.starts_with("Claude Code: ") && fallback.contains("Runs its CLI"),
        "{fallback}"
    );
    assert!(description(with_acp(&missing, true)).is_none());
}

/// Whether `pid` is an uncollected zombie.
fn zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| Some(stat.rsplit_once(") ")?.1.starts_with('Z')))
        .unwrap_or(false)
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition within 20 seconds");
}

fn delegation(registry: &Arc<DelegationRegistry>) -> DelegationContext {
    DelegationContext {
        registry: Arc::clone(registry),
        session: "parent".into(),
        depth: 0,
    }
}

#[tokio::test]
async fn an_idle_conversation_whose_agent_dies_is_collected_and_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let registry = Arc::new(DelegationRegistry::new(home.path()));
    let tool = acp_tool(
        &script,
        store(Duration::from_secs(3600)),
        Some(delegation(&registry)),
        None,
    );
    let first = tool
        .execute(json!({"prompt":"hello"}), context(dir.path(), None))
        .await
        .unwrap();
    let handle = json(&first)["session"].as_str().unwrap().to_owned();
    // Between turns the conversation keeps its agent, recorded as running.
    assert_eq!(registry.list(true).len(), 1);
    let agent = pid(dir.path());
    // The agent dies while nothing reads from it, as a crash would.
    crate::process::ProcessGroup::new(agent)
        .expect("agent process group")
        .signal(libc::SIGKILL);
    until(|| registry.list(true).is_empty()).await;
    assert!(!zombie(agent), "the dead agent was left a zombie");
    let next = tool
        .execute(
            json!({"prompt":"recall","session":handle}),
            context(dir.path(), None),
        )
        .await
        .unwrap_err();
    assert!(next.0.contains("ACP server exited"), "{next}");
}

#[tokio::test]
async fn killing_a_background_job_s_agent_finishes_the_job_and_frees_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = fake_agent(dir.path(), "normal");
    let registry = Arc::new(DelegationRegistry::new(home.path()));
    let (finished_tx, mut finished) = tokio::sync::mpsc::unbounded_channel();
    let jobs = Arc::new(crate::delegate::background::BackgroundJobs::new(
        1,
        Some(finished_tx),
    ));
    let tool = crate::delegate::background::BackgroundCapable {
        inner: Arc::new(acp_tool(
            &script,
            store(Duration::from_secs(3600)),
            Some(delegation(&registry)),
            None,
        )),
        jobs: Arc::clone(&jobs),
    };
    tool.execute(
        json!({"prompt":"hang","background":true}),
        context(dir.path(), None),
    )
    .await
    .unwrap();
    // Wait until the agent is inside the prompt turn, not its handshake.
    until(|| calls(dir.path()).contains("hang")).await;
    let agent = pid(dir.path());
    let handle = registry.list(true)[0].record.handle.clone();
    // `scv agents kill` of the running turn's agent.
    registry.kill(&handle).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), finished.recv())
        .await
        .expect("the job finished")
        .unwrap();
    let reports = jobs.take_unreported();
    assert_eq!(reports.len(), 1, "a report turn follows");
    assert_eq!(reports[0].status, "failed");
    assert!(reports[0].reply.contains("exited"), "{}", reports[0].reply);
    until(|| registry.list(true).is_empty()).await;
    assert!(!zombie(agent));
    assert_eq!(jobs.running(), 0, "the slot is free");
}
