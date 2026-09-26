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
fn only_the_owners_direct_chat_is_logged() {
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
    assert!(log.logs("owner@im.wechat"));
    assert!(!log.logs("someone"));
    assert!(!log.logs("group\0owner@im.wechat"));
    let reference = log.reference();
    assert_eq!(reference.channel, "wechat");
    assert_eq!(
        reference.conversation,
        crate::conversation_dir("owner@im.wechat")
    );
    log.system("SCV restarted.");
    log.end();
    let dir = home
        .path()
        .join("wechat/default")
        .join(&reference.conversation);
    let (episodes, _) = history::episodes(&dir, None, None, 5).unwrap();
    assert_eq!(episodes.len(), 1);
    assert!(episodes[0].ended);
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
