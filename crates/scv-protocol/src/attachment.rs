//! Files that travel with a turn or a reply.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CHAT_ATTACH_TOOL;

/// A file a client attaches to its turn, already saved on the daemon's host,
/// such as a photo or document a chat user sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    /// `image`, `audio`, `video`, `file`, or `sticker`.
    pub kind: String,
    /// Absolute path of a regular file on the daemon's host.
    pub path: String,
    /// The name the sender gave it; empty when the platform has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// MIME type, such as `image/png`; empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    /// Size in bytes.
    pub size: u64,
    /// What a voice message said, when the platform transcribed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// A file the model attached to its reply with [`CHAT_ATTACH_TOOL`], as the
/// tool's successful `tool.completed` output reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyAttachment {
    /// Absolute, symlink-free path of the file the tool checked.
    pub path: String,
    /// The name to show the recipient.
    pub name: String,
    /// MIME type, such as `application/pdf`; empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    /// Size in bytes.
    pub size: u64,
    /// Text sent with the file, if any.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caption: String,
}

/// The attachment a successful [`CHAT_ATTACH_TOOL`] call reports: its output
/// is `{"attached": {...}, ...}`. Anything else is `None`.
pub fn reply_attachment(tool: &str, success: bool, output: &str) -> Option<ReplyAttachment> {
    if tool != CHAT_ATTACH_TOOL || !success {
        return None;
    }
    let value: Value = serde_json::from_str(output).ok()?;
    serde_json::from_value(value.get("attached")?.clone()).ok()
}
