//! Turning Feishu messages, from socket events or chat history, into the
//! bridge's inbound messages, and the checkpoint that drives catch-up.

use scv_channels::{Inbound, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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

/// What a socket event carried.
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
    let text = content
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .and_then(|content| match message_type? {
            "text" => content.get("text")?.as_str().map(str::to_owned),
            "post" => post_text(&content),
            _ => None,
        })
        .map(|text| replace_mentions(&text, mentions, bot_open_id))
        .filter(|text| !text.trim().is_empty());
    let Some(text) = text else {
        return Some(ignored());
    };
    Some(Received {
        inbound: Inbound::Text(Message {
            id: id.to_owned(),
            sender: sender.to_owned(),
            text: text.trim().to_owned(),
            reply_to: id.to_owned(),
            group: group.then(|| chat_id.to_owned()),
        }),
        chat_id: chat_id.to_owned(),
        group,
        created_ms,
    })
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

/// The text of a rich-text (`post`) message, one paragraph per line.
fn post_text(content: &Value) -> Option<String> {
    // Events carry the post itself; some payloads wrap it by locale.
    let post = if content.get("content").is_some() {
        content
    } else {
        content
            .as_object()?
            .values()
            .find(|v| v.get("content").is_some())?
    };
    let mut lines = Vec::new();
    if let Some(title) = post.get("title").and_then(Value::as_str)
        && !title.trim().is_empty()
    {
        lines.push(title.to_owned());
    }
    for paragraph in post.get("content")?.as_array()? {
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
                _ => {}
            }
        }
        lines.push(line);
    }
    Some(lines.join("\n"))
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

    #[test]
    fn rich_text_is_flattened_and_other_types_are_only_seen() {
        let post = json!({"title": "Plan", "content": [
            [{"tag": "text", "text": "step "}, {"tag": "a", "text": "one", "href": "https://x"}],
            [{"tag": "img", "image_key": "k"}, {"tag": "text", "text": "two"}],
        ]});
        assert_eq!(
            text(parse_event(
                &event("p2p", "post", post, json!([])),
                Some(BOT)
            )),
            Some(("Plan\nstep one\ntwo".into(), None))
        );
        let image = event("p2p", "image", json!({"image_key": "k"}), json!([]));
        assert!(matches!(
            parse_event(&image, Some(BOT)),
            Some(Event::Message(Received {
                inbound: Inbound::Ignored { .. },
                ..
            }))
        ));
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
