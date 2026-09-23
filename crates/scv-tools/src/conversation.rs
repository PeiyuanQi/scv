//! Multi-turn conversations with delegated agents.
//!
//! A session's conversations live in one [`ConversationStore`] shared by its
//! `agent_*` tools, and end with the session. The model sees only SCV-issued
//! handles such as `codex-2`; the CLI's own session IDs stay here and are
//! never accepted from the model. While a conversation exists, a marker file
//! named after its session ID tells `scv agents gc` to keep its transcript.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use scv_core::ToolError;
use serde::{Deserialize, Serialize};

use crate::{
    adapters::ConversationFiles,
    agent_output::valid_session_id,
    delegation::{ProcessIdentity, write_private_json},
};

/// Transcripts younger than this are never removed, so a turn that is still
/// running (and so still writing its transcript) is safe from `gc`.
pub const MIN_GC_AGE: Duration = Duration::from_secs(3600);

/// Longest handle accepted from the model.
const MAX_HANDLE_BYTES: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct ConversationLimits {
    /// Conversations remembered per session; starting another forgets the
    /// least recently used idle one.
    pub max: usize,
    /// A conversation unused this long is forgotten.
    pub idle: Duration,
}

#[derive(Debug)]
struct Conversation {
    agent: String,
    cwd: PathBuf,
    /// The CLI's session ID, once known.
    vendor: Option<String>,
    turns: u32,
    busy: bool,
    last_used: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    conversations: HashMap<String, Conversation>,
    /// Next handle number per agent.
    next: HashMap<String, u32>,
}

/// One session's conversations with its delegated agents.
#[derive(Debug)]
pub struct ConversationStore {
    limits: ConversationLimits,
    marker_dir: Option<PathBuf>,
    owner: Option<ProcessIdentity>,
    inner: Mutex<Inner>,
}

/// On-disk marker keeping a live conversation's transcript from `gc`.
#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    owner: ProcessIdentity,
    agent: String,
    handle: String,
}

/// A turn in progress. Finish it with [`TurnGuard::finish`]; dropping it
/// instead (a cancelled or abandoned call) frees the conversation.
#[derive(Debug)]
pub(crate) struct TurnGuard {
    store: Arc<ConversationStore>,
    pub handle: String,
    pub turn: u32,
    /// Continuing: the known session ID. Starting: the ID SCV chose, if any.
    pub vendor: Option<String>,
    finished: bool,
}

/// Whether `value` has the shape of an SCV conversation handle
/// (`<agent>-<number>`). CLI session IDs never do.
pub fn is_handle(value: &str) -> bool {
    value.len() <= MAX_HANDLE_BYTES
        && value.split_once('-').is_some_and(|(agent, number)| {
            !agent.is_empty()
                && agent
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
        })
}

