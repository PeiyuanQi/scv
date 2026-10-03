//! Daemon control: health, delegations, and the commands `scv` sends over
//! `daemon.control`.

use serde::{Deserialize, Serialize};

/// Where a daemon component (a channel account) is in its lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    /// Turned off in configuration.
    Disabled,
    /// Starting up.
    Starting,
    /// Running and connected to its platform.
    Connected,
    /// Running but not connected.
    Disconnected,
    /// Waiting before a restart after a failure.
    Backoff,
    /// Shutting down.
    Stopping,
    /// Not running.
    Stopped,
    /// Stopped after an error it cannot recover from, such as expired credentials.
    Failed,
}

/// The health of one channel account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentHealth {
    /// Component ID, `<channel>:<account>`.
    pub id: String,
    /// The chat channel this account belongs to, such as `wechat`.
    #[serde(default)]
    pub channel: String,
    /// The account name within the channel.
    pub account: String,
    /// The platform's ID of the bot account, when known.
    pub bot_id: Option<String>,
    /// The platform's ID of the account's owner, when known.
    pub user_id: Option<String>,
    /// Whether the account should run.
    pub enabled: bool,
    /// Lifecycle state.
    pub state: ComponentState,
    /// When the platform last answered successfully.
    pub last_success_unix_seconds: Option<u64>,
    /// The latest error, if any.
    pub error: Option<String>,
    /// Restarts since the daemon started.
    pub restarts: u64,
    /// Effective remote tool authority; `owner` only when the owner ID is known.
    #[serde(default)]
    pub remote_tools: RemoteTools,
    /// Who the account answers, as set; `owner` with no known owner ID
    /// answers nobody. `None` from daemons before 0.3.0, which answer anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senders: Option<Senders>,
    /// What a chat account carries, when it is not an ordinary chat:
    /// `mail` for a mail chat. Absent for ordinary chats, email accounts,
    /// and daemons before mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<Purpose>,
    /// An email account's counts: never addresses, subjects, or any other
    /// mail text. Absent for chat accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailCounts>,
}

/// What a chat account carries.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Conversations with a model (the default).
    #[default]
    Chat,
    /// Only the email accounts' reports to the owner: no model ever answers
    /// in it, it has no tools, and SCV never logs it.
    Mail,
}

/// An email account's activity, as counts only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MailCounts {
    /// Mail claimed and not yet decided.
    pub claimed: u64,
    /// Report items waiting to be sent to the mail chat.
    pub queued: u64,
    /// Mail decided today (the account's local day).
    pub seen_today: u64,
    /// Of those, mail a model triaged.
    pub triaged_today: u64,
    /// Of those, mail reported to the owner.
    pub reported_today: u64,
    /// Model tokens spent today.
    pub tokens_today: u64,
    /// The daily token budget.
    pub token_budget: u64,
    /// Digest messages sent to the mail chat in the last 24 hours.
    pub messages_24h: u64,
    /// When the mailbox was last checked, in Unix seconds.
    pub last_check_unix_seconds: Option<u64>,
}

/// Who may use tools through a remote bridge account.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTools {
    /// Every remote session is tool-free (the default).
    #[default]
    None,
    /// The account's authenticated owner gets full, auto-approved tools.
    Owner,
}

/// Whose messages a remote bridge account answers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Senders {
    /// Only the account's authenticated owner (the default); everyone else's
    /// messages are dropped unanswered. Without a known owner ID, nobody.
    #[default]
    Owner,
    /// Anyone who can reach the bot; everyone but the owner stays tool-free.
    Anyone,
}

/// What `scv status` shows: the daemon, its components, and its delegations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    /// The daemon's release.
    pub version: String,
    /// The daemon's process ID.
    pub pid: u32,
    /// Channel accounts.
    pub components: Vec<ComponentHealth>,
    /// Delegated agent runs.
    #[serde(default)]
    pub delegations: DelegationSummary,
    /// A restart the daemon has scheduled, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartInfo>,
    /// The question a `confirm_ask` or `confirm_status` request is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<ConfirmInfo>,
    /// The response to a project ledger command, when one was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectResponse>,
}

/// The lifecycle phase observed for a durable project.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPhase {
    /// The project has been created and is being understood.
    Discovery,
    /// Tasks and dependencies are being prepared.
    Planning,
    /// One or more implementation tasks are running.
    Implementation,
    /// Work is being checked or has failed a run.
    Verification,
    /// Work is ready for staging.
    Staging,
    /// Work has been released to production.
    Production,
    /// The project is being reported or closed.
    Reporting,
}

