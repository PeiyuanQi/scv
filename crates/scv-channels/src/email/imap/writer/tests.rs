//! Unit tests for `src/email/imap/writer.rs`.

use super::super::fake::{self, Fake, Step, command};
use super::*;
use crate::email::content::{CONTENT_VERSION, Folder, FolderRole, Form, Mailbox, Origin, Outgoing};
use crate::email::settings::SentCopy;
use crate::email::source::MailSource;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::io::DuplexStream;

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] ready\r\n";
const READ_CAPS: &str = "IMAP4rev1 ID";
const MOVE_CAPS: &str = "IMAP4rev1 ID MOVE";
const UIDPLUS_CAPS: &str = "IMAP4rev1 ID UIDPLUS";
const BOTH_CAPS: &str = "IMAP4rev1 ID MOVE UIDPLUS";

const UID: u32 = 42;
const UIDVALIDITY: u32 = 7;
const RECEIVED: &str = "01-Sep-2026 00:00:00 +0000";
const SIZE: u64 = 100;
const FROM: &str = "alice@example.com";
const SUBJECT: &str = "Hello";
const MESSAGE_ID: &str = "<m@example.com>";
const DRAFT_ID: &str = "<draft@example.com>";
const SENT_ID: &str = "<sent@example.com>";

const WITH_TRASH: &str = "* LIST () \"/\" \"INBOX\"\r\n* LIST (\\Trash) \"/\" \"Trash\"\r\n";
const NO_TRASH: &str = "* LIST () \"/\" \"INBOX\"\r\n";

/// One scripted duplex per [`Connect::connect`], in the order given.
struct Scripted {
    streams: Mutex<VecDeque<DuplexStream>>,
}

#[async_trait]
impl Connect for Scripted {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<Self::Stream> {
        let Some(stream) = self.streams.lock().unwrap().pop_front() else {
            anyhow::bail!("the test has no scripted connection left");
        };
        Ok(stream)
    }
}

fn config() -> ImapConfig {
    ImapConfig {
        host: "imap.example.com".to_owned(),
        port: 993,
        username: "me@example.com".to_owned(),
        password: "secret".to_owned(),
        mailbox: "INBOX".to_owned(),
    }
}

fn effects_of(stream: DuplexStream) -> ImapEffects<Scripted> {
    ImapEffects {
        connect: Scripted {
            streams: Mutex::new(VecDeque::from([stream])),
        },
        config: config(),
        smtp: None,
    }
}

/// Login, then `ID` when `caps` offers it, then `rest`.
fn steps(caps: &str, rest: Vec<Step>) -> Vec<Step> {
    let mut script = vec![command(
        "LOGIN \"me@example.com\" \"secret\"",
        &format!("{{tag}} OK [CAPABILITY {caps}] signed in\r\n"),
    )];
    if caps.split(' ').any(|cap| cap == "ID") {
        script.push(command(
            &format!(
                "ID (\"name\" \"SCV\" \"version\" \"{}\")",
                env!("CARGO_PKG_VERSION")
            ),
            "* ID NIL\r\n{tag} OK\r\n",
        ));
    }
    script.extend(rest);
    script
}

fn select(uidvalidity: u32) -> Step {
    command(
        "SELECT \"INBOX\"",
        &format!("* OK [UIDVALIDITY {uidvalidity}] valid\r\n{{tag}} OK [READ-WRITE] selected\r\n"),
    )
}

fn examine_mailbox(name: &str, uidvalidity: u32) -> Step {
    command(
        &format!("EXAMINE \"{name}\""),
        &format!("* OK [UIDVALIDITY {uidvalidity}] valid\r\n{{tag}} OK [READ-ONLY] examined\r\n"),
    )
}

fn list(body: &str) -> Step {
    command("LIST \"\" \"*\"", &format!("{body}{{tag}} OK listed\r\n"))
}

fn logout() -> Step {
    command("LOGOUT", "* BYE bye\r\n{tag} OK logged out\r\n")
}

fn fetch_line(uid: u32) -> String {
    format!(
        "UID FETCH {uid} (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE \
         BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])"
    )
}

