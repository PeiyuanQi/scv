//! The Slack channel: a transport for the shared channel bridge over Slack's
//! Events API in Socket Mode and its Web API, with tokens the operator
//! enters by hand.
//!
//! Slack retries an event that was not acknowledged only a few times, and
//! keeps nothing for an app that stays disconnected. So each connection
//! starts by listing the history of every conversation and thread the
//! checkpoint knows since its newest message, and an event is acknowledged
//! only once the bridge has made its claim durable, which is when it asks
//! for the next batch. A thread is a conversation of its own, answered
//! inside the thread.

#![forbid(unsafe_code)]

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use scv_client::{Layout, Secret};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

use crate::retry::{Attempt, SendLabels, retry_send};
use crate::{
    AccountRun, Batch, Channel, ChannelKind, Downloaded, Inbound, Media, MediaKind, Outbound,
    OutboundFile, Resolved, SendOutcome, Transport,
};

mod api;
mod credentials;
mod inbound;
mod socket;

use api::{Api, Refusal};
pub use credentials::Account;
pub(crate) use credentials::Store;
use inbound::{Checkpoint, Mark, Received, Reference, valid_id};

/// The channel name: `scv channels <command> slack`.
pub const CHANNEL: &str = "slack";
/// The status error of a Slack account whose last contact failed, since
/// the specific cause stays in the daemon log.
pub const CONTACT_NOTE: &str = "Slack contact failed. Check that Socket Mode is on, that both tokens are valid with the scopes in the setup guide, and that HTTPS and WebSocket access to slack.com works. SCV retries automatically; the daemon log names the cause.";

/// How long one receive waits on the socket before reporting in.
const RECEIVE_WINDOW: Duration = Duration::from_secs(25);
/// Catch-up reaches back at most this far, whatever the checkpoint says.
const CATCH_UP_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// Conversations and threads listed, and pages of 50 messages each, in one
/// catch-up: at most 32 calls of each method, within the 50 a minute Slack
/// allows for history.
const CATCH_UP_CHATS: usize = 16;
const CATCH_UP_THREADS: usize = 16;
const CATCH_UP_PAGES: usize = 2;

/// The Slack channel, through a bot app in one workspace.
pub struct Slack;

/// Tokens the operator enters by hand; SCV never creates, rotates, or
/// discovers Slack secrets.
pub struct Login {
    pub bot_token: Secret,
    pub app_token: Secret,
    /// The owner's member ID (`U…` or `W…`); without one, remote tools stay
    /// off for everyone and an owner-only account answers nobody.
    pub owner_user_id: Option<String>,
}

#[async_trait]
impl Channel for Slack {
    const KIND: ChannelKind = ChannelKind::Slack;
    type Credentials = Account;
    type Login = Login;

    async fn login(layout: &Layout, account: &str, login: Login) -> Result<()> {
        credentials::login(&Store::new(layout, CHANNEL), account, login).await
    }

    fn owner(credentials: &Account) -> Option<&str> {
        credentials.owner_user_id.as_deref()
    }

    /// The bot's member ID.
    fn bot_id(credentials: &Account) -> Option<String> {
        Some(credentials.bot_user_id.clone())
    }

    fn title(_: &Account) -> &'static str {
        "Slack"
    }

    /// Only a live, caught-up connection reports healthy.
    async fn run(run: AccountRun<'_>, credentials: &Account) -> Result<()> {
        let bridge = run.bridge(Self::KIND)?;
        let transport = SocketMode::new(credentials.clone())?;
        let store = Store::new(run.layout, CHANNEL);
        crate::serve(&transport, bridge, &store, |saved| Ok(saved == credentials)).await
    }
}

/// The Slack transport of one account.
struct SocketMode {
    api: Api,
    credentials: Account,
    connection: Mutex<Connection>,
    /// How long one receive waits on the socket.
    window: Duration,
}

#[derive(Default)]
struct Connection {
    socket: Option<socket::Link>,
    /// Whether this socket's catch-up is done.
    caught_up: bool,
    /// How far this socket's catch-up got, so one that stopped part way
    /// resumes after what it already listed.
    progress: Progress,
    /// The envelope of the last batch, acknowledged on the next receive.
    pending: Option<String>,
}

/// One socket's catch-up so far.
#[derive(Default)]
struct Progress {
    /// Conversations and threads listed, by checkpoint key.
    listed: HashSet<String>,
    chats: usize,
    threads: usize,
    /// Threads the conversations' listings showed newer replies in, with
    /// their mark and the newest reply's `ts`, which ranks them.
    replied: HashMap<String, (Mark, u64)>,
}

impl SocketMode {
    fn new(credentials: Account) -> Result<Self> {
        credentials.validate()?;
        Ok(Self {
            api: Api::new(credentials.bot_token.clone(), credentials.app_token.clone())?,
            credentials,
            connection: Mutex::new(Connection::default()),
            window: RECEIVE_WINDOW,
        })
    }

