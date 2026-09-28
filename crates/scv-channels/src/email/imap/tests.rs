//! Unit tests for `src/email/imap/mod.rs`.

use super::*;
use fake::{Fake, Step, command};
use tokio::io::{AsyncWriteExt as _, DuplexStream};

mod changes;
mod messages;

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN ID] ready\r\n";

fn config() -> ImapConfig {
    ImapConfig {
        host: "imap.example.com".to_owned(),
        port: 993,
        username: "me@example.com".to_owned(),
        password: "secret".to_owned(),
        mailbox: "INBOX".to_owned(),
    }
}

fn login() -> Step {
    command(
        "LOGIN \"me@example.com\" \"secret\"",
        "{tag} OK [CAPABILITY IMAP4rev1 ID IDLE MOVE UIDPLUS] signed in\r\n",
    )
}

fn id() -> Step {
    command(
        &format!(
            "ID (\"name\" \"SCV\" \"version\" \"{}\")",
            env!("CARGO_PKG_VERSION")
        ),
        "* ID NIL\r\n{tag} OK\r\n",
    )
}

/// `EXAMINE "INBOX"` answered with this state.
fn examine(uidvalidity: u32, uidnext: Option<u32>) -> Step {
    let uidnext = uidnext
        .map(|uidnext| format!("* OK [UIDNEXT {uidnext}] next\r\n"))
        .unwrap_or_default();
    command(
        "EXAMINE \"INBOX\"",
        &format!(
            "* 3 EXISTS\r\n* OK [UIDVALIDITY {uidvalidity}] valid\r\n{uidnext}{{tag}} OK [READ-ONLY] done\r\n"
        ),
    )
}

/// A session opening INBOX (`UIDVALIDITY` 7, `UIDNEXT` 10), then `rest`.
fn session(rest: Vec<Step>) -> Vec<Step> {
    let mut script = vec![login(), id(), examine(7, Some(10))];
    script.extend(rest);
    script
}

async fn open(script: Vec<Step>) -> (ImapSource<DuplexStream>, Fake) {
    let (stream, fake) = fake::serve(GREETING, script);
    let source = ImapSource::start(stream, &config()).await.unwrap();
    (source, fake)
}

/// The commands the server saw once the source is gone.
async fn finish(source: ImapSource<DuplexStream>, fake: Fake) -> Vec<String> {
    drop(source);
    fake.finish().await
}

fn cursor(mailbox: &str, uidvalidity: u32, next: u32) -> Cursor {
    Cursor {
        provider: ProviderKind::Imap,
        value: serde_json::json!({
            "mailbox": mailbox,
            "uidvalidity": uidvalidity,
            "next": next,
        })
        .to_string(),
    }
}

fn inbox(uid: u32) -> SourceRef {
    SourceRef::Imap {
        mailbox: "INBOX".to_owned(),
        uidvalidity: 7,
        uid,
    }
}

#[tokio::test]
async fn starts_by_signing_in_identifying_and_examining() {
    let (source, fake) = open(session(vec![])).await;
    assert_eq!(
        source.caps(),
        Caps {
            push: true,
            move_: true,
            uidplus: true,
            special_use: false,
            id: true,
        }
    );
    assert_eq!((source.uidvalidity, source.uidnext), (7, Some(10)));
    let received = finish(source, fake).await;
    assert_eq!(received.len(), 3);
    assert!(received[0].starts_with("A0001 LOGIN "));
    assert!(received[1].starts_with("A0002 ID "));
    assert_eq!(received[2], "A0003 EXAMINE \"INBOX\"");
}

