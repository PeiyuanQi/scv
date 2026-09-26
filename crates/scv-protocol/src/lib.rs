//! Dependency-light wire types shared by SCV clients and the server.
//!
//! [`ClientMessage`] is everything a client sends and [`ServerEvent`]
//! everything the server answers, one JSON object per line ([`FrameDecoder`]
//! bounds each line). Additive fields, and new values of the enums that
//! parse unknown values as `Unknown` ([`ErrorCode`], [`ToolErrorKind`],
//! [`ServerEvent`]), keep [`PROTOCOL_VERSION`]; a breaking change bumps it.
//! This crate holds no runtime policy and does no I/O.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod attachment;
mod background;
mod client;
mod daemon;
mod error;
mod frame;
mod server;

use serde::{Deserialize, Serialize};

pub use attachment::{Attachment, ReplyAttachment, reply_attachment};
pub use background::{JobChange, JobStatus, OriginKind, TurnOrigin};
pub use client::ClientMessage;
pub use daemon::{
    ComponentHealth, ComponentState, DaemonCommand, DaemonStatus, DelegationInfo,
    DelegationSummary, RemoteTools, RestartInfo, Senders,
};
pub use error::{ErrorCode, ToolErrorKind};
pub use frame::{Frame, FrameDecoder, Overflow, Step, encode_frame, trim_line};
pub use server::ServerEvent;

/// The protocol version. Client and server must speak the same one.
pub const PROTOCOL_VERSION: u32 = 3;

/// The longest `session.start` channel name.
pub const MAX_CHANNEL_NAME_BYTES: usize = 32;
/// Files one `turn.start` may attach.
pub const MAX_TURN_ATTACHMENTS: usize = 16;
/// The tool a chat session's model calls to send a file with its reply.
pub const CHAT_ATTACH_TOOL: &str = "chat_attach";

/// A prompt waiting for the running turn to finish.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntry {
    /// Stable ID of this queued prompt.
    pub queue_id: String,
    /// Bumped on every change; edits must name the current revision.
    pub revision: u64,
    /// The prompt text.
    pub prompt: String,
    /// The client that queued it.
    pub submitter: String,
    /// Files the queued `turn.start` attached; they run with its prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A client's or server's name and version, exchanged at `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    /// Program name, such as `scv-tui`.
    pub name: String,
    /// Its release.
    pub version: String,
}

/// Tokens a turn used, as the provider reported them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Usage {
    /// Prompt tokens, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Answer tokens, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[cfg(test)]
mod tests;
