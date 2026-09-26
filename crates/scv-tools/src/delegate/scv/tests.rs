//! Unit tests for `src/delegate/scv.rs`.

use std::{os::unix::fs::PermissionsExt as _, sync::Mutex as StdMutex, time::Duration};

use async_trait::async_trait;
use scv_core::{AgentError, ApprovalGate, ApprovalRequest, ToolApprovals};

use super::*;
use crate::delegate::{
    conversation::ConversationLimits,
    records::{DelegationRegistry, ProcessIdentity},
};

/// A bash stand-in for `scv server --stdio`, in `tests/fake_scv.sh`. It
/// writes its PID to `<mode>.pid`, answers the handshake, logs each turn it
/// starts to `<mode>.turns`, and handles turns according to `mode`.
fn fake_scv(dir: &Path, mode: &str) -> PathBuf {
    let path = dir.join(format!("fake-scv-{mode}"));
    let script = include_str!("tests/fake_scv.sh")
        .replace(
            "@PID@",
            &dir.join(format!("{mode}.pid")).display().to_string(),
        )
        .replace(
            "@DEPTH@",
            &dir.join(format!("{mode}.depth")).display().to_string(),
        )
        .replace(
            "@CANCELS@",
            &dir.join(format!("{mode}.cancels")).display().to_string(),
        )
        .replace(
            "@TURNS@",
            &dir.join(format!("{mode}.turns")).display().to_string(),
        )
        .replace(
            "@FINISH@",
            &dir.join(format!("{mode}.finish")).display().to_string(),
        )
        .replace(
            "@SETTLE@",
            &dir.join(format!("{mode}.settle")).display().to_string(),
        )
        .replace(
            "@ANSWERS@",
            &dir.join(format!("{mode}.answers")).display().to_string(),
        )
        .replace("@VERSION@", &PROTOCOL_VERSION.to_string())
        .replace("@MODE@", mode);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Open conversation `scv-1` with a turn that finishes at once, under the
/// default timeout.
async fn warm_up(tool: &ScvAgentTool, dir: &Path) {
    let output = tool
        .execute(json!({"prompt":"warm up"}), context(dir, None))
        .await
        .unwrap();
    assert_eq!(json(&output)["session"], "scv-1", "{}", output.content);
}

fn tool(
    script: &Path,
    conversations: Arc<ConversationStore>,
    delegation: Option<DelegationContext>,
) -> ScvAgentTool {
    ScvAgentTool {
        name: "agent_scv".into(),
        command: script.display().to_string(),
        resolved: Some(script.to_owned()),
        args: Vec::new(),
        environment: Vec::new(),
        timeouts: Timeouts {
            default: Duration::from_secs(20),
            max: Duration::from_secs(30),
        },
        output_limit: 64 * 1024,
        delegation,
        conversations,
    }
}

fn store(idle: Duration) -> Arc<ConversationStore> {
    Arc::new(ConversationStore::new(
        ConversationLimits { max: 8, idle },
        None,
    ))
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

fn pid(dir: &Path, mode: &str) -> u32 {
    std::fs::read_to_string(dir.join(format!("{mode}.pid")))
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
    panic!("nested SCV {pid} is still running");
}

fn json(output: &ToolOutput) -> Value {
    serde_json::from_str(&output.content).unwrap()
}

#[tokio::test]
async fn conversations_continue_on_one_child_with_progress_and_records() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "echo");
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
    let delegation = DelegationContext {
        registry: Arc::clone(&registry),
        session: "parent-session".into(),
        depth: 0,
    };
    let conversations = store(Duration::from_secs(3600));
    let tool = tool(&script, Arc::clone(&conversations), Some(delegation));
    let context = context(dir.path(), None);

    let first = tool
        .execute(json!({"prompt":"one"}), context.clone())
        .await
        .unwrap();
    let first = json(&first);
    assert_eq!(first["agent"], "scv");
    assert_eq!(first["status"], "completed");
    assert_eq!(first["reply"], "reply 1");
    assert_eq!(first["session"], "scv-1");
    assert_eq!(first["turn"], 1);
    assert_eq!(first["usage"]["output_tokens"], 4);
    let child = pid(dir.path(), "echo");
    assert!(alive(child), "the nested SCV stays up between turns");
    // The child runs one level deeper and is recorded as a delegation.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("echo.depth"))
            .unwrap()
            .trim(),
        "1"
    );
    let entries = registry.list(false);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].record.agent, "scv");
    assert_eq!(entries[0].record.conversation.as_deref(), Some("scv-1"));
    // No background job of its own: idle between turns, which does not hold
    // a planned restart.
    assert!(entries[0].record.idle_since_unix.is_some());
    assert_eq!(entries[0].record.background_jobs, None);
    assert!(!entries[0].working());
    let progress = context.progress.take().unwrap_or_default();
    assert!(progress.contains("thinking"), "{progress}");
    assert!(progress.contains("bash done"), "{progress}");
    assert!(!progress.contains("stale event"), "{progress}");
    assert!(!progress.contains("PRIVATE"), "{progress}");

    let second = tool
        .execute(json!({"prompt":"two","session":"scv-1"}), context.clone())
        .await
        .unwrap();
    let second = json(&second);
    assert_eq!(second["reply"], "reply 2", "the same child served turn two");
    assert_eq!(second["turn"], 2);
    assert_eq!(pid(dir.path(), "echo"), child);
    assert_eq!(registry.list(false)[0].record.turn, Some(2));

    // Ending the session's conversations shuts the child down.
    drop(tool);
    drop(conversations);
    wait_gone(child).await;
    for _ in 0..100 {
        if registry.list(true).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        registry.list(true).is_empty(),
        "the record outlived the child"
    );
}

