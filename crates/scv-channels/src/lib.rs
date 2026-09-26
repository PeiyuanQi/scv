//! SCV's chat channels: WeChat ([`wechat`], feature `wechat`) and Feishu or
//! Lark ([`feishu`], feature `feishu`), on the bridge they share.
//!
//! The bridge claims inbound messages durably, runs each conversation's
//! turns in order on its own SCV daemon session, and delivers replies with
//! stable retry identities. A channel supplies only its `Transport`:
//! receiving messages and sending them. The daemon runs an account with
//! [`run`] and manages saved accounts through [`ChannelKind::accounts`].

// Without a channel the bridge has no transport to run, and the channel
// dispatch has no arms that use their inputs.
#![cfg_attr(
    not(any(feature = "wechat", feature = "feishu")),
    allow(
        dead_code,
        unused_variables,
        reason = "a build without channels only checks the shared code"
    )
)]

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use scv_protocol::Attachment;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use uuid::Uuid;

use intake::{Conversation, Verdict};

pub use scv_client::Layout;
mod channel;
#[cfg(feature = "feishu")]
pub mod feishu;
pub mod hub;
mod intake;
pub mod media;
pub mod retry;
pub mod session;
pub mod state;
#[cfg(feature = "wechat")]
pub mod wechat;

pub use channel::{AccountRun, Accounts, Channel, ChannelCredentials, ChannelKind, run};
pub use media::{MediaKind, MediaOptions, MediaSettings};

/// Bytes in one outbound message; longer replies go out in parts.
pub const MAX_REPLY_BYTES: usize = 16 * 1024;
/// A turn's whole answer. Beyond one message it is sent in parts.
pub const MAX_TOTAL_REPLY_BYTES: usize = 4 * MAX_REPLY_BYTES;
pub const FAILURE_REPLY: &str = "SCV could not complete that request.";

/// The reply to a message whose turn a planned restart interrupted.
pub fn restarted_reply(restart: &hub::Restart) -> String {
    format!(
        "SCV restarted to update to v{} before finishing this; ask again if you still need it.",
        restart.to_version
    )
}

/// The notice telling a direct chat which of its background jobs stopped
/// with the previous run: a planned restart when `restart` is set,
/// otherwise an unexpected stop of the daemon or the account's bridge.
pub fn stopped_jobs_notice(restart: Option<&hub::Restart>, jobs: &[state::RunningJob]) -> String {
    let mut notice = match restart {
        Some(restart) => format!(
            "SCV restarted to update to v{}, which stopped background work that was still running:",
            restart.to_version
        ),
        None => {
            "An unexpected interruption stopped background work that was still running:".to_owned()
        }
    };
    for job in jobs {
        let agent = job.tool.strip_prefix("agent_").unwrap_or(&job.tool);
        notice.push_str(&format!("\n- {} ({agent})", job.job));
        if !job.task.is_empty() {
            notice.push_str(&format!(": {}", job.task));
        }
    }
    notice.push_str("\nAsk again if you still need it.");
    notice
}
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
/// Owner turns may run tools and delegated agents, which take longer.
pub const OWNER_TURN_TIMEOUT: Duration = Duration::from_secs(1800);
/// Model time an owner turn keeps beyond its longest single tool call.
const OWNER_TURN_MARGIN: Duration = Duration::from_secs(300);
/// Senders whose turns may run at once.
pub const MAX_CONCURRENT_TURNS: usize = 4;
/// Claimed messages one conversation may have waiting or running.
pub const MAX_QUEUED_PER_CONVERSATION: usize = 8;
/// Claimed messages across all conversations.
const MAX_CLAIMS: usize = 64;
/// Live sender sessions.
const MAX_SESSIONS: usize = 32;
/// A conversation idle this long after its last turn closes its session.
const SESSION_IDLE: Duration = Duration::from_secs(1800);
/// How long a state write waits out a daemon command's transaction.
const STATE_BUSY_RETRY: Duration = Duration::from_secs(5);
pub const BUSY_REPLY: &str =
    "SCV is still working on your earlier messages. Please send this one again later.";
pub const HELD_HEADER: &str = "[Earlier reply that could not be delivered at the time]\n";
pub const LATEST_HEADER: &str = "[Reply to your latest message]\n";
/// Refused replies are delivered with the conversation's next reply for a week.
const HELD_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HELD_MAX_PER_CONVERSATION: usize = 4;
const HELD_MAX_BYTES_PER_CONVERSATION: usize = 4 * MAX_HELD_REPLY_BYTES;
const HELD_MAX_TOTAL: usize = 128;
/// Half a message, so a held reply always fits beside a short new one.
const MAX_HELD_REPLY_BYTES: usize = MAX_REPLY_BYTES / 2;
/// How long fetching what one message refers to may take.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long downloading one file may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// How often old media files are removed.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// The account owner granted remote tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOwner {
    /// The owner's authenticated sender ID on the channel.
    pub user_id: String,
    /// How long one owner turn may run; see [`owner_turn_timeout`].
    pub turn_timeout: Duration,
}

/// An owner turn outlasts the longest tool call the session allows
/// (`tools.max_timeout_seconds`) by a margin for the model's own work, and
/// never runs shorter than 30 minutes.
pub fn owner_turn_timeout(max_tool_timeout: Duration) -> Duration {
    max_tool_timeout
        .saturating_add(OWNER_TURN_MARGIN)
        .max(OWNER_TURN_TIMEOUT)
}

/// What a transport received in one wait.
pub struct Batch {
    pub messages: Vec<Inbound>,
    /// The checkpoint to resume from, saved once every claim in the batch is
    /// durable. `None` keeps the previous one.
    pub checkpoint: Option<String>,
}

/// One received message with an ID.
pub enum Inbound {
    /// Not something to answer (a system message, or missing a field a
    /// reply needs); it is only recorded as seen.
    Ignored { id: String },
    /// A message to answer: text, files, or both.
    Text(Message),
}

