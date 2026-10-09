//! Unit tests for `src/delegate/review.rs`: the reviewed job's loop, run
//! through the `agent` tool against scripted agents whose conversations live
//! in a real store.

use super::*;
use crate::{
    AgentDefaults, BusyConfig,
    args::Timeouts,
    delegate::{
        agent::{Accepts, Backend},
        background::{BackgroundCapable, BackgroundJobs, CancelTool, StatusTool},
        conversation::ConversationLimits,
    },
};
use scv_protocol::{JobReport, LandingEvidence, LandingStatus};
use std::collections::VecDeque;
use tokio::sync::mpsc;

/// What a scripted agent does on one call.
#[derive(Debug, Clone)]
enum Step {
    /// It replies with `status`; its conversation can be continued unless
    /// `lost`.
    Reply {
        status: &'static str,
        reply: String,
        error: Option<String>,
        lost: bool,
    },
    /// SCV cannot start it, as for a missing executable.
    Missing,
    /// It runs until cancelled.
    Hang,
    /// It ignores cancellation and replies only once released, as an agent
    /// slow to stop does.
    Stuck(Arc<tokio::sync::Notify>),
    /// The same, replying this.
    StuckReply(Arc<tokio::sync::Notify>, String),
}

fn says(reply: impl Into<String>) -> Step {
    Step::Reply {
        status: "completed",
        reply: reply.into(),
        error: None,
        lost: false,
    }
}

fn ends(status: &'static str, reply: impl Into<String>, error: Option<&str>) -> Step {
    Step::Reply {
        status,
        reply: reply.into(),
        error: error.map(str::to_owned),
        lost: false,
    }
}

fn block(info: &str, body: &str) -> String {
    format!("I looked at it.\n\n```{info}\n{body}\n```")
}

fn verdict(body: &str) -> Step {
    says(block("scv-verdict", body))
}

const APPROVE: &str = r#"{"verdict":"approve","summary":"Correct and tested.","evidence":["ran cargo test: all passed"]}"#;
const APPROVE_RANGE: &str = r#"{"verdict":"approve","summary":"Correct.","evidence":["ran cargo test"],"approved":{"base":"1b2c3d4","head":"7e8f9a0"}}"#;
const CHANGES: &str = r#"{"verdict":"changes","summary":"A race remains.","findings":[{"severity":"blocking","title":"Sleep instead of a lock","location":"shop/src/cart.rs:88"},{"severity":"minor","title":"Typo"}]}"#;
const STILL_OPEN: &str =
    r#"{"verdict":"changes","summary":"Still racy.","prior":[{"id":"1.1","status":"open"}]}"#;
const RESOLVED: &str = r#"{"verdict":"approve","summary":"Fixed.","evidence":["ran it 20x"],"prior":[{"id":"1.1","status":"resolved"}]}"#;

/// One call an agent received.
#[derive(Debug, Clone)]
struct Call {
    agent: String,
    session: Option<String>,
    prompt: String,
    timeout: Option<u64>,
}

/// The scripted agents of one session.
#[derive(Default)]
struct Script {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    calls: Mutex<Vec<Call>>,
    /// Handles each agent's run got, in call order.
    handles: Mutex<Vec<(String, String)>>,
}

struct Scripted {
    name: String,
    conversations: Arc<ConversationStore>,
    script: Arc<Script>,
}

#[async_trait]
impl Backend for Scripted {
    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        Ok(format!(
            "Launch {} with timeout {:?}",
            self.name, args.timeout_seconds
        ))
    }

    fn busy(&self, arguments: &Value) -> Result<bool, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        Ok(args
            .session
            .is_some_and(|handle| self.conversations.is_busy(&handle)))
    }

    async fn wait_idle(
        &self,
        arguments: &Value,
        cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        match args.session {
            Some(handle) => self.conversations.wait_idle(&handle, cancellation).await,
            None => Ok(()),
        }
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: AgentArgs = parse_args(&arguments)?;
        lock(&self.script.calls).push(Call {
            agent: self.name.clone(),
            session: args.session.clone(),
            prompt: args.prompt.clone(),
            timeout: args.timeout_seconds,
        });
        let turn = self.conversations.begin(
            &self.name,
            args.session.as_deref(),
            std::path::Path::new("/w"),
            false,
        )?;
        let step = lock(&self.script.steps)
            .get_mut(&self.name)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("{} got an unscripted call: {}", self.name, args.prompt));
        if matches!(step, Step::Missing) {
            return Err(ToolError::unavailable(format!(
                "{} executable was not found on PATH",
                self.name
            )));
        }
        lock(&self.script.handles).push((self.name.clone(), turn.handle.clone()));
        let (status, reply, error, lost) = match step {
            Step::Reply {
                status,
                reply,
                error,
                lost,
            } => (status, reply, error, lost),
            Step::Hang => {
                context.cancellation.cancelled().await;
                drop(turn);
                return Err(ToolError::cancelled("cancelled"));
            }
            Step::Stuck(release) => {
                release.notified().await;
                ("completed", "Done, late.".to_owned(), None, false)
            }
            Step::StuckReply(release, reply) => {
                release.notified().await;
                ("completed", reply, None, false)
            }
            Step::Missing => unreachable!(),
        };
        let number = turn.turn;
        let session = if lost {
            turn.forget();
            None
        } else {
            turn.finish(Some(format!("v-{}", self.name)), status == "completed")
        };
        let mut value = json!({"agent": self.name, "status": status, "reply": reply,
                               "truncated": false});
        if let Some(session) = session {
            value["session"] = session.into();
            value["turn"] = number.into();
        }
        if let Some(error) = error {
            value["error"] = error.into();
        }
        let failure = match status {
            "completed" => None,
            "timeout" => Some(ToolFailure::Limit),
            "cancelled" => Some(ToolFailure::Cancelled),
            _ => Some(ToolFailure::Failed),
        };
        Ok(ToolOutput {
            content: value.to_string(),
            failure,
            truncated: false,
        })
    }
}

struct Fixture {
    tool: BackgroundCapable,
    jobs: Arc<BackgroundJobs>,
    conversations: Arc<ConversationStore>,
    script: Arc<Script>,
    finished: mpsc::UnboundedReceiver<()>,
    home: tempfile::TempDir,
}

/// A session offering `agents` (each continuable unless listed in
/// `once`), the first one preferred.
fn fixture_with(agents: &[&str], once: &[&str], limits: ConversationLimits) -> Fixture {
    let conversations = Arc::new(ConversationStore::new(limits, None));
    let script = Arc::new(Script::default());
    let offered = agents
        .iter()
        .map(|name| Offered {
            name: (*name).to_owned(),
            backend: Arc::new(Scripted {
                name: (*name).to_owned(),
                conversations: Arc::clone(&conversations),
                script: Arc::clone(&script),
            }),
            accepts: Accepts {
                session: !once.contains(name),
                ..Accepts::default()
            },
            model_hint: String::new(),
            offered: None,
            use_for: None,
            defaults: AgentDefaults::default(),
            holds_settings: true,
            busy: BusyConfig::default(),
        })
        .collect();
    let timeouts = Timeouts {
        default: Duration::from_secs(60),
        max: Duration::from_secs(600),
    };
    let (finished_tx, finished) = mpsc::unbounded_channel();
    let jobs = Arc::new(BackgroundJobs::new(4, Some(finished_tx)));
    let home = tempfile::tempdir().unwrap();
    let tool = BackgroundCapable {
        inner: Arc::new(AgentTool::new(offered, &[agents[0].to_owned()], timeouts)),
        jobs: Arc::clone(&jobs),
        conversations: Arc::clone(&conversations),
        reviews: Some(home.path().join("reviews")),
        session: Some("session-1".into()),
    };
    Fixture {
        tool,
        jobs,
        conversations,
        script,
        finished,
        home,
    }
}

fn fixture(agents: &[&str]) -> Fixture {
    fixture_with(
        agents,
        &[],
        ConversationLimits {
            max: 8,
            idle: Duration::from_secs(86400),
        },
    )
}

fn context() -> ToolContext {
    let mut context = ToolContext::new(std::env::temp_dir(), CancellationToken::new());
    context.call_id = "call-1".into();
    context
}

impl Fixture {
    fn script(&self, agent: &str, steps: impl IntoIterator<Item = Step>) {
        lock(&self.script.steps)
            .entry(agent.to_owned())
            .or_default()
            .extend(steps);
    }

    async fn start(&self, arguments: Value) -> Result<Value, ToolError> {
        let output = self.tool.execute(arguments, context()).await?;
        Ok(serde_json::from_str(&output.content).unwrap())
    }

    /// Wait for the job to finish: its report and its result.
    async fn finished(&mut self) -> (JobReport, Value) {
        tokio::time::timeout(Duration::from_secs(10), self.finished.recv())
            .await
            .expect("the job did not finish")
            .unwrap();
        let report = self.jobs.take_unreported().pop().expect("no report");
        let status = StatusTool {
            jobs: Arc::clone(&self.jobs),
        };
        let described = status
            .execute(json!({"job": report.job}), context())
            .await
            .unwrap();
        let described: Value = serde_json::from_str(&described.content).unwrap();
        (report, described["result"].clone())
    }

    fn calls(&self) -> Vec<Call> {
        lock(&self.script.calls).clone()
    }

    /// The agents called, in order.
    fn order(&self) -> Vec<String> {
        self.calls().into_iter().map(|call| call.agent).collect()
    }