    /// The tokens still belong to the saved installation. Slack is asked
    /// once per run, and again after it rejects the bot token.
    async fn verify(&self) -> Result<()> {
        let identity = self.api.identity().await?;
        if identity.team_id != self.credentials.team_id
            || identity.app_id != self.credentials.app_id
            || identity.bot_user_id != self.credentials.bot_user_id
        {
            bail!("Slack token belongs to a different workspace or app; log out before changing it")
        }
        Ok(())
    }

    /// Messages each known conversation and thread received since its
    /// mark, oldest first per conversation and thread, each once, moving
    /// the marks past everything listed, and whether the catch-up is done.
    ///
    /// The 16 most recently active conversations are listed, then 16
    /// threads: first those the listings showed newer replies in, by their
    /// newest reply, then the known ones by their marks. A listing Slack
    /// refuses is skipped. Any other failure, such as a rate limit, stops
    /// the catch-up: what it found so far is handed over, and the next call
    /// resumes after it; with nothing to hand over, it fails.
    async fn catch_up(
        &self,
        checkpoint: &mut Checkpoint,
        progress: &mut Progress,
    ) -> Result<(Vec<Inbound>, bool)> {
        let before = checkpoint.clone();
        let mut found = Vec::new();
        let listed = self.list(checkpoint, progress, &mut found).await;
        let mut handed = HashSet::new();
        found.retain(|received: &Received| handed.insert(received.inbound.id().to_owned()));
        let messages = found
            .into_iter()
            .map(|received| {
                checkpoint.observe(&received);
                received.inbound
            })
            .collect::<Vec<_>>();
        match listed {
            Ok(()) => Ok((messages, true)),
            Err(error) if !messages.is_empty() || *checkpoint != before => {
                tracing::warn!("Slack catch-up stopped part way and resumes next: {error:#}");
                Ok((messages, false))
            }
            Err(error) => Err(error),
        }
    }

