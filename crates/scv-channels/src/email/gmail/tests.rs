//! Unit tests for `src/email/gmail.rs`.

use super::*;
use crate::email::content::{
    ActionContent, CONTENT_VERSION, Display, Folder, FolderRole, Form, Mailbox, Origin, Outgoing,
    Source,
};
use crate::email::credentials::GrantKind;
use crate::email::oauth::OAuthProvider;
use crate::email::settings::SentCopy;
use crate::email::source::Address;
use crate::email::test_support::{FakeHttp, HttpRequest, TOKEN_PATH, token_answer, tokens};
use std::sync::Arc;

const MSG: &str = "msg-1";
const THREAD: &str = "thr-1";
const OUT_ID: &str = "<reply-1@example.com>";
const RAW: &[u8] = b"From: me@example.com\r\nMessage-ID: <reply-1@example.com>\r\n\r\nHello.";

/// A local Gmail API. The sender slot holds the writer grant, as a Gmail
/// account's send scope does.
struct GmailBox {
    fake: FakeHttp,
    home: tempfile::TempDir,
}

impl GmailBox {
    async fn open(answer: impl Fn(&HttpRequest) -> (u16, String) + Send + Sync + 'static) -> Self {
        let fake = FakeHttp::start(move |request| {
            token_answer(request).unwrap_or_else(|| answer(request))
        })
        .await;
        Self {
            fake,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn tokens(&self, kind: GrantKind) -> Arc<super::super::oauth::TokenSource> {
        tokens(
            self.home.path(),
            &self.fake.origin,
            OAuthProvider::Gmail,
            kind,
        )
    }

    fn source(&self, label: &str) -> GmailSource {
        GmailSource::new(
            Api::new(
                Flavor::Gmail,
                self.fake.origin.clone(),
                self.tokens(GrantKind::Reader),
                Mode::Read,
            )
            .unwrap(),
            label,
        )
        .unwrap()
    }

    fn effects(&self, writer: bool, sender: bool) -> GmailEffects {
        GmailEffects::new(EffectParts {
            origin: self.fake.origin.clone(),
            reader: self.tokens(GrantKind::Reader),
            writer: writer.then(|| self.tokens(GrantKind::Writer)),
            sender: sender.then(|| self.tokens(GrantKind::Writer)),
        })
    }

    /// API calls, in order, without the token refreshes.
    fn calls(&self) -> Vec<HttpRequest> {
        self.fake
            .requests()
            .into_iter()
            .filter(|request| request.path != TOKEN_PATH)
            .collect()
    }
}

fn assert_token(request: &HttpRequest, token: &str) {
    assert_eq!(
        request.header("authorization"),
        Some(token),
        "{} {} used the wrong grant",
        request.method,
        request.path
    );
}

fn assert_no_secret(text: &str) {
    for secret in [
        "at-reader",
        "at-writer",
        "at-sender",
        "rt-reader",
        "rt-writer",
        "SECRET-PHRASE",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
}

fn gmail_ref(id: &str) -> SourceRef {
    SourceRef::Gmail { id: id.to_owned() }
}

fn cursor(history: &str) -> Cursor {
    Cursor {
        provider: ProviderKind::Gmail,
        value: history.to_owned(),
    }
}

/// An approved action on `id`. Outgoing mail carries `message_id`.
fn approved(kind: ActionKind, id: &str, message_id: Option<&str>) -> Approved {
    let folder = match kind {
        ActionKind::Trash => Some(("TRASH", FolderRole::Trash)),
        ActionKind::Spam => Some(("SPAM", FolderRole::Junk)),
        ActionKind::Archive => Some(("ARCHIVE", FolderRole::Archive)),
        ActionKind::Draft => Some(("DRAFT", FolderRole::Drafts)),
        ActionKind::Send => Some(("SENT", FolderRole::Sent)),
        ActionKind::MarkRead => None,
    };
    let content = ActionContent {
        v: CONTENT_VERSION,
        id: format!("a{}", "0123456789abcdef".repeat(2)),
        account: "default".into(),
        fingerprint: "fingerprint".into(),
        kind,
        group: None,
        origin: Origin::Owner {
            route: "feishu:mail".into(),
            message_id: "om_request".into(),
        },
        source: Some(Source {
            reference: gmail_ref(id),
            identity: parse::api_identity("gmail", id),
            locator: parse::api_identity("gmail", id),
            message_id: Some("<orig@example.com>".into()),
        }),
        display: Some(Display {
            handle: "4K7P".into(),
            from_address: "ada@example.com".into(),
            from_name: "Ada".into(),
            subject: "Hello".into(),
        }),
        folder: folder.map(|(name, role)| Folder {
            role,
            name: name.into(),
        }),
        message: message_id.map(|message_id| Outgoing {
            form: Form::Reply,
            from: Mailbox {
                name: String::new(),
                address: "me@example.com".into(),
            },
            to: vec!["ada@example.com".into()],
            cc: Vec::new(),
            subject: "Re: Hello".into(),
            body: "Thanks.".into(),
            in_reply_to: Some("<orig@example.com>".into()),
            references: vec!["<orig@example.com>".into()],
            message_id: message_id.into(),
            sent_copy: SentCopy::Provider,
            notes: Vec::new(),
        }),
        created_at: 1_728_032_400,
        hard_expiry: 1_728_032_400 + 86_400,
        digest: String::new(),
    }
    .seal_digest();
    Approved::for_tests(content, 1, None)
}

fn part(id: &str, mime: &str, charset: Option<&str>) -> PartRef {
    PartRef {
        id: id.into(),
        mime: mime.into(),
        charset: charset.map(str::to_owned),
        encoding: TransferEncoding::Binary,
        size: 5,
    }
}

/// `format=full` for one message: encoded headers, plain before HTML, an
/// attachment named with its size.
fn full_message() -> serde_json::Value {
    json!({
        "id": MSG,
        "threadId": THREAD,
        "labelIds": ["INBOX", "UNREAD", "CATEGORY_PROMOTIONS"],
        "internalDate": "1728032400123",
        "sizeEstimate": 4096,
        "payload": {
            "mimeType": "multipart/mixed",
            "headers": [
                {"name": "From", "value": "=?UTF-8?Q?Ada_Lovelace?= <ada@example.com>"},
                {"name": "Reply-To", "value": "Billing <lists@example.com>"},
                {"name": "To", "value": "Me <me@example.com>, Bob <bob@example.org>"},
                {"name": "Cc", "value": "\"Lovelace, Ada\" <ada-cc@example.com>"},
                {"name": "Subject", "value": "=?UTF-8?Q?Caf=C3=A9_notes?="},
                {"name": "Message-ID", "value": "<m1@example.com>"},
                {"name": "References", "value": "<a@example.com> <b@example.com>"},
                {"name": "List-Id", "value": "=?UTF-8?Q?Caf=C3=A9?= <list.example.com>"},
                {"name": "List-Unsubscribe", "value": "<mailto:u@example.com>"},
                {"name": "Precedence", "value": " Bulk "},
                {"name": "Auto-Submitted", "value": "Auto-Generated"},
                {"name": "Return-Path", "value": "< >"},
                {"name": "Date", "value": "Fri, 4 Oct 2024 09:00:00 +0000"}
            ],
            "parts": [
                {
                    "mimeType": "multipart/alternative",
                    "parts": [
                        {
                            "partId": "0.0",
                            "mimeType": "text/html",
                            "headers": [{"name": "Content-Type", "value": "text/html; charset=utf-8"}],
                            "body": {"size": 20, "data": "PGh0bWw-"}
                        },
                        {
                            "partId": "0.1",
                            "mimeType": "text/plain",
                            "headers": [{"name": "Content-Type", "value": "text/plain; charset=\"ISO-8859-1\""}],
                            "body": {"size": 4, "data": "Y2Fm6Q"}
                        }
                    ]
                },
                {
                    "partId": "1",
                    "mimeType": "text/plain",
                    "filename": "readme.txt",
                    "body": {"size": 9, "attachmentId": "att-readme"}
                },
                {
                    "partId": "2",
                    "mimeType": "application/pdf",
                    "filename": "notes.pdf",
                    "body": {"size": 5000, "attachmentId": "att-notes"}
                }
            ]
        }
    })
}

#[tokio::test]
async fn a_source_accepts_only_a_label_id_and_names_gmail_folders() {
    let home = tempfile::tempdir().unwrap();
    let origin = "http://127.0.0.1:9";
    let api = || {
        Api::new(
            Flavor::Gmail,
            origin.into(),
            tokens(home.path(), origin, OAuthProvider::Gmail, GrantKind::Reader),
            Mode::Read,
        )
        .unwrap()
    };
    for label in ["INBOX", " INBOX ", "CATEGORY_PROMOTIONS", "Label_1-a"] {
        let source = GmailSource::new(api(), label).unwrap();
        assert_eq!(source.label, label.trim(), "{label}");
    }
    for label in [
        "INBOX/Secret",
        "has space",
        "café",
        "label.name",
        "..",
        "a?b",
        "",
        "   ",
    ] {
        assert!(
            GmailSource::new(api(), label).is_err(),
            "{label} is not a label id"
        );
    }
    let mut source = GmailSource::new(api(), "INBOX").unwrap();
    assert_eq!(
        source.folders(&FolderNames::default()).await.unwrap(),
        gmail_folders()
    );
    assert_eq!(
        source.caps(),
        Caps {
            find_by_message_id: true,
            sent_autofile: true,
            can_move: true,
            ..Caps::default()
        }
    );
}

#[test]
fn meta_of_reads_headers_prefers_plain_text_and_bounds_inflated_fields() {
    let meta = meta_of(MSG, &full_message());
    let identity = parse::api_identity("gmail", MSG);
    assert_eq!(meta.source, gmail_ref(MSG));
    assert_eq!(meta.identity, identity);
    assert_eq!(meta.locator, identity);
    assert_eq!(
        meta.received_at, 1_728_032_400,
        "internalDate ms become seconds"
    );
    assert_eq!(meta.size, 4096);
    assert_eq!(
        meta.from,
        Some(Address {
            name: "Ada Lovelace".into(),
            address: "ada@example.com".into(),
        })
    );
    assert_eq!(
        meta.reply_to,
        Some(Address {
            name: "Billing".into(),
            address: "lists@example.com".into(),
        })
    );
    assert_eq!(
        meta.to,
        vec![
            Address {
                name: "Me".into(),
                address: "me@example.com".into(),
            },
            Address {
                name: "Bob".into(),
                address: "bob@example.org".into(),
            },
        ]
    );
    assert_eq!(
        meta.cc,
        vec![Address {
            name: "Lovelace, Ada".into(),
            address: "ada-cc@example.com".into(),
        }]
    );
    assert_eq!(meta.subject, "Café notes");
    assert_eq!(meta.message_id.as_deref(), Some("<m1@example.com>"));
    assert_eq!(
        meta.references,
        vec!["<a@example.com>".to_owned(), "<b@example.com>".to_owned()]
    );
    assert_eq!(meta.date.as_deref(), Some("Fri, 4 Oct 2024 09:00:00 +0000"));
    assert_eq!(
        meta.signals.list_id.as_deref(),
        Some("Café <list.example.com>")
    );
    assert!(meta.signals.list_unsubscribe);
    assert_eq!(meta.signals.precedence.as_deref(), Some("bulk"));
    assert_eq!(
        meta.signals.auto_submitted.as_deref(),
        Some("auto-generated")
    );
    assert!(meta.signals.null_return_path);
    assert_eq!(meta.category.as_deref(), Some("CATEGORY_PROMOTIONS"));
    let text = meta.text.unwrap();
    assert_eq!(
        text.id, "0.1",
        "plain is preferred over the earlier html part"
    );
    assert_eq!(text.mime, "text/plain");
    assert_eq!(text.charset.as_deref(), Some("iso-8859-1"));
    assert_eq!(text.encoding, TransferEncoding::Binary);
    assert_eq!(text.size, 4);
    assert_eq!(meta.attachments.len(), 2);
    assert_eq!(meta.attachments[0].name, "readme.txt");
    assert_eq!(meta.attachments[0].mime, "text/plain");
    assert_eq!(meta.attachments[0].size, 9);
    assert_eq!(meta.attachments[1].name, "notes.pdf");
    assert_eq!(meta.attachments[1].mime, "application/pdf");
    assert_eq!(meta.attachments[1].size, 5000);

    let mut parts = Vec::new();
    for n in 0..20 {
        parts.push(json!({
            "partId": format!("{n}"),
            "mimeType": "application/octet-stream",
            "filename": "N".repeat(400),
            "body": {"size": 9, "attachmentId": format!("att{n}")}
        }));
    }
    let references = (0..12)
        .map(|n| format!("<m{n}@example.com>"))
        .collect::<Vec<_>>()
        .join(" ");
    let tos = (0..70)
        .map(|n| format!("Person {n} <p{n}@example.com>"))
        .collect::<Vec<_>>()
        .join(", ");
    let inflated = meta_of(
        MSG,
        &json!({
            "labelIds": ["CATEGORY_PROMOTIONS"],
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    {"name": "Subject", "value": "S".repeat(2000)},
                    {"name": "From", "value": format!("{} <ada@example.com>", "N".repeat(400))},
                    {"name": "To", "value": tos},
                    {"name": "References", "value": references},
                    {"name": "List-Id", "value": "L".repeat(400)}
                ],
                "parts": parts
            }
        }),
    );
    assert_eq!(inflated.subject.len(), 1024);
    assert!(inflated.subject.ends_with("…"));
    assert_eq!(inflated.from.unwrap().name.len(), 256);
    assert_eq!(inflated.to.len(), 64);
    assert_eq!(inflated.to[0].address, "p0@example.com");
    assert_eq!(inflated.to[63].address, "p63@example.com");
    assert_eq!(
        inflated.references,
        (2..12)
            .map(|n| format!("<m{n}@example.com>"))
            .collect::<Vec<_>>()
    );
    assert_eq!(inflated.signals.list_id.unwrap().len(), 256);
    assert_eq!(inflated.attachments.len(), 16);
    assert_eq!(inflated.attachments[0].name.len(), 256);
    assert!(inflated.attachments[0].name.ends_with("…"));
}

#[tokio::test]
async fn the_first_check_stores_the_profile_history_id_and_lists_nothing() {
    let mailbox = GmailBox::open(|request| {
        assert_eq!(request.path, "/gmail/v1/users/me/profile");
        (
            200,
            r#"{"historyId": 900, "emailAddress": "SECRET-PHRASE"}"#.into(),
        )
    })
    .await;
    let mut source = mailbox.source("INBOX");
    for cursor in [
        None,
        Some(Cursor {
            provider: ProviderKind::Imap,
            value: "1".into(),
        }),
        Some(Cursor {
            provider: ProviderKind::Graph,
            value: "{}".into(),
        }),
    ] {
        let changes = source.changes(cursor.as_ref(), 10, 86_400).await.unwrap();
        let Changes::New { refs, next } = changes else {
            panic!("the first check is not a resync");
        };
        assert!(refs.is_empty());
        assert_eq!(next.provider, ProviderKind::Gmail);
        assert_eq!(next.value, "900");
    }
    assert!(
        mailbox
            .calls()
            .iter()
            .all(|call| call.path.ends_with("/profile"))
    );
    for call in mailbox.calls() {
        assert_token(&call, "Bearer at-reader");
    }
}

#[tokio::test]
async fn history_keeps_the_watched_label_drops_duplicates_and_hostile_ids_and_follows_pages() {
    let mailbox = GmailBox::open(|request| {
        assert_eq!(request.path, "/gmail/v1/users/me/history");
        assert_eq!(request.query("historyTypes"), Some("messageAdded"));
        assert_eq!(request.query("labelId"), Some("INBOX"));
        let start = request.query("startHistoryId");
        let page = request.query("pageToken");
        if start == Some("1") && page.is_none() {
            assert_eq!(request.query("maxResults"), Some("10"));
            return (
                200,
                json!({
                    "history": [{
                        "id": "10",
                        "messagesAdded": [
                            {"message": {"id": "m1", "labelIds": ["INBOX"]}},
                            {"message": {"id": "m1", "labelIds": ["INBOX"]}},
                            {"message": {"id": "../secret", "labelIds": ["INBOX"]}},
                            {"message": {"id": "other", "labelIds": ["SPAM"]}}
                        ]
                    }],
                    "nextPageToken": "page-2",
                    "historyId": "10"
                })
                .to_string(),
            );
        }
        if start == Some("1") && page == Some("page-2") {
            return (
                200,
                json!({
                    "history": [{
                        "id": "11",
                        "messagesAdded": [
                            {"message": {"id": "m1", "labelIds": ["INBOX"]}},
                            {"message": {"id": "m2", "labelIds": ["INBOX", "UNREAD"]}}
                        ]
                    }],
                    "historyId": "50"
                })
                .to_string(),
            );
        }
        if start == Some("2") {
            assert_eq!(request.query("maxResults"), Some("500"));
            return (
                200,
                json!({
                    "history": [
                        {"id": "8", "messagesAdded": [{"message": {"id": "m8", "labelIds": ["INBOX"]}}]},
                        {"id": "9", "messagesAdded": [{"message": {"id": "m9", "labelIds": ["INBOX"]}}]}
                    ]
                })
                .to_string(),
            );
        }
        (500, r#"{"error":"SECRET-PHRASE at-reader"}"#.into())
    })
    .await;
    let mut source = mailbox.source("INBOX");
    let changes = source
        .changes(Some(&cursor("1")), 10, 86_400)
        .await
        .unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("history is not a resync");
    };
    assert_eq!(refs, vec![gmail_ref("m1"), gmail_ref("m2")]);
    assert_eq!(
        next.value, "50",
        "a finished history uses the answer's historyId"
    );
    let changes = source
        .changes(Some(&cursor("2")), 10_000, 86_400)
        .await
        .unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("history is not a resync");
    };
    assert_eq!(refs, vec![gmail_ref("m8"), gmail_ref("m9")]);
    assert_eq!(
        next.value, "9",
        "with no historyId the last record is the cursor"
    );
    assert!(
        mailbox
            .calls()
            .iter()
            .all(|call| !call.path.contains("../secret"))
    );
    for call in mailbox.calls() {
        assert_token(&call, "Bearer at-reader");
    }
}

