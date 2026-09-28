//! Unit tests for `src/email/imap/client.rs`.

use super::*;
use crate::email::imap::fake::{self, Fake, Step, command, line};
use tokio::io::DuplexStream;

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] ready\r\n";

async fn connect(greeting: &str, script: Vec<Step>) -> (Client<DuplexStream>, Fake) {
    let (stream, fake) = fake::serve(greeting, script);
    let client = Client::start(stream, COMMAND_TIMEOUT).await.unwrap();
    (client, fake)
}

/// The commands the server saw once the client is gone.
async fn finish(client: Client<DuplexStream>, fake: Fake) -> Vec<String> {
    drop(client);
    fake.finish().await
}

#[tokio::test]
async fn the_greeting_may_carry_capabilities_or_preauthenticate() {
    let (client, fake) = connect(GREETING, vec![]).await;
    assert_eq!(client.capabilities(), ["IMAP4REV1", "AUTH=PLAIN"]);
    assert!(client.has("auth=plain"));
    assert!(!client.preauthenticated());
    assert!(finish(client, fake).await.is_empty());

    let (client, fake) = connect("* PREAUTH signed in already\r\n", vec![]).await;
    assert!(client.preauthenticated());
    assert!(client.capabilities().is_empty());
    finish(client, fake).await;
}