impl ConversationStore {
    /// `marker_dir` is `$SCV_HOME/run/conversations`; `None` keeps no markers.
    pub fn new(limits: ConversationLimits, marker_dir: Option<PathBuf>) -> Self {
        Self {
            limits,
            marker_dir,
            owner: ProcessIdentity::current(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Start a turn: a new conversation when `handle` is `None`, otherwise
    /// the next turn of that conversation. `assign_id` has SCV choose the
    /// CLI's session ID for a new conversation.
    pub(crate) fn begin(
        self: &Arc<Self>,
        agent: &str,
        handle: Option<&str>,
        cwd: &Path,
        assign_id: bool,
    ) -> Result<TurnGuard, ToolError> {
        let mut inner = self.inner.lock().expect("conversation lock");
        let now = Instant::now();
        let expired: Vec<String> = inner
            .conversations
            .iter()
            .filter(|(_, conversation)| {
                !conversation.busy && now.duration_since(conversation.last_used) >= self.limits.idle
            })
            .map(|(handle, _)| handle.clone())
            .collect();
        for handle in &expired {
            if let Some(conversation) = inner.conversations.remove(handle) {
                self.remove_marker(conversation.vendor.as_deref());
            }
        }
        let Some(handle) = handle else {
            return self.start(&mut inner, agent, cwd, assign_id, now);
        };
        if !is_handle(handle) {
            return Err(ToolError(format!(
                "session {:?} is not a conversation handle; pass the `session` value an \
                 earlier {agent} call returned, or omit it to start a new conversation",
                crate::bounded(handle, 80)
            )));
        }
        let Some(conversation) = inner.conversations.get_mut(handle) else {
            let reason = if expired.iter().any(|expired| expired == handle) {
                format!(
                    "was forgotten after {} seconds idle",
                    self.limits.idle.as_secs()
                )
            } else {
                "is unknown in this session".to_owned()
            };
            return Err(ToolError(format!(
                "conversation {handle} {reason}; omit session to start a new conversation"
            )));
        };
        if conversation.agent != agent {
            return Err(ToolError(format!(
                "conversation {handle} belongs to agent_{}, not agent_{agent}",
                conversation.agent
            )));
        }
        if conversation.busy {
            return Err(ToolError(format!(
                "session busy: conversation {handle} is still running a turn"
            )));
        }
        if conversation.cwd != cwd {
            return Err(ToolError(format!(
                "conversation {handle} runs in {:?}; continue it there or omit session to \
                 start a new conversation in {:?}",
                conversation.cwd, cwd
            )));
        }
        let Some(vendor) = conversation.vendor.clone() else {
            return Err(ToolError(format!(
                "conversation {handle} cannot be continued: the agent reported no session"
            )));
        };
        conversation.busy = true;
        conversation.last_used = now;
        Ok(TurnGuard {
            store: Arc::clone(self),
            handle: handle.to_owned(),
            turn: conversation.turns + 1,
            vendor: Some(vendor),
            finished: false,
        })
    }

    fn start(
        self: &Arc<Self>,
        inner: &mut Inner,
        agent: &str,
        cwd: &Path,
        assign_id: bool,
        now: Instant,
    ) -> Result<TurnGuard, ToolError> {
        while inner.conversations.len() >= self.limits.max.max(1) {
            let Some(oldest) = inner
                .conversations
                .iter()
                .filter(|(_, conversation)| !conversation.busy)
                .min_by_key(|(_, conversation)| conversation.last_used)
                .map(|(handle, _)| handle.clone())
            else {
                return Err(ToolError(format!(
                    "all {} conversations of this session are running a turn",
                    inner.conversations.len()
                )));
            };
            if let Some(conversation) = inner.conversations.remove(&oldest) {
                self.remove_marker(conversation.vendor.as_deref());
            }
        }
        let number = inner.next.entry(agent.to_owned()).or_insert(0);
        *number += 1;
        let handle = format!("{agent}-{number}");
        let vendor = assign_id.then(|| uuid::Uuid::new_v4().to_string());
        inner.conversations.insert(
            handle.clone(),
            Conversation {
                agent: agent.to_owned(),
                cwd: cwd.to_owned(),
                vendor: None,
                turns: 0,
                busy: true,
                last_used: now,
            },
        );
        if let Some(vendor) = &vendor {
            self.write_marker(vendor, agent, &handle);
        }
        Ok(TurnGuard {
            store: Arc::clone(self),
            handle,
            turn: 1,
            vendor,
            finished: false,
        })
    }

    fn write_marker(&self, vendor: &str, agent: &str, handle: &str) {
        let (Some(dir), Some(owner)) = (&self.marker_dir, self.owner) else {
            return;
        };
        let marker = Marker {
            owner,
            agent: agent.to_owned(),
            handle: handle.to_owned(),
        };
        // A missing marker only makes `gc` less careful; it never fails a turn.
        let _ = write_private_json(dir, &format!("{vendor}.json"), &marker);
    }

    fn remove_marker(&self, vendor: Option<&str>) {
        if let (Some(dir), Some(vendor)) = (&self.marker_dir, vendor) {
            let _ = std::fs::remove_file(dir.join(format!("{vendor}.json")));
        }
    }

    /// Handles this session remembers, for tests and diagnostics.
    pub fn handles(&self) -> Vec<String> {
        let mut handles: Vec<_> = self
            .inner
            .lock()
            .expect("conversation lock")
            .conversations
            .keys()
            .cloned()
            .collect();
        handles.sort();
        handles
    }
}

impl Drop for ConversationStore {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().expect("conversation lock");
        let vendors: Vec<_> = inner
            .conversations
            .values()
            .filter_map(|conversation| conversation.vendor.clone())
            .collect();
        for vendor in vendors {
            self.remove_marker(Some(&vendor));
        }
    }
}

impl TurnGuard {
    /// Settle the turn. `reported` is the session ID the CLI printed. Returns
    /// the handle when the conversation can be continued; a first turn that
    /// failed without the CLI reporting a session is forgotten.
    pub(crate) fn finish(mut self, reported: Option<String>, completed: bool) -> Option<String> {
        self.finished = true;
        let reported = reported.filter(|id| valid_session_id(id));
        let mut inner = self.store.inner.lock().expect("conversation lock");
        let first = self.turn == 1;
        if first && reported.is_none() && !completed {
            inner.conversations.remove(&self.handle);
            drop(inner);
            self.store.remove_marker(self.vendor.as_deref());
            return None;
        }
        let vendor = reported.or_else(|| self.vendor.clone());
        let conversation = inner.conversations.get_mut(&self.handle)?;
        conversation.busy = false;
        conversation.turns = self.turn;
        conversation.last_used = Instant::now();
        let previous = std::mem::replace(&mut conversation.vendor, vendor.clone());
        let agent = conversation.agent.clone();
        drop(inner);
        if previous != vendor {
            self.store.remove_marker(previous.as_deref());
        }
        match vendor {
            Some(vendor) => {
                self.store.write_marker(&vendor, &agent, &self.handle);
                Some(self.handle.clone())
            }
            None => None,
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut inner = self.store.inner.lock().expect("conversation lock");
        if self.turn == 1 {
            inner.conversations.remove(&self.handle);
            drop(inner);
            self.store.remove_marker(self.vendor.as_deref());
        } else if let Some(conversation) = inner.conversations.get_mut(&self.handle) {
            conversation.busy = false;
        }
    }
}

/// What `scv agents gc` found for one agent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// Transcripts removed, or that would be with `dry_run`.
    pub removed: Vec<PathBuf>,
    pub bytes: u64,
    /// Old transcripts kept because a live conversation still uses them.
    pub kept_live: usize,
}

/// Remove transcripts under `adapter_home` last modified at least
/// `older_than` ago, keeping any whose name carries the session ID of a live
/// conversation (a marker in `marker_dir` whose owning process runs).
/// Symlinks are never followed. Markers of dead owners are removed unless
/// `dry_run`.
pub fn collect_garbage(
    adapter_home: &Path,
    files: ConversationFiles,
    marker_dir: &Path,
    older_than: Duration,
    dry_run: bool,
) -> std::io::Result<GcReport> {
    let older_than = older_than.max(MIN_GC_AGE);
    let live = live_sessions(marker_dir, dry_run);
    let root = adapter_home.join(files.dir);
    let mut report = GcReport::default();
    let Some(cutoff) = SystemTime::now().checked_sub(older_than) else {
        return Ok(report);
    };
    let mut transcripts = Vec::new();
    walk(&root, files.extension, &mut transcripts)?;
    transcripts.sort();
    for (path, modified, bytes) in transcripts {
        if modified > cutoff {
            continue;
        }
        let relative = path.strip_prefix(&root).unwrap_or(&path);
        let in_use = relative.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            live.iter().any(|id| name.contains(id.as_str()))
        });
        if in_use {
            report.kept_live += 1;
            continue;
        }
        if !dry_run {
            std::fs::remove_file(&path)?;
        }
        report.bytes += bytes;
        report.removed.push(path);
    }
    if !dry_run {
        remove_empty_dirs(&root, &root);
    }
    Ok(report)
}