impl Inbound {
    fn id(&self) -> &str {
        match self {
            Self::Ignored { id } => id,
            Self::Text(message) => &message.id,
        }
    }
}

/// A message to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub sender: String,
    /// The text, with markers such as `[sticker]` for content that has no
    /// file to fetch; may be empty when the message is only files.
    pub text: String,
    /// The transport's handle for replying to this message.
    pub reply_to: String,
    /// The group chat it was sent in. Group messages never carry owner
    /// authority and never share the sender's direct-chat session.
    pub group: Option<String>,
    /// Files the message carries, fetched before its turn.
    pub media: Vec<Media>,
    /// What the message refers to, such as a quoted message, for the
    /// transport to resolve before the turn; opaque to the bridge.
    pub reference: Option<String>,
}

impl Message {
    /// A text message with no files or references.
    pub fn text(id: &str, sender: &str, text: &str, reply_to: &str, group: Option<&str>) -> Self {
        Self {
            id: id.into(),
            sender: sender.into(),
            text: text.into(),
            reply_to: reply_to.into(),
            group: group.map(str::to_owned),
            media: Vec::new(),
            reference: None,
        }
    }
}

/// A file a received message carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    pub kind: MediaKind,
    /// The sender's file name; empty when the platform gives none.
    pub name: String,
    /// The size the platform announced, when it did.
    pub size: Option<u64>,
    /// The type the platform declared, when it did.
    pub mime: Option<String>,
    /// What a voice message said, when the platform transcribed it.
    pub transcript: Option<String>,
    /// How the transport fetches it; opaque to the bridge.
    pub source: String,
}

/// A downloaded file.
#[derive(Debug)]
pub struct Downloaded {
    pub bytes: Vec<u8>,
    /// The type the platform declared while serving it, if any.
    pub mime: Option<String>,
}

/// What a message's reference resolved to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Text shown to the model before the message, such as the quoted
    /// message or the forwarded ones.
    pub context: String,
    /// Files the referenced messages carry.
    pub media: Vec<Media>,
}

/// A file sent as its own message after a reply's text.
pub struct OutboundFile<'a> {
    pub to: &'a str,
    /// The inbound message's reply handle; empty for a message that answers
    /// nothing.
    pub reply_to: &'a str,
    /// Which message of the reply this is, counting its text parts first.
    pub part: usize,
    pub path: &'a Path,
    pub name: &'a str,
    pub mime: &'a str,
    pub kind: MediaKind,
    /// Stable across retries of this file, including after a restart.
    pub client_id: &'a str,
}

/// One part of an outbound message.
pub struct Outbound<'a> {
    pub to: &'a str,
    /// The inbound message's reply handle; empty for a message that answers
    /// nothing, such as a background report.
    pub reply_to: &'a str,
    /// Which part of the message this is, from 0.
    pub part: usize,
    pub text: &'a str,
    /// Stable across retries of this part, including after a restart.
    pub client_id: &'a str,
}

/// How the transport answered a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered,
    /// The platform explicitly refused the message. Retrying the same
    /// request cannot succeed, so the refusal is final for that reply.
    Rejected,
}

/// A chat platform's connection for one account.
#[async_trait]
pub(crate) trait Transport: Send + Sync {
    /// Names the channel in logs and errors, such as `ClawBot`.
    fn label(&self) -> &'static str;

    /// The channel's name as its users know it, such as `WeChat`, which the
    /// model is told so it writes for a chat.
    fn channel(&self) -> &'static str;

    /// Wait for the next batch after `checkpoint`. Success is authenticated
    /// contact with the platform and reports the account healthy; an error
    /// reports it disconnected and is retried with backoff.
    ///
    /// The checkpoint is opaque to the bridge: a polling transport keeps its
    /// cursor there, and a push transport may keep its own resume point, such
    /// as the newest message time per chat for catching up after a
    /// reconnect. `receive` is called again only once every claim from the
    /// previous batch and its checkpoint are durable, so a push transport
    /// acknowledges the previous batch's events at the start of the next call.
    async fn receive(&self, checkpoint: &str) -> Result<Batch>;

    /// Send one part of a message. Retry transient failures, reporting each
    /// with `report(false)`, then fail; a refusal is `Rejected`.
    async fn send(
        &self,
        message: &Outbound<'_>,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome>;

    /// Download a received file, failing when it is larger than `max_bytes`.
    /// Errors must not carry URLs, keys, or tokens.
    async fn download(&self, media: &Media, max_bytes: u64) -> Result<Downloaded> {
        let _ = (media, max_bytes);
        bail!("{} cannot download files", self.label())
    }

    /// Resolve a message's reference, such as fetching the message it
    /// quotes. `message_id` is the message that carries it.
    async fn resolve(&self, message_id: &str, reference: &str) -> Result<Resolved> {
        let _ = (message_id, reference);
        Ok(Resolved::default())
    }

    /// Upload and send one file, as `send` does a text part.
    async fn send_file(
        &self,
        file: &OutboundFile<'_>,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome> {
        let _ = (file, report);
        bail!("{} cannot send files", self.label())
    }
}

/// What the bridge needs to run one account; a channel's `run` builds it
/// with [`AccountRun::bridge`].
pub(crate) struct BridgeRun<'a> {
    pub(crate) account: &'a str,
    pub(crate) workspace: &'a Path,
    pub(crate) socket: &'a Path,
    /// The account owner, whether or not it holds remote tools.
    pub(crate) owner: Option<&'a str>,
    /// The owner, when the account grants it remote tools; every other
    /// sender stays tool-free.
    pub(crate) tool_owner: Option<ToolOwner>,
    /// Whose messages the account answers; the rest are only marked seen.
    pub(crate) senders: state::Senders,
    /// Where received and outgoing files live and how large they may be.
    pub(crate) media: MediaOptions,
    /// The account's connection to the daemon's hub, which then sees its
    /// owner work and chats and can queue notices.
    pub(crate) link: &'a hub::Link,
    pub(crate) report: &'a (dyn Fn(bool) + Send + Sync),
}