#[tokio::test]
async fn asks_for_capabilities_when_not_told_and_skips_id_when_not_offered() {
    let script = vec![
        command(
            "CAPABILITY",
            "* CAPABILITY IMAP4rev1 AUTH=PLAIN\r\n{tag} OK\r\n",
        ),
        command(
            "LOGIN \"me@example.com\" \"secret\"",
            "{tag} OK signed in\r\n",
        ),
        command(
            "CAPABILITY",
            "* CAPABILITY IMAP4rev1 SPECIAL-USE\r\n{tag} OK\r\n",
        ),
        examine(7, Some(10)),
    ];
    let (stream, fake) = fake::serve("* OK ready\r\n", script);
    let source = ImapSource::start(stream, &config()).await.unwrap();
    assert_eq!(
        source.caps(),
        Caps {
            special_use: true,
            ..Caps::default()
        }
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn a_preauthenticated_session_does_not_sign_in() {
    let (stream, fake) = fake::serve(
        "* PREAUTH [CAPABILITY IMAP4rev1] welcome\r\n",
        vec![examine(7, Some(10))],
    );
    let source = ImapSource::start(stream, &config()).await.unwrap();
    assert_eq!(finish(source, fake).await, ["A0001 EXAMINE \"INBOX\""]);
}

#[tokio::test]
async fn a_mailbox_without_uidvalidity_cannot_be_read() {
    let script = vec![
        login(),
        id(),
        command("EXAMINE \"INBOX\"", "* 3 EXISTS\r\n{tag} OK done\r\n"),
    ];
    let (stream, fake) = fake::serve(GREETING, script);
    let error = ImapSource::start(stream, &config()).await.err().unwrap();
    assert!(error.to_string().contains("UIDVALIDITY"), "{error}");
    fake.finish().await;
}

#[tokio::test]
async fn a_refused_mailbox_is_reported_without_the_servers_text() {
    let script = vec![
        login(),
        id(),
        command(
            "EXAMINE \"INBOX\"",
            "{tag} NO [NONEXISTENT] No mailbox named \"INBOX\" for Mr Secret\r\n",
        ),
    ];
    let (stream, fake) = fake::serve(GREETING, script);
    let error = ImapSource::start(stream, &config()).await.err().unwrap();
    let message = format!("{error:#}");
    assert!(
        message.contains("EXAMINE") && message.contains("[NONEXISTENT]"),
        "{message}"
    );
    assert!(!message.contains("Secret"), "{message}");
    fake.finish().await;
}

#[tokio::test]
async fn close_logs_out() {
    let (source, fake) = open(session(vec![command(
        "LOGOUT",
        "* BYE bye\r\n{tag} OK\r\n",
    )]))
    .await;
    source.close().await;
    let received = fake.finish().await;
    assert_eq!(received.last().unwrap(), "A0004 LOGOUT");
}

#[test]
fn the_config_debug_output_hides_credentials() {
    let debug = format!("{:?}", config());
    assert!(debug.contains("imap.example.com") && debug.contains("INBOX"));
    assert!(!debug.contains("secret") && !debug.contains("me@example.com"));
}

#[test]
fn part_sections_are_part_numbers() {
    for good in ["1", "1.2", "10.3.1"] {
        assert!(part_section(good), "{good}");
    }
    for bad in [
        "", "0", "1.", ".1", "1..2", "01", "TEXT", "1.MIME", "1]<0.9>", "1 2",
    ] {
        assert!(!part_section(bad), "{bad}");
    }
}

#[tokio::test]
#[should_panic(expected = "not read-only")]
async fn the_fake_server_fails_a_test_that_sends_a_store() {
    let (mut stream, fake) = fake::serve(GREETING, vec![]);
    stream
        .write_all(b"A1 UID STORE 1 +FLAGS (\\Seen)\r\n")
        .await
        .unwrap();
    drop(stream);
    fake.finish().await;
}

#[tokio::test]
#[should_panic(expected = "not read-only")]
async fn the_fake_server_fails_a_test_that_fetches_without_peek() {
    let (mut stream, fake) = fake::serve(GREETING, vec![]);
    stream
        .write_all(b"A1 UID FETCH 1 (UID BODY[1])\r\n")
        .await
        .unwrap();
    drop(stream);
    fake.finish().await;
}
