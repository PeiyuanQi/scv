//! Multi-turn conversations with delegated agents.
//!
//! A session's conversations live in one `ConversationStore` shared by its
//! agents' backends, and end with the session. The model sees only SCV-issued
//! handles such as `codex-2`; the CLI's own session IDs stay here and are
//! never accepted from the model. While a conversation exists, a marker file
//! named after its session ID tells `scv agents gc` to keep its transcript.

use std::{
    any::Any,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use scv_core::ToolError;
use scv_protocol::AttachState;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{
    delegate::{
        adapters::{self, ConversationFiles},
        output::valid_session_id,
        records::{ProcessIdentity, write_private_json},
    },
    sync::lock,
};

/// Transcripts younger than this are never removed, so a turn that is still
/// running (and so still writing its transcript) is safe from `gc`.
pub(crate) const MIN_GC_AGE: Duration = Duration::from_secs(3600);

/// Longest handle accepted from the model.
const MAX_HANDLE_BYTES: usize = 64;

/// How the error for a session whose conversations are all in use begins
/// after its count, which a reviewed job reads as "no room for a reviewer".
pub(crate) const ALL_IN_USE: &str = "conversations of this session are running a turn";

#[derive(Debug, Clone, Copy)]
pub struct ConversationLimits {
    /// Conversations remembered per session; starting another forgets the
    /// least recently used idle one.
    pub max: usize,
    /// A conversation unused this long is forgotten.
    pub idle: Duration,
}

/// What a live conversation keeps between turns, such as its running child
/// process. Dropping the last reference (when the conversation is forgotten,
/// expires, or its session ends) is what shuts the child down.
#[derive(Clone)]
pub(crate) struct Attachment(pub(crate) Arc<dyn Any + Send + Sync>);

impl fmt::Debug for Attachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Attachment")
    }
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
    attachment: Option<Attachment>,
    /// Reviewed jobs holding it ([`PinGuard`]): it is neither evicted nor
    /// expired.
    pins: u32,
    /// Released while a turn ran: it goes when that turn ends.
    released: bool,
    /// The owner holds it in the agent's own terminal ([`TakeoverHold`]);
    /// `busy` is set too, so no turn runs meanwhile.
    taken_over: bool,
}

#[derive(Debug, Default)]
struct Inner {
    conversations: HashMap<String, Conversation>,
    /// Next handle number per agent.
    next: HashMap<String, u32>,
}

/// One session's conversations with its delegated agents.
#[derive(Debug)]
pub(crate) struct ConversationStore {
    limits: ConversationLimits,
    marker_dir: Option<PathBuf>,
    owner: Option<ProcessIdentity>,
    inner: Mutex<Inner>,
    /// Woken whenever a turn ends.
    changed: Notify,
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
    pub(crate) handle: String,
    pub(crate) turn: u32,
    /// Continuing: the known session ID. Starting: the ID SCV chose, if any.
    pub(crate) vendor: Option<String>,
    /// Continuing: what the conversation kept. Starting: what
    /// [`TurnGuard::attach`] set, stored when the turn finishes.
    attachment: Option<Attachment>,
    finished: bool,
}

/// The owner's hold on a conversation taken over in its agent's own
/// terminal: until it drops, the conversation runs no turn (calls wait or
/// queue as behind a running turn) and is neither evicted nor expired.
#[derive(Debug)]
pub(crate) struct TakeoverHold {
    store: Arc<ConversationStore>,
    pub(crate) handle: String,
    pub(crate) cwd: PathBuf,
    /// The agent's own session ID, which its terminal resumes.
    pub(crate) vendor: String,
    /// Turns the conversation has run.
    pub(crate) turns: u32,
    /// What the conversation kept between turns (a live ACP server), taken
    /// out so the caller stops it: its next turn starts a new one that
    /// loads the session, with what was done in the terminal.
    pub(crate) attachment: Option<Attachment>,
}

