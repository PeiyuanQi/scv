//! The agent workbench: what SCV knows about delegated agents, background
//! jobs, and their sessions across the whole instance, as a dashboard reads
//! it ([`WorkbenchSnapshot`]), and the activity a delegated run records for
//! its live view ([`RunEvent`], [`RunLogPage`]).
//!
//! Every type here is additive to protocol version 3: a daemon sends them
//! only in answer to the `workbench*`, `run_log`, and `worktree*`
//! `daemon.control` actions, which only newer clients send.

use serde::{Deserialize, Serialize};

/// What a delegated run, a background job, or a session is doing, as far as
/// SCV can tell. A state a client does not know parses as `unknown`, which
/// also means SCV itself cannot tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Launching: starting the process, its handshake, or its session.
    Starting,
    /// A turn runs and the agent reports activity.
    Working,
    /// The agent waits for a person: an approval request it relayed, or the
    /// session's own approval request, is unanswered.
    AwaitingInput,
    /// A live agent (an ACP server or nested SCV) that waits between turns
    /// for its next prompt; or a session with no turn running.
    Idle,
    /// A prompt that waits for its conversation's running turn.
    Queued,
    /// The turn finished and the agent completed its task.
    Completed,
    /// The run or job failed.
    Failed,
    /// The agent's model refused the request.
    Declined,
    /// The run reached its time limit.
    Timeout,
    /// Stopped on request.
    Cancelled,
    /// Its SCV process or session stopped before it finished: a crash, a
    /// restart, or the session's client going away.
    Interrupted,
    /// SCV cannot tell: a turn has reported nothing for a long while, or the
    /// process that owns it cannot be observed. Also any state this client
    /// does not know.
    #[serde(other)]
    Unknown,
}

impl AgentState {
    /// The state as SCV writes it, such as `awaiting_input`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::AwaitingInput => "awaiting_input",
            Self::Idle => "idle",
            Self::Queued => "queued",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "declined",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }

    /// Whether nothing more happens in this state by itself: the run or job
    /// has ended one way or another.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Declined
                | Self::Timeout
                | Self::Cancelled
                | Self::Interrupted
        )
    }

    /// Whether a person is needed to go on.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::AwaitingInput)
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Who started a background job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOrigin {
    /// The session's model, through an `agent` call.
    #[default]
    Model,
    /// The owner, from the workbench (a dashboard or `scv jobs`).
    Owner,
    /// An origin this client does not know.
    #[serde(other)]
    Unknown,
}

/// A git worktree SCV created for one job, so its agent works apart from
/// the repository's own checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRef {
    /// The worktree's directory, under `$SCV_HOME/worktrees`.
    pub path: String,
    /// The branch SCV created for it, such as `scv/codex-3f9a2c`.
    pub branch: String,
    /// The repository it was created from.
    pub repo: String,
}

/// A worktree SCV created, as `scv agents worktrees` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Where it is, its branch, and the repository it belongs to.
    #[serde(flatten)]
    pub worktree: WorktreeRef,
    /// Whether it has uncommitted changes; absent when git could not tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// The jobs recorded as working in it, as `<session>/<job>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
}

/// One SCV session the workbench knows: a live daemon session, the
/// workbench's own host for jobs the owner starts, or one that ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchSession {
    /// The session ID.
    pub id: String,
    /// Who it serves, such as `TUI`, `WeChat`, `scv exec`, or `workbench`.
    pub label: String,
    /// Its workspace directory.
    pub workspace: String,
    /// Whether it still runs in a process of this instance.
    pub live: bool,
    /// What it is doing, when it is live.
    pub state: AgentState,
    /// SCV's words for that state, such as which approval it waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One background job, from the instance's job ledger and, while it runs,
/// its live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchJob {
    /// The session that owns it.
    pub session: String,
    /// Its handle in that session, such as `job-3`.
    pub job: String,
    /// The agent that runs it, such as `codex`.
    pub agent: String,
    /// The first line of its prompt, shortened.
    pub task: String,
    /// The session's workspace.
    pub workspace: String,
    /// Where its agent runs: a directory in the workspace, or its worktree.
    pub cwd: String,
    /// What it is doing.
    pub state: AgentState,
    /// SCV's words for that state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Who started it.
    pub started_by: JobOrigin,
    /// The conversation it runs a turn of, such as `codex-2`, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// The delegated run serving it now, or that served it last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The worktree SCV created for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRef>,
    /// A reviewed job (`review`), whose journal holds its rounds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reviewed: bool,
    /// The owner can resume it from the workbench.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resumable: bool,
    /// The job it was resumed as, `<session>/<job>`, once it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_as: Option<String>,
    /// When it was started.
    pub created_unix_seconds: u64,
    /// When its record last changed.
    pub updated_unix_seconds: u64,
}