/// Run one account over `transport` until the returned future is dropped or
/// fails. It holds the account's lifetime lock, binds delivery state to the
/// credentials the transport uses (`running` says whether they are the saved
/// ones), recovers interrupted work (describing work a planned restart
/// interrupted as such), and delivers recovered replies before receiving.
#[tracing::instrument(
    name = "channel",
    skip_all,
    fields(channel = transport.channel(), account = run.account)
)]
pub(crate) async fn serve<C: state::Credentials, T: Transport>(
    transport: &T,
    run: BridgeRun<'_>,
    store: &state::Store<C>,
    running: impl FnOnce(&C) -> Result<bool>,
) -> Result<()> {
    let BridgeRun {
        account,
        workspace,
        socket,
        owner,
        tool_owner,
        senders,
        media,
        link,
        report,
    } = run;
    let _lock = store.lock(account)?;
    let mut state = store.bind_state(account, running)?;
    recover_interrupted_after(store, account, &mut state, link.take_restart().as_ref())?;
    let (registration, mut notices) = link.register();
    let bridge = Bridge {
        transport,
        account,
        workspace,
        socket,
        store,
        report,
        owner,
        tool_owner: tool_owner.as_ref().map(|owner| owner.user_id.as_str()),
        senders,
        registration,
        media: &media,
        state: Mutex::new(state),
        turns: Semaphore::new(MAX_CONCURRENT_TURNS),
        replies: Notify::new(),
    };
    bridge.prune_media();
    // Replies recovered from an earlier run go out before the first poll.
    bridge.deliver_backlog().await?;
    // Conversations run as futures owned by this one, never as spawned tasks,
    // so cancellation drops every session and request with it.
    let (start, mut started) = mpsc::unbounded_channel();
    let mut conversations = FuturesUnordered::new();
    let poll = bridge.poll(tool_owner.as_ref(), &start);
    let deliver = bridge.deliver();
    tokio::pin!(poll, deliver);
    loop {
        tokio::select! {
            result = &mut poll => return result,
            result = &mut deliver => return result,
            Some((owner, recipient, jobs, watching)) = started.recv() => conversations.push(bridge.converse(owner, recipient, jobs, watching)),
            Some(result) = conversations.next(), if !conversations.is_empty() => result?,
            Some(notice) = notices.recv() => {
                bridge
                    .queue_unprompted(&notice.to, session::Reply::text(notice.text.clone()))
                    .await?;
                notice.stored();
            }
        }
    }
}

/// One accepted message, claimed durably before it is queued.
struct Job {
    message_id: String,
    text: String,
    media: Vec<Media>,
    reference: Option<String>,
    /// The conversation, which names the directory its files go to.
    key: String,
    limit: Duration,
}

/// A turn's input after its message's files and references are fetched.
struct Prepared {
    text: String,
    attachments: Vec<Attachment>,
}

/// Why a file was not brought into the turn: a note for the model, and a
/// reply for the sender when the message has nothing else.
struct Refusal {
    note: String,
    reply: String,
}

/// A conversation's media directory name: a digest of its key, so sender
/// and group IDs never become paths.
fn conversation_dir(key: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

enum Step {
    Idle,
    Progress,
    Retry,
}

/// A new conversation: whether its sender is the owner, who receives its
/// unprompted reports, its queue, and its `watching` flag.
type Starter = mpsc::UnboundedSender<(
    bool,
    Option<String>,
    mpsc::UnboundedReceiver<Job>,
    Arc<AtomicBool>,
)>;

struct Bridge<'a, C, T> {
    transport: &'a T,
    account: &'a str,
    workspace: &'a Path,
    socket: &'a Path,
    store: &'a state::Store<C>,
    report: &'a (dyn Fn(bool) + Send + Sync),
    /// The account owner, whether or not it holds remote tools.
    owner: Option<&'a str>,
    /// The owner granted remote tools, whose claimed messages count as owner
    /// work in the hub.
    tool_owner: Option<&'a str>,
    /// Whose messages the account answers.
    senders: state::Senders,
    registration: hub::Registration,
    media: &'a MediaOptions,
    /// The only copy of delivery state. Every change is saved while held.
    state: Mutex<state::BridgeState>,
    /// Bounds how many senders' turns run at once.
    turns: Semaphore,
    /// Wakes the delivery loop when a reply is queued.
    replies: Notify,
}