#[tokio::test]
async fn a_bye_greeting_refuses_without_the_servers_text() {
    let (stream, fake) = fake::serve("* BYE [UNAVAILABLE] secret maintenance note\r\n", vec![]);
    let error = Client::start(stream, COMMAND_TIMEOUT).await.err().unwrap();
    let message = format!("{error:#}");
    assert!(message.contains("[UNAVAILABLE]"), "{message}");
    assert!(!message.contains("secret"), "{message}");
    fake.finish().await;

    for greeting in ["A1 OK hello\r\n", "+ hello\r\n", "* 1 EXISTS\r\n"] {
        let (stream, fake) = fake::serve(greeting, vec![]);
        assert!(Client::start(stream, COMMAND_TIMEOUT).await.is_err());
        fake.finish().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_silent_server_times_out_at_the_greeting() {
    let (client_end, server_end) = tokio::io::duplex(64);
    let error = Client::start(client_end, COMMAND_TIMEOUT)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("greeting"), "{error}");
    drop(server_end);
}

#[tokio::test]
async fn tags_count_up_and_capabilities_refresh() {
    let (mut client, fake) = connect(
        GREETING,
        vec![
            command("NOOP", "{tag} OK done\r\n"),
            command(
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1 IDLE MOVE\r\n{tag} OK done\r\n",
            ),
        ],
    )
    .await;
    client.noop().await.unwrap();
    client.capability().await.unwrap();
    assert_eq!(client.capabilities(), ["IMAP4REV1", "IDLE", "MOVE"]);
    assert_eq!(
        finish(client, fake).await,
        ["A0001 NOOP", "A0002 CAPABILITY"]
    );
}

#[tokio::test]
async fn login_quotes_a_printable_password() {
    let (mut client, fake) = connect(
        GREETING,
        vec![command(
            "LOGIN \"me@example.com\" \"p\\\"a\\\\ss word\"",
            "{tag} OK [CAPABILITY IMAP4rev1 ID IDLE] signed in\r\n",
        )],
    )
    .await;
    client
        .login("me@example.com", "p\"a\\ss word")
        .await
        .unwrap();
    assert!(client.has("ID"));
    assert_eq!(finish(client, fake).await.len(), 1);
}

#[tokio::test]
async fn login_sends_a_non_ascii_password_as_a_literal() {
    let (mut client, fake) = connect(
        GREETING,
        vec![
            command("LOGIN \"me\" {10}\r\npässwörd", "{tag} OK signed in\r\n"),
            command(
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1 ID\r\n{tag} OK done\r\n",
            ),
        ],
    )
    .await;
    client.login("me", "pässwörd").await.unwrap();
    assert!(client.has("ID"));
    finish(client, fake).await;
}

#[tokio::test]
async fn login_disabled_uses_authenticate_plain() {
    let greeting = "* OK [CAPABILITY IMAP4rev1 LOGINDISABLED AUTH=PLAIN] ready\r\n";
    let (mut client, fake) = connect(
        greeting,
        vec![
            command("AUTHENTICATE PLAIN", "+ \r\n"),
            line(
                "AG1lQGV4YW1wbGUuY29tAHNlY3JldA==",
                "{tag} OK [CAPABILITY IMAP4rev1 ID] signed in\r\n",
            ),
        ],
    )
    .await;
    client.login("me@example.com", "secret").await.unwrap();
    assert!(client.has("ID"));
    assert_eq!(
        finish(client, fake).await,
        [
            "A0001 AUTHENTICATE PLAIN",
            "AG1lQGV4YW1wbGUuY29tAHNlY3JldA=="
        ]
    );
}

#[tokio::test]
async fn authenticate_plain_uses_an_initial_response_with_sasl_ir() {
    let greeting = "* OK [CAPABILITY IMAP4rev1 LOGINDISABLED SASL-IR AUTH=PLAIN] ready\r\n";
    let (mut client, fake) = connect(
        greeting,
        vec![command(
            "AUTHENTICATE PLAIN AG1lQGV4YW1wbGUuY29tAHNlY3JldA==",
            "{tag} OK [CAPABILITY IMAP4rev1] signed in\r\n",
        )],
    )
    .await;
    client.login("me@example.com", "secret").await.unwrap();
    finish(client, fake).await;
}

#[tokio::test]
async fn login_disabled_without_plain_sends_nothing() {
    let greeting = "* OK [CAPABILITY IMAP4rev1 LOGINDISABLED AUTH=XOAUTH2] ready\r\n";
    let (mut client, fake) = connect(greeting, vec![]).await;
    assert!(client.login("me", "secret").await.is_err());
    assert!(client.login("me", "sec\0ret").await.is_err());
    assert!(finish(client, fake).await.is_empty());
}

/// Every way an error may be shown or logged.
fn shown(error: &anyhow::Error) -> String {
    format!("{error}\n{error:#}\n{error:?}")
}

#[tokio::test]
async fn a_failed_login_never_repeats_the_servers_text_or_the_secret() {
    const SECRET: &str = "hunter2-SECRET";
    // A hostile server echoes the password, in its text and as a response
    // code, around a Unicode line separator that would fake a log line.
    let replies = [
        format!(
            "{{tag}} NO [AUTHENTICATIONFAILED] Invalid password {SECRET} for me@example.com\u{2028}INFO signed in\r\n"
        ),
        format!("{{tag}} NO [{SECRET}] {SECRET}\r\n"),
        format!("{{tag}} BAD [ALERT] \u{2029}{SECRET}\u{85}\r\n"),
    ];
    for reply in replies {
        let (mut client, fake) = connect(
            GREETING,
            vec![command(
                &format!("LOGIN \"me@example.com\" \"{SECRET}\""),
                &reply,
            )],
        )
        .await;
        let error = client.login("me@example.com", SECRET).await.unwrap_err();
        let message = shown(&error);
        assert!(message.contains("IMAP sign-in was refused"), "{message}");
        assert!(message.contains("authorization code"), "{message}");
        for leaked in [SECRET, "hunter2", "me@example.com", "Invalid", "INFO"] {
            assert!(!message.contains(leaked), "{leaked}: {message}");
        }
        assert!(
            !message.contains(['\u{2028}', '\u{2029}', '\u{85}']),
            "{message}"
        );
        assert!(!client.is_broken());
        finish(client, fake).await;
    }
}

#[tokio::test]
async fn a_failed_authenticate_plain_never_repeats_the_credentials() {
    let greeting = "* OK [CAPABILITY IMAP4rev1 LOGINDISABLED AUTH=PLAIN SASL-IR] ready\r\n";
    // `\0me\0secret`, which the server echoes back.
    let response = "AG1lAHNlY3JldA==";
    let (mut client, fake) = connect(
        greeting,
        vec![command(
            &format!("AUTHENTICATE PLAIN {response}"),
            &format!("{{tag}} NO [AUTHENTICATIONFAILED] bad credentials {response} secret\r\n"),
        )],
    )
    .await;
    let message = shown(&client.login("me", "secret").await.unwrap_err());
    assert!(message.contains("[AUTHENTICATIONFAILED]"), "{message}");
    assert!(
        !message.contains(response) && !message.contains("secret"),
        "{message}"
    );
    finish(client, fake).await;
}

#[tokio::test]
async fn only_standard_response_codes_are_named() {
    let (mut client, fake) = connect(
        GREETING,
        vec![
            command("EXAMINE \"INBOX\"", "{tag} NO [overquota] full\r\n"),
            command("EXAMINE \"INBOX\"", "{tag} NO [X-SECRET-TOKEN] full\r\n"),
        ],
    )
    .await;
    let message = shown(&client.examine("INBOX").await.unwrap_err());
    assert!(message.contains("NO [OVERQUOTA]"), "{message}");
    let message = shown(&client.examine("INBOX").await.unwrap_err());
    assert!(message.contains("EXAMINE: NO"), "{message}");
    assert!(
        !message.contains("SECRET") && !message.contains("[X"),
        "{message}"
    );
    finish(client, fake).await;
}

#[tokio::test]
async fn a_refusal_names_the_verb_and_code_but_not_the_servers_text() {
    let (mut client, fake) = connect(
        GREETING,
        vec![
            command(
                "EXAMINE \"Secret Project\"",
                "{tag} NO [NONEXISTENT] Mailbox \"Secret Project\" doesn't exist\r\n",
            ),
            command(
                "UID SEARCH ALL",
                "{tag} BAD Could not parse \"Subject: invoice\"\r\n",
            ),
            command("NOOP", "{tag} OK\r\n"),
        ],
    )
    .await;
    let message = client
        .examine("Secret Project")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("EXAMINE") && message.contains("NO [NONEXISTENT]"),
        "{message}"
    );
    assert!(
        !message.contains("Secret") && !message.contains("exist"),
        "{message}"
    );
    let message = format!("{:#}", client.uid_search(Search::All).await.unwrap_err());
    assert!(
        message.contains("UID SEARCH") && message.contains("BAD"),
        "{message}"
    );
    assert!(
        !message.contains("invoice") && !message.contains("parse"),
        "{message}"
    );
    // A refusal leaves the connection usable.
    assert!(!client.is_broken());
    client.noop().await.unwrap();
    finish(client, fake).await;
}

