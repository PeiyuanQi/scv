//! Unit tests for `src/chatlog.rs`.

use super::*;

#[test]
fn the_host_offset_is_a_real_zone_offset() {
    let (ms, local) = local_now();
    assert!(ms > 1_700_000_000_000);
    let stamp = local.stamp();
    // `2026-09-26 14:04:05 -07:00`: an offset within a day, in whole minutes.
    assert_eq!(stamp.len(), 26, "{stamp}");
    let offset = utc_offset(ms as i64 / 1000);
    assert!(offset.abs() < 24 * 3600 && offset % 60 == 0, "{offset}");
}

#[test]
fn only_the_owners_direct_chat_and_its_threads_are_logged() {
    let home = tempfile::tempdir().unwrap();
    let log = ChatLog::new(
        LogOptions {
            root: home.path().join("wechat/default"),
            channel: "wechat",
            account: "default".into(),
            gap: Duration::from_secs(7200),
        },
        "owner@im.wechat",
    )
    .unwrap();
    let owner = "owner@im.wechat";
    let thread = format!("{owner}\0thread\0omt_1");
    assert!(log.conversation(owner, owner).is_some());
    assert!(log.conversation(&thread, owner).is_some());
    assert!(log.conversation("someone", "someone").is_none());
    // The owner's messages in a group, and in a thread there, are not logged.
    assert!(log.conversation("group\0owner@im.wechat", owner).is_none());
    let group_thread = "group\0owner@im.wechat\0thread\0omt_2";
    assert!(log.conversation(group_thread, owner).is_none());
    // A sender is checked, not just the key: someone whose ID merely starts
    // like the owner's thread is not the owner.
    assert!(log.conversation(&thread, &thread).is_none());

    let direct = log.conversation(owner, owner).unwrap();
    let reference = direct.reference();
    assert_eq!(reference.channel, "wechat");
    assert_eq!(reference.conversation, crate::conversation_dir(owner));
    direct.system("SCV restarted.");
    direct.end();
    // A thread is a conversation of its own, in a directory of its own.
    let threaded = log.conversation(&thread, owner).unwrap();
    assert_eq!(
        threaded.reference().conversation,
        crate::conversation_dir(&thread)
    );
    threaded.system("In the thread.");
    let episodes = |conversation: &str| {
        let dir = home.path().join("wechat/default").join(conversation);
        history::episodes(&dir, None, None, 5).unwrap().0
    };
    let direct_episodes = episodes(&reference.conversation);
    assert_eq!(direct_episodes.len(), 1);
    assert!(direct_episodes[0].ended);
    let thread_episodes = episodes(&crate::conversation_dir(&thread));
    assert_eq!(thread_episodes.len(), 1);
    assert!(!thread_episodes[0].ended);
}

#[test]
fn a_log_remembers_a_bounded_number_of_conversations() {
    let home = tempfile::tempdir().unwrap();
    let log = ChatLog::new(LogOptions::test(home.path(), "feishu"), "ou_owner").unwrap();
    log.conversation("ou_owner", "ou_owner")
        .unwrap()
        .system("direct");
    for index in 0..(MAX_REMEMBERED + 3) {
        let key = format!("ou_owner\0thread\0omt_{index}");
        log.conversation(&key, "ou_owner").unwrap().system("thread");
    }
    assert!(log.logs.lock().unwrap().len() <= MAX_REMEMBERED);
    assert!(log.logs.lock().unwrap().contains_key("ou_owner"));
    // A forgotten conversation finds its open episode again.
    let first = "ou_owner\0thread\0omt_0";
    log.conversation(first, "ou_owner").unwrap().system("again");
    let dir = home
        .path()
        .join("history/feishu/default")
        .join(crate::conversation_dir(first));
    let (episodes, _) = history::episodes(&dir, None, None, 5).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].messages, 2);
}

#[test]
fn an_account_name_too_long_for_session_start_is_not_logged() {
    let home = tempfile::tempdir().unwrap();
    let account = "a".repeat(65);
    let options = LogOptions {
        root: home.path().join("wechat").join(&account),
        channel: "wechat",
        account,
        gap: Duration::from_secs(7200),
    };
    assert!(ChatLog::new(options, "owner").is_none());
}

#[test]
fn voice_transcripts_are_recorded_with_their_files() {
    let attachment = scv_protocol::Attachment {
        kind: "audio".into(),
        path: "/m/voice.silk".into(),
        name: "voice.silk".into(),
        mime: "audio/silk".into(),
        size: 3,
        transcript: Some("call me back".into()),
    };
    assert_eq!(received(&[attachment])[0].transcript, "call me back");
    let media = crate::Media {
        kind: crate::MediaKind::Audio,
        name: "voice.silk".into(),
        size: None,
        mime: None,
        transcript: Some("call me back".into()),
        source: "x".into(),
    };
    assert_eq!(announced(&[media])[0].transcript, "call me back");
}