/// Wait until `condition` holds, under a generous ceiling.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what} never happened");
}

#[tokio::test]
async fn a_childs_own_background_job_keeps_it_at_work_until_reported() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "background");
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
    let delegation = DelegationContext {
        registry: Arc::clone(&registry),
        session: "parent-session".into(),
        depth: 0,
    };
    let tool = tool(&script, store(Duration::from_secs(3600)), Some(delegation));
    let gate = Arc::new(Gate {
        answer: true,
        requests: StdMutex::new(Vec::new()),
    });
    let output = tool
        .execute(
            json!({"prompt":"land it"}),
            context(dir.path(), Some(Arc::clone(&gate))),
        )
        .await
        .unwrap();
    assert_eq!(json(&output)["reply"], "started job-1");
    let run = || {
        let mut entries = registry.list(false);
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.remove(0)
    };
    // The call's turn ended, but the job it started runs on inside the
    // child: at work, so a planned restart waits.
    let between = run();
    assert!(between.record.idle_since_unix.is_some(), "{between:?}");
    assert_eq!(between.record.background_jobs, Some(1));
    assert!(between.working());

    // The job finishes and the child starts a turn to report it. No call is
    // there to carry its approval request to a person, so it is denied, not
    // relayed.
    std::fs::write(dir.path().join("background.finish"), "").unwrap();
    let answers = dir.path().join("background.answers");
    eventually("the report's approval answer", || answers.exists()).await;
    assert_eq!(std::fs::read_to_string(&answers).unwrap().trim(), "denied");
    assert!(gate.requests.lock().unwrap().is_empty());
    // Reported, but the turn reporting it still runs: still at work.
    assert_eq!(run().record.background_jobs, Some(1));
    assert!(run().working());

    // The report turn streams more than a pipe holds, then ends; the job is
    // settled and the child idle.
    std::fs::write(dir.path().join("background.settle"), "").unwrap();
    eventually("the report turn's end", || !run().working()).await;
    assert_eq!(run().record.background_jobs, None);
    assert!(run().record.idle_since_unix.is_some());

    // The next call finds the conversation in step.
    let next = tool
        .execute(
            json!({"prompt":"warm up","session":"scv-1"}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    assert_eq!(json(&next)["reply"], "warm", "{}", next.content);
    assert_eq!(json(&next)["turn"], 2);
}

#[test]
fn background_work_follows_jobs_until_their_report_turn_ends() {
    let event = |value: Value| serde_json::from_value::<ServerEvent>(value).unwrap();
    let tool_completed = |jobs: Value| {
        event(
            json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":1,
            "call_id":"c","name":"agent_codex","success":true,"output":"{}","truncated":false,"jobs":jobs}),
        )
    };
    let turn = |kind: &str, request: &str, origin: Value| {
        let mut value = json!({"type":kind,"request_id":request,"session_id":"s","turn_id":"t","seq":2,
            "steps":1,"usage":{},"code":"internal","message":"x"});
        if !origin.is_null() {
            value["origin"] = origin;
        }
        event(value)
    };
    let mut work = BackgroundWork::default();
    // Two jobs start; one is seen through agent_wait.
    assert_eq!(
        work.observe(&tool_completed(json!([
            {"job":"job-1","tool":"agent_codex","status":"running"},
            {"job":"job-2","tool":"agent_codex","status":"running"}
        ]))),
        Some(2)
    );
    assert_eq!(
        work.observe(&tool_completed(
            json!([{"job":"job-2","tool":"agent_codex","status":"completed"}])
        )),
        Some(1)
    );
    // A client's own turn changes nothing.
    assert_eq!(
        work.observe(&turn("turn.completed", "r", Value::Null)),
        None
    );
    // The other is reported in a turn of the server's own, and counts until
    // that turn ends, however it ends.
    let origin = json!({"kind":"background","jobs":["job-1"]});
    assert_eq!(
        work.observe(&turn("turn.started", "background:1", origin.clone())),
        None
    );
    assert_eq!(work.count(), 1);
    assert_eq!(
        work.observe(&turn("turn.failed", "background:1", origin.clone())),
        Some(0)
    );
    // A server-started turn of a kind this client does not know counts too.
    let unknown = json!({"kind":"something-new"});
    assert_eq!(
        work.observe(&turn("turn.started", "later:1", unknown.clone())),
        Some(1)
    );
    assert_eq!(
        work.observe(&turn("turn.cancelled", "later:1", unknown)),
        Some(0)
    );
}