/// The reducer's current project status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// The orchestrator can make progress.
    Active,
    /// No task can currently make progress.
    Blocked,
    /// An owner decision is required.
    WaitingApproval,
    /// Every task has completed successfully.
    Completed,
    /// A task exhausted its retry budget or a run failed permanently.
    Failed,
    /// The project was intentionally retired.
    Archived,
}

/// A task's observed state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectTaskStatus {
    /// Waiting for dependencies.
    Pending,
    /// Eligible to start.
    Ready,
    /// Owned by a currently running agent.
    Running,
    /// Waiting on an external condition.
    Blocked,
    /// Finished successfully.
    Done,
    /// Failed permanently.
    Failed,
    /// Its owner stopped reporting heartbeats.
    Stale,
}

/// A delegated run's observed state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRunStatus {
    /// The delegated run is still executing.
    Running,
    /// The run finished successfully.
    Succeeded,
    /// The run finished with an error.
    Failed,
    /// The run was intentionally cancelled.
    Cancelled,
    /// The run stopped reporting heartbeats.
    Stale,
}

/// A compact project record suitable for status output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSummary {
    /// Stable project identifier.
    pub id: String,
    /// Owner-visible project name.
    pub name: String,
    /// Absolute workspace directory.
    pub workspace: String,
    /// Current lifecycle phase.
    pub phase: ProjectPhase,
    /// Current reducer status.
    pub status: ProjectStatus,
    /// Creation time in Unix seconds.
    pub created_unix_seconds: u64,
    /// Last reducer update in Unix seconds.
    pub updated_unix_seconds: u64,
    /// Number of tasks in the project.
    pub task_count: u64,
    /// Number of tasks in the done state.
    pub completed_tasks: u64,
    /// Last task or run heartbeat, when one exists.
    pub last_heartbeat_unix_seconds: Option<u64>,
}

/// A project task and its dependency/progress evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectTask {
    /// Stable task identifier.
    pub id: String,
    /// Owner-visible task title.
    pub title: String,
    /// Current task state.
    pub status: ProjectTaskStatus,
    /// Task identifiers that must complete first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Number of failed attempts.
    pub retries: u32,
    /// Maximum failed attempts before the task is permanently failed.
    pub max_retries: u32,
    /// The current run, when the task is running.
    pub run_id: Option<String>,
    /// Latest observed progress text.
    pub progress: Option<String>,
    /// Last heartbeat in Unix seconds.
    pub last_heartbeat_unix_seconds: Option<u64>,
}

/// A run tracked by the project ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRun {
    /// Stable run identifier.
    pub id: String,
    /// Task owned by this run.
    pub task_id: String,
    /// Agent adapter that owns the run.
    pub agent: String,
    /// Current run state.
    pub status: ProjectRunStatus,
    /// One-based attempt number.
    pub attempt: u32,
    /// Start time in Unix seconds.
    pub started_unix_seconds: u64,
    /// Last observed update in Unix seconds.
    pub updated_unix_seconds: u64,
    /// Latest observed progress text.
    pub progress: Option<String>,
}

/// One durable event, exposed for audit and recovery inspection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectEvent {
    /// Monotonic event sequence within the instance ledger.
    pub sequence: u64,
    /// Project that owns the event.
    pub project_id: String,
    /// Reducer event kind.
    pub kind: String,
    /// Event time in Unix seconds.
    pub timestamp_unix_seconds: u64,
    /// Event-specific evidence.
    pub data: serde_json::Value,
}

/// A report generated from observed ledger state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectReport {
    /// Current project summary.
    pub project: ProjectSummary,
    /// Current task records.
    pub tasks: Vec<ProjectTask>,
    /// Current run records.
    pub runs: Vec<ProjectRun>,
    /// Report generation time in Unix seconds.
    pub generated_unix_seconds: u64,
    /// Human-readable evidence notes.
    pub evidence: Vec<String>,
}

