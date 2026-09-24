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
    pub active: u64,
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
    /// workspace and remote tool grant.
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
}