impl<C: state::Credentials, T: Transport> Bridge<'_, C, T> {
    /// Save state, waiting out a daemon command's short transaction.
    async fn save(&self, state: &state::BridgeState) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STATE_BUSY_RETRY;
        let result = loop {
            match self.store.save_state(self.account, state) {
                Err(error) if state::is_busy(&error) && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                result => break result,
            }
        };
        if result.is_ok()
            && let Some(owner) = self.tool_owner
        {
            let claims = state
                .in_flight
                .iter()
                .filter(|claim| conversation_key(&claim.key, &claim.to_user_id) == owner)
                .count();
            self.registration.set_owner_claims(claims);
        }
        result
    }

    /// Keep the durable record of `recipient`'s running background jobs in
    /// step with its session, saving only when it changed.
    async fn record_jobs(&self, recipient: &str, session: Option<&session::Session>) -> Result<()> {
        let current = session.map(session::Session::jobs).unwrap_or_default();
        let mut state = self.state.lock().await;
        let recorded: Vec<&str> = state
            .jobs
            .iter()
            .filter(|job| job.to_user_id == recipient)
            .map(|job| job.job.as_str())
            .collect();
        if recorded.len() == current.len()
            && current
                .iter()
                .all(|(job, _)| recorded.contains(&job.as_str()))
        {
            return Ok(());
        }
        let now = unix_now();
        let mut kept = Vec::new();
        for (job, info) in current {
            let started_at = state
                .jobs
                .iter()
                .find(|recorded| recorded.to_user_id == recipient && recorded.job == job)
                .map_or(now, |recorded| recorded.started_at);
            kept.push(state::RunningJob {
                to_user_id: recipient.to_owned(),
                job,
                tool: info.tool,
                task: info.task,
                started_at,
            });
        }
        state.jobs.retain(|job| job.to_user_id != recipient);
        state.jobs.extend(kept);
        self.save(&state).await
    }

    /// Receive messages, durably claim accepted ones, and queue each on its
    /// conversation. Receiving continues while turns run.
    async fn poll(&self, tool_owner: Option<&ToolOwner>, start: &Starter) -> Result<()> {
        let mut conversations: HashMap<String, Conversation> = HashMap::new();
        let mut backoff = retry::Backoff::new();
        let mut pruned = Instant::now();
        loop {
            if pruned.elapsed() >= PRUNE_EVERY {
                pruned = Instant::now();
                self.prune_media();
            }
            let cursor = self.state.lock().await.cursor.clone();
            let batch = match self.transport.receive(&cursor).await {
                Ok(batch) => {
                    (self.report)(true);
                    batch
                }
                Err(error) => {
                    tracing::warn!(error = %error, "poll failed");
                    (self.report)(false);
                    backoff.wait().await;
                    continue;
                }
            };
            backoff.reset();
            // Conversations close their queues after idling.
            conversations.retain(|_, conversation| !conversation.jobs.is_closed());
            for inbound in &batch.messages {
                self.accept(inbound, tool_owner, &mut conversations, start)
                    .await?;
            }
            let mut state = self.state.lock().await;
            if let Some(next) = batch.checkpoint {
                state.cursor = next;
            }
            // Claims from this batch are durable before the cursor moves past it.
            self.save(&state).await?;
            drop(state);
            // Also yield for immediately-ready mocked transports and empty batches.
            tokio::task::yield_now().await;
        }
    }

    async fn accept(
        &self,
        inbound: &Inbound,
        tool_owner: Option<&ToolOwner>,
        conversations: &mut HashMap<String, Conversation>,
        start: &Starter,
    ) -> Result<()> {
        let id = inbound.id();
        let mut state = self.state.lock().await;
        let verdict = intake::classify(
            inbound,
            &intake::Intake {
                state: &state,
                conversations,
                owner: self.owner,
                tool_owner,
                senders: self.senders,
            },
        );
        let (sender, turn) = match verdict {
            Verdict::Ignore => {
                mark_seen(&mut state, id);
                return self.save(&state).await;
            }
            Verdict::Stranger => {
                // Neither who sent it nor what it says reaches the log.
                tracing::info!("ignored a message from someone other than the account owner");
                mark_seen(&mut state, id);
                return self.save(&state).await;
            }
            Verdict::Claimed => return Ok(()),
            Verdict::Busy(sender) => (sender, None),
            Verdict::Turn {
                sender,
                owner,
                limit,
                evict,
            } => (sender, Some((owner, limit, evict))),
        };
        let message = sender.message;
        if sender.owner_chat {
            self.registration.owner_wrote(&message.sender);
        }
        let key = sender.key;
        let Some((owner, limit, evict)) = turn else {
            tracing::warn!("at the work limit; asking a sender to retry later");
            let mut busy = new_pending(
                id,
                &message.sender,
                &message.reply_to,
                BUSY_REPLY,
                MAX_REPLY_BYTES,
            );
            busy.key = key;
            busy.transient = true;
            state.pending.push(busy);
            self.save(&state).await?;
            drop(state);
            self.replies.notify_one();
            return Ok(());
        };
        state.in_flight.push(state::InFlight {
            message_id: id.to_owned(),
            to_user_id: message.sender.clone(),
            context_token: message.reply_to.clone(),
            key: key.clone(),
        });
        self.save(&state).await?;
        drop(state);
        if let Some(evict) = evict {
            // Dropping its queue ends that conversation after its current turn.
            conversations.remove(&evict);
        }
        let mut job = Job {
            message_id: id.to_owned(),
            text: message.text.clone(),
            media: message.media.clone(),
            reference: message.reference.clone(),
            key: key.clone(),
            limit,
        };
        loop {
            if let Some(conversation) = conversations.get_mut(&key) {
                match conversation.jobs.send(job) {
                    Ok(()) => {
                        conversation.last_used = Instant::now();
                        return Ok(());
                    }
                    // The conversation idled out; start a fresh one.
                    Err(mpsc::error::SendError(returned)) => {
                        job = returned;
                        conversations.remove(&key);
                    }
                }
            } else {
                let (jobs, queue) = mpsc::unbounded_channel();
                // Only a direct chat can receive unprompted background reports.
                let recipient = message.group.is_none().then(|| message.sender.clone());
                let watching = Arc::new(AtomicBool::new(false));
                start
                    .send((owner, recipient, queue, Arc::clone(&watching)))
                    .map_err(|_| {
                        anyhow!("{} conversation runner stopped", self.transport.label())
                    })?;
                conversations.insert(
                    key.clone(),
                    Conversation {
                        jobs,
                        last_used: Instant::now(),
                        watching,
                    },
                );
            }
        }
    }

    /// Run one conversation's turns in order on its own SCV session. A turn
    /// that fails on the server keeps the session; a broken or timed-out one
    /// resets it. While background jobs the session started are running,
    /// the conversation stays open and sends each finished job's report to
    /// `recipient` as an unprompted message.
    async fn converse(
        &self,
        owner: bool,
        recipient: Option<String>,
        mut jobs: mpsc::UnboundedReceiver<Job>,
        watching_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        enum Next {
            Job(Option<Job>),
            Idle,
            Report(Result<session::Reply>),
        }
        let mut session: Option<session::Session> = None;
        // The daemon sees which session a direct chat runs on and its
        // unreported background work, so a planned restart can wait for it.
        let tracker = recipient
            .as_deref()
            .map(|peer| self.registration.conversation(peer));
        loop {
            if let Some(recipient) = &recipient {
                self.record_jobs(recipient, session.as_ref()).await?;
            }
            if let Some(tracker) = &tracker {
                tracker.update(
                    session.as_ref().map(|session| session.session_id.as_str()),
                    session.as_ref().map_or(0, session::Session::pending_work),
                );
            }
            // A running report turn is watched too, or its answer would wait
            // unread until the chat's next message.
            let watching = recipient.is_some()
                && session.as_ref().is_some_and(|session| {
                    session.background_jobs() > 0 || session.has_reports() || session.reporting()
                });
            // Tell the poller, so a full session table never closes this
            // conversation while it has work in flight.
            watching_flag.store(
                watching
                    || session
                        .as_ref()
                        .is_some_and(|session| session.background_jobs() > 0),
                Ordering::Release,
            );
            let next = {
                let message = async {
                    if watching {
                        Next::Job(jobs.recv().await)
                    } else {
                        match tokio::time::timeout(SESSION_IDLE, jobs.recv()).await {
                            Ok(job) => Next::Job(job),
                            Err(_) => Next::Idle,
                        }
                    }
                };
                let report = async {
                    match session.as_mut() {
                        Some(session) if watching => Next::Report(session.next_report().await),
                        _ => std::future::pending().await,
                    }
                };
                tokio::select! {
                    next = message => next,
                    next = report => next,
                }
            };
            let job = match next {
                Next::Idle | Next::Job(None) => {
                    // Closing the session stops its jobs; no restart will.
                    if let Some(recipient) = &recipient {
                        self.record_jobs(recipient, None).await?;
                    }
                    return Ok(());
                }
                Next::Report(Ok(report)) => {
                    if let Some(recipient) = &recipient {
                        self.queue_unprompted(recipient, report).await?;
                    }
                    continue;
                }
                Next::Report(Err(_)) => {
                    session = None;
                    continue;
                }
                Next::Job(Some(job)) => job,
            };
            let reply = {
                let _turn = self
                    .turns
                    .acquire()
                    .await
                    .map_err(|_| anyhow!("{} turn limiter closed", self.transport.label()))?;
                let result = tokio::time::timeout(job.limit, async {
                    let prepared = match self.prepare(&job, owner).await {
                        Ok(prepared) => prepared,
                        // Nothing the model could use: tell the sender why.
                        Err(refusal) => return Ok(session::Reply::text(refusal)),
                    };
                    if session.is_none() {
                        if owner {
                            tracing::info!("owner session starts with remote tools");
                        }
                        session = Some(
                            session::Session::connect(
                                self.socket,
                                self.workspace,
                                owner,
                                Some(self.transport.channel()),
                            )
                            .await?,
                        );
                    }
                    session
                        .as_mut()
                        .expect("session was just connected")
                        .turn(&prepared.text, prepared.attachments, MAX_TOTAL_REPLY_BYTES)
                        .await
                })
                .await;
                match result {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(_)) => {
                        // A turn the server failed leaves a healthy session,
                        // and with it any background jobs it runs.
                        if session.as_ref().is_none_or(session::Session::is_broken) {
                            session = None;
                        }
                        session::Reply::text(FAILURE_REPLY)
                    }
                    Err(_) => {
                        // Out of time: cancel the turn but keep a session
                        // that has background jobs running.
                        let keep = match session.as_mut() {
                            Some(session) if session.background_jobs() > 0 => {
                                session.cancel_current().await.unwrap_or(false)
                                    && !session.is_broken()
                            }
                            _ => false,
                        };
                        if !keep {
                            session = None;
                        }
                        session::Reply::text(FAILURE_REPLY)
                    }
                }
            };
            let (text, files) = self.outgoing(reply);
            // The claim becomes a durable pending reply in one state write.
            let mut state = self.state.lock().await;
            let index = state
                .in_flight
                .iter()
                .position(|claim| claim.message_id == job.message_id)
                .ok_or_else(|| anyhow!("{} lost a claimed message", self.transport.label()))?;
            let claim = state.in_flight.remove(index);
            let mut pending = compose_pending(&mut state, &claim, &text, unix_now());
            pending.files = files;
            state.pending.push(pending);
            self.save(&state).await?;
            drop(state);
            self.replies.notify_one();
            // Background reports that finished during the turn follow it.
            let reports = session
                .as_mut()
                .map(session::Session::take_reports)
                .unwrap_or_default();
            if let Some(recipient) = &recipient {
                for report in reports {
                    self.queue_unprompted(recipient, report).await?;
                }
            }
        }
    }

    /// Fetch what a message refers to and the files it carries, as a turn's
    /// text and attachments. When nothing usable is left, the error is a
    /// short reply for the sender instead of a turn.
    async fn prepare(&self, job: &Job, owner: bool) -> std::result::Result<Prepared, String> {
        let mut context = String::new();
        let mut media = job.media.clone();
        if let Some(reference) = &job.reference {
            match tokio::time::timeout(
                RESOLVE_TIMEOUT,
                self.transport.resolve(&job.message_id, reference),
            )
            .await
            {
                Ok(Ok(resolved)) => {
                    context = resolved.context;
                    media.extend(resolved.media);
                }
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "could not resolve a message reference");
                    context = "[The message this refers to could not be loaded.]".into();
                }
                Err(_) => context = "[The message this refers to could not be loaded.]".into(),
            }
        }
        let mut notes = Vec::new();
        let mut attachments = Vec::new();
        let mut refusals = Vec::new();
        let mut heard = false;
        let skipped = media.len().saturating_sub(media::MAX_MESSAGE_MEDIA);
        let dir = self.media.inbox.join(conversation_dir(&job.key));
        for item in media.iter().take(media::MAX_MESSAGE_MEDIA) {
            match self.fetch(item, owner, &dir).await {
                Ok(attachment) => attachments.push(attachment),
                Err(Refusal { note, reply }) => {
                    // A voice message's transcript is worth a turn by itself.
                    if item
                        .transcript
                        .as_deref()
                        .is_some_and(|transcript| !transcript.trim().is_empty())
                    {
                        heard = true;
                    }
                    notes.push(note);
                    refusals.push(reply);
                }
            }
        }
        if skipped > 0 {
            notes.push(format!("[{skipped} more files were not opened]"));
        }
        let mut text = String::new();
        for part in [context.trim(), job.text.trim()] {
            if !part.is_empty() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(part);
            }
        }
        for note in &notes {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(note);
        }
        if job.text.trim().is_empty()
            && context.trim().is_empty()
            && attachments.is_empty()
            && !heard
        {
            return Err(refusals
                .into_iter()
                .next()
                .unwrap_or_else(|| "SCV could not read that message.".into()));
        }
        Ok(Prepared { text, attachments })
    }

    /// Download one file under the sender's limits and save it privately.
    async fn fetch(
        &self,
        item: &Media,
        owner: bool,
        dir: &Path,
    ) -> std::result::Result<Attachment, Refusal> {
        let noun = item.kind.noun();
        let label = if item.name.is_empty() {
            noun.to_owned()
        } else {
            format!("{noun} {}", media::safe_name(&item.name))
        };
        let transcript = item
            .transcript
            .as_deref()
            .map(str::trim)
            .filter(|transcript| !transcript.is_empty());
        let said = transcript.map_or_else(String::new, |t| format!(" It says: \"{t}\""));
        let Some(limit) = self.media.settings.limit(item.kind, owner) else {
            let reply = if owner || self.media.settings.owner_max_mib == 0 {
                "Receiving files is turned off for this SCV account.".to_owned()
            } else if item.kind == MediaKind::Image {
                "SCV cannot open pictures from you here; please send text.".to_owned()
            } else {
                format!("SCV can read text and pictures from you here, but not a {noun}.")
            };
            return Err(Refusal {
                note: format!("[{label}: not opened for this sender]{said}"),
                reply,
            });
        };
        if item.size.is_some_and(|size| size > limit) {
            return Err(Refusal {
                note: format!(
                    "[{label}: not downloaded, larger than the {} MB limit]{said}",
                    limit / (1024 * 1024)
                ),
                reply: format!(
                    "That {noun} is larger than SCV's {} MB limit.",
                    limit / (1024 * 1024)
                ),
            });
        }
        let failed = || Refusal {
            note: format!("[{label}: download failed]{said}"),
            reply: format!("SCV could not download that {noun}. Please send it again."),
        };
        let downloaded = match tokio::time::timeout(
            DOWNLOAD_TIMEOUT,
            self.transport.download(item, limit),
        )
        .await
        {
            Ok(Ok(downloaded)) => downloaded,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, noun, "could not download a file");
                return Err(failed());
            }
            Err(_) => {
                tracing::warn!(noun, "timed out downloading a file");
                return Err(failed());
            }
        };
        if downloaded.bytes.len() as u64 > limit {
            return Err(Refusal {
                note: format!(
                    "[{label}: not kept, larger than the {} MB limit]{said}",
                    limit / (1024 * 1024)
                ),
                reply: format!(
                    "That {noun} is larger than SCV's {} MB limit.",
                    limit / (1024 * 1024)
                ),
            });
        }
        let mime = media::mime_type(
            downloaded.mime.as_deref().or(item.mime.as_deref()),
            &item.name,
            &downloaded.bytes[..downloaded.bytes.len().min(64)],
        );
        let name = if item.name.trim().is_empty() {
            format!("{}.{}", item.kind.as_str(), media::extension(&mime))
        } else {
            item.name.clone()
        };
        let dir = dir.to_path_buf();
        let bytes = downloaded.bytes;
        let size = bytes.len() as u64;
        let saved = tokio::task::spawn_blocking(move || media::save(&dir, &name, &bytes))
            .await
            .map_err(|error| anyhow!(error))
            .and_then(|result| result);
        let path = match saved {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(error = %error, noun, "could not save a file");
                return Err(failed());
            }
        };
        Ok(Attachment {
            kind: item.kind.as_str().into(),
            path: path.display().to_string(),
            name: media::safe_name(&item.name),
            mime,
            size,
            transcript: transcript.map(str::to_owned),
        })
    }

    /// A reply's text and the files it can send. A file that is not a
    /// regular file in the outbox, or beyond the per-reply limit, is
    /// dropped with a note; captions go into the text.
    fn outgoing(&self, reply: session::Reply) -> (String, Vec<state::PendingFile>) {
        let mut text = if reply.text.trim().is_empty() && reply.files.is_empty() {
            "SCV completed without a text response.".to_owned()
        } else {
            reply.text
        };
        let mut files = Vec::new();
        let mut dropped = 0;
        for file in reply.files {
            let path = PathBuf::from(&file.path);
            if files.len() >= media::MAX_REPLY_FILES
                || file.size > media::MAX_REPLY_FILE_BYTES
                || !media::is_inside(&self.media.outbox, &path)
            {
                dropped += 1;
                continue;
            }
            let name = match media::safe_name(&file.name) {
                name if name.is_empty() => "file".to_owned(),
                name => name,
            };
            let head = media::head(&path);
            let mime = media::mime_type(Some(&file.mime), &name, &head);
            if !file.caption.trim().is_empty() {
                if !text.trim().is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!("{name}: {}", file.caption.trim()));
            }
            files.push(state::PendingFile {
                path: file.path,
                kind: MediaKind::for_mime(&mime),
                name,
                mime,
                client_id: Uuid::new_v4().to_string(),
            });
        }
        if dropped > 0 {
            tracing::warn!(dropped, "dropped attached files");
            text.push_str(&format!("\n\n[{dropped} attached files could not be sent]"));
        }
        (text, files)
    }

    /// Remove received and sent files past their retention.
    fn prune_media(&self) {
        let keep = self.media.settings.keep();
        let removed =
            media::prune(&self.media.inbox, keep) + media::prune(&self.media.outbox, keep);
        if removed > 0 {
            tracing::info!(removed, "removed old media files");
        }
    }

    /// Queue a message to `recipient` that answers no inbound message, such
    /// as a finished background job's report. It is sent without a reply
    /// handle, durably and with stable client IDs like any reply.
    async fn queue_unprompted(&self, recipient: &str, report: session::Reply) -> Result<()> {
        let (text, files) = self.outgoing(report);
        let mut state = self.state.lock().await;
        let mut pending = new_pending("", recipient, "", &text, MAX_REPLY_BYTES);
        pending.key = recipient.to_owned();
        pending.files = files;
        state.pending.push(pending);
        self.save(&state).await?;
        drop(state);
        self.replies.notify_one();
        Ok(())
    }

    /// Deliver every queued reply before polling starts. A reply that still
    /// cannot be sent fails the run so the supervisor retries with backoff.
    async fn deliver_backlog(&self) -> Result<()> {
        loop {
            match self.deliver_next().await? {
                Step::Idle => return Ok(()),
                Step::Progress => {}
                Step::Retry => bail!("{} could not deliver the reply", self.transport.label()),
            }
        }
    }

    /// Deliver queued replies in order for the life of the run.
    async fn deliver(&self) -> Result<()> {
        let mut backoff = retry::Backoff::new();
        loop {
            match self.deliver_next().await? {
                Step::Idle => self.replies.notified().await,
                Step::Progress => backoff.reset(),
                Step::Retry => backoff.wait().await,
            }
        }
    }

    /// Send the oldest pending reply: its text parts, then its files. Only
    /// this path edits or removes pending replies, so the first entry stays
    /// the same one between state locks.
    async fn deliver_next(&self) -> Result<Step> {
        let mut pending = {
            let mut state = self.state.lock().await;
            let Some(pending) = state.pending.first_mut() else {
                return Ok(Step::Idle);
            };
            let chunks = text_chunks(pending).len();
            let mut changed = false;
            while pending.client_ids.len() < chunks {
                pending.client_ids.push(Uuid::new_v4().to_string());
                changed = true;
            }
            if pending.next_chunk > chunks {
                pending.next_chunk = 0;
                changed = true;
            }
            let pending = pending.clone();
            if changed {
                self.save(&state).await?;
            }
            pending
        };
        let chunks = text_chunks(&pending);
        let mut refused = false;
        while pending.next_chunk < chunks.len() {
            let index = pending.next_chunk;
            let message = Outbound {
                to: &pending.to_user_id,
                reply_to: &pending.context_token,
                part: index,
                text: &chunks[index],
                client_id: &pending.client_ids[index],
            };
            match self.transport.send(&message, self.report).await {
                Ok(SendOutcome::Delivered) => {}
                Ok(SendOutcome::Rejected) => {
                    refused = true;
                    break;
                }
                Err(_) => return Ok(Step::Retry),
            }
            pending.next_chunk += 1;
            let mut state = self.state.lock().await;
            state.pending[0].next_chunk = pending.next_chunk;
            self.save(&state).await?;
        }
        // Files follow a delivered text; a refused text drops them.
        while !refused && pending.next_file < pending.files.len() {
            let index = pending.next_file;
            let file = &pending.files[index];
            let path = PathBuf::from(&file.path);
            let outbound = OutboundFile {
                to: &pending.to_user_id,
                reply_to: &pending.context_token,
                part: chunks.len() + index,
                path: &path,
                name: &file.name,
                mime: &file.mime,
                kind: file.kind,
                client_id: &file.client_id,
            };
            if media::is_inside(&self.media.outbox, &path) {
                match self.transport.send_file(&outbound, self.report).await {
                    Ok(SendOutcome::Delivered) => {}
                    Ok(SendOutcome::Rejected) => {
                        tracing::warn!("the platform refused an attached file");
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "could not send an attached file");
                        return Ok(Step::Retry);
                    }
                }
            } else {
                tracing::warn!("skipped a file that left its outbox");
            }
            pending.next_file += 1;
            let mut state = self.state.lock().await;
            state.pending[0].next_file = pending.next_file;
            self.save(&state).await?;
        }
        let mut state = self.state.lock().await;
        state.pending.remove(0);
        if !pending.message_id.is_empty() {
            mark_seen(&mut state, &pending.message_id);
        }
        if refused {
            if !pending.files.is_empty() {
                tracing::warn!(
                    files = pending.files.len(),
                    "dropped the files of a refused reply"
                );
            }
            hold_refused(&mut state, &pending, &chunks, unix_now());
        }
        self.save(&state).await?;
        drop(state);
        for file in &pending.files {
            let _ = std::fs::remove_file(&file.path);
        }
        Ok(Step::Progress)
    }
}

