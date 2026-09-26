//! What the daemon shares in-process with the chat bridges it runs.
//!
//! The daemon owns one [`Hub`]; each running account reaches it through a
//! [`Link`]. Through the hub the daemon learns which chat a daemon session
//! answers, whether owner work is still in flight (a planned restart waits
//! for it), and which chat the owner last wrote from, and it can queue a
//! notice into an account's durable outbox and hold a yes/no question for an
//! owner's direct chat, which the owner's next explicit answer there resolves
//! (`scv confirm`). Bridges learn why the daemon last restarted, so they
//! describe work that a planned restart interrupted accurately.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

/// How long [`Hub::notify`] waits for a bridge to store a notice.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);
/// The last-owner record is rewritten at most this often for the same chat.
const LAST_OWNER_REFRESH: u64 = 10 * 60;

/// A planned restart, as the bridges of the restarted daemon describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    /// The version the restart updated to.
    pub to_version: String,
}

/// The direct chat a daemon session answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The account's component ID, `<channel>:<account>`.
    pub component: String,
    /// The chat partner's ID on the channel.
    pub peer: String,
}

/// The chat the account owner last wrote from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastOwner {
    pub component: String,
    pub peer: String,
    pub unix_seconds: u64,
}

/// A message for one account's outbox, sent like a background report.
pub struct Notice {
    pub to: String,
    pub text: String,
    stored: oneshot::Sender<()>,
}

impl Notice {
    /// The bridge stored the notice durably.
    pub fn stored(self) {
        let _ = self.stored.send(());
    }
}

/// Why [`Hub::notify`] could not hand a notice over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyError {
    /// No bridge of that account is running.
    NotRunning,
    /// The bridge stopped or did not store the notice in time.
    NotStored,
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotRunning => "the account's bridge is not running",
            Self::NotStored => "the account's bridge did not store the notice",
        })
    }
}

impl std::error::Error for NotifyError {}

/// A question taken from the hub to be answered. Dropping it unanswered
/// tells the asker the answer was lost.
pub struct Answer(oneshot::Sender<bool>);

impl Answer {
    /// Hand the owner's answer to the asker.
    pub fn give(self, yes: bool) {
        let _ = self.0.send(yes);
    }
}

#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
    restart: Mutex<Option<Restart>>,
    /// Accounts that already recovered in this daemon: a later restart of a
    /// bridge alone is not the planned restart.
    recovered: Mutex<std::collections::HashSet<String>>,
    /// Where the last-owner record persists across restarts.
    last_owner_path: Option<PathBuf>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    bridges: HashMap<String, Bridge>,
    conversations: HashMap<u64, Conversation>,
    last_owner: Option<LastOwner>,
    /// Questions waiting for an answer, by account and direct chat; at most
    /// one per chat. They live only as long as this daemon.
    questions: HashMap<(String, String), Question>,
}

/// A yes/no question to the owner, waiting in their direct chat.
struct Question {
    id: String,
    answer: oneshot::Sender<bool>,
    /// The question is in the chat's outbox, so the owner may answer it.
    open: bool,
}

struct Bridge {
    id: u64,
    /// The account owner's ID, whoever holds its tool grant.
    owner: Option<String>,
    notices: mpsc::UnboundedSender<Notice>,
    /// Owner messages claimed and not yet answered durably.
    owner_claims: usize,
}

struct Conversation {
    component: String,
    peer: String,
    session: Option<String>,
    /// Background jobs, report turns, and reports not yet stored.
    work: usize,
}

