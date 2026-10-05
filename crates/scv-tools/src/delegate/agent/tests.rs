//! Unit tests for `src/delegate/agent.rs`.

use std::{sync::Mutex, time::Duration};

use scv_core::ToolFailure;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::delegate::{adapters, output};

/// An agent that records the calls it runs and answers with `result`, or
/// with a completed reply naming itself.
struct Recorder {
    name: &'static str,
    risked: Mutex<Vec<Value>>,
    ran: Mutex<Vec<Value>>,
    result: Mutex<Option<Result<ToolOutput, ToolError>>>,
    /// Its conversations are running a turn.
    busy: Mutex<bool>,
    idle: tokio::sync::Notify,
}

impl Recorder {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            risked: Mutex::default(),
            ran: Mutex::default(),
            result: Mutex::default(),
            busy: Mutex::new(false),
            idle: tokio::sync::Notify::new(),
        })
    }

    fn set_busy(&self, busy: bool) {
        *self.busy.lock().unwrap() = busy;
        self.idle.notify_waiters();
    }

    fn answering(name: &'static str, result: Result<ToolOutput, ToolError>) -> Arc<Self> {
        let recorder = Self::new(name);
        *recorder.result.lock().unwrap() = Some(result);
        recorder
    }

    fn touched(&self) -> bool {
        !self.risked.lock().unwrap().is_empty() || !self.ran.lock().unwrap().is_empty()
    }
}

#[async_trait]
impl Backend for Recorder {
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        self.risked.lock().unwrap().push(arguments.clone());
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok(format!("Launch {} with the prompt.", self.name))
    }

    fn busy(&self, _arguments: &Value) -> Result<bool, ToolError> {
        Ok(*self.busy.lock().unwrap())
    }

    async fn wait_idle(
        &self,
        _arguments: &Value,
        _cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        loop {
            let idle = self.idle.notified();
            if !*self.busy.lock().unwrap() {
                return Ok(());
            }
            idle.await;
        }
    }

    async fn execute(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.ran.lock().unwrap().push(arguments);
        self.result.lock().unwrap().take().unwrap_or_else(|| {
            Ok(ToolOutput::success(
                json!({"agent":self.name,"status":"completed","reply":"done"}).to_string(),
            ))
        })
    }
}

fn timeouts() -> Timeouts {
    Timeouts {
        default: Duration::from_secs(60),
        max: Duration::from_secs(600),
    }
}

/// What the built-in adapter `name` takes, as the registry offers it
/// natively.
fn built_in(name: &str) -> Accepts {
    let adapter = adapters::adapter(name).unwrap();
    Accepts {
        model: !adapter.model_args.is_empty() || name == "scv",
        effort: !adapter.effort_args.is_empty(),
        session: adapter.resume.is_supported() || name == "scv",
    }
}

fn offered(recorder: &Arc<Recorder>) -> Offered {
    let adapter = adapters::adapter(recorder.name).unwrap();
    Offered {
        name: recorder.name.to_owned(),
        backend: Arc::clone(recorder) as Arc<dyn Backend>,
        accepts: built_in(recorder.name),
        model_hint: adapter.model_hint.to_owned(),
        offered: None,
        use_for: None,
        defaults: AgentDefaults::default(),
        holds_settings: true,
        busy: BusyConfig::default(),
    }
}

/// A dispatcher over `recorders`, preferring the agents named in `prefer`.
fn dispatcher(recorders: &[&Arc<Recorder>], prefer: &[&str]) -> AgentTool {
    AgentTool::new(
        recorders.iter().map(|recorder| offered(recorder)).collect(),
        &prefer
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
        timeouts(),
    )
}

fn context() -> ToolContext {
    ToolContext::new(std::env::temp_dir(), CancellationToken::new())
}

async fn run(tool: &AgentTool, arguments: Value) -> Result<Value, ToolError> {
    let output = tool.execute(arguments, context()).await?;
    Ok(serde_json::from_str(&output.content).unwrap())
}