#[tokio::test]
async fn nested_approvals_are_relayed_to_the_session_gate() {
    for answer in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_scv(dir.path(), "approve");
        let gate = Arc::new(Gate {
            answer,
            requests: StdMutex::new(Vec::new()),
        });
        let tool = tool(&script, store(Duration::from_secs(3600)), None);
        let output = tool
            .execute(
                json!({"prompt":"clean up"}),
                context(dir.path(), Some(Arc::clone(&gate))),
            )
            .await
            .unwrap();
        let requests = gate.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].call_id, "call-1");
        assert_eq!(requests[0].name, "bash");
        assert_eq!(requests[0].risk, ToolRisk::Process);
        assert_eq!(requests[0].summary, "[scv-1 depth 1] Run rm -rf build");
        assert_eq!(
            json(&output)["reply"],
            if answer { "approved" } else { "denied" }
        );
    }
}

#[tokio::test]
async fn without_a_gate_nested_approvals_are_denied() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "approve");
    let tool = tool(&script, store(Duration::from_secs(3600)), None);
    let output = tool
        .execute(json!({"prompt":"clean up"}), context(dir.path(), None))
        .await
        .unwrap();
    assert_eq!(json(&output)["reply"], "denied");
}

#[tokio::test]
async fn a_child_ignoring_turn_cancel_is_killed_at_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "hang");
    let conversations = store(Duration::from_secs(3600));
    let tool = tool(&script, Arc::clone(&conversations), None);
    // Start the child first, so the one-second timeout covers the prompt
    // alone, not bash start-up and the handshake.
    warm_up(&tool, dir.path()).await;
    let output = tool
        .execute(
            json!({"prompt":"slow","session":"scv-1","timeout_seconds":1}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "timeout");
    assert!(output.is_error());
    assert!(value["reply"].as_str().unwrap().contains("shut down"));
    assert!(
        dir.path().join("hang.cancels").exists(),
        "turn.cancel was not sent"
    );
    wait_gone(pid(dir.path(), "hang")).await;
    assert!(
        conversations.handles().is_empty(),
        "a dead child's conversation stays"
    );
}

#[tokio::test]
async fn a_timed_out_turn_that_settles_stays_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "cancellable");
    let conversations = store(Duration::from_secs(3600));
    let tool = tool(&script, Arc::clone(&conversations), None);
    warm_up(&tool, dir.path()).await;
    let output = tool
        .execute(
            json!({"prompt":"slow","session":"scv-1","timeout_seconds":1}),
            context(dir.path(), None),
        )
        .await
        .unwrap();
    assert_eq!(json(&output)["status"], "timeout");
    assert_eq!(json(&output)["session"], "scv-1");
    assert!(alive(pid(dir.path(), "cancellable")));
    assert_eq!(conversations.handles(), ["scv-1"]);
}

#[tokio::test]
async fn cancelling_the_call_cancels_the_nested_turn() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "cancellable");
    let tool = tool(&script, store(Duration::from_secs(3600)), None);
    let context = context(dir.path(), None);
    let cancel = context.cancellation.clone();
    let turns = dir.path().join("cancellable.turns");
    tokio::spawn(async move {
        // Cancel once the nested turn is running, however slowly it started.
        for _ in 0..400 {
            if turns.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        cancel.cancel();
    });
    let error = tool
        .execute(json!({"prompt":"slow"}), context)
        .await
        .unwrap_err();
    assert!(error.message.contains("cancelled"), "{error}");
    assert!(dir.path().join("cancellable.cancels").exists());
}

