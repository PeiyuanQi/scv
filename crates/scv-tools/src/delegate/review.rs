//! Reviewed jobs: an `agent` call with `review` runs as one background job
//! in which a builder does the work and a fresh reviewer on another agent
//! checks it, round by round, on a structured verdict.
//!
//! A round is one builder turn, in one conversation the job holds for its
//! whole life, then one verdict on it from a reviewer conversation started
//! for that round. SCV code alone decides from the parsed verdict, the round
//! counter, turn statuses, and cancellation whether to go on: only `approve`
//! approves, the loop never runs past the call's round limit, and the last
//! round's builder turn always gets a verdict. With `land: after_approval`
//! the builder lands in one more turn after approval, and the approving
//! reviewer then checks the landed commits in one confirmation turn, which
//! is not a round. The job records every step in its journal and ends with
//! the builder's last result plus SCV's own `review` and `landing` fields.
//!
//! The reviewer is routed: Claude's work goes to Codex, Codex's to Claude,
//! any other agent's to Claude, then Codex; Grok steps in only when those
//! are unavailable, and a fresh conversation of the builder's own agent is
//! the last resort. Only the availability classification of the result moves
//! to the next agent. A refusal hands the review to Grok for the rest of the
//! job, never to the builder's own agent. A named reviewer is never swapped.

mod blocks;
mod journal;
mod prompts;
mod state;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolFailure, ToolOutput, ToolRisk, ToolSpec};
use scv_protocol::{
    JobStatus, LandMode, LandingStatus, OpenFinding, ReviewOutcome, ReviewerResult,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

pub(crate) use self::state::{Live, land_name};
use self::{
    blocks::{Decision, Expect, ReportStatus, Settlement, Severity, Verdict, clean},
    journal::Journal,
    prompts::{Finding, ReviewerBrief},
    state::{Phase, TurnKind},
};
use crate::{
    args::{bounded, parse_args},
    delegate::{
        agent::{AgentTool, Offered},
        background::lane::{Lanes, Place},
        conversation::{ALL_IN_USE, ConversationStore, PinGuard},
        request::{AgentArgs, blank_as_none},
    },
    sync::lock,
};

/// The default round limit.
const DEFAULT_ROUNDS: u32 = 3;
/// The highest round limit a call may set.
const MAX_ROUNDS: u32 = 20;
/// The longest `focus`, in bytes.
const MAX_FOCUS_BYTES: usize = 2 * 1024;
/// A repair turn's longest timeout.
const REPAIR_TIMEOUT: Duration = Duration::from_secs(300);
/// Bytes of a builder reply the journal keeps.
const JOURNAL_BUILDER_BYTES: usize = 8 * 1024;
/// Bytes of a reviewer reply the journal keeps.
const JOURNAL_REVIEWER_BYTES: usize = 16 * 1024;
/// Bytes of the builder's prompt the journal keeps.
const JOURNAL_PROMPT_BYTES: usize = 8 * 1024;
/// Characters of a refusal kept for the result.
const REFUSAL_CHARS: usize = 500;
/// Characters of a verdict summary kept for the result.
const SUMMARY_CHARS: usize = 300;
/// What the approval and the up-front checks show for a reviewer's prompt,
/// which SCV writes each round.
const PLACEHOLDER_PROMPT: &str =
    "[SCV review prompt: the reviewer's rules, the builder's brief, and its latest reply]";
/// What replaces a result's fallback or refusal note when an approved
/// change did not land.
const NOT_LANDED_NOTE: &str = "The review approved this work, but it did not land. Tell the \
     user; do not land it through another agent unless they ask.";

/// The `review` object of an `agent` call.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewArgs {
    #[serde(default, deserialize_with = "blank_as_none")]
    agent: Option<String>,
    rounds: Option<u32>,
    #[serde(default, deserialize_with = "blank_as_none")]
    land: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    focus: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    model: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    effort: Option<String>,
    timeout_seconds: Option<u64>,
}

