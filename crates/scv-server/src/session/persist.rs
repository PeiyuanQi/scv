//! Saved session queues: a session whose client asked (`persist_queue`)
//! keeps its queued prompts, and the start of the prompt its running turn
//! serves, in `<session_queues>/<session>.json` (mode 0600, replaced
//! atomically) while it runs.
//!
//! The file goes when the client ends the session. A daemon that stops
//! (a restart or a crash) leaves it, so a session started afterwards can
//! restore the prompts that never started (`session.start` `restore`),
//! paused. The prompt whose turn was running is never sent again: its
//! outcome is unknown, so SCV only names it.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use scv_client::Layout;
use scv_protocol::{QueueEntry, RestorableSession};
use scv_tools::delegation::ProcessIdentity;
use serde::{Deserialize, Serialize};

/// Longest saved queue file read.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Characters of a running turn's prompt kept, to name it.
const ACTIVE_CHARS: usize = 300;
/// Saved queues older than this are removed.
pub(crate) const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

/// One session's saved queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedQueue {
    pub(crate) v: u32,
    pub(crate) session: String,
    pub(crate) label: String,
    pub(crate) workspace: PathBuf,
    /// The SCV process that runs the session.
    pub(crate) owner: ProcessIdentity,
    pub(crate) saved_unix: u64,
    #[serde(default)]
    pub(crate) paused: bool,
    #[serde(default)]
    pub(crate) entries: Vec<QueueEntry>,
    /// The start of the prompt the running turn serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active: Option<String>,
}

impl SavedQueue {
    /// Nothing worth restoring.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.active.is_none()
    }
}

/// The saved queue of `session`, or of nobody when nothing waits.
pub(crate) fn save(layout: &Layout, queue: &SavedQueue) {
    if !valid_session(&queue.session) {
        return;
    }
    if queue.is_empty() {
        remove(layout, &queue.session);
        return;
    }
    let dir = layout.session_queues();
    let written = (|| -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let bytes = serde_json::to_vec(queue).map_err(std::io::Error::other)?;
        scv_client::fs::replace_private(&dir.join(format!("{}.json", queue.session)), &bytes)
    })();
    if let Err(error) = written {
        tracing::debug!(session = %queue.session, %error, "could not save a session's queue");
    }
}

/// Forget `session`'s saved queue.
pub(crate) fn remove(layout: &Layout, session: &str) {
    if valid_session(session) {
        let _ = std::fs::remove_file(layout.session_queues().join(format!("{session}.json")));
    }
}

/// `session`'s saved queue.
pub(crate) fn load(layout: &Layout, session: &str) -> Option<SavedQueue> {
    valid_session(session)
        .then(|| read(&layout.session_queues().join(format!("{session}.json"))))
        .flatten()
}

fn read(path: &Path) -> Option<SavedQueue> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    let queue: SavedQueue = serde_json::from_slice(&bytes).ok()?;
    (path.file_stem()?.to_str()? == queue.session).then_some(queue)
}

/// Whether `queue` belongs to a session that stopped with its process: the
/// only kind a new session may restore.
pub(crate) fn stopped(queue: &SavedQueue) -> bool {
    !queue.owner.is_alive()
}

/// Every saved queue of a session that stopped, newest first, removing
/// ones older than [`KEEP`].
pub(crate) fn restorable(layout: &Layout) -> Vec<RestorableSession> {
    let Ok(entries) = std::fs::read_dir(layout.session_queues()) else {
        return Vec::new();
    };
    let now = unix_now();
    let mut found = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Some(queue) = read(&entry.path()) else {
            continue;
        };
        if !stopped(&queue) {
            continue;
        }
        if now.saturating_sub(queue.saved_unix) > KEEP.as_secs() {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        found.push(RestorableSession {
            id: queue.session,
            label: queue.label,
            workspace: queue.workspace.display().to_string(),
            queued: u32::try_from(queue.entries.len()).unwrap_or(u32::MAX),
            interrupted: queue.active,
            saved_unix_seconds: queue.saved_unix,
        });
    }
    found.sort_by_key(|session| std::cmp::Reverse(session.saved_unix_seconds));
    found
}

/// The start of `prompt`, to name a running turn.
pub(crate) fn active_line(prompt: &str) -> String {
    let mut line: String = prompt.chars().take(ACTIVE_CHARS).collect();
    if prompt.chars().count() > ACTIVE_CHARS {
        line.push('…');
    }
    line
}

/// Session IDs are UUIDs: letters, digits, and `-`.
pub(crate) fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 64
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
