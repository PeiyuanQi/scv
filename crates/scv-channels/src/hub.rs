//! What the daemon shares in-process with the chat bridges it runs.
//!
//! The daemon owns one [`Hub`]; each running account reaches it through a
//! [`Link`]. Through the hub the daemon learns which chat a daemon session
//! answers, whether owner work is still in flight (a planned restart waits
//! for it), and which chat the owner last wrote from, and it can queue a
//! notice into an account's durable outbox and hold a yes/no question for an
//! owner's direct chat (`scv confirm`). The question opens once the bridge
//! has delivered its text, and the owner's next explicit answer there, sent
//! after that, resolves it. Bridges learn why the daemon last restarted, so
//! they describe work that a planned restart interrupted accurately.
//!
//! Mail chats (`purpose = "mail"`) are set apart here: the hub queues SCV's
//! own notices and questions only to ordinary chats, and quarantined keyed
//! notices, which carry mail text, only to mail chats
//! ([`Hub::notify_keyed`]). It mirrors how each keyed notice went, so the
//! email account that handed it over learns whether it was stored,
//! delivered, or refused, and it holds each running email account's counts
//! for `mail status` and daemon status.
//!
//! An email account that takes actions registers its authority here: the
//! channel its ledger reads commands from, and the approval codes and
//! message handles it answers for, so an owner's command in a mail chat
//! reaches the one account that can act on it. The hub carries commands and
//! replies and never decides anything itself. It also counts the actions
//! being carried out, and holds the drain flag a planned restart raises so
//! that no new one starts.

use scv_protocol::{MailAction, MailCounts, Purpose};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

use crate::mail_chat::{ChatEvidence, MailCommand};

/// How long [`Hub::notify`] waits for a bridge to store a notice.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a mail command waits for the accounts' answers.
const MAIL_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// The answer when an account took a command but has not answered in time;
/// its ledger sends the outcome to the chat when it has one.
pub(crate) const STILL_WORKING_REPLY: &str = "Still working on that; SCV will confirm here.";
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
    pub(crate) unix_seconds: u64,
}

/// A message for one account's outbox, sent like a background report. Its
/// text is SCV's own, never the model's, so a channel that marks SCV's words
/// (WeChat's `system msg: ` code block) marks it when the bridge stores it,
/// unless it is quarantined.
pub struct Notice {
    pub to: String,
    pub text: String,
    /// The question whose text this is ([`Hub::send_question`]): it opens
    /// only once the text reaches the chat, and is never held for later.
    pub question: Option<String>,
    /// A quarantined notice's key ([`Hub::notify_keyed`]). Its text holds
    /// mail and goes out as it is, unlabelled; only a mail chat stores it,
    /// once per key.
    pub key: Option<String>,
    stored: oneshot::Sender<()>,
}

/// Which kind of notice [`Hub::queue`] hands over.
enum NoticeKind {
    /// SCV's own words, for an ordinary chat, possibly a question's text.
    Plain(Option<String>),
    /// A quarantined notice for a mail chat, under its key.
    Keyed(String),
}

impl Notice {
    fn plain(question: Option<&str>) -> NoticeKind {
        NoticeKind::Plain(question.map(str::to_owned))
    }

    fn keyed(key: &str) -> NoticeKind {
        NoticeKind::Keyed(key.to_owned())
    }

    /// The bridge stored the notice durably.
    pub fn stored(self) {
        let _ = self.stored.send(());
    }

    /// Whoever queued the notice stopped waiting for it, as [`Hub::notify`]
    /// does after [`NOTIFY_TIMEOUT`]; it was told the notice was not stored,
    /// so the bridge must not send it late.
    pub(crate) fn abandoned(&self) -> bool {
        self.stored.is_closed()
    }
}

/// Why [`Hub::notify`] could not hand a notice over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyError {
    /// No bridge of that account is running.
    NotRunning,
    /// The bridge stopped or did not store the notice in time.
    NotStored,
    /// The account carries the other kind of notice: a mail chat takes only
    /// quarantined mail notices, and an ordinary chat never takes one.
    WrongPurpose,
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotRunning => "the account's bridge is not running",
            Self::NotStored => "the account's bridge did not store the notice",
            Self::WrongPurpose => {
                "the account is a mail chat, or a mail notice went to an ordinary chat"
            }
        })
    }
}