fn message_fields(uid: u32, flags: &str) -> String {
    format!(
        "UID {uid} FLAGS ({flags}) INTERNALDATE \"{RECEIVED}\" RFC822.SIZE {SIZE} \
         ENVELOPE (NIL \"{SUBJECT}\" ((\"Alice\" NIL \"alice\" \"example.com\")) \
         NIL NIL NIL NIL NIL NIL \"{MESSAGE_ID}\") \
         BODY[HEADER.FIELDS (MESSAGE-ID)] NIL"
    )
}

fn fetch_reply(uid: u32, flags: &str) -> String {
    format!(
        "* 1 FETCH ({})\r\n{{tag}} OK fetched\r\n",
        message_fields(uid, flags)
    )
}

fn fetch_step(uid: u32, flags: &str) -> Step {
    command(&fetch_line(uid), &fetch_reply(uid, flags))
}

fn empty_fetch(uid: u32) -> Step {
    command(&fetch_line(uid), "{tag} OK fetched\r\n")
}

fn search_line(id: &str) -> String {
    format!("UID SEARCH HEADER MESSAGE-ID \"{id}\"")
}

fn search_step(id: &str, hits: &[u32]) -> Step {
    let numbers = hits
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    command(
        &search_line(id),
        &format!("* SEARCH {numbers}\r\n{{tag}} OK found\r\n"),
    )
}

fn append_line(folder: &str, flags: &str, message: &str) -> String {
    format!(
        "APPEND \"{folder}\" {flags} {{{}}}\r\n{message}",
        message.len()
    )
}

fn outgoing_bytes(message_id: &str) -> String {
    format!("From: me@example.com\r\nMessage-ID: {message_id}\r\n\r\nThanks.\r\n")
}

/// What the server saw, and none of it is a mailbox-destroying command.
/// `UID EXPUNGE` of one message is the move's allowed exception.
async fn seen(effects: ImapEffects<Scripted>, fake: Fake) -> Vec<String> {
    drop(effects);
    let commands = fake.finish().await;
    for command in &commands {
        let text = command
            .split_once(' ')
            .map_or(command.as_str(), |(_, rest)| rest);
        let verb = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        assert!(
            !matches!(
                verb.as_str(),
                "CLOSE" | "EXPUNGE" | "DELETE" | "CREATE" | "RENAME"
            ),
            "destructive command: {command}"
        );
    }
    commands
}

/// The tagged commands of a session whose caps include `ID`, in order.
fn expect(commands: &[&str]) -> Vec<String> {
    let mut all = vec![
        "LOGIN \"me@example.com\" \"secret\"".to_owned(),
        format!(
            "ID (\"name\" \"SCV\" \"version\" \"{}\")",
            env!("CARGO_PKG_VERSION")
        ),
    ];
    all.extend(commands.iter().map(|command| (*command).to_owned()));
    all.into_iter()
        .enumerate()
        .map(|(index, command)| format!("A{:04} {command}", index + 1))
        .collect()
}

fn applied(code: OutcomeCode) -> Execution {
    Execution::Applied {
        code,
        sent_copy: None,
    }
}

fn stored() -> (String, String) {
    let id = MESSAGE_ID.as_bytes();
    (
        crate::email::parse::identity("INBOX", Some(id), RECEIVED, SIZE, FROM, SUBJECT),
        crate::email::parse::locator(Some(id), RECEIVED, SIZE, FROM, SUBJECT),
    )
}

fn parsed_fetch(flags: &str) -> super::super::fetch::Fetched {
    let raw = format!("* 1 FETCH ({})\r\n", message_fields(UID, flags));
    match super::super::wire::parse(raw.as_bytes()).expect("the sample fetch parses") {
        super::super::wire::Response::Message { values, .. } => {
            let mut fetched = super::super::fetch::Fetched::default();
            fetched.merge(values[0].list().expect("a fetch list"));
            fetched
        }
        other => panic!("not a fetch: {other:?}"),
    }
}

fn approve(content: ActionContent, resume: Option<Resume>) -> Approved {
    Approved::for_tests(content, 1, resume)
}

fn base(
    kind: ActionKind,
    folder: Option<Folder>,
    source: Option<Source>,
    message_id: Option<&str>,
) -> ActionContent {
    ActionContent {
        v: CONTENT_VERSION,
        id: "a0123456789abcdef0123456789abcdef".to_owned(),
        account: "default".to_owned(),
        fingerprint: "fingerprint".to_owned(),
        kind,
        group: None,
        origin: Origin::Triage {
            source: "inbox".to_owned(),
        },
        source,
        display: None,
        folder,
        message: message_id.map(outgoing),
        created_at: 1_728_000_000,
        hard_expiry: 1_728_000_000 + 3 * 86_400,
        digest: String::new(),
    }
    .seal_digest()
}

