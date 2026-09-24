//! The Responses API's streamed event and error shapes, as SCV reads them.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseEvent {
    /// Empty when the provider names the event only on its `event:` line.
    #[serde(rename = "type", default)]
    pub(crate) event_type: String,
    pub(crate) delta: Option<String>,
    pub(crate) output_index: Option<usize>,
    pub(crate) item: Option<ResponseItem>,
    pub(crate) response: Option<ResponseSummary>,
    /// An `error` event carries its details either nested or at top level.
    pub(crate) error: Option<Value>,
    pub(crate) code: Option<Value>,
    pub(crate) message: Option<Value>,
    pub(crate) annotation: Option<Value>,
}
#[derive(Debug, Deserialize)]
pub(crate) struct ResponseSummary {
    pub(crate) usage: Option<ResponseUsage>,
    pub(crate) error: Option<Value>,
    pub(crate) incomplete_details: Option<IncompleteDetails>,
}
#[derive(Debug, Deserialize)]
pub(crate) struct IncompleteDetails {
    pub(crate) reason: Option<String>,
}

/// A provider error as OpenAI-compatible endpoints report it.
#[derive(Debug, Default)]
pub(crate) struct ErrorDetails {
    pub(crate) kind: Option<Value>,
    pub(crate) code: Option<Value>,
    pub(crate) message: Option<String>,
}

impl ErrorDetails {
    /// Accepts an error object or a bare string; other fields are ignored.
    pub(crate) fn from_value(value: &Value) -> Self {
        match value {
            Value::String(message) => Self {
                message: Some(message.clone()),
                ..Default::default()
            },
            Value::Object(object) => Self {
                kind: object.get("type").cloned(),
                code: object.get("code").cloned(),
                message: object.get("message").map(|message| match message {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                }),
            },
            _ => Self::default(),
        }
    }

    fn labels(&self) -> impl Iterator<Item = String> + '_ {
        [&self.code, &self.kind]
            .into_iter()
            .flatten()
            .filter_map(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .filter(|label| !label.is_empty())
    }

    pub(crate) fn describe(&self) -> String {
        let mut labels: Vec<String> = self.labels().collect();
        labels.dedup();
        let message = self
            .message
            .as_deref()
            .filter(|message| !message.trim().is_empty())
            .unwrap_or("no details");
        if labels.is_empty() {
            message.to_owned()
        } else {
            format!("{message} ({})", labels.join(", "))
        }
    }

    /// Overload, rate-limit, and server-side errors may clear on retry;
    /// request and policy errors will not.
    pub(crate) fn is_transient(&self) -> bool {
        const TRANSIENT: [&str; 5] = [
            "overload",
            "unavailable",
            "rate_limit",
            "server_error",
            "timeout",
        ];
        self.labels().any(|label| {
            let label = label.to_ascii_lowercase();
            TRANSIENT.iter().any(|needle| label.contains(needle))
        }) || self
            .message
            .as_deref()
            .is_some_and(|message| message.to_ascii_lowercase().contains("overloaded"))
    }
}

/// Reads a JSON error body such as `{"error":{"message":…}}`, optionally
/// behind a `data:` prefix, from bytes that are not a complete event stream.
pub(crate) fn error_details(bytes: &[u8]) -> Option<ErrorDetails> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let text = text
        .strip_prefix("data:")
        .map(str::trim_start)
        .unwrap_or(text);
    let value: Value = serde_json::from_str(text).ok()?;
    let details = ErrorDetails::from_value(value.get("error").unwrap_or(&value));
    (details.message.is_some() || details.code.is_some()).then_some(details)
}
#[derive(Debug, Deserialize)]
pub(crate) struct ResponseUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}
#[derive(Debug, Deserialize)]
pub(crate) struct ResponseItem {
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    pub(crate) _id: Option<String>,
    pub(crate) call_id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) content: Option<Value>,
}