    fn journal(&self) -> Vec<Value> {
        let dir = self.home.path().join("reviews");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        std::fs::read_to_string(&entries[0])
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn events(&self) -> Vec<String> {
        self.journal()
            .iter()
            .map(|event| {
                let name = event["event"].as_str().unwrap().to_owned();
                match event.get("kind").and_then(Value::as_str) {
                    Some(kind) => format!("{name}:{kind}"),
                    None => name,
                }
            })
            .collect()
    }

    /// Wait until `agent` has been called `times` times.
    async fn until_called(&self, agent: &str, times: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self
                .calls()
                .iter()
                .filter(|call| call.agent == agent)
                .count()
                < times
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the call never came");
    }
}

fn outcome(report: &JobReport) -> &scv_protocol::JobOutcome {
    report
        .outcome
        .as_ref()
        .expect("a reviewed job's report has an outcome")
}

#[tokio::test]
async fn an_approval_in_round_one_takes_one_builder_and_one_reviewer_turn() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Fixed the race on branch fix/race.")]);
    session.script("codex", [verdict(APPROVE)]);
    let started = session
        .start(json!({"prompt":"Fix the flaky checkout test.","review":{}}))
        .await
        .unwrap();
    assert_eq!(started["job"], "job-1");
    assert_eq!(
        started["review"]["reviewers"],
        json!(["codex", "grok", "claude"])
    );
    assert_eq!(started["review"]["rounds"], 3);
    assert_eq!(started["review"]["land"], "none");
    let journal = started["review"]["journal"].as_str().unwrap().to_owned();
    // The start change carries the journal, for a chat bridge to name.
    let changes = session.jobs.take_changes("call-1");
    assert_eq!(changes[0].journal.as_deref(), Some(journal.as_str()));
    let (report, result) = session.finished().await;
    assert_eq!(session.order(), ["claude", "codex"]);
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!((outcome.review.round, outcome.review.rounds), (1, 3));
    assert_eq!(outcome.review.reviewer, "codex");
    assert_eq!(outcome.review.fallback, None);
    assert_eq!(outcome.landing.status, LandingStatus::NotRequested);
    assert_eq!(report.status, JobStatus::Completed);
    assert_eq!(report.session.as_deref(), Some("claude-1"));
    assert_eq!(
        scv_protocol::outcome_notice(outcome),
        "job-1 · Review: approved · round 1 of 3 · reviewer codex\njob-1 · Landing: not requested"
    );
    // The result is the builder's, with SCV's review and landing.
    assert_eq!(result["reply"], "Fixed the race on branch fix/race.");
    assert_eq!(result["review"]["outcome"], "approved");
    assert_eq!(result["review"]["builder_session"], "claude-1");
    assert_eq!(result["landing"]["status"], "not_requested");
    // The reviewer's conversation is gone; the builder's is kept.
    assert_eq!(session.conversations.handles(), ["claude-1"]);
    // The builder heard the notice; the reviewer never learns the limit.
    let calls = session.calls();
    assert!(
        calls[0]
            .prompt
            .starts_with("Fix the flaky checkout test.\n\n[SCV review]")
    );
    assert!(calls[0].prompt.contains("up to 3 rounds"));
    assert!(!calls[1].prompt.contains("3 rounds"));
    assert!(
        calls[1]
            .prompt
            .contains("Fixed the race on branch fix/race.")
    );
    assert_eq!(calls[1].timeout, Some(60));
    assert_eq!(
        session.events(),
        [
            "review.started",
            "builder.started:round",
            "builder.finished:round",
            "reviewer.started:review",
            "reviewer.finished:review",
            "verdict",
            "review.finished",
        ]
    );
    let events = session.journal();
    assert_eq!(events[0]["job"], "job-1");
    assert_eq!(events[0]["session"], "session-1");
    assert_eq!(
        events[0]["reviewers"],
        json!([{"agent":"codex"},{"agent":"grok"},{"agent":"claude"}])
    );
    assert_eq!(events[6]["outcome"], "approved");
    let seqs: Vec<u64> = events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=7).collect::<Vec<_>>());
}

#[tokio::test]
async fn changes_then_approve_continues_the_builder_with_a_fresh_reviewer_each_round() {
    let mut session = fixture(&["codex", "claude", "grok"]);
    session.script("codex", [says("First try."), says("Used a lock.")]);
    session.script("claude", [verdict(CHANGES), verdict(RESOLVED)]);
    session
        .start(json!({"prompt":"Fix it.","review":{"rounds":3,"focus":"no sleeps"}}))
        .await
        .unwrap();
    let (report, result) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.review.round, 2);
    let calls = session.calls();
    assert_eq!(session.order(), ["codex", "claude", "codex", "claude"]);
    // Round 2 continues the builder's conversation...
    assert_eq!(calls[2].session.as_deref(), Some("codex-1"));
    assert!(calls[2].prompt.starts_with("[SCV review, round 2 of 3]"));
    assert!(
        calls[2]
            .prompt
            .contains("- 1.1 [blocking] \"Sleep instead of a lock\"")
    );
    assert!(calls[2].prompt.contains("- 1.2 [minor] \"Typo\""));
    // ...and each round's reviewer is a new conversation.
    assert_eq!(calls[3].session, None);
    let handles: Vec<String> = lock(&session.script.handles)
        .iter()
        .filter(|(agent, _)| agent == "claude")
        .map(|(_, handle)| handle.clone())
        .collect();
    assert_eq!(handles, ["claude-1", "claude-2"]);
    assert!(calls[3].prompt.contains("1.1 [blocking]"));
    assert!(calls[3].prompt.contains("Focus from the user: no sleeps"));
    assert_eq!(result["review"]["findings"], json!([]));
    assert_eq!(session.conversations.handles(), ["codex-1"]);
}

#[tokio::test]
async fn changes_in_the_last_round_leave_the_review_unresolved_without_another_builder_turn() {
    for rounds in [1_u32, 3] {
        let mut session = fixture(&["codex", "claude"]);
        session.script(
            "codex",
            (0..rounds).map(|round| says(format!("Try {round}."))),
        );
        let mut reviews = vec![verdict(CHANGES)];
        reviews.extend((1..rounds).map(|_| verdict(STILL_OPEN)));
        session.script("claude", reviews);
        session
            .start(json!({"prompt":"Fix it.","review":{"rounds":rounds}}))
            .await
            .unwrap();
        let (report, result) = session.finished().await;
        let outcome = outcome(&report);
        assert_eq!(outcome.review.outcome, ReviewOutcome::Unresolved);
        assert_eq!(outcome.review.reason.as_deref(), Some("round_limit"));
        assert_eq!(outcome.review.round, rounds);
        assert_eq!(outcome.review.open_count, 1);
        assert_eq!(outcome.review.open[0].id, "1.1");
        assert_eq!(
            session
                .order()
                .iter()
                .filter(|agent| *agent == "codex")
                .count(),
            usize::try_from(rounds).unwrap()
        );
        assert_eq!(report.status, JobStatus::Completed);
        assert_eq!(result["review"]["open_count"], 1);
        assert!(
            scv_protocol::outcome_notice(outcome).contains(&format!(
                "unresolved after {rounds} of {rounds} rounds · 1 blocking finding open:\n  - \
                 \"Sleep instead of a lock\" (shop/src/cart.rs:88)"
            )),
            "{}",
            scv_protocol::outcome_notice(outcome)
        );
    }
}

#[tokio::test]
async fn an_escalation_ends_the_review() {
    let mut session = fixture(&["codex", "claude"]);
    session.script("codex", [says("Dropped the table.")]);
    session.script(
        "claude",
        [verdict(
            r#"{"verdict":"escalate","summary":"Dropping a table needs the user."}"#,
        )],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Escalated);
    assert_eq!(
        outcome.review.summary.as_deref(),
        Some("Dropping a table needs the user.")
    );
}

#[tokio::test]
async fn an_unavailable_reviewer_hands_over_in_the_same_round_and_stays_skipped() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("One."), says("Two.")]);
    session.script("codex", [ends("failed", "", Some("Error: not logged in"))]);
    session.script("grok", [verdict(CHANGES), verdict(RESOLVED)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(
        session.order(),
        ["claude", "codex", "grok", "claude", "grok"]
    );
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.review.reviewer, "grok");
    assert_eq!(
        outcome.review.fallback.as_deref(),
        Some("codex unavailable")
    );
    assert_eq!(
        scv_protocol::outcome_notice(outcome)
            .lines()
            .next()
            .unwrap(),
        "job-1 · Review: approved · round 2 of 3 · reviewer grok (codex unavailable)"
    );
    // Every attempt's conversation is released.
    assert_eq!(session.conversations.handles(), ["claude-1"]);
}

#[tokio::test]
async fn a_missing_executable_counts_as_unavailable_and_the_last_resort_is_labelled() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Done."), verdict(APPROVE)]);
    session.script("codex", [Step::Missing]);
    session.script("grok", [ends("failed", "", Some("HTTP 429 rate limit"))]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.review.reviewer, "claude");
    assert_eq!(
        outcome.review.fallback.as_deref(),
        Some("same agent as builder: codex, grok unavailable")
    );
    // A fresh conversation, never the builder's.
    assert_eq!(session.calls()[3].session, None);
}

#[tokio::test]
async fn every_reviewer_unavailable_gives_no_verdict() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Done."), Step::Missing]);
    session.script("codex", [Step::Missing]);
    session.script("grok", [Step::Missing]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::NoVerdict);
    assert_eq!(
        outcome.review.reason.as_deref(),
        Some("no_reviewer_available")
    );
    assert!(
        scv_protocol::outcome_notice(outcome)
            .contains("no verdict in round 1 of 3 · no reviewer available (codex, grok, claude)")
    );
}

