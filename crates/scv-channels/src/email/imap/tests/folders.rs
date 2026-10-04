//! Unit tests for `src/email/imap/mod.rs`.

use super::super::client::COMMAND_TIMEOUT;
use super::*;
use crate::email::source::MailSource;

fn folder(name: &str, attributes: &[&str]) -> Listed {
    Listed {
        name: name.to_owned(),
        attributes: attributes
            .iter()
            .map(|attribute| (*attribute).to_owned())
            .collect(),
    }
}

fn names(drafts: &str, sent: &str, trash: &str, junk: &str, archive: &str) -> FolderNames {
    FolderNames {
        drafts: drafts.to_owned(),
        sent: sent.to_owned(),
        trash: trash.to_owned(),
        junk: junk.to_owned(),
        archive: archive.to_owned(),
    }
}

/// `writable` is only for `XLIST`: the read-only fake treats that verb as a
/// mailbox change, though the guard allows it and it does not change one.
async fn listed_over(
    greeting: &str,
    expect: &str,
    reply: &str,
    writable: bool,
) -> (Vec<Listed>, Vec<String>) {
    let script = vec![command(expect, reply)];
    let (stream, fake) = if writable {
        fake::serve_writable(greeting, script)
    } else {
        fake::serve(greeting, script)
    };
    let mut client = client::Client::start(stream, COMMAND_TIMEOUT)
        .await
        .expect("greeting");
    let listed = client.list_folders().await.expect("list");
    drop(client);
    (listed, fake.finish().await)
}

#[tokio::test]
async fn list_folders_requests_special_use_and_skips_unprintable_names() {
    let (listed, commands) = listed_over(
        "* OK [CAPABILITY IMAP4rev1 SPECIAL-USE XLIST] ready\r\n",
        "LIST \"\" \"*\" RETURN (SPECIAL-USE)",
        "* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"\r\n\
         * LIST (\\Sent) \"/\" \"Entwürfe\"\r\n\
         * LIST (\\Trash) \"/\" \"\"\r\n\
         * LIST (\\Junk) \"/\" NIL\r\n\
         * LIST (\\Archive) \"/\" \"Archive\"\r\n\
         {tag} OK listed\r\n",
        false,
    )
    .await;
    assert_eq!(commands, ["A0001 LIST \"\" \"*\" RETURN (SPECIAL-USE)"]);
    assert_eq!(
        listed,
        [
            folder("Drafts", &["\\HASNOCHILDREN", "\\DRAFTS"]),
            folder("Archive", &["\\ARCHIVE"]),
        ]
    );
}

#[tokio::test]
async fn list_folders_uses_xlist_when_that_is_all_the_server_offers() {
    let (listed, commands) = listed_over(
        "* OK [CAPABILITY IMAP4rev1 XLIST] ready\r\n",
        "XLIST \"\" \"*\"",
        "* XLIST (\\Drafts) \"/\" \"Drafts\"\r\n\
         * LIST (\\Sent) \"/\" \"Sent\"\r\n\
         {tag} OK listed\r\n",
        true,
    )
    .await;
    assert_eq!(commands, ["A0001 XLIST \"\" \"*\""]);
    assert_eq!(listed, [folder("Drafts", &["\\DRAFTS"])]);
}

#[tokio::test]
async fn list_folders_uses_a_plain_list_when_the_server_marks_nothing() {
    let (listed, commands) = listed_over(
        "* OK [CAPABILITY IMAP4rev1] ready\r\n",
        "LIST \"\" \"*\"",
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK listed\r\n",
        false,
    )
    .await;
    assert_eq!(commands, ["A0001 LIST \"\" \"*\""]);
    assert_eq!(listed, [folder("INBOX", &["\\HASNOCHILDREN"])]);
}

#[test]
fn resolve_folders_guesses_nothing_from_common_names() {
    let common = [
        folder("Drafts", &[]),
        folder("Sent", &[]),
        folder("Trash", &[]),
        folder("Junk", &[]),
        folder("Spam", &[]),
        folder("Archive", &[]),
        folder("INBOX", &[]),
    ];
    assert_eq!(
        resolve_folders(&common, &FolderNames::default()),
        Folders::default(),
        "an unmarked folder is not chosen because of its name"
    );
    assert_eq!(
        resolve_folders(
            &common,
            &names("drafts", "sent", "trash", "junk", "archive")
        ),
        Folders::default(),
        "a configured name matches the wire byte for byte"
    );
}