/// How a quarantined keyed notice went, as the mail chat that stored it
/// records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KeyedOutcome {
    /// Stored in the outbox and not yet sent.
    Pending,
    /// The platform accepted it, at this Unix millisecond by this host's
    /// clock.
    Delivered { at_ms: u64 },
    /// The platform refused it; it is not sent again.
    Refused,
}

impl std::error::Error for NotifyError {}

/// Where a question stood when [`Hub::withdraw`] dropped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    /// It no longer waited: it was answered, or its text could not be
    /// delivered, which the asker's receiver tells.
    Settled,
    /// Its text had not reached the chat; the bridge drops it unsent.
    Unsent,
    /// Its text had reached the chat, which went unanswered.
    Unanswered,
}

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
    /// A disk holding chat files is below its free-space floor, so bridges
    /// save no new files from chat.
    low_disk: std::sync::atomic::AtomicBool,
    /// A planned restart is about to happen: no email account starts
    /// another action.
    mail_drain: AtomicBool,
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
    /// How each quarantined notice went, by mail chat and key: a mirror of
    /// each mail chat's `recent_keys`, reloaded when it registers.
    keyed: HashMap<(String, String), KeyedOutcome>,
    /// Running email accounts, by component ID.
    mail: HashMap<String, MailAccount>,
}

/// A running email account as the hub knows it.
struct MailAccount {
    id: u64,
    /// The mail chats it reports to, in order.
    routes: Vec<String>,
    counts: MailCounts,
    /// Where its ledger takes commands, when it takes actions.
    authority: Option<mpsc::Sender<MailRequest>>,
    /// The approval codes it answers for, live or remembered.
    codes: HashSet<String>,
    /// The handles of its reported mail.
    handles: HashSet<String>,
    /// Actions it is carrying out now.
    executing: usize,
}

/// What the daemon asks an email account to do outside a mail chat. None
/// of these can approve anything: they only withdraw or list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailOrder {
    /// `scv mail cancel`: withdraw action `action`, or every action.
    Cancel { action: Option<String> },
    /// `scv mail status`: the actions, by ID, kind, and state.
    List,
}

/// What an email account's ledger is asked.
#[cfg_attr(
    not(feature = "email"),
    allow(dead_code, reason = "only an email account's ledger reads a request")
)]
#[derive(Debug)]
pub(crate) enum MailWork {
    /// A command the owner sent in a mail chat, with its evidence.
    Chat {
        command: MailCommand,
        evidence: ChatEvidence,
    },
    /// An order from the daemon.
    Order(MailOrder),
}

/// One request to an email account's ledger, and where its answer goes.
#[cfg_attr(
    not(feature = "email"),
    allow(dead_code, reason = "only an email account's ledger reads a request")
)]
#[derive(Debug)]
pub(crate) struct MailRequest {
    pub(crate) work: MailWork,
    pub(crate) reply: oneshot::Sender<MailReply>,
}

/// An email account's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailReply {
    /// SCV's words for the chat or the command line: codes, kinds, times,
    /// and counts, never an address, a subject, or a model's text.
    Text(String),
    /// The account's actions, for `scv mail status`.
    Actions(Vec<MailAction>),
}

/// Why [`Hub::mail_order`] could not reach an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailOrderError {
    /// No running email account by that name takes actions.
    NotRunning,
    /// The account did not answer in time.
    NoAnswer,
}

impl std::fmt::Display for MailOrderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotRunning => "that email account is not running with mail actions on",
            Self::NoAnswer => "the email account did not answer in time",
        })
    }
}

impl std::error::Error for MailOrderError {}

/// The most keyed outcomes the mirror holds for one mail chat; a mail
/// chat keeps no more in its own state.
const MAX_KEYED_PER_CHAT: usize = crate::state::MAX_RECENT_KEYS;

