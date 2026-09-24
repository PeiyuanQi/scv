//! Dependency-light wire types shared by SCV clients and the server.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 3;

/// The longest `session.start` channel name.
pub const MAX_CHANNEL_NAME_BYTES: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Disabled,
    Starting,
    Connected,
    Disconnected,
    Backoff,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentHealth {
    pub id: String,
    /// The chat channel this account belongs to, such as `wechat`.
    #[serde(default)]
    pub channel: String,
    pub account: String,
    pub bot_id: Option<String>,
    pub user_id: Option<String>,
    pub enabled: bool,
    pub state: ComponentState,
    pub last_success_unix_seconds: Option<u64>,
    pub error: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub components: Vec<ComponentHealth>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationInfo {
    pub handle: String,
    pub agent: String,
    pub session: String,
    pub depth: u32,
    pub pid: u32,
    /// The SCV process that started it.
    pub owner_pid: u32,
    /// Live processes in its group plus tagged processes outside it.
    pub processes: u32,
    pub cwd: String,
    pub started_unix_seconds: u64,
    /// The owning SCV process is gone; the daemon will stop it.
    pub orphaned: bool,
    /// The conversation this run is a turn of, and which turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DaemonCommand {
    Status,
    Reload,
    /// Enable or disable one channel account, optionally changing its
    /// workspace and remote tool grant.
    ChannelSet {
        channel: String,
        account: String,
        enabled: bool,
        workspace: Option<String>,
        /// Omitted keeps the saved setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_tools: Option<RemoteTools>,
    },
    /// Stop one channel account and remove its credentials and state.
    ChannelLogout {
        channel: String,
        account: String,
    },
    /// List running delegations; `all` includes orphans awaiting cleanup.
    Delegations {
        #[serde(default)]
        all: bool,
    },
    /// Stop one delegation by handle, or every orphaned one.
    DelegationKill {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_wait_seconds: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntry {
    pub queue_id: String,
    pub revision: u64,
    pub prompt: String,
    pub submitter: String,
}

/// Why the server started a turn on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnOrigin {
    /// `background`: finished background delegations are being reported.
    pub kind: String,
    /// The background jobs this turn reports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
}

/// `TurnOrigin::kind` of a turn reporting finished background delegations.
pub const ORIGIN_BACKGROUND: &str = "background";

/// Background delegation jobs a `tool.completed` output shows starting or
/// settling. `agent_*` calls with `background: true` return
/// `{"job", "status":"running", "background":true}`; `agent_wait` and
/// `agent_status` return job objects (or `{"jobs":[...]}`) whose status is no
/// longer `running` once they finish. Clients use this to keep a session open
/// while its jobs run, so the jobs are not cancelled with it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackgroundJobUpdate {
    pub started: Vec<String>,
    pub settled: Vec<String>,
}

pub fn background_job_update(output: &str) -> BackgroundJobUpdate {
    let mut update = BackgroundJobUpdate::default();
    let Ok(serde_json::Value::Object(value)) = serde_json::from_str::<serde_json::Value>(output)
    else {
        return update;
    };
    let mut visit = |job: &serde_json::Map<String, serde_json::Value>| {
        let (Some(id), Some(status)) = (
            job.get("job").and_then(serde_json::Value::as_str),
            job.get("status").and_then(serde_json::Value::as_str),
        ) else {
            return;
        };
        if status == "running" {
            if job.get("background").and_then(serde_json::Value::as_bool) == Some(true) {
                update.started.push(id.to_owned());
            }
        } else {
            update.settled.push(id.to_owned());
        }
    };
    visit(&value);
    for job in value
        .get("jobs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let serde_json::Value::Object(job) = job {
            visit(job);
        }
    }
    update
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "daemon.control")]
    DaemonControl {
        request_id: String,
        command: DaemonCommand,
    },
    #[serde(rename = "initialize")]
    Initialize {
        request_id: String,
        protocol_version: u32,
        client: PeerInfo,
    },
    #[serde(rename = "session.start")]
    SessionStart {
        request_id: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no_tools: Option<bool>,
        /// Delegation depth of the client, when it is itself a delegated
        /// agent (such as a nested SCV). Tools started from the session count
        /// from it, so the depth limit holds across processes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_depth: Option<u32>,
        /// The chat channel this session answers on, as its users name it
        /// (such as `WeChat` or `Feishu`). The user reads short plain-text
        /// replies there and never sees tool calls, so the server tells the
        /// model. At most [`MAX_CHANNEL_NAME_BYTES`], without control
        /// characters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// The client approves every approval request of this session
        /// without asking anyone. Background jobs, which outlive the turn
        /// that could carry their requests, then get the same answer;
        /// otherwise they get only what the approval policy grants unasked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_approve: Option<bool>,
    },
    #[serde(rename = "session.attach")]
    SessionAttach {
        request_id: String,
        session_id: String,
        cwd: String,
    },
    #[serde(rename = "turn.start")]
    TurnStart {
        request_id: String,
        session_id: String,
        prompt: String,
    },
    #[serde(rename = "queue.update")]
    QueueUpdate {
        request_id: String,
        session_id: String,
        queue_id: String,
        revision: u64,
        prompt: String,
    },
    #[serde(rename = "queue.move")]
    QueueMove {
        request_id: String,
        session_id: String,
        queue_id: String,
        revision: u64,
        before_queue_id: Option<String>,
    },
    #[serde(rename = "queue.remove")]
    QueueRemove {
        request_id: String,
        session_id: String,
        queue_id: String,
        revision: u64,
    },
    #[serde(rename = "session.pause")]
    SessionPause {
        request_id: String,
        session_id: String,
        paused: bool,
    },
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        request_id: String,
        session_id: String,
        turn_id: String,
    },
    #[serde(rename = "approval.resolve")]
    ApprovalResolve {
        request_id: String,
        session_id: String,
        approval_id: String,
        approved: bool,
    },
    #[serde(rename = "session.clear")]
    SessionClear {
        request_id: String,
        session_id: String,
    },
}

