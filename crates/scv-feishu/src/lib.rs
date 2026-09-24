//! The Feishu (and Lark) channel: a transport for the shared channel bridge
//! over Feishu's event long connection and Open Platform API.
//!
//! Feishu pushes events over a WebSocket and does not redeliver messages
//! sent while SCV was disconnected. So each connection starts by listing the
//! history of every chat the checkpoint knows since its newest message, and
//! socket events are acknowledged only once the bridge has made their claims
//! durable, which is when it asks for the next batch.

use anyhow::Result;
use async_trait::async_trait;
use scv_channels::{Batch, Inbound, Outbound, SendOutcome, Transport};
pub use scv_channels::{ToolOwner, owner_turn_timeout};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;

pub mod api;
pub mod frame;
pub mod inbound;
pub mod login;
pub mod socket;
pub mod state;

use api::{Api, Attempt, Endpoints, Refusal};
use inbound::{Checkpoint, Event, Received};

/// The channel name this crate serves: `scv channels <command> feishu`.
pub const CHANNEL: &str = "feishu";

/// How long one receive waits on the socket before reporting in.
const RECEIVE_WINDOW: Duration = Duration::from_secs(25);
/// Once a message arrives, how long to gather others that follow it.
const GATHER: Duration = Duration::from_millis(50);
const MAX_BATCH: usize = 256;
/// Catch-up reaches back at most this far, whatever the checkpoint says.
const CATCH_UP_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// Chats listed, and pages of 50 messages per chat, in one catch-up.
const CATCH_UP_CHATS: usize = 32;
const CATCH_UP_PAGES: usize = 4;

/// Run one account until cancelled. Only a live connection reports healthy.
/// Cancellation drops all owned I/O and sessions; no tasks are spawned.
///
/// `tool_owner` is the authenticated owner when the account grants its owner
/// remote tools; every other sender stays tool-free. `link` connects the
/// account to the daemon's hub.
#[allow(clippy::too_many_arguments)]
pub async fn run_supervised(
    credentials: &state::Account,
    account: &str,
    workspace: &Path,
    socket: &Path,
    tool_owner: Option<&ToolOwner>,
    cancellation: CancellationToken,
    report: Arc<dyn Fn(bool) + Send + Sync>,
    link: scv_channels::hub::Link,
) -> Result<()> {
    let work = async {
        state::validate_name(account)?;
        credentials.validate()?;
        let store = state::store()?;
        let transport = Feishu::new(Endpoints::for_brand(credentials.brand), credentials)?;
        let result = scv_channels::run_linked(
            &transport,
            account,
            workspace,
            socket,
            tool_owner,
            &store,
            |saved| Ok(saved == credentials),
            report.as_ref(),
            &link,
        )
        .await;
        if result.is_err() {
            report(false);
        }
        result
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = work => result,
    }
}

/// The Feishu transport of one account.
pub struct Feishu {
    api: Api,
    link: Mutex<Connection>,
    /// How long one receive waits on the socket.
    window: Duration,
    /// Feishu or Lark, as the users of this account know it.
    brand: state::Brand,
}

#[derive(Default)]
struct Connection {
    socket: Option<socket::Link>,
    /// Whether this socket's catch-up is done.
    caught_up: bool,
    /// Socket events of the last batch, acknowledged on the next receive.
    unacknowledged: Vec<socket::Delivery>,
    /// The bot's own `open_id`, to tell whether a group message mentions it.
    bot: Option<String>,
}

impl Feishu {
    pub fn new(endpoints: Endpoints, credentials: &state::Account) -> Result<Self> {
        Ok(Self {
            api: Api::new(endpoints, &credentials.app_id, &credentials.app_secret)?,
            link: Mutex::new(Connection::default()),
            window: RECEIVE_WINDOW,
            brand: credentials.brand,
        })
    }

    /// Messages each known chat received since its checkpoint, oldest first
    /// per chat. A chat whose history Feishu refuses is skipped; a transport
    /// failure fails the catch-up so it runs again.
    async fn catch_up(&self, checkpoint: &Checkpoint, bot: Option<&str>) -> Result<Vec<Received>> {
        let now = unix_seconds();
        let floor = now.saturating_sub(CATCH_UP_WINDOW.as_secs());
        let mut chats: Vec<_> = checkpoint.chats.iter().collect();
        chats.sort_by_key(|(_, mark)| std::cmp::Reverse(mark.last_ms));
        let mut found = Vec::new();
        for (chat, mark) in chats.into_iter().take(CATCH_UP_CHATS) {
            let start = (mark.last_ms / 1000).max(floor);
            let mut page = None;
            for _ in 0..CATCH_UP_PAGES {
                let listed = match self
                    .api
                    .history(chat, start, now + 1, page.as_deref())
                    .await
                {
                    Ok(listed) => listed,
                    Err(error) => match error.downcast_ref::<Refusal>() {
                        Some(refusal) => {
                            tracing::warn!("Feishu catch-up skipped a chat: {refusal}");
                            break;
                        }
                        None => return Err(error),
                    },
                };
                found.extend(
                    listed
                        .items
                        .iter()
                        .filter_map(|item| inbound::parse_history(item, mark.group, bot))
                        .filter(|received| received.created_ms >= mark.last_ms),
                );
                match listed.next {
                    Some(next) => page = Some(next),
                    None => break,
                }
            }
        }
        Ok(found)
    }
}