/// Payload returned by a project command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectResponse {
    /// A project was created.
    Created {
        /// Created project summary.
        project: ProjectSummary,
    },
    /// A project status was requested.
    Status {
        /// Current project summary.
        project: ProjectSummary,
    },
    /// Project events were requested.
    Events {
        /// Project identifier.
        project_id: String,
        /// Highest sequence included in a compacted snapshot, when older
        /// events are no longer retained in the live tail.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted_before: Option<u64>,
        /// Matching events.
        events: Vec<ProjectEvent>,
    },
    /// Project tasks were requested.
    Tasks {
        /// Project identifier.
        project_id: String,
        /// Current tasks.
        tasks: Vec<ProjectTask>,
    },
    /// A report was generated.
    Report {
        /// Generated report.
        report: ProjectReport,
    },
    /// A project changed.
    Updated {
        /// Current project summary.
        project: ProjectSummary,
    },
    /// A task was created.
    TaskAdded {
        /// Current project summary.
        project: ProjectSummary,
        /// New task identifier.
        task_id: String,
    },
    /// A run was started.
    RunStarted {
        /// Current project summary.
        project: ProjectSummary,
        /// New run identifier.
        run_id: String,
    },
}

/// How long a question to the owner waits for an answer unless the asker
/// says otherwise.
pub const DEFAULT_CONFIRM_SECONDS: u64 = 30 * 60;
/// The longest a question to the owner may wait: the default ceiling of a
/// delegated agent's own tool call (`tools.max_timeout_seconds`), which is
/// how a delegated agent asks.
pub const MAX_CONFIRM_SECONDS: u64 = 4 * 60 * 60;

/// A yes/no question to the owner in chat (`scv confirm`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfirmInfo {
    /// The question's ID, for `confirm_status`.
    pub id: String,
    /// Where it stands.
    pub state: ConfirmState,
    /// The account whose owner was asked, as `<channel>:<account>`.
    pub chat: String,
    /// When no answer counts as no.
    pub deadline_unix_seconds: u64,
}

/// Where a question to the owner stands.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmState {
    /// Sent, or being stored for sending, and waiting for an answer.
    Pending,
    /// The owner answered yes.
    Yes,
    /// The owner answered no.
    No,
    /// No answer came before the deadline, which counts as no.
    Expired,
    /// The asker stopped asking about it before an answer came.
    Withdrawn,
    /// The question could not be handed to the chat, or its answer was lost.
    Failed,
    /// A state this client does not know, from a newer daemon.
    #[serde(other)]
    Unknown,
}

/// A restart into a newly installed release, waiting for owner work to end.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestartInfo {
    /// The release it restarts into.
    pub to_version: String,
    /// What it still waits for, such as the requesting delegation or an
    /// owner's message; `None` once it restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// The delegation that asked, whose report goes out first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    /// The chat the announcement goes to, as `<channel>:<account>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// When it restarts even if work is still running.
    pub deadline_unix_seconds: u64,
}

/// Delegated agent runs of the daemon's SCV instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationSummary {
    /// Running delegations, whichever SCV process of the instance started them.
    /// Live agents waiting between turns count too.
    pub active: u64,
    /// How many of `active` are live agents (a nested SCV or an ACP agent)
    /// waiting between turns, apart from a nested SCV whose own background
    /// jobs still count; `None` from a daemon that does not tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<u64>,
    /// Orphaned delegations the daemon has stopped since it started.
    pub reaped: u64,
    /// Listed delegations, for `delegations` and `delegation_kill`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<DelegationInfo>,
    /// Handles this request stopped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub killed: Vec<String>,
}

/// One running delegated agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationInfo {
    /// The delegation to stop.
    pub handle: String,
    /// The agent, such as `codex`.
    pub agent: String,
    /// The SCV session that started it.
    pub session: String,
    /// Delegation depth: 1 for an agent SCV started directly.
    pub depth: u32,
    /// The agent's process ID (and process group).
    pub pid: u32,
    /// The SCV process that started it.
    pub owner_pid: u32,
    /// Live processes in its group plus tagged processes outside it.
    pub processes: u32,
    /// Its working directory.
    pub cwd: String,
    /// When it started.
    pub started_unix_seconds: u64,
    /// The owning SCV process is gone; the daemon will stop it.
    pub orphaned: bool,
    /// The conversation this run is a turn of, and which turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// Which turn of that conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// A live agent (a nested SCV or an ACP agent) with no turn running:
    /// when its last turn ended. Absent while it works, for a per-turn run,
    /// and from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since_unix_seconds: Option<u64>,
    /// A nested SCV's own background jobs that still run or wait to be
    /// reported to it; a planned restart waits for them even between turns.
    /// Absent when there are none, for other agents, and from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_jobs: Option<u32>,
}