fn outgoing(message_id: &str) -> Outgoing {
    Outgoing {
        form: Form::Reply,
        from: Mailbox {
            name: String::new(),
            address: "me@example.com".to_owned(),
        },
        to: vec!["alice@example.com".to_owned()],
        cc: Vec::new(),
        subject: "Re: Hello".to_owned(),
        body: "Thanks.".to_owned(),
        in_reply_to: None,
        references: Vec::new(),
        message_id: message_id.to_owned(),
        sent_copy: SentCopy::Append,
        notes: Vec::new(),
    }
}

fn imap_message(identity: &str, locator: &str, message_id: Option<&str>) -> Source {
    Source {
        reference: SourceRef::Imap {
            mailbox: "INBOX".to_owned(),
            uidvalidity: UIDVALIDITY,
            uid: UID,
        },
        identity: identity.to_owned(),
        locator: locator.to_owned(),
        message_id: message_id.map(str::to_owned),
    }
}

fn role_folder(role: FolderRole, name: &str) -> Folder {
    Folder {
        role,
        name: name.to_owned(),
    }
}

fn mark_read(identity: &str, locator: &str) -> ActionContent {
    base(
        ActionKind::MarkRead,
        None,
        Some(imap_message(identity, locator, Some(MESSAGE_ID))),
        None,
    )
}

fn trash(identity: &str, locator: &str, message_id: Option<&str>) -> ActionContent {
    base(
        ActionKind::Trash,
        Some(role_folder(FolderRole::Trash, "Trash")),
        Some(imap_message(identity, locator, message_id)),
        None,
    )
}

fn draft_content() -> ActionContent {
    base(
        ActionKind::Draft,
        Some(role_folder(FolderRole::Drafts, "Drafts")),
        None,
        Some(DRAFT_ID),
    )
}

fn send_content() -> ActionContent {
    base(
        ActionKind::Send,
        Some(role_folder(FolderRole::Sent, "Sent")),
        None,
        Some(SENT_ID),
    )
}

async fn run_change(
    caps: &str,
    rest: Vec<Step>,
    content: ActionContent,
    resume: Option<Resume>,
) -> (Execution, Vec<String>) {
    let approved = approve(content, resume);
    let (stream, fake) = fake::serve_writable(GREETING, steps(caps, rest));
    let mut effects = effects_of(stream);
    let execution = effects.change(&approved).await;
    (execution, seen(effects, fake).await)
}

async fn run_draft(rest: Vec<Step>, message: &str) -> (Execution, Vec<String>) {
    let approved = approve(draft_content(), None);
    let (stream, fake) = fake::serve_writable(GREETING, steps(READ_CAPS, rest));
    let mut effects = effects_of(stream);
    let execution = effects.save_draft(&approved, message.as_bytes()).await;
    (execution, seen(effects, fake).await)
}

async fn run_probe(caps: &str, rest: Vec<Step>, content: &ActionContent) -> (Probe, Vec<String>) {
    let (stream, fake) = fake::serve(GREETING, steps(caps, rest));
    let mut effects = effects_of(stream);
    let probe = effects.probe(content).await;
    (probe, seen(effects, fake).await)
}

fn checked(flags: &str) -> Vec<Step> {
    vec![select(UIDVALIDITY), fetch_step(UID, flags), logout()]
}

async fn reader_identity() -> (String, String) {
    let fetch = format!(
        "UID FETCH {UID} (UID INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE \
         BODY.PEEK[HEADER.FIELDS ({})])",
        super::super::HEADER_FIELDS.join(" ")
    );
    let script = steps(
        READ_CAPS,
        vec![
            examine_mailbox("INBOX", UIDVALIDITY),
            command(&fetch, &fetch_reply(UID, "")),
        ],
    );
    let (stream, fake) = fake::serve(GREETING, script);
    let mut source = super::super::ImapSource::start(stream, &config())
        .await
        .expect("the reader signs in");
    let meta = source
        .metadata(&[SourceRef::Imap {
            mailbox: "INBOX".to_owned(),
            uidvalidity: UIDVALIDITY,
            uid: UID,
        }])
        .await
        .expect("metadata");
    assert_eq!(meta.len(), 1, "the reader returns the fetched message");
    let identity = meta[0].identity.clone();
    let locator = meta[0].locator.clone();
    drop(source);
    fake.finish().await;
    (identity, locator)
}

