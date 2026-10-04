//! The action audit log, `state/mail/<account>/audit.jsonl` (mode `0600`).
//!
//! Every write of the ledger that changes an action appends one JSON line
//! per change: when, the action's ID and kind, what happened (the state it
//! reached, or a new code), the first eight hex digits of its digest, and a
//! detail that is only ever a mail chat's name or an outcome code. It never
//! holds a code, a handle, an address, a subject, or any mail text. The
//! lines are derived from the state before and after the write, so no
//! transition can skip them. They are synced to `audit.jsonl.pending`
//! before the state file is replaced, then folded into the log. A crash
//! between the two leaves the journal: the next start appends it when the
//! state file is the one it names, and drops it when an older state is
//! still on disk. The janitor drops lines older than `retention.audit_days`
//! and keeps the file within `retention.max_audit_kib`.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::Path;

use super::ledger::MailState;

/// One change to one action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub(crate) at: u64,
    pub(crate) id: String,
    pub(crate) kind: String,
    /// `proposed`, a state it reached (`previewing`, `open`, `approved`,
    /// `executing`, `done`, …), or `reissued`.
    pub(crate) event: String,
    pub(crate) generation: u32,
    pub(crate) digest: String,
    /// The mail chat it was approved in, or its outcome code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

/// The changes to actions between `before` and `after`.
pub(crate) fn changes(before: &MailState, after: &MailState, now: u64) -> Vec<Line> {
    let mut lines = Vec::new();
    for entry in &after.actions {
        let old = before.actions.iter().find(|old| old.id == entry.id);
        let line = |event: &str, detail: Option<String>| Line {
            at: now,
            id: entry.id.clone(),
            kind: entry.kind.name().to_owned(),
            event: event.to_owned(),
            generation: entry.generation,
            digest: entry.digest.chars().take(8).collect(),
            detail,
        };
        let Some(old) = old else {
            lines.push(line("proposed", None));
            continue;
        };
        if old.generation != entry.generation {
            lines.push(line("reissued", None));
        }
        if old.state != entry.state {
            let detail = match entry.state {
                super::ledger::ActionState::Approved => entry
                    .approval
                    .as_ref()
                    .map(|approval| approval.route.clone()),
                _ => entry.outcome.as_ref().map(|outcome| {
                    serde_json::to_value(outcome.code)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default()
                }),
            };
            lines.push(line(entry.state.name(), detail.filter(|d| !d.is_empty())));
        }
    }
    lines
}

/// The journal beside the audit log: `audit.jsonl.pending`.
pub(crate) fn pending_path(audit: &Path) -> std::path::PathBuf {
    let mut name = audit.file_name().unwrap_or_default().to_os_string();
    name.push(".pending");
    audit.with_file_name(name)
}

/// The lines of one state write, named by the digest of the state text
/// that write commits. Synced before the state file is replaced.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    state: String,
    lines: Vec<Line>,
}

/// Record `lines` for the state text `state_text` and sync them. An empty
/// `lines` records nothing.
pub(crate) fn stage(audit: &Path, state_text: &str, lines: &[Line]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let pending = Pending {
        state: super::parse::digest(state_text.as_bytes()),
        lines: lines.to_vec(),
    };
    let bytes = serde_json::to_vec(&pending).context("could not record the mail audit journal")?;
    scv_client::fs::replace_private(&pending_path(audit), &bytes)
        .context("could not record the mail audit journal")
}

/// Append `lines` to the audit log when they are not already its tail, then
/// drop the journal.
pub(crate) fn commit_pending(audit: &Path, lines: &[Line]) -> Result<()> {
    if !lines.is_empty() && !ends_with(audit, lines)? {
        append(audit, lines)?;
    }
    remove_pending(&pending_path(audit))
}

/// Fold a journal left by a crash. When the state file at `state_file` is
/// the one the journal names, its lines are appended. When it is not, the
/// transition never landed and the journal is dropped. A journal that
/// cannot be read is left in place and this fails, so the lines are not
/// thrown away.
pub(crate) fn reconcile(audit: &Path, state_file: &Path) -> Result<()> {
    let path = pending_path(audit);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let pending: Pending =
        serde_json::from_slice(&bytes).context("could not read the mail audit journal")?;
    if ends_with(audit, &pending.lines)? {
        return remove_pending(&path);
    }
    let state = match std::fs::read(state_file) {
        Ok(state) => state,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    if pending.state == super::parse::digest(&state) {
        append(audit, &pending.lines)?;
    }
    remove_pending(&path)
}

fn ends_with(audit: &Path, lines: &[Line]) -> Result<bool> {
    if lines.is_empty() {
        return Ok(true);
    }
    let text = match std::fs::read_to_string(audit) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut parsed = Vec::new();
    for line in text.lines() {
        parsed.push(
            serde_json::from_str::<Line>(line).context("a mail audit line could not be read")?,
        );
    }
    Ok(parsed.len() >= lines.len() && parsed[parsed.len() - lines.len()..] == *lines)
}

fn remove_pending(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                scv_client::fs::sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Append `lines` by replacing the log atomically. A crash must leave either
/// the old tail or the whole new tail, never a partial line or batch that
/// prevents journal reconciliation or duplicates a prefix of the batch.
pub(crate) fn append(path: &Path, lines: &[Line]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut bytes = Vec::new();
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(mut file) => {
            file.read_to_end(&mut bytes)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("could not open the mail audit log"),
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(&serde_json::to_string(line)?);
        text.push('\n');
    }
    bytes.extend_from_slice(text.as_bytes());
    scv_client::fs::replace_private(path, &bytes)?;
    Ok(())
}

/// Keep only the lines of the log at `path` from the last `max_age`
/// seconds before `now`, and at most `max_bytes` of the newest of them.
/// Returns how many lines were dropped.
pub(crate) fn prune(path: &Path, now: u64, max_age: u64, max_bytes: usize) -> Result<usize> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| {
            serde_json::from_str::<Line>(line)
                .is_ok_and(|parsed| now.saturating_sub(parsed.at) < max_age)
        })
        .collect();
    let mut size: usize = kept.iter().map(|line| line.len() + 1).sum();
    while size > max_bytes && !kept.is_empty() {
        size -= kept.remove(0).len() + 1;
    }
    let dropped = lines.len() - kept.len();
    if dropped > 0 {
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        scv_client::fs::replace_private(path, out.as_bytes())?;
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests;