/// A reviewed call, checked before approval: who builds, who may review in
/// which order, and the call's limits.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// The builder's agent.
    pub(crate) builder: String,
    /// The reviewer order, agents the session does not offer dropped.
    pub(crate) reviewers: Vec<Reviewer>,
    /// The call named its reviewer: it is never swapped.
    pub(crate) named: bool,
    pub(crate) rounds: u32,
    pub(crate) land: LandMode,
    pub(crate) focus: Option<String>,
    /// Each reviewer turn's timeout.
    pub(crate) timeout: Duration,
    /// The builder's `cwd`, where every reviewer runs.
    pub(crate) cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Reviewer {
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

impl Plan {
    /// The arguments of a reviewer turn: a new conversation with `prompt`,
    /// or the next turn of `session`.
    fn reviewer_call(
        &self,
        reviewer: &Reviewer,
        prompt: &str,
        session: Option<&str>,
        timeout: Duration,
    ) -> Value {
        let mut call = Map::new();
        match session {
            Some(session) => call.insert("session".into(), session.into()),
            None => call.insert("agent".into(), reviewer.agent.clone().into()),
        };
        call.insert("prompt".into(), prompt.into());
        call.insert("timeout_seconds".into(), timeout.as_secs().max(1).into());
        if let Some(cwd) = &self.cwd {
            call.insert("cwd".into(), cwd.clone().into());
        }
        if let Some(model) = &reviewer.model {
            call.insert("model".into(), model.clone().into());
        }
        if let Some(effort) = &reviewer.effort {
            call.insert("effort".into(), effort.clone().into());
        }
        Value::Object(call)
    }

    /// The order's agents, as the start result and journal name them.
    pub(crate) fn reviewer_names(&self) -> Vec<&str> {
        self.reviewers
            .iter()
            .map(|reviewer| reviewer.agent.as_str())
            .collect()
    }
}

/// The default reviewer order for work by `builder`.
fn default_order(builder: &str) -> Vec<&str> {
    let mut order = match builder {
        "claude" => vec!["codex", "grok"],
        "codex" => vec!["claude", "grok"],
        _ => vec!["claude", "codex", "grok"],
    };
    // A fresh conversation of the builder's own agent is the last resort.
    if !order.contains(&builder) {
        order.push(builder);
    }
    order
}

/// Check a reviewed call before approval: `builder` was routed for the
/// call, whose routed arguments are `call`, and `review` is its `review`
/// object. Every reviewer the order may launch is routed like an ordinary
/// call, so nothing launches that was not checked and approved.
pub(crate) fn plan(
    agents: &AgentTool,
    builder: &Offered,
    call: &Value,
    review: &Value,
    max_conversations: usize,
) -> Result<Plan, ToolError> {
    let args: ReviewArgs = parse_args(review)
        .map_err(|error| ToolError::invalid_arguments(format!("review: {}", error.message)))?;
    let call_args: AgentArgs = parse_args(call)?;
    if !builder.accepts.session {
        let continuing = agents.continuing();
        return Err(ToolError::invalid_arguments(if continuing.is_empty() {
            format!(
                "{} cannot continue a conversation, which a reviewed call needs for its fix \
                 rounds, and no agent in this session can; omit review",
                builder.name
            )
        } else {
            format!(
                "{} cannot continue a conversation, which a reviewed call needs for its fix \
                 rounds; these agents can: {}. Call one of them, or omit review",
                builder.name,
                continuing.join(", ")
            )
        }));
    }
    let rounds = args.rounds.unwrap_or(DEFAULT_ROUNDS);
    if !(1..=MAX_ROUNDS).contains(&rounds) {
        return Err(ToolError::invalid_arguments(format!(
            "review.rounds must be from 1 to {MAX_ROUNDS}"
        )));
    }
    let land = match args.land.as_deref() {
        None => LandMode::NoLanding,
        Some("after_approval") => LandMode::AfterApproval,
        Some("before_review") => LandMode::BeforeReview,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "review.land {:?} is not after_approval or before_review; omit it when the job \
                 must not land",
                bounded(other, 40)
            )));
        }
    };
    if let Some(focus) = &args.focus
        && (focus.len() > MAX_FOCUS_BYTES
            || focus
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'))
    {
        return Err(ToolError::invalid_arguments(format!(
            "review.focus must be at most {} KiB of text without control characters",
            MAX_FOCUS_BYTES / 1024
        )));
    }
    if args.agent.is_none() && (args.model.is_some() || args.effort.is_some()) {
        let option = if args.model.is_some() {
            "model"
        } else {
            "effort"
        };
        return Err(ToolError::invalid_arguments(format!(
            "review.{option} needs review.agent: a routed reviewer runs with its own defaults"
        )));
    }
    if max_conversations < 2 {
        return Err(ToolError::invalid_arguments(
            "a reviewed call needs room for two conversations, the builder and a reviewer \
             (agent.max_conversations is below 2)",
        ));
    }
    let timeout = agents.timeouts().resolve(args.timeout_seconds)?;
    let named = args.agent.is_some();
    let order: Vec<Reviewer> = match &args.agent {
        Some(agent) => vec![Reviewer {
            agent: agent.clone(),
            model: args.model.clone(),
            effort: args.effort.clone(),
        }],
        None => default_order(&builder.name)
            .into_iter()
            .filter(|agent| agents.find(agent).is_some())
            .map(|agent| Reviewer {
                agent: agent.to_owned(),
                model: None,
                effort: None,
            })
            .collect(),
    };
    let plan = Plan {
        builder: builder.name.clone(),
        reviewers: order,
        named,
        rounds,
        land,
        focus: args.focus,
        timeout,
        cwd: call_args.cwd,
    };
    for reviewer in &plan.reviewers {
        let routed = plan.reviewer_call(reviewer, PLACEHOLDER_PROMPT, None, timeout);
        let (agent, routed) = agents
            .route(&routed)
            .map_err(|error| ToolError::invalid_arguments(format!("review: {}", error.message)))?;
        agent.backend.risk(&routed).map_err(|error| ToolError {
            kind: error.kind,
            message: format!("review: {}", error.message),
        })?;
    }
    if plan.reviewers.is_empty() {
        return Err(ToolError::invalid_arguments(
            "no reviewer is offered in this session",
        ));
    }
    Ok(plan)
}

/// What the user approves for a reviewed call beyond its builder's launch:
/// the rounds, the landing mode, and every reviewer launch the order may
/// make.
pub(crate) fn approval_summary(plan: &Plan, agents: &AgentTool) -> Result<String, ToolError> {
    let mut text = format!("\nReviewed: up to {} rounds", plan.rounds);
    text.push_str(match plan.land {
        LandMode::AfterApproval => {
            ", then LANDS AFTER APPROVAL in one more builder turn, which the approving reviewer \
             then checks in one confirmation turn."
        }
        LandMode::BeforeReview => {
            ". LANDS BEFORE REVIEW: the builder may land each round's work before it is \
             reviewed."
        }
        _ => "; does not land.",
    });
    text.push_str("\nBuilder: this conversation, one turn per round.");
    text.push_str(if plan.named || plan.reviewers.len() == 1 {
        "\nReviewer each round:"
    } else {
        "\nReviewer each round, the first available of:"
    });
    for reviewer in &plan.reviewers {
        let call = plan.reviewer_call(reviewer, PLACEHOLDER_PROMPT, None, plan.timeout);
        let (agent, routed) = agents.route(&call)?;
        let same = if reviewer.agent == plan.builder {
            " (same agent as builder)"
        } else {
            ""
        };
        text.push_str(&format!(
            "\n  a fresh {} conversation{same}: {}",
            reviewer.agent,
            agent.backend.approval_summary(&routed)?
        ));
    }
    Ok(text)
}

/// The `review` property of the `agent` tool's schema.
pub(crate) fn schema(agents: &AgentTool) -> Value {
    json!({
        "type":"object",
        "description":"Have an independent reviewer check this coding task before it counts as \
            done. Set it only when the user asked for a review or accepted your suggestion of \
            one (the delegating skill says when to suggest it). The call then runs as a \
            background job: the agent builds, a fresh reviewer on another agent checks the \
            work and returns a structured verdict, and the agent fixes blocking findings, up \
            to rounds times. SCV reports the outcome in its own Review and Landing lines; only \
            an approved review is approved, and landed work is not approved work. Every field \
            is optional.",
        "properties":{
            "agent":{
                "type":"string",
                "enum":agents.names(),
                "description":"The reviewer, when the user named one; it is then never swapped. \
                    Omit it to let SCV choose: codex reviews claude's work, claude reviews \
                    everyone else's."
            },
            "rounds":{
                "type":"integer","minimum":1,"maximum":MAX_ROUNDS,
                "description":"Most rounds of build and review; default 3. Never raise it to \
                    get past an unresolved review: that is the user's call."
            },
            "land":{
                "type":"string","enum":["after_approval","before_review"],
                "description":"Omit it unless the user's request includes landing. \
                    after_approval: the agent lands only after the review approves. \
                    before_review: only when the user explicitly asked to land first and \
                    review after."
            },
            "focus":{"type":"string","description":"What the reviewer should look at in particular, briefly."},
            "model":{"type":"string","description":"The named reviewer's model, only with agent."},
            "effort":{"type":"string","description":"The named reviewer's effort, only with agent."},
            "timeout_seconds":{"type":"integer","minimum":1,"description":"Each reviewer turn's timeout."}
        },
        "additionalProperties":false
    })
}

/// A reviewed job's record, which its tool and its job share: the live
/// state, which the job reads for its changes and report, and the journal,
/// which only the job's own task writes, until the job actually ends.
/// Dropped without an ending, as when its job is cut off, it ends the
/// journal itself; a journal nothing was written to is removed.
pub(crate) struct Record {
    pub(crate) live: Live,
    journal: Mutex<Journal>,
}

