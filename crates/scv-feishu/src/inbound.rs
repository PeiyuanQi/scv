//! Turning Feishu messages, from socket events or chat history, into the
//! bridge's inbound messages, and the checkpoint that drives catch-up.

use crate::api::Resource;
use scv_channels::{Inbound, Media, MediaKind, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Longest text taken from an interactive card, in characters.
const MAX_CARD_CHARS: usize = 2000;

const MAX_ID_BYTES: usize = 256;
/// Chats the checkpoint remembers for catch-up, most recently active first.
pub const MAX_CHATS: usize = 64;

/// A received message and where it belongs in the checkpoint.
pub struct Received {
    pub inbound: Inbound,
    pub chat_id: String,
    pub group: bool,
    /// Creation time in Unix milliseconds.
    pub created_ms: u64,
}

/// What a socket event carried. Events are handled one at a time, so the
/// message variant's size does not matter.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Message(Received),
    /// Another event type, such as `im.message.message_read_v1`.
    Other,
}

/// Parse an event payload. Errors mean a malformed event, which is
/// acknowledged and dropped.
pub fn parse_event(payload: &[u8], bot_open_id: Option<&str>) -> Option<Event> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    if value.pointer("/header/event_type").and_then(Value::as_str) != Some("im.message.receive_v1")
    {
        return Some(Event::Other);
    }
    let message = value.pointer("/event/message")?;
    let sender_type = value
        .pointer("/event/sender/sender_type")
        .and_then(Value::as_str);
    let sender = value
        .pointer("/event/sender/sender_id/open_id")
        .and_then(Value::as_str);
    let mentions = message
        .get("mentions")
        .and_then(Value::as_array)
        .map(|mentions| {
            mentions
                .iter()
                .filter_map(|m| {
                    Some(Mention {
                        key: m.get("key")?.as_str()?,
                        open_id: m.pointer("/id/open_id").and_then(Value::as_str),
                        name: m.get("name").and_then(Value::as_str).unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    received(
        message,
        str_field(message, "chat_type"),
        str_field(message, "message_type"),
        str_field(message, "content"),
        sender_type == Some("user"),
        sender,
        &mentions,
        bot_open_id,
    )
    .map(Event::Message)
}

/// Parse one item of a chat's history. `group` is the chat's kind, which
/// history items do not repeat.
pub fn parse_history(item: &Value, group: bool, bot_open_id: Option<&str>) -> Option<Received> {
    if item.get("deleted").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let from_user = item.pointer("/sender/sender_type").and_then(Value::as_str) == Some("user")
        && item.pointer("/sender/id_type").and_then(Value::as_str) == Some("open_id");
    let sender = item.pointer("/sender/id").and_then(Value::as_str);
    let mentions = item
        .get("mentions")
        .and_then(Value::as_array)
        .map(|mentions| {
            mentions
                .iter()
                .filter_map(|m| {
                    Some(Mention {
                        key: m.get("key")?.as_str()?,
                        open_id: (m.get("id_type").and_then(Value::as_str) == Some("open_id"))
                            .then(|| m.get("id").and_then(Value::as_str))
                            .flatten(),
                        name: m.get("name").and_then(Value::as_str).unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // The bot's own messages and other apps' are history, not requests.
    if item.pointer("/sender/sender_type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    received(
        item,
        Some(if group { "group" } else { "p2p" }),
        str_field(item, "msg_type"),
        item.pointer("/body/content").and_then(Value::as_str),
        from_user,
        sender,
        &mentions,
        bot_open_id,
    )
}

struct Mention<'a> {
    key: &'a str,
    open_id: Option<&'a str>,
    name: &'a str,
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[allow(clippy::too_many_arguments)]
fn received(
    message: &Value,
    chat_type: Option<&str>,
    message_type: Option<&str>,
    content: Option<&str>,
    from_user: bool,
    sender: Option<&str>,
    mentions: &[Mention<'_>],
    bot_open_id: Option<&str>,
) -> Option<Received> {
    let id = str_field(message, "message_id").filter(|id| valid_id(id))?;
    let chat_id = str_field(message, "chat_id").filter(|id| valid_id(id))?;
    let created_ms = str_field(message, "create_time")
        .and_then(|time| time.parse().ok())
        .unwrap_or(0);
    let group = chat_type != Some("p2p");
    let ignored = || Received {
        inbound: Inbound::Ignored { id: id.to_owned() },
        chat_id: chat_id.to_owned(),
        group,
        created_ms,
    };
    let Some(sender) = sender.filter(|sender| from_user && valid_id(sender)) else {
        return Some(ignored());
    };
    // In a group the bot answers only messages that mention it.
    let mentioned = bot_open_id.is_some_and(|bot| mentions.iter().any(|m| m.open_id == Some(bot)));
    if group && !mentioned {
        return Some(ignored());
    }
    let content = content
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .unwrap_or_default();
    let Some(parsed) = message_type.and_then(|kind| parse_content(id, kind, &content)) else {
        return Some(ignored());
    };
    let text = replace_mentions(&parsed.text, mentions, bot_open_id);
    let parent = str_field(message, "parent_id").filter(|parent| valid_id(parent));
    let reference = (parent.is_some() || parsed.forward).then(|| {
        serde_json::to_string(&Reference {
            parent: parent.map(str::to_owned),
            forward: parsed.forward,
        })
        .unwrap_or_default()
    });
    if text.trim().is_empty() && parsed.media.is_empty() && reference.is_none() {
        return Some(ignored());
    }
    Some(Received {
        inbound: Inbound::Text(Message {
            id: id.to_owned(),
            sender: sender.to_owned(),
            text: text.trim().to_owned(),
            reply_to: id.to_owned(),
            group: group.then(|| chat_id.to_owned()),
            media: parsed.media,
            reference,
        }),
        chat_id: chat_id.to_owned(),
        group,
        created_ms,
    })
}

/// What a message refers to, resolved before its turn: the message it
/// quotes, and for a forwarded bundle the messages inside it.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forward: bool,
}

/// A message's content as the bridge takes it: text, including markers for
/// what has no file, and the files to fetch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub text: String,
    pub media: Vec<Media>,
    /// A forwarded bundle, whose messages are fetched before the turn.
    pub forward: bool,
}

/// Parse the content of message `id` of `kind`, or `None` for a system
/// message, which is only marked seen.
pub fn parse_content(id: &str, kind: &str, content: &Value) -> Option<Content> {
    let text = |key: &str| content.get(key).and_then(Value::as_str).unwrap_or_default();
    let resource = |key: &str, resource: &str| Resource {
        message_id: id.to_owned(),
        key: key.to_owned(),
        kind: resource.to_owned(),
    };
    let media =
        |kind: MediaKind, key: &str, resource_kind: &str, name: &str, mime: Option<&str>| {
            (!key.is_empty()).then(|| Media {
                kind,
                name: name.to_owned(),
                size: None,
                mime: mime.map(str::to_owned),
                transcript: None,
                source: serde_json::to_string(&resource(key, resource_kind)).unwrap_or_default(),
            })
        };
    let mut parsed = Content::default();
    match kind {
        "text" => parsed.text = text("text").to_owned(),
        "post" => {
            let (body, files) = post(id, content);
            parsed.text = body;
            parsed.media = files;
        }
        "image" => parsed.media.extend(media(
            MediaKind::Image,
            text("image_key"),
            "image",
            "",
            None,
        )),
        "file" => parsed.media.extend(media(
            MediaKind::File,
            text("file_key"),
            "file",
            text("file_name"),
            None,
        )),
        "audio" => parsed.media.extend(media(
            MediaKind::Audio,
            text("file_key"),
            "file",
            "",
            Some("audio/opus"),
        )),
        "media" => parsed.media.extend(media(
            MediaKind::Video,
            text("file_key"),
            "file",
            text("file_name"),
            None,
        )),
        // Feishu's resource API does not serve stickers.
        "sticker" => parsed.text = "[sticker]".into(),
        "share_chat" => parsed.text = "[shared a group chat]".into(),
        "share_user" => parsed.text = "[shared a contact card]".into(),
        "location" => {
            let name = text("name");
            let (latitude, longitude) = (text("latitude"), text("longitude"));
            parsed.text = if latitude.is_empty() {
                format!("[location: {name}]")
            } else {
                format!("[location: {name} ({latitude}, {longitude})]")
            };
        }
        "interactive" => parsed.text = format!("[card] {}", card_text(content)),
        "merge_forward" => {
            parsed.text = "[Forwarded messages]".into();
            parsed.forward = true;
        }
        "system" => return None,
        other => parsed.text = format!("[{other} message]"),
    }
    Some(parsed)
}

/// The readable text of an interactive card: its titles and text elements,
/// one per line, bounded.
fn card_text(content: &Value) -> String {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                // A title reads before the body, whatever the key order.
                let keys = ["title", "text", "content"];
                for key in keys {
                    if let Some(Value::String(text)) = map.get(key)
                        && !text.trim().is_empty()
                    {
                        out.push(text.trim().to_owned());
                    }
                }
                for (key, value) in map {
                    if !value.is_string() || !keys.contains(&key.as_str()) {
                        collect(value, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            _ => {}
        }
    }
    let mut lines = Vec::new();
    collect(content, &mut lines);
    let text = lines.join("\n");
    let mut bounded: String = text.chars().take(MAX_CARD_CHARS).collect();
    if text.chars().count() > MAX_CARD_CHARS {
        bounded.push('…');
    }
    bounded
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

/// Mention placeholders such as `@_user_1` become `@name`; the bot's own
/// mention is dropped.
fn replace_mentions(text: &str, mentions: &[Mention<'_>], bot_open_id: Option<&str>) -> String {
    let mut text = text.to_owned();
    // Longer keys first, so `@_user_1` does not clobber `@_user_10`.
    let mut ordered: Vec<_> = mentions.iter().filter(|m| !m.key.is_empty()).collect();
    ordered.sort_by_key(|m| std::cmp::Reverse(m.key.len()));
    for mention in ordered {
        let replacement = if bot_open_id.is_some() && mention.open_id == bot_open_id {
            String::new()
        } else {
            format!("@{}", mention.name)
        };
        text = text.replace(mention.key, &replacement);
    }
    text
}

/// The text of a rich-text (`post`) message, one paragraph per line, and
/// the images and videos embedded in it.
fn post(id: &str, content: &Value) -> (String, Vec<Media>) {
    // Events carry the post itself; some payloads wrap it by locale.
    let post = if content.get("content").is_some() {
        Some(content)
    } else {
        content
            .as_object()
            .and_then(|map| map.values().find(|v| v.get("content").is_some()))
    };
    let Some(post) = post else {
        return (String::new(), Vec::new());
    };
    let mut lines = Vec::new();
    let mut media = Vec::new();
    if let Some(title) = post.get("title").and_then(Value::as_str)
        && !title.trim().is_empty()
    {
        lines.push(title.to_owned());
    }
    let embedded = |kind: MediaKind, key: Option<&str>, resource: &str| {
        key.filter(|key| !key.is_empty()).map(|key| Media {
            kind,
            name: String::new(),
            size: None,
            mime: None,
            transcript: None,
            source: serde_json::to_string(&Resource {
                message_id: id.to_owned(),
                key: key.to_owned(),
                kind: resource.to_owned(),
            })
            .unwrap_or_default(),
        })
    };
    for paragraph in post
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut line = String::new();
        for element in paragraph.as_array().into_iter().flatten() {
            let text = element.get("text").and_then(Value::as_str);
            match element.get("tag").and_then(Value::as_str) {
                Some("text" | "a" | "code_block" | "md") => line.push_str(text.unwrap_or_default()),
                Some("at") => {
                    if let Some(name) = element.get("user_name").and_then(Value::as_str) {
                        line.push('@');
                        line.push_str(name);
                    }
                }
                Some("img") => media.extend(embedded(
                    MediaKind::Image,
                    element.get("image_key").and_then(Value::as_str),
                    "image",
                )),
                Some("media") => media.extend(embedded(
                    MediaKind::Video,
                    element.get("file_key").and_then(Value::as_str),
                    "file",
                )),
                Some("emotion") => {
                    if let Some(emoji) = element.get("emoji_type").and_then(Value::as_str) {
                        line.push_str(&format!("[{emoji}]"));
                    }
                }
                _ => {}
            }
        }
        lines.push(line);
    }
    (lines.join("\n"), media)
}

/// What the transport has received, per chat: the newest creation time it
/// handed to the bridge. Catch-up lists each chat's history from there.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(default)]
    pub chats: BTreeMap<String, ChatMark>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMark {
    pub group: bool,
    pub last_ms: u64,
}

impl Checkpoint {
    /// An unreadable checkpoint starts empty: catch-up then has nothing to
    /// list, and deduplication still drops repeats.
    pub fn parse(value: &str) -> Self {
        if value.is_empty() {
            return Self::default();
        }
        serde_json::from_str(value).unwrap_or_else(|_| {
            tracing::warn!("Feishu checkpoint was unreadable; catch-up starts fresh");
            Self::default()
        })
    }

    pub fn observe(&mut self, received: &Received) {
        let mark = self
            .chats
            .entry(received.chat_id.clone())
            .or_insert(ChatMark {
                group: received.group,
                last_ms: 0,
            });
        mark.last_ms = mark.last_ms.max(received.created_ms);
        if self.chats.len() > MAX_CHATS {
            let oldest = self
                .chats
                .iter()
                .min_by_key(|(_, mark)| mark.last_ms)
                .map(|(chat, _)| chat.clone());
            if let Some(oldest) = oldest {
                self.chats.remove(&oldest);
            }
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BOT: &str = "ou_bot";

    fn event(chat_type: &str, message_type: &str, content: Value, mentions: Value) -> Vec<u8> {
        json!({
            "schema": "2.0",
            "header": {"event_id": "e1", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_user"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_1", "chat_id": "oc_1", "chat_type": chat_type,
                    "create_time": "1790221416232", "message_type": message_type,
                    "content": content.to_string(), "mentions": mentions,
                },
            },
        })
        .to_string()
        .into_bytes()
    }

    fn text(event: Option<Event>) -> Option<(String, Option<String>)> {
        match event? {
            Event::Message(Received {
                inbound: Inbound::Text(message),
                ..
            }) => Some((message.text, message.group)),
            _ => None,
        }
    }

    #[test]
    fn direct_text_becomes_a_message_to_answer() {
        let parsed = parse_event(
            &event("p2p", "text", json!({"text": " hello "}), json!([])),
            Some(BOT),
        );
        let Some(Event::Message(received)) = parsed else {
            panic!("expected a message")
        };
        assert_eq!(received.chat_id, "oc_1");
        assert_eq!(received.created_ms, 1790221416232);
        assert!(!received.group);
        let Inbound::Text(message) = received.inbound else {
            panic!("expected text")
        };
        assert_eq!(
            (
                message.id.as_str(),
                message.sender.as_str(),
                message.text.as_str()
            ),
            ("om_1", "ou_user", "hello")
        );
        assert_eq!(message.reply_to, "om_1");
        assert_eq!(message.group, None);
    }

    #[test]
    fn group_messages_count_only_when_they_mention_the_bot() {
        let mentions = json!([
            {"key": "@_user_1", "id": {"open_id": BOT}, "name": "SCV"},
            {"key": "@_user_2", "id": {"open_id": "ou_amy"}, "name": "Amy"},
        ]);
        let payload = event(
            "group",
            "text",
            json!({"text": "@_user_1 ask @_user_2 please"}),
            mentions,
        );
        assert_eq!(
            text(parse_event(&payload, Some(BOT))),
            Some(("ask @Amy please".into(), Some("oc_1".into())))
        );
        let unmentioned = event("group", "text", json!({"text": "hi all"}), json!([]));
        assert!(matches!(
            parse_event(&unmentioned, Some(BOT)),
            Some(Event::Message(Received {
                inbound: Inbound::Ignored { .. },
                ..
            }))
        ));
        // Without the bot's own ID no group message is answered.
        assert_eq!(text(parse_event(&payload, None)), None);
    }

    fn answer(event: Option<Event>) -> Message {
        match event {
            Some(Event::Message(Received {
                inbound: Inbound::Text(message),
                ..
            })) => message,
            _ => panic!("expected a message"),
        }
    }

    fn resource(media: &Media) -> Resource {
        serde_json::from_str(&media.source).unwrap()
    }

    #[test]
    fn rich_text_is_flattened_and_its_images_are_fetched() {
        let post = json!({"title": "Plan", "content": [
            [{"tag": "text", "text": "step "}, {"tag": "a", "text": "one", "href": "https://x"}],
            [{"tag": "img", "image_key": "img_1"}, {"tag": "text", "text": "two"}],
            [{"tag": "media", "file_key": "file_1", "image_key": "img_cover"}],
        ]});
        let message = answer(parse_event(
            &event("p2p", "post", post, json!([])),
            Some(BOT),
        ));
        assert_eq!(message.text, "Plan\nstep one\ntwo");
        assert_eq!(message.media.len(), 2);
        assert_eq!(message.media[0].kind, MediaKind::Image);
        assert_eq!(
            resource(&message.media[0]),
            Resource {
                message_id: "om_1".into(),
                key: "img_1".into(),
                kind: "image".into()
            }
        );
        assert_eq!(message.media[1].kind, MediaKind::Video);
        assert_eq!(resource(&message.media[1]).kind, "file");
    }

    #[test]
    fn files_become_media_to_fetch() {
        let cases = [
            (
                "image",
                json!({"image_key": "img_k"}),
                MediaKind::Image,
                "",
                "image",
                None,
            ),
            (
                "file",
                json!({"file_key": "file_k", "file_name": "report.pdf"}),
                MediaKind::File,
                "report.pdf",
                "file",
                None,
            ),
            (
                "audio",
                json!({"file_key": "file_k", "duration": 3000}),
                MediaKind::Audio,
                "",
                "file",
                Some("audio/opus"),
            ),
            (
                "media",
                json!({"file_key": "file_k", "image_key": "img_c", "file_name": "clip.mp4"}),
                MediaKind::Video,
                "clip.mp4",
                "file",
                None,
            ),
        ];
        for (kind, content, expected, name, served_as, mime) in cases {
            let message = answer(parse_event(
                &event("p2p", kind, content, json!([])),
                Some(BOT),
            ));
            assert_eq!(message.text, "", "{kind}");
            assert_eq!(message.media.len(), 1, "{kind}");
            let media = &message.media[0];
            assert_eq!(media.kind, expected, "{kind}");
            assert_eq!(media.name, name, "{kind}");
            assert_eq!(media.mime.as_deref(), mime, "{kind}");
            assert_eq!(resource(media).kind, served_as, "{kind}");
            assert_eq!(resource(media).message_id, "om_1", "{kind}");
        }
        // A file message without a key has nothing to answer.
        assert!(matches!(
            parse_event(&event("p2p", "image", json!({}), json!([])), Some(BOT)),
            Some(Event::Message(Received {
                inbound: Inbound::Ignored { .. },
                ..
            }))
        ));
    }

    #[test]
    fn content_without_files_becomes_readable_markers() {
        let cases = [
            ("sticker", json!({"file_key": "k"}), "[sticker]"),
            (
                "share_chat",
                json!({"chat_id": "oc_x"}),
                "[shared a group chat]",
            ),
            (
                "share_user",
                json!({"user_id": "ou_x"}),
                "[shared a contact card]",
            ),
            (
                "location",
                json!({"name": "Office", "latitude": "31.2", "longitude": "121.5"}),
                "[location: Office (31.2, 121.5)]",
            ),
            (
                "interactive",
                json!({"title": "Deploy", "elements": [[{"tag": "text", "text": "Approved"}]]}),
                "[card] Deploy\nApproved",
            ),
            ("vote", json!({}), "[vote message]"),
        ];
        for (kind, content, expected) in cases {
            let message = answer(parse_event(
                &event("p2p", kind, content, json!([])),
                Some(BOT),
            ));
            assert_eq!(message.text, expected, "{kind}");
            assert!(message.media.is_empty(), "{kind}");
        }
        let system = event("p2p", "system", json!({"template": "x"}), json!([]));
        assert!(matches!(
            parse_event(&system, Some(BOT)),
            Some(Event::Message(Received {
                inbound: Inbound::Ignored { .. },
                ..
            }))
        ));
    }

    #[test]
    fn quotes_and_forwarded_bundles_are_resolved_before_the_turn() {
        let mut quoted: Value =
            serde_json::from_slice(&event("p2p", "text", json!({"text": "why?"}), json!([])))
                .unwrap();
        quoted["event"]["message"]["parent_id"] = json!("om_parent");
        let message = answer(parse_event(quoted.to_string().as_bytes(), Some(BOT)));
        assert_eq!(message.text, "why?");
        let reference: Reference =
            serde_json::from_str(message.reference.as_deref().unwrap()).unwrap();
        assert_eq!(reference.parent.as_deref(), Some("om_parent"));
        assert!(!reference.forward);

        let forward = answer(parse_event(
            &event(
                "p2p",
                "merge_forward",
                json!({"content": "Merged"}),
                json!([]),
            ),
            Some(BOT),
        ));
        assert_eq!(forward.text, "[Forwarded messages]");
        let reference: Reference =
            serde_json::from_str(forward.reference.as_deref().unwrap()).unwrap();
        assert!(reference.forward);
        assert_eq!(reference.parent, None);

        let plain = answer(parse_event(
            &event("p2p", "text", json!({"text": "hi"}), json!([])),
            Some(BOT),
        ));
        assert_eq!(plain.reference, None);
    }

    #[test]
    fn other_events_and_malformed_payloads_are_told_apart() {
        let read = json!({"header": {"event_type": "im.message.message_read_v1"}, "event": {}});
        assert!(matches!(
            parse_event(read.to_string().as_bytes(), Some(BOT)),
            Some(Event::Other)
        ));
        assert!(parse_event(b"not json", Some(BOT)).is_none());
        let no_id =
            json!({"header": {"event_type": "im.message.receive_v1"}, "event": {"message": {}}});
        assert!(parse_event(no_id.to_string().as_bytes(), Some(BOT)).is_none());
    }

    #[test]
    fn history_items_skip_the_bot_and_deleted_messages() {
        let user = json!({
            "message_id": "om_2", "chat_id": "oc_1", "msg_type": "text",
            "create_time": "1790221500000",
            "sender": {"id": "ou_user", "id_type": "open_id", "sender_type": "user"},
            "body": {"content": "{\"text\":\"while offline\"}"},
        });
        let received = parse_history(&user, false, Some(BOT)).unwrap();
        assert_eq!(received.created_ms, 1790221500000);
        assert!(
            matches!(&received.inbound, Inbound::Text(m) if m.text == "while offline" && m.sender == "ou_user")
        );
        let mut bot = user.clone();
        bot["sender"] = json!({"id": "cli_x", "id_type": "app_id", "sender_type": "app"});
        assert!(parse_history(&bot, false, Some(BOT)).is_none());
        let mut deleted = user.clone();
        deleted["deleted"] = json!(true);
        assert!(parse_history(&deleted, false, Some(BOT)).is_none());
    }

    #[test]
    fn checkpoint_keeps_the_newest_time_per_chat_within_bounds() {
        let mut checkpoint = Checkpoint::parse("");
        for index in 0..(MAX_CHATS + 5) {
            checkpoint.observe(&Received {
                inbound: Inbound::Ignored { id: "m".into() },
                chat_id: format!("oc_{index}"),
                group: false,
                created_ms: 1000 + index as u64,
            });
        }
        assert_eq!(checkpoint.chats.len(), MAX_CHATS);
        assert!(!checkpoint.chats.contains_key("oc_0"));
        let reparsed = Checkpoint::parse(&checkpoint.to_json());
        assert!(reparsed == checkpoint);
        // Older messages never move a chat's mark back.
        let before = checkpoint.chats["oc_10"].last_ms;
        checkpoint.observe(&Received {
            inbound: Inbound::Ignored { id: "m".into() },
            chat_id: "oc_10".into(),
            group: false,
            created_ms: 1,
        });
        assert_eq!(checkpoint.chats["oc_10"].last_ms, before);
        assert!(Checkpoint::parse("garbage").chats.is_empty());
    }
}