#[tokio::test]
async fn a_refused_command_writes_nothing_and_ends_the_connection() {
    let (mut client, fake) = connect(GREETING, vec![command("NOOP", "{tag} OK\r\n")]).await;
    client.noop().await.unwrap();
    let error = client
        .run("STORE 1 +FLAGS (\\Seen)", Vec::new())
        .await
        .unwrap_err();
    let violation = error.downcast_ref::<guard::GuardViolation>().unwrap();
    assert_eq!(violation.verb, "STORE");
    assert!(client.is_broken());
    assert!(client.noop().await.is_err());
    assert_eq!(finish(client, fake).await, ["A0001 NOOP"]);

    let (mut client, fake) = connect(GREETING, vec![]).await;
    let append = client
        .run(
            "APPEND INBOX ",
            vec![Part::Literal(b"Subject: x\r\n\r\nbody".to_vec())],
        )
        .await;
    assert!(append.is_err());
    let expunge = client.run("UID EXPUNGE 1:*", Vec::new()).await;
    assert!(expunge.is_err());
    assert!(finish(client, fake).await.is_empty());
}

#[tokio::test]
async fn a_protocol_error_ends_the_connection() {
    let (mut client, fake) = connect(GREETING, vec![command("NOOP", "{tag} MAYBE\r\n")]).await;
    assert!(client.noop().await.is_err());
    assert!(client.is_broken());
    // Nothing more is sent: the server would fail the test on any command.
    assert!(client.noop().await.is_err());
    assert!(client.examine("INBOX").await.is_err());
    assert_eq!(finish(client, fake).await, ["A0001 NOOP"]);
}

#[tokio::test]
async fn a_closed_connection_ends_the_client() {
    let (stream, server_end) = tokio::io::duplex(1024);
    let mut server_end = server_end;
    server_end.write_all(GREETING.as_bytes()).await.unwrap();
    let mut client = Client::start(stream, COMMAND_TIMEOUT).await.unwrap();
    drop(server_end);
    assert!(client.noop().await.is_err());
    assert!(client.is_broken());
}

#[tokio::test(start_paused = true)]
async fn a_command_times_out_and_ends_the_connection() {
    let (mut client, fake) = connect(GREETING, vec![Step::Hang]).await;
    let started = tokio::time::Instant::now();
    let error = client.noop().await.unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(started.elapsed() >= COMMAND_TIMEOUT);
    assert!(client.is_broken());
    assert!(client.noop().await.is_err());
    assert_eq!(finish(client, fake).await, ["A0001 NOOP"]);
}

#[tokio::test]
async fn examine_reports_the_mailbox_state_and_encodes_its_name() {
    let (mut client, fake) = connect(
        GREETING,
        vec![
            command(
                "EXAMINE \"&XfJT0ZAB-\"",
                "* FLAGS (\\Seen)\r\n* 172 EXISTS\r\n* 1 RECENT\r\n\
                 * OK [UIDVALIDITY 3857529045] UIDs valid\r\n* OK [UIDNEXT 4392] next\r\n\
                 * OK [PERMANENTFLAGS ()] none\r\n{tag} OK [READ-ONLY] done\r\n",
            ),
            command(
                "EXAMINE \"a\\\"b\"",
                "* OK [UIDVALIDITY 0] bad\r\n{tag} OK done\r\n",
            ),
        ],
    )
    .await;
    assert_eq!(
        client.examine("已发送").await.unwrap(),
        Examined {
            uidvalidity: Some(3857529045),
            uidnext: Some(4392),
            exists: Some(172),
        }
    );
    assert_eq!(client.examine("a\"b").await.unwrap(), Examined::default());
    finish(client, fake).await;
}