impl Record {
    pub(crate) fn journal_id(&self) -> String {
        lock(&self.journal).id().to_owned()
    }

    /// Append one event; a failure marks the journal incomplete.
    fn append(&self, event: &str, data: Value) -> std::io::Result<()> {
        lock(&self.journal)
            .append(event, data)
            .inspect_err(|_| self.live.journal_failed())
    }

    /// End the journal with `review.finished` from the live state and the
    /// job's `status`, once. The review and landing as they then stand, a
    /// failed write included.
    pub(crate) fn end(
        &self,
        cancelled: bool,
        status: JobStatus,
    ) -> (scv_protocol::ReviewSummary, scv_protocol::LandingSummary) {
        let mut journal = lock(&self.journal);
        if journal.is_started() && !journal.is_finished() {
            let (review, landing) = self.live.snapshot(cancelled);
            let written = journal.append(
                "review.finished",
                json!({
                    "outcome": review.outcome,
                    "reason": review.reason,
                    "round": review.round,
                    "status": status.as_str(),
                    "journal_incomplete": review.journal_incomplete,
                    "landing": landing,
                }),
            );
            if written.is_err() {
                self.live.journal_failed();
            }
        }
        drop(journal);
        self.live.snapshot(cancelled)
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        let journal = self
            .journal
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !journal.is_started() {
            journal.remove_empty();
        } else if !journal.is_finished() {
            let (review, landing) = self.live.snapshot(true);
            let _ = journal.append(
                "review.finished",
                json!({
                    "outcome": review.outcome,
                    "reason": review.reason,
                    "round": review.round,
                    "status": JobStatus::Cancelled.as_str(),
                    "journal_incomplete": review.journal_incomplete,
                    "landing": landing,
                }),
            );
        }
    }
}

/// The `agent` tool for one reviewed job: it runs the whole loop as the
/// job's one call. Built per job, with the job's [`Record`].
pub(crate) struct ReviewedTool {
    agents: Arc<AgentTool>,
    lanes: Arc<Lanes>,
    conversations: Arc<ConversationStore>,
    plan: Plan,
    /// The builder's call, without `background` and `review`.
    call: Value,
    /// The call's prompt.
    prompt: String,
    /// The SCV session it runs in, for the journal.
    session: Option<String>,
    record: Arc<Record>,
}

impl ReviewedTool {
    /// Prepare the reviewed job for `call` under `plan`, creating its
    /// journal in `dir`. Nothing is written until [`Self::begin`].
    pub(in crate::delegate) fn new(
        agents: Arc<AgentTool>,
        lanes: Arc<Lanes>,
        conversations: Arc<ConversationStore>,
        plan: Plan,
        call: Value,
        session: Option<String>,
        dir: &std::path::Path,
    ) -> Result<Self, ToolError> {
        let journal = Journal::create(dir).map_err(|error| {
            ToolError::failed(format!(
                "could not create the review journal in {}: {error}",
                dir.display()
            ))
        })?;
        let live = Live::new(plan.rounds, plan.land, journal.id().to_owned());
        let prompt = parse_args::<AgentArgs>(&call)?.prompt;
        Ok(Self {
            agents,
            lanes,
            conversations,
            plan,
            call,
            prompt,
            session,
            record: Arc::new(Record {
                live,
                journal: Mutex::new(journal),
            }),
        })
    }

    pub(crate) fn record(&self) -> &Arc<Record> {
        &self.record
    }

    pub(crate) fn journal_id(&self) -> String {
        self.record.journal_id()
    }

    /// Record the review's start as job `job`. A journal that cannot be
    /// written refuses the call before its job starts.
    pub(crate) fn begin(&self, job: &str) -> Result<(), ToolError> {
        let args: AgentArgs = parse_args(&self.call)?;
        let mut builder = json!({"agent": self.plan.builder});
        for (key, value) in [
            ("session", &args.session),
            ("model", &args.model),
            ("effort", &args.effort),
        ] {
            if let Some(value) = value {
                builder[key] = value.clone().into();
            }
        }
        let reviewers: Vec<Value> = self
            .plan
            .reviewers
            .iter()
            .map(|reviewer| {
                let mut entry = json!({"agent": reviewer.agent});
                if let Some(model) = &reviewer.model {
                    entry["model"] = model.clone().into();
                }
                if let Some(effort) = &reviewer.effort {
                    entry["effort"] = effort.clone().into();
                }
                entry
            })
            .collect();
        let mut journal = lock(&self.record.journal);
        let id = journal.id().to_owned();
        journal
            .append(
                "review.started",
                json!({
                    "review": id,
                    "job": job,
                    "session": self.session,
                    "cwd": self.plan.cwd,
                    "builder": builder,
                    "reviewers": reviewers,
                    "named": self.plan.named,
                    "rounds": self.plan.rounds,
                    "land": state::land_name(self.plan.land),
                    "focus": self.plan.focus,
                    "prompt": cap(&args.prompt, JOURNAL_PROMPT_BYTES),
                }),
            )
            .map_err(|error| {
                ToolError::failed(format!("could not write the review journal: {error}"))
            })
    }

    /// Append one event. A failure halts the loop and is never hidden: the
    /// outcome says the journal is incomplete.
    fn write(&self, event: &str, data: Value) -> Result<(), Halt> {
        self.record.append(event, data).map_err(|_| Halt::Journal)
    }

    /// The result of a reviewed job whose loop never ran, because it was
    /// cancelled while queued (`error` says so) or its session ended first.
    /// Its journal ends here, like the result, stopped before round 1.
    pub(crate) fn unstarted(&self, error: &ToolError) -> ToolOutput {
        self.conclude(
            true,
            Ending {
                builder: None,
                error: Some(error.message.clone()),
                builder_session: None,
                findings: &[],
                not_landed: false,
            },
        )
    }