#[tokio::test]
async fn marking_read_selects_checks_the_message_and_stores_seen() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            command("UID STORE 42 +FLAGS.SILENT (\\Seen)", "{tag} OK stored\r\n"),
            logout(),
        ],
        mark_read(&identity, &locator),
        None,
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::Applied));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "UID STORE 42 +FLAGS.SILENT (\\Seen)",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn marking_read_does_nothing_when_the_message_is_already_seen() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        checked("\\Seen"),
        mark_read(&identity, &locator),
        None,
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::AlreadyDone));
    assert_eq!(
        commands,
        expect(&["SELECT \"INBOX\"", &fetch_line(UID), "LOGOUT"])
    );
}

#[tokio::test]
async fn marking_read_writes_nothing_when_the_identity_differs() {
    let (_, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        checked(""),
        mark_read("not-the-approved-message", &locator),
        None,
    )
    .await;
    assert_eq!(
        execution,
        not_applied(false, OutcomeCode::Mismatch),
        "a different message is not marked"
    );
    assert_eq!(
        commands,
        expect(&["SELECT \"INBOX\"", &fetch_line(UID), "LOGOUT"])
    );
}

#[tokio::test]
async fn marking_read_writes_nothing_when_uidvalidity_changed() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        vec![select(UIDVALIDITY + 1), logout()],
        mark_read(&identity, &locator),
        None,
    )
    .await;
    assert_eq!(execution, not_applied(false, OutcomeCode::Mismatch));
    assert_eq!(commands, expect(&["SELECT \"INBOX\"", "LOGOUT"]));
}

#[tokio::test]
async fn marking_read_reports_gone_when_the_fetch_is_empty() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        vec![select(UIDVALIDITY), empty_fetch(UID), logout()],
        mark_read(&identity, &locator),
        None,
    )
    .await;
    assert_eq!(execution, not_applied(false, OutcomeCode::Gone));
    assert_eq!(
        commands,
        expect(&["SELECT \"INBOX\"", &fetch_line(UID), "LOGOUT"])
    );
}

#[tokio::test]
async fn marking_read_reports_gone_when_select_is_refused() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        vec![
            command(
                "SELECT \"INBOX\"",
                "{tag} NO [NONEXISTENT] no such mailbox\r\n",
            ),
            logout(),
        ],
        mark_read(&identity, &locator),
        None,
    )
    .await;
    assert_eq!(execution, not_applied(false, OutcomeCode::Gone));
    assert_eq!(commands, expect(&["SELECT \"INBOX\"", "LOGOUT"]));
}

#[tokio::test]
async fn a_move_with_move_lists_the_destination_and_moves_the_uid() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        MOVE_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(WITH_TRASH),
            command("UID MOVE 42 \"Trash\"", "{tag} OK moved\r\n"),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        None,
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::Applied));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "UID MOVE 42 \"Trash\"",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_move_is_unsupported_when_the_destination_is_not_listed() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        MOVE_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(NO_TRASH),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        None,
    )
    .await;
    assert_eq!(execution, not_applied(false, OutcomeCode::Unsupported));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_move_answered_trycreate_or_nonexistent_is_not_retried() {
    let (identity, locator) = stored();
    for code in ["TRYCREATE", "NONEXISTENT"] {
        let (execution, commands) = run_change(
            MOVE_CAPS,
            vec![
                select(UIDVALIDITY),
                fetch_step(UID, ""),
                list(WITH_TRASH),
                command(
                    "UID MOVE 42 \"Trash\"",
                    &format!("{{tag}} NO [{code}] missing\r\n"),
                ),
                logout(),
            ],
            trash(&identity, &locator, Some(MESSAGE_ID)),
            None,
        )
        .await;
        assert_eq!(
            execution,
            not_applied(false, OutcomeCode::Unsupported),
            "{code}"
        );
        assert_eq!(
            commands,
            expect(&[
                "SELECT \"INBOX\"",
                &fetch_line(UID),
                "LIST \"\" \"*\"",
                "UID MOVE 42 \"Trash\"",
                "LOGOUT",
            ])
        );
    }
}

