use super::*;
use crate::slack::tests::{account, event};

fn message(received: Option<Received>) -> Message {
    match received.expect("a message").inbound {
        Inbound::Text(message) => message,
        Inbound::Ignored { .. } => panic!("ignored"),
    }
}

#[test]
fn direct_group_and_thread_routing_preserve_authority() {
    let account = account();
    let direct = message(parse_event(&event("D123"), &account));
    assert!(direct.group.is_none());
    assert_eq!(direct.sent_ms, Some(1_700_000_000_000));
    assert_eq!(
        target(&direct.reply_to, &direct.sender).unwrap(),
        ("D123", None)
    );
    // A channel is answered only through a mention of the bot.
    let mut group = event("C123");
    assert!(parse_event(&group, &account).is_none());
    group["event"]["type"] = "app_mention".into();
    group["event"]["text"] = "<@UBOT> hello".into();
    assert!(parse_event(&group, &account).is_some());
    group["event"]["text"] = "<@UOTHER> hello".into();
    assert!(parse_event(&group, &account).is_none());
    group["event"]["text"] = "<@UBOT> hello &lt;b&gt; &amp;amp;".into();
    group["event"]["thread_ts"] = "1700000000.000000".into();
    let threaded = message(parse_event(&group, &account));
    assert_eq!(threaded.text, "hello <b> &amp;");
    assert_eq!(threaded.group.as_deref(), Some("C123"));
    let thread = threaded.thread.unwrap();
    assert_eq!(thread.id, "C123:1700000000.000000");
    assert_eq!(threaded.reply_to, thread.reply_to);
    assert_eq!(
        target(&thread.reply_to, "U123").unwrap(),
        ("C123", Some("1700000000.000000"))
    );
    let origin: Reference = serde_json::from_str(thread.origin.as_deref().unwrap()).unwrap();
    assert_eq!(origin.channel.as_deref(), Some("C123"));
    assert_eq!(origin.root.as_deref(), Some("1700000000.000000"));
    // A mention event from a direct conversation never counts as one.
    let mut mention = event("D123");
    mention["event"]["type"] = "app_mention".into();
    assert!(parse_event(&mention, &account).is_none());
}

#[test]
fn a_threads_reply_also_sent_to_its_conversation_stays_in_the_thread() {
    let mut value = event("D123");
    value["event"]["subtype"] = "thread_broadcast".into();
    value["event"]["thread_ts"] = "1699999999.000000".into();
    let parsed = message(parse_event(&value, &account()));
    assert_eq!(parsed.thread.unwrap().id, "D123:1699999999.000000");
}

#[test]
fn a_threads_root_is_in_the_conversation_itself() {
    let mut root = event("D123");
    root["event"]["thread_ts"] = root["event"]["ts"].clone();
    let root = message(parse_event(&root, &account()));
    assert!(root.thread.is_none());
    assert_eq!(root.reply_to, "slack:D123");
}

#[test]
fn foreign_installations_bots_edits_and_malformed_events_are_ignored() {
    let account = account();
    for (pointer, value) in [
        ("/team_id", json!("TOTHER")),
        ("/api_app_id", json!("AOTHER")),
        ("/event/bot_id", json!("B1")),
        ("/event/app_id", json!("A1")),
        ("/event/user", json!("UBOT")),
        ("/event/user", json!("display name")),
        ("/event/subtype", json!("message_changed")),
        ("/event/ts", json!("1..2")),
        ("/event/thread_ts", json!("bad")),
        ("/event/channel", json!("X123")),
        ("/event/channel_type", json!("mpim")),
        ("/event/text", json!("  ")),
    ] {
        let mut value_event = event("D123");
        // bot_id, app_id, subtype, and thread_ts are absent in the base.
        if let Some(key) = pointer.strip_prefix("/event/") {
            value_event["event"][key] = value;
        } else {
            *value_event.pointer_mut(pointer).unwrap() = value;
        }
        assert!(parse_event(&value_event, &account).is_none(), "{pointer}");
    }
    assert!(target("slack:C123/thread:bad", "U123").is_err());
    assert!(target("bad", "U123").is_err());
    assert!(target("", "not-a-user").is_err());
    assert_eq!(target("", "U123").unwrap(), ("U123", None));
}