/// One delegated run (an agent process) and what it is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchRun {
    /// Its delegation handle, such as `codex-3f9a2c`; its run log's name.
    pub handle: String,
    /// The agent, such as `codex`.
    pub agent: String,
    /// The session that started it.
    pub session: String,
    /// How SCV reaches it: `cli` (one process per turn), `acp`, or `scv`.
    pub transport: String,
    /// The conversation it serves, such as `codex-2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// The turn of that conversation it serves, or served last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// The background job it serves, when it serves one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Its working directory.
    pub cwd: String,
    /// What it is doing.
    pub state: AgentState,
    /// SCV's words for that state, such as the approval it waits for or
    /// how long it has been quiet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When it started.
    pub started_unix_seconds: u64,
    /// When it last reported anything.
    pub last_activity_unix_seconds: u64,
    /// Its process still runs.
    pub live: bool,
    /// A running turn of it can take a further prompt (ACP steering).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steerable: bool,
}

/// An approval request a background job relayed while the owner watches the
/// workbench, waiting for the owner's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchApproval {
    /// Answer with `workbench_approve` naming this ID.
    pub id: String,
    /// The session whose job asks.
    pub session: String,
    /// The job that asks.
    pub job: String,
    /// The tool the request is for, as the nested agent named it.
    pub name: String,
    /// Its risk, such as `process`.
    pub risk: String,
    /// The working directory it concerns.
    pub cwd: String,
    /// What it will do.
    pub summary: String,
    /// When it was asked.
    pub since_unix_seconds: u64,
    /// When it gets the answer the session gives unattended (a denial).
    pub expires_unix_seconds: u64,
}

/// A session whose queued prompts SCV kept when its process stopped (a
/// restart or crash), which a new session can restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorableSession {
    /// The session that stopped.
    pub id: String,
    /// Who it served.
    pub label: String,
    /// Its workspace.
    pub workspace: String,
    /// Prompts that never started.
    pub queued: u32,
    /// The start of the prompt whose turn ran when it stopped, which is
    /// never sent again by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<String>,
    /// When it was last saved.
    pub saved_unix_seconds: u64,
}

/// Everything the workbench shows, at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchSnapshot {
    /// When it was taken.
    pub generated_unix_seconds: u64,
    /// Sessions with jobs, runs, or live state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<WorkbenchSession>,
    /// Background jobs, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<WorkbenchJob>,
    /// Delegated runs, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<WorkbenchRun>,
    /// Approval requests waiting for the owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<WorkbenchApproval>,
    /// Sessions whose queued prompts can be restored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restorable: Vec<RestorableSession>,
}

/// What one line of a run's activity log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    /// The run began: its agent, transport, and working directory.
    RunStarted,
    /// A turn began with a prompt.
    TurnStarted,
    /// A line of the agent's own message.
    Text,
    /// A tool call or command the agent started, as a short redacted line.
    Tool,
    /// A tool call that failed.
    ToolFailed,
    /// The agent's current plan step.
    Plan,
    /// The agent asked for approval.
    ApprovalRequested,
    /// That request was answered.
    ApprovalResolved,
    /// A prompt was added to the running turn.
    Steered,
    /// A turn ended.
    TurnEnded,
    /// The run's process ended.
    RunEnded,
    /// SCV's own note, such as why the run stopped.
    Note,
    /// The log reached its size limit; only turns, approvals, and the end
    /// are recorded after this.
    Truncated,
    /// A kind this client does not know.
    #[serde(other)]
    Unknown,
}

/// One entry of a run's activity log. Text is display-only: agent messages
/// and short tool lines with credential-like values redacted, never tool
/// output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Its position in the log, from 1.
    pub seq: u64,
    /// When it happened, in Unix milliseconds.
    pub ts_ms: u64,
    /// What it records.
    pub kind: RunEventKind,
    /// Its text: the message line, tool line, prompt, summary, or note.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The conversation turn it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// How a turn or run ended, or an approval was answered (`approved`,
    /// `denied`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The background job a turn serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// An approval request's risk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    /// Who sent a turn's prompt or answered an approval: `model`, `owner`,
    /// or `policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// Part of a run's activity log, after a position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLogPage {
    /// The run, by delegation handle.
    pub handle: String,
    /// Its events after the requested position, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<RunEvent>,
    /// Pass as `after` to read on: the last position returned, or the
    /// requested one when nothing was new.
    pub next: u64,
    /// The run has ended, so nothing more will be logged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ended: bool,
    /// More events wait beyond this page.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

/// How `workbench_send` hands the owner's prompt to a conversation that is
/// running a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Steer the running turn when its agent can take the prompt, else
    /// queue it.
    #[default]
    Auto,
    /// Queue it behind the running turn.
    Queue,
    /// Steer the running turn, or fail when it cannot be steered.
    Steer,
}

/// What `session.start` restored from a session that stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredQueue {
    /// The session whose queue was restored.
    pub from: String,
    /// Queued prompts restored; the new session's queue starts paused.
    pub queued: u32,
    /// The start of the prompt that was running when the old session
    /// stopped. It is not sent again: its outcome is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<String>,
}

#[cfg(test)]
mod tests;