#[tokio::test]
async fn a_call_runs_on_the_agent_it_names_or_the_first_preferred_one() {
    let (claude, codex, grok) = (
        Recorder::new("claude"),
        Recorder::new("codex"),
        Recorder::new("grok"),
    );
    // `pi` is preferred but not offered, so `codex` is the default.
    let tool = dispatcher(&[&claude, &codex, &grok], &["pi", "codex", "claude"]);
    assert_eq!(
        run(&tool, json!({"prompt":"hi"})).await.unwrap()["agent"],
        "codex"
    );
    assert_eq!(
        run(&tool, json!({"agent":"grok","prompt":"hi"}))
            .await
            .unwrap()["agent"],
        "grok"
    );
    // A blank agent counts as omitted.
    assert_eq!(
        run(&tool, json!({"agent":"","prompt":"hi"})).await.unwrap()["agent"],
        "codex"
    );
    assert!(!claude.touched());
    // The backend gets the call as the model made it.
    assert_eq!(
        grok.ran.lock().unwrap().as_slice(),
        [json!({"agent":"grok","prompt":"hi"})]
    );
    assert_eq!(
        tool.risk(&json!({"prompt":"hi"})).unwrap(),
        ToolRisk::Delegate
    );
    assert_eq!(codex.risked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn without_background_jobs_a_busy_conversation_waits_or_fails() {
    let claude = Recorder::new("claude");
    claude.set_busy(true);
    // Nothing can queue, so the default refuses the call as busy.
    let tool = dispatcher(&[&claude], &["claude"]);
    let error = tool
        .execute(json!({"prompt":"later","session":"claude-1"}), context())
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "session busy: conversation claude-1 is still running a turn"
    );
    assert!(claude.ran.lock().unwrap().is_empty());
    // The agent's own policy waits for the turn, unless the call says otherwise.
    let mut waits = offered(&claude);
    waits.busy.behavior = BusyBehavior::Wait;
    let tool = Arc::new(AgentTool::new(vec![waits], &["claude".into()], timeouts()));
    let error = tool
        .execute(
            json!({"prompt":"later","session":"claude-1","on_busy":"fail"}),
            context(),
        )
        .await
        .unwrap_err();
    assert!(error.message.starts_with("session busy"), "{error}");
    let waiting = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move { run(&tool, json!({"prompt":"later","session":"claude-1"})).await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    assert!(claude.ran.lock().unwrap().is_empty());
    claude.set_busy(false);
    assert_eq!(waiting.await.unwrap().unwrap()["status"], "completed");
    assert_eq!(claude.ran.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_session_handle_names_its_conversation_s_agent() {
    let (claude, codex) = (Recorder::new("claude"), Recorder::new("codex"));
    let tool = dispatcher(&[&claude, &codex], &["claude"]);
    let continued = run(&tool, json!({"prompt":"next","session":"codex-2"}))
        .await
        .unwrap();
    assert_eq!(continued["agent"], "codex");
    // Naming the same agent is fine; another one is refused before launch.
    tool.risk(&json!({"agent":"codex","prompt":"next","session":"codex-2"}))
        .unwrap();
    let mismatch = tool
        .risk(&json!({"agent":"claude","prompt":"next","session":"codex-2"}))
        .unwrap_err();
    assert_eq!(mismatch.kind, ToolFailure::InvalidArguments);
    assert_eq!(
        mismatch.message,
        "conversation codex-2 belongs to codex, not claude; omit agent to continue it, or omit \
         session to start a new conversation with claude"
    );
    // A handle of an agent this session does not offer is unknown here.
    let unknown = tool
        .risk(&json!({"prompt":"next","session":"grok-1"}))
        .unwrap_err();
    assert!(
        unknown
            .message
            .starts_with("conversation grok-1 is unknown in this session"),
        "{unknown}"
    );
    assert!(!claude.touched());
}

#[tokio::test]
async fn calls_an_agent_cannot_take_fail_before_anything_launches() {
    let (claude, dsh, grok, scv) = (
        Recorder::new("claude"),
        Recorder::new("dsh"),
        Recorder::new("grok"),
        Recorder::new("scv"),
    );
    let tool = dispatcher(&[&claude, &dsh, &grok, &scv], &["claude"]);
    for (arguments, message) in [
        (
            json!({"agent":"dsh","prompt":"hi","model":"deepseek-v4"}),
            "dsh does not take model; these agents do: claude, grok, scv. Omit model, or call \
             one of them",
        ),
        (
            json!({"agent":"scv","prompt":"hi","effort":"high"}),
            "scv does not take effort; these agents do: claude, grok. Omit effort, or call one \
             of them",
        ),
        (
            json!({"agent":"grok","prompt":"hi","session":"handle"}),
            "grok does not take session (it starts a new conversation on every call); these \
             agents do: claude, scv. Omit session, or call one of them",
        ),
        (
            json!({"agent":"zcode","prompt":"hi"}),
            "agent \"zcode\" is not offered in this session; choose one of: claude, dsh, grok, \
             scv",
        ),
    ] {
        let error = tool.risk(&arguments).unwrap_err();
        assert_eq!(error.kind, ToolFailure::InvalidArguments, "{arguments}");
        assert_eq!(error.message, message, "{arguments}");
        assert_eq!(tool.approval_summary(&arguments).unwrap_err(), error);
        assert_eq!(
            tool.execute(arguments.clone(), context())
                .await
                .unwrap_err(),
            error
        );
    }
    // No agent takes it at all.
    let lone = dispatcher(&[&dsh], &["dsh"]);
    assert_eq!(
        lone.risk(&json!({"prompt":"hi","effort":"high"}))
            .unwrap_err()
            .message,
        "dsh does not take effort, and no agent in this session does; omit it"
    );
    // A blank option counts as omitted, as models send `""` for unset.
    lone.risk(&json!({"prompt":"hi","model":"","effort":" "}))
        .unwrap();
    // Nothing reached a backend but the last call.
    for recorder in [&claude, &grok, &scv] {
        assert!(!recorder.touched(), "{} ran", recorder.name);
    }
    assert_eq!(dsh.risked.lock().unwrap().len(), 1);
    assert!(dsh.ran.lock().unwrap().is_empty());
}

#[test]
fn without_a_preferred_agent_the_call_must_name_one() {
    let (claude, codex) = (Recorder::new("claude"), Recorder::new("codex"));
    let tool = dispatcher(&[&codex, &claude], &["pi"]);
    let error = tool.risk(&json!({"prompt":"hi"})).unwrap_err();
    assert_eq!(error.kind, ToolFailure::InvalidArguments);
    assert_eq!(
        error.message,
        "name the agent: the user prefers none of the agents this session offers ([agent] \
         prefer); choose one of: claude, codex"
    );
    let spec = tool.spec();
    assert_eq!(spec.parameters["required"], json!(["agent", "prompt"]));
    let description = spec.parameters["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(
        description.starts_with("Which agent runs the task; a session handle"),
        "{description}"
    );
    // With a preference, `agent` may be left out.
    let preferred = dispatcher(&[&codex, &claude], &["codex"]).spec();
    assert_eq!(preferred.parameters["required"], json!(["prompt"]));
    assert!(
        preferred.parameters["properties"]["agent"]["description"]
            .as_str()
            .unwrap()
            .starts_with("Which agent runs the task. Defaults to codex, the first of the user's"),
        "{}",
        preferred.parameters
    );
}

#[test]
fn the_schema_lists_only_offered_agents_and_the_options_some_of_them_take() {
    let names = ["claude", "codex", "dsh", "grok", "pi", "scv"];
    let recorders: Vec<Arc<Recorder>> = names.into_iter().map(Recorder::new).collect();
    let all = dispatcher(&recorders.iter().collect::<Vec<_>>(), &["codex"]).spec();
    assert_eq!(all.name, "agent");
    let properties = &all.parameters["properties"];
    assert_eq!(properties["agent"]["enum"], json!(names));
    assert_eq!(properties["effort"]["enum"], json!(AGENT_EFFORTS));
    assert_eq!(properties["model"]["type"], "string");
    for option in ["model", "effort"] {
        assert!(
            properties[option]["description"]
                .as_str()
                .unwrap()
                .contains("omit it to use the user's default that line gives"),
            "{option}"
        );
    }
    assert!(
        properties["session"]["description"]
            .as_str()
            .unwrap()
            .contains("such as \"claude-1\", which names its agent"),
        "{properties}"
    );
    assert_eq!(
        properties["on_busy"]["enum"],
        json!(["queue", "wait", "steer", "fail"])
    );
    assert!(
        properties["cwd"]["description"]
            .as_str()
            .unwrap()
            .contains("AGENTS.md")
    );
    assert_eq!(properties["timeout_seconds"]["maximum"], 600);
    assert!(
        properties["timeout_seconds"]["description"]
            .as_str()
            .unwrap()
            .contains("Defaults to 60; at most 600")
    );
    assert_eq!(all.parameters["additionalProperties"], false);
    assert!(all.description.contains("`session` handle continues"));
    // Each agent's line names its product, what it offers and takes, and
    // its model family.
    let agent = properties["agent"]["description"].as_str().unwrap();
    for line in [
        "\n- claude (Claude Code): Anthropic's coding agent;",
        "Takes model (Claude model alias or ID, such as sonnet or opus), effort, and session.",
        "\n- codex (Codex): OpenAI's coding agent;",
        "Takes model (OpenAI model ID from the Codex configuration; not a Claude alias), \
         effort, and session.",
        "\n- dsh (DeepSeek Harness): a coding agent on DeepSeek models; it reads, edits, and \
         runs code in a project. Takes no model, effort, or session.",
        "including a safety or guardrail refusal. Takes model (xAI Grok model ID, such as \
         grok-4.7) and effort.",
        "\n- scv (SCV): a nested SCV session",
        "Takes model (Model ID for the nested SCV's provider; applies to a new conversation \
         only) and session.",
    ] {
        assert!(agent.contains(line), "{line:?} missing from {agent}");
    }

    // One agent: only its own options, and the enum names it alone.
    let dsh = Recorder::new("dsh");
    let alone = dispatcher(&[&dsh], &[]).spec();
    let properties = &alone.parameters["properties"];
    assert_eq!(properties["agent"]["enum"], json!(["dsh"]));
    for absent in ["model", "effort", "session", "on_busy"] {
        assert!(properties.get(absent).is_none(), "{absent} offered");
    }
    assert!(
        !alone.description.contains("session"),
        "{}",
        alone.description
    );
}

#[test]
fn agent_lines_carry_the_user_s_note_and_defaults() {
    let codex = Recorder::new("codex");
    let mut entry = offered(&codex);
    entry.use_for = Some("coding".into());
    entry.defaults = AgentDefaults {
        model: Some("gpt-5.5".into()),
        effort: Some("medium".into()),
        hard_task_effort: Some("ultra".into()),
    };
    let tool = AgentTool::new(vec![entry], &[], timeouts());
    let spec = tool.spec();
    let agent = spec.parameters["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(
        agent.ends_with(
            "and session. The user's note on when to use it: coding. The user's defaults, used \
             when a call leaves them out: model gpt-5.5 and effort medium. For a hard task, \
             pass effort ultra."
        ),
        "{agent}"
    );
    // The hard-task effort is a value the schema allows.
    let efforts = spec.parameters["properties"]["effort"]["enum"]
        .as_array()
        .unwrap();
    assert!(efforts.contains(&"ultra".into()), "{efforts:?}");
}

#[tokio::test]
async fn a_call_that_leaves_out_model_or_effort_runs_with_the_user_s_defaults() {
    let (claude, codex, dsh) = (
        Recorder::new("claude"),
        Recorder::new("codex"),
        Recorder::new("dsh"),
    );
    let defaults = AgentDefaults {
        model: Some("opus[1m]".into()),
        effort: Some("medium".into()),
        hard_task_effort: Some("high".into()),
    };
    let with_defaults = |recorder: &Arc<Recorder>, holds_settings: bool| {
        let mut entry = offered(recorder);
        entry.accepts.session = true;
        entry.defaults = defaults.clone();
        entry.holds_settings = holds_settings;
        entry
    };
    // claude keeps a conversation's settings, as over ACP; codex runs its
    // CLI once per turn; dsh takes neither option.
    let tool = AgentTool::new(
        vec![
            with_defaults(&claude, true),
            with_defaults(&codex, false),
            with_defaults(&dsh, false),
        ],
        &["claude".to_owned()],
        timeouts(),
    );
    let last = |recorder: &Arc<Recorder>| {
        let ran = recorder.ran.lock().unwrap();
        let arguments = ran.last().unwrap();
        (arguments["model"].clone(), arguments["effort"].clone())
    };
    let pair = |model: &str, effort: &str| (json!(model), json!(effort));

    // Left out, or blank as models send it: the user's defaults.
    for arguments in [
        json!({"prompt":"hi"}),
        json!({"prompt":"hi","model":"","effort":" "}),
    ] {
        run(&tool, arguments).await.unwrap();
        assert_eq!(last(&claude), pair("opus[1m]", "medium"));
    }
    // The backend validates, and the user approves, what will run.
    tool.risk(&json!({"prompt":"hi"})).unwrap();
    let risked = claude.risked.lock().unwrap().last().cloned().unwrap();
    assert_eq!(risked["model"], "opus[1m]");
    // Named values win, one at a time; the hard-task effort is only ever
    // passed by the caller.
    run(&tool, json!({"prompt":"hi","effort":"high"}))
        .await
        .unwrap();
    assert_eq!(last(&claude), pair("opus[1m]", "high"));
    run(&tool, json!({"prompt":"hi","model":"sonnet"}))
        .await
        .unwrap();
    assert_eq!(last(&claude), pair("sonnet", "medium"));
    // A continued conversation keeps what its session holds.
    run(&tool, json!({"prompt":"more","session":"claude-1"}))
        .await
        .unwrap();
    assert_eq!(last(&claude), (Value::Null, Value::Null));
    // A CLI run once per turn gets them on every turn.
    run(
        &tool,
        json!({"agent":"codex","prompt":"more","session":"codex-1"}),
    )
    .await
    .unwrap();
    assert_eq!(last(&codex), pair("opus[1m]", "medium"));
    // An agent that takes neither is never given them.
    run(&tool, json!({"agent":"dsh","prompt":"hi"}))
        .await
        .unwrap();
    assert_eq!(last(&dsh), (Value::Null, Value::Null));
}

#[test]
fn approval_summaries_name_the_agent() {
    let (claude, codex) = (Recorder::new("claude"), Recorder::new("codex"));
    let tool = dispatcher(&[&claude, &codex], &["claude"]);
    assert_eq!(
        tool.approval_summary(&json!({"agent":"codex","prompt":"hi"}))
            .unwrap(),
        "agent codex: Launch codex with the prompt."
    );
    assert_eq!(
        tool.approval_summary(&json!({"prompt":"hi"})).unwrap(),
        "agent claude: Launch claude with the prompt."
    );
}

fn failed(agent: &str, error: &str) -> ToolOutput {
    ToolOutput::failed(
        ToolFailure::Failed,
        json!({"agent":agent,"status":"failed","reply":"","error":error}).to_string(),
    )
}

fn declined(agent: &str) -> ToolOutput {
    ToolOutput::failed(
        ToolFailure::Failed,
        json!({"agent":agent,"status":"declined","reply":"I won't.","note":output::DECLINED_NOTE})
            .to_string(),
    )
}

#[tokio::test]
async fn results_name_the_other_agents_as_values_for_agent() {
    let grok = Recorder::answering("grok", Ok(failed("grok", "HTTP 429: rate limited")));
    let (claude, codex) = (Recorder::new("claude"), Recorder::new("codex"));
    let tool = dispatcher(&[&claude, &codex, &grok], &["grok"]);
    let output = tool
        .execute(json!({"prompt":"hi"}), context())
        .await
        .unwrap();
    assert_eq!(output.failure, Some(ToolFailure::Unavailable));
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(
        value["fallback"],
        "This agent could not run: it is missing, signed out, or its provider returned an \
         error. Other agents are available: claude, codex. Call agent again with one of them."
    );

    // A declined request points to grok when it is offered.
    let claude = Recorder::answering("claude", Ok(declined("claude")));
    let tool = dispatcher(&[&claude, &codex, &grok], &["claude"]);
    let value = run(&tool, json!({"prompt":"hi"})).await.unwrap();
    assert_eq!(value["note"], output::DECLINED_NOTE_TRY_GROK);
    assert!(
        output::DECLINED_NOTE_TRY_GROK.contains("call agent again with agent grok"),
        "{}",
        output::DECLINED_NOTE_TRY_GROK
    );
    assert!(value.get("fallback").is_none(), "{value}");
    // Without grok, or from grok itself, the note stays: tell the user.
    for (tool, agent) in [
        (
            dispatcher(
                &[
                    &Recorder::answering("claude", Ok(declined("claude"))),
                    &codex,
                ],
                &["claude"],
            ),
            "claude",
        ),
        (
            dispatcher(
                &[&Recorder::answering("grok", Ok(declined("grok"))), &codex],
                &["grok"],
            ),
            "grok",
        ),
    ] {
        let value = run(&tool, json!({"prompt":"hi"})).await.unwrap();
        assert_eq!(value["agent"], agent);
        assert_eq!(value["note"], output::DECLINED_NOTE, "{agent}");
    }
}

#[test]
fn the_offered_agents_are_read_from_the_registry() {
    let mut registry = ToolRegistry::default();
    assert!(offered_agents(&registry).is_empty());
    let (pi, codex) = (Recorder::new("pi"), Recorder::new("codex"));
    registry
        .register(Arc::new(dispatcher(&[&pi, &codex], &[])))
        .unwrap();
    assert_eq!(offered_agents(&registry), ["codex", "pi"]);
}
