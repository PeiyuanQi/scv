//! Background delegation jobs, as clients see them.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why the server started a turn on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnOrigin {
    /// What the turn is for.
    pub kind: OriginKind,
    /// The background jobs this turn reports. Once it starts, the model has
    /// seen their results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
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
/// A job appears once as `running`, from the `agent_*` call that started it,
/// and once more with how it ended, from the `agent_wait`, `agent_status`, or
/// `agent_cancel` call through which the model saw that result. A job whose
/// result the model sees in a report turn is settled by that turn's
/// [`TurnOrigin::jobs`] instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobChange {
    /// The job's handle, such as `job-1`.
    pub job: String,
    /// The delegating tool, such as `agent_codex`.
    pub tool: String,
    /// `running` when the call started it; otherwise how it ended.
    pub status: JobStatus,
    /// The first line of the delegated prompt, shortened; empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
}

impl JobChange {
    /// Whether the call started the job, rather than settled it.
    pub fn started(&self) -> bool {
        self.status == JobStatus::Running
    }
}
