//! Background delegation jobs, as clients see them.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::review::{JobOutcome, describe_outcome};

/// Why the server started a turn on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnOrigin {
    /// What the turn is for.
    pub kind: OriginKind,
    /// The background jobs this turn reports. Its end settles them: the
    /// model has seen their results when it completes, the user stopped the
    /// report when it is cancelled, and when it fails they are reported
    /// directly ([`ServerEvent::BackgroundReported`](crate::ServerEvent::BackgroundReported),
    /// sent first), unless `retry_seconds` is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
    /// On a report turn's `turn.failed` only: its jobs are still unreported,
    /// and the server starts another report turn for them in about this many
    /// seconds, or as soon as another turn succeeds. Absent everywhere else,
    /// and from servers before 0.3.11, whose failed report turns settled their
    /// jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_seconds: Option<u64>,
    /// The outcomes of the reviewed jobs among `jobs`, which clients show
    /// as SCV's own lines before the model's report. Absent from servers
    /// before 0.3.14.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<JobOutcome>,
}

/// What a server-started turn is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OriginKind {
    /// Finished background delegations are being reported.
    Background,
    /// A kind this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// Where a background job stands, with the strings the model reads in the
/// job tools' results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobStatus {
    /// Still working.
    Running,
    /// The agent finished its task.
    Completed,
    /// The agent or its run failed.
    Failed,
    /// The agent's model refused the request.
    Declined,
    /// The run reached its time limit.
    Timeout,
    /// Stopped on request.
    Cancelled,
    /// A status this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

impl JobStatus {
    /// The status as the model reads it, such as `completed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "declined",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A background job a tool call started, or whose result the call showed
/// the model (`tool.completed.jobs`). A session's clients keep it open while
/// its jobs run, since closing the session cancels them.
///
/// A job appears once as `running`, from the `agent` call that started it,
/// and once more with how it ended, from the `agent_wait`, `agent_status`, or
/// `agent_cancel` call through which the model saw that result. A job whose
/// result the model sees in a report turn is settled by that turn's
/// [`TurnOrigin::jobs`] instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobChange {
    /// The job's handle, such as `job-1`.
    pub job: String,
    /// The delegating tool: `agent`, or `agent_<name>` from SCV 0.3.0 and
    /// older, which had one tool per agent.
    pub tool: String,
    /// The agent that runs the job, such as `codex`; empty from SCV 0.3.0
    /// and older. [`JobChange::agent_name`] reads either form.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// `running` when the call started it; otherwise how it ended.
    pub status: JobStatus,
    /// The first line of the delegated prompt, shortened; empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// On a reviewed job's start: its review journal's ID, such as
    /// `rev-1759961234-3fa9c1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<String>,
    /// On the change that settles a reviewed job: its outcome, which clients
    /// show as SCV's own lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<JobOutcome>,
}

impl JobChange {
    /// Whether the call started the job, rather than settled it.
    pub fn started(&self) -> bool {
        self.status == JobStatus::Running
    }

    /// The agent that runs the job, such as `codex`, from either form.
    pub fn agent_name(&self) -> &str {
        job_agent(&self.agent, &self.tool)
    }
}

/// A finished background job's result, as the server reports it: to the
/// model in a report turn's prompt, or to the client in
/// [`ServerEvent::BackgroundReported`](crate::ServerEvent::BackgroundReported)
/// when the model could not report it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobReport {
    /// The job's handle, such as `job-1`.
    pub job: String,
    /// The agent that ran it, such as `codex`.
    pub agent: String,
    /// The first line of the delegated prompt, shortened; empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// How it ended.
    pub status: JobStatus,
    /// The agent conversation it ran in, which a later `agent` call may
    /// continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The agent's reply, bounded: untrusted delegated-agent output.
    pub reply: String,
    /// A reviewed job's outcome, which SCV states in its own words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<JobOutcome>,
}

/// `reports` as people and the model read them: for each job, a line naming
/// it, its agent, conversation, and status, then its task, a reviewed job's
/// outcome lines ([`describe_outcome`]), and its reply.
pub fn describe_reports(reports: &[JobReport]) -> String {
    let mut text = String::new();
    for report in reports {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("{} ({}", report.job, report.agent));
        if let Some(session) = &report.session {
            text.push_str(&format!(", conversation {session}"));
        }
        text.push_str(&format!("): {}\n", report.status));
        if !report.task.is_empty() {
            text.push_str(&format!("Task: {}\n", report.task));
        }
        if let Some(outcome) = &report.outcome {
            text.push_str(&describe_outcome(outcome));
        }
        let reply = report.reply.trim();
        if !reply.is_empty() {
            text.push_str(reply);
            text.push('\n');
        }
    }
    text
}

/// The agent a background job runs, such as `codex`: `agent` when it is
/// set, otherwise what follows `agent_` in the delegating `tool`, as SCV
/// 0.3.0 and older named it (`agent_codex`), otherwise `tool` itself.
pub fn job_agent<'a>(agent: &'a str, tool: &'a str) -> &'a str {
    if agent.is_empty() {
        tool.strip_prefix("agent_").unwrap_or(tool)
    } else {
        agent
    }
}
