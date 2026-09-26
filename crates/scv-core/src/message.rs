//! What the model sees: the canonical history messages and the user's input.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
        /// Images that come with the text, for a model that accepts image
        /// input. History keeps their paths, not their bytes.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageInput>,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    HistoryNote {
        content: String,
    },
}

impl Message {
    /// A user message of plain text.
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            images: Vec::new(),
        }
    }

    /// Estimated context cost: the serialized size in tokens, plus a small
    /// per-message overhead and a fixed amount per image.
    pub(crate) fn estimated_tokens(&self, bytes_per_token: usize) -> usize {
        let bytes = serde_json::to_vec(self).map_or(0, |value| value.len());
        let images = match self {
            Self::User { images, .. } => images.len(),
            _ => 0,
        };
        bytes
            .div_ceil(bytes_per_token)
            .saturating_add(4)
            .saturating_add(images.saturating_mul(IMAGE_TOKENS))
    }
}

/// What one image costs in context, whatever its size: providers scale
/// images down to a bounded number of tiles.
pub(crate) const IMAGE_TOKENS: usize = 1600;

/// An image file shown to the model with a user message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageInput {
    /// Absolute path on the host; the provider reads it for each request, so
    /// an image removed since is replaced by a note.
    pub path: PathBuf,
    /// MIME type, such as `image/png`.
    pub mime: String,
}

/// The user's side of a turn: text, and images for a model that accepts them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnInput {
    pub text: String,
    pub images: Vec<ImageInput>,
}

impl From<String> for TurnInput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for TurnInput {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[cfg(test)]
mod tests;