#[test]
fn files_become_media_or_markers() {
    let mut value = event("D123");
    value["event"]["subtype"] = "file_share".into();
    value["event"]["text"] = "".into();
    value["event"]["files"] = json!([
        {"name": "cat.png", "mimetype": "image/png", "size": 12,
         "url_private_download": "https://files.slack.com/files-pri/T1-F1/download/cat.png",
         "url_private": "https://files.slack.com/files-pri/T1-F1/cat.png"},
        {"name": "clip.webm", "mimetype": "audio/webm", "subtype": "slack_audio",
         "url_private": "https://files.slack.com/files-pri/T1-F2/clip.webm",
         "transcription": {"status": "complete", "preview": {"content": " call me "}}},
        {"title": "notes", "mimetype": "text/plain",
         "url_private": "https://files.slack.com/files-pri/T1-F3/notes"},
        {"name": "drive.doc", "mode": "external", "url_private": "https://docs.example/x"},
        {"name": "gone.pdf", "mimetype": "application/pdf", "mode": "tombstone"},
    ]);
    let parsed = message(parse_event(&value, &account()));
    assert_eq!(
        parsed.text,
        "[file drive.doc: Slack offers no download of it]\n\
         [file gone.pdf: Slack offers no download of it]"
    );
    let kinds: Vec<_> = parsed.media.iter().map(|media| media.kind).collect();
    assert_eq!(kinds, [MediaKind::Image, MediaKind::Audio, MediaKind::File]);
    let image = &parsed.media[0];
    assert_eq!(
        (image.name.as_str(), image.size, image.mime.as_deref()),
        ("cat.png", Some(12), Some("image/png"))
    );
    let source: Value = serde_json::from_str(&image.source).unwrap();
    assert_eq!(
        source["url"],
        "https://files.slack.com/files-pri/T1-F1/download/cat.png"
    );
    assert_eq!(parsed.media[1].transcript.as_deref(), Some("call me"));
    assert_eq!(parsed.media[2].name, "notes");
}

#[test]
fn shared_messages_become_a_quote_and_mark_the_message_quoted() {
    let mut value = event("D123");
    value["event"]["text"] = "".into();
    value["event"]["attachments"] = json!([
        {"is_share": true, "author_name": "Ada", "text": "ship it &amp; tell me"},
        {"is_msg_unfurl": true, "fallback": "second"},
        {"title": "a link preview", "text": "not a quote"},
    ]);
    let parsed = message(parse_event(&value, &account()));
    assert!(parsed.quoted);
    let reference: Reference = serde_json::from_str(parsed.reference.as_deref().unwrap()).unwrap();
    assert_eq!(
        reference.quote.as_deref(),
        Some("Ada: ship it & tell me\nsecond")
    );
    // An unfurl alone is no quote, but still keeps a mail chat from reading
    // the message as a command.
    value["event"]["text"] = "look".into();
    value["event"]["attachments"] = json!([{"title": "preview"}]);
    let parsed = message(parse_event(&value, &account()));
    assert!(parsed.quoted && parsed.reference.is_none());
}

#[test]
fn history_items_take_their_conversation_from_the_listing() {
    let account = account();
    let item = json!({"type": "message", "user": "U123", "text": "hi", "ts": "1700000000.000005"});
    let received = parse_history(&item, "D123", false, &account).unwrap();
    assert_eq!(received.channel, "D123");
    assert_eq!(received.ts, 1_700_000_000_000_005);
    assert!(parse_history(&item, "C123", true, &account).is_none());
    let mut mention = item.clone();
    mention["text"] = "<@UBOT> hi".into();
    let received = parse_history(&mention, "C123", true, &account).unwrap();
    assert!(received.group);
    let mut other = item;
    other["type"] = "channel_join".into();
    assert!(parse_history(&other, "D123", false, &account).is_none());
}

#[test]
fn timestamps_round_trip_in_microseconds() {
    assert_eq!(micros("1700000000.000100"), Some(1_700_000_000_000_100));
    assert_eq!(ts(1_700_000_000_000_100), "1700000000.000100");
    for bad in [
        "",
        "1",
        ".000001",
        "1.1",
        "1.0000001",
        "a.000001",
        "1.00000a",
    ] {
        assert_eq!(micros(bad), None, "{bad}");
    }
}

#[test]
fn checkpoint_marks_conversations_and_threads_apart_and_stays_bounded() {
    let account = account();
    let mut checkpoint = Checkpoint::default();
    let mut threaded = event("D123");
    threaded["event"]["thread_ts"] = "1699999999.000000".into();
    checkpoint.observe(&parse_event(&threaded, &account).unwrap());
    // The thread's message makes its conversation known without moving it
    // past the conversation's own messages.
    assert_eq!(
        checkpoint.threads["D123:1699999999.000000"].last,
        1_700_000_000_000_001
    );
    assert_eq!(checkpoint.chats["D123"].last, 1_700_000_000_000_001);
    checkpoint.listed_chat(
        "D123",
        Mark {
            group: false,
            last: 5,
        },
    );
    assert_eq!(checkpoint.chats["D123"].last, 1_700_000_000_000_001);
    for index in 0..MAX_CHATS + 5 {
        checkpoint.listed_chat(
            &format!("C{index}"),
            Mark {
                group: true,
                last: 1_800_000_000_000_000 + index as u64,
            },
        );
    }
    assert_eq!(checkpoint.chats.len(), MAX_CHATS);
    assert!(!checkpoint.chats.contains_key("D123"));
    assert_eq!(Checkpoint::parse(&checkpoint.to_json()), checkpoint);
    assert_eq!(Checkpoint::parse("{bad"), Checkpoint::default());
}