impl Hub {
    /// A hub that keeps the last-owner record at `last_owner_path`.
    pub fn new(last_owner_path: Option<PathBuf>) -> Arc<Self> {
        let last_owner = last_owner_path.as_ref().and_then(|path| {
            let bytes = std::fs::read(path).ok()?;
            serde_json::from_slice(&bytes).ok()
        });
        Arc::new(Self {
            inner: Mutex::new(Inner {
                last_owner,
                ..Default::default()
            }),
            restart: Mutex::new(None),
            recovered: Mutex::default(),
            last_owner_path,
        })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record why this daemon started, before any bridge recovers.
    pub fn set_restart(&self, restart: Option<Restart>) {
        *self.restart.lock().unwrap_or_else(PoisonError::into_inner) = restart;
    }

    pub fn restart(&self) -> Option<Restart> {
        self.restart
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The direct chat `session` answers, if a bridge runs it.
    pub fn origin(&self, session: &str) -> Option<Origin> {
        self.inner()
            .conversations
            .values()
            .find(|conversation| conversation.session.as_deref() == Some(session))
            .map(|conversation| Origin {
                component: conversation.component.clone(),
                peer: conversation.peer.clone(),
            })
    }

    /// Owner messages every bridge has claimed and not yet answered durably.
    pub fn owner_claims(&self) -> usize {
        self.inner()
            .bridges
            .values()
            .map(|bridge| bridge.owner_claims)
            .sum()
    }

    /// Background work of `session` whose report is not yet stored.
    pub fn session_work(&self, session: &str) -> usize {
        self.inner()
            .conversations
            .values()
            .filter(|conversation| conversation.session.as_deref() == Some(session))
            .map(|conversation| conversation.work)
            .sum()
    }

    /// `None` while no bridge of `component` runs; otherwise its owner.
    pub fn owner(&self, component: &str) -> Option<Option<String>> {
        self.inner()
            .bridges
            .get(component)
            .map(|bridge| bridge.owner.clone())
    }

    /// The chat the account owner last wrote from, on any account.
    pub fn last_owner(&self) -> Option<LastOwner> {
        self.inner().last_owner.clone()
    }

    /// Queue `text` for `to` in `component`'s durable outbox and wait until
    /// it is stored. Delivery then follows the account's normal retries.
    pub async fn notify(&self, component: &str, to: &str, text: &str) -> Result<(), NotifyError> {
        let (stored, done) = oneshot::channel();
        let notice = Notice {
            to: to.to_owned(),
            text: text.to_owned(),
            stored,
        };
        {
            let inner = self.inner();
            let bridge = inner
                .bridges
                .get(component)
                .ok_or(NotifyError::NotRunning)?;
            bridge
                .notices
                .send(notice)
                .map_err(|_| NotifyError::NotRunning)?;
        }
        match tokio::time::timeout(NOTIFY_TIMEOUT, done).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(NotifyError::NotStored),
        }
    }

    /// Hold question `id` for the direct chat with `peer` on `component`.
    /// `None` when a question already waits in that chat. The caller sends
    /// the question itself, such as with [`Hub::notify`], then calls
    /// [`Hub::open`], after which the owner's next explicit yes or no there
    /// answers it.
    pub fn ask(&self, id: &str, component: &str, peer: &str) -> Option<oneshot::Receiver<bool>> {
        let mut inner = self.inner();
        let key = (component.to_owned(), peer.to_owned());
        if inner.questions.contains_key(&key) {
            return None;
        }
        let (answer, answered) = oneshot::channel();
        inner.questions.insert(
            key,
            Question {
                id: id.to_owned(),
                answer,
                open: false,
            },
        );
        Some(answered)
    }

    /// Question `id` is on its way to the owner: let their answer count.
    pub fn open(&self, id: &str) {
        for question in self.inner().questions.values_mut() {
            if question.id == id {
                question.open = true;
            }
        }
    }

    /// Drop question `id` unless it was answered first; whether it was still
    /// waiting.
    pub fn withdraw(&self, id: &str) -> bool {
        let mut inner = self.inner();
        let before = inner.questions.len();
        inner.questions.retain(|_, question| question.id != id);
        inner.questions.len() < before
    }

    fn record_owner(&self, component: &str, peer: &str) {
        let now = unix_now();
        let record = {
            let mut inner = self.inner();
            let fresh = inner.last_owner.as_ref().is_some_and(|last| {
                last.component == component
                    && last.peer == peer
                    && now.saturating_sub(last.unix_seconds) < LAST_OWNER_REFRESH
            });
            if fresh {
                return;
            }
            let record = LastOwner {
                component: component.to_owned(),
                peer: peer.to_owned(),
                unix_seconds: now,
            };
            inner.last_owner = Some(record.clone());
            record
        };
        if let Some(path) = &self.last_owner_path
            && let Err(error) = write_private(path, &record)
        {
            tracing::warn!("could not save the owner's last chat: {error:#}");
        }
    }
}

/// An account's connection to the daemon's hub.
#[derive(Clone)]
pub struct Link {
    hub: Option<Arc<Hub>>,
    component: String,
    owner: Option<String>,
}

impl Link {
    /// `component` is `<channel>:<account>`; `owner` is the account owner's
    /// ID from its credentials, whether or not it holds the tool grant.
    pub fn new(hub: Arc<Hub>, component: impl Into<String>, owner: Option<String>) -> Self {
        Self {
            hub: Some(hub),
            component: component.into(),
            owner: owner.filter(|owner| !owner.is_empty()),
        }
    }

    /// A link to no daemon: nothing is shared and no notice arrives.
    pub fn detached() -> Self {
        Self {
            hub: None,
            component: String::new(),
            owner: None,
        }
    }

