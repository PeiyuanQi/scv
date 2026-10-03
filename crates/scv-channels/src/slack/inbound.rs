//! Turning Slack messages, from Socket Mode events or a conversation's
//! history, into the bridge's inbound messages, and the checkpoint that
//! drives catch-up.
//!
//! A conversation is a direct message (`D…`) or a channel that mentions the
//! bot (`C…`, `G…`). A message whose `thread_ts` names another message is in
//! that message's thread: a conversation of its own, answered inside it.

use super::Account;
use crate::{Inbound, Media, MediaKind, Message, Thread};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Longest context shared messages or a thread's root add, in bytes.
pub(super) const MAX_CONTEXT_BYTES: usize = 16 * 1024;
/// Conversations the checkpoint remembers for catch-up, most recently
/// active first.
const MAX_CHATS: usize = 64;
/// Threads the checkpoint remembers for catch-up, likewise.
const MAX_THREADS: usize = 64;
/// Longest file name kept, in characters.
const MAX_NAME_CHARS: usize = 255;

pub(super) fn valid_id(value: &str, prefixes: &str) -> bool {
    (2..=128).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(|b| prefixes.as_bytes().contains(b))
        && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A Slack timestamp (`ts`, `1700000000.000100`) in microseconds; it is
/// also the message's ID within its conversation.
pub(super) fn micros(value: &str) -> Option<u64> {
    let (seconds, fraction) = value.split_once('.')?;
    if seconds.is_empty()
        || seconds.len() > 12
        || fraction.len() != 6
        || !seconds
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    seconds
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(fraction.parse().ok()?)
}

/// Microseconds as a Slack timestamp.
pub(super) fn ts(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

/// The reply handle for `channel`, inside the thread on `root` when given.
fn handle(channel: &str, root: Option<&str>) -> String {
    root.map_or_else(
        || format!("slack:{channel}"),
        |root| format!("slack:{channel}/thread:{root}"),
    )
}

/// Where a reply goes: the conversation, and the thread root when it goes
/// inside a thread. An empty handle, a message that answers nothing, goes
/// to `user` directly.
pub(super) fn target<'a>(handle: &'a str, user: &'a str) -> Result<(&'a str, Option<&'a str>)> {
    if handle.is_empty() && valid_id(user, "UW") {
        return Ok((user, None));
    }
    if let Some(rest) = handle.strip_prefix("slack:") {
        let (channel, root) = rest
            .split_once("/thread:")
            .map_or((rest, None), |(channel, root)| (channel, Some(root)));
        if valid_id(channel, "DCG") && root.is_none_or(|root| micros(root).is_some()) {
            return Ok((channel, root));
        }
    }
    bail!("invalid Slack reply handle")
}

/// A received message and where it belongs in the checkpoint.
pub(super) struct Received {
    pub(super) inbound: Inbound,
    pub(super) channel: String,
    /// The root of the thread it was sent in, if any.
    pub(super) thread: Option<String>,
    pub(super) group: bool,
    /// Its `ts`, in microseconds.
    pub(super) ts: u64,
}

/// A Socket Mode `events_api` payload: a direct message to the bot, or a
/// mention of it in a channel, from this installation.
pub(super) fn parse_event(payload: &Value, account: &Account) -> Option<Received> {
    if payload["type"] != "event_callback"
        || payload["team_id"] != account.team_id
        || payload["api_app_id"] != account.app_id
    {
        return None;
    }
    let event = &payload["event"];
    let channel = event["channel"].as_str().filter(|id| valid_id(id, "DCG"))?;
    let direct = channel.starts_with('D') && event["channel_type"] == "im";
    // Direct messages come as `message.im`; a channel's messages only as
    // the mentions `app_mention` carries, never from a direct conversation.
    match event["type"].as_str()? {
        "message" if direct => {}
        "app_mention" if !channel.starts_with('D') => {}
        _ => return None,
    }
    parse_message(event, channel, !direct, account)
}

/// One message of `channel`'s history; `group` says whether the
/// conversation is a channel rather than a direct message, which history
/// does not repeat.
pub(super) fn parse_history(
    item: &Value,
    channel: &str,
    group: bool,
    account: &Account,
) -> Option<Received> {
    if item["type"] != "message" {
        return None;
    }
    parse_message(item, channel, group, account)
}

fn parse_message(event: &Value, channel: &str, group: bool, account: &Account) -> Option<Received> {
    // Bots (SCV included), apps, edits, deletions, and joins are not
    // requests; a message with files is, and so is a thread's reply also
    // sent to its conversation.
    if event.get("bot_id").is_some()
        || event.get("app_id").is_some()
        || event
            .get("subtype")
            .is_some_and(|subtype| subtype != "file_share" && subtype != "thread_broadcast")
    {
        return None;
    }
    let sender = event["user"].as_str().filter(|id| valid_id(id, "UW"))?;
    if sender == account.bot_user_id {
        return None;
    }
    let ts_text = event["ts"].as_str()?;
    let ts = micros(ts_text)?;
    let mention = format!("<@{}>", account.bot_user_id);
    let raw = event["text"].as_str().unwrap_or_default();
    // In a channel the bot answers only messages that mention it.
    if group && !raw.contains(&mention) {
        return None;
    }
    let mut text = decode(&raw.replace(&mention, ""));
    let media = files(event, &mut text);
    // A thread's root names its own `ts`; any other `thread_ts` is the
    // root of the thread this message is in.
    let root = match event.get("thread_ts") {
        Some(root) => {
            let root = root.as_str()?;
            micros(root)?;
            (root != ts_text).then_some(root)
        }
        None => None,
    };
    let quote = shared(event);
    if text.trim().is_empty() && media.is_empty() && quote.is_none() {
        return None;
    }
    let reference = quote.map(|quote| {
        Reference {
            quote: Some(quote),
            ..Reference::default()
        }
        .to_json()
    });
    let thread = root.map(|root| Thread {
        id: format!("{channel}:{root}"),
        reply_to: handle(channel, Some(root)),
        origin: Some(
            Reference {
                channel: Some(channel.to_owned()),
                root: Some(root.to_owned()),
                quote: None,
            }
            .to_json(),
        ),
    });
    Some(Received {
        inbound: Inbound::Text(Message {
            id: format!("{channel}:{ts_text}"),
            sender: sender.into(),
            text: text.trim().into(),
            reply_to: handle(channel, root),
            group: group.then(|| channel.into()),
            media,
            // Attachments other than shares, such as unfurls, still mark it
            // quoted, so a mail chat never reads it as a command.
            quoted: reference.is_some()
                || event["attachments"]
                    .as_array()
                    .is_some_and(|attachments| !attachments.is_empty()),
            reference,
            sent_ms: Some(ts / 1000),
            thread,
        }),
        channel: channel.into(),
        thread: root.map(str::to_owned),
        group,
        ts,
    })
}

/// The text and files of a message from history, such as a thread's root,
/// for context; the bot's mention is dropped.
pub(super) fn content(item: &Value, account: &Account) -> (String, Vec<Media>) {
    let mention = format!("<@{}>", account.bot_user_id);
    let raw = item["text"].as_str().unwrap_or_default();
    let mut text = decode(&raw.replace(&mention, ""));
    let media = files(item, &mut text);
    (text.trim().to_owned(), media)
}

/// Slack escapes `&`, `<`, and `>` in message text; `&amp;` goes last.
fn decode(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The files a message carries, to download before its turn. A file Slack
/// gives no download for (deleted, hidden by the plan's limit, stored
/// outside Slack, or shared from another organization) becomes a marker in
/// `text` instead.
fn files(event: &Value, text: &mut String) -> Vec<Media> {
    let mut media = Vec::new();
    for file in event["files"].as_array().into_iter().flatten() {
        let name: String = file["name"]
            .as_str()
            .or_else(|| file["title"].as_str())
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_NAME_CHARS)
            .collect();
        let mime = file["mimetype"]
            .as_str()
            .filter(|mime| mime.len() <= 128 && !mime.chars().any(char::is_control))
            .map(str::to_owned);
        let kind = if file["subtype"] == "slack_audio" {
            MediaKind::Audio
        } else {
            match mime.as_deref().and_then(|mime| mime.split_once('/')) {
                Some(("image", _)) => MediaKind::Image,
                Some(("video", _)) => MediaKind::Video,
                Some(("audio", _)) => MediaKind::Audio,
                _ => MediaKind::File,
            }
        };
        let hosted = !matches!(
            file["mode"].as_str(),
            Some("tombstone" | "hidden_by_limit" | "external")
        );
        let url = file["url_private_download"]
            .as_str()
            .or_else(|| file["url_private"].as_str())
            .filter(|_| hosted);
        let Some(url) = url else {
            if !text.is_empty() {
                text.push('\n');
            }
            let label = if name.is_empty() {
                kind.noun().to_owned()
            } else {
                format!("{} {name}", kind.noun())
            };
            text.push_str(&format!("[{label}: Slack offers no download of it]"));
            continue;
        };
        media.push(Media {
            kind,
            name,
            size: file["size"].as_u64(),
            mime,
            transcript: transcript(file),
            source: json!({"url": url}).to_string(),
        });
    }
    media
}

/// The transcript Slack made of an audio or video clip, when it finished.
fn transcript(file: &Value) -> Option<String> {
    if file.pointer("/transcription/status")? != "complete" {
        return None;
    }
    let text = file.pointer("/transcription/preview/content")?.as_str()?;
    Some(bounded(text.trim(), MAX_CONTEXT_BYTES)).filter(|text| !text.is_empty())
}

/// What messages shared into this one say, as Slack includes them (a
/// shared or linked message), with their authors, bounded.
fn shared(event: &Value) -> Option<String> {
    let mut lines = Vec::new();
    for attachment in event["attachments"].as_array().into_iter().flatten() {
        if attachment["is_share"] != true && attachment["is_msg_unfurl"] != true {
            continue;
        }
        let text = attachment["text"]
            .as_str()
            .or_else(|| attachment["fallback"].as_str())
            .unwrap_or_default();
        let text = decode(text);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match attachment["author_name"]
            .as_str()
            .map(str::trim)
            .filter(|author| !author.is_empty())
        {
            Some(author) => lines.push(format!("{author}: {text}")),
            None => lines.push(text.to_owned()),
        }
    }
    (!lines.is_empty()).then(|| bounded(&lines.join("\n"), MAX_CONTEXT_BYTES))
}

/// `text` cut to at most `max` bytes on a character boundary.
pub(super) fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    format!("{}…", scv_client::text::utf8_prefix(text, max))
}