    /// End the job: write `review.finished` from the live state, then build
    /// the result: the builder's last result, or an empty one, with SCV's
    /// `review` and `landing`. A failed final write shows in both.
    fn conclude(&self, cancelled: bool, ending: Ending<'_>) -> ToolOutput {
        let status = if cancelled {
            JobStatus::Cancelled
        } else {
            ending
                .builder
                .as_ref()
                .map_or(JobStatus::Failed, |builder| builder.status)
        };
        let (review, landing) = self.record.end(cancelled, status);
        let (mut value, failure, truncated) = match ending.builder {
            Some(builder) => (
                builder.value,
                builder.output.failure,
                builder.output.truncated,
            ),
            None => (
                agent_result(&self.plan.builder, status, ending.error.as_deref()),
                Some(if cancelled {
                    ToolFailure::Cancelled
                } else {
                    ToolFailure::Failed
                }),
                false,
            ),
        };
        if ending.not_landed && review.outcome == ReviewOutcome::Approved {
            value.remove("fallback");
            value.insert("note".into(), NOT_LANDED_NOTE.into());
        }
        let mut review_value = serde_json::to_value(&review).unwrap_or_default();
        if let Some(object) = review_value.as_object_mut() {
            if let Some(handle) = ending.builder_session {
                object.insert("builder_session".into(), handle.into());
            }
            let findings: Vec<Value> = ending
                .findings
                .iter()
                .map(|finding| {
                    json!({
                        "id": finding.id,
                        "severity": finding.severity.as_str(),
                        "title": finding.title,
                        "location": finding.location,
                        "detail": finding.detail,
                    })
                })
                .collect();
            object.insert("findings".into(), findings.into());
        }
        let mut landing_value = serde_json::to_value(&landing).unwrap_or_default();
        let url = self
            .record
            .live
            .read(|state| state.landing.url().map(str::to_owned));
        if let (Some(object), Some(url)) = (landing_value.as_object_mut(), url) {
            object.insert("url".into(), url.into());
        }
        value.insert("review".into(), review_value);
        value.insert("landing".into(), landing_value);
        ToolOutput {
            content: Value::Object(value).to_string(),
            failure: if cancelled {
                Some(ToolFailure::Cancelled)
            } else {
                failure
            },
            truncated,
        }
    }
}

/// What the end of a reviewed job knows besides its live state.
struct Ending<'a> {
    /// The builder's latest turn.
    builder: Option<Turn>,
    /// Why no builder turn ran, for the result's `error`.
    error: Option<String>,
    builder_session: Option<String>,
    /// The last verdict's findings, numbered.
    findings: &'a [Finding],
    /// The approved change did not land.
    not_landed: bool,
}

#[async_trait]
impl Tool for ReviewedTool {
    fn spec(&self) -> ToolSpec {
        self.agents.spec()
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        self.agents.risk(arguments)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        self.agents.approval_summary(arguments)
    }

    async fn execute(
        &self,
        _arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let mut run = Run::new(self, context);
        let halt = run.rounds().await;
        Ok(run.finish(halt))
    }
}

/// Why the loop stopped early. Its outcome is already fixed in the live
/// state, or the job is cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Halt {
    /// The outcome is fixed, or the job was cancelled.
    Done,
    /// A journal write failed.
    Journal,
}

/// A conversation a reviewed job started for a reviewer, ended when
/// dropped, on every path.
struct Held {
    conversations: Arc<ConversationStore>,
    handle: String,
    pin: Option<PinGuard>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.pin = None;
        self.conversations.release(&self.handle);
    }
}

/// One delegated turn's result, read the same way for every role.
struct Turn {
    output: ToolOutput,
    /// The agent result's JSON object.
    value: Map<String, Value>,
    status: JobStatus,
    reply: String,
    error: Option<String>,
    session: Option<String>,
    truncated: bool,
    /// The availability classifier marked it unavailable.
    unavailable: bool,
    /// It could not start: every conversation slot is in use.
    no_slot: bool,
}

impl Turn {
    fn read(agent: &str, result: Result<ToolOutput, ToolError>) -> Self {
        let output = result.unwrap_or_else(ToolOutput::from);
        let value = if let Ok(Value::Object(value)) = serde_json::from_str(&output.content) {
            value
        } else {
            // SCV's own error, such as a refused start: no agent result.
            let status = match output.failure {
                None => JobStatus::Completed,
                Some(ToolFailure::Cancelled) => JobStatus::Cancelled,
                Some(_) => JobStatus::Failed,
            };
            let error = output.is_error().then_some(output.content.as_str());
            agent_result(agent, status, error)
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let status = value
            .get("status")
            .cloned()
            .and_then(|status| serde_json::from_value(status).ok())
            .unwrap_or(if output.is_error() {
                JobStatus::Failed
            } else {
                JobStatus::Completed
            });
        let no_slot = output.failure == Some(ToolFailure::Limit)
            && text("error").is_some_and(|error| error.contains(ALL_IN_USE));
        Self {
            status,
            reply: text("reply").unwrap_or_default(),
            error: text("error"),
            session: text("session"),
            truncated: value.get("truncated").and_then(Value::as_bool) == Some(true),
            unavailable: output.failure == Some(ToolFailure::Unavailable),
            no_slot,
            value,
            output,
        }
    }

    fn completed(&self) -> bool {
        self.status == JobStatus::Completed
    }

    /// `timed out`, `failed`, and so on, for SCV's words.
    fn ended(&self) -> Option<&'static str> {
        match self.status {
            JobStatus::Completed => None,
            JobStatus::Timeout => Some("timed out"),
            JobStatus::Declined => Some("was declined"),
            JobStatus::Cancelled => Some("was stopped"),
            _ => Some("failed"),
        }
    }
}

/// Why a round ended without a verdict, as its `no_verdict` reason.
fn status_reason(role: &str, status: JobStatus) -> String {
    let status = match status {
        JobStatus::Timeout => "timeout",
        JobStatus::Declined => "declined",
        JobStatus::Cancelled => "cancelled",
        _ => "failed",
    };
    format!("{role}_{status}")
}

/// Why an agent was skipped for the rest of the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    Unavailable,
    Declined,
}

/// One reviewed job's loop.
struct Run<'a> {
    tool: &'a ReviewedTool,
    context: ToolContext,
    token: CancellationToken,
    /// The builder's conversation, once known.
    handle: Option<String>,
    /// The lane place the loop took for a conversation it started.
    place: Option<Place>,
    pin: Option<PinGuard>,
    round: u32,
    /// Blocking findings still open.
    open: Vec<Finding>,
    /// The last verdict's findings, numbered.
    findings: Vec<Finding>,
    /// Agents skipped for the rest of the job, and why.
    skipped: HashMap<String, Skip>,
    /// A reviewer declined: only Grok may review from now on.
    refused: bool,
    /// The builder's latest turn.
    builder: Option<Turn>,
    /// The approving reviewer kept for the landing confirmation.
    kept: Option<(String, Held)>,
    /// The approved change did not land.
    not_landed: bool,
}

impl<'a> Run<'a> {
    fn new(tool: &'a ReviewedTool, context: ToolContext) -> Self {
        let handle = parse_args::<AgentArgs>(&tool.call)
            .ok()
            .and_then(|args| args.session);
        Self {
            tool,
            token: context.cancellation.clone(),
            context,
            handle,
            place: None,
            pin: None,
            round: 0,
            open: Vec::new(),
            findings: Vec::new(),
            skipped: HashMap::new(),
            refused: false,
            builder: None,
            kept: None,
            not_landed: false,
        }
    }

    fn plan(&self) -> &Plan {
        &self.tool.plan
    }

