//! Dependency-light wire types shared by SCV clients and the server.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntry {
    pub queue_id: String,
    pub revision: u64,
    pub prompt: String,
    pub submitter: String,
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
    #[serde(rename = "initialize")]
    Initialize {
        request_id: String,
        protocol_version: u32,
        client: PeerInfo,
    },
    #[serde(rename = "session.start")]
    SessionStart { request_id: String, cwd: String },
    #[serde(rename = "session.attach")]
    SessionAttach { request_id: String, session_id: String, cwd: String },
    #[serde(rename = "turn.start")]
    TurnStart {
        request_id: String,
        session_id: String,
        prompt: String,
    },
    #[serde(rename = "queue.update")]
    QueueUpdate { request_id: String, session_id: String, queue_id: String, revision: u64, prompt: String },
    #[serde(rename = "queue.move")]
    QueueMove { request_id: String, session_id: String, queue_id: String, revision: u64, before_queue_id: Option<String> },
    #[serde(rename = "queue.remove")]
    QueueRemove { request_id: String, session_id: String, queue_id: String, revision: u64 },
    #[serde(rename = "session.pause")]
    SessionPause { request_id: String, session_id: String, paused: bool },
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

impl ClientMessage {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Initialize { request_id, .. }
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
    QueueSnapshot { request_id: Option<String>, session_id: String, seq: u64, entries: Vec<QueueEntry>, paused: bool },
    #[serde(rename = "queue.enqueued")]
    QueueEnqueued { request_id: String, session_id: String, seq: u64, entry: QueueEntry, position: usize },
    #[serde(rename = "queue.updated")]
    QueueUpdated { request_id: String, session_id: String, seq: u64, entry: QueueEntry },
    #[serde(rename = "queue.moved")]
    QueueMoved { request_id: String, session_id: String, seq: u64, queue_id: String, position: usize, revision: u64 },
    #[serde(rename = "queue.removed")]
    QueueRemoved { request_id: String, session_id: String, seq: u64, queue_id: String, revision: u64 },
    #[serde(rename = "queue.dequeued")]
    QueueDequeued { request_id: String, session_id: String, seq: u64, queue_id: String, turn_id: String },
    #[serde(rename = "session.paused")]
    SessionPaused { request_id: String, session_id: String, seq: u64, paused: bool },
    #[serde(rename = "turn.started")]
    TurnStarted {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
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
    },
    #[serde(rename = "turn.cancelled")]
    TurnCancelled {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed {
        request_id: String,
        session_id: String,
        turn_id: String,
        seq: u64,
        code: String,
        message: String,
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
            request_id: "q1".into(), session_id: "s".into(), queue_id: "q".into(),
            revision: 2, before_queue_id: None,
        };
        let encoded = serde_json::to_string(&message).unwrap();
        assert_eq!(serde_json::from_str::<ClientMessage>(&encoded).unwrap(), message);
        let event = ServerEvent::QueueSnapshot {
            request_id: None, session_id: "s".into(), seq: 4,
            entries: vec![QueueEntry { queue_id: "q".into(), revision: 1, prompt: "hello".into(), submitter: "cli".into() }],
            paused: false,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<ServerEvent>(&encoded).unwrap(), event);
    }
}
