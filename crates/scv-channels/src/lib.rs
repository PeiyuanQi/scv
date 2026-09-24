//! The bridge every chat channel shares: it claims inbound messages durably,
//! runs each conversation's turns in order on its own SCV daemon session,
//! and delivers replies with stable retry identities. A channel supplies
//! only its [`Transport`]: receiving messages and sending them.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use uuid::Uuid;

pub mod session;
pub mod state;

/// Bytes in one outbound message; longer replies go out in parts.
pub const MAX_REPLY_BYTES: usize = 16 * 1024;
/// A turn's whole answer. Beyond one message it is sent in parts.
pub const MAX_TOTAL_REPLY_BYTES: usize = 4 * MAX_REPLY_BYTES;
pub const FAILURE_REPLY: &str = "SCV could not complete that request.";
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
    /// Not something to answer (not text, or missing a field a reply
    /// needs); it is only recorded as seen.
    Ignored {
        id: String,
    },
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

/// A text message to answer.
pub struct Message {
    pub id: String,
    pub sender: String,
    pub text: String,
    /// The transport's handle for replying to this message.
    pub reply_to: String,
    /// The group chat it was sent in. Group messages never carry owner
    /// authority and never share the sender's direct-chat session.
    pub group: Option<String>,
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
pub trait Transport: Send + Sync {
    /// Names the channel in logs and errors, such as `ClawBot`.
    fn label(&self) -> &'static str;

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
}

/// Run one account until the returned future is dropped or fails. It holds
/// the account's lifetime lock, binds delivery state to the credentials the
/// transport uses (`running` says whether they are the saved ones), recovers
/// interrupted work, and delivers recovered replies before receiving.
///
/// `tool_owner` is the authenticated owner when the account grants its owner
/// remote tools; every other sender stays tool-free.
#[allow(clippy::too_many_arguments)]
pub async fn run<C: state::Credentials, T: Transport>(
    transport: &T,
    account: &str,
    workspace: &Path,
    socket: &Path,
    tool_owner: Option<&ToolOwner>,
    store: &state::Store<C>,
    running: impl FnOnce(&C) -> Result<bool>,
    report: &(dyn Fn(bool) + Send + Sync),
) -> Result<()> {
    let _lock = store.lock(account)?;
    let mut state = store.bind_state(account, running)?;
    recover_interrupted(store, account, &mut state)?;
    let bridge = Bridge {
        transport,
        account,
        workspace,
        socket,
        store,
        report,
        state: Mutex::new(state),
        turns: Semaphore::new(MAX_CONCURRENT_TURNS),
        replies: Notify::new(),
    };
    // Replies recovered from an earlier run go out before the first poll.
    bridge.deliver_backlog().await?;
    // Conversations run as futures owned by this one, never as spawned tasks,
    // so cancellation drops every session and request with it.
    let (start, mut started) = mpsc::unbounded_channel();
    let mut conversations = FuturesUnordered::new();
    let poll = bridge.poll(tool_owner, &start);
    let deliver = bridge.deliver();
    tokio::pin!(poll, deliver);
    loop {
        tokio::select! {
            result = &mut poll => return result,
            result = &mut deliver => return result,
            Some((owner, recipient, jobs)) = started.recv() => conversations.push(bridge.converse(owner, recipient, jobs)),
            Some(result) = conversations.next(), if !conversations.is_empty() => result?,
        }
    }
}

/// One accepted message, claimed durably before it is queued.
struct Job {
    message_id: String,
    text: String,
    limit: Duration,
}

/// A live conversation's job queue as seen by the poller.
struct Conversation {
    jobs: mpsc::UnboundedSender<Job>,
    last_used: Instant,
}

enum Step {
    Idle,
    Progress,
    Retry,
}

type Starter = mpsc::UnboundedSender<(bool, Option<String>, mpsc::UnboundedReceiver<Job>)>;

struct Bridge<'a, C, T> {
    transport: &'a T,
    account: &'a str,
    workspace: &'a Path,
    socket: &'a Path,
    store: &'a state::Store<C>,
    report: &'a (dyn Fn(bool) + Send + Sync),
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
        loop {
            match self.store.save_state(self.account, state) {
                Err(error) if state::is_busy(&error) && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                result => return result,
            }
        }
    }

    /// Receive messages, durably claim accepted ones, and queue each on its
    /// conversation. Receiving continues while turns run.
    async fn poll(&self, tool_owner: Option<&ToolOwner>, start: &Starter) -> Result<()> {
        let mut conversations: HashMap<String, Conversation> = HashMap::new();
        let mut backoff = Duration::from_secs(1);
        loop {
            let cursor = self.state.lock().await.cursor.clone();
            let batch = match self.transport.receive(&cursor).await {
                Ok(batch) => {
                    (self.report)(true);
                    batch
                }
                Err(error) => {
                    tracing::warn!("{} poll failed: {error}", self.transport.label());
                    (self.report)(false);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            backoff = Duration::from_secs(1);
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
        if state.seen.iter().any(|seen| seen == id) {
            // Keep all IDs encountered in this bounded batch until its cursor
            // commits, including IDs recovered from the preceding run.
            mark_seen(&mut state, id);
            return self.save(&state).await;
        }
        // Claimed or answered but not yet delivered: never a second turn.
        if state.in_flight.iter().any(|claim| claim.message_id == id)
            || state.pending.iter().any(|pending| pending.message_id == id)
        {
            return Ok(());
        }
        let Inbound::Text(message) = inbound else {
            mark_seen(&mut state, id);
            return self.save(&state).await;
        };
        let (sender, ctx, group) = (
            message.sender.as_str(),
            message.reply_to.as_str(),
            &message.group,
        );
        let key = group
            .as_ref()
            .map_or_else(|| sender.to_owned(), |group| format!("{group}\0{sender}"));
        let owner =
            group.is_none() && tool_owner.is_some_and(|tool_owner| tool_owner.user_id == sender);
        let limit = match tool_owner {
            Some(tool_owner) if owner => tool_owner.turn_timeout,
            _ => TURN_TIMEOUT,
        };
        let waiting = |key: &str| {
            state
                .in_flight
                .iter()
                .filter(|claim| conversation_key(&claim.key, &claim.to_user_id) == key)
                .count()
        };
        // A full session table makes room by closing the least recently used
        // conversation that has nothing waiting.
        let evict = if conversations.contains_key(&key) || conversations.len() < MAX_SESSIONS {
            None
        } else {
            conversations
                .iter()
                .filter(|(key, _)| waiting(key) == 0)
                .min_by_key(|(_, conversation)| conversation.last_used)
                .map(|(key, _)| key.clone())
        };
        let room = state.in_flight.len() < MAX_CLAIMS
            && waiting(&key) < MAX_QUEUED_PER_CONVERSATION
            && (conversations.contains_key(&key)
                || conversations.len() < MAX_SESSIONS
                || evict.is_some());
        if !room {
            tracing::warn!(
                "{} is at its work limit; asking a sender to retry later",
                self.transport.label()
            );
            let mut busy = new_pending(id, sender, ctx, BUSY_REPLY, MAX_REPLY_BYTES);
            busy.key = key;
            busy.transient = true;
            state.pending.push(busy);
            self.save(&state).await?;
            drop(state);
            self.replies.notify_one();
            return Ok(());
        }
        state.in_flight.push(state::InFlight {
            message_id: id.to_owned(),
            to_user_id: sender.into(),
            context_token: ctx.into(),
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
                let recipient = group.is_none().then(|| sender.to_owned());
                start.send((owner, recipient, queue)).map_err(|_| {
                    anyhow!("{} conversation runner stopped", self.transport.label())
                })?;
                conversations.insert(
                    key.clone(),
                    Conversation {
                        jobs,
                        last_used: Instant::now(),
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
    ) -> Result<()> {
        enum Next {
            Job(Option<Job>),
            Idle,
            Report(Result<String>),
        }
        let mut session: Option<session::Session> = None;
        loop {
            let watching = recipient.is_some()
                && session
                    .as_ref()
                    .is_some_and(|session| session.background_jobs() > 0 || session.has_reports());
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
                Next::Idle | Next::Job(None) => return Ok(()),
                Next::Report(Ok(report)) => {
                    if let Some(recipient) = &recipient {
                        self.queue_unprompted(recipient, &report).await?;
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
                    if session.is_none() {
                        if owner {
                            tracing::info!(
                                "{} owner session starts with remote tools",
                                self.transport.label()
                            );
                        }
                        session = Some(
                            session::Session::connect(self.socket, self.workspace, owner).await?,
                        );
                    }
                    session
                        .as_mut()
                        .expect("session was just connected")
                        .turn(&job.text, MAX_TOTAL_REPLY_BYTES)
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
                        FAILURE_REPLY.into()
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
                        FAILURE_REPLY.into()
                    }
                }
            };
            let reply = if reply.trim().is_empty() {
                "SCV completed without a text response.".into()
            } else {
                reply
            };
            // The claim becomes a durable pending reply in one state write.
            let mut state = self.state.lock().await;
            let index = state
                .in_flight
                .iter()
                .position(|claim| claim.message_id == job.message_id)
                .ok_or_else(|| anyhow!("{} lost a claimed message", self.transport.label()))?;
            let claim = state.in_flight.remove(index);
            let pending = compose_pending(&mut state, &claim, &reply, unix_now());
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
                    self.queue_unprompted(recipient, &report).await?;
                }
            }
        }
    }

    /// Queue a message to `recipient` that answers no inbound message, such
    /// as a finished background job's report. It is sent without a reply
    /// handle, durably and with stable client IDs like any reply.
    async fn queue_unprompted(&self, recipient: &str, text: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let mut pending = new_pending("", recipient, "", text, MAX_REPLY_BYTES);
        pending.key = recipient.to_owned();
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
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.deliver_next().await? {
                Step::Idle => self.replies.notified().await,
                Step::Progress => backoff = Duration::from_secs(1),
                Step::Retry => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    /// Send the oldest pending reply. Only this path edits or removes pending
    /// replies, so the first entry stays the same one between state locks.
    async fn deliver_next(&self) -> Result<Step> {
        let mut pending = {
            let mut state = self.state.lock().await;
            let Some(pending) = state.pending.first_mut() else {
                return Ok(Step::Idle);
            };
            let chunks = split_utf8(&pending.reply, MAX_REPLY_BYTES).len();
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
        let chunks = split_utf8(&pending.reply, MAX_REPLY_BYTES);
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
        let mut state = self.state.lock().await;
        state.pending.remove(0);
        if !pending.message_id.is_empty() {
            mark_seen(&mut state, &pending.message_id);
        }
        if refused {
            hold_refused(&mut state, &pending, &chunks, unix_now());
        }
        self.save(&state).await?;
        Ok(Step::Progress)
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
    let mut end = MAX_HELD_REPLY_BYTES - MARKER.len();
    while !reply.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &reply[..end])
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
    if state.in_flight.is_empty() {
        return Ok(());
    }
    let now = unix_now();
    for claim in std::mem::take(&mut state.in_flight) {
        let pending = compose_pending(state, &claim, FAILURE_REPLY, now);
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