    /// The listings of [`SocketMode::catch_up`], adding to `found`.
    async fn list(
        &self,
        checkpoint: &mut Checkpoint,
        progress: &mut Progress,
        found: &mut Vec<Received>,
    ) -> Result<()> {
        let floor = unix_micros().saturating_sub(CATCH_UP_WINDOW.as_micros() as u64);
        let chats = most_recent(&checkpoint.chats, CATCH_UP_CHATS)
            .into_iter()
            .filter(|(channel, _)| !progress.listed.contains(channel))
            .take(CATCH_UP_CHATS.saturating_sub(progress.chats))
            .collect::<Vec<_>>();
        for (channel, mark) in chats {
            let since = mark.last.max(floor);
            let mut newest_first = Vec::new();
            let mut cursor = None;
            for _ in 0..CATCH_UP_PAGES {
                let listed = self.api.history(&channel, since, cursor.as_deref()).await;
                let Some(page) = skip_refused(listed)? else {
                    break;
                };
                newest_first.extend(page.messages);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            let mut newest = since;
            for item in newest_first.iter().rev() {
                let Some(ts) = item["ts"].as_str().and_then(inbound::micros) else {
                    continue;
                };
                newest = newest.max(ts);
                found.extend(inbound::parse_history(
                    item,
                    &channel,
                    mark.group,
                    &self.credentials,
                ));
                // A root listed with replies newer than the mark.
                let latest = item["latest_reply"]
                    .as_str()
                    .and_then(inbound::micros)
                    .filter(|latest| *latest > since);
                if let Some(latest) = latest.filter(|_| item["thread_ts"] == item["ts"]) {
                    let key = format!("{channel}:{}", item["ts"].as_str().unwrap_or_default());
                    let seen = Mark {
                        group: mark.group,
                        last: since,
                    };
                    progress.replied.insert(key, (seen, latest));
                }
            }
            checkpoint.listed_chat(
                &channel,
                Mark {
                    group: mark.group,
                    last: newest,
                },
            );
            progress.listed.insert(channel);
            progress.chats += 1;
        }
        // Threads with replies the listings showed come first, newest reply
        // first, then known threads by their marks.
        let mut threads: HashMap<String, (Mark, Option<u64>)> = checkpoint
            .threads
            .iter()
            .map(|(key, mark)| (key.clone(), (*mark, None)))
            .collect();
        for (key, (mark, latest)) in &progress.replied {
            let known = threads.get(key).map_or(*mark, |(known, _)| *known);
            threads.insert(key.clone(), (known, Some(*latest)));
        }
        let mut threads = threads
            .into_iter()
            .filter(|(key, _)| !progress.listed.contains(key))
            .collect::<Vec<_>>();
        threads.sort_by_key(|(_, (mark, latest))| {
            std::cmp::Reverse((latest.is_some(), latest.unwrap_or(mark.last)))
        });
        threads.truncate(CATCH_UP_THREADS.saturating_sub(progress.threads));
        for (key, (mark, _)) in threads {
            let Some((channel, root)) = key.split_once(':') else {
                continue;
            };
            let since = mark.last.max(floor);
            let mut newest = since;
            let mut cursor = None;
            for _ in 0..CATCH_UP_PAGES {
                let listed = self
                    .api
                    .replies(channel, root, since, cursor.as_deref())
                    .await;
                let Some(page) = skip_refused(listed)? else {
                    break;
                };
                for item in &page.messages {
                    let Some(ts) = item["ts"].as_str().and_then(inbound::micros) else {
                        continue;
                    };
                    if ts <= since || item["ts"] == root {
                        continue;
                    }
                    newest = newest.max(ts);
                    found.extend(inbound::parse_history(
                        item,
                        channel,
                        mark.group,
                        &self.credentials,
                    ));
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            checkpoint.listed_thread(
                &key,
                Mark {
                    group: mark.group,
                    last: newest,
                },
            );
            progress.listed.insert(key);
            progress.threads += 1;
        }
        Ok(())
    }
}

/// The `count` most recently active of `marks`, newest first.
fn most_recent(
    marks: &std::collections::BTreeMap<String, Mark>,
    count: usize,
) -> Vec<(String, Mark)> {
    let mut marks: Vec<_> = marks
        .iter()
        .map(|(key, mark)| (key.clone(), *mark))
        .collect();
    marks.sort_by_key(|(_, mark)| std::cmp::Reverse(mark.last));
    marks.truncate(count);
    marks
}

/// A history page, or `None` with a warning when Slack refused the listing,
/// as it does for a conversation the bot left or a scope the app lacks. A
/// rejected token fails it, since no listing can work.
fn skip_refused(listed: Result<api::Page>) -> Result<Option<api::Page>> {
    match listed {
        Ok(page) => Ok(Some(page)),
        Err(error) => match error.downcast_ref::<Refusal>() {
            Some(refusal) if !refusal.is_auth() => {
                tracing::warn!("Slack catch-up skipped a conversation or thread: {refusal}");
                Ok(None)
            }
            _ => Err(error),
        },
    }
}

#[async_trait]
impl Transport for SocketMode {
    fn label(&self) -> &'static str {
        "Slack"
    }

    fn channel(&self) -> &'static str {
        "Slack"
    }

    /// Acknowledge the previous batch, (re)connect and catch up when needed,
    /// then wait for one message event.
    async fn receive(&self, cursor: &str) -> Result<Batch> {
        let mut checkpoint = Checkpoint::parse(cursor);
        let before = checkpoint.clone();
        let mut guard = self.connection.lock().await;
        let connection = &mut *guard;
        // A rejected bot token fails every receive until the owner signs in
        // again, even while the app-level token keeps the socket open.
        self.verify().await?;
        let pending = connection.pending.take();
        // Taking the socket means an error or cancellation discards it, so
        // an envelope ID is never sent to a replacement socket.
        let mut socket = if let Some(mut socket) = connection.socket.take() {
            if let Some(id) = pending {
                socket.ack(&id).await?;
            }
            socket
        } else {
            connection.caught_up = false;
            connection.progress = Progress::default();
            socket::Link::connect(&self.api, &self.credentials.app_id).await?
        };
        if !connection.caught_up {
            // Connected first, so nothing sent during catch-up is missed;
            // kept when catch-up stops, which resumes on the next call.
            connection.socket = Some(socket);
            let (messages, done) = self
                .catch_up(&mut checkpoint, &mut connection.progress)
                .await?;
            if done {
                connection.caught_up = true;
                // Nothing read the socket meanwhile, so its silence so far
                // says nothing about it.
                if let Some(socket) = connection.socket.as_mut() {
                    socket.resume();
                }
            }
            // Report the new connection healthy at once.
            return Ok(batch(messages, &before, &checkpoint));
        }
        let deadline = Instant::now() + self.window;
        loop {
            let Some(envelope) = socket.next(deadline).await? else {
                connection.socket = Some(socket);
                return Ok(batch(Vec::new(), &before, &checkpoint));
            };
            if let Some(received) = inbound::parse_event(&envelope.payload, &self.credentials) {
                // One message per batch: its acknowledgement waits only for
                // its claim, never for a turn.
                checkpoint.observe(&received);
                connection.pending = Some(envelope.envelope_id);
                connection.socket = Some(socket);
                return Ok(batch(vec![received.inbound], &before, &checkpoint));
            }
            // Nothing to claim: acknowledge at once.
            socket.ack(&envelope.envelope_id).await?;
        }
    }

    /// Replies go to their conversation, inside the thread they came from;
    /// a message that answers nothing, such as a background report, goes to
    /// the user's direct conversation. Slack has no idempotency key, so a
    /// retry after a lost response can post a part twice.
    async fn send(
        &self,
        message: &Outbound<'_>,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome> {
        // Recovered replies can go out before the first receive.
        self.verify().await?;
        let labels = SendLabels {
            refused: "Slack refused a reply",
            failed: "Slack reply send failed",
            gave_up: "Slack could not deliver the reply",
        };
        let sent = retry_send(labels, report, || self.api.post(message)).await?;
        Ok(outcome(sent))
    }

    async fn download(&self, media: &Media, max_bytes: u64) -> Result<Downloaded> {
        let source: serde_json::Value =
            serde_json::from_str(&media.source).map_err(|_| anyhow!("bad media source"))?;
        let url = source["url"]
            .as_str()
            .ok_or_else(|| anyhow!("bad media source"))?;
        let html = media
            .mime
            .as_deref()
            .is_some_and(|mime| mime.starts_with("text/html"));
        let (bytes, mime) = self.api.download(url, max_bytes, html).await?;
        Ok(Downloaded { bytes, mime })
    }

    /// Fetch the root a thread is on, with its files, and show what shared
    /// messages say, as text before the message.
    async fn resolve(&self, _: &str, reference: &str) -> Result<Resolved> {
        let reference: Reference =
            serde_json::from_str(reference).map_err(|_| anyhow!("bad message reference"))?;
        let mut resolved = Resolved::default();
        if let (Some(channel), Some(root)) = (&reference.channel, &reference.root) {
            let item = self.api.message(channel, root).await?;
            let (text, media) = inbound::content(&item, &self.credentials);
            let text = match (text.is_empty(), media.first()) {
                (false, _) => text,
                (true, Some(file)) => match file.kind {
                    MediaKind::Image => "an image".into(),
                    MediaKind::Audio => "a voice message".into(),
                    MediaKind::Video => "a video".into(),
                    MediaKind::File => "a file".into(),
                },
                (true, None) => "a message".into(),
            };
            resolved.context = format!(
                "[Thread on: {}]",
                inbound::bounded(&text, inbound::MAX_CONTEXT_BYTES)
            );
            resolved.media = media;
        }
        if let Some(quote) = &reference.quote {
            if !resolved.context.is_empty() {
                resolved.context.push_str("\n\n");
            }
            resolved.context.push_str(&format!("[Quoting: {quote}]"));
        }
        Ok(resolved)
    }

    /// Upload the file, then share it like a text part: in the reply's
    /// conversation and thread, or the user's direct conversation.
    async fn send_file(
        &self,
        file: &OutboundFile<'_>,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome> {
        self.verify().await?;
        let Ok((target, thread)) = inbound::target(file.reply_to, file.to) else {
            tracing::warn!("Slack refused a file (invalid Slack reply handle); not retrying");
            return Ok(SendOutcome::Rejected);
        };
        let path = file.path.to_owned();
        let bytes = tokio::task::spawn_blocking(move || {
            crate::media::read_bounded(&path, crate::media::MAX_REPLY_FILE_BYTES)
        })
        .await??;
        let labels = SendLabels {
            refused: "Slack refused a file upload",
            failed: "Slack file upload failed",
            gave_up: "Slack could not upload the file",
        };
        let uploaded =
            retry_send(labels, report, || self.api.upload(file.name, bytes.clone())).await?;
        let Some(file_id) = uploaded else {
            return Ok(SendOutcome::Rejected);
        };
        let labels = SendLabels {
            refused: "Slack refused a file share",
            failed: "Slack file share failed",
            gave_up: "Slack could not share the file",
        };
        let shared = retry_send(labels, report, || async {
            // Files are shared into a conversation by its ID, so a user's
            // direct conversation is opened first.
            let channel = if valid_id(target, "UW") {
                match api::attempt(self.api.open_direct(target).await) {
                    Attempt::Done(channel) => channel,
                    Attempt::Refused(reason) => return Attempt::Refused(reason),
                    Attempt::Retry(reason) => return Attempt::Retry(reason),
                }
            } else {
                target.to_owned()
            };
            self.api.share(&file_id, file.name, &channel, thread).await
        })
        .await?;
        Ok(outcome(shared))
    }
}

/// A retried send's result as the bridge counts it: a refusal is final.
fn outcome(sent: Option<()>) -> SendOutcome {
    match sent {
        Some(()) => SendOutcome::Delivered,
        None => SendOutcome::Rejected,
    }
}

fn batch(messages: Vec<Inbound>, before: &Checkpoint, after: &Checkpoint) -> Batch {
    Batch {
        messages,
        checkpoint: (after != before).then(|| after.to_json()),
    }
}

fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64)
}

#[cfg(test)]
mod tests;