/// What a `daemon.control` message asks the daemon to do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// Report [`DaemonStatus`].
    Status,
    /// Reread channel settings and reconcile the components now.
    Reload,
    /// Enable or disable one channel account, optionally changing its
    /// workspace, remote tool grant, and whose messages it answers.
    ChannelSet {
        /// The chat channel, such as `wechat` or `feishu`.
        channel: String,
        /// The account name within the channel.
        account: String,
        /// Whether the account should run.
        enabled: bool,
        /// The account's workspace directory; `None` keeps the saved one.
        workspace: Option<String>,
        /// Omitted keeps the saved setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_tools: Option<RemoteTools>,
        /// Whose messages the account answers; omitted keeps the saved
        /// setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        senders: Option<Senders>,
        /// What a chat account carries; omitted keeps the saved setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<Purpose>,
    },
    /// Stop one channel account and remove its credentials and state.
    ChannelLogout {
        /// The chat channel, such as `wechat` or `feishu`.
        channel: String,
        /// The account name within the channel.
        account: String,
    },
    /// List running delegations; `all` includes orphans awaiting cleanup.
    Delegations {
        /// Include orphans awaiting cleanup.
        #[serde(default)]
        all: bool,
    },
    /// Stop one delegation by handle, or every orphaned one.
    DelegationKill {
        /// The delegation to stop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
        /// Stop every orphaned delegation instead.
        #[serde(default)]
        orphans: bool,
    },
    /// Restart into the release installed at the daemon's own path once the
    /// requesting delegation has finished and its report is stored and no
    /// owner message is being answered, or at `max_wait_seconds` anyway.
    RestartWhenIdle {
        /// The release the caller installed; the daemon checks it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// The commit it was built from, for the announcement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        /// The caller's `SCV_PARENT` chain, naming the delegation to wait for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// Longest wait before restarting anyway.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_wait_seconds: Option<u64>,
    },
    /// Ask the owner a yes/no question in chat: in the chat that started the
    /// work `parent` names, or else the notify target. The reply's `confirm`
    /// names the question; `confirm_status` then follows it.
    ConfirmAsk {
        /// The question, as the owner reads it.
        question: String,
        /// The caller's `SCV_PARENT` chain, naming the delegation whose chat
        /// is asked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// How long no answer waits before it counts as no
        /// ([`DEFAULT_CONFIRM_SECONDS`], at most [`MAX_CONFIRM_SECONDS`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
    },
    /// Report where the question `id` stands. Asking also keeps it alive: a
    /// question nobody asks about for a minute is withdrawn.
    ConfirmStatus {
        /// The ID `confirm_ask` returned.
        id: String,
    },
    /// Explicitly create an owner-authorized durable project.
    ProjectCreate {
        /// Owner-visible project name.
        name: String,
        /// Absolute workspace directory.
        workspace: String,
    },
    /// Read a project's reducer state without contacting an agent.
    ProjectStatus {
        /// Project name or identifier.
        project: String,
    },
    /// Read the append-only event history after an optional sequence.
    ProjectEvents {
        /// Project name or identifier.
        project: String,
        /// Return events after this sequence.
        after: Option<u64>,
    },
    /// Read current tasks and observed progress.
    ProjectTasks {
        /// Project name or identifier.
        project: String,
    },
    /// Build a report from ledger evidence.
    ProjectReport {
        /// Project name or identifier.
        project: String,
    },
    /// Add a task to an explicitly created project.
    ProjectTaskAdd {
        /// Project name or identifier.
        project: String,
        /// Owner-visible task title.
        title: String,
        /// Task identifiers that must be done first.
        depends_on: Vec<String>,
        /// Maximum failed attempts before permanent failure.
        max_retries: u32,
    },
    /// Record an observed task state/progress update.
    ProjectTaskUpdate {
        /// Project name or identifier.
        project: String,
        /// Task identifier.
        task: String,
        /// New task state.
        status: ProjectTaskStatus,
        /// Optional progress text.
        progress: Option<String>,
    },
    /// Record a supervised agent run starting.
    ProjectRunStart {
        /// Project name or identifier.
        project: String,
        /// Task identifier.
        task: String,
        /// Agent adapter name.
        agent: String,
    },
    /// Record observed progress from a run heartbeat.
    ProjectRunProgress {
        /// Project name or identifier.
        project: String,
        /// Run identifier.
        run: String,
        /// Progress text.
        progress: String,
    },
    /// Record an observed terminal run state.
    ProjectRunFinish {
        /// Project name or identifier.
        project: String,
        /// Run identifier.
        run: String,
        /// Terminal run state.
        status: ProjectRunStatus,
    },
    /// Record a heartbeat for a project task/run.
    ProjectHeartbeat {
        /// Project name or identifier.
        project: String,
        /// Optional task identifier.
        task: Option<String>,
        /// Optional run identifier.
        run: Option<String>,
    },
}
