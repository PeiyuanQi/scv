//! Starting a chat session with the open episode of its chat log (see
//! [`scv_client::history`]), so a conversation carries on after its session
//! idled out or the daemon restarted.

use std::{path::Path, time::Duration};

use scv_client::history::{self, Entry, Role};
use scv_core::Message;

/// Most text an open episode brings into a new session; the newest messages
/// are kept.
const MAX_RELOAD_BYTES: usize = 64 * 1024;
/// Most text one reloaded message keeps.
const MAX_MESSAGE_BYTES: usize = MAX_RELOAD_BYTES / 4;

/// The history a new session for the chat logged in `dir` starts with: the
/// open episode's messages after a note saying where they come from, or
/// nothing when no episode is open. `tools` says whether the model can read
/// the rest of the log.
pub(crate) fn reload(dir: &Path, gap: Duration, now_ms: u64, tools: bool) -> Vec<Message> {
    match history::open_episode(dir, gap, now_ms) {
        Ok(Some(episode)) => messages(&episode.messages, tools),
        Ok(None) => Vec::new(),
        Err(error) => {
            tracing::warn!(error = %error, "could not reload the open episode of a chat log");
            Vec::new()
        }
    }
}

fn messages(entries: &[Entry], tools: bool) -> Vec<Message> {
    let mut kept = Vec::new();
    let mut bytes = 0;
    for entry in entries.iter().rev() {
        let size =
            entry.text.len().min(MAX_MESSAGE_BYTES) + entry.quote.len().min(MAX_MESSAGE_BYTES);
        if !kept.is_empty() && bytes + size > MAX_RELOAD_BYTES {
            break;
        }
        bytes += size;
        kept.push(entry);
    }
    kept.reverse();
    let left_out = entries.len() - kept.len();
    let mut note = String::from(
        "[SCV started a new session for this chat, so it reloaded the conversation so far \
         from its chat log: the messages that follow, SCV's own messages marked as such.",
    );
    if left_out > 0 {
        note.push_str(&format!(" {left_out} earlier messages are left out"));
        note.push_str(if tools {
            "; chat_history can read them."
        } else {
            "."
        });
    }
    note.push(']');
    let mut history = vec![Message::user(note)];
    for entry in kept {
        match entry.role {
            Role::Owner => history.push(Message::user(owner_text(entry))),
            Role::Scv => {
                if entry.report {
                    history.push(Message::user(
                        "[SCV background report: a background job finished, and SCV sent \
                         the user the report that follows]",
                    ));
                }
                history.push(Message::Assistant {
                    content: bounded(&entry.text),
                    tool_calls: Vec::new(),
                });
            }
            Role::System => history.push(Message::user(format!(
                "[SCV sent the user this message itself]\n{}",
                bounded(&entry.text)
            ))),
            Role::Unknown => {}
        }
    }
    history
}

/// An owner message as the model first saw it: what it quoted, its text,
/// notes about files that did not come in, and the files it carried.
fn owner_text(entry: &Entry) -> String {
    let mut parts: Vec<String> = [&entry.quote, &entry.text]
        .into_iter()
        .map(|part| bounded(part.trim()))
        .filter(|part| !part.is_empty())
        .collect();
    parts.extend(entry.notes.iter().cloned());
    if !entry.files.is_empty() {
        let files: Vec<String> = entry
            .files
            .iter()
            .map(|file| {
                if file.transcript.is_empty() {
                    format!("{} {}", file.kind, file.name)
                } else {
                    format!(
                        "{} {} (it says: \"{}\")",
                        file.kind, file.name, file.transcript
                    )
                }
            })
            .collect();
        parts.push(format!("[The user attached: {}]", files.join(", ")));
    }
    parts.join("\n\n")
}

fn bounded(text: &str) -> String {
    let kept = scv_client::text::utf8_prefix(text, MAX_MESSAGE_BYTES);
    if kept.len() < text.len() {
        format!("{kept}\n[cut]")
    } else {
        kept.to_owned()
    }
}

#[cfg(test)]
mod tests;
