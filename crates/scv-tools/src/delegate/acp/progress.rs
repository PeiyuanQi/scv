//! Turning `session/update` notifications into the reply and short,
//! redacted progress lines.

use std::collections::HashMap;

use serde_json::Value;

use crate::{
    args::bounded,
    delegate::{
        progress::redact,
        scv::{LineProgress, Reply},
    },
};

/// Turns `session/update` notifications into the reply and progress lines.
/// Tool output never becomes progress: only titles, kinds, and plan steps.
#[derive(Default)]
pub(super) struct Progress {
    pub(super) lines: LineProgress,
    pub(super) titles: HashMap<String, String>,
    pub(super) plan: Option<String>,
}

impl Progress {
    pub(super) fn update(
        &mut self,
        params: &Value,
        reply: &mut Reply,
        sink: &scv_core::ProgressSink,
    ) {
        let Some(update) = params.get("update") else {
            return;
        };
        let text = |key: &str| update.get(key).and_then(Value::as_str);
        match text("sessionUpdate") {
            Some("agent_message_chunk") => {
                if update.pointer("/content/type").and_then(Value::as_str) == Some("text")
                    && let Some(chunk) = update.pointer("/content/text").and_then(Value::as_str)
                {
                    reply.delta(chunk);
                    self.lines.push(chunk, sink);
                }
            }
            Some("tool_call") => {
                self.lines.flush(sink);
                let title = text("title").unwrap_or_default();
                let kind = text("kind").unwrap_or("tool");
                if let Some(id) = text("toolCallId") {
                    self.titles.insert(id.to_owned(), title.to_owned());
                }
                sink.report(&line(kind, title));
            }
            Some("tool_call_update") => {
                if let (Some(id), Some(title)) = (text("toolCallId"), text("title")) {
                    self.titles.insert(id.to_owned(), title.to_owned());
                }
                if text("status") == Some("failed") {
                    let title = text("toolCallId")
                        .and_then(|id| self.titles.get(id))
                        .map_or("tool call", String::as_str);
                    sink.report(&format!("{} failed", line("", title)));
                }
            }
            Some("plan") => {
                let entries = update.get("entries").and_then(Value::as_array);
                let current = entries.and_then(|entries| {
                    entries
                        .iter()
                        .find(|entry| {
                            entry.get("status").and_then(Value::as_str) == Some("in_progress")
                        })
                        .or_else(|| {
                            entries.iter().find(|entry| {
                                entry.get("status").and_then(Value::as_str) == Some("pending")
                            })
                        })
                });
                if let Some(content) = current
                    .and_then(|entry| entry.get("content"))
                    .and_then(Value::as_str)
                {
                    let step = format!("plan: {}", line("", content));
                    if self.plan.as_deref() != Some(step.as_str()) {
                        sink.report(&step);
                        self.plan = Some(step);
                    }
                }
            }
            _ => {}
        }
    }
}

/// One short progress line: the redacted title, or the kind when untitled.
pub(super) fn line(kind: &str, title: &str) -> String {
    let title = title.split(['\n', '\r']).next().unwrap_or_default().trim();
    if title.is_empty() {
        kind.to_owned()
    } else {
        bounded(&redact(title), 160)
    }
}