#[tokio::test]
async fn a_short_limit_stops_after_a_full_record_and_the_next_check_continues() {
    let mailbox = GmailBox::open(|request| {
        let start = request.query("startHistoryId");
        assert_eq!(request.query("maxResults"), Some("1"));
        if start == Some("7") {
            return (
                200,
                json!({
                    "history": [
                        {"id": "10", "messagesAdded": [{"message": {"id": "m1", "labelIds": ["INBOX"]}}]},
                        {"id": "11", "messagesAdded": [{"message": {"id": "m2", "labelIds": ["INBOX"]}}]}
                    ],
                    "historyId": "999"
                })
                .to_string(),
            );
        }
        if start == Some("10") {
            return (
                200,
                json!({
                    "history": [
                        {"id": "11", "messagesAdded": [{"message": {"id": "m2", "labelIds": ["INBOX"]}}]}
                    ],
                    "historyId": "30"
                })
                .to_string(),
            );
        }
        (500, "{}".into())
    })
    .await;
    let mut source = mailbox.source("INBOX");
    let changes = source.changes(Some(&cursor("7")), 1, 86_400).await.unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("expected a partial page");
    };
    assert_eq!(refs, vec![gmail_ref("m1")]);
    assert_eq!(
        next.value, "10",
        "the record that did not fit is left in place"
    );
    let changes = source.changes(Some(&next), 1, 86_400).await.unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("expected the rest");
    };
    assert_eq!(refs, vec![gmail_ref("m2")]);
    assert_eq!(next.value, "30");
}