#[tokio::test]
async fn uid_search_drops_what_the_star_quirk_matches() {
    let (mut client, fake) = connect(
        GREETING,
        vec![
            command("UID SEARCH UID 10:*", "* SEARCH 9\r\n{tag} OK\r\n"),
            command(
                "UID SEARCH UID 10:*",
                "* SEARCH 12 10\r\n* SEARCH 11 12\r\n{tag} OK\r\n",
            ),
            command("UID SEARCH SINCE 25-Sep-2026", "* SEARCH\r\n{tag} OK\r\n"),
        ],
    )
    .await;
    assert!(
        client
            .uid_search(Search::UidFrom(10))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        client.uid_search(Search::UidFrom(10)).await.unwrap(),
        [10, 11, 12]
    );
    assert!(
        client
            .uid_search(Search::Since(1790596800 - 3 * 86_400))
            .await
            .unwrap()
            .is_empty()
    );
    finish(client, fake).await;
}

#[tokio::test]
async fn uid_fetch_merges_split_responses_and_drops_others() {
    let (mut client, fake) = connect(
        GREETING,
        vec![command(
            "UID FETCH 5,6 (UID RFC822.SIZE BODY.PEEK[1]<0.100>)",
            "* 1 FETCH (UID 5 RFC822.SIZE 10)\r\n\
             * 3 FETCH (FLAGS (\\Seen))\r\n\
             * 1 FETCH (BODY[1]<0> {5}\r\nhello)\r\n\
             * 2 FETCH (UID 9 RFC822.SIZE 1)\r\n\
             * 4 FETCH (UID 6 BODY[1]<0> NIL RFC822.SIZE 20)\r\n\
             {tag} OK done\r\n",
        )],
    )
    .await;
    assert!(
        client
            .uid_fetch(&[], &[Item::Uid])
            .await
            .unwrap()
            .is_empty()
    );
    let fetched = client
        .uid_fetch(
            &[5, 6],
            &[
                Item::Uid,
                Item::Size,
                Item::Peek {
                    section: "1",
                    partial: Some((0, 100)),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(fetched.len(), 2);
    assert_eq!(fetched[0].uid, Some(5));
    assert_eq!(fetched[0].size, Some(10));
    assert_eq!(fetched[0].section("1"), Some(&b"hello"[..]));
    assert_eq!(fetched[1].uid, Some(6));
    assert_eq!(fetched[1].section("1"), None);
    finish(client, fake).await;
}

#[tokio::test]
async fn a_malformed_untagged_response_is_skipped() {
    let (mut client, fake) = connect(
        GREETING,
        vec![command(
            "UID FETCH 5,6 (UID RFC822.SIZE)",
            "* 1 FETCH (UID 5 BODYSTRUCTURE ((((\"x\" (\r\n\
             * 2 FETCH (UID 6 RFC822.SIZE 10)\r\n{tag} OK\r\n",
        )],
    )
    .await;
    let fetched = client.uid_fetch(&[5, 6], &[Item::Size]).await.unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].uid, Some(6));
    assert!(!client.is_broken());
    finish(client, fake).await;
}

#[tokio::test]
async fn id_sends_quoted_fields_and_tolerates_a_refusal() {
    let (mut client, fake) = connect(
        GREETING,
        vec![command(
            "ID (\"name\" \"SCV\" \"version\" \"1.2.3\")",
            "{tag} BAD unknown command\r\n",
        )],
    )
    .await;
    client
        .id(&[("name", "SCV"), ("version", "1.2.3")])
        .await
        .unwrap();
    assert!(!client.is_broken());
    finish(client, fake).await;
}

#[tokio::test]
async fn logout_finishes_the_connection() {
    let (mut client, fake) = connect(
        GREETING,
        vec![command("LOGOUT", "* BYE logging out\r\n{tag} OK done\r\n")],
    )
    .await;
    client.logout().await;
    assert!(client.is_broken());
    assert!(client.noop().await.is_err());
    // A second logout sends nothing.
    client.logout().await;
    assert_eq!(finish(client, fake).await, ["A0001 LOGOUT"]);
}

#[test]
fn segments_split_where_the_server_must_answer() {
    assert_eq!(
        segments(&[
            Part::Text("A1 LOGIN ".to_owned()),
            Part::Literal(b"me".to_vec()),
            Part::Text(" \"pw\"".to_owned()),
        ]),
        [b"A1 LOGIN {2}\r\n".to_vec(), b"me \"pw\"\r\n".to_vec()]
    );
    assert_eq!(
        segments(&[
            Part::Text("A1 AUTHENTICATE PLAIN".to_owned()),
            Part::Line("AAAA".to_owned()),
        ]),
        [b"A1 AUTHENTICATE PLAIN\r\n".to_vec(), b"AAAA\r\n".to_vec()]
    );
    assert_eq!(astring("é"), Part::Literal("é".as_bytes().to_vec()));
    assert_eq!(astring("a\"b"), Part::Text("\"a\\\"b\"".to_owned()));
}