    /// Why the daemon last restarted, for the account's first recovery in
    /// this daemon only; later runs of the bridge were not restarted by it.
    pub fn take_restart(&self) -> Option<Restart> {
        let hub = self.hub.as_ref()?;
        let first = hub
            .recovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.component.clone());
        if first { hub.restart() } else { None }
    }

    /// Announce a running bridge, which then stores each [`Notice`] it
    /// receives. Dropping the registration withdraws it.
    pub fn register(&self) -> (Registration, mpsc::UnboundedReceiver<Notice>) {
        let (notices, received) = mpsc::unbounded_channel();
        let Some(hub) = &self.hub else {
            // Nothing sends: keep the sender so the receiver never closes.
            return (
                Registration {
                    hub: None,
                    component: String::new(),
                    id: 0,
                    _idle: Some(notices),
                },
                received,
            );
        };
        let id = {
            let mut inner = hub.inner();
            inner.next += 1;
            let id = inner.next;
            inner.bridges.insert(
                self.component.clone(),
                Bridge {
                    id,
                    owner: self.owner.clone(),
                    notices,
                    owner_claims: 0,
                },
            );
            id
        };
        (
            Registration {
                hub: Some(Arc::clone(hub)),
                component: self.component.clone(),
                id,
                _idle: None,
            },
            received,
        )
    }
}

/// A running bridge's entry in the hub.
pub struct Registration {
    hub: Option<Arc<Hub>>,
    component: String,
    id: u64,
    _idle: Option<mpsc::UnboundedSender<Notice>>,
}

impl Registration {
    /// Owner messages this bridge claimed and has not answered durably.
    pub fn set_owner_claims(&self, claims: usize) {
        if let Some(hub) = &self.hub
            && let Some(bridge) = hub.inner().bridges.get_mut(&self.component)
            && bridge.id == self.id
        {
            bridge.owner_claims = claims;
        }
    }

    /// The account owner wrote to the bot directly from `peer`.
    pub fn owner_wrote(&self, peer: &str) {
        if let Some(hub) = &self.hub {
            hub.record_owner(&self.component, peer);
        }
    }

    /// Whether a question the owner can answer waits in the direct chat with
    /// `peer`.
    pub fn asking(&self, peer: &str) -> bool {
        self.hub.as_ref().is_some_and(|hub| {
            hub.inner()
                .questions
                .get(&(self.component.clone(), peer.to_owned()))
                .is_some_and(|question| question.open)
        })
    }

    /// Take the question the owner can answer in the direct chat with
    /// `peer`, to answer it; no question can be taken twice.
    pub fn take_question(&self, peer: &str) -> Option<Answer> {
        let hub = self.hub.as_ref()?;
        let key = (self.component.clone(), peer.to_owned());
        let mut inner = hub.inner();
        if !inner.questions.get(&key)?.open {
            return None;
        }
        inner
            .questions
            .remove(&key)
            .map(|question| Answer(question.answer))
    }

    /// Track the direct chat with `peer` while the tracker lives.
    pub fn conversation(&self, peer: &str) -> Tracker {
        let Some(hub) = &self.hub else {
            return Tracker { hub: None, id: 0 };
        };
        let mut inner = hub.inner();
        inner.next += 1;
        let id = inner.next;
        inner.conversations.insert(
            id,
            Conversation {
                component: self.component.clone(),
                peer: peer.to_owned(),
                session: None,
                work: 0,
            },
        );
        Tracker {
            hub: Some(Arc::clone(hub)),
            id,
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(hub) = &self.hub {
            let mut inner = hub.inner();
            if inner
                .bridges
                .get(&self.component)
                .is_some_and(|bridge| bridge.id == self.id)
            {
                inner.bridges.remove(&self.component);
            }
        }
    }
}

/// A direct chat's entry in the hub.
pub struct Tracker {
    hub: Option<Arc<Hub>>,
    id: u64,
}

impl Tracker {
    /// The daemon session the chat currently runs on, and its unreported
    /// background work.
    pub fn update(&self, session: Option<&str>, work: usize) {
        if let Some(hub) = &self.hub
            && let Some(conversation) = hub.inner().conversations.get_mut(&self.id)
        {
            conversation.session = session.map(str::to_owned);
            conversation.work = work;
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if let Some(hub) = &self.hub {
            hub.inner().conversations.remove(&self.id);
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Write `value` as JSON readable only by the user, replacing the file whole.
fn write_private(path: &std::path::Path, value: &impl Serialize) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)?;
    scv_client::fs::replace_private(path, &serde_json::to_vec(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests;
