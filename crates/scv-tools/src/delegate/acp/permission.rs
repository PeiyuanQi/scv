//! Mapping an agent's `session/request_permission` onto SCV's approval
//! gate and back.

use std::path::Path;

use scv_core::ToolRisk;
use serde_json::{Value, json};

use crate::{args::bounded, builtin::fs::is_secret_like, delegate::progress::redact};

/// The approval risk and summary for an agent's permission request. The
/// risk follows SCV's own tools: reads are read-only unless they touch a
/// secret-like path, edits are file-system work, and commands are processes.
pub(super) fn describe_permission(params: &Value, label: &str) -> (ToolRisk, String) {
    let call = params.get("toolCall").unwrap_or(&Value::Null);
    let kind = call.get("kind").and_then(Value::as_str).unwrap_or("other");
    let title = call
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("an unnamed tool call");
    let paths: Vec<&str> = call
        .get("locations")
        .and_then(Value::as_array)
        .map(|locations| {
            locations
                .iter()
                .filter_map(|location| location.get("path").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let secret = paths.iter().any(|path| is_secret_like(Path::new(path)));
    let risk = match kind {
        "read" | "search" | "think" if !secret => ToolRisk::ReadOnly,
        "read" | "search" | "edit" | "delete" | "move" => ToolRisk::Filesystem,
        "execute" => ToolRisk::Process,
        "fetch" => ToolRisk::Network,
        _ => ToolRisk::Delegate,
    };
    let title = redact(&title.replace(['\n', '\r'], " "));
    let mut summary = format!("{label} {} ({kind})", bounded(&title, 500));
    let unnamed: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|path| !title.contains(path))
        .collect();
    if !unnamed.is_empty() {
        summary.push_str(&format!(" on {}", bounded(&unnamed.join(", "), 500)));
    }
    (risk, summary)
}

/// The permission outcome for SCV's decision: allow or reject once, falling
/// back to the "always" option, and cancelling when the agent offers neither.
pub(super) fn choose_option(options: Option<&Value>, approved: bool) -> Value {
    let kinds: [&str; 2] = if approved {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    let options = options.and_then(Value::as_array);
    for kind in kinds {
        if let Some(option) = options
            .into_iter()
            .flatten()
            .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
            && let Some(id) = option.get("optionId")
        {
            return json!({"outcome": "selected", "optionId": id});
        }
    }
    json!({"outcome": "cancelled"})
}
