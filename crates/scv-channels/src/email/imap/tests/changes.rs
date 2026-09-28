//! Change tests for `src/email/imap/mod.rs`: the first run, new mail,
//! limits, and resets, against the scripted server.

use super::*;

/// 2026-09-28 12:00:00 UTC.
const NOW: u64 = 1790596800;
const THREE_DAYS: u64 = 3 * 86_400;

#[tokio::test]
async fn the_first_run_lists_nothing_and_starts_at_uidnext() {
    let (mut source, fake) = open(session(vec![examine(7, Some(10))])).await;
    let changes = source.changes(None, 50, 86_400).await.unwrap();
    assert_eq!(
        changes,
        Changes::New {
            refs: vec![],
            next: cursor("INBOX", 7, 10),
        }
    );
    let received = finish(source, fake).await;
    assert_eq!(received.last().unwrap(), "A0004 EXAMINE \"INBOX\"");
}

#[tokio::test]
async fn the_first_run_without_uidnext_starts_after_the_highest_uid() {
    let (mut source, fake) = open(session(vec![
        examine(7, None),
        command("UID SEARCH ALL", "* SEARCH 3 8 5\r\n{tag} OK\r\n"),
        examine(7, None),
        command("UID SEARCH ALL", "* SEARCH\r\n{tag} OK\r\n"),
    ]))
    .await;
    let Changes::New { refs, next } = source.changes(None, 50, 86_400).await.unwrap() else {
        panic!("not new");
    };
    assert!(refs.is_empty());
    assert_eq!(next, cursor("INBOX", 7, 9));
    let Changes::New { next, .. } = source.changes(None, 50, 86_400).await.unwrap() else {
        panic!("not new");
    };
    assert_eq!(next, cursor("INBOX", 7, 1));
    finish(source, fake).await;
}