    fn live(&self) -> &Live {
        &self.tool.record.live
    }

    fn update(&self, change: impl FnOnce(&mut state::State)) {
        self.live().update(&self.token, change);
    }

    /// End the loop with `outcome`.
    fn stop(&self, outcome: ReviewOutcome, reason: Option<&str>) -> Halt {
        self.live().fix(&self.token, outcome, reason);
        Halt::Done
    }

    /// Halt if the job was cancelled.
    fn check(&self) -> Result<(), Halt> {
        if self.token.is_cancelled() {
            Err(Halt::Done)
        } else {
            Ok(())
        }
    }

    fn phase(&self, phase: Phase, line: &str) {
        self.update(|state| state.phase = phase);
        self.context.progress.report(line);
    }

    async fn rounds(&mut self) -> Halt {
        match self.run_rounds().await {
            Ok(()) => Halt::Done,
            Err(halt) => halt,
        }
    }

    async fn run_rounds(&mut self) -> Result<(), Halt> {
        let rounds = self.plan().rounds;
        if let Some(handle) = self.handle.clone() {
            // The job's lane place for a conversation it continues was
            // taken when the call arrived and is held to the end.
            self.pin = self.tool.conversations.pin(&handle);
            self.update(|state| state.builder_session = Some(handle));
        }
        let mut prompt = format!(
            "{}{}",
            self.tool.prompt,
            prompts::builder_notice(self.plan().land, rounds)
        );
        for round in 1..=rounds {
            self.round = round;
            self.update(|state| state.round = round);
            self.builder_turn(TurnKind::Round, prompt).await?;
            let (status, session, reply) = self
                .builder
                .as_ref()
                .map_or((JobStatus::Failed, None, String::new()), |turn| {
                    (turn.status, turn.session.clone(), turn.reply.clone())
                });
            if status != JobStatus::Completed {
                let reason = status_reason("builder", status);
                return Err(self.stop(ReviewOutcome::Stopped, Some(&reason)));
            }
            if round == 1 && self.handle.is_none() {
                let Some(handle) = session else {
                    return Err(self.stop(ReviewOutcome::Stopped, Some("builder_no_session")));
                };
                // Held before the handle is shown anywhere, so a later call
                // to the conversation queues behind the whole job.
                self.place = Some(self.tool.lanes.join(&handle));
                self.pin = self.tool.conversations.pin(&handle);
                if self.pin.is_none() {
                    return Err(self.stop(ReviewOutcome::Stopped, Some("builder_no_session")));
                }
                self.handle = Some(handle.clone());
                self.update(|state| state.builder_session = Some(handle));
            }
            let (verdict, reviewer, held) = self.review(round, &reply).await?;
            self.settle(round, &verdict);
            match verdict.decision {
                Decision::Approve => {
                    self.stop(ReviewOutcome::Approved, None);
                    if self.plan().land == LandMode::AfterApproval {
                        self.land(round, &verdict, reviewer, held).await?;
                    }
                    return Ok(());
                }
                Decision::Escalate => return Err(self.stop(ReviewOutcome::Escalated, None)),
                Decision::Changes if round == rounds => {
                    return Err(self.stop(ReviewOutcome::Unresolved, Some("round_limit")));
                }
                Decision::Changes => {
                    let mut findings = self.open.clone();
                    findings.extend(
                        self.findings
                            .iter()
                            .filter(|finding| finding.severity == Severity::Minor)
                            .cloned(),
                    );
                    let landed = self.live().read(|state| state.landing.landed_so_far());
                    prompt = prompts::fix_prompt(
                        round + 1,
                        rounds,
                        self.plan().land,
                        &findings,
                        landed
                            .as_ref()
                            .map(|(reference, commits)| (reference.as_str(), commits.as_slice())),
                    );
                }
            }
        }
        Ok(())
    }

    /// Number the verdict's findings and work out which blocking ones stay
    /// open.
    fn settle(&mut self, round: u32, verdict: &Verdict) {
        let still_open: Vec<Finding> =
            self.open
                .iter()
                .filter(|finding| {
                    verdict.prior.iter().any(|settled| {
                        settled.id == finding.id && settled.status == Settlement::Open
                    })
                })
                .cloned()
                .collect();
        self.findings = verdict
            .findings
            .iter()
            .enumerate()
            .map(|(index, finding)| Finding {
                id: format!("{round}.{}", index + 1),
                severity: finding.severity,
                title: finding.title.clone(),
                detail: finding.detail.clone(),
                location: finding.location.clone(),
            })
            .collect();
        self.open = still_open;
        self.open.extend(
            self.findings
                .iter()
                .filter(|finding| finding.severity == Severity::Blocking)
                .cloned(),
        );
        let open: Vec<OpenFinding> = self
            .open
            .iter()
            .map(|finding| OpenFinding {
                id: finding.id.clone(),
                title: finding.title.clone(),
                location: finding.location.clone(),
            })
            .collect();
        let summary = blocks::cut(&verdict.summary, SUMMARY_CHARS);
        self.update(|state| {
            state.open = open;
            state.summary = Some(summary);
        });
    }

    /// Run one builder turn of `kind` with `prompt` and record it, its
    /// landing report included. The turn is kept as the builder's latest
    /// (`self.builder`) before anything that can fail, so the result never
    /// shows an older one.
    async fn builder_turn(&mut self, kind: TurnKind, prompt: String) -> Result<(), Halt> {
        let round = self.round;
        let (phase, line) = match kind {
            TurnKind::Round => (
                Phase::Builder,
                format!(
                    "review round {round} of {}: {} works",
                    self.plan().rounds,
                    self.plan().builder
                ),
            ),
            TurnKind::Landing => (
                Phase::Landing,
                format!("review approved: {} lands the work", self.plan().builder),
            ),
        };
        self.update(|state| state.landing.turn_started(kind));
        self.phase(phase, &line);
        self.tool.write(
            "builder.started",
            json!({"round": round, "kind": kind.as_str()}),
        )?;
        if let Some(place) = &self.place
            && place.wait_first(&self.token).await.is_err()
        {
            return Err(Halt::Done);
        }
        let mut call = self.tool.call.clone();
        if let Some(object) = call.as_object_mut() {
            object.insert("prompt".into(), prompt.into());
            if let Some(handle) = &self.handle {
                object.insert("session".into(), handle.clone().into());
            }
        }
        let result = self
            .tool
            .agents
            .execute_when_idle(call, self.context.clone())
            .await;
        let turn = Turn::read(&self.plan().builder, result);
        // Recorded before anything else, journal included, so a turn that
        // landed and then failed, or whose journal write fails, still
        // counts as landed.
        let report = blocks::parse_landing(&turn.reply, turn.truncated);
        let ended = turn.ended();
        let may_land = self.live().read(|state| state.landing.may_land(kind));
        self.update(|state| {
            state.landing.record(kind, round, ended, &report);
        });
        let finished = json!({
            "round": round,
            "kind": kind.as_str(),
            "session": turn.session,
            "turn": turn.value.get("turn"),
            "status": turn.status.as_str(),
            "error": turn.error.as_deref().map(|error| cap(error, JOURNAL_BUILDER_BYTES)),
            "reply": cap(&turn.reply, JOURNAL_BUILDER_BYTES),
            "truncated": turn.truncated,
        });
        self.builder = Some(turn);
        self.tool.write("builder.finished", finished)?;
        if may_land || !matches!(report, Ok(None)) {
            let mut event = json!({"round": round, "kind": kind.as_str(), "authorized": may_land});
            match &report {
                Ok(Some(report)) => {
                    event["report"] = json!({
                        "status": report.status.as_str(),
                        "ref": report.reference,
                        "commits": report.commits,
                        "url": report.url,
                        "detail": report.detail,
                    });
                    event["authorized"] =
                        (may_land || report.status == ReportStatus::NotLanded).into();
                }
                Ok(None) => event["error"] = "no scv-landing block".into(),
                Err(error) => event["error"] = error.clone().into(),
            }
            self.tool.write("landing", event)?;
        }
        self.check()?;
        Ok(())
    }