impl ServerEvent {
    /// The submitting request of a turn-scoped event, which identifies the
    /// turn to a client that has several turns' events interleaved.
    pub fn turn_request_id(&self) -> Option<&str> {
        match self {
            Self::QueueDequeued { request_id, .. }
            | Self::TurnStarted { request_id, .. }
            | Self::AssistantDelta { request_id, .. }
            | Self::AssistantCompleted { request_id, .. }
            | Self::ToolProposed { request_id, .. }
            | Self::ApprovalRequested { request_id, .. }
            | Self::ToolStarted { request_id, .. }
            | Self::ToolProgress { request_id, .. }
            | Self::ToolCompleted { request_id, .. }
            | Self::ContextCompacted { request_id, .. }
            | Self::TurnCompleted { request_id, .. }
            | Self::TurnCancelled { request_id, .. }
            | Self::TurnFailed { request_id, .. } => Some(request_id),
            _ => None,
        }
    }
}

impl ClientMessage {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Initialize { request_id, .. }
            | Self::DaemonControl { request_id, .. }
            | Self::SessionStart { request_id, .. }
            | Self::SessionAttach { request_id, .. }
            | Self::TurnStart { request_id, .. }
            | Self::QueueUpdate { request_id, .. }
            | Self::QueueMove { request_id, .. }
            | Self::QueueRemove { request_id, .. }
            | Self::SessionPause { request_id, .. }
            | Self::TurnCancel { request_id, .. }
            | Self::ApprovalResolve { request_id, .. }
            | Self::SessionClear { request_id, .. } => request_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerEvent {
    #[serde(rename = "daemon.status")]
    DaemonStatus {
        request_id: String,
        status: DaemonStatus,
    },
    #[serde(rename = "initialized")]
    Initialized {
        request_id: String,
        protocol_version: u32,
        server: PeerInfo,
    },
    #[serde(rename = "session.started")]
    SessionStarted {
        request_id: String,
        session_id: String,
        cwd: String,
        model: String,
        context_max_tokens: usize,
        max_server_frame_bytes: usize,
        max_transcript_bytes: usize,
        max_transcript_items: usize,
        max_prompt_history_bytes: usize,
        max_prompt_history_items: usize,
    },
    #[serde(rename = "queue.snapshot")]
    QueueSnapshot {
        request_id: Option<String>,
        session_id: String,
        seq: u64,
        entries: Vec<QueueEntry>,
        paused: bool,
    },
    #[serde(rename = "queue.enqueued")]
    QueueEnqueued {
        request_id: String,
        session_id: String,
        seq: u64,
        entry: QueueEntry,
        position: usize,
    },
    #[serde(rename = "queue.updated")]
    QueueUpdated {
        request_id: String,
        session_id: String,
        seq: u64,
        entry: QueueEntry,
    },
    #[serde(rename = "queue.moved")]
    QueueMoved {
        request_id: String,
        session_id: String,
        seq: u64,
        queue_id: String,
        position: usize,
        revision: u64,
    },
    #[serde(rename = "queue.removed")]
    QueueRemoved {
        request_id: String,
        session_id: String,
        seq: u64,
        queue_id: String,
        revision: u64,
    },
    #[serde(rename = "queue.dequeued")]
    QueueDequeued {
        request_id: String,
        session_id: String,
        seq: u64,
        queue_id: String,
        turn_id: String,
    },
    #[serde(rename = "session.paused")]
    SessionPaused {
        request_id: String,
        session_id: String,
        seq: u64,
        paused: bool,
    },
    #[serde(rename = "turn.started")]
    TurnStarted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    #[serde(rename = "assistant.delta")]
    AssistantDelta {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        content: String,
    },
    #[serde(rename = "assistant.completed")]
    AssistantCompleted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        content: String,
    },
    #[serde(rename = "tool.proposed")]
    ToolProposed {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        call_id: String,
        name: String,
        arguments: Value,
    },
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        approval_id: String,
        call_id: String,
        name: String,
        risk: String,
        cwd: String,
        summary: String,
    },
    #[serde(rename = "tool.started")]
    ToolStarted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        call_id: String,
        name: String,
    },
    /// Short status lines from a running tool, at most two events a second
    /// per call and 512 bytes each. Display only; not part of the history.
    #[serde(rename = "tool.progress")]
    ToolProgress {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        call_id: String,
        text: String,
    },
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        call_id: String,
        name: String,
        success: bool,
        output: String,
        truncated: bool,
    },
    #[serde(rename = "context.compacted")]
    ContextCompacted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        before_tokens: usize,
        after_tokens: usize,
        removed_messages: usize,
    },
    #[serde(rename = "session.trimmed")]
    SessionTrimmed {
        request_id: String,
        session_id: String,
        seq: u64,
        removed_messages: usize,
        history_bytes: usize,
    },
    #[serde(rename = "session.cleared")]
    SessionCleared {
        request_id: String,
        session_id: String,
        seq: u64,
    },
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        steps: usize,
        usage: Usage,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    #[serde(rename = "turn.cancelled")]
    TurnCancelled {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        code: String,
        message: String,
        /// Set when the server started this turn itself, such as to report
        /// finished background work; absent for a client's own `turn.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        code: String,
        message: String,
        fatal: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_round_trip() {
        let message = ClientMessage::TurnStart {
            request_id: "3".into(),
            session_id: "session".into(),
            prompt: "hello".into(),
        };
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"type\":\"turn.start\""));
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            message
        );
    }

    #[test]
    fn tool_progress_and_delegation_depth_round_trip() {
        let event = ServerEvent::ToolProgress {
            request_id: "r".into(),
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 4,
            call_id: "c".into(),
            text: "$ cargo test\nupdate …/src/lib.rs".into(),
        };
        let wire = serde_json::to_string(&event).unwrap();
        assert!(wire.contains(r#""type":"tool.progress""#));
        assert_eq!(serde_json::from_str::<ServerEvent>(&wire).unwrap(), event);

        let start = |depth| ClientMessage::SessionStart {
            request_id: "1".into(),
            cwd: "/w".into(),
            provider: None,
            model: None,
            base_url: None,
            no_tools: None,
            delegation_depth: depth,
            channel: None,
            auto_approve: None,
        };
        let nested = serde_json::to_string(&start(Some(2))).unwrap();
        assert!(nested.contains(r#""delegation_depth":2"#));
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&nested).unwrap(),
            start(Some(2))
        );
        // Omitted when unset, and optional on the wire.
        let direct = serde_json::to_string(&start(None)).unwrap();
        assert!(!direct.contains("delegation_depth"));
        let older = r#"{"type":"session.start","request_id":"1","cwd":"/w"}"#;
        assert_eq!(
            serde_json::from_str::<ClientMessage>(older).unwrap(),
            start(None)
        );
        assert_eq!(PROTOCOL_VERSION, 3);
    }

    #[test]
    fn chat_sessions_name_their_channel_and_approval_mode() {
        let chat = ClientMessage::SessionStart {
            request_id: "1".into(),
            cwd: "/w".into(),
            provider: None,
            model: None,
            base_url: None,
            no_tools: Some(false),
            delegation_depth: None,
            channel: Some("WeChat".into()),
            auto_approve: Some(true),
        };
        let wire = serde_json::to_string(&chat).unwrap();
        assert!(wire.contains(r#""channel":"WeChat""#), "{wire}");
        assert!(wire.contains(r#""auto_approve":true"#), "{wire}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&wire).unwrap(), chat);
        // Frames from older clients omit both.
        let older = r#"{"type":"session.start","request_id":"1","cwd":"/w"}"#;
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(older).unwrap(),
            ClientMessage::SessionStart {
                channel: None,
                auto_approve: None,
                ..
            }
        ));
    }

    #[test]
    fn restart_requests_and_scheduled_restarts_round_trip() {
        let request = DaemonCommand::RestartWhenIdle {
            version: Some("0.1.37".into()),
            commit: Some("abc1234".into()),
            parent: Some("0a1b2c3d/session/codex-3f9a2c".into()),
            max_wait_seconds: Some(600),
        };
        let wire = serde_json::to_string(&request).unwrap();
        assert!(wire.contains(r#""action":"restart_when_idle""#), "{wire}");
        assert_eq!(
            serde_json::from_str::<DaemonCommand>(&wire).unwrap(),
            request
        );
        assert_eq!(
            serde_json::from_str::<DaemonCommand>(r#"{"action":"restart_when_idle"}"#).unwrap(),
            DaemonCommand::RestartWhenIdle {
                version: None,
                commit: None,
                parent: None,
                max_wait_seconds: None,
            }
        );
        // Status from a daemon without a scheduled restart omits it, and
        // older status frames parse.
        let status: DaemonStatus =
            serde_json::from_str(r#"{"version":"0.1.0","pid":1,"components":[]}"#).unwrap();
        assert!(status.restart.is_none());
        assert!(!serde_json::to_string(&status).unwrap().contains("restart"));
    }

    #[test]
    fn additive_fields_are_ignored() {
        let json = r#"{"type":"session.clear","request_id":"1","session_id":"s","future":true}"#;
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(json).unwrap(),
            ClientMessage::SessionClear { .. }
        ));
    }

    #[test]
    fn event_round_trip() {
        let event = ServerEvent::AssistantDelta {
            request_id: "1".into(),
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 4,
            content: "hello".into(),
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerEvent>(&encoded).unwrap(),
            event
        );
    }

    #[test]
    fn queue_messages_and_events_round_trip() {
        let message = ClientMessage::QueueMove {
            request_id: "q1".into(),
            session_id: "s".into(),
            queue_id: "q".into(),
            revision: 2,
            before_queue_id: None,
        };
        let encoded = serde_json::to_string(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&encoded).unwrap(),
            message
        );
        let event = ServerEvent::QueueSnapshot {
            request_id: None,
            session_id: "s".into(),
            seq: 4,
            entries: vec![QueueEntry {
                queue_id: "q".into(),
                revision: 1,
                prompt: "hello".into(),
                submitter: "cli".into(),
            }],
            paused: false,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerEvent>(&encoded).unwrap(),
            event
        );
    }

    #[test]
    fn remote_tools_fields_are_additive() {
        let legacy: DaemonCommand = serde_json::from_str(
            r#"{"action":"channel_set","channel":"wechat","account":"a","enabled":true,"workspace":null}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy,
            DaemonCommand::ChannelSet {
                remote_tools: None,
                ..
            }
        ));
        let owner = DaemonCommand::ChannelSet {
            channel: "wechat".into(),
            account: "a".into(),
            enabled: true,
            workspace: None,
            remote_tools: Some(RemoteTools::Owner),
        };
        let encoded = serde_json::to_string(&owner).unwrap();
        assert!(encoded.contains(r#""remote_tools":"owner""#));
        assert_eq!(
            serde_json::from_str::<DaemonCommand>(&encoded).unwrap(),
            owner
        );
        let health: ComponentHealth = serde_json::from_str(
            r#"{"id":"clawbot:a","account":"a","bot_id":null,"user_id":null,"enabled":true,"state":"connected","last_success_unix_seconds":null,"error":null,"restarts":0}"#,
        )
        .unwrap();
        assert_eq!(health.remote_tools, RemoteTools::None);
        // Daemons before channels reported no channel.
        assert!(health.channel.is_empty());
    }

    #[test]
    fn delegation_control_round_trips_and_older_status_still_parses() {
        for (command, wire) in [
            (
                DaemonCommand::Delegations { all: true },
                r#"{"action":"delegations","all":true}"#,
            ),
            (
                DaemonCommand::DelegationKill {
                    handle: Some("codex-3f9a2c".into()),
                    orphans: false,
                },
                r#"{"action":"delegation_kill","handle":"codex-3f9a2c","orphans":false}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&command).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<DaemonCommand>(wire).unwrap(),
                command
            );
        }
        assert_eq!(
            serde_json::from_str::<DaemonCommand>(r#"{"action":"delegation_kill","orphans":true}"#)
                .unwrap(),
            DaemonCommand::DelegationKill {
                handle: None,
                orphans: true
            }
        );
        // A status from a daemon without delegation tracking.
        let status: DaemonStatus =
            serde_json::from_str(r#"{"version":"0.1.23","pid":7,"components":[]}"#).unwrap();
        assert_eq!(status.delegations, DelegationSummary::default());
    }

    #[test]
    fn server_started_turns_carry_their_origin_and_client_turns_omit_it() {
        let started = ServerEvent::TurnStarted {
            request_id: "background:1".into(),
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 4,
            origin: Some(TurnOrigin {
                kind: ORIGIN_BACKGROUND.into(),
                jobs: vec!["job-1".into()],
            }),
        };
        let json = serde_json::to_value(&started).unwrap();
        assert_eq!(
            json["origin"],
            serde_json::json!({"kind":"background","jobs":["job-1"]})
        );
        assert_eq!(
            serde_json::from_value::<ServerEvent>(json).unwrap(),
            started
        );
        assert_eq!(started.turn_request_id(), Some("background:1"));
        // A client's own turn has no origin on the wire, and older frames parse.
        let own: ServerEvent = serde_json::from_str(
            r#"{"type":"turn.completed","request_id":"r","session_id":"s","turn_id":"t","seq":9,"steps":1,"usage":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            own,
            ServerEvent::TurnCompleted { origin: None, .. }
        ));
        assert!(!serde_json::to_string(&own).unwrap().contains("origin"));
    }

    #[test]
    fn background_job_updates_come_from_start_wait_and_status_outputs() {
        let started = background_job_update(
            r#"{"job":"job-1","tool":"agent_codex","status":"running","background":true}"#,
        );
        assert_eq!(started.started, vec!["job-1".to_owned()]);
        assert!(started.settled.is_empty());
        // A running job listed by agent_status is neither started nor settled.
        let listed = background_job_update(
            r#"{"jobs":[{"job":"job-1","status":"running"},{"job":"job-2","status":"failed"}]}"#,
        );
        assert!(listed.started.is_empty());
        assert_eq!(listed.settled, vec!["job-2".to_owned()]);
        let waited = background_job_update(r#"{"job":"job-1","status":"completed"}"#);
        assert_eq!(waited.settled, vec!["job-1".to_owned()]);
        // Ordinary agent results and non-JSON output are no job updates.
        for other in [
            r#"{"agent":"codex","status":"completed"}"#,
            "plain text",
            "[1]",
        ] {
            assert_eq!(background_job_update(other), BackgroundJobUpdate::default());
        }
    }
}