#[tokio::test]
async fn a_refusal_hands_the_review_to_grok_for_the_rest_of_the_job() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("One."), says("Two.")]);
    session.script("codex", [ends("declined", "I can't help with that.", None)]);
    // Round 2: codex is skipped, and grok being unavailable never reaches
    // the same-agent fallback.
    session.script(
        "grok",
        [
            verdict(CHANGES),
            ends("failed", "", Some("503 service unavailable")),
        ],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(
        session.order(),
        ["claude", "codex", "grok", "claude", "grok"]
    );
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::NoVerdict);
    assert_eq!(outcome.review.reason.as_deref(), Some("reviewer_declined"));
    assert_eq!(outcome.review.refusals.len(), 1);
    assert_eq!(outcome.review.refusals[0].agent, "codex");
    assert_eq!(outcome.review.refusals[0].reply, "I can't help with that.");
}

#[tokio::test]
async fn a_refusal_with_a_grok_builder_ends_in_no_verdict() {
    let mut session = fixture(&["grok", "claude", "codex"]);
    session.script("grok", [says("Done.")]);
    session.script("claude", [ends("declined", "No.", None)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(session.order(), ["grok", "claude"]);
    assert_eq!(
        outcome(&report).review.reason.as_deref(),
        Some("reviewer_declined")
    );
}

#[tokio::test]
async fn a_named_reviewer_is_never_swapped() {
    for (step, reason) in [
        (Step::Missing, "reviewer_unavailable"),
        (ends("declined", "No.", None), "reviewer_declined"),
    ] {
        let mut session = fixture(&["claude", "codex", "grok"]);
        session.script("claude", [says("Done.")]);
        session.script("codex", [step]);
        session
            .start(json!({"prompt":"Fix it.","review":{"agent":"codex"}}))
            .await
            .unwrap();
        let (report, _) = session.finished().await;
        assert_eq!(session.order(), ["claude", "codex"]);
        assert_eq!(outcome(&report).review.reason.as_deref(), Some(reason));
    }
}

#[tokio::test]
async fn a_negative_verdict_counts_however_the_run_ended_but_approval_needs_a_completed_run() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Done.")]);
    // It wrote a verdict, then its provider failed: no fallback around it.
    session.script(
        "codex",
        [ends(
            "failed",
            block(
                "scv-verdict",
                r#"{"verdict":"escalate","summary":"Needs the user."}"#,
            ),
            Some("HTTP 529 overloaded"),
        )],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(session.order(), ["claude", "codex"]);
    assert_eq!(outcome(&report).review.outcome, ReviewOutcome::Escalated);

    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Done.")]);
    session.script(
        "codex",
        [ends("timeout", block("scv-verdict", APPROVE), None)],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::NoVerdict);
    assert_eq!(outcome.review.reason.as_deref(), Some("reviewer_timeout"));
}

#[tokio::test]
async fn a_timeout_or_failure_without_an_availability_error_gives_no_verdict_and_no_fallback() {
    for (step, reason) in [
        (ends("timeout", "", None), "reviewer_timeout"),
        (ends("failed", "", Some("HTTP 500")), "reviewer_failed"),
        (ends("cancelled", "", None), "reviewer_cancelled"),
    ] {
        let mut session = fixture(&["claude", "codex", "grok"]);
        session.script("claude", [says("Done.")]);
        session.script("codex", [step]);
        session
            .start(json!({"prompt":"Fix it.","review":{}}))
            .await
            .unwrap();
        let (report, _) = session.finished().await;
        assert_eq!(session.order(), ["claude", "codex"], "{reason}");
        assert_eq!(outcome(&report).review.reason.as_deref(), Some(reason));
        assert_eq!(session.conversations.handles(), ["claude-1"]);
    }
}

#[tokio::test]
async fn a_malformed_verdict_gets_one_repair_turn_in_the_same_conversation() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    session.script("codex", [says("LGTM!"), verdict(APPROVE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(outcome(&report).review.outcome, ReviewOutcome::Approved);
    let calls = session.calls();
    assert_eq!(calls[2].session.as_deref(), Some("codex-1"));
    assert!(
        calls[2]
            .prompt
            .starts_with("Your reply had no valid scv-verdict block (no scv-verdict block)")
    );
    assert_eq!(calls[2].timeout, Some(60));
    assert!(
        session
            .events()
            .contains(&"reviewer.started:repair".to_owned())
    );

    // Malformed twice.
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    session.script("codex", [says("LGTM!"), says("Still LGTM!")]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(
        outcome(&report).review.reason.as_deref(),
        Some("malformed_verdict")
    );
    assert_eq!(session.order(), ["claude", "codex", "codex"]);
}

#[tokio::test]
async fn a_reviewer_that_cannot_be_continued_gets_no_repair() {
    let mut session = fixture_with(
        &["claude", "codex"],
        &["codex"],
        ConversationLimits {
            max: 8,
            idle: Duration::from_secs(86400),
        },
    );
    session.script("claude", [says("Done.")]);
    session.script(
        "codex",
        [Step::Reply {
            status: "completed",
            reply: "LGTM".into(),
            error: None,
            lost: true,
        }],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(session.order(), ["claude", "codex"]);
    assert_eq!(
        outcome(&report).review.reason.as_deref(),
        Some("malformed_verdict")
    );
}

#[tokio::test]
async fn a_builder_turn_that_does_not_complete_stops_the_review() {
    for (step, reason, status) in [
        (
            ends("failed", "", Some("boom")),
            "builder_failed",
            JobStatus::Failed,
        ),
        (
            ends("timeout", "", None),
            "builder_timeout",
            JobStatus::Timeout,
        ),
        (
            ends("declined", "No.", None),
            "builder_declined",
            JobStatus::Declined,
        ),
    ] {
        let mut session = fixture(&["claude", "codex"]);
        session.script("claude", [step]);
        session
            .start(json!({"prompt":"Fix it.","review":{}}))
            .await
            .unwrap();
        let (report, _) = session.finished().await;
        let outcome = outcome(&report);
        assert_eq!(outcome.review.outcome, ReviewOutcome::Stopped);
        assert_eq!(outcome.review.reason.as_deref(), Some(reason));
        assert_eq!(report.status, status);
    }
    // In round 2 too.
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("One."), ends("failed", "", Some("boom"))]);
    session.script("codex", [verdict(CHANGES)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(outcome(&report).review.round, 2);
    assert_eq!(
        outcome(&report).review.reason.as_deref(),
        Some("builder_failed")
    );
    // A first turn without a conversation to continue.
    let mut session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [Step::Reply {
            status: "completed",
            reply: "Done.".into(),
            error: None,
            lost: true,
        }],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(
        outcome(&report).review.reason.as_deref(),
        Some("builder_no_session")
    );
}

const LANDED: &str = r#"{"status":"landed","ref":"origin/main","commits":["4f2a9c1e0b7d"],"detail":"squash-merged"}"#;

#[tokio::test]
async fn landing_after_approval_runs_one_landing_turn_and_the_approver_confirms_it() {
    let mut session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [
            says("Committed 1b2c3d4..7e8f9a0."),
            says(block("scv-landing", LANDED)),
        ],
    );
    session.script(
        "codex",
        [
            verdict(APPROVE_RANGE),
            says(block(
                "scv-landing-check",
                r#"{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1"],"evidence":["diff equals 1b2c3d4..7e8f9a0"]}"#,
            )),
        ],
    );
    let started = session
        .start(json!({"prompt":"Fix it and land it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    assert_eq!(started["review"]["land"], "after_approval");
    let (report, result) = session.finished().await;
    assert_eq!(session.order(), ["claude", "codex", "claude", "codex"]);
    let calls = session.calls();
    assert!(calls[0].prompt.contains("name its base and head commits"));
    assert!(
        calls[2]
            .prompt
            .starts_with("[SCV review] The independent review approved round 1.")
    );
    // The confirmation continues the approving reviewer's conversation.
    assert_eq!(calls[3].session.as_deref(), Some("codex-1"));
    assert!(
        calls[3]
            .prompt
            .contains("You approved 1b2c3d4..7e8f9a0 in round 1")
    );
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.review.round, 1);
    assert_eq!(outcome.landing.status, LandingStatus::Landed);
    assert_eq!(
        outcome.landing.evidence,
        Some(LandingEvidence::ReviewerConfirmed)
    );
    assert!(!outcome.landing.unauthorized && !outcome.landing.landed_before_review);
    assert_eq!(
        scv_protocol::outcome_notice(outcome),
        "job-1 · Review: approved · round 1 of 3 · reviewer codex\n\
         job-1 · Landing: landed · 4f2a9c1 → origin/main · confirmed by reviewer codex"
    );
    assert_eq!(result["landing"]["evidence"], "reviewer_confirmed");
    // The approving reviewer is released after the confirmation.
    assert_eq!(session.conversations.handles(), ["claude-1"]);
    assert!(
        session
            .events()
            .contains(&"reviewer.started:confirmation".to_owned())
    );
    assert!(session.events().contains(&"landing_check".to_owned()));
}

#[tokio::test]
async fn a_confirmation_without_a_usable_result_leaves_the_landing_unconfirmed() {
    for (step, reason) in [
        (ends("timeout", "", None), "the confirmation timed out"),
        (says("It looks fine."), "malformed confirmation"),
        (
            ends("declined", "No.", None),
            "the approving reviewer declined the confirmation",
        ),
        (Step::Missing, "the approving reviewer was unavailable"),
    ] {
        let mut session = fixture(&["claude", "codex", "grok"]);
        session.script(
            "claude",
            [says("Committed."), says(block("scv-landing", LANDED))],
        );
        session.script("codex", [verdict(APPROVE_RANGE), step]);
        session
            .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
            .await
            .unwrap();
        let (report, _) = session.finished().await;
        let outcome = outcome(&report);
        assert_eq!(outcome.review.outcome, ReviewOutcome::Approved, "{reason}");
        assert_eq!(outcome.review.round, 1);
        assert_eq!(outcome.landing.evidence, Some(LandingEvidence::Unconfirmed));
        assert_eq!(outcome.landing.reason.as_deref(), Some(reason));
        // No repair and no other agent.
        assert_eq!(
            session.order(),
            ["claude", "codex", "claude", "codex"],
            "{reason}"
        );
        assert_eq!(report.status, JobStatus::Completed);
    }
}

#[tokio::test]
async fn a_disputed_landing_stays_approved_and_nothing_is_fixed() {
    let mut session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [says("Committed."), says(block("scv-landing", LANDED))],
    );
    session.script(
        "codex",
        [
            verdict(APPROVE_RANGE),
            says(block(
                "scv-landing-check",
                r#"{"status":"mismatch","ref":"origin/main","note":"a conflict was resolved"}"#,
            )),
        ],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(
        scv_protocol::outcome_notice(outcome)
            .lines()
            .last()
            .unwrap(),
        "job-1 · Landing: landed · 4f2a9c1 → origin/main · reviewer codex could NOT confirm: the \
         landed change does not match the approved one"
    );
    assert_eq!(session.order().len(), 4);
}

#[tokio::test]
async fn an_approved_change_that_did_not_land_says_so_and_gets_no_confirmation() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script(
        "claude",
        [
            says("Committed."),
            says(block(
                "scv-landing",
                r#"{"status":"not_landed","detail":"cargo deny failed"}"#,
            )),
        ],
    );
    session.script("codex", [verdict(APPROVE_RANGE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    let (report, result) = session.finished().await;
    assert_eq!(session.order(), ["claude", "codex", "claude"]);
    let not_landed = outcome(&report);
    assert_eq!(not_landed.review.outcome, ReviewOutcome::Approved);
    assert_eq!(not_landed.landing.status, LandingStatus::NotLanded);
    assert_eq!(result["note"], NOT_LANDED_NOTE);
    assert_eq!(session.conversations.handles(), ["claude-1"]);

    // A landing turn that fails: approved, landing unknown, the turn's status.
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Committed."), ends("timeout", "", None)]);
    session.script("codex", [verdict(APPROVE_RANGE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    let (report, result) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.landing.status, LandingStatus::Unknown);
    assert_eq!(
        outcome.landing.reason.as_deref(),
        Some("the landing turn timed out")
    );
    assert_eq!(report.status, JobStatus::Timeout);
    assert_eq!(result["note"], NOT_LANDED_NOTE);
}

#[tokio::test]
async fn an_approval_that_lands_after_review_must_name_the_approved_commits() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Committed.")]);
    session.script("codex", [verdict(APPROVE), verdict(APPROVE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.reason.as_deref(), Some("malformed_verdict"));
    assert_eq!(outcome.landing.status, LandingStatus::NotAttempted);
    assert_eq!(
        outcome.landing.reason.as_deref(),
        Some("review not approved")
    );
}

#[tokio::test]
async fn landing_before_review_is_checked_and_fixes_land_on_top() {
    let mut session = fixture(&["codex", "claude"]);
    session.script(
        "codex",
        [
            says(block(
                "scv-landing",
                r#"{"status":"landed","ref":"origin/main","commits":["4f2a9c1"]}"#,
            )),
            says(block(
                "scv-landing",
                r#"{"status":"landed","ref":"origin/main","commits":["9e8d7c6"]}"#,
            )),
        ],
    );
    session.script(
        "claude",
        [
            verdict(
                r#"{"verdict":"changes","summary":"A null cart still crashes.",
                    "findings":[{"severity":"blocking","title":"Null cart crash"}],
                    "landing_check":{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1"],"evidence":["git branch -r --contains"]}}"#,
            ),
            verdict(
                r#"{"verdict":"approve","summary":"Fixed.","evidence":["ran it"],
                    "prior":[{"id":"1.1","status":"resolved"}],
                    "landing_check":{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1","9e8d7c6"],"evidence":["git log origin/main"]}}"#,
            ),
        ],
    );
    session
        .start(json!({"prompt":"Hotfix it.","review":{"land":"before_review"}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let calls = session.calls();
    assert!(
        calls[1]
            .prompt
            .contains("landed 4f2a9c1 on origin/main this round")
    );
    assert!(
        calls[2]
            .prompt
            .contains("already landed on origin/main (4f2a9c1)")
    );
    assert!(calls[2].prompt.contains("Land the fix as before"));
    let landed = outcome(&report);
    assert_eq!(landed.review.outcome, ReviewOutcome::Approved);
    assert_eq!(
        scv_protocol::outcome_notice(landed).lines().last().unwrap(),
        "job-1 · Landing: landed before review · 4f2a9c1, 9e8d7c6 → origin/main · confirmed by \
         reviewer claude"
    );
    // A round turn without a report is unknown.
    let mut session = fixture(&["codex", "claude"]);
    session.script("codex", [says("Landed, I think.")]);
    session.script("claude", [verdict(APPROVE)]);
    session
        .start(json!({"prompt":"Hotfix it.","review":{"land":"before_review"}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(outcome(&report).landing.status, LandingStatus::Unknown);
}

#[tokio::test]
async fn an_unauthorized_landing_is_shown_as_such() {
    let mut session = fixture(&["codex", "claude"]);
    session.script("codex", [says(block("scv-landing", LANDED))]);
    session.script(
        "claude",
        [verdict(
            r#"{"verdict":"escalate","summary":"It landed without permission.",
                "landing_check":{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1"],"evidence":["git log"]}}"#,
        )],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let outcome = outcome(&report);
    assert_eq!(outcome.review.outcome, ReviewOutcome::Escalated);
    assert!(outcome.landing.unauthorized);
    assert!(
        scv_protocol::outcome_notice(outcome)
            .ends_with("confirmed by reviewer claude · NOT authorized by this call"),
        "{}",
        scv_protocol::outcome_notice(outcome)
    );
}

#[tokio::test]
async fn cancelling_during_the_review_stops_it_and_releases_everything() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    session.script("codex", [Step::Hang]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    session.until_called("codex", 1).await;
    // While it runs, agent_status shows where the review stands.
    let status = StatusTool {
        jobs: Arc::clone(&session.jobs),
    };
    let running = status
        .execute(json!({"job":"job-1"}), context())
        .await
        .unwrap();
    let running: Value = serde_json::from_str(&running.content).unwrap();
    assert_eq!(running["review"]["phase"], "reviewer");
    assert_eq!(running["review"]["round"], 1);
    assert_eq!(running["review"]["reviewer"], "codex");
    assert_eq!(running["review"]["builder_session"], "claude-1");
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    cancel
        .execute(json!({"job":"job-1"}), cancelling)
        .await
        .unwrap();
    let change = session.jobs.take_changes("cancel").pop().unwrap();
    let outcome = change.outcome.unwrap();
    assert_eq!(outcome.review.outcome, ReviewOutcome::Stopped);
    assert_eq!(outcome.review.reason.as_deref(), Some("cancelled"));
    assert_eq!(
        scv_protocol::outcome_notice(&outcome)
            .lines()
            .next()
            .unwrap(),
        "job-1 · Review: NOT approved · stopped in round 1 of 3 · the job was cancelled"
    );
    tokio::time::timeout(Duration::from_secs(5), session.finished.recv())
        .await
        .unwrap();
    let events = session.journal();
    let finished = events.last().unwrap();
    assert_eq!(finished["event"], "review.finished");
    assert_eq!(finished["outcome"], "stopped");
    assert_eq!(finished["reason"], "cancelled");
    assert_eq!(finished["status"], "cancelled");
    // The reviewer's first turn was abandoned; the builder's pin is gone.
    assert_eq!(session.conversations.handles(), ["claude-1"]);
}

#[tokio::test]
async fn cancelling_during_the_landing_turn_keeps_the_approval_and_leaves_the_landing_unknown() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Committed."), Step::Hang]);
    session.script("codex", [verdict(APPROVE_RANGE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    session.until_called("claude", 2).await;
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    cancel
        .execute(json!({"job":"job-1"}), cancelling)
        .await
        .unwrap();
    let outcome = session
        .jobs
        .take_changes("cancel")
        .pop()
        .unwrap()
        .outcome
        .unwrap();
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.landing.status, LandingStatus::Unknown);
    assert_eq!(
        outcome.landing.reason.as_deref(),
        Some("the job was cancelled during a turn that may land")
    );
    tokio::time::timeout(Duration::from_secs(5), session.finished.recv())
        .await
        .unwrap();
    let finished = session.journal().pop().unwrap();
    assert_eq!(finished["outcome"], "approved");
    assert_eq!(finished["landing"]["status"], "unknown");
    // The kept approving reviewer is released too.
    assert_eq!(session.conversations.handles(), ["claude-1"]);
}

#[tokio::test]
async fn cancelling_during_the_confirmation_leaves_the_landing_unconfirmed() {
    let mut session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [says("Committed."), says(block("scv-landing", LANDED))],
    );
    session.script("codex", [verdict(APPROVE_RANGE), Step::Hang]);
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    session.until_called("codex", 2).await;
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    cancel
        .execute(json!({"job":"job-1"}), cancelling)
        .await
        .unwrap();
    let outcome = session
        .jobs
        .take_changes("cancel")
        .pop()
        .unwrap()
        .outcome
        .unwrap();
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!(outcome.landing.status, LandingStatus::Landed);
    assert_eq!(outcome.landing.evidence, Some(LandingEvidence::Unconfirmed));
    assert_eq!(
        outcome.landing.reason.as_deref(),
        Some("the job was cancelled during the confirmation")
    );
    tokio::time::timeout(Duration::from_secs(5), session.finished.recv())
        .await
        .unwrap();
    assert_eq!(session.conversations.handles(), ["claude-1"]);
}

#[tokio::test]
async fn a_reviewed_call_cancelled_while_queued_still_ends_its_journal() {
    let mut session = fixture(&["claude", "codex"]);
    // An earlier conversation, busy with a turn.
    session.script("claude", [says("First."), Step::Hang]);
    session
        .start(json!({"prompt":"Start.","background":true}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.finished.recv())
        .await
        .unwrap();
    let _ = session.jobs.take_unreported();
    session
        .start(json!({"prompt":"Keep going.","session":"claude-1","background":true}))
        .await
        .unwrap();
    session.until_called("claude", 2).await;
    let queued = session
        .start(json!({"prompt":"Fix it.","session":"claude-1","review":{}}))
        .await
        .unwrap();
    assert_eq!(queued["queued"], true);
    assert_eq!(queued["job"], "job-3");
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    cancel
        .execute(json!({"job":"job-3"}), cancelling)
        .await
        .unwrap();
    let outcome = session
        .jobs
        .take_changes("cancel")
        .pop()
        .unwrap()
        .outcome
        .unwrap();
    assert_eq!(
        scv_protocol::outcome_notice(&outcome),
        "job-3 · Review: NOT approved · stopped before round 1 · the job was cancelled while \
         queued\njob-3 · Landing: not requested"
    );
    tokio::time::timeout(Duration::from_secs(5), session.finished.recv())
        .await
        .unwrap();
    // The tool that owned the journal is dropped once its task ends.
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.journal().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let events = session.journal();
    assert_eq!(events[0]["event"], "review.started");
    assert_eq!(events[1]["event"], "review.finished");
    assert_eq!(events[1]["outcome"], "stopped");
    assert_eq!(events[1]["round"], 0);
    // The busy conversation still runs its own turn.
    assert_eq!(session.order(), ["claude", "claude"]);
}

#[tokio::test]
async fn a_reviewed_call_queues_instead_of_steering_and_fails_when_told_to() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("First."), Step::Hang]);
    session
        .start(json!({"prompt":"Start.","background":true}))
        .await
        .unwrap();
    session.finished.recv().await.unwrap();
    session
        .start(json!({"prompt":"Busy.","session":"claude-1","background":true}))
        .await
        .unwrap();
    session.until_called("claude", 2).await;
    let failed = session
        .start(json!({"prompt":"Fix it.","session":"claude-1","on_busy":"fail","review":{}}))
        .await
        .unwrap_err();
    assert!(
        failed.message.contains("session busy"),
        "{}",
        failed.message
    );
    let queued = session
        .start(json!({"prompt":"Fix it.","session":"claude-1","on_busy":"steer","review":{}}))
        .await
        .unwrap();
    assert_eq!(queued["queued"], true);
    // A refused call leaves no journal behind.
    let journals = std::fs::read_dir(session.home.path().join("reviews"))
        .unwrap()
        .count();
    assert_eq!(journals, 1);
}

#[tokio::test]
async fn no_room_for_a_reviewer_gives_no_verdict_and_tries_no_other_agent() {
    let mut session = fixture_with(
        &["claude", "codex", "grok"],
        &[],
        ConversationLimits {
            max: 2,
            idle: Duration::from_secs(86400),
        },
    );
    // Another conversation is busy, and the builder's is pinned.
    session.script("grok", [Step::Hang]);
    session
        .start(json!({"agent":"grok","prompt":"Elsewhere.","background":true}))
        .await
        .unwrap();
    session.until_called("grok", 1).await;
    session.script("claude", [says("Done.")]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let slotless = outcome(&report);
    assert_eq!(
        slotless.review.reason.as_deref(),
        Some("no_conversation_slot")
    );
    assert_eq!(slotless.review.tried[0].result, ReviewerResult::NoSlot);
    // The reviewer could not start, and no other agent was tried.
    assert_eq!(session.order(), ["grok", "claude", "codex"]);
}

#[tokio::test]
async fn the_pinned_builder_survives_idle_expiry_during_a_long_review() {
    let mut session = fixture_with(
        &["claude", "codex"],
        &[],
        ConversationLimits {
            max: 8,
            idle: Duration::from_millis(1),
        },
    );
    session.script("claude", [says("One."), says("Two.")]);
    session.script("codex", [verdict(CHANGES), verdict(RESOLVED)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(outcome(&report).review.outcome, ReviewOutcome::Approved);
    assert_eq!(session.calls()[2].session.as_deref(), Some("claude-1"));
}

#[tokio::test]
async fn a_reviewed_call_is_checked_before_anything_starts() {
    let session = fixture_with(
        &["claude", "codex", "dsh"],
        &["dsh"],
        ConversationLimits {
            max: 8,
            idle: Duration::from_secs(86400),
        },
    );
    for (arguments, error) in [
        (
            json!({"prompt":"x","background":false,"review":{}}),
            "runs as a background job",
        ),
        (
            json!({"agent":"dsh","prompt":"x","review":{}}),
            "dsh cannot continue a conversation, which a reviewed call needs for its fix rounds; \
             these agents can: claude, codex",
        ),
        (json!({"prompt":"x","review":{"rounds":0}}), "from 1 to 20"),
        (json!({"prompt":"x","review":{"rounds":21}}), "from 1 to 20"),
        (
            json!({"prompt":"x","review":{"land":"now"}}),
            "is not after_approval",
        ),
        (
            json!({"prompt":"x","review":{"focus":"x".repeat(2049)}}),
            "at most 2 KiB",
        ),
        (
            json!({"prompt":"x","review":{"model":"m"}}),
            "review.model needs review.agent",
        ),
        (json!({"prompt":"x","review":{"agent":"pi"}}), "not offered"),
        (json!({"prompt":"x","review":{"extra":1}}), "unknown field"),
        (
            json!({"prompt":"x","review":{"timeout_seconds":601}}),
            "timeout",
        ),
    ] {
        let refused = session.tool.risk(&arguments).unwrap_err();
        assert!(
            refused.message.contains(error),
            "{arguments}: {}",
            refused.message
        );
        assert_eq!(refused.kind, ToolFailure::InvalidArguments, "{arguments}");
    }
    assert!(session.calls().is_empty());
    // No journal was created for any of them.
    assert!(!session.home.path().join("reviews").exists());
    let small = fixture_with(
        &["claude", "codex"],
        &[],
        ConversationLimits {
            max: 1,
            idle: Duration::from_secs(86400),
        },
    );
    let refused = small
        .tool
        .risk(&json!({"prompt":"x","review":{}}))
        .unwrap_err();
    assert!(
        refused.message.contains("room for two conversations"),
        "{}",
        refused.message
    );
}

#[tokio::test]
async fn the_approval_names_every_reviewer_launch_the_rounds_and_the_landing_mode() {
    let session = fixture(&["claude", "codex", "grok"]);
    let summary = session
        .tool
        .approval_summary(&json!({"prompt":"x","review":{"land":"after_approval"}}))
        .unwrap();
    assert!(
        summary.contains("Reviewed: up to 3 rounds, then LANDS AFTER APPROVAL"),
        "{summary}"
    );
    assert!(summary.contains("which the approving reviewer then checks in one confirmation turn"));
    assert!(summary.contains("Reviewer each round, the first available of:"));
    assert!(summary.contains("a fresh codex conversation: Launch codex with timeout Some(60)"));
    assert!(summary.contains("a fresh grok conversation: Launch grok"));
    assert!(summary.contains("a fresh claude conversation (same agent as builder): Launch claude"));
    let before = session
        .tool
        .approval_summary(&json!({"prompt":"x","review":{"land":"before_review","agent":"grok"}}))
        .unwrap();
    assert!(before.contains("LANDS BEFORE REVIEW"), "{before}");
    assert!(before.contains("Reviewer each round:\n  a fresh grok conversation"));
    let none = session
        .tool
        .approval_summary(&json!({"prompt":"x","review":{}}))
        .unwrap();
    assert!(
        none.contains("Reviewed: up to 3 rounds; does not land."),
        "{none}"
    );
}

#[tokio::test]
async fn reviewers_the_session_does_not_offer_are_dropped_and_other_builders_get_claude_first() {
    let session = fixture(&["dsh", "claude", "codex", "grok"]);
    let started = session
        .tool
        .approval_summary(&json!({"prompt":"x","review":{}}))
        .unwrap();
    let order: Vec<&str> = started
        .lines()
        .filter_map(|line| line.trim().strip_prefix("a fresh "))
        .map(|line| line.split(' ').next().unwrap())
        .collect();
    assert_eq!(order, ["claude", "codex", "grok", "dsh"]);
    let grok = fixture(&["grok", "claude"]);
    let started = grok
        .tool
        .approval_summary(&json!({"prompt":"x","review":{}}))
        .unwrap();
    let order: Vec<&str> = started
        .lines()
        .filter_map(|line| line.trim().strip_prefix("a fresh "))
        .map(|line| line.split(' ').next().unwrap())
        .collect();
    assert_eq!(order, ["claude", "grok"]);
}

#[tokio::test]
async fn review_is_offered_only_with_a_journal_and_a_continuable_agent() {
    let session = fixture(&["claude", "codex"]);
    let spec = session.tool.spec();
    assert!(spec.parameters.pointer("/properties/review").is_some());
    assert_eq!(
        spec.parameters
            .pointer("/properties/review/properties/rounds/maximum"),
        Some(&json!(20))
    );
    let plain =
        BackgroundCapable::plain(Arc::clone(&session.tool.inner), Arc::clone(&session.jobs));
    assert!(
        plain
            .spec()
            .parameters
            .pointer("/properties/review")
            .is_none()
    );
    let refused = plain.risk(&json!({"prompt":"x","review":{}})).unwrap_err();
    assert!(
        refused.message.contains("not available"),
        "{}",
        refused.message
    );
    let once = fixture_with(
        &["claude"],
        &["claude"],
        ConversationLimits {
            max: 8,
            idle: Duration::from_secs(86400),
        },
    );
    assert!(
        once.tool
            .spec()
            .parameters
            .pointer("/properties/review")
            .is_none()
    );
}

#[tokio::test]
async fn the_report_tells_the_model_how_to_speak_of_reviewed_jobs() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    session.script("codex", [verdict(APPROVE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let prompt = crate::delegate::background::report_prompt(std::slice::from_ref(&report));
    assert!(
        prompt.contains("SCV shows the user its own Review and Landing lines"),
        "{prompt}"
    );
    assert!(
        prompt.contains("Review: approved · round 1 of 3 · reviewer codex\nLanding: not requested")
    );
    let plain = JobReport {
        outcome: None,
        ..report
    };
    assert!(
        !crate::delegate::background::report_prompt(&[plain]).contains("Review and Landing lines")
    );
}

/// Run a reviewed job of `call` on `session`'s agents directly, with its
/// journal failing from event `fail_from` on.
async fn run_with_failing_journal(session: &Fixture, call: Value, fail_from: u64) -> Value {
    let split_review = call["review"].clone();
    let mut builder_call = call.clone();
    builder_call.as_object_mut().unwrap().remove("review");
    let agents = Arc::clone(&session.tool.inner);
    let (builder, routed) = agents.route(&builder_call).unwrap();
    let plan = plan(&agents, builder, &routed, &split_review, 8).unwrap();
    let tool = ReviewedTool::new(
        Arc::clone(&agents),
        Arc::default(),
        Arc::clone(&session.conversations),
        plan,
        builder_call,
        None,
        &session.home.path().join("reviews"),
    )
    .unwrap();
    tool.begin("job-1").unwrap();
    lock(&tool.record.journal).fail_from = Some(fail_from);
    let output = tool.execute(Value::Null, context()).await.unwrap();
    serde_json::from_str(&output.content).unwrap()
}

#[tokio::test]
async fn a_journal_that_cannot_be_written_stops_the_review() {
    let session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    // review.started is event 1; builder.started, 2; builder.finished, 3.
    let result =
        run_with_failing_journal(&session, json!({"prompt":"Fix it.","review":{}}), 3).await;
    assert_eq!(result["review"]["outcome"], "stopped");
    assert_eq!(result["review"]["reason"], "journal_error");
    assert_eq!(result["review"]["journal_incomplete"], true);
    assert_eq!(session.order(), ["claude"]);
    assert_eq!(
        scv_protocol::outcome_notice(
            &serde_json::from_value(json!({
                "job":"job-1","review":result["review"],"landing":result["landing"]
            }))
            .unwrap()
        )
        .lines()
        .next()
        .unwrap(),
        "job-1 · Review: NOT approved · stopped in round 1 of 3 · the journal could not be written"
    );
}

#[tokio::test]
async fn a_landing_whose_journal_write_fails_is_still_recorded() {
    let session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [says("Committed."), says(block("scv-landing", LANDED))],
    );
    session.script("codex", [verdict(APPROVE_RANGE)]);
    // 1 started, 2-3 builder, 4-5 reviewer, 6 verdict, 7 landing turn
    // started, 8 its builder.finished.
    let result = run_with_failing_journal(
        &session,
        json!({"prompt":"Fix it.","review":{"land":"after_approval"}}),
        8,
    )
    .await;
    // The approval was fixed first and stands, the journal shown
    // incomplete; the landing counts, and the confirmation it was due never
    // ran, so it is NOT confirmed rather than merely builder-reported.
    assert_eq!(result["review"]["outcome"], "approved");
    assert_eq!(result["review"]["journal_incomplete"], true);
    assert_eq!(result["landing"]["status"], "landed");
    assert_eq!(result["landing"]["evidence"], "unconfirmed");
    assert_eq!(
        result["landing"]["reason"],
        "the confirmation did not run: the journal could not be written"
    );
    assert_eq!(session.order(), ["claude", "codex", "claude"]);
    assert_eq!(session.conversations.handles(), ["claude-1"]);
}

#[tokio::test]
async fn a_failed_final_journal_write_leaves_the_approval_and_says_the_journal_is_incomplete() {
    let session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    session.script("codex", [verdict(APPROVE)]);
    // 1 started, 2-3 builder, 4-5 reviewer, 6 verdict, 7 review.finished.
    let result =
        run_with_failing_journal(&session, json!({"prompt":"Fix it.","review":{}}), 7).await;
    assert_eq!(result["review"]["outcome"], "approved", "{result}");
    assert_eq!(result["review"]["journal_incomplete"], true, "{result}");
    let outcome: scv_protocol::JobOutcome = serde_json::from_value(json!({
        "job":"job-1","review":result["review"],"landing":result["landing"]
    }))
    .unwrap();
    assert!(
        scv_protocol::outcome_notice(&outcome)
            .lines()
            .next()
            .unwrap()
            .ends_with("INCOMPLETE: a write failed"),
        "{}",
        scv_protocol::outcome_notice(&outcome)
    );
    // The journal has no end, as for a crash: its result is unknown there.
    let events: Vec<String> = session
        .journal()
        .iter()
        .map(|event| event["event"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(events.last().map(String::as_str), Some("verdict"));
}

#[tokio::test]
async fn a_journal_failure_after_a_landing_turn_keeps_that_turns_status_and_reply() {
    let session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [
            says("Committed."),
            ends("timeout", "The landing timed out after a push.", None),
        ],
    );
    session.script("codex", [verdict(APPROVE_RANGE)]);
    // 8 is the landing turn's builder.finished.
    let result = run_with_failing_journal(
        &session,
        json!({"prompt":"Fix it.","review":{"land":"after_approval"}}),
        8,
    )
    .await;
    assert_eq!(result["status"], "timeout", "{result}");
    assert_eq!(result["reply"], "The landing timed out after a push.");
    assert_eq!(result["review"]["outcome"], "approved");
    assert_eq!(result["landing"]["status"], "unknown");
    assert_eq!(result["landing"]["reason"], "the landing turn timed out");
}

#[tokio::test]
async fn a_queued_cancel_gives_the_same_outcome_in_the_result_the_change_and_the_journal() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("First."), Step::Hang]);
    session
        .start(json!({"prompt":"Start.","background":true}))
        .await
        .unwrap();
    session.finished.recv().await.unwrap();
    let _ = session.jobs.take_unreported();
    session
        .start(json!({"prompt":"Busy.","session":"claude-1","background":true}))
        .await
        .unwrap();
    session.until_called("claude", 2).await;
    session
        .start(json!({"prompt":"Review it.","session":"claude-1","review":{}}))
        .await
        .unwrap();
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    let output = cancel
        .execute(json!({"job":"job-3"}), cancelling)
        .await
        .unwrap();
    let described: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(described["status"], "cancelled");
    let result = &described["result"];
    assert_eq!(result["review"]["outcome"], "stopped", "{described}");
    assert_eq!(result["review"]["reason"], "cancelled");
    assert_eq!(result["review"]["round"], 0);
    assert_eq!(result["landing"]["status"], "not_requested");
    let journal = result["review"]["journal"].as_str().unwrap();
    assert!(journal.starts_with("rev-"), "{journal}");
    assert!(result["error"].as_str().unwrap().contains("cancelled"));
    let change = session.jobs.take_changes("cancel").pop().unwrap();
    let outcome = change.outcome.unwrap();
    assert_eq!(serde_json::to_value(&outcome.review).unwrap(), {
        let mut review = result["review"].clone();
        review.as_object_mut().unwrap().remove("findings");
        review
    });
    let finished = session.journal().pop().unwrap();
    assert_eq!(finished["event"], "review.finished");
    assert_eq!(finished["outcome"], "stopped");
    assert_eq!(finished["status"], "cancelled");
}

#[tokio::test]
async fn a_refusal_in_the_repair_turn_is_kept_and_no_other_reviewer_is_asked() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Done.")]);
    session.script(
        "codex",
        [
            says("I cannot format that verdict."),
            ends("declined", "I decline this review.", None),
        ],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, result) = session.finished().await;
    assert_eq!(session.order(), ["claude", "codex", "codex"]);
    assert_eq!(result["review"]["reason"], "reviewer_declined");
    assert_eq!(result["review"]["refusals"][0]["agent"], "codex");
    assert_eq!(
        result["review"]["refusals"][0]["reply"],
        "I decline this review."
    );
    let tried = &outcome(&report).review.tried;
    assert_eq!(tried[0].result, ReviewerResult::Declined);
    // A repair that times out says so too.
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("Done.")]);
    session.script("codex", [says("LGTM."), ends("timeout", "", None)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(
        outcome(&report).review.reason.as_deref(),
        Some("reviewer_timeout")
    );
}

#[tokio::test]
async fn the_report_quotes_what_a_declining_reviewer_said() {
    let mut session = fixture(&["claude", "codex", "grok"]);
    session.script("claude", [says("Done.")]);
    session.script(
        "codex",
        [ends(
            "declined",
            "I decline because this requires access I cannot use.",
            None,
        )],
    );
    session.script("grok", [verdict(APPROVE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, result) = session.finished().await;
    assert_eq!(
        result["review"]["refusals"][0]["reply"],
        "I decline because this requires access I cannot use."
    );
    for text in [
        crate::delegate::background::report_prompt(std::slice::from_ref(&report)),
        crate::delegate::background::delivered_note(std::slice::from_ref(&report), "HTTP 503"),
    ] {
        assert!(
            text.contains(
                "reviewer codex declined, saying (untrusted): \"I decline because this requires \
                 access I cannot use.\""
            ),
            "{text}"
        );
    }
}

#[tokio::test]
async fn a_confirmation_run_that_failed_never_counts_even_with_a_negative_block() {
    let mut session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        [says("Committed."), says(block("scv-landing", LANDED))],
    );
    session.script(
        "codex",
        [
            verdict(APPROVE_RANGE),
            ends(
                "failed",
                block(
                    "scv-landing-check",
                    r#"{"status":"mismatch","ref":"origin/main"}"#,
                ),
                Some("HTTP 500"),
            ),
        ],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    let landing = &outcome(&report).landing;
    assert_eq!(landing.evidence, Some(LandingEvidence::Unconfirmed));
    assert_eq!(landing.reason.as_deref(), Some("the confirmation failed"));
    // The journal keeps what the block said.
    let check = session
        .journal()
        .into_iter()
        .find(|event| event["event"] == "landing_check")
        .unwrap();
    assert_eq!(check["check"]["status"], "mismatch");
    assert_eq!(check["evidence"], "unconfirmed");
}

#[tokio::test]
async fn every_open_finding_is_settled_across_rounds_however_long_their_details() {
    let mut session = fixture(&["claude", "codex"]);
    session.script("claude", [says("One."), says("Two."), says("Three.")]);
    let findings = |round: u32| -> Vec<Value> {
        (1..=10)
            .map(|n| {
                json!({"severity":"blocking","title":format!("Defect {round}.{n}"),
                       "detail":"x".repeat(1300)})
            })
            .collect()
    };
    let round1 = json!({"verdict":"changes","summary":"Ten defects.","findings":findings(1)});
    let prior: Vec<Value> = (1..=10)
        .map(|n| json!({"id":format!("1.{n}"),"status":"open"}))
        .collect();
    let round2 =
        json!({"verdict":"changes","summary":"Ten more.","prior":prior,"findings":findings(2)});
    let all: Vec<Value> = (1..=2)
        .flat_map(|round| {
            (1..=10).map(move |n| json!({"id":format!("{round}.{n}"),"status":"resolved"}))
        })
        .collect();
    let round3 = json!({"verdict":"approve","summary":"All fixed.","prior":all,
                        "evidence":["ran the tests"]});
    session.script(
        "codex",
        [
            verdict(&round1.to_string()),
            verdict(&round2.to_string()),
            verdict(&round3.to_string()),
        ],
    );
    session
        .start(json!({"prompt":"Fix it.","review":{}}))
        .await
        .unwrap();
    let (report, _) = session.finished().await;
    assert_eq!(outcome(&report).review.outcome, ReviewOutcome::Approved);
    let calls = session.calls();
    let reviewer = &calls[5].prompt;
    let fix = &calls[4].prompt;
    for round in 1..=2 {
        for n in 1..=10 {
            let id = format!("- {round}.{n} [blocking]");
            assert!(reviewer.contains(&id), "{id} missing for the reviewer");
            assert!(fix.contains(&id), "{id} missing for the builder");
        }
    }
}

/// A reviewed job of `call` on `session`'s agents, started in its job store
/// with its journal failing from event `fail_from` on: queued behind a turn
/// that never ends, or running.
fn start_with_failing_journal(
    session: &Fixture,
    call: Value,
    fail_from: Option<u64>,
    queued: bool,
) -> Arc<ReviewedTool> {
    let agents = Arc::clone(&session.tool.inner);
    let (builder, routed) = agents.route(&call).unwrap();
    let plan = plan(&agents, builder, &routed, &json!({}), 8).unwrap();
    let lanes: Arc<Lanes> = Arc::default();
    let reviewed = Arc::new(
        ReviewedTool::new(
            Arc::clone(&agents),
            Arc::clone(&lanes),
            Arc::clone(&session.conversations),
            plan,
            call.clone(),
            None,
            &session.home.path().join("reviews"),
        )
        .unwrap(),
    );
    lock(&reviewed.record.journal).fail_from = fail_from;
    let turn = if queued {
        // A turn ahead of it that never ends.
        std::mem::forget(lanes.join("claude-9"));
        crate::delegate::background::Turn::Queued(lanes.join("claude-9"))
    } else {
        crate::delegate::background::Turn::New
    };
    let tool: Arc<dyn Tool> = reviewed.clone();
    session
        .jobs
        .start(
            tool,
            "agent",
            "claude",
            call,
            &context(),
            turn,
            Some(&reviewed),
        )
        .unwrap();
    reviewed
}

/// Cancel job-1 for call `cancel`, and send its change as the server does:
/// the change and the job as the call returns it.
async fn cancel_job(session: &Fixture) -> (scv_protocol::JobOutcome, Value) {
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    let output = cancel
        .execute(json!({"job":"job-1"}), cancelling)
        .await
        .unwrap();
    let change = session.jobs.take_changes("cancel").pop().unwrap();
    session.jobs.published("cancel", true);
    (
        change.outcome.unwrap(),
        serde_json::from_str(&output.content).unwrap(),
    )
}

#[tokio::test]
async fn a_journal_failure_while_a_cancelled_job_stops_reaches_the_cancels_lines() {
    for queued in [false, true] {
        let session = fixture(&["claude", "codex"]);
        session.script("claude", [Step::Hang]);
        // Running: 3 is the cancelled builder turn's builder.finished.
        // Queued: 2 is review.finished, the loop never having run.
        let fail_from = if queued { 2 } else { 3 };
        let _reviewed = start_with_failing_journal(
            &session,
            json!({"prompt":"Fix it."}),
            Some(fail_from),
            queued,
        );
        if !queued {
            session.until_called("claude", 1).await;
        }
        let (outcome, described) = cancel_job(&session).await;
        let result = &described["result"];
        assert_eq!(result["review"]["journal_incomplete"], true, "{described}");
        assert!(outcome.review.journal_incomplete, "queued {queued}");
        assert_eq!(outcome.review.outcome, ReviewOutcome::Stopped);
        // The cancel's change, which clients show, matches the result.
        let mut review = result["review"].clone();
        review.as_object_mut().unwrap().remove("findings");
        review.as_object_mut().unwrap().remove("builder_session");
        assert_eq!(serde_json::to_value(&outcome.review).unwrap(), review);
        assert!(
            scv_protocol::outcome_notice(&outcome)
                .lines()
                .next()
                .unwrap()
                .ends_with("INCOMPLETE: a write failed"),
            "{}",
            scv_protocol::outcome_notice(&outcome)
        );
    }
}

/// The journal's events, by name.
fn event_names(session: &Fixture) -> Vec<String> {
    session
        .journal()
        .iter()
        .map(|event| event["event"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_job_still_stopping_after_the_cancel_wait_keeps_its_journal_and_reports_later() {
    for fail in [None, Some(3)] {
        let mut session = fixture(&["claude", "codex"]);
        let release = Arc::new(tokio::sync::Notify::new());
        session.script("claude", [Step::Stuck(Arc::clone(&release))]);
        let _reviewed =
            start_with_failing_journal(&session, json!({"prompt":"Fix it."}), fail, false);
        session.until_called("claude", 1).await;
        // The agent ignores the cancel past the 10-second wait: the cancel
        // states the decided outcome and that the journal is still open.
        let (outcome, described) = cancel_job(&session).await;
        assert_eq!(described["status"], "running", "{described}");
        assert_eq!(outcome.review.outcome, ReviewOutcome::Stopped);
        assert!(outcome.review.journal_pending);
        assert!(!outcome.review.journal_incomplete);
        assert!(
            scv_protocol::outcome_notice(&outcome).contains("still open"),
            "{}",
            scv_protocol::outcome_notice(&outcome)
        );
        assert_eq!(event_names(&session), ["review.started", "builder.started"]);
        assert!(session.jobs.take_updates().is_empty());
        // The job stops later and journals what it did; the session then
        // gets its final outcome, once.
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(60), session.finished.recv())
            .await
            .unwrap();
        let updates = session.jobs.take_updates();
        assert_eq!(updates.len(), 1);
        let update = &updates[0];
        assert_eq!(update.job, "job-1");
        assert_eq!(update.review.outcome, ReviewOutcome::Stopped);
        assert!(!update.review.journal_pending);
        assert_eq!(update.review.journal_incomplete, fail.is_some());
        if fail.is_some() {
            assert!(scv_protocol::outcome_notice(update).contains("INCOMPLETE"));
        } else {
            assert_eq!(
                event_names(&session),
                [
                    "review.started",
                    "builder.started",
                    "builder.finished",
                    "review.finished"
                ]
            );
        }
        // The result says the same; nothing else follows.
        let status = StatusTool {
            jobs: Arc::clone(&session.jobs),
        };
        let mut looking = context();
        looking.call_id = "status".into();
        let described = status
            .execute(json!({"job":"job-1"}), looking)
            .await
            .unwrap();
        let described: Value = serde_json::from_str(&described.content).unwrap();
        assert_eq!(described["status"], "cancelled");
        let mut review = described["result"]["review"].clone();
        review.as_object_mut().unwrap().remove("findings");
        review.as_object_mut().unwrap().remove("builder_session");
        assert_eq!(serde_json::to_value(&update.review).unwrap(), review);
        assert!(session.jobs.take_changes("status").is_empty());
        assert!(session.jobs.take_unreported().is_empty());
        assert!(session.jobs.take_updates().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn what_a_landing_turn_reports_after_the_cancel_is_still_journaled() {
    let mut session = fixture(&["claude", "codex"]);
    let release = Arc::new(tokio::sync::Notify::new());
    session.script(
        "claude",
        [
            says("Committed."),
            Step::StuckReply(Arc::clone(&release), block("scv-landing", LANDED)),
        ],
    );
    session.script("codex", [verdict(APPROVE_RANGE)]);
    session
        .start(json!({"prompt":"Fix it.","review":{"land":"after_approval"}}))
        .await
        .unwrap();
    session.until_called("claude", 2).await;
    let (outcome, _) = cancel_job(&session).await;
    // The earned approval stands; the landing is conservatively unknown.
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert!(outcome.review.journal_pending);
    assert_eq!(outcome.landing.status, LandingStatus::Unknown);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(60), session.finished.recv())
        .await
        .unwrap();
    let events = session.journal();
    let landing = events
        .iter()
        .find(|event| event["event"] == "landing")
        .expect("the late landing report is journaled");
    assert_eq!(landing["report"]["commits"][0], "4f2a9c1e0b7d");
    assert_eq!(events.last().unwrap()["event"], "review.finished");
    // The decision does not move, and the update says the journal is whole.
    let update = session.jobs.take_updates().pop().unwrap();
    assert_eq!(update.review.outcome, ReviewOutcome::Approved);
    assert_eq!(update.landing.status, LandingStatus::Unknown);
    assert!(!update.review.journal_incomplete && !update.review.journal_pending);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_never_waits_on_the_journal() {
    let mut session = fixture(&["claude", "codex"]);
    let release = Arc::new(tokio::sync::Notify::new());
    session.script("claude", [Step::Stuck(Arc::clone(&release))]);
    let reviewed = start_with_failing_journal(&session, json!({"prompt":"Fix it."}), None, false);
    session.until_called("claude", 1).await;
    // Another thread holds the journal, as a write stuck on a slow disk
    // does, for far longer than the cancel waits.
    let record = Arc::clone(&reviewed.record);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (unblock_tx, unblock_rx) = std::sync::mpsc::channel::<()>();
    let stalled = std::thread::spawn(move || {
        let _journal = lock(&record.journal);
        ready_tx.send(()).unwrap();
        let _ = unblock_rx.recv_timeout(Duration::from_secs(30));
    });
    ready_rx.recv().unwrap();
    let began = std::time::Instant::now();
    let (outcome, _) = cancel_job(&session).await;
    // The 10-second wait passed in paused time; the journal never held it.
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(outcome.review.journal_pending);
    unblock_tx.send(()).unwrap();
    stalled.join().unwrap();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(60), session.finished.recv())
        .await
        .unwrap();
    assert_eq!(session.jobs.take_updates().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_dropped_mid_wait_still_leaves_the_final_outcome_to_the_session() {
    let mut session = fixture(&["claude", "codex"]);
    let release = Arc::new(tokio::sync::Notify::new());
    session.script("claude", [Step::Stuck(Arc::clone(&release))]);
    let _reviewed = start_with_failing_journal(&session, json!({"prompt":"Fix it."}), None, false);
    session.until_called("claude", 1).await;
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    // The call's turn is aborted during its wait: its change never reaches
    // a client.
    let aborted = tokio::time::timeout(
        Duration::from_secs(1),
        cancel.execute(json!({"job":"job-1"}), cancelling),
    )
    .await;
    assert!(aborted.is_err());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(60), session.finished.recv())
        .await
        .unwrap();
    let update = session.jobs.take_updates().pop().unwrap();
    assert_eq!(update.review.outcome, ReviewOutcome::Stopped);
    assert!(!update.review.journal_pending);
}

#[tokio::test(start_paused = true)]
async fn a_final_outcome_whose_change_could_not_be_sent_goes_out_as_an_update() {
    let session = fixture(&["claude", "codex"]);
    let release = Arc::new(tokio::sync::Notify::new());
    session.script("claude", [Step::Stuck(Arc::clone(&release))]);
    let _reviewed =
        start_with_failing_journal(&session, json!({"prompt":"Fix it."}), Some(3), false);
    session.until_called("claude", 1).await;
    // The owner cancels the turn while agent_cancel waits; the job stops
    // within the wait, its journal failing.
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    let turn = cancelling.cancellation.clone();
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let (result, ()) = tokio::join!(cancel.execute(json!({"job":"job-1"}), cancelling), async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        turn.cancel();
        release.notify_one();
    });
    assert!(result.is_ok());
    let change = session.jobs.take_changes("cancel").pop().unwrap();
    assert!(change.outcome.unwrap().review.journal_incomplete);
    assert!(session.jobs.take_updates().is_empty(), "the change owes it");
    // Its turn being cancelled, the tool.completed carrying it fails.
    session.jobs.published("cancel", false);
    let updates = session.jobs.take_updates();
    assert_eq!(updates.len(), 1);
    assert!(updates[0].review.journal_incomplete && !updates[0].review.journal_pending);
    // An update that could not be sent stays owed; one sent is done.
    session.jobs.updated(&["job-1".to_owned()], false);
    assert_eq!(session.jobs.take_updates().len(), 1);
    session.jobs.updated(&["job-1".to_owned()], true);
    assert!(session.jobs.take_updates().is_empty());
}

#[tokio::test]
async fn pruning_finished_jobs_never_loses_a_cancel_or_what_it_owes() {
    let mut session = fixture(&["claude", "codex"]);
    session.script(
        "claude",
        (0..16)
            .map(|_| says("Earlier job done."))
            .chain([Step::Hang]),
    );
    // Sixteen finished jobs not yet reported fill the finished list.
    for _ in 0..16 {
        session
            .start(json!({"prompt":"Earlier job.","background":true}))
            .await
            .unwrap();
        session.finished.recv().await.unwrap();
    }
    let _reviewed =
        start_with_failing_journal(&session, json!({"prompt":"Review this."}), Some(3), false);
    session.until_called("claude", 17).await;
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    let output = cancel
        .execute(json!({"job":"job-17"}), cancelling)
        .await
        .expect("the cancel still describes its job");
    let described: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(described["status"], "cancelled");
    let change = session.jobs.take_changes("cancel").pop().unwrap();
    assert!(change.outcome.unwrap().review.journal_incomplete);
}

#[tokio::test(start_paused = true)]
async fn a_job_stopping_before_its_cancels_change_is_sent_has_the_change_say_the_final_outcome() {
    let mut session = fixture(&["claude", "codex"]);
    let release = Arc::new(tokio::sync::Notify::new());
    session.script("claude", [Step::Stuck(Arc::clone(&release))]);
    let _reviewed =
        start_with_failing_journal(&session, json!({"prompt":"Fix it."}), Some(3), false);
    session.until_called("claude", 1).await;
    let cancel = CancelTool {
        jobs: Arc::clone(&session.jobs),
    };
    let mut cancelling = context();
    cancelling.call_id = "cancel".into();
    cancel
        .execute(json!({"job":"job-1"}), cancelling)
        .await
        .unwrap();
    // The call returned with the journal still open; the job stops before
    // its tool.completed is sent.
    release.notify_one();
    session.finished.recv().await.unwrap();
    assert!(
        session.jobs.take_updates().is_empty(),
        "never ahead of the change"
    );
    let change = session.jobs.take_changes("cancel").pop().unwrap();
    let outcome = change.outcome.unwrap();
    assert!(
        !outcome.review.journal_pending,
        "superseded by the final outcome"
    );
    assert!(outcome.review.journal_incomplete);
    session.jobs.published("cancel", true);
    assert!(session.jobs.take_updates().is_empty(), "delivered once");
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_job_stays_pending_until_its_final_outcome_is_sent() {
    let mut session = fixture(&["claude", "codex"]);
    let release = Arc::new(tokio::sync::Notify::new());
    session.script("claude", [Step::Stuck(Arc::clone(&release))]);
    let _reviewed =
        start_with_failing_journal(&session, json!({"prompt":"Fix it."}), Some(3), false);
    session.until_called("claude", 1).await;
    let (pending, _) = cancel_job(&session).await;
    assert!(pending.review.journal_pending);
    // Still stopping, and owed: one job, counted once.
    assert_eq!((session.jobs.running(), session.jobs.pending()), (1, 1));
    release.notify_one();
    session.finished.recv().await.unwrap();
    // Stopped, its final outcome not yet sent: still pending.
    assert_eq!((session.jobs.running(), session.jobs.pending()), (0, 1));
    let updates = session.jobs.take_updates();
    assert!(updates[0].review.journal_incomplete);
    assert_eq!(session.jobs.pending(), 1, "being sent");
    session.jobs.updated(&["job-1".to_owned()], false);
    assert_eq!(session.jobs.pending(), 1, "the send failed");
    session.jobs.take_updates();
    session.jobs.updated(&["job-1".to_owned()], true);
    assert_eq!(session.jobs.pending(), 0);
}
