//! Unit tests for `src/feishu/inbound.rs`.

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
        serde_json::from_slice(&event("p2p", "text", json!({"text": "why?"}), json!([]))).unwrap();
    quoted["event"]["message"]["parent_id"] = json!("om_parent");
    let message = answer(parse_event(quoted.to_string().as_bytes(), Some(BOT)));
    assert_eq!(message.text, "why?");
    let reference: Reference = serde_json::from_str(message.reference.as_deref().unwrap()).unwrap();
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
    let reference: Reference = serde_json::from_str(forward.reference.as_deref().unwrap()).unwrap();
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

#[test]
fn a_voice_message_gets_the_voice_reply_because_feishu_sends_no_transcript() {
    use crate::intake::{Intake, Verdict, classify};
    let parsed = parse_event(
        &event(
            "p2p",
            "audio",
            json!({"file_key": "file_v3_voice", "duration": 2000}),
            json!([]),
        ),
        Some(BOT),
    );
    let Some(Event::Message(received)) = parsed else {
        panic!("expected a message")
    };
    let state = crate::state::BridgeState::default();
    let conversations = std::collections::HashMap::new();
    let intake = Intake {
        state: &state,
        conversations: &conversations,
        owner: Some("ou_user"),
        tool_owner: None,
        senders: crate::state::Senders::Owner,
        question: false,
    };
    let Verdict::Unheard(sender) = classify(&received.inbound, &intake) else {
        panic!("a Feishu voice message gets the voice reply");
    };
    assert_eq!(sender.message.reply_to, "om_1");
    assert!(sender.owner_chat);
}