#[tokio::test]
async fn a_move_answered_with_a_plain_no_can_be_retried() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        MOVE_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(WITH_TRASH),
            command("UID MOVE 42 \"Trash\"", "{tag} NO [LIMIT] later\r\n"),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        None,
    )
    .await;
    assert_eq!(execution, not_applied(true, OutcomeCode::Refused));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "UID MOVE 42 \"Trash\"",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_move_without_move_copies_deletes_and_expunges_the_one_uid() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        UIDPLUS_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(WITH_TRASH),
            command("UID COPY 42 \"Trash\"", "{tag} OK copied\r\n"),
            command(
                "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
                "{tag} OK stored\r\n",
            ),
            command("UID EXPUNGE 42", "{tag} OK expunged\r\n"),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        None,
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::Applied));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "UID COPY 42 \"Trash\"",
            "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 42",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_failure_after_the_copy_is_ambiguous() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        UIDPLUS_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(WITH_TRASH),
            command("UID COPY 42 \"Trash\"", "{tag} OK copied\r\n"),
            command(
                "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
                "{tag} NO [UNAVAILABLE] later\r\n",
            ),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        None,
    )
    .await;
    assert_eq!(execution, Execution::Ambiguous);
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "UID COPY 42 \"Trash\"",
            "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
            "LOGOUT",
        ]),
        "the original is not expunged when deleting it did not apply"
    );
}