/// A yes/no question to the owner, waiting in their direct chat.
struct Question {
    id: String,
    answer: oneshot::Sender<bool>,
    /// When its text reached the chat, in Unix milliseconds by this host's
    /// clock. Until then nothing answers it, and after that only a message
    /// sent later.
    delivered_ms: Option<u64>,
}

struct Bridge {
    id: u64,
    /// The account owner's ID, whoever holds its tool grant.
    owner: Option<String>,
    /// What the account carries: an ordinary chat or a mail chat.
    purpose: Purpose,
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
            low_disk: std::sync::atomic::AtomicBool::new(false),
            mail_drain: AtomicBool::new(false),
        })
    }

    /// Record whether a disk holding the chat log, chat media, or kept files
    /// is below its free-space floor; while it is, bridges save no new files
    /// from chat.
    pub fn set_low_disk(&self, low: bool) {
        self.low_disk
            .store(low, std::sync::atomic::Ordering::Release);
    }

    pub fn low_disk(&self) -> bool {
        self.low_disk.load(std::sync::atomic::Ordering::Acquire)
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

    /// `None` while no bridge of `component` runs; otherwise what it
    /// carries.
    pub fn purpose(&self, component: &str) -> Option<Purpose> {
        self.inner()
            .bridges
            .get(component)
            .map(|bridge| bridge.purpose)
    }

    /// Whether a running bridge of `component` is a mail chat, which never
    /// takes SCV's notices or questions.
    pub fn is_mail_chat(&self, component: &str) -> bool {
        self.purpose(component) == Some(Purpose::Mail)
    }

    /// Queue a quarantined notice, whose `text` holds mail, for `to` in the
    /// mail chat `component`'s outbox under `key`, and wait until it is
    /// stored, as [`Hub::notify`] does. The hub refuses any account that is
    /// not a running mail chat, and the bridge stores a key once: handing
    /// the same key over again is acknowledged without a second copy.
    pub async fn notify_keyed(
        &self,
        component: &str,
        to: &str,
        text: &str,
        key: &str,
    ) -> Result<(), NotifyError> {
        self.queue(component, to, text, Notice::keyed(key)).await
    }

    /// How the quarantined notice `key` went in mail chat `component`;
    /// `None` when that chat has no record of it.
    pub fn keyed_outcome(&self, component: &str, key: &str) -> Option<KeyedOutcome> {
        self.inner()
            .keyed
            .get(&(component.to_owned(), key.to_owned()))
            .copied()
    }

    /// Announce a running email account that reports to the mail chats
    /// `routes`. Dropping the registration withdraws it.
    pub fn register_mail(
        self: &Arc<Self>,
        component: &str,
        routes: Vec<String>,
    ) -> MailRegistration {
        let mut inner = self.inner();
        inner.next += 1;
        let id = inner.next;
        inner.mail.insert(
            component.to_owned(),
            MailAccount {
                id,
                routes,
                counts: MailCounts::default(),
                authority: None,
                codes: HashSet::new(),
                handles: HashSet::new(),
                executing: 0,
            },
        );
        MailRegistration {
            hub: Arc::clone(self),
            component: component.to_owned(),
            id,
        }
    }

    /// A running email account's counts, for daemon status.
    pub fn mail_counts(&self, component: &str) -> Option<MailCounts> {
        self.inner()
            .mail
            .get(component)
            .map(|account| account.counts.clone())
    }

    /// The running email accounts that report to mail chat `route`, by
    /// component ID, with their counts.
    pub fn mail_accounts_for(&self, route: &str) -> Vec<(String, MailCounts)> {
        let mut accounts: Vec<_> = self
            .inner()
            .mail
            .iter()
            .filter(|(_, account)| account.routes.iter().any(|r| r == route))
            .map(|(component, account)| (component.clone(), account.counts.clone()))
            .collect();
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        accounts
    }

    /// The running email accounts that take actions, by component ID.
    pub fn mail_authorities(&self) -> Vec<String> {
        let mut found: Vec<String> = self
            .inner()
            .mail
            .iter()
            .filter(|(_, account)| account.authority.is_some())
            .map(|(component, _)| component.clone())
            .collect();
        found.sort();
        found
    }

    /// Mail actions every email account is carrying out now; a planned
    /// restart waits for none.
    pub fn mail_executing(&self) -> usize {
        self.inner()
            .mail
            .values()
            .map(|account| account.executing)
            .sum()
    }

    /// Raise or lower the drain flag: while it is up, no email account
    /// starts another action, and those under way finish. Raised, it holds
    /// for every action counted after this returns, so a restart that then
    /// sees [`Hub::mail_executing`] at zero sees none start.
    pub fn set_mail_drain(&self, drain: bool) {
        let _inner = self.inner();
        self.mail_drain.store(drain, Ordering::SeqCst);
    }

    /// Whether the drain flag is up.
    pub fn mail_draining(&self) -> bool {
        self.mail_drain.load(Ordering::SeqCst)
    }

    /// Hand the daemon's `order` to email account `component` and wait for
    /// its answer.
    pub async fn mail_order(
        &self,
        component: &str,
        order: MailOrder,
    ) -> Result<MailReply, MailOrderError> {
        let authority = self
            .inner()
            .mail
            .get(component)
            .and_then(|account| account.authority.clone())
            .ok_or(MailOrderError::NotRunning)?;
        let (reply, answer) = oneshot::channel();
        authority
            .send(MailRequest {
                work: MailWork::Order(order),
                reply,
            })
            .await
            .map_err(|_| MailOrderError::NotRunning)?;
        match tokio::time::timeout(MAIL_COMMAND_TIMEOUT, answer).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => Err(MailOrderError::NoAnswer),
        }
    }

    /// The email accounts that take actions and report to `route`, with
    /// where to send them work, sorted by component.
    fn authorities_for(&self, route: &str) -> Vec<(String, mpsc::Sender<MailRequest>)> {
        let mut found: Vec<_> = self
            .inner()
            .mail
            .iter()
            .filter(|(_, account)| account.routes.iter().any(|r| r == route))
            .filter_map(|(component, account)| {
                account
                    .authority
                    .clone()
                    .map(|authority| (component.clone(), authority))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// The account that answers for code or handle `key`, among those that
    /// report to `route` when it is set, and where to send it work.
    fn authority_of(
        &self,
        key: &str,
        handle: bool,
        route: Option<&str>,
    ) -> Option<(String, mpsc::Sender<MailRequest>)> {
        self.inner().mail.iter().find_map(|(component, account)| {
            let known = if handle {
                account.handles.contains(key)
            } else {
                account.codes.contains(key)
            };
            let here = route.is_none_or(|route| account.routes.iter().any(|known| known == route));
            (known && here)
                .then(|| account.authority.clone())
                .flatten()
                .map(|authority| (component.clone(), authority))
        })
    }

    /// Carry out the owner's `command` from mail chat `evidence.route`: hand
    /// it to the email accounts it concerns (by code, by handle, or every
    /// account that reports there) and gather their answers, one line each,
    /// waiting at most ten seconds. The answer holds SCV's words only.
    pub(crate) async fn mail_command(
        &self,
        evidence: ChatEvidence,
        command: MailCommand,
    ) -> String {
        let route = evidence.route.clone();
        let everyone = self.authorities_for(&route);
        let mut work: Vec<(mpsc::Sender<MailRequest>, MailCommand)> = Vec::new();
        let mut answers: Vec<String> = Vec::new();
        match &command {
            MailCommand::Approve(codes) | MailCommand::Deny(codes) => {
                let mut by_account: Vec<(String, mpsc::Sender<MailRequest>, Vec<String>)> =
                    Vec::new();
                for code in codes {
                    match self.authority_of(code, false, None) {
                        Some((component, authority)) => {
                            match by_account
                                .iter_mut()
                                .find(|(known, ..)| *known == component)
                            {
                                Some((_, _, list)) => list.push(code.clone()),
                                None => by_account.push((component, authority, vec![code.clone()])),
                            }
                        }
                        None if everyone.is_empty() => {
                            answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into());
                        }
                        None => answers.push(format!("No mail action has code {code}.")),
                    }
                }
                for (_, authority, list) in by_account {
                    let each = match command {
                        MailCommand::Approve(_) => MailCommand::Approve(list),
                        _ => MailCommand::Deny(list),
                    };
                    work.push((authority, each));
                }
            }
            // A revision or a request about reported mail goes only to an
            // account that reports here, so another chat's codes and handles
            // are neither used nor confirmed from this one.
            MailCommand::Revise { code, .. } => {
                match self.authority_of(code, false, Some(&route)) {
                    Some((_, authority)) => work.push((authority, command.clone())),
                    None if everyone.is_empty() => {
                        answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into());
                    }
                    None => answers.push(format!("No mail action has code {code}.")),
                }
            }
            MailCommand::Reply { handle, .. }
            | MailCommand::Forward { handle, .. }
            | MailCommand::Message { handle, .. } => {
                match self.authority_of(handle, true, Some(&route)) {
                    Some((_, authority)) => work.push((authority, command.clone())),
                    None if everyone.is_empty() => {
                        answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into());
                    }
                    None => answers.push(format!(
                        "No reported mail has handle #{handle}; handles work for mail reported in \
                     the last days only."
                    )),
                }
            }
            MailCommand::Compose { account, .. } => {
                let chosen = match account {
                    Some(name) => {
                        let component = format!("email:{name}");
                        everyone
                            .iter()
                            .find(|(known, _)| *known == component)
                            .cloned()
                    }
                    None if everyone.len() == 1 => everyone.first().cloned(),
                    None => None,
                };
                match chosen {
                    Some((_, authority)) => work.push((authority, command.clone())),
                    None if everyone.is_empty() => {
                        answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into());
                    }
                    None if account.is_some() => answers.push(
                        "No mail account by that name takes actions here; nothing was done.".into(),
                    ),
                    None => answers.push(
                        "Say which account: mail compose ACCOUNT ADDRESS what to write.".into(),
                    ),
                }
            }
            MailCommand::DenyAll | MailCommand::Status => {
                if everyone.is_empty() && command == MailCommand::DenyAll {
                    answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into());
                }
                for (_, authority) in &everyone {
                    work.push((authority.clone(), command.clone()));
                }
            }
        }
        let deadline = tokio::time::Instant::now() + MAIL_COMMAND_TIMEOUT;
        let mut waiting = Vec::new();
        for (authority, command) in work {
            let (reply, answer) = oneshot::channel();
            let request = MailRequest {
                work: MailWork::Chat {
                    command,
                    evidence: evidence.clone(),
                },
                reply,
            };
            match tokio::time::timeout_at(deadline, authority.send(request)).await {
                Ok(Ok(())) => waiting.push(answer),
                _ => answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into()),
            }
        }
        for answer in waiting {
            match tokio::time::timeout_at(deadline, answer).await {
                Ok(Ok(MailReply::Text(text))) => answers.push(text),
                Ok(Ok(MailReply::Actions(_))) => {}
                Ok(Err(_)) => answers.push(crate::mail_chat::NOT_RUNNING_REPLY.into()),
                Err(_) => answers.push(STILL_WORKING_REPLY.into()),
            }
        }
        answers.dedup();
        answers.join("\n")
    }

    /// The chat the account owner last wrote from, on any account.
    pub fn last_owner(&self) -> Option<LastOwner> {
        self.inner().last_owner.clone()
    }

    /// Queue `text` for `to` in `component`'s durable outbox and wait until
    /// it is stored. Delivery then follows the account's normal retries.
    /// Once this returns an error, or its future is dropped, the bridge
    /// drops the notice instead of storing it late.
    pub async fn notify(&self, component: &str, to: &str, text: &str) -> Result<(), NotifyError> {
        self.queue(component, to, text, Notice::plain(None)).await
    }

    /// Queue the text of question `id`, held with [`Hub::ask`], as
    /// [`Hub::notify`] queues a notice. The bridge opens the question once
    /// the text reaches the chat, fails it if the platform refuses it, and
    /// drops the text unsent once the question no longer waits.
    pub async fn send_question(
        &self,
        id: &str,
        component: &str,
        to: &str,
        text: &str,
    ) -> Result<(), NotifyError> {
        self.queue(component, to, text, Notice::plain(Some(id)))
            .await
    }

    async fn queue(
        &self,
        component: &str,
        to: &str,
        text: &str,
        kind: NoticeKind,
    ) -> Result<(), NotifyError> {
        let (stored, done) = oneshot::channel();
        let (question, key) = match kind {
            NoticeKind::Plain(question) => (question, None),
            NoticeKind::Keyed(key) => (None, Some(key)),
        };
        let notice = Notice {
            to: to.to_owned(),
            text: text.to_owned(),
            question,
            key,
            stored,
        };
        {
            let inner = self.inner();
            let bridge = inner
                .bridges
                .get(component)
                .ok_or(NotifyError::NotRunning)?;
            // Mail goes only to mail chats, and SCV's notices and questions
            // never do: a mail chat carries nothing else.
            if (bridge.purpose == Purpose::Mail) != notice.key.is_some() {
                return Err(NotifyError::WrongPurpose);
            }
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
    /// its text with [`Hub::send_question`]; once the bridge has delivered
    /// it, the owner's next explicit yes or no there answers it.
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
                delivered_ms: None,
            },
        );
        Some(answered)
    }

    /// Drop question `id` unless it was settled first, saying where it
    /// stood.
    pub fn withdraw(&self, id: &str) -> Withdrawal {
        let mut inner = self.inner();
        let Some(key) = question_key(&inner, None, id) else {
            return Withdrawal::Settled;
        };
        match inner.questions.remove(&key) {
            Some(Question {
                delivered_ms: Some(_),
                ..
            }) => Withdrawal::Unanswered,
            Some(_) => Withdrawal::Unsent,
            None => Withdrawal::Settled,
        }
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
    pub(crate) fn take_restart(&self) -> Option<Restart> {
        let hub = self.hub.as_ref()?;
        let first = hub
            .recovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.component.clone());
        if first { hub.restart() } else { None }
    }

    /// Announce a running ordinary chat's bridge; see [`Link::register_as`].
    pub fn register(&self) -> (Registration, mpsc::UnboundedReceiver<Notice>) {
        self.register_as(Purpose::Chat)
    }

    /// Announce a running bridge carrying `purpose`, which then stores each
    /// [`Notice`] it receives. Dropping the registration withdraws it.
    pub fn register_as(&self, purpose: Purpose) -> (Registration, mpsc::UnboundedReceiver<Notice>) {
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
                    purpose,
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

#[cfg(feature = "email")]
impl Link {
    /// Announce this email account, which reports to the mail chats
    /// `routes`; `None` for a link to no daemon.
    pub(crate) fn register_mail(&self, routes: Vec<String>) -> Option<MailRegistration> {
        self.hub
            .as_ref()
            .map(|hub| hub.register_mail(&self.component, routes))
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
    /// Load this mail chat's record of its keyed notices into the hub's
    /// mirror, replacing whatever the mirror held for it.
    pub(crate) fn load_keyed(&self, keys: impl IntoIterator<Item = (String, KeyedOutcome)>) {
        let Some(hub) = &self.hub else { return };
        let mut inner = hub.inner();
        inner
            .keyed
            .retain(|(component, _), _| component != &self.component);
        for (key, outcome) in keys.into_iter().take(MAX_KEYED_PER_CHAT) {
            inner.keyed.insert((self.component.clone(), key), outcome);
        }
    }

    /// Record how this mail chat's keyed notice `key` went.
    pub(crate) fn record_keyed(&self, key: &str, outcome: KeyedOutcome) {
        let Some(hub) = &self.hub else { return };
        let mut inner = hub.inner();
        let held = inner
            .keyed
            .keys()
            .filter(|(component, _)| component == &self.component)
            .count();
        let entry = (self.component.clone(), key.to_owned());
        if held >= MAX_KEYED_PER_CHAT && !inner.keyed.contains_key(&entry) {
            // Past its bound the chat's own record is the authority; the
            // email account then treats the key as unknown and hands over
            // again, which the chat acknowledges without a second copy.
            return;
        }
        inner.keyed.insert(entry, outcome);
    }

    /// Forget keyed notices this mail chat no longer remembers.
    pub(crate) fn forget_keyed(&self, keep: &dyn Fn(&str) -> bool) {
        let Some(hub) = &self.hub else { return };
        hub.inner()
            .keyed
            .retain(|(component, key), _| component != &self.component || keep(key));
    }

    /// Hand the owner's `command`, sent in this mail chat as `message_id`
    /// by `peer` at `sent_ms`, to the email accounts it concerns, and gather
    /// their answer; with no daemon, say nothing runs.
    pub(crate) async fn mail_command(
        &self,
        peer: &str,
        message_id: &str,
        sent_ms: Option<u64>,
        command: MailCommand,
    ) -> String {
        let Some(hub) = &self.hub else {
            return crate::mail_chat::NOT_RUNNING_REPLY.to_owned();
        };
        let evidence = ChatEvidence {
            route: self.component.clone(),
            peer: peer.to_owned(),
            message_id: message_id.to_owned(),
            sent_ms,
        };
        hub.mail_command(evidence, command).await
    }

    /// The running email accounts that report to this mail chat, with their
    /// counts.
    pub(crate) fn mail_accounts(&self) -> Vec<(String, MailCounts)> {
        self.hub
            .as_ref()
            .map(|hub| hub.mail_accounts_for(&self.component))
            .unwrap_or_default()
    }

    /// Whether the daemon found a disk holding chat files nearly full.
    pub(crate) fn low_disk(&self) -> bool {
        self.hub.as_ref().is_some_and(|hub| hub.low_disk())
    }

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

    /// When the question waiting in the direct chat with `peer` reached it,
    /// in Unix milliseconds; `None` while no question there can be answered.
    pub fn asking(&self, peer: &str) -> Option<u64> {
        let hub = self.hub.as_ref()?;
        hub.inner()
            .questions
            .get(&(self.component.clone(), peer.to_owned()))?
            .delivered_ms
    }

    /// Take the question in the direct chat with `peer` to answer it with a
    /// message sent at `sent_ms`, which must not be earlier than the question
    /// reached the chat; no question can be taken twice.
    pub fn take_question(&self, peer: &str, sent_ms: u64) -> Option<Answer> {
        let hub = self.hub.as_ref()?;
        let key = (self.component.clone(), peer.to_owned());
        let mut inner = hub.inner();
        if inner.questions.get(&key)?.delivered_ms? > sent_ms {
            return None;
        }
        inner
            .questions
            .remove(&key)
            .map(|question| Answer(question.answer))
    }

    /// Whether question `id` still waits on this account, so its text is
    /// still worth sending.
    pub(crate) fn question_waiting(&self, id: &str) -> bool {
        self.hub
            .as_ref()
            .is_some_and(|hub| question_key(&hub.inner(), Some(&self.component), id).is_some())
    }

    /// Question `id`'s text reached the chat: from now on the owner's answer
    /// counts.
    pub fn question_delivered(&self, id: &str) {
        let Some(hub) = &self.hub else { return };
        let mut inner = hub.inner();
        if let Some(key) = question_key(&inner, Some(&self.component), id)
            && let Some(question) = inner.questions.get_mut(&key)
        {
            question.delivered_ms.get_or_insert_with(unix_ms);
        }
    }

    /// Question `id`'s text could not be delivered: drop the question, which
    /// tells its asker that no answer will come.
    pub fn question_undelivered(&self, id: &str) {
        let Some(hub) = &self.hub else { return };
        let mut inner = hub.inner();
        if let Some(key) = question_key(&inner, Some(&self.component), id) {
            inner.questions.remove(&key);
        }
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

/// A running email account's entry in the hub.
pub struct MailRegistration {
    hub: Arc<Hub>,
    component: String,
    id: u64,
}

#[cfg_attr(
    not(feature = "email"),
    allow(
        dead_code,
        reason = "only an email account's ledger serves commands and draws codes and handles"
    )
)]
impl MailRegistration {
    /// Change this account's entry, if it is still this registration's.
    fn with<T>(&self, change: impl FnOnce(&mut MailAccount) -> T) -> Option<T> {
        let mut inner = self.hub.inner();
        inner
            .mail
            .get_mut(&self.component)
            .filter(|account| account.id == self.id)
            .map(change)
    }

    /// Publish the account's current counts.
    pub fn set_counts(&self, counts: MailCounts) {
        self.with(|account| account.counts = counts);
    }

    /// Take commands for this account's actions from `commands`.
    pub(crate) fn serve(&self, commands: mpsc::Sender<MailRequest>) {
        self.with(|account| account.authority = Some(commands));
    }

    /// Record `code` as this account's, unless any running account already
    /// holds it. A code is registered when it is drawn, so a quick answer
    /// always finds its account.
    pub(crate) fn claim_code(&self, code: &str) -> bool {
        let mut inner = self.hub.inner();
        if inner
            .mail
            .values()
            .any(|account| account.codes.contains(code))
        {
            return false;
        }
        inner
            .mail
            .get_mut(&self.component)
            .filter(|account| account.id == self.id)
            .is_some_and(|account| account.codes.insert(code.to_owned()))
    }

    /// Forget codes this account no longer answers for.
    pub(crate) fn forget_codes(&self, codes: &[String]) {
        self.with(|account| {
            for code in codes {
                account.codes.remove(code);
            }
        });
    }

    /// Record `handle` as this account's, unless any running account already
    /// holds it.
    pub(crate) fn claim_handle(&self, handle: &str) -> bool {
        let mut inner = self.hub.inner();
        if inner
            .mail
            .values()
            .any(|account| account.handles.contains(handle))
        {
            return false;
        }
        inner
            .mail
            .get_mut(&self.component)
            .filter(|account| account.id == self.id)
            .is_some_and(|account| account.handles.insert(handle.to_owned()))
    }

    /// Forget handles this account no longer answers for.
    pub(crate) fn forget_handles(&self, handles: &[String]) {
        self.with(|account| {
            for handle in handles {
                account.handles.remove(handle);
            }
        });
    }

    /// Count an action as under way, unless a planned restart drains mail:
    /// then nothing new starts. The flag is read under the lock the count
    /// is kept under, so a restart never misses an action that starts.
    /// The server's restarter reads the total through [`Hub::mail_executing`].
    pub fn begin_execution(&self) -> bool {
        let mut inner = self.hub.inner();
        if self.hub.mail_draining() {
            return false;
        }
        inner
            .mail
            .get_mut(&self.component)
            .filter(|account| account.id == self.id)
            .map(|account| account.executing += 1)
            .is_some()
    }

    /// An action counted by [`MailRegistration::begin_execution`] ended.
    pub fn end_execution(&self) {
        self.with(|account| account.executing = account.executing.saturating_sub(1));
    }

    /// The owner of the running chat account `route`: `None` while it is not
    /// running or has no owner.
    pub(crate) fn chat_owner(&self, route: &str) -> Option<String> {
        self.hub.owner(route).flatten()
    }

    /// The hub this account reports through.
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }
}

impl Drop for MailRegistration {
    fn drop(&mut self) {
        let mut inner = self.hub.inner();
        if inner
            .mail
            .get(&self.component)
            .is_some_and(|account| account.id == self.id)
        {
            inner.mail.remove(&self.component);
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

/// The chat that holds question `id`, on `component` when one is given.
fn question_key(inner: &Inner, component: Option<&str>, id: &str) -> Option<(String, String)> {
    inner
        .questions
        .iter()
        .find(|((holder, _), question)| {
            question.id == id && component.is_none_or(|component| component == holder)
        })
        .map(|(key, _)| key.clone())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Now, in Unix milliseconds.
pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
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