/// Remove markers whose owning SCV process no longer runs, returning how
/// many were removed. A short-lived `scv exec` leaves its markers behind when
/// it exits; the daemon's reconcile pass clears them.
pub fn remove_stale_markers(marker_dir: &Path) -> usize {
    let before = marker_count(marker_dir);
    let live = live_sessions(marker_dir, false).len();
    before.saturating_sub(live)
}

fn marker_count(marker_dir: &Path) -> usize {
    std::fs::read_dir(marker_dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".json"))
                    .is_some_and(valid_session_id)
            })
            .count()
    })
}

/// Session IDs of conversations whose owning SCV process still runs.
fn live_sessions(marker_dir: &Path, dry_run: bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(marker_dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
            .filter(|id| valid_session_id(id))
            .map(str::to_owned)
        else {
            continue;
        };
        let alive = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
            .is_some_and(|marker| marker.owner.is_alive());
        if alive {
            live.push(id);
        } else if !dry_run {
            let _ = std::fs::remove_file(&path);
        }
    }
    live
}

fn walk(
    dir: &Path,
    extension: &str,
    found: &mut Vec<(PathBuf, SystemTime, u64)>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            walk(&entry.path(), extension, found)?;
        } else if metadata.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|actual| actual == extension)
        {
            found.push((entry.path(), metadata.modified()?, metadata.len()));
        }
    }
    Ok(())
}