#[tokio::test]
async fn a_move_is_unsupported_without_move_or_uidplus() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        READ_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(WITH_TRASH),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        None,
    )
    .await;
    assert_eq!(execution, not_applied(false, OutcomeCode::Unsupported));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn resuming_after_a_copy_stores_deleted_and_expunges() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        BOTH_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            list(WITH_TRASH),
            command(
                "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
                "{tag} OK stored\r\n",
            ),
            command("UID EXPUNGE 42", "{tag} OK expunged\r\n"),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        Some(Resume::AfterCopy),
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::Applied));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 42",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn resuming_at_expunge_expunges_the_one_uid() {
    let (identity, locator) = stored();
    let (execution, commands) = run_change(
        BOTH_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, "\\Deleted"),
            list(WITH_TRASH),
            command("UID EXPUNGE 42", "{tag} OK expunged\r\n"),
            logout(),
        ],
        trash(&identity, &locator, Some(MESSAGE_ID)),
        Some(Resume::Expunge),
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::Applied));
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "LIST \"\" \"*\"",
            "UID EXPUNGE 42",
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_draft_is_appended_when_its_message_id_is_absent() {
    let message = outgoing_bytes(DRAFT_ID);
    let (execution, commands) = run_draft(
        vec![
            examine_mailbox("Drafts", 3),
            search_step(DRAFT_ID, &[]),
            command(
                &append_line("Drafts", "(\\Draft \\Seen)", &message),
                "{tag} OK appended\r\n",
            ),
            logout(),
        ],
        &message,
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::Applied));
    assert_eq!(
        commands,
        expect(&[
            "EXAMINE \"Drafts\"",
            &search_line(DRAFT_ID),
            &append_line("Drafts", "(\\Draft \\Seen)", &message),
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_draft_already_there_is_not_appended_again() {
    let message = outgoing_bytes(DRAFT_ID);
    let (execution, commands) = run_draft(
        vec![
            examine_mailbox("Drafts", 3),
            search_step(DRAFT_ID, &[9]),
            logout(),
        ],
        &message,
    )
    .await;
    assert_eq!(execution, applied(OutcomeCode::AlreadyDone));
    assert_eq!(
        commands,
        expect(&["EXAMINE \"Drafts\"", &search_line(DRAFT_ID), "LOGOUT",])
    );
}

#[tokio::test(start_paused = true)]
async fn a_dropped_connection_during_append_is_ambiguous() {
    let message = outgoing_bytes(DRAFT_ID);
    let approved = approve(draft_content(), None);
    let (stream, fake) = fake::serve_writable(
        GREETING,
        steps(
            READ_CAPS,
            vec![
                examine_mailbox("Drafts", 3),
                search_step(DRAFT_ID, &[]),
                Step::Hang,
            ],
        ),
    );
    let mut effects = effects_of(stream);
    let execution = effects.save_draft(&approved, message.as_bytes()).await;
    let commands = seen(effects, fake).await;
    assert_eq!(execution, Execution::Ambiguous);
    assert_eq!(
        commands,
        expect(&[
            "EXAMINE \"Drafts\"",
            &search_line(DRAFT_ID),
            &append_line("Drafts", "(\\Draft \\Seen)", &message),
        ])
    );
}

#[tokio::test]
async fn copy_sent_appends_a_seen_copy_to_the_sent_folder() {
    let message = outgoing_bytes(SENT_ID);
    let approved = approve(send_content(), None);
    let (stream, fake) = fake::serve_writable(
        GREETING,
        steps(
            READ_CAPS,
            vec![
                examine_mailbox("Sent", 4),
                search_step(SENT_ID, &[]),
                command(
                    &append_line("Sent", "(\\Seen)", &message),
                    "{tag} OK appended\r\n",
                ),
                logout(),
            ],
        ),
    );
    let mut effects = effects_of(stream);
    assert!(effects.copy_sent(&approved, message.as_bytes()).await);
    assert_eq!(
        seen(effects, fake).await,
        expect(&[
            "EXAMINE \"Sent\"",
            &search_line(SENT_ID),
            &append_line("Sent", "(\\Seen)", &message),
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_probe_finds_a_draft_that_was_appended() {
    let content = draft_content();
    let (probe, commands) = run_probe(
        READ_CAPS,
        vec![
            examine_mailbox("Drafts", 3),
            search_step(DRAFT_ID, &[9]),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::Done);
    assert_eq!(
        commands,
        expect(&["EXAMINE \"Drafts\"", &search_line(DRAFT_ID), "LOGOUT",])
    );
}

#[tokio::test]
async fn a_probe_reports_a_missing_draft_as_not_done() {
    let content = draft_content();
    let (probe, _) = run_probe(
        READ_CAPS,
        vec![
            examine_mailbox("Drafts", 3),
            search_step(DRAFT_ID, &[]),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::NotDone);
}

#[tokio::test]
async fn a_probe_never_reports_a_missing_send_as_not_done() {
    let content = send_content();
    let (probe, commands) = run_probe(
        READ_CAPS,
        vec![
            examine_mailbox("Sent", 4),
            search_step(SENT_ID, &[]),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::Unknown);
    assert_eq!(
        commands,
        expect(&["EXAMINE \"Sent\"", &search_line(SENT_ID), "LOGOUT"])
    );
}

#[tokio::test]
async fn a_probe_sees_a_message_marked_read() {
    let (identity, locator) = stored();
    let content = mark_read(&identity, &locator);
    let (probe, commands) = run_probe(
        READ_CAPS,
        vec![
            examine_mailbox("INBOX", UIDVALIDITY),
            fetch_step(UID, "\\Seen"),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::Done);
    assert_eq!(
        commands,
        expect(&["EXAMINE \"INBOX\"", &fetch_line(UID), "LOGOUT"])
    );
}

#[tokio::test]
async fn a_probe_reports_an_unread_message_as_not_done() {
    let (identity, locator) = stored();
    let content = mark_read(&identity, &locator);
    let (probe, _) = run_probe(
        READ_CAPS,
        vec![
            examine_mailbox("INBOX", UIDVALIDITY),
            fetch_step(UID, ""),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::NotDone);
}

fn moved_away(hits: &[u32]) -> Vec<Step> {
    let mut steps = vec![examine_mailbox("INBOX", UIDVALIDITY), empty_fetch(UID)];
    steps.extend(destination(hits));
    steps
}

fn destination(hits: &[u32]) -> Vec<Step> {
    let mut steps = vec![examine_mailbox("Trash", 9), search_step(MESSAGE_ID, hits)];
    if !hits.is_empty() {
        steps.push(fetch_step(hits[0], ""));
    }
    steps.push(logout());
    steps
}

#[tokio::test]
async fn a_probe_finds_a_moved_message_in_the_destination() {
    let (identity, locator) = stored();
    let content = trash(&identity, &locator, Some(MESSAGE_ID));
    let (probe, commands) = run_probe(READ_CAPS, moved_away(&[9]), &content).await;
    assert_eq!(probe, Probe::Done);
    assert_eq!(
        commands,
        expect(&[
            "EXAMINE \"INBOX\"",
            &fetch_line(UID),
            "EXAMINE \"Trash\"",
            &search_line(MESSAGE_ID),
            &fetch_line(9),
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_probe_resumes_after_the_copy_when_the_original_remains() {
    let (identity, locator) = stored();
    let content = trash(&identity, &locator, Some(MESSAGE_ID));
    let mut steps = vec![examine_mailbox("INBOX", UIDVALIDITY), fetch_step(UID, "")];
    steps.extend(destination(&[9]));
    let (probe, _) = run_probe(READ_CAPS, steps, &content).await;
    assert_eq!(probe, Probe::Resume(Resume::AfterCopy));
}

#[tokio::test]
async fn a_probe_resumes_at_expunge_when_the_original_is_deleted() {
    let (identity, locator) = stored();
    let content = trash(&identity, &locator, Some(MESSAGE_ID));
    let mut steps = vec![
        examine_mailbox("INBOX", UIDVALIDITY),
        fetch_step(UID, "\\Deleted"),
    ];
    steps.extend(destination(&[9]));
    let (probe, _) = run_probe(READ_CAPS, steps, &content).await;
    assert_eq!(probe, Probe::Resume(Resume::Expunge));
}

#[tokio::test]
async fn a_probe_reports_gone_when_the_message_is_in_neither_folder() {
    let (identity, locator) = stored();
    let content = trash(&identity, &locator, Some(MESSAGE_ID));
    let (probe, commands) = run_probe(READ_CAPS, moved_away(&[]), &content).await;
    assert_eq!(probe, Probe::Gone);
    assert_eq!(
        commands,
        expect(&[
            "EXAMINE \"INBOX\"",
            &fetch_line(UID),
            "EXAMINE \"Trash\"",
            &search_line(MESSAGE_ID),
            "LOGOUT",
        ])
    );
}

#[tokio::test]
async fn a_probe_without_a_message_id_is_not_done_when_the_server_can_move() {
    let (identity, locator) = stored();
    let content = trash(&identity, &locator, None);
    let (probe, commands) = run_probe(
        MOVE_CAPS,
        vec![
            examine_mailbox("INBOX", UIDVALIDITY),
            fetch_step(UID, ""),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::NotDone);
    assert_eq!(
        commands,
        expect(&["EXAMINE \"INBOX\"", &fetch_line(UID), "LOGOUT"]),
        "the destination is not searched without a message id"
    );
}

#[tokio::test]
async fn a_probe_without_a_message_id_is_unknown_when_the_server_cannot_move() {
    let (identity, locator) = stored();
    let content = trash(&identity, &locator, None);
    let (probe, _) = run_probe(
        READ_CAPS,
        vec![
            examine_mailbox("INBOX", UIDVALIDITY),
            fetch_step(UID, ""),
            logout(),
        ],
        &content,
    )
    .await;
    assert_eq!(probe, Probe::Unknown);
}

#[tokio::test]
async fn the_identity_and_locator_agree_with_what_the_reader_computes() {
    let fetched = parsed_fetch("");
    let (identity, locator) = names("INBOX", &fetched);
    let (expected_identity, expected_locator) = stored();
    assert_eq!(identity, expected_identity, "names() is parse::identity");
    assert_eq!(locator, expected_locator, "names() is parse::locator");
    assert_ne!(identity, locator, "the locator is not tied to the mailbox");
    let (read_identity, read_locator) = reader_identity().await;
    assert_eq!(read_identity, identity);
    assert_eq!(read_locator, locator);
    let (execution, commands) = run_change(
        READ_CAPS,
        vec![
            select(UIDVALIDITY),
            fetch_step(UID, ""),
            command("UID STORE 42 +FLAGS.SILENT (\\Seen)", "{tag} OK stored\r\n"),
            logout(),
        ],
        mark_read(&identity, &locator),
        None,
    )
    .await;
    assert_eq!(
        execution,
        applied(OutcomeCode::Applied),
        "the writer accepts the reader's identity"
    );
    assert_eq!(
        commands,
        expect(&[
            "SELECT \"INBOX\"",
            &fetch_line(UID),
            "UID STORE 42 +FLAGS.SILENT (\\Seen)",
            "LOGOUT",
        ])
    );
}