#[tokio::test]
async fn new_mail_is_listed_oldest_first_from_the_cursor() {
    let (mut source, fake) = open(session(vec![
        examine(7, Some(13)),
        command("UID SEARCH UID 10:*", "* SEARCH 12 10 11\r\n{tag} OK\r\n"),
    ]))
    .await;
    let changes = source
        .changes(Some(&cursor("INBOX", 7, 10)), 50, 86_400)
        .await
        .unwrap();
    assert_eq!(
        changes,
        Changes::New {
            refs: vec![inbox(10), inbox(11), inbox(12)],
            next: cursor("INBOX", 7, 13),
        }
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn the_star_quirk_lists_nothing_and_moves_to_uidnext() {
    // UID 10 came and went: `10:*` matches the highest UID, 9.
    let (mut source, fake) = open(session(vec![
        examine(7, Some(11)),
        command("UID SEARCH UID 10:*", "* SEARCH 9\r\n{tag} OK\r\n"),
    ]))
    .await;
    let changes = source
        .changes(Some(&cursor("INBOX", 7, 10)), 50, 86_400)
        .await
        .unwrap();
    assert_eq!(
        changes,
        Changes::New {
            refs: vec![],
            next: cursor("INBOX", 7, 11),
        }
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn nothing_is_searched_while_uidnext_has_not_passed_the_cursor() {
    let (mut source, fake) = open(session(vec![examine(7, Some(10)), examine(7, Some(10))])).await;
    for next in [10, 12] {
        let changes = source
            .changes(Some(&cursor("INBOX", 7, next)), 50, 86_400)
            .await
            .unwrap();
        assert_eq!(
            changes,
            Changes::New {
                refs: vec![],
                next: cursor("INBOX", 7, next),
            }
        );
    }
    finish(source, fake).await;
}

#[tokio::test]
async fn a_limit_truncates_and_the_cursor_resumes_after_the_last_listed() {
    let (mut source, fake) = open(session(vec![
        examine(7, Some(20)),
        command(
            "UID SEARCH UID 5:*",
            "* SEARCH 5 6 7 8 9 10 11 12\r\n{tag} OK\r\n",
        ),
        examine(7, Some(20)),
        command(
            "UID SEARCH UID 8:*",
            "* SEARCH 8 9 10 11 12\r\n{tag} OK\r\n",
        ),
        examine(7, Some(20)),
        command("UID SEARCH UID 11:*", "* SEARCH 11 12\r\n{tag} OK\r\n"),
    ]))
    .await;
    let Changes::New { refs, next } = source
        .changes(Some(&cursor("INBOX", 7, 5)), 3, 86_400)
        .await
        .unwrap()
    else {
        panic!("not new");
    };
    assert_eq!(refs, [inbox(5), inbox(6), inbox(7)]);
    assert_eq!(next, cursor("INBOX", 7, 8));
    let Changes::New { refs, next } = source.changes(Some(&next), 3, 86_400).await.unwrap() else {
        panic!("not new");
    };
    assert_eq!(refs, [inbox(8), inbox(9), inbox(10)]);
    assert_eq!(next, cursor("INBOX", 7, 11));
    // The rest fits: the cursor moves on to UIDNEXT.
    let Changes::New { refs, next } = source.changes(Some(&next), 3, 86_400).await.unwrap() else {
        panic!("not new");
    };
    assert_eq!(refs, [inbox(11), inbox(12)]);
    assert_eq!(next, cursor("INBOX", 7, 20));
    finish(source, fake).await;
}

#[tokio::test]
async fn a_zero_limit_keeps_the_cursor() {
    let (mut source, fake) = open(session(vec![
        examine(7, Some(20)),
        command("UID SEARCH UID 5:*", "* SEARCH 5 6\r\n{tag} OK\r\n"),
    ]))
    .await;
    let changes = source
        .changes(Some(&cursor("INBOX", 7, 5)), 0, 86_400)
        .await
        .unwrap();
    assert_eq!(
        changes,
        Changes::New {
            refs: vec![],
            next: cursor("INBOX", 7, 5),
        }
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn a_cursor_at_zero_resumes_from_one() {
    let (mut source, fake) = open(session(vec![
        examine(7, Some(3)),
        command("UID SEARCH UID 1:*", "* SEARCH 1 2\r\n{tag} OK\r\n"),
    ]))
    .await;
    let Changes::New { refs, .. } = source
        .changes(Some(&cursor("INBOX", 7, 0)), 10, 86_400)
        .await
        .unwrap()
    else {
        panic!("not new");
    };
    assert_eq!(refs, [inbox(1), inbox(2)]);
    finish(source, fake).await;
}

#[tokio::test]
async fn a_new_uidvalidity_resets_to_the_newest_in_the_window() {
    let (mut source, fake) = open(session(vec![
        examine(7, Some(30)),
        command(
            "UID SEARCH SINCE 25-Sep-2026",
            "* SEARCH 23 20 22 21\r\n{tag} OK\r\n",
        ),
    ]))
    .await;
    let changes = source
        .changes_at(Some(&cursor("INBOX", 6, 400)), 2, THREE_DAYS, NOW)
        .await
        .unwrap();
    assert_eq!(
        changes,
        Changes::Reset {
            recent: vec![inbox(22), inbox(23)],
            beyond: 2,
            next: cursor("INBOX", 7, 30),
        }
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn another_mailbox_or_an_unreadable_cursor_resets() {
    let unreadable = [
        cursor("Archive", 7, 5),
        Cursor {
            provider: ProviderKind::Imap,
            value: "not json".to_owned(),
        },
        Cursor {
            provider: ProviderKind::Imap,
            value: r#"{"mailbox":"INBOX","uidvalidity":7}"#.to_owned(),
        },
    ];
    let mut script = Vec::new();
    for _ in &unreadable {
        script.push(examine(7, Some(10)));
        script.push(command(
            "UID SEARCH SINCE 25-Sep-2026",
            "* SEARCH 8 9\r\n{tag} OK\r\n",
        ));
    }
    let (mut source, fake) = open(session(script)).await;
    for cursor_value in &unreadable {
        let changes = source
            .changes_at(Some(cursor_value), 10, THREE_DAYS, NOW)
            .await
            .unwrap();
        assert_eq!(
            changes,
            Changes::Reset {
                recent: vec![inbox(8), inbox(9)],
                beyond: 0,
                next: cursor("INBOX", 7, 10),
            },
            "{cursor_value:?}"
        );
    }
    finish(source, fake).await;
}

#[tokio::test]
async fn a_reset_without_uidnext_resumes_after_the_highest_uid() {
    let (mut source, fake) = open(session(vec![
        examine(7, None),
        command(
            "UID SEARCH SINCE 25-Sep-2026",
            "* SEARCH 4 6\r\n{tag} OK\r\n",
        ),
        command("UID SEARCH ALL", "* SEARCH 1 2 4 6 9\r\n{tag} OK\r\n"),
    ]))
    .await;
    let changes = source
        .changes_at(Some(&cursor("INBOX", 1, 3)), 10, THREE_DAYS, NOW)
        .await
        .unwrap();
    assert_eq!(
        changes,
        Changes::Reset {
            recent: vec![inbox(4), inbox(6)],
            beyond: 0,
            next: cursor("INBOX", 7, 10),
        }
    );
    finish(source, fake).await;
}
