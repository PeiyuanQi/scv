//! Helpers every tool shares for its call arguments: parsing, validation,
//! timeouts, and bounded text for approval summaries.

use std::time::Duration;

use scv_core::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};

/// A tool call's arguments as `T`, or a model-facing error naming what failed.
pub(crate) fn parse_args<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, ToolError> {
    serde_json::from_value(value.clone())
        .map_err(|error| ToolError(format!("invalid arguments: {error}")))
}

/// A shell command or agent prompt must say something.
pub(crate) fn validate_process_args(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError("command or prompt must be non-empty".into()));
    }
    Ok(())
}

/// A process tool's default timeout and the ceiling a call may raise it to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    pub(crate) default: Duration,
    pub(crate) max: Duration,
}

impl Timeouts {
    /// The call's timeout: its own request up to the ceiling, else the
    /// default. A request above the ceiling is refused, never clamped, so the
    /// caller learns the limit instead of being cut off early.
    pub(crate) fn resolve(self, requested: Option<u64>) -> Result<Duration, ToolError> {
        match requested {
            None => Ok(self.default.min(self.max)),
            Some(0) => Err(ToolError("timeout_seconds must be positive".into())),
            Some(seconds) if seconds > self.max.as_secs() => Err(ToolError(format!(
                "timeout_seconds {seconds} exceeds the configured maximum of {} seconds \
                 (tools.max_timeout_seconds)",
                self.max.as_secs()
            ))),
            Some(seconds) => Ok(Duration::from_secs(seconds)),
        }
    }
}

pub(crate) fn timeout_schema(timeouts: Timeouts) -> Value {
    json!({
        "type":"integer",
        "minimum":1,
        "maximum":timeouts.max.as_secs(),
        "description":format!(
            "Seconds before the process is killed. Defaults to {}; at most {}. \
             Raise it for long work such as builds, releases, or landing a change.",
            timeouts.default.min(timeouts.max).as_secs(),
            timeouts.max.as_secs()
        )
    })
}

/// `value` cut to `max_chars` characters, with `…` when anything was cut.
pub(crate) fn bounded(value: &str, max_chars: usize) -> String {
    let mut output: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}