#[tokio::test]
async fn a_forgotten_history_id_lists_the_window_oldest_first() {
    let mailbox = GmailBox::open(|request| match request.path.as_str() {
        "/gmail/v1/users/me/history" => (404, r#"{"error":"SECRET-PHRASE"}"#.into()),
        "/gmail/v1/users/me/messages" => {
            assert_eq!(request.query("labelIds"), Some("INBOX"));
            assert_eq!(request.query("q"), Some("newer_than:3d"));
            assert_eq!(request.query("maxResults"), Some("2"));
            (
                200,
                json!({
                    "messages": [
                        {"id": "new"},
                        {"id": "old"},
                        {"id": "../secret"}
                    ],
                    "resultSizeEstimate": 5
                })
                .to_string(),
            )
        }
        "/gmail/v1/users/me/profile" => (200, r#"{"historyId": "4242"}"#.into()),
        _ => (500, "{}".into()),
    })
    .await;
    let mut source = mailbox.source("INBOX");
    let changes = source
        .changes(Some(&cursor("1")), 2, 2 * 86_400 + 10)
        .await
        .unwrap();
    let Changes::Reset {
        recent,
        beyond,
        next,
    } = changes
    else {
        panic!("a 404 history is a resync");
    };
    assert_eq!(recent, vec![gmail_ref("old"), gmail_ref("new")]);
    assert_eq!(beyond, 3);
    assert_eq!(next.value, "4242");
    let calls = mailbox.calls();
    let paths: Vec<_> = calls.iter().map(|call| call.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "/gmail/v1/users/me/history",
            "/gmail/v1/users/me/messages",
            "/gmail/v1/users/me/profile",
        ]
    );
    assert!(
        mailbox
            .calls()
            .iter()
            .all(|call| !call.path.contains("secret"))
    );
}

#[tokio::test]
async fn metadata_skips_a_missing_message_and_text_decodes_base64url_with_its_charset() {
    let message = full_message();
    let mailbox = GmailBox::open(move |request| {
        assert_eq!(request.query("format"), Some("full"));
        assert_token(request, "Bearer at-reader");
        match request.path.as_str() {
            "/gmail/v1/users/me/messages/msg-1" => (200, message.to_string()),
            "/gmail/v1/users/me/messages/padded" => (
                200,
                json!({
                    "payload": {
                        "partId": "0",
                        "mimeType": "text/plain",
                        "body": {"data": "Y2Fmw6k=", "size": 5}
                    }
                })
                .to_string(),
            ),
            "/gmail/v1/users/me/messages/latin" => (
                200,
                json!({
                    "payload": {
                        "partId": "0",
                        "mimeType": "text/plain",
                        "headers": [{"name": "Content-Type", "value": "text/plain; charset=iso-8859-1"}],
                        "body": {"data": "Y2Fm6Q", "size": 4}
                    }
                })
                .to_string(),
            ),
            "/gmail/v1/users/me/messages/long" => (
                200,
                json!({
                    "payload": {
                        "partId": "0",
                        "mimeType": "text/html",
                        "body": {"data": "w6nDqQ", "size": 4}
                    }
                })
                .to_string(),
            ),
            "/gmail/v1/users/me/messages/missing" => (404, r#"{"error":"SECRET-PHRASE"}"#.into()),
            "/gmail/v1/users/me/messages/boom" => {
                (500, r#"{"error":"Bearer at-reader saw SECRET-PHRASE"}"#.into())
            }
            other => panic!("unexpected path {other}"),
        }
    })
    .await;
    let mut source = mailbox.source("INBOX");
    let found = source
        .metadata(&[
            gmail_ref("missing"),
            gmail_ref(MSG),
            SourceRef::Imap {
                mailbox: "INBOX".into(),
                uidvalidity: 1,
                uid: 1,
            },
            SourceRef::Graph { id: "g".into() },
        ])
        .await
        .unwrap();
    assert_eq!(found, vec![meta_of(MSG, &full_message())]);
    assert!(
        source
            .text(&gmail_ref("missing"), &part("0", "text/plain", None), 100)
            .await
            .unwrap()
            .is_none()
    );

    let padded = source
        .text(
            &gmail_ref("padded"),
            &part("0", "text/plain", Some("utf-8")),
            100,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(padded.text, "café");
    assert!(!padded.html);
    assert!(!padded.truncated);

    let latin = source
        .metadata(&[gmail_ref("latin")])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        latin.text.as_ref().unwrap().charset.as_deref(),
        Some("iso-8859-1")
    );
    let decoded = source
        .text(&latin.source, latin.text.as_ref().unwrap(), 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decoded.text, "café");
    assert!(!decoded.truncated);

    let cut = source
        .text(
            &gmail_ref("long"),
            &part("0", "text/html", Some("utf-8")),
            3,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cut.text, "é");
    assert!(cut.html);
    assert!(cut.truncated, "the part was longer than the bytes kept");

    let error = source.metadata(&[gmail_ref("boom")]).await.unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains('5'), "{text}");
    assert_no_secret(&text);
}

#[tokio::test]
async fn an_attachment_part_is_fetched_by_id_and_a_hostile_attachment_id_is_not() {
    let mailbox = GmailBox::open(|request| match request.path.as_str() {
        "/gmail/v1/users/me/messages/msg-1" => (
            200,
            json!({
                "payload": {
                    "partId": "2",
                    "mimeType": "text/plain",
                    "body": {"attachmentId": "att-1", "size": 5}
                }
            })
            .to_string(),
        ),
        "/gmail/v1/users/me/messages/msg-1/attachments/att-1" => {
            (200, r#"{"data":"Y2Fmw6k=","size":5}"#.into())
        }
        "/gmail/v1/users/me/messages/bad" => (
            200,
            json!({
                "payload": {
                    "partId": "2",
                    "mimeType": "text/plain",
                    "body": {"attachmentId": "../secret", "size": 5}
                }
            })
            .to_string(),
        ),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let mut source = mailbox.source("INBOX");
    let text = source
        .text(
            &gmail_ref(MSG),
            &part("2", "text/plain", Some("utf-8")),
            100,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text.text, "café");
    assert!(!text.truncated);
    let hostile = source
        .text(&gmail_ref("bad"), &part("2", "text/plain", None), 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hostile.text, "");
    assert!(
        !mailbox
            .calls()
            .iter()
            .any(|call| call.path.contains("secret"))
    );
    assert!(
        mailbox
            .calls()
            .iter()
            .any(|call| call.path.ends_with("/attachments/att-1"))
    );
    for call in mailbox.calls() {
        assert_token(&call, "Bearer at-reader");
    }
}

fn labels_of(labels: &[&str]) -> String {
    json!({ "id": MSG, "labelIds": labels, "threadId": THREAD }).to_string()
}

#[tokio::test]
async fn a_change_uses_the_reader_to_look_and_the_writer_to_apply_the_approved_id() {
    let cases = [
        (
            ActionKind::Trash,
            "/gmail/v1/users/me/messages/msg-1/trash",
            None,
        ),
        (
            ActionKind::Spam,
            "/gmail/v1/users/me/messages/msg-1/modify",
            Some(json!({"addLabelIds": ["SPAM"], "removeLabelIds": ["INBOX"]})),
        ),
        (
            ActionKind::Archive,
            "/gmail/v1/users/me/messages/msg-1/modify",
            Some(json!({"removeLabelIds": ["INBOX"]})),
        ),
        (
            ActionKind::MarkRead,
            "/gmail/v1/users/me/messages/msg-1/modify",
            Some(json!({"removeLabelIds": ["UNREAD"]})),
        ),
    ];
    for (kind, path, body) in cases {
        let mailbox = GmailBox::open(move |request| {
            if request.method == "GET" {
                assert_eq!(request.query("format"), Some("minimal"));
                (200, labels_of(&["INBOX", "UNREAD"]))
            } else {
                (200, r#"{"id":"other-id","snippet":"SECRET-PHRASE"}"#.into())
            }
        })
        .await;
        let mut effects = mailbox.effects(true, true);
        let done = effects.change(&approved(kind, MSG, None)).await;
        assert_eq!(
            done,
            Execution::Applied {
                code: OutcomeCode::Applied,
                sent_copy: None,
            }
        );
        assert_no_secret(&format!("{done:?}"));
        let calls = mailbox.calls();
        assert_eq!(calls.len(), 2, "{kind:?} {calls:?}");
        assert_eq!(calls[0].method, "GET");
        assert_eq!(calls[0].path, "/gmail/v1/users/me/messages/msg-1");
        assert_token(&calls[0], "Bearer at-reader");
        assert_eq!(calls[1].method, "POST");
        assert_eq!(calls[1].path, path);
        assert_token(&calls[1], "Bearer at-writer");
        assert!(!calls[1].path.contains("other-id"));
        match &body {
            None => assert!(calls[1].body.is_empty(), "trash has no body"),
            Some(expected) => assert_eq!(calls[1].json(), *expected),
        }
    }
}

#[tokio::test]
async fn a_change_that_already_happened_or_lost_the_message_does_not_write() {
    let done_labels = [
        (ActionKind::Trash, vec!["TRASH", "INBOX"]),
        (ActionKind::Spam, vec!["SPAM"]),
        (ActionKind::Archive, vec!["CATEGORY_PROMOTIONS"]),
        (ActionKind::MarkRead, vec!["INBOX"]),
    ];
    for (kind, labels) in done_labels {
        let labels = labels.clone();
        let mailbox = GmailBox::open(move |request| {
            assert_eq!(request.method, "GET");
            (200, labels_of(&labels))
        })
        .await;
        let mut effects = mailbox.effects(true, true);
        assert_eq!(
            effects.change(&approved(kind, MSG, None)).await,
            Execution::Applied {
                code: OutcomeCode::AlreadyDone,
                sent_copy: None,
            }
        );
        assert!(mailbox.calls().iter().all(|call| call.method == "GET"));
        assert_token(&mailbox.calls()[0], "Bearer at-reader");
    }
    let mailbox = GmailBox::open(|request| {
        assert_eq!(request.method, "GET");
        (404, r#"{"error":"SECRET-PHRASE"}"#.into())
    })
    .await;
    let mut effects = mailbox.effects(false, false);
    assert_eq!(
        effects
            .change(&approved(ActionKind::Trash, MSG, None))
            .await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Gone,
        }
    );
    assert_eq!(mailbox.calls().len(), 1);
}

#[tokio::test]
async fn a_refused_or_failed_write_is_classified_without_echoing_the_provider_body() {
    let cases = [
        (
            503,
            Execution::NotApplied {
                retry: true,
                code: OutcomeCode::Refused,
            },
        ),
        (
            403,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::AuthFailed,
            },
        ),
        (
            400,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Refused,
            },
        ),
    ];
    for (status, expected) in cases {
        let mailbox = GmailBox::open(move |request| {
            if request.method == "GET" {
                (200, labels_of(&["INBOX", "UNREAD"]))
            } else {
                (
                    status,
                    r#"{"error":"Bearer at-writer saw SECRET-PHRASE"}"#.into(),
                )
            }
        })
        .await;
        let mut effects = mailbox.effects(true, true);
        let done = effects
            .change(&approved(ActionKind::Archive, MSG, None))
            .await;
        assert_eq!(done, expected, "status {status}");
        assert_no_secret(&format!("{done:?}"));
        assert_eq!(
            mailbox.calls().len(),
            2,
            "a failed write is not retried here"
        );
    }
    let mailbox = GmailBox::open(|request| {
        assert_eq!(request.method, "GET");
        (200, labels_of(&["INBOX", "UNREAD"]))
    })
    .await;
    let mut effects = mailbox.effects(false, false);
    assert_eq!(
        effects
            .change(&approved(ActionKind::MarkRead, MSG, None))
            .await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::AuthFailed,
        }
    );
    assert!(mailbox.calls().iter().all(|call| call.method == "GET"));
}

#[tokio::test]
async fn save_draft_searches_with_the_reader_then_posts_base64url_raw_and_the_thread() {
    let found = GmailBox::open(|request| {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.query("q"),
            Some("rfc822msgid:reply-1@example.com in:anywhere")
        );
        assert_token(request, "Bearer at-reader");
        (200, r#"{"messages":[{"id":"already"}]}"#.into())
    })
    .await;
    let mut effects = found.effects(true, true);
    assert_eq!(
        effects
            .save_draft(&approved(ActionKind::Draft, MSG, Some(OUT_ID)), RAW)
            .await,
        Execution::Applied {
            code: OutcomeCode::AlreadyDone,
            sent_copy: None,
        }
    );
    assert_eq!(found.calls().len(), 1);

    let mailbox = GmailBox::open(|request| match request.path.as_str() {
        "/gmail/v1/users/me/messages" => (200, r#"{"messages":[]}"#.into()),
        "/gmail/v1/users/me/messages/msg-1" => (200, labels_of(&["INBOX"])),
        "/gmail/v1/users/me/messages/msg-2" => (200, r#"{"threadId":"../secret"}"#.into()),
        "/gmail/v1/users/me/drafts" => (200, r#"{"id":"draft-9"}"#.into()),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let mut effects = mailbox.effects(true, true);
    let saved = effects
        .save_draft(&approved(ActionKind::Draft, MSG, Some(OUT_ID)), RAW)
        .await;
    assert_eq!(
        saved,
        Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        }
    );
    let saved = effects
        .save_draft(&approved(ActionKind::Draft, "msg-2", Some(OUT_ID)), RAW)
        .await;
    assert_eq!(
        saved,
        Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        }
    );
    let posts: Vec<_> = mailbox
        .calls()
        .into_iter()
        .filter(|call| call.method == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    for post in &posts {
        assert_eq!(post.path, "/gmail/v1/users/me/drafts");
        assert_token(post, "Bearer at-writer");
        assert!(!post.path.contains("other"));
        let body = post.json();
        let raw = body["message"]["raw"].as_str().unwrap();
        assert_eq!(api::from_base64url(raw).unwrap(), RAW);
    }
    assert_eq!(posts[0].json()["message"]["threadId"], json!(THREAD));
    assert!(posts[1].json()["message"].get("threadId").is_none());
    assert!(mailbox.calls().iter().all(|call| {
        call.method == "POST" || call.header("authorization") == Some("Bearer at-reader")
    }));

    let bare = GmailBox::open(|_| (500, r#"{"error":"SECRET-PHRASE"}"#.into())).await;
    let mut effects = bare.effects(true, true);
    assert_eq!(
        effects
            .save_draft(&approved(ActionKind::Draft, MSG, None), RAW)
            .await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Internal,
        }
    );
    assert!(bare.calls().is_empty());
}

#[tokio::test]
async fn send_posts_raw_with_the_writer_grant_and_treats_doubt_as_ambiguous() {
    let mailbox = GmailBox::open(|request| match request.path.as_str() {
        "/gmail/v1/users/me/messages/msg-1" => (200, labels_of(&["INBOX"])),
        "/gmail/v1/users/me/messages/send" => (200, r#"{"id":"sent-9"}"#.into()),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let mut effects = mailbox.effects(true, true);
    assert!(
        effects
            .copy_sent(&approved(ActionKind::Send, MSG, Some(OUT_ID)), RAW)
            .await
    );
    assert!(mailbox.calls().is_empty(), "Gmail files sent mail itself");
    let sent = effects
        .send(&approved(ActionKind::Send, MSG, Some(OUT_ID)), RAW)
        .await;
    assert_eq!(
        sent,
        Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        }
    );
    let posts: Vec<_> = mailbox
        .calls()
        .into_iter()
        .filter(|call| call.method == "POST")
        .collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].path, "/gmail/v1/users/me/messages/send");
    assert_token(&posts[0], "Bearer at-writer");
    assert_eq!(
        api::from_base64url(posts[0].json()["raw"].as_str().unwrap()).unwrap(),
        RAW
    );
    assert_eq!(posts[0].json()["threadId"], json!(THREAD));
    assert_eq!(
        mailbox.calls()[0].header("authorization"),
        Some("Bearer at-reader")
    );

    let cases = [
        (503, Execution::Ambiguous),
        (
            400,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Refused,
            },
        ),
        (0, Execution::Ambiguous),
    ];
    for (status, expected) in cases {
        let mailbox = GmailBox::open(move |request| {
            if request.path.ends_with("/messages/msg-1") {
                (200, labels_of(&["INBOX"]))
            } else {
                (
                    status,
                    r#"{"error":"Bearer at-writer saw SECRET-PHRASE"}"#.into(),
                )
            }
        })
        .await;
        let mut effects = mailbox.effects(true, true);
        let done = effects
            .send(&approved(ActionKind::Send, MSG, Some(OUT_ID)), RAW)
            .await;
        assert_eq!(done, expected, "status {status}");
        assert_no_secret(&format!("{done:?}"));
        assert_eq!(
            mailbox
                .calls()
                .iter()
                .filter(|call| call.method == "POST")
                .count(),
            1
        );
    }

    let mailbox = GmailBox::open(|_| (500, "{}".into())).await;
    let mut effects = mailbox.effects(true, false);
    assert_eq!(
        effects
            .send(&approved(ActionKind::Send, MSG, Some(OUT_ID)), RAW)
            .await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::AuthFailed,
        }
    );
    assert!(
        mailbox.fake.requests().is_empty(),
        "no sender grant means nothing is sent"
    );
}

#[tokio::test]
async fn probes_call_a_missing_send_unknown_and_a_missing_draft_not_done() {
    let search = GmailBox::open(|request| {
        assert_eq!(request.method, "GET");
        assert_token(request, "Bearer at-reader");
        let query = request.query("q").unwrap_or_default();
        assert!(!query.contains("SECRET"));
        if query.contains("in:sent") {
            assert!(query.contains("rfc822msgid:reply-1@example.com"));
            return (200, r#"{"messages":[{"id":"sent-1"}]}"#.into());
        }
        assert!(query.contains("in:anywhere"));
        (200, r#"{"resultSizeEstimate":0}"#.into())
    })
    .await;
    let mut effects = search.effects(true, true);
    let send = approved(ActionKind::Send, MSG, Some(OUT_ID));
    assert_eq!(effects.probe(send.content()).await, Probe::Done);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Draft, MSG, Some(OUT_ID)).content())
            .await,
        Probe::NotDone
    );

    let missed = GmailBox::open(|request| {
        assert!(request.query("q").unwrap().contains("in:sent"));
        (200, r#"{"messages":[]}"#.into())
    })
    .await;
    let mut effects = missed.effects(true, true);
    assert_eq!(
        effects.probe(send.content()).await,
        Probe::Unknown,
        "a send that is not in Sent is unknown, never not done"
    );

    let moves = [
        (ActionKind::Archive, vec!["INBOX"], Probe::NotDone),
        (
            ActionKind::Archive,
            vec!["CATEGORY_PROMOTIONS"],
            Probe::Done,
        ),
        (ActionKind::Trash, vec!["TRASH"], Probe::Done),
        (ActionKind::Spam, vec!["INBOX"], Probe::NotDone),
        (ActionKind::MarkRead, vec!["UNREAD"], Probe::NotDone),
        (ActionKind::MarkRead, vec!["INBOX"], Probe::Done),
    ];
    for (kind, labels, expected) in moves {
        let labels = labels.clone();
        let mailbox = GmailBox::open(move |request| {
            assert_eq!(request.query("format"), Some("minimal"));
            assert_token(request, "Bearer at-reader");
            (200, labels_of(&labels))
        })
        .await;
        let mut effects = mailbox.effects(true, true);
        assert_eq!(
            effects.probe(approved(kind, MSG, None).content()).await,
            expected,
            "{kind:?}"
        );
        assert!(mailbox.calls().iter().all(|call| call.method == "GET"));
    }

    let gone = GmailBox::open(|_| (404, r#"{"error":"SECRET-PHRASE"}"#.into())).await;
    let mut effects = gone.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Trash, MSG, None).content())
            .await,
        Probe::Gone
    );
    let down = GmailBox::open(|_| (0, String::new())).await;
    let mut effects = down.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Archive, MSG, None).content())
            .await,
        Probe::Unreachable
    );
}

#[tokio::test]
async fn a_hostile_id_is_refused_before_it_is_requested_and_errors_omit_secrets() {
    let mailbox = GmailBox::open(|_| (200, r#"{"error":"SECRET-PHRASE at-reader"}"#.into())).await;
    let mut source = mailbox.source("INBOX");
    let error = source
        .metadata(&[gmail_ref("../secret")])
        .await
        .unwrap_err();
    assert!(mailbox.fake.requests().is_empty());
    assert_no_secret(&format!("{error:#}"));
    let mut effects = mailbox.effects(true, true);
    let done = effects
        .change(&approved(ActionKind::Trash, "../secret", None))
        .await;
    assert!(mailbox.fake.requests().is_empty(), "{done:?}");
    assert_no_secret(&format!("{done:?}"));
    assert!(matches!(
        done,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Internal,
        }
    ));
}