#[async_trait]
impl Transport for Feishu {
    fn label(&self) -> &'static str {
        "Feishu"
    }

    fn channel(&self) -> &'static str {
        self.brand.title()
    }

    /// Acknowledge the previous batch, (re)connect and catch up when needed,
    /// then wait for socket events.
    async fn receive(&self, checkpoint: &str) -> Result<Batch> {
        let mut checkpoint = Checkpoint::parse(checkpoint);
        let before = checkpoint.clone();
        let mut guard = self.link.lock().await;
        let connection = &mut *guard;
        // The bridge asks again only after the last batch's claims and
        // checkpoint are durable, so its events can be acknowledged now.
        let acknowledged = std::mem::take(&mut connection.unacknowledged);
        if let Some(socket) = connection.socket.as_mut() {
            for delivery in acknowledged {
                if let Err(error) = socket.acknowledge(delivery).await {
                    tracing::warn!("Feishu acknowledgement failed: {error:#}");
                    connection.socket = None;
                    break;
                }
            }
        }
        if connection.socket.is_none() {
            if connection.bot.is_none() {
                match self.api.bot_open_id().await {
                    Ok(bot) => connection.bot = Some(bot),
                    Err(error) => {
                        tracing::warn!(
                            "Feishu bot info unavailable; group messages go unanswered: {error:#}"
                        )
                    }
                }
            }
            connection.socket = Some(socket::Link::connect(&self.api).await?);
            connection.caught_up = false;
        }
        let bot = connection.bot.clone();
        let mut messages = Vec::new();
        if !connection.caught_up {
            for received in self.catch_up(&checkpoint, bot.as_deref()).await? {
                checkpoint.observe(&received);
                messages.push(received.inbound);
            }
            connection.caught_up = true;
            // Report the new connection healthy at once.
            return Ok(batch(messages, &before, &checkpoint));
        }
        let socket = connection
            .socket
            .as_mut()
            .expect("the socket was just connected");
        let window_end = Instant::now() + self.window;
        while messages.len() < MAX_BATCH {
            let until = if messages.is_empty() {
                window_end
            } else {
                (Instant::now() + GATHER).min(window_end)
            };
            let delivery = match socket.next(until).await {
                Ok(Some(delivery)) => delivery,
                Ok(None) => break,
                Err(error) => {
                    connection.socket = None;
                    if messages.is_empty() {
                        return Err(error);
                    }
                    // Hand over what arrived; Feishu may send it again,
                    // and deduplication drops the repeat.
                    tracing::warn!("{error:#}");
                    break;
                }
            };
            match inbound::parse_event(&delivery.payload, bot.as_deref()) {
                Some(Event::Message(received)) => {
                    checkpoint.observe(&received);
                    messages.push(received.inbound);
                    connection.unacknowledged.push(delivery);
                }
                parsed => {
                    if parsed.is_none() {
                        tracing::warn!("Feishu sent a malformed message event; dropping it");
                    }
                    // Nothing to claim: acknowledge at once.
                    if let Err(error) = socket.acknowledge(delivery).await {
                        connection.socket = None;
                        if messages.is_empty() {
                            return Err(error);
                        }
                        break;
                    }
                }
            }
        }
        Ok(batch(messages, &before, &checkpoint))
    }

    /// Replies answer their message; a message that answers nothing, such as
    /// a background report, goes to the user directly. Transient failures
    /// retry with the same `uuid`, so Feishu delivers each part once.
    async fn send(
        &self,
        message: &Outbound<'_>,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome> {
        let mut delay = Duration::from_secs(1);
        for attempt in 0..3 {
            match self
                .api
                .send_text(
                    message.to,
                    message.reply_to,
                    message.text,
                    message.client_id,
                )
                .await
            {
                Attempt::Delivered => return Ok(SendOutcome::Delivered),
                Attempt::Refused(reason) => {
                    tracing::warn!("Feishu refused a reply ({reason}); not retrying");
                    return Ok(SendOutcome::Rejected);
                }
                Attempt::Retry(reason) => {
                    tracing::warn!(attempt, "Feishu reply send failed: {reason}")
                }
            }
            report(false);
            if attempt == 2 {
                anyhow::bail!("Feishu could not deliver the reply");
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        unreachable!("delivery loop returns after success or final attempt")
    }
}

fn batch(messages: Vec<Inbound>, before: &Checkpoint, after: &Checkpoint) -> Batch {
    Batch {
        messages,
        checkpoint: (after != before).then(|| after.to_json()),
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
