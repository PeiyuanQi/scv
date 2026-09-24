//! Keeping the canonical session history within its configured limits.

use crate::{AgentError, CoreEvent, EventSink, Message};

#[derive(Debug, Clone)]
pub struct HistoryLimits {
    pub max_bytes: usize,
    pub max_messages: usize,
    pub note_max_chars: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_messages: 10_000,
            note_max_chars: 4_000,
        }
    }
}

/// Trim the oldest complete turns until `history` is within `limits`, keeping
/// one [`Message::HistoryNote`] in their place. The active turn (from the
/// newest user message on) is never trimmed: if it alone is over a limit, the
/// turn fails and the runtime rolls it back.
pub(crate) async fn enforce_limits(
    history: &mut Vec<Message>,
    limits: &HistoryLimits,
    sink: &dyn EventSink,
) -> Result<(), AgentError> {
    let mut total_removed = 0;
    while history.len() > limits.max_messages || history_bytes(history) > limits.max_bytes {
        let latest_user = history
            .iter()
            .rposition(|message| matches!(message, Message::User { .. }))
            .unwrap_or(0);
        let active = &history[latest_user..];
        if active.len() > limits.max_messages || history_bytes(active) > limits.max_bytes {
            return Err(AgentError::HistoryLimit(
                "active turn exceeds configured session history limit".into(),
            ));
        }
        let first_user = history
            .iter()
            .position(|message| matches!(message, Message::User { .. }))
            .unwrap_or(latest_user);
        if first_user == latest_user {
            if matches!(history.first(), Some(Message::HistoryNote { .. })) {
                history.remove(0);
                total_removed += 1;
                continue;
            }
            return Err(AgentError::HistoryLimit(
                "session history cannot be reduced within its configured limit".into(),
            ));
        }
        let end = history[first_user + 1..]
            .iter()
            .position(|message| matches!(message, Message::User { .. }))
            .map(|index| first_user + 1 + index)
            .ok_or_else(|| {
                AgentError::HistoryLimit(
                    "session history has no complete group available to trim".into(),
                )
            })?;
        let removed: Vec<Message> = history.drain(..end).collect();
        total_removed += removed.len();
        let note = Message::HistoryNote {
            content: summarize_history_trim(&removed, total_removed, limits.note_max_chars),
        };
        if matches!(history.first(), Some(Message::HistoryNote { .. })) {
            history.remove(0);
        }
        history.insert(0, note);
    }
    if total_removed > 0 {
        sink.emit(CoreEvent::SessionTrimmed {
            removed_messages: total_removed,
            history_bytes: history_bytes(history),
        })
        .await?;
    }
    Ok(())
}

fn history_bytes(history: &[Message]) -> usize {
    serde_json::to_vec(history).map_or(usize::MAX, |value| value.len())
}

fn summarize_history_trim(messages: &[Message], removed: usize, max_chars: usize) -> String {
    let mut note =
        format!("[SCV trimmed {removed} earlier canonical messages to enforce session limits.]\n");
    for message in messages {
        let (label, content) = match message {
            Message::User { content, .. } => ("user", content.as_str()),
            Message::Assistant { content, .. } => ("assistant", content.as_str()),
            Message::Tool {
                name,
                content,
                is_error,
                ..
            } => {
                let status = if *is_error { "failed" } else { "ok" };
                note.push_str(&format!("tool {name} ({status}): "));
                ("", content.as_str())
            }
            Message::HistoryNote { content } => ("earlier", content.as_str()),
        };
        if !label.is_empty() {
            note.push_str(label);
            note.push_str(": ");
        }
        note.push_str(&char_tail(content, 160).replace('\n', " "));
        note.push('\n');
        if note.chars().count() >= max_chars {
            break;
        }
    }
    truncate_chars(&note, max_chars)
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(crate) fn char_tail(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    value
        .chars()
        .skip(count.saturating_sub(max_chars))
        .collect()
}