#[tokio::test]
async fn a_child_dying_mid_turn_fails_the_call_and_ends_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "die");
    let conversations = store(Duration::from_secs(3600));
    let tool = tool(&script, Arc::clone(&conversations), None);
    let output = tool
        .execute(json!({"prompt":"work"}), context(dir.path(), None))
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["status"], "failed");
    assert!(
        value["reply"].as_str().unwrap().contains("exited"),
        "{value}"
    );
    assert!(
        value["stderr_tail"].as_str().unwrap().contains("boom"),
        "{value}"
    );
    assert!(conversations.handles().is_empty());
    let error = tool
        .execute(
            json!({"prompt":"again","session":"scv-1"}),
            context(dir.path(), None),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("unknown"), "{error}");
}

#[tokio::test]
async fn idle_conversations_shut_their_child_down() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "echo");
    let conversations = store(Duration::from_millis(300));
    let tool = tool(&script, Arc::clone(&conversations), None);
    tool.execute(json!({"prompt":"one"}), context(dir.path(), None))
        .await
        .unwrap();
    let first = pid(dir.path(), "echo");
    tokio::time::sleep(Duration::from_millis(400)).await;
    // The next call forgets the idle conversation, which ends its child.
    tool.execute(json!({"prompt":"two"}), context(dir.path(), None))
        .await
        .unwrap();
    assert_ne!(pid(dir.path(), "echo"), first);
    wait_gone(first).await;
}

#[test]
fn arguments_are_checked_before_approval() {
    let dir = tempfile::tempdir().unwrap();
    let tool = tool(&dir.path().join("scv"), store(Duration::from_secs(1)), None);
    for (arguments, message) in [
        (json!({"prompt":"x","effort":"high"}), "effort"),
        (
            json!({"prompt":"x","session":"scv-1","model":"m"}),
            "new conversation",
        ),
        (
            json!({"prompt":"x","session":"0199a213-81c0"}),
            "not a conversation handle",
        ),
        (json!({"prompt":"x","timeout_seconds":999999}), "exceeds"),
    ] {
        let error = tool.risk(&arguments).unwrap_err();
        assert!(error.message.contains(message), "{arguments}: {error}");
    }
    let summary = tool
        .approval_summary(&json!({"prompt":"do it","cwd":"scv"}))
        .unwrap();
    assert!(summary.contains("new nested SCV"), "{summary}");
    assert!(summary.contains("come back here for approval"), "{summary}");
    let summary = tool
        .approval_summary(&json!({"prompt":"more","session":"scv-2"}))
        .unwrap();
    assert!(summary.contains("conversation scv-2"), "{summary}");
}

#[test]
fn the_registry_offers_agent_scv_only_below_the_depth_limit() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_scv(dir.path(), "echo");
    let adapter = crate::AgentAdapterConfig {
        command: script.display().to_string(),
        args: Vec::new(),
        prompt_args: Vec::new(),
        full_permission_args: None,
        model_args: Vec::new(),
        effort_args: Vec::new(),
        model_hint: String::new(),
        environment: Vec::new(),
        search_dirs: Vec::new(),
        output: crate::delegate::adapters::OutputFormat::Text,
        resume: crate::delegate::adapters::Resume::Unsupported,
        home: None,
        transport: crate::delegate::adapters::Transport::ScvProtocol,
        acp: None,
        use_for: None,
        model: None,
        effort: None,
    };
    let home = tempfile::tempdir().unwrap();
    for (depth, offered) in [(0, true), (1, true), (2, false)] {
        let config = crate::ToolsConfig {
            delegation: Some(DelegationContext {
                registry: Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
                    home.path(),
                ))),
                session: "s".into(),
                depth,
            }),
            ..crate::ToolsConfig::default()
        };
        let registry = crate::builtin_registry(
            config,
            crate::SkillMap::new(),
            Vec::new(),
            1024,
            std::collections::HashMap::from([("agent_scv".to_owned(), adapter.clone())]),
        )
        .unwrap();
        assert_eq!(
            registry.get("agent_scv").is_some(),
            offered,
            "depth {depth}"
        );
        if let Some(tool) = registry.get("agent_scv") {
            let spec = tool.spec();
            assert!(spec.parameters["properties"]["session"].is_object());
            assert!(spec.parameters["properties"].get("effort").is_none());
        }
    }
}