/// A pending reply's text parts; none when it is only files.
fn text_chunks(pending: &state::PendingDelivery) -> Vec<String> {
    if pending.reply.is_empty() && !pending.files.is_empty() {
        Vec::new()
    } else {
        split_utf8(&pending.reply, MAX_REPLY_BYTES)
    }
}

/// The conversation a claim or reply belongs to. Older state recorded none,
/// which means the direct chat with the recipient.
fn conversation_key<'a>(key: &'a str, to_user_id: &'a str) -> &'a str {
    if key.is_empty() { to_user_id } else { key }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Build the reply to a claimed message. Earlier refused replies to the same
/// conversation ride ahead of it, oldest first, as far as one message allows.
fn compose_pending(
    state: &mut state::BridgeState,
    claim: &state::InFlight,
    own: &str,
    now: u64,
) -> state::PendingDelivery {
    prune_held(state, now);
    let key = conversation_key(&claim.key, &claim.to_user_id).to_owned();
    let own_bytes = LATEST_HEADER.len() + own.len();
    let mut body = String::new();
    let mut carried = Vec::new();
    let mut index = 0;
    while index < state.held.len() {
        if state.held[index].key != key {
            index += 1;
            continue;
        }
        let bytes = HELD_HEADER.len() + state.held[index].reply.len() + 2;
        if body.len() + bytes + own_bytes > MAX_REPLY_BYTES {
            break;
        }
        let held = state.held.remove(index);
        body.push_str(HELD_HEADER);
        body.push_str(&held.reply);
        body.push_str("\n\n");
        carried.push(held);
    }
    let reply = if carried.is_empty() {
        own.to_owned()
    } else {
        body.push_str(LATEST_HEADER);
        body.push_str(own);
        body
    };
    let mut pending = new_pending(
        &claim.message_id,
        &claim.to_user_id,
        &claim.context_token,
        &reply,
        MAX_REPLY_BYTES,
    );
    pending.key = claim.key.clone();
    if !carried.is_empty() {
        pending.own = Some(own.to_owned());
        pending.carried = carried;
    }
    pending
}

/// Keep a refused reply for the conversation's next message. Carried replies
/// that never reached the sender return to the store ahead of this one.
fn hold_refused(
    state: &mut state::BridgeState,
    pending: &state::PendingDelivery,
    chunks: &[String],
    now: u64,
) {
    if pending.transient {
        return;
    }
    let key = conversation_key(&pending.key, &pending.to_user_id).to_owned();
    let reply = if pending.next_chunk == 0 {
        state.held.extend(pending.carried.iter().cloned());
        pending.own.clone().unwrap_or_else(|| pending.reply.clone())
    } else {
        chunks[pending.next_chunk..].concat()
    };
    state.held.push(state::HeldReply {
        key,
        to_user_id: pending.to_user_id.clone(),
        reply: truncate_held(reply),
        held_at: now,
    });
    // Stable: restored replies keep their place ahead of newer ones.
    state.held.sort_by_key(|held| held.held_at);
    prune_held(state, now);
}

/// A held reply must fit in one message beside a new reply.
fn truncate_held(reply: String) -> String {
    const MARKER: &str = "\n[truncated]";
    if reply.len() <= MAX_HELD_REPLY_BYTES {
        return reply;
    }
    let kept = scv_client::text::utf8_prefix(&reply, MAX_HELD_REPLY_BYTES - MARKER.len());
    format!("{kept}{MARKER}")
}

/// Drop held replies past their age, then the oldest beyond each
/// conversation's count and byte limits and the overall count.
fn prune_held(state: &mut state::BridgeState, now: u64) {
    let before = state.held.len();
    state
        .held
        .retain(|held| now.saturating_sub(held.held_at) < HELD_MAX_AGE.as_secs());
    let mut usage: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut keep = vec![false; state.held.len()];
    let mut total = 0;
    for (index, held) in state.held.iter().enumerate().rev() {
        let (count, bytes) = usage.entry(held.key.as_str()).or_default();
        if total < HELD_MAX_TOTAL
            && *count < HELD_MAX_PER_CONVERSATION
            && *bytes + held.reply.len() <= HELD_MAX_BYTES_PER_CONVERSATION
        {
            keep[index] = true;
            *count += 1;
            *bytes += held.reply.len();
            total += 1;
        }
    }
    let mut keep = keep.into_iter();
    state.held.retain(|_| keep.next().unwrap_or(false));
    let dropped = before - state.held.len();
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "channel discarded undelivered replies past their limits"
        );
    }
}