/// What a message refers to, resolved before its turn: for a thread's
/// origin the root the thread is on, and the text of messages shared into
/// it, which Slack sends with the message itself.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Reference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) quote: Option<String>,
}

impl Reference {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What the transport has received, per conversation and per thread: the
/// newest `ts` it listed or handed to the bridge. Catch-up lists each one's
/// history from there. A conversation's history leaves out the replies in
/// its threads, which is why threads have marks of their own.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Checkpoint {
    /// By conversation ID.
    #[serde(default)]
    pub(super) chats: BTreeMap<String, Mark>,
    /// By thread, `<conversation>:<root ts>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) threads: BTreeMap<String, Mark>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Mark {
    pub(super) group: bool,
    /// The newest `ts`, in microseconds.
    pub(super) last: u64,
}

impl Checkpoint {
    /// An unreadable checkpoint starts empty: catch-up then has nothing to
    /// list, and deduplication still drops repeats.
    pub(super) fn parse(value: &str) -> Self {
        if value.is_empty() {
            return Self::default();
        }
        serde_json::from_str(value).unwrap_or_else(|_| {
            tracing::warn!("Slack checkpoint was unreadable; catch-up starts fresh");
            Self::default()
        })
    }

    /// Note that `received` was handed to the bridge. A thread's message
    /// moves its thread's mark, and makes its conversation known from then
    /// on without moving the conversation's mark, which only the
    /// conversation's own messages move.
    pub(super) fn observe(&mut self, received: &Received) {
        let seen = Mark {
            group: received.group,
            last: received.ts,
        };
        match &received.thread {
            Some(root) => {
                self.chats.entry(received.channel.clone()).or_insert(seen);
                let key = format!("{}:{root}", received.channel);
                advance(&mut self.threads, &key, seen, MAX_THREADS);
            }
            None => advance(&mut self.chats, &received.channel, seen, MAX_CHATS),
        }
        bound(&mut self.chats, MAX_CHATS);
    }

    /// Move a conversation's mark up to `seen`: catch-up listed everything
    /// before it, including messages that were not for the bot.
    pub(super) fn listed_chat(&mut self, channel: &str, seen: Mark) {
        advance(&mut self.chats, channel, seen, MAX_CHATS);
    }

    /// Likewise for the thread `key`.
    pub(super) fn listed_thread(&mut self, key: &str, seen: Mark) {
        advance(&mut self.threads, key, seen, MAX_THREADS);
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Move `key`'s mark in `marks` up to `seen`, adding it when new, and keep
/// at most `max` marks.
fn advance(marks: &mut BTreeMap<String, Mark>, key: &str, seen: Mark, max: usize) {
    let mark = marks
        .entry(key.to_owned())
        .or_insert(Mark { last: 0, ..seen });
    mark.last = mark.last.max(seen.last);
    bound(marks, max);
}

/// Forget the least recently active marks beyond `max`.
fn bound(marks: &mut BTreeMap<String, Mark>, max: usize) {
    while marks.len() > max {
        let oldest = marks
            .iter()
            .min_by_key(|(_, mark)| mark.last)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(oldest) => marks.remove(&oldest),
            None => break,
        };
    }
}

#[cfg(test)]
mod tests;