/// Remove directories below `root` left empty, deepest first.
fn remove_empty_dirs(dir: &Path, root: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if std::fs::symlink_metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir()) {
            remove_empty_dirs(&entry.path(), root);
        }
    }
    if dir != root {
        // Fails, harmlessly, unless the directory is empty.
        let _ = std::fs::remove_dir(dir);
    }
}

/// Parse an age such as `30d`, `12h`, `90m`, `45s`, or plain seconds.
pub fn parse_age(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, unit) = match value.find(|c: char| !c.is_ascii_digit()) {
        Some(index) => value.split_at(index),
        None => (value, "s"),
    };
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid age {value:?}; use a number with s, m, h, or d"))?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => {
            return Err(format!(
                "invalid age {value:?}; use a number with s, m, h, or d"
            ));
        }
    };
    number
        .checked_mul(unit)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("age {value:?} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(max: usize, idle: Duration, markers: Option<&Path>) -> Arc<ConversationStore> {
        Arc::new(ConversationStore::new(
            ConversationLimits { max, idle },
            markers.map(Path::to_path_buf),
        ))
    }

    const DAY: Duration = Duration::from_secs(86400);

    #[test]
    fn handles_are_issued_per_agent_and_vendor_ids_are_not_handles() {
        assert!(is_handle("codex-2"));
        assert!(is_handle("pi-10"));
        for not_handle in [
            "01a0cd5a-7195-7b31-a503-e235d5da7b45",
            "b514bbf5-a5b7-4bbe-83f9-5824ab41c35c",
            "codex",
            "codex-",
            "-1",
            "Codex-1",
            "codex-1a",
            "../codex-1",
        ] {
            assert!(!is_handle(not_handle), "{not_handle}");
        }
        let store = store(8, DAY, None);
        let cwd = Path::new("/w");
        let first = store.begin("codex", None, cwd, false).unwrap();
        assert_eq!((first.handle.as_str(), first.turn), ("codex-1", 1));
        assert_eq!(first.vendor, None);
        assert_eq!(
            first.finish(Some("t-1".into()), true).as_deref(),
            Some("codex-1")
        );
        let claude = store.begin("claude", None, cwd, true).unwrap();
        assert_eq!(claude.handle, "claude-1");
        assert!(claude.vendor.is_some());
        claude.finish(None, true);
        let second = store.begin("codex", None, cwd, false).unwrap();
        assert_eq!(second.handle, "codex-2");
    }

    #[test]
    fn continuing_pins_agent_and_cwd_and_counts_turns() {
        let store = store(8, DAY, None);
        let cwd = Path::new("/w/scv");
        let turn = store.begin("codex", None, cwd, false).unwrap();
        turn.finish(Some("thread-a".into()), true);
        let next = store.begin("codex", Some("codex-1"), cwd, false).unwrap();
        assert_eq!(next.turn, 2);
        assert_eq!(next.vendor.as_deref(), Some("thread-a"));
        // One turn at a time.
        let busy = store
            .begin("codex", Some("codex-1"), cwd, false)
            .unwrap_err();
        assert!(busy.0.starts_with("session busy"), "{}", busy.0);
        next.finish(Some("thread-a".into()), true);
        let moved = store
            .begin("codex", Some("codex-1"), Path::new("/w/other"), false)
            .unwrap_err();
        assert!(moved.0.contains("runs in"), "{}", moved.0);
        let other_agent = store
            .begin("claude", Some("codex-1"), cwd, false)
            .unwrap_err();
        assert!(
            other_agent.0.contains("belongs to agent_codex"),
            "{}",
            other_agent.0
        );
        let third = store.begin("codex", Some("codex-1"), cwd, false).unwrap();
        assert_eq!(third.turn, 3);
    }

    #[test]
    fn vendor_ids_and_unknown_handles_are_rejected() {
        let store = store(8, DAY, None);
        let cwd = Path::new("/w");
        store
            .begin("codex", None, cwd, false)
            .unwrap()
            .finish(Some("01a0cd5a-7195-7b31".into()), true);
        let vendor = store
            .begin("codex", Some("01a0cd5a-7195-7b31"), cwd, false)
            .unwrap_err();
        assert!(
            vendor.0.contains("not a conversation handle"),
            "{}",
            vendor.0
        );
        let unknown = store
            .begin("codex", Some("codex-9"), cwd, false)
            .unwrap_err();
        assert!(
            unknown.0.contains("unknown in this session"),
            "{}",
            unknown.0
        );
        // Another session's store knows nothing of this one's handles.
        let other = super::tests::store(8, DAY, None);
        assert!(other.begin("codex", Some("codex-1"), cwd, false).is_err());
    }

    #[test]
    fn timed_out_turns_stay_resumable_but_failed_first_turns_are_forgotten() {
        let store = store(8, DAY, None);
        let cwd = Path::new("/w");
        // A first turn that timed out after the CLI reported its session.
        let turn = store.begin("codex", None, cwd, false).unwrap();
        assert_eq!(
            turn.finish(Some("t".into()), false).as_deref(),
            Some("codex-1")
        );
        assert_eq!(
            store
                .begin("codex", Some("codex-1"), cwd, false)
                .unwrap()
                .turn,
            2
        );
        // A first turn that failed before the CLI reported anything.
        let failed = store.begin("codex", None, cwd, false).unwrap();
        assert_eq!(failed.finish(None, false), None);
        assert!(!store.handles().contains(&"codex-2".to_owned()));
        // An abandoned first turn is forgotten; an abandoned later turn frees the conversation.
        drop(store.begin("codex", None, cwd, false).unwrap());
        assert_eq!(store.handles(), vec!["codex-1".to_owned()]);
    }

    #[test]
    fn limits_forget_the_least_recently_used_and_idle_conversations() {
        let store = store(2, DAY, None);
        let cwd = Path::new("/w");
        for id in ["a", "b"] {
            store
                .begin("codex", None, cwd, false)
                .unwrap()
                .finish(Some(id.into()), true);
        }
        // codex-1 was used more recently than codex-2.
        store
            .begin("codex", Some("codex-1"), cwd, false)
            .unwrap()
            .finish(Some("a".into()), true);
        store
            .begin("codex", None, cwd, false)
            .unwrap()
            .finish(Some("c".into()), true);
        assert_eq!(
            store.handles(),
            vec!["codex-1".to_owned(), "codex-3".to_owned()]
        );
        // Every remembered conversation busy: no room for another.
        let busy_a = store.begin("codex", Some("codex-1"), cwd, false).unwrap();
        let busy_b = store.begin("codex", Some("codex-3"), cwd, false).unwrap();
        assert!(store.begin("codex", None, cwd, false).is_err());
        drop((busy_a, busy_b));

        let idle = super::tests::store(8, Duration::ZERO, None);
        idle.begin("pi", None, cwd, true)
            .unwrap()
            .finish(None, true);
        let expired = idle.begin("pi", Some("pi-1"), cwd, true).unwrap_err();
        assert!(
            expired.0.contains("forgotten after 0 seconds idle"),
            "{}",
            expired.0
        );
    }

    #[test]
    fn markers_follow_the_conversation_and_gc_keeps_live_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let markers = home.path().join("run/conversations");
        let adapter = home.path().join("adapters/codex");
        let day = adapter.join("sessions/2026/01/02");
        std::fs::create_dir_all(&day).unwrap();
        let old = SystemTime::now() - Duration::from_secs(10 * 86400);
        let transcript = |id: &str| {
            let path = day.join(format!("rollout-2026-01-02T00-00-00-{id}.jsonl"));
            std::fs::write(&path, "{}\n").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
            path
        };
        let live = transcript("live-id");
        let stale = transcript("stale-id");
        let recent = day.join("rollout-recent-id.jsonl");
        std::fs::write(&recent, "{}\n").unwrap();
        // A link out of the tree is never followed or removed.
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), day.join("link.jsonl")).unwrap();

        let store = store(8, DAY, Some(&markers));
        store
            .begin("codex", None, Path::new("/w"), false)
            .unwrap()
            .finish(Some("live-id".into()), true);
        assert!(markers.join("live-id.json").is_file());
        // A marker left by a process that no longer runs does not protect anything.
        write_private_json(
            &markers,
            "stale-id.json",
            &Marker {
                owner: ProcessIdentity {
                    pid: u32::MAX - 1,
                    start_time: 1,
                },
                agent: "codex".into(),
                handle: "codex-9".into(),
            },
        )
        .unwrap();
        let files = ConversationFiles {
            dir: "sessions",
            extension: "jsonl",
        };
        let dry = collect_garbage(&adapter, files, &markers, DAY, true).unwrap();
        assert_eq!(dry.removed, vec![stale.clone()]);
        assert_eq!(dry.kept_live, 1);
        assert!(stale.exists() && markers.join("stale-id.json").exists());
        let report = collect_garbage(&adapter, files, &markers, DAY, false).unwrap();
        assert_eq!(report.removed, vec![stale.clone()]);
        assert!(!stale.exists() && live.exists() && recent.exists());
        assert!(outside.path().exists());
        assert!(!markers.join("stale-id.json").exists());
        // Ending the session releases its transcripts.
        drop(store);
        assert!(!markers.join("live-id.json").exists());
        // Never younger than the minimum age, whatever was asked.
        let report = collect_garbage(&adapter, files, &markers, Duration::ZERO, false).unwrap();
        assert_eq!(report.removed, vec![live]);
        assert!(recent.exists());
    }

    #[test]
    fn ages_parse_with_units() {
        assert_eq!(parse_age("30d"), Ok(Duration::from_secs(30 * 86400)));
        assert_eq!(parse_age("12h"), Ok(Duration::from_secs(12 * 3600)));
        assert_eq!(parse_age("90m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_age("45"), Ok(Duration::from_secs(45)));
        assert!(parse_age("3w").is_err());
        assert!(parse_age("d").is_err());
        assert!(parse_age("-1d").is_err());
    }
}