    /// Get round `round`'s verdict on the builder's `reply`: the verdict,
    /// the reviewer agent, and its conversation.
    async fn review(
        &mut self,
        round: u32,
        reply: &str,
    ) -> Result<(Verdict, String, Option<Held>), Halt> {
        let reviewers = self.plan().reviewers.clone();
        let builder = self.plan().builder.clone();
        let named = self.plan().named;
        let landed_report = self
            .live()
            .read(|state| state.landing.reported(TurnKind::Round, round));
        let open_ids: Vec<String> = self.open.iter().map(|finding| finding.id.clone()).collect();
        let expect = Expect {
            open: &open_ids,
            landed: landed_report.is_some(),
            after_approval: self.plan().land == LandMode::AfterApproval,
        };
        let brief = ReviewerBrief {
            land: self.plan().land,
            prompt: &self.tool.prompt,
            focus: self.plan().focus.as_deref(),
            reply,
            open: &self.open,
            landed: landed_report
                .as_ref()
                .map(|(reference, commits)| (reference.as_str(), commits.as_slice())),
        };
        let prompt = prompts::reviewer_prompt(&brief);
        let grok_offered = reviewers.iter().any(|reviewer| reviewer.agent == "grok");
        let mut last = None;
        for reviewer in &reviewers {
            let agent = reviewer.agent.as_str();
            if self.skipped.contains_key(agent) {
                continue;
            }
            // After a refusal only Grok may review, and never as the
            // builder's own agent.
            if self.refused && (agent != "grok" || agent == builder) {
                continue;
            }
            self.check()?;
            last = Some(agent.to_owned());
            let fallback = self.fallback(agent);
            self.update(|state| {
                state.reviewer = agent.to_owned();
                state.fallback.clone_from(&fallback);
                state.landing.checking(TurnKind::Round, round, Some(agent));
            });
            let line = match &fallback {
                Some(fallback) => format!("review round {round}: {agent} reviews ({fallback})"),
                None => format!("review round {round}: {agent} reviews"),
            };
            self.phase(Phase::Reviewer, &line);
            self.tool.write(
                "reviewer.started",
                json!({"round": round, "agent": agent, "kind": "review"}),
            )?;
            let call = self
                .plan()
                .reviewer_call(reviewer, &prompt, None, self.plan().timeout);
            let result = self
                .tool
                .agents
                .execute_when_idle(call, self.context.clone())
                .await;
            let turn = Turn::read(agent, result);
            let mut held = turn.session.clone().map(|handle| Held {
                conversations: Arc::clone(&self.tool.conversations),
                handle,
                pin: None,
            });
            self.record_reviewer(round, agent, "review", &turn)?;
            let parsed = blocks::parse_verdict(&turn.reply, turn.truncated, &expect);
            if let Ok(verdict) = &parsed {
                self.record_verdict(round, agent, &parsed)?;
                // A negative verdict counts however the run ended, so a
                // fallback can never get around it; approval needs a run
                // that completed.
                if verdict.decision != Decision::Approve || turn.completed() {
                    let verdict = verdict.clone();
                    self.reviewed(round, agent, &verdict);
                    return Ok((verdict, agent.to_owned(), held.take()));
                }
                self.update(|state| state.tried(agent, ReviewerResult::Failed));
                return Err(self.no_verdict(round, agent, &status_reason("reviewer", turn.status)));
            }
            self.check()?;
            if turn.no_slot {
                self.update(|state| state.tried(agent, ReviewerResult::NoSlot));
                return Err(self.no_verdict(round, agent, "no_conversation_slot"));
            }
            if turn.unavailable {
                self.update(|state| state.tried(agent, ReviewerResult::Unavailable));
                if named {
                    return Err(self.no_verdict(round, agent, "reviewer_unavailable"));
                }
                if self.refused {
                    return Err(self.no_verdict(round, agent, "reviewer_declined"));
                }
                self.skipped.insert(agent.to_owned(), Skip::Unavailable);
                continue;
            }
            match turn.status {
                JobStatus::Declined => {
                    let words = clean(&turn.reply, REFUSAL_CHARS);
                    self.update(|state| {
                        state.tried(agent, ReviewerResult::Declined);
                        state.refused(agent, words);
                    });
                    let grok_left = grok_offered
                        && agent != "grok"
                        && builder != "grok"
                        && !self.skipped.contains_key("grok");
                    if named || !grok_left {
                        return Err(self.no_verdict(round, agent, "reviewer_declined"));
                    }
                    self.refused = true;
                    self.skipped.insert(agent.to_owned(), Skip::Declined);
                }
                JobStatus::Completed => {
                    let error = parsed.err().unwrap_or_default();
                    self.record_verdict(round, agent, &Err(error.clone()))?;
                    let Some(handle) = held.as_ref().map(|held| held.handle.clone()) else {
                        self.update(|state| state.tried(agent, ReviewerResult::Failed));
                        return Err(self.no_verdict(round, agent, "malformed_verdict"));
                    };
                    match self
                        .repair(round, reviewer, &handle, &error, &expect)
                        .await?
                    {
                        Ok(verdict) => {
                            self.reviewed(round, agent, &verdict);
                            return Ok((verdict, agent.to_owned(), held.take()));
                        }
                        Err(reason) => return Err(self.no_verdict(round, agent, &reason)),
                    }
                }
                status => {
                    self.update(|state| state.tried(agent, ReviewerResult::Failed));
                    return Err(self.no_verdict(round, agent, &status_reason("reviewer", status)));
                }
            }
        }
        let reason = if self.refused {
            "reviewer_declined"
        } else {
            "no_reviewer_available"
        };
        let agent = last.unwrap_or_default();
        Err(self.no_verdict(round, &agent, reason))
    }