impl Drop for TakeoverHold {
    fn drop(&mut self) {
        let mut inner = lock(&self.store.inner);
        if let Some(conversation) = inner.conversations.get_mut(&self.handle)
            && conversation.taken_over
        {
            conversation.taken_over = false;
            conversation.busy = false;
            conversation.last_used = Instant::now();
            if conversation.released {
                let removed = inner.conversations.remove(&self.handle);
                drop(inner);
                self.store.remove_marker(
                    removed
                        .and_then(|conversation| conversation.vendor)
                        .as_deref(),
                );
            }
        }
        self.store.changed.notify_waiters();
    }
}

/// What keeps a conversation from being taken over in its agent's own
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Obstacle {
    /// The owner holds it in its terminal already.
    TakenOver,
    /// It runs a turn: the only obstacle an interrupting takeover gets
    /// past, by stopping the job that runs it.
    Running,
    /// A reviewed job holds it between its rounds.
    Reviewed,
    /// The agent has reported no session of its own for it yet.
    NoSession,
}

impl Obstacle {
    /// The owner's view of it.
    pub(crate) fn state(self) -> AttachState {
        match self {
            Self::TakenOver => AttachState::TakenOver,
            Self::Running => AttachState::Busy,
            Self::Reviewed => AttachState::Reviewed,
            Self::NoSession => AttachState::NoSession,
        }
    }

    /// How SCV refuses to take conversation `handle` over for it.
    pub(crate) fn refusal(self, handle: &str) -> String {
        match self {
            Self::TakenOver => format!("{handle} is taken over in its terminal already"),
            Self::Running => {
                format!("{handle} is running a turn; attach once it is idle, or stop its job first")
            }
            Self::Reviewed => format!(
                "{handle} is held by a reviewed job between its rounds; attach once the review ends"
            ),
            Self::NoSession => {
                format!("{handle} has no session of the agent's own to open in its terminal")
            }
        }
    }
}

/// What keeps `conversation` from being taken over, first what
/// [`ConversationStore::take_over`] names first.
fn obstacles(conversation: &Conversation) -> Vec<Obstacle> {
    [
        (conversation.taken_over, Obstacle::TakenOver),
        (conversation.busy, Obstacle::Running),
        (conversation.pins > 0, Obstacle::Reviewed),
        (conversation.vendor.is_none(), Obstacle::NoSession),
    ]
    .into_iter()
    .filter_map(|(applies, obstacle)| applies.then_some(obstacle))
    .collect()
}

/// The refusal for a conversation `handle` the session does not have.
pub(crate) fn unknown_conversation(handle: &str) -> String {
    format!(
        "no conversation {handle} in this session; it ended, or it was forgotten after it sat idle"
    )
}

/// A reviewed job's hold on a conversation, keeping it out of eviction and
/// idle expiry until dropped.
#[derive(Debug)]
pub(crate) struct PinGuard {
    store: Arc<ConversationStore>,
    handle: String,
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        if let Some(conversation) = lock(&self.store.inner).conversations.get_mut(&self.handle) {
            conversation.pins = conversation.pins.saturating_sub(1);
        }
    }
}

/// Whether `value` has the shape of an SCV conversation handle
/// (`<agent>-<number>`). CLI session IDs never do.
pub(crate) fn is_handle(value: &str) -> bool {
    handle_agent(value).is_some()
}

/// The agent whose conversation `handle` names, such as `codex` for
/// `codex-2`, when it has the shape of a handle.
pub(crate) fn handle_agent(handle: &str) -> Option<&str> {
    if handle.len() > MAX_HANDLE_BYTES {
        return None;
    }
    let (agent, number) = handle.rsplit_once('-')?;
    (!number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && adapters::adapter(agent).is_some())
    .then_some(agent)
}

impl ConversationStore {
    /// `marker_dir` is `$SCV_HOME/state/conversations`; `None` keeps no markers.
    pub(crate) fn new(limits: ConversationLimits, marker_dir: Option<PathBuf>) -> Self {
        Self {
            limits,
            marker_dir,
            owner: ProcessIdentity::current(),
            inner: Mutex::new(Inner::default()),
            changed: Notify::new(),
        }
    }

