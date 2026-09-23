//! Safe, testable primitives for the WeChat iLink ClawBot adapter.

use anyhow::{Result, anyhow, bail};

pub mod bridge;
pub mod protocol;
pub mod state;

use futures_util::stream::{FuturesUnordered, StreamExt as _};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub async fn login(base: &str, account: &str) -> Result<()> {
    state::validate_name(account)?;
    let client = http_client()?;
    let base = normalize_base_url(base)?;
    let qr = response_json(
        client
            .get(format!("{base}/ilink/bot/get_bot_qrcode?bot_type=3"))
            .timeout(Duration::from_secs(20))
            .send()
            .await?,
    )
    .await?;
    check_envelope(&qr)?;
    let code = qr
        .get("qrcode")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("login response omitted qrcode"))?;
    println!(
        "Scan this ClawBot QR code in WeChat:\n{}",
        qr.get("qrcode_img_content")
            .and_then(Value::as_str)
            .unwrap_or(code)
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() >= deadline {
            bail!("ClawBot QR login timed out; run `scv clawbot login` again")
        }
        let status = response_json(
            client
                .get(format!("{base}/ilink/bot/get_qrcode_status"))
                .query(&[("qrcode", code)])
                .timeout(Duration::from_secs(50))
                .send()
                .await?,
        )
        .await?;
        check_envelope(&status)?;
        match status
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
        {
            "confirmed" => {
                let (token, bot_id, user_id) = bridge::validate_confirmed_login(&status)?;
                let host = normalize_base_url(
                    status
                        .get("baseurl")
                        .and_then(Value::as_str)
                        .unwrap_or(&base),
                )?;
                bridge::validate_origin_pair(&base, &host)?;
                state::save_account(
                    account,
                    &state::Account {
                        token: token.into(),
                        base_url: host.clone(),
                        bot_id: Some(bot_id.into()),
                        user_id: Some(user_id.into()),
                    },
                )?;
                println!("ClawBot login confirmed for {bot_id} at {host}.");
                return Ok(());
            }
            "expired" => bail!("ClawBot QR code expired; run `scv clawbot login` again"),
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

const MAX_REPLY_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGE_ID_BYTES: usize = 256;
const MAX_BATCH_MESSAGES: usize = 4096;
const FAILURE_REPLY: &str = "SCV could not complete that request.";
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
/// Owner turns may run tools and delegated agents, which take longer.
const OWNER_TURN_TIMEOUT: Duration = Duration::from_secs(1800);
/// Model time an owner turn keeps beyond its longest single tool call.
const OWNER_TURN_MARGIN: Duration = Duration::from_secs(300);
/// Senders whose turns may run at once.
const MAX_CONCURRENT_TURNS: usize = 4;
/// Claimed messages one conversation may have waiting or running.
const MAX_QUEUED_PER_CONVERSATION: usize = 8;
/// Claimed messages across all conversations.
const MAX_CLAIMS: usize = 64;
/// Live sender sessions.
const MAX_SESSIONS: usize = 32;
/// A conversation idle this long after its last turn closes its session.
const SESSION_IDLE: Duration = Duration::from_secs(1800);
/// How long a state write waits out a daemon command's transaction.
const STATE_BUSY_RETRY: Duration = Duration::from_secs(5);
const BUSY_REPLY: &str =
    "SCV is still working on your earlier messages. Please send this one again later.";
const HELD_HEADER: &str = "[Earlier reply that could not be delivered at the time]\n";
const LATEST_HEADER: &str = "[Reply to your latest message]\n";
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
    /// The owner's authenticated iLink user ID.
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

/// Compatibility entry point. Connects to the existing daemon; launches no process.
pub async fn run(token: &str, base_url: &str, account: &str, workspace: &Path) -> Result<()> {
    run_supervised(
        token,
        base_url,
        account,
        workspace,
        &scv_client::default_socket_path()?,
        None,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await
}

/// Run one account until cancelled. Only a validated authenticated getupdates
/// response reports healthy. Cancellation drops all owned I/O and sessions;
/// no adapter tasks are spawned. The caller supplies any external stop timeout.
///
/// `tool_owner` is the authenticated owner when the account grants its owner
/// remote tools; every other sender stays tool-free.
#[allow(clippy::too_many_arguments)]
pub async fn run_supervised(
    token: &str,
    base_url: &str,
    account: &str,
    workspace: &Path,
    socket: &Path,
    tool_owner: Option<&ToolOwner>,
    cancellation: CancellationToken,
    report: Arc<dyn Fn(bool) + Send + Sync>,
) -> Result<()> {
    until_cancelled(cancellation, async {
        state::validate_name(account)?;
        let base_url = normalize_base_url(base_url)?;
        let store = state::Store::new(state::root()?);
        let result = run_loop(
            token,
            &base_url,
            account,
            workspace,
            socket,
            tool_owner,
            &store,
            report.as_ref(),
        )
        .await;
        if result.is_err() {
            report(false);
        }
        result
    })
    .await
}

async fn until_cancelled(
    cancellation: CancellationToken,
    work: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = work => result,
    }
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn response_json(response: reqwest::Response) -> Result<Value> {
    let body = response_body(response).await?;
    serde_json::from_slice(&body).map_err(|_| anyhow!("invalid ClawBot response"))
}

async fn response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        bail!(
            "ClawBot HTTP request failed with status {}",
            response.status()
        )
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("ClawBot response exceeds limit")
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("ClawBot response exceeds limit")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    token: &str,
    base_url: &str,
    account: &str,
    workspace: &Path,
    socket: &Path,
    tool_owner: Option<&ToolOwner>,
    store: &state::Store,
    report: &(dyn Fn(bool) + Send + Sync),
) -> Result<()> {
    let _lock = store.lock(account)?;
    let client = http_client()?;
    let mut state = store.bind_state(account, token, base_url)?;
    recover_interrupted(store, account, &mut state)?;
    let bridge = Bridge {
        client,
        token,
        base_url,
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
            Some((owner, jobs)) = started.recv() => conversations.push(bridge.converse(owner, jobs)),
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

struct Bridge<'a> {
    client: reqwest::Client,
    token: &'a str,
    base_url: &'a str,
    account: &'a str,
    workspace: &'a Path,
    socket: &'a Path,
    store: &'a state::Store,
    report: &'a (dyn Fn(bool) + Send + Sync),
    /// The only copy of delivery state. Every change is saved while held.
    state: Mutex<state::BridgeState>,
    /// Bounds how many senders' turns run at once.
    turns: Semaphore,
    /// Wakes the delivery loop when a reply is queued.
    replies: Notify,
}

impl Bridge<'_> {
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

    /// Poll iLink, durably claim accepted messages, and queue each on its
    /// conversation. Polling continues while turns run.
    async fn poll(
        &self,
        tool_owner: Option<&ToolOwner>,
        start: &mpsc::UnboundedSender<(bool, mpsc::UnboundedReceiver<Job>)>,
    ) -> Result<()> {
        let mut conversations: HashMap<String, Conversation> = HashMap::new();
        let mut backoff = Duration::from_secs(1);
        loop {
            let cursor = self.state.lock().await.cursor.clone();
            let response = async {
                let response = self.client.post(format!("{}/ilink/bot/getupdates", self.base_url))
                    .headers(bridge::auth_headers(self.token, u32::from_le_bytes(*Uuid::new_v4().as_bytes().first_chunk::<4>().unwrap())))
                    .json(&serde_json::json!({"get_updates_buf":cursor,"base_info":{"channel_version":"1.0.0"}}))
                    .timeout(Duration::from_secs(50)).send().await?;
                let value = response_json(response).await?;
                validate_updates(&value)?;
                Ok::<_, anyhow::Error>(value)
            }.await;
            let response = match response {
                Ok(response) => {
                    (self.report)(true);
                    response
                }
                Err(error) => {
                    tracing::warn!("ClawBot poll failed: {error}");
                    (self.report)(false);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            backoff = Duration::from_secs(1);
            // Conversations close their queues after idling.
            conversations.retain(|_, conversation| !conversation.jobs.is_closed());
            for msg in response
                .get("msgs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.accept(msg, tool_owner, &mut conversations, start)
                    .await?;
            }
            let mut state = self.state.lock().await;
            if let Some(next) = response.get("get_updates_buf").and_then(Value::as_str) {
                state.cursor = next.into();
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
        msg: &Value,
        tool_owner: Option<&ToolOwner>,
        conversations: &mut HashMap<String, Conversation>,
        start: &mpsc::UnboundedSender<(bool, mpsc::UnboundedReceiver<Job>)>,
    ) -> Result<()> {
        let Some(id) = message_id(msg) else {
            return Ok(());
        };
        let mut state = self.state.lock().await;
        if state.seen.iter().any(|seen| seen == &id) {
            // Keep all IDs encountered in this bounded batch until its cursor
            // commits, including IDs recovered from the preceding run.
            mark_seen(&mut state, &id);
            return self.save(&state).await;
        }
        // Claimed or answered but not yet delivered: never a second turn.
        if state.in_flight.iter().any(|claim| claim.message_id == id)
            || state.pending.iter().any(|pending| pending.message_id == id)
        {
            return Ok(());
        }
        let text = msg
            .get("item_list")
            .and_then(Value::as_array)
            .and_then(|xs| {
                xs.iter()
                    .find_map(|x| x.get("text_item")?.get("text")?.as_str())
            })
            .filter(|text| !text.trim().is_empty());
        let sender = msg
            .get("from_user_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let ctx = msg
            .get("context_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let (Some(text), Some(sender), Some(ctx), Some(1)) = (
            text,
            sender,
            ctx,
            msg.get("message_type").and_then(Value::as_i64),
        ) else {
            mark_seen(&mut state, &id);
            return self.save(&state).await;
        };
        // Group messages never carry owner authority and never share the
        // sender's direct-chat session, whose history may hold tool output.
        let group = match msg.get("group_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(group)) if group.is_empty() => None,
            Some(Value::String(group)) => Some(group.clone()),
            Some(other) => Some(other.to_string()),
        };
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
            tracing::warn!("ClawBot is at its work limit; asking a sender to retry later");
            let mut busy = new_pending(&id, sender, ctx, BUSY_REPLY, MAX_REPLY_BYTES);
            busy.key = key;
            busy.transient = true;
            state.pending.push(busy);
            self.save(&state).await?;
            drop(state);
            self.replies.notify_one();
            return Ok(());
        }
        state.in_flight.push(state::InFlight {
            message_id: id.clone(),
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
            message_id: id,
            text: text.to_owned(),
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
                start
                    .send((owner, queue))
                    .map_err(|_| anyhow!("ClawBot conversation runner stopped"))?;
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

    /// Run one conversation's turns in order on its own SCV session. A failed
    /// or timed-out turn resets only this session.
    async fn converse(&self, owner: bool, mut jobs: mpsc::UnboundedReceiver<Job>) -> Result<()> {
        let mut session: Option<protocol::Session> = None;
        loop {
            let Ok(Some(job)) = tokio::time::timeout(SESSION_IDLE, jobs.recv()).await else {
                return Ok(());
            };
            let reply = {
                let _turn = self
                    .turns
                    .acquire()
                    .await
                    .map_err(|_| anyhow!("ClawBot turn limiter closed"))?;
                let result = tokio::time::timeout(job.limit, async {
                    if session.is_none() {
                        if owner {
                            tracing::info!("ClawBot owner session starts with remote tools");
                        }
                        session = Some(
                            protocol::Session::connect(self.socket, self.workspace, owner).await?,
                        );
                    }
                    session
                        .as_mut()
                        .expect("session was just connected")
                        .turn(&job.text, MAX_REPLY_BYTES)
                        .await
                })
                .await;
                match result {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(_)) | Err(_) => {
                        session = None;
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
                .ok_or_else(|| anyhow!("ClawBot lost a claimed message"))?;
            let claim = state.in_flight.remove(index);
            let pending = compose_pending(&mut state, &claim, &reply, unix_now());
            state.pending.push(pending);
            self.save(&state).await?;
            drop(state);
            self.replies.notify_one();
        }
    }

    /// Deliver every queued reply before polling starts. A reply that still
    /// cannot be sent fails the run so the supervisor retries with backoff.
    async fn deliver_backlog(&self) -> Result<()> {
        loop {
            match self.deliver_next().await? {
                Step::Idle => return Ok(()),
                Step::Progress => {}
                Step::Retry => bail!("ClawBot could not deliver the reply"),
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
            let body = bridge::reply_body(
                &pending.to_user_id,
                &pending.context_token,
                &chunks[index],
                &pending.client_ids[index],
            );
            match bridge::send_reply_request(
                &self.client,
                self.token,
                self.base_url,
                &body,
                self.report,
            )
            .await
            {
                Ok(bridge::SendOutcome::Delivered) => {}
                Ok(bridge::SendOutcome::Rejected) => {
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
/// conversation ride ahead of it, oldest first, as far as one message allows:
/// iLink accepts one reply per inbound context token.
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
            "ClawBot discarded undelivered replies past their limits"
        );
    }
}

fn message_id(msg: &Value) -> Option<String> {
    let value = msg.get("message_id").or_else(|| msg.get("msg_id"))?;
    match value {
        Value::String(id) if !id.is_empty() && id.len() <= MAX_MESSAGE_ID_BYTES => Some(id.clone()),
        Value::Number(id) if id.as_u64().is_some() => Some(id.to_string()),
        _ => None,
    }
}

fn validate_updates(value: &Value) -> Result<()> {
    // Current iLink getupdates responses omit `ret` on success, while error
    // responses and older servers use the common envelope. Accept both forms.
    if value.get("ret").is_some() || value.get("errcode").is_some() {
        check_envelope(value)?;
    } else if !value.get("msgs").is_some_and(Value::is_array)
        || !value.get("get_updates_buf").is_some_and(Value::is_string)
    {
        bail!("iLink updates response omitted success fields")
    }
    if value
        .get("msgs")
        .and_then(Value::as_array)
        .is_some_and(|msgs| msgs.len() > MAX_BATCH_MESSAGES)
    {
        bail!("ClawBot updates batch exceeds limit")
    }
    if value.get("msgs").is_some_and(|msgs| !msgs.is_array())
        || value
            .get("get_updates_buf")
            .is_some_and(|cursor| !cursor.is_string())
    {
        bail!("invalid ClawBot updates response")
    }
    Ok(())
}

fn recover_interrupted(
    store: &state::Store,
    account: &str,
    state: &mut state::BridgeState,
) -> Result<()> {
    if state.in_flight.is_empty() {
        return Ok(());
    }
    // Interrupted work is never resubmitted; each claim gets a failure reply.
    let now = unix_now();
    for claim in std::mem::take(&mut state.in_flight) {
        let pending = compose_pending(state, &claim, FAILURE_REPLY, now);
        state.pending.push(pending);
    }
    store.save_state(account, state)
}

fn new_pending(
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

pub fn normalize_base_url(value: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(value.trim()).map_err(|e| anyhow!("invalid ClawBot base URL: {e}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.path() != "/" && !url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("ClawBot base URL must be an HTTPS origin")
    }
    Ok(value.trim().trim_end_matches('/').to_owned())
}

fn mark_seen(state: &mut state::BridgeState, id: &str) {
    if let Some(index) = state.seen.iter().position(|seen| seen == id) {
        state.seen.remove(index);
    }
    state.seen.push(id.to_owned());
    let excess = state.seen.len().saturating_sub(4096);
    state.seen.drain(..excess);
}

pub fn check_envelope(value: &serde_json::Value) -> Result<()> {
    let ret = value
        .get("ret")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("iLink response omitted ret"))?;
    if ret != 0 || value.get("errcode").is_some_and(|v| v.as_i64() != Some(0)) {
        bail!("iLink API rejected request")
    }
    Ok(())
}

/// Classify a 2xx iLink sendmessage body. Live acknowledgements omit `ret`
/// and need not be JSON, so only an explicit non-zero `ret` or `errcode`
/// rejects; the error is a bounded diagnostic without message content or
/// non-integer code values.
pub fn check_send_ack(body: &[u8]) -> std::result::Result<(), String> {
    let Ok(Value::Object(value)) = serde_json::from_slice::<Value>(body) else {
        return Ok(());
    };
    let code = |key: &str| {
        value
            .get(key)
            .filter(|v| !v.is_null() && v.as_i64() != Some(0))
            .map(|v| {
                v.as_i64()
                    .map_or_else(|| "non-integer".into(), |n| n.to_string())
            })
    };
    let (ret, errcode) = (code("ret"), code("errcode"));
    if ret.is_none() && errcode.is_none() {
        return Ok(());
    }
    let errmsg: String = value
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    Err(format!(
        "ret={} errcode={} errmsg={errmsg:?}",
        ret.as_deref().unwrap_or("-"),
        errcode.as_deref().unwrap_or("-")
    ))
}

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
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_origins() {
        assert!(normalize_base_url("https://example.test").is_ok());
        assert!(normalize_base_url("http://example.test").is_err());
        assert!(normalize_base_url("https://user@example.test").is_err());
    }
    #[test]
    fn chunks_on_utf8_boundaries() {
        let chunks = split_utf8("a🙂b", 4);
        assert_eq!(chunks, vec!["a", "🙂", "b"]);
    }
    #[test]
    fn chunks_make_progress_below_codepoint_size() {
        assert_eq!(split_utf8("🙂", 1), vec!["🙂"]);
        assert_eq!(split_utf8("🙂", 0), vec!["🙂"]);
    }
    #[test]
    fn validates_ret() {
        assert!(check_envelope(&serde_json::json!({"ret":0})).is_ok());
        assert!(check_envelope(&serde_json::json!({"ret":1})).is_err());
    }

    #[test]
    fn accepts_live_send_ack_without_ret() {
        for delivered in [
            &b""[..],
            b"{}",
            br#"{"ret":0}"#,
            br#"{"ret":null}"#,
            b"ok",
            b"[]",
        ] {
            assert!(check_send_ack(delivered).is_ok());
        }
        assert_eq!(
            check_send_ack(br#"{"ret":-2,"errmsg":"prepare failed"}"#).unwrap_err(),
            r#"ret=-2 errcode=- errmsg="prepare failed""#
        );
        assert!(check_send_ack(br#"{"errcode":40001}"#).is_err());
        assert!(check_send_ack(br#"{"ret":"0"}"#).is_err());
        assert_eq!(
            check_send_ack(br#"{"ret":{"detail":"x"},"errcode":7}"#).unwrap_err(),
            r#"ret=non-integer errcode=7 errmsg="""#
        );
    }

    #[test]
    fn accepts_live_getupdates_success_without_ret() {
        assert!(
            validate_updates(&serde_json::json!({
                "msgs": [],
                "sync_buf": "sync",
                "get_updates_buf": "cursor"
            }))
            .is_ok()
        );
    }

    #[test]
    fn rejects_getupdates_error_without_ret() {
        assert!(
            validate_updates(&serde_json::json!({
                "errcode": -14,
                "errmsg": "session timeout"
            }))
            .is_err()
        );
    }

    #[test]
    fn preserves_string_and_unsigned_numeric_message_ids() {
        assert_eq!(
            message_id(&serde_json::json!({"message_id": "string-id"})).as_deref(),
            Some("string-id")
        );
        assert_eq!(
            message_id(&serde_json::json!({"message_id": u64::MAX})).as_deref(),
            Some("18446744073709551615")
        );
        assert_eq!(
            message_id(&serde_json::json!({"msg_id": 42})).as_deref(),
            Some("42")
        );
        assert!(message_id(&serde_json::json!({"message_id": null, "msg_id": 42})).is_none());
        assert!(message_id(&serde_json::json!({"message_id": -1})).is_none());
        assert!(message_id(&serde_json::json!({"message_id": 1.5})).is_none());
        assert!(message_id(&serde_json::from_str(r#"{"message_id":1e3}"#).unwrap()).is_none());
        assert!(
            message_id(&serde_json::from_str(r#"{"message_id":18446744073709551616}"#).unwrap())
                .is_none()
        );
        assert!(
            message_id(&serde_json::json!({
                "message_id": "x".repeat(MAX_MESSAGE_ID_BYTES + 1)
            }))
            .is_none()
        );
    }
}