    /// A reviewer gave round `round`'s verdict.
    fn reviewed(&self, round: u32, agent: &str, verdict: &Verdict) {
        self.update(|state| {
            state.tried(agent, ReviewerResult::Verdict);
            if let Some(check) = &verdict.landing_check {
                state.landing.checked(TurnKind::Round, round, agent, check);
            }
        });
    }

    /// End round `round` without a verdict, for `reason`.
    fn no_verdict(&self, round: u32, agent: &str, reason: &str) -> Halt {
        self.update(|state| {
            state.landing.unchecked(
                TurnKind::Round,
                round,
                (!agent.is_empty()).then_some(agent),
                "the review gave no verdict",
            );
        });
        self.stop(ReviewOutcome::NoVerdict, Some(reason))
    }

    /// The one repair turn for a malformed verdict, in the reviewer's own
    /// conversation `handle`: a usable verdict, or why the round has none.
    /// Whatever it ends with, no other reviewer is asked.
    async fn repair(
        &mut self,
        round: u32,
        reviewer: &Reviewer,
        handle: &str,
        error: &str,
        expect: &Expect<'_>,
    ) -> Result<Result<Verdict, String>, Halt> {
        let agent = reviewer.agent.as_str();
        self.check()?;
        self.phase(
            Phase::Repair,
            &format!("review round {round}: {agent} repairs its verdict"),
        );
        self.tool.write(
            "reviewer.started",
            json!({"round": round, "agent": agent, "kind": "repair"}),
        )?;
        let timeout = self.plan().timeout.min(REPAIR_TIMEOUT);
        let call = self.plan().reviewer_call(
            reviewer,
            &prompts::repair_prompt(error),
            Some(handle),
            timeout,
        );
        let turn = self.continue_reviewer(agent, handle, call).await?;
        self.record_reviewer(round, agent, "repair", &turn)?;
        let parsed = blocks::parse_verdict(&turn.reply, turn.truncated, expect);
        self.record_verdict(round, agent, &parsed)?;
        self.check()?;
        if let Ok(verdict) = parsed
            && (verdict.decision != Decision::Approve || turn.completed())
        {
            return Ok(Ok(verdict));
        }
        let (result, reason) = if turn.unavailable {
            (
                ReviewerResult::Unavailable,
                "reviewer_unavailable".to_owned(),
            )
        } else {
            match turn.status {
                JobStatus::Completed => (ReviewerResult::Failed, "malformed_verdict".to_owned()),
                JobStatus::Declined => {
                    let words = clean(&turn.reply, REFUSAL_CHARS);
                    self.update(|state| state.refused(agent, words));
                    (ReviewerResult::Declined, "reviewer_declined".to_owned())
                }
                status => (ReviewerResult::Failed, status_reason("reviewer", status)),
            }
        };
        self.update(|state| state.tried(agent, result));
        Ok(Err(reason))
    }

    /// Run the next turn of reviewer conversation `handle`, after any call
    /// ahead of it in its lane.
    async fn continue_reviewer(
        &mut self,
        agent: &str,
        handle: &str,
        call: Value,
    ) -> Result<Turn, Halt> {
        let place = self.tool.lanes.join(handle);
        if place.wait_first(&self.token).await.is_err() {
            return Err(Halt::Done);
        }
        let result = self
            .tool
            .agents
            .execute_when_idle(call, self.context.clone())
            .await;
        drop(place);
        Ok(Turn::read(agent, result))
    }

    /// After an approving verdict in `round` with `land: after_approval`:
    /// the builder's landing turn, then the approving reviewer's check.
    async fn land(
        &mut self,
        round: u32,
        verdict: &Verdict,
        reviewer: String,
        held: Option<Held>,
    ) -> Result<(), Halt> {
        // Kept, pinned, through the landing turn and its confirmation.
        let held = held.map(|mut held| {
            held.pin = self.tool.conversations.pin(&held.handle);
            held
        });
        self.builder_turn(TurnKind::Landing, prompts::landing_prompt(round))
            .await?;
        let (reported, status) = self.live().read(|state| {
            (
                state.landing.reported(TurnKind::Landing, round),
                state.landing.status(),
            )
        });
        let completed = self.builder.as_ref().is_some_and(Turn::completed);
        let Some((reference, commits)) = reported.filter(|_| completed) else {
            // Did not land, so nothing to confirm: the result says so in
            // place of any fallback or refusal note.
            self.not_landed = !completed
                || matches!(
                    status,
                    Some(LandingStatus::NotLanded | LandingStatus::Failed)
                );
            return Ok(());
        };
        let Some(held) = held else {
            self.update(|state| {
                state.landing.unchecked(
                    TurnKind::Landing,
                    round,
                    Some(&reviewer),
                    "the approving reviewer can't be continued",
                );
            });
            self.tool.write(
                "landing_check",
                json!({"round": round, "agent": reviewer,
                       "error": "the approving reviewer can't be continued"}),
            )?;
            return Ok(());
        };
        self.kept = Some((reviewer.clone(), held));
        self.confirm(round, verdict, &reviewer, &reference, &commits)
            .await
    }

