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
    // The message's own creation time is when the sender sent it.
    assert_eq!(message.sent_ms, Some(1790221416232));
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
    // A caught-up message keeps when it was sent, not when it arrived.
    assert!(matches!(&received.inbound, Inbound::Text(m) if m.sent_ms == Some(1790221500000)));
    let mut untimed = user.clone();
    untimed.as_object_mut().unwrap().remove("create_time");
    assert!(matches!(
        parse_history(&untimed, false, Some(BOT)).unwrap().inbound,
        Inbound::Text(Message { sent_ms: None, .. })
    ));
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
            thread_id: None,
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
        thread_id: None,
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
        question: None,
    };
    let Verdict::Unheard(sender) = classify(&received.inbound, &intake) else {
        panic!("a Feishu voice message gets the voice reply");
    };
    assert_eq!(sender.message.reply_to, "om_1");
    assert!(sender.owner_chat);
}

/// `payload`'s message with `fields` set, as Feishu marks replies and
/// thread messages.
fn with(payload: &[u8], fields: Value) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(payload).unwrap();
    for (key, field) in fields.as_object().unwrap() {
        value["event"]["message"][key] = field.clone();
    }
    value.to_string().into_bytes()
}

#[test]
fn a_thread_message_belongs_to_its_thread_and_is_answered_inside_it() {
    // Inside a thread both `root_id` and `parent_id` point at its root.
    let payload = with(
        &event("p2p", "text", json!({"text": "and then?"}), json!([])),
        json!({"thread_id": "omt_1", "root_id": "om_root", "parent_id": "om_root"}),
    );
    let Some(Event::Message(received)) = parse_event(&payload, Some(BOT)) else {
        panic!("expected a message")
    };
    assert_eq!(received.thread_id.as_deref(), Some("omt_1"));
    let Inbound::Text(message) = received.inbound else {
        panic!("expected text")
    };
    assert_eq!(message.reply_to, "thread:om_1");
    // The root is what the thread is on, not a quote.
    assert_eq!(message.reference, None);
    assert!(!message.quoted);
    let thread = message.thread.unwrap();
    assert_eq!(thread.id, "omt_1");
    assert_eq!(thread.reply_to, "thread:om_root");
    let origin: Reference = serde_json::from_str(thread.origin.as_deref().unwrap()).unwrap();
    assert_eq!(
        origin,
        Reference {
            root: Some("om_root".into()),
            ..Reference::default()
        }
    );
}

#[test]
fn a_reply_outside_a_thread_quotes_and_answers_in_the_chat() {
    // An ordinary reply chain names its parent and root but no thread.
    let payload = with(
        &event("p2p", "text", json!({"text": "why?"}), json!([])),
        json!({"root_id": "om_root", "parent_id": "om_parent"}),
    );
    let message = answer(parse_event(&payload, Some(BOT)));
    assert_eq!(message.thread, None);
    assert_eq!(message.reply_to, "om_1");
    let reference: Reference = serde_json::from_str(message.reference.as_deref().unwrap()).unwrap();
    assert_eq!(reference.parent.as_deref(), Some("om_parent"));
    assert_eq!(reference.root, None);
    assert!(message.quoted);
}

#[test]
fn a_threads_root_starts_it_and_has_no_origin_to_show() {
    // A topic group's post, or a root listed once its thread began.
    let payload = with(
        &event(
            "group",
            "text",
            json!({"text": "@_user_1 topic"}),
            json!([
                {"key": "@_user_1", "id": {"open_id": BOT}, "name": "SCV"},
            ]),
        ),
        json!({"thread_id": "omt_2"}),
    );
    let message = answer(parse_event(&payload, Some(BOT)));
    let thread = message.thread.unwrap();
    assert_eq!(thread.reply_to, "thread:om_1");
    assert_eq!(thread.origin, None);
    assert_eq!(message.group.as_deref(), Some("oc_1"));
    // A thread message in a group that does not mention the bot is only
    // seen, but still marks its thread for catch-up.
    let unmentioned = with(
        &event("group", "text", json!({"text": "hi"}), json!([])),
        json!({"thread_id": "omt_2", "root_id": "om_root", "parent_id": "om_root"}),
    );
    let Some(Event::Message(received)) = parse_event(&unmentioned, Some(BOT)) else {
        panic!("expected a message")
    };
    assert!(matches!(received.inbound, Inbound::Ignored { .. }));
    assert_eq!(received.thread_id.as_deref(), Some("omt_2"));
    // A thread ID Feishu could not have sent is not a thread.
    let bad = with(
        &event("p2p", "text", json!({"text": "hi"}), json!([])),
        json!({"thread_id": "omt\u{0}x"}),
    );
    assert_eq!(answer(parse_event(&bad, Some(BOT))).thread, None);
}

#[test]
fn history_items_keep_their_thread() {
    let item = json!({
        "message_id": "om_2", "chat_id": "oc_1", "msg_type": "text",
        "create_time": "1790221416232", "thread_id": "omt_1",
        "root_id": "om_root", "parent_id": "om_root",
        "sender": {"id": "ou_user", "id_type": "open_id", "sender_type": "user"},
        "body": {"content": json!({"text": "later"}).to_string()},
    });
    let received = parse_history(&item, false, Some(BOT)).unwrap();
    assert_eq!(received.thread_id.as_deref(), Some("omt_1"));
    let Inbound::Text(message) = received.inbound else {
        panic!("expected text")
    };
    assert_eq!(message.reply_to, "thread:om_2");
    assert_eq!(message.thread.unwrap().reply_to, "thread:om_root");
}

#[test]
fn thread_messages_move_their_threads_mark_and_make_the_chat_known() {
    let received = |chat: &str, thread: Option<&str>, created_ms: u64| Received {
        inbound: Inbound::Ignored { id: "m".into() },
        chat_id: chat.into(),
        thread_id: thread.map(str::to_owned),
        group: false,
        created_ms,
    };
    let mut checkpoint = Checkpoint::parse("");
    checkpoint.observe(&received("oc_1", None, 100));
    checkpoint.observe(&received("oc_1", Some("omt_1"), 500));
    // The chat's own mark moves only with its own messages.
    assert_eq!(checkpoint.chats["oc_1"].last_ms, 100);
    assert_eq!(checkpoint.threads["omt_1"].last_ms, 500);
    checkpoint.observe(&received("oc_1", Some("omt_1"), 400));
    assert_eq!(checkpoint.threads["omt_1"].last_ms, 500);
    // A thread in a chat not seen before makes the chat known from then on.
    checkpoint.observe(&received("oc_2", Some("omt_2"), 700));
    assert_eq!(checkpoint.chats["oc_2"].last_ms, 700);
    for index in 0..(MAX_THREADS + 3) {
        checkpoint.observe(&received(
            "oc_1",
            Some(&format!("omt_x{index}")),
            1000 + index as u64,
        ));
    }
    assert_eq!(checkpoint.threads.len(), MAX_THREADS);
    assert!(!checkpoint.threads.contains_key("omt_1"));
    assert!(Checkpoint::parse(&checkpoint.to_json()) == checkpoint);
    // A checkpoint from before threads reads as one without any, and one
    // without threads writes none, as such releases wrote it.
    let old = r#"{"chats":{"oc_1":{"group":false,"last_ms":5}}}"#;
    let parsed = Checkpoint::parse(old);
    assert!(parsed.threads.is_empty());
    assert_eq!(parsed.to_json(), old);
}
