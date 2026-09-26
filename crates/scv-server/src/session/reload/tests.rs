//! Unit tests for `src/session/reload.rs`.

use super::*;
use scv_client::history::{FileRef, LocalTime, Log};

const GAP: Duration = Duration::from_secs(7200);
const AT: i64 = 1_790_456_645;

fn entry(role: Role, text: &str) -> Entry {
    Entry {
        at: AT as u64 * 1000,
        role,
        text: text.into(),
        ..Entry::default()
    }
}

fn text(message: &Message) -> &str {
    match message {
        Message::User { content, .. } | Message::Assistant { content, .. } => content,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn the_open_episode_comes_back_in_order_with_roles() {
    let home = tempfile::tempdir().unwrap();
    let mut log = Log::new(home.path().to_path_buf(), GAP);
    let local = LocalTime::at(AT, 0);
    let mut photo = entry(Role::Owner, "what is this?");
    photo.quote = "an earlier message".into();
    photo.files.push(FileRef {
        kind: "image".into(),
        name: "cat.jpg".into(),
        path: "/m/cat.jpg".into(),
        ..FileRef::default()
    });
    photo
        .notes
        .push("[file a.zip: not opened for this sender]".into());
    log.append(photo, &local).unwrap();
    log.append(entry(Role::Scv, "A cat."), &local).unwrap();
    let mut report = entry(Role::Scv, "The build finished.");
    report.report = true;
    log.append(report, &local).unwrap();
    log.append(entry(Role::System, "SCV restarted into 0.3.2."), &local)
        .unwrap();
    let history = reload(home.path(), GAP, AT as u64 * 1000 + 60_000, true);
    let texts: Vec<&str> = history.iter().map(text).collect();
    assert!(
        texts[0].starts_with("[SCV started a new session"),
        "{}",
        texts[0]
    );
    assert!(!texts[0].contains("left out"));
    assert_eq!(
        texts[1],
        "an earlier message\n\nwhat is this?\n\n[file a.zip: not opened for this sender]\n\n\
         [The user attached: image cat.jpg]"
    );
    assert!(matches!(history[2], Message::Assistant { .. }));
    assert_eq!(texts[2], "A cat.");
    assert!(texts[3].starts_with("[SCV background report"));
    assert_eq!(texts[4], "The build finished.");
    assert_eq!(
        texts[5],
        "[SCV sent the user this message itself]\nSCV restarted into 0.3.2."
    );
    // After the gap nothing comes back.
    assert!(reload(home.path(), GAP, (AT as u64 + 7200) * 1000, true).is_empty());
    assert!(reload(&home.path().join("missing"), GAP, 0, true).is_empty());
}

#[test]
fn a_long_episode_keeps_its_newest_messages() {
    let entries: Vec<Entry> = (0..100)
        .map(|index| entry(Role::Owner, &format!("{index:04}{}", "x".repeat(2000))))
        .collect();
    let history = messages(&entries, true);
    let first = text(&history[0]);
    assert!(
        first.contains("earlier messages are left out; chat_history can read them"),
        "{first}"
    );
    assert!(text(history.last().unwrap()).starts_with("0099"));
    let kept = history.len() - 1;
    assert!(first.contains(&format!("{} earlier", 100 - kept)));
    assert!(kept * 2004 <= MAX_RELOAD_BYTES);
    // A tool-free session is not pointed at a tool it lacks.
    assert!(text(&messages(&entries, false)[0]).ends_with("left out.]"));
    // One huge message still comes back, cut.
    let huge = messages(&[entry(Role::Scv, &"y".repeat(200_000))], true);
    assert!(text(&huge[1]).ends_with("\n[cut]"));
    assert!(text(&huge[1]).len() <= MAX_MESSAGE_BYTES + 6);
}