/// Turn every claim an interrupted run left into a failure reply, durably.
/// Interrupted work is never resubmitted.
pub fn recover_interrupted<C: state::Credentials>(
    store: &state::Store<C>,
    account: &str,
    state: &mut state::BridgeState,
) -> Result<()> {
    recover_interrupted_after(store, account, state, None)
}

/// [`recover_interrupted`], describing the interruption: after a planned
/// `restart`, claims get [`restarted_reply`] instead of [`FAILURE_REPLY`].
/// Each direct chat whose background jobs the previous run left running is
/// told which stopped. Everything is saved in one write.
pub fn recover_interrupted_after<C: state::Credentials>(
    store: &state::Store<C>,
    account: &str,
    state: &mut state::BridgeState,
    restart: Option<&hub::Restart>,
) -> Result<()> {
    if state.in_flight.is_empty() && state.jobs.is_empty() {
        return Ok(());
    }
    let now = unix_now();
    let reply = restart.map_or_else(|| FAILURE_REPLY.to_owned(), restarted_reply);
    for claim in std::mem::take(&mut state.in_flight) {
        let pending = compose_pending(state, &claim, &reply, now);
        state.pending.push(pending);
    }
    let jobs = std::mem::take(&mut state.jobs);
    let mut recipients: Vec<&str> = Vec::new();
    for job in &jobs {
        if !recipients.contains(&job.to_user_id.as_str()) {
            recipients.push(&job.to_user_id);
        }
    }
    for recipient in recipients {
        let stopped: Vec<_> = jobs
            .iter()
            .filter(|job| job.to_user_id == recipient)
            .cloned()
            .collect();
        let notice = stopped_jobs_notice(restart, &stopped);
        let mut pending = new_pending("", recipient, "", &notice, MAX_REPLY_BYTES);
        pending.key = recipient.to_owned();
        state.pending.push(pending);
    }
    store.save_state(account, state)
}