#[test]
fn resolve_folders_prefers_marks_then_a_configured_utf7_name() {
    let drafts = utf7::encode("Entwürfe");
    assert_eq!(drafts, "Entw&APw-rfe");
    let folders = [
        folder("Nope", &["\\DRAFTS", "\\NOSELECT"]),
        folder("Ghost", &["\\TRASH", "\\NONEXISTENT"]),
        folder(&drafts, &[]),
        folder("Drafts", &[]),
        folder("Sent", &["\\SENT"]),
        folder("Bulk", &["\\SPAM"]),
        folder("Junk", &["\\JUNK"]),
        folder("Old", &["\\ARCHIVE", "\\NOSELECT"]),
        folder("Archive", &[]),
    ];
    assert_eq!(
        resolve_folders(
            &folders,
            &names("Entwürfe", "Sent Items", "Trash", "", "Archive")
        ),
        Folders {
            drafts: Some(drafts),
            sent: Some("Sent".to_owned()),
            trash: None,
            junk: Some("Bulk".to_owned()),
            archive: Some("Archive".to_owned()),
        }
    );
    assert_eq!(
        resolve_folders(&folders, &names("Missing", "", "Trash", "", "")).drafts,
        None,
        "a configured name that is not listed is not invented"
    );
}

#[tokio::test]
async fn the_source_resolves_special_use_marks_over_the_connection() {
    let script = vec![
        command(
            "LOGIN \"me@example.com\" \"secret\"",
            "{tag} OK [CAPABILITY IMAP4rev1 ID SPECIAL-USE] signed in\r\n",
        ),
        id(),
        examine(7, Some(10)),
        command(
            "LIST \"\" \"*\" RETURN (SPECIAL-USE)",
            "* LIST (\\Drafts) \"/\" \"Drafts\"\r\n\
             * LIST (\\Sent) \"/\" \"Sent\"\r\n\
             * LIST (\\Trash \\Noselect) \"/\" \"Trash\"\r\n\
             * LIST () \"/\" \"Deleted Items\"\r\n\
             * LIST (\\Junk) \"/\" \"Junk\"\r\n\
             * LIST (\\Archive) \"/\" \"Archive\"\r\n\
             {tag} OK listed\r\n",
        ),
    ];
    let (stream, fake) = fake::serve(GREETING, script);
    let mut source = ImapSource::start(stream, &config()).await.expect("session");
    let folders = source
        .folders(&names("Entwürfe", "", "Deleted Items", "", ""))
        .await
        .expect("folders");
    assert_eq!(
        folders,
        Folders {
            drafts: Some("Drafts".to_owned()),
            sent: Some("Sent".to_owned()),
            trash: Some("Deleted Items".to_owned()),
            junk: Some("Junk".to_owned()),
            archive: Some("Archive".to_owned()),
        }
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn the_source_matches_a_configured_unicode_name_and_does_not_guess() {
    let wire = utf7::encode("Entwürfe");
    let (mut source, fake) = open(session(vec![command(
        "LIST \"\" \"*\"",
        &format!(
            "* LIST () \"/\" \"{wire}\"\r\n\
             * LIST () \"/\" \"Drafts\"\r\n\
             * LIST (\\Noselect) \"/\" \"Sent\"\r\n\
             {{tag}} OK listed\r\n"
        ),
    )]))
    .await;
    let folders = source
        .folders(&names("Entwürfe", "Sent", "Trash", "", ""))
        .await
        .expect("folders");
    assert_eq!(folders.drafts.as_deref(), Some(wire.as_str()));
    assert_eq!(folders.sent, None, "a \\Noselect folder is not usable");
    assert_eq!(
        folders.trash, None,
        "an absent configured name is not guessed"
    );
    assert_eq!(folders.junk, None);
    assert_eq!(folders.archive, None);
    finish(source, fake).await;
}

#[tokio::test]
async fn a_message_id_search_asks_for_that_header() {
    let (mut source, fake) = open(session(vec![command(
        "UID SEARCH HEADER MESSAGE-ID \"<x@y>\"",
        "* SEARCH 9 4\r\n{tag} OK found\r\n",
    )]))
    .await;
    let found = source
        .client
        .uid_search(client::Search::MessageId("<x@y>".to_owned()))
        .await
        .expect("search");
    assert_eq!(found, [4, 9]);
    let commands = finish(source, fake).await;
    assert_eq!(
        commands.last().map(String::as_str),
        Some("A0004 UID SEARCH HEADER MESSAGE-ID \"<x@y>\"")
    );
}