    /// How many conversations the session remembers, and for how long.
    pub(crate) fn limits(&self) -> ConversationLimits {
        self.limits
    }

    /// Keep conversation `handle` from eviction and idle expiry until the
    /// guard drops; `None` when the session does not have it.
    pub(crate) fn pin(self: &Arc<Self>, handle: &str) -> Option<PinGuard> {
        let mut inner = lock(&self.inner);
        let conversation = inner.conversations.get_mut(handle)?;
        conversation.pins += 1;
        Some(PinGuard {
            store: Arc::clone(self),
            handle: handle.to_owned(),
        })
    }

    /// End conversation `handle`, as for a reviewer whose round is over:
    /// its marker goes and its attachment drops, which stops a live ACP
    /// server or nested SCV. One running a turn goes when the turn ends.
    pub(crate) fn release(&self, handle: &str) {
        let mut inner = lock(&self.inner);
        let Some(conversation) = inner.conversations.get_mut(handle) else {
            return;
        };
        if conversation.busy {
            conversation.released = true;
            return;
        }
        let removed = inner.conversations.remove(handle);
        drop(inner);
        self.remove_marker(
            removed
                .and_then(|conversation| conversation.vendor)
                .as_deref(),
        );
    }

    /// Number new conversations after `handles` this session's ID already
    /// used, so a session that comes back under its ID (a daemon's
    /// workbench session) never reuses a handle an earlier record names.
    pub(crate) fn continue_after<'a>(&self, handles: impl IntoIterator<Item = &'a str>) {
        let mut inner = lock(&self.inner);
        for handle in handles {
            if let Some((agent, number)) = handle.rsplit_once('-')
                && let Ok(number) = number.parse::<u32>()
            {
                let next = inner.next.entry(agent.to_owned()).or_insert(0);
                *next = (*next).max(number);
            }
        }
    }

    /// Whether the session has conversation `handle`.
    pub(crate) fn knows(&self, handle: &str) -> bool {
        lock(&self.inner).conversations.contains_key(handle)
    }

    /// Where conversation `handle` runs.
    pub(crate) fn cwd(&self, handle: &str) -> Option<PathBuf> {
        lock(&self.inner)
            .conversations
            .get(handle)
            .map(|conversation| conversation.cwd.clone())
    }

    /// The agent conversation `handle` belongs to.
    pub(crate) fn agent_of(&self, handle: &str) -> Option<String> {
        lock(&self.inner)
            .conversations
            .get(handle)
            .map(|conversation| conversation.agent.clone())
    }

    /// What keeps conversation `handle` from being taken over in its
    /// agent's own terminal now, in the order [`Self::take_over`] refuses
    /// it; `None` when the session does not have it.
    pub(crate) fn obstacles(&self, handle: &str) -> Option<Vec<Obstacle>> {
        lock(&self.inner).conversations.get(handle).map(obstacles)
    }

    /// Hold conversation `handle` for the owner's takeover in its agent's
    /// own terminal: it must run no turn and have the agent's session ID.
    pub(crate) fn take_over(self: &Arc<Self>, handle: &str) -> Result<TakeoverHold, String> {
        let mut inner = lock(&self.inner);
        let conversation = inner
            .conversations
            .get_mut(handle)
            .ok_or_else(|| unknown_conversation(handle))?;
        if let Some(obstacle) = obstacles(conversation).first() {
            return Err(obstacle.refusal(handle));
        }
        let Some(vendor) = conversation.vendor.clone() else {
            return Err(Obstacle::NoSession.refusal(handle));
        };
        conversation.busy = true;
        conversation.taken_over = true;
        conversation.last_used = Instant::now();
        Ok(TakeoverHold {
            store: Arc::clone(self),
            handle: handle.to_owned(),
            cwd: conversation.cwd.clone(),
            vendor,
            turns: conversation.turns,
            attachment: conversation.attachment.take(),
        })
    }

    /// Whether conversation `handle` is running a turn.
    pub(crate) fn is_busy(&self, handle: &str) -> bool {
        lock(&self.inner)
            .conversations
            .get(handle)
            .is_some_and(|c| c.busy)
    }

    /// What conversation `handle` keeps between turns, if anything.
    pub(crate) fn attachment(&self, handle: &str) -> Option<Attachment> {
        lock(&self.inner)
            .conversations
            .get(handle)
            .and_then(|c| c.attachment.clone())
    }

    /// Wait until conversation `handle` runs no turn.
    pub(crate) async fn wait_idle(
        &self,
        handle: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        loop {
            let notified = self.changed.notified();
            if !self.is_busy(handle) {
                return Ok(());
            }
            tokio::select! {
                () = notified => {},
                () = cancellation.cancelled() => return Err(ToolError::cancelled("waiting for delegated conversation")),
            }
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
        let mut inner = lock(&self.inner);
        let now = Instant::now();
        let expired: Vec<String> = inner
            .conversations
            .iter()
            .filter(|(_, conversation)| {
                !conversation.busy
                    && conversation.pins == 0
                    && now.duration_since(conversation.last_used) >= self.limits.idle
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
            return Err(ToolError::invalid_arguments(format!(
                "session {:?} is not a conversation handle; pass the `session` value an \
                 earlier {agent} call returned, or omit it to start a new conversation",
                crate::args::bounded(handle, 80)
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
            return Err(ToolError::invalid_arguments(format!(
                "conversation {handle} {reason}; omit session to start a new conversation"
            )));
        };
        if conversation.agent != agent {
            return Err(ToolError::invalid_arguments(format!(
                "conversation {handle} belongs to {}, not {agent}",
                conversation.agent
            )));
        }
        if conversation.taken_over {
            return Err(ToolError::failed(format!(
                "session busy: conversation {handle} is taken over by the user in its terminal"
            )));
        }
        if conversation.busy {
            return Err(ToolError::failed(format!(
                "session busy: conversation {handle} is still running a turn"
            )));
        }
        if conversation.cwd != cwd {
            return Err(ToolError::invalid_arguments(format!(
                "conversation {handle} runs in {:?}; continue it there or omit session to \
                 start a new conversation in {:?}",
                conversation.cwd, cwd
            )));
        }
        let Some(vendor) = conversation.vendor.clone() else {
            return Err(ToolError::failed(format!(
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
            attachment: conversation.attachment.clone(),
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
                .filter(|(_, conversation)| !conversation.busy && conversation.pins == 0)
                .min_by_key(|(_, conversation)| conversation.last_used)
                .map(|(handle, _)| handle.clone())
            else {
                let held = if inner.conversations.values().any(|c| c.pins > 0) {
                    " or held by a reviewed job"
                } else {
                    ""
                };
                return Err(ToolError::limit(format!(
                    "all {} {ALL_IN_USE}{held}",
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
                attachment: None,
                pins: 0,
                released: false,
                taken_over: false,
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
            attachment: None,
            finished: false,
        })
    }

    /// Take in a conversation the agent saved under its own session ID
    /// `vendor`, with no process kept from before, so a call can continue it
    /// (resuming an interrupted job). Returns its handle. The same agent,
    /// vendor session, and directory already held here is that handle
    /// again, so a second resume queues behind it instead of starting
    /// another process in the one agent session.
    pub(crate) fn adopt(&self, agent: &str, cwd: &Path, vendor: &str) -> Result<String, ToolError> {
        if !valid_session_id(vendor) {
            return Err(ToolError::invalid_arguments(
                "that is not an agent session ID SCV can pass on",
            ));
        }
        let mut inner = lock(&self.inner);
        let now = Instant::now();
        if let Some(handle) = inner
            .conversations
            .iter()
            .find_map(|(handle, conversation)| {
                (conversation.agent == agent
                    && conversation.vendor.as_deref() == Some(vendor)
                    && conversation.cwd == cwd)
                    .then(|| handle.clone())
            })
        {
            if let Some(conversation) = inner.conversations.get_mut(&handle) {
                conversation.last_used = now;
            }
            return Ok(handle);
        }
        while inner.conversations.len() >= self.limits.max.max(1) {
            let Some(oldest) = inner
                .conversations
                .iter()
                .filter(|(_, conversation)| !conversation.busy && conversation.pins == 0)
                .min_by_key(|(_, conversation)| conversation.last_used)
                .map(|(handle, _)| handle.clone())
            else {
                return Err(ToolError::limit(format!(
                    "all {} {ALL_IN_USE}",
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
        inner.conversations.insert(
            handle.clone(),
            Conversation {
                agent: agent.to_owned(),
                cwd: cwd.to_owned(),
                vendor: Some(vendor.to_owned()),
                turns: 1,
                busy: false,
                last_used: now,
                attachment: None,
                pins: 0,
                released: false,
                taken_over: false,
            },
        );
        drop(inner);
        self.write_marker(vendor, agent, &handle);
        Ok(handle)
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

    /// Handles this session remembers, for tests.
    #[cfg(test)]
    pub(crate) fn handles(&self) -> Vec<String> {
        let mut handles: Vec<_> = lock(&self.inner).conversations.keys().cloned().collect();
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
    /// What the conversation keeps between turns, if anything.
    pub(crate) fn attachment(&self) -> Option<&Attachment> {
        self.attachment.as_ref()
    }

    /// Keep `attachment` with the conversation once this turn finishes.
    pub(crate) fn attach(&mut self, attachment: Attachment) {
        self.attachment = Some(attachment);
    }

    /// End the conversation now, for one that cannot go on (its live child
    /// exited). Its attachment is dropped.
    pub(crate) fn forget(mut self) {
        self.finished = true;
        let removed = lock(&self.store.inner).conversations.remove(&self.handle);
        self.store.remove_marker(
            removed
                .and_then(|conversation| conversation.vendor)
                .as_deref(),
        );
        self.store.remove_marker(self.vendor.as_deref());
    }

    /// Settle the turn. `reported` is the session ID the CLI printed. Returns
    /// the handle when the conversation can be continued; a first turn that
    /// failed without the CLI reporting a session is forgotten.
    pub(crate) fn finish(mut self, reported: Option<String>, completed: bool) -> Option<String> {
        self.finished = true;
        let reported = reported.filter(|id| valid_session_id(id));
        let mut inner = lock(&self.store.inner);
        let first = self.turn == 1;
        if first && reported.is_none() && !completed {
            inner.conversations.remove(&self.handle);
            drop(inner);
            self.store.remove_marker(self.vendor.as_deref());
            return None;
        }
        let vendor = reported.or_else(|| self.vendor.clone());
        let conversation = inner.conversations.get_mut(&self.handle)?;
        if conversation.released {
            let removed = inner.conversations.remove(&self.handle);
            drop(inner);
            self.store.remove_marker(
                removed
                    .and_then(|conversation| conversation.vendor)
                    .as_deref(),
            );
            self.store.remove_marker(vendor.as_deref());
            return None;
        }
        if let Some(attachment) = self.attachment.take() {
            conversation.attachment = Some(attachment);
        }
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
        if !self.finished {
            let mut inner = lock(&self.store.inner);
            if self.turn == 1 {
                inner.conversations.remove(&self.handle);
                drop(inner);
                self.store.remove_marker(self.vendor.as_deref());
            } else if let Some(conversation) = inner.conversations.get_mut(&self.handle) {
                conversation.busy = false;
                if conversation.released {
                    let removed = inner.conversations.remove(&self.handle);
                    drop(inner);
                    self.store.remove_marker(
                        removed
                            .and_then(|conversation| conversation.vendor)
                            .as_deref(),
                    );
                }
            }
        }
        // However the turn ended, its conversation runs none now.
        self.store.changed.notify_waiters();
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
pub(crate) fn remove_stale_markers(marker_dir: &Path) -> usize {
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
mod tests;