/// A pending delivery of `reply` with a fresh client ID per part.
pub fn new_pending(
    message_id: &str,
    to_user_id: &str,
    context_token: &str,
    reply: &str,
    max_bytes: usize,
) -> state::PendingDelivery {
    let chunks = split_utf8(reply, max_bytes);
    state::PendingDelivery {
        message_id: message_id.to_owned(),
        to_user_id: to_user_id.to_owned(),
        context_token: context_token.to_owned(),
        reply: reply.to_owned(),
        client_ids: chunks.iter().map(|_| Uuid::new_v4().to_string()).collect(),
        ..Default::default()
    }
}

fn mark_seen(state: &mut state::BridgeState, id: &str) {
    if let Some(index) = state.seen.iter().position(|seen| seen == id) {
        state.seen.remove(index);
    }
    state.seen.push(id.to_owned());
    let excess = state.seen.len().saturating_sub(4096);
    state.seen.drain(..excess);
}

/// Split `value` into parts of at most `max` bytes on character boundaries;
/// a character longer than `max` is its own part.
pub fn split_utf8(value: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    let max = max.max(1);
    while rest.len() > max {
        let mut end = max;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = rest
                .char_indices()
                .nth(1)
                .map_or(rest.len(), |(index, _)| index);
        }
        out.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests;