    /// The approving reviewer's one confirmation turn.
    async fn confirm(
        &mut self,
        round: u32,
        verdict: &Verdict,
        agent: &str,
        reference: &str,
        commits: &[String],
    ) -> Result<(), Halt> {
        self.check()?;
        let handle = self
            .kept
            .as_ref()
            .map(|(_, held)| held.handle.clone())
            .unwrap_or_default();
        self.update(|state| {
            state.reviewer = agent.to_owned();
            state
                .landing
                .checking(TurnKind::Landing, round, Some(agent));
        });
        self.phase(
            Phase::Confirmation,
            &format!("review: {agent} checks the landing"),
        );
        self.tool.write(
            "reviewer.started",
            json!({"round": round, "agent": agent, "kind": "confirmation"}),
        )?;
        let approved = verdict
            .approved
            .as_ref()
            .map(|approved| (approved.base.as_str(), approved.head.as_str()))
            .unwrap_or_default();
        let report = json!({"status":"landed","ref":reference,"commits":commits}).to_string();
        let prompt = prompts::confirmation_prompt(approved, round, &report, reference);
        let reviewer = self
            .plan()
            .reviewers
            .iter()
            .find(|reviewer| reviewer.agent == agent)
            .cloned()
            .unwrap_or(Reviewer {
                agent: agent.to_owned(),
                model: None,
                effort: None,
            });
        let call =
            self.plan()
                .reviewer_call(&reviewer, &prompt, Some(&handle), self.plan().timeout);
        let turn = self.continue_reviewer(agent, &handle, call).await?;
        let parsed = blocks::parse_check(&turn.reply, turn.truncated);
        // Only a run that completed checks anything: one that did not leaves
        // the landing NOT confirmed whatever its block says, which the
        // journal keeps. No fallback exists here for a negative block to
        // guard against, unlike a round's verdict.
        let usable = parsed.as_ref().ok().filter(|_| turn.completed());
        let reason = if usable.is_some() {
            None
        } else if turn.unavailable {
            Some("the approving reviewer was unavailable")
        } else {
            match turn.status {
                JobStatus::Completed => Some("malformed confirmation"),
                JobStatus::Timeout => Some("the confirmation timed out"),
                JobStatus::Declined => {
                    let words = clean(&turn.reply, REFUSAL_CHARS);
                    self.update(|state| state.refused(agent, words));
                    Some("the approving reviewer declined the confirmation")
                }
                JobStatus::Cancelled => Some("the confirmation was stopped"),
                _ => Some("the confirmation failed"),
            }
        };
        let mut evidence = Value::Null;
        let mut evidence_reason = Value::Null;
        self.update(|state| {
            match (usable, &reason) {
                (Some(check), _) => state
                    .landing
                    .checked(TurnKind::Landing, round, agent, check),
                (None, Some(reason)) => {
                    state
                        .landing
                        .unchecked(TurnKind::Landing, round, Some(agent), reason);
                }
                (None, None) => {}
            }
            let summary = state
                .landing
                .summary(ReviewOutcome::Approved, round, None, None);
            evidence = serde_json::to_value(summary.evidence).unwrap_or_default();
            evidence_reason = summary.reason.into();
        });
        self.record_reviewer(round, agent, "confirmation", &turn)?;
        let mut event = json!({"round": round, "agent": agent,
                               "evidence": evidence, "reason": evidence_reason});
        match &parsed {
            Ok(check) => {
                event["check"] = json!({
                    "status": check.status.as_str(),
                    "ref": check.reference,
                    "commits": check.commits,
                    "evidence": check.evidence,
                    "note": check.note,
                });
            }
            Err(error) => event["error"] = error.clone().into(),
        }
        self.tool.write("landing_check", event)?;
        self.kept = None;
        Ok(())
    }

    fn record_reviewer(
        &self,
        round: u32,
        agent: &str,
        kind: &str,
        turn: &Turn,
    ) -> Result<(), Halt> {
        self.tool.write(
            "reviewer.finished",
            json!({
                "round": round,
                "agent": agent,
                "kind": kind,
                "status": turn.status.as_str(),
                "error": turn.error.as_deref().map(|error| cap(error, JOURNAL_REVIEWER_BYTES)),
                "unavailable": turn.unavailable,
                "reply": cap(&turn.reply, JOURNAL_REVIEWER_BYTES),
            }),
        )
    }

    fn record_verdict(
        &self,
        round: u32,
        agent: &str,
        parsed: &Result<Verdict, String>,
    ) -> Result<(), Halt> {
        let mut event = json!({"round": round, "agent": agent});
        match parsed {
            Ok(verdict) => {
                let findings: Vec<Value> = verdict
                    .findings
                    .iter()
                    .enumerate()
                    .map(|(index, finding)| {
                        json!({
                            "id": format!("{round}.{}", index + 1),
                            "severity": finding.severity.as_str(),
                            "title": finding.title,
                            "detail": finding.detail,
                            "location": finding.location,
                        })
                    })
                    .collect();
                let prior: Vec<Value> = verdict
                    .prior
                    .iter()
                    .map(|settled| {
                        json!({"id": settled.id, "status": settled.status.as_str(), "note": settled.note})
                    })
                    .collect();
                event["verdict"] = json!({
                    "verdict": verdict.decision.as_str(),
                    "summary": verdict.summary,
                    "prior": prior,
                    "findings": findings,
                    "evidence": verdict.evidence,
                    "landing_check": verdict.landing_check.as_ref().map(|check| json!({
                        "status": check.status.as_str(),
                        "ref": check.reference,
                        "commits": check.commits,
                        "evidence": check.evidence,
                        "note": check.note,
                    })),
                    "approved": verdict.approved.as_ref().map(|approved| json!({
                        "base": approved.base, "head": approved.head,
                    })),
                });
            }
            Err(error) => event["error"] = error.clone().into(),
        }
        self.tool.write("verdict", event)
    }

    /// Why `agent` reviews instead of the agents before it in the order.
    fn fallback(&self, agent: &str) -> Option<String> {
        let mut unavailable = Vec::new();
        let mut declined = Vec::new();
        for reviewer in &self.plan().reviewers {
            if reviewer.agent == agent {
                break;
            }
            match self.skipped.get(&reviewer.agent) {
                Some(Skip::Unavailable) => unavailable.push(reviewer.agent.as_str()),
                Some(Skip::Declined) => declined.push(reviewer.agent.as_str()),
                None => {}
            }
        }
        let mut parts = Vec::new();
        if !unavailable.is_empty() {
            parts.push(format!("{} unavailable", unavailable.join(", ")));
        }
        if !declined.is_empty() {
            parts.push(format!("{} declined", declined.join(", ")));
        }
        let why = parts.join("; ");
        if agent == self.plan().builder {
            Some(if why.is_empty() {
                "same agent as builder".to_owned()
            } else {
                format!("same agent as builder: {why}")
            })
        } else {
            (!why.is_empty()).then_some(why)
        }
    }

    /// End the job: the journal's last event, and the builder's last result
    /// with SCV's `review` and `landing`.
    fn finish(mut self, halt: Halt) -> ToolOutput {
        if halt == Halt::Journal {
            // Stops a review whose outcome was not yet decided; one already
            // decided stands, with its journal shown incomplete.
            self.live()
                .fix(&self.token, ReviewOutcome::Stopped, Some("journal_error"));
        }
        let cancelled = self.token.is_cancelled();
        self.update(|state| state.phase = Phase::Finished);
        // Released before the job's result is visible.
        self.kept = None;
        self.pin = None;
        self.place = None;
        let findings = std::mem::take(&mut self.findings);
        self.tool.conclude(
            cancelled,
            Ending {
                builder: self.builder.take(),
                error: None,
                builder_session: self.handle.take(),
                findings: &findings,
                not_landed: self.not_landed,
            },
        )
    }
}

/// An agent result with no reply, for a turn that produced none.
fn agent_result(agent: &str, status: JobStatus, error: Option<&str>) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("agent".into(), agent.into());
    value.insert("status".into(), status.as_str().into());
    value.insert("reply".into(), "".into());
    if let Some(error) = error {
        value.insert("error".into(), error.into());
    }
    value
}

/// `text` cut to `limit` bytes on a character boundary.
fn cap(text: &str, limit: usize) -> String {
    scv_client::text::utf8_prefix(text, limit).to_owned()
}

#[cfg(test)]
mod tests;
