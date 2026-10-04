//! Unit tests for `src/email/graph.rs`.

use super::*;
use crate::email::content::{
    ActionContent, CONTENT_VERSION, Display, Folder, FolderRole, Form, Mailbox, Origin, Outgoing,
    Source,
};
use crate::email::credentials::GrantKind;
use crate::email::oauth::OAuthProvider;
use crate::email::settings::SentCopy;
use crate::email::source::{Address, AttachmentInfo};
use crate::email::test_support::{FakeHttp, HttpRequest, TOKEN_PATH, token_answer, tokens};
use std::sync::Arc;

const MSG: &str = "AAMkAGI_1";
const OUT_ID: &str = "<reply-1@example.com>";
const QUOTED_ID: &str = "<o'brien@example.com>";
const RAW: &[u8] = b"MIME\xff-body";
const T0: &str = "2024-10-04T09:00:00Z";
const T1: &str = "2024-10-04T09:00:05Z";

/// A local Graph API. Reads use the reader grant, changes the writer grant,
/// and sending the sender grant.
struct GraphBox {
    fake: FakeHttp,
    home: tempfile::TempDir,
}

impl GraphBox {
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
            OAuthProvider::Graph,
            kind,
        )
    }

    fn source(&self) -> GraphSource {
        GraphSource::new(
            Api::new(
                Flavor::Graph,
                self.fake.origin.clone(),
                self.tokens(GrantKind::Reader),
                Mode::Read,
            )
            .unwrap(),
            "Inbox",
        )
        .unwrap()
    }

    fn effects(&self, writer: bool, sender: bool) -> GraphEffects {
        GraphEffects::new(EffectParts {
            origin: self.fake.origin.clone(),
            reader: self.tokens(GrantKind::Reader),
            writer: writer.then(|| self.tokens(GrantKind::Writer)),
            sender: sender.then(|| self.tokens(GrantKind::Sender)),
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

fn assert_prefer(request: &HttpRequest) {
    let prefer = request.header("prefer").unwrap_or_default();
    assert!(
        prefer.contains("ImmutableId") && prefer.contains("outlook.body-content-type=\"text\""),
        "{prefer}"
    );
}

fn assert_no_secret(text: &str) {
    for secret in [
        "at-reader",
        "at-writer",
        "at-sender",
        "rt-reader",
        "rt-sender",
        "SECRET-PHRASE",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
}

fn graph_ref(id: &str) -> SourceRef {
    SourceRef::Graph { id: id.to_owned() }
}

fn position(cursor: &Cursor) -> (String, Vec<String>) {
    assert_eq!(cursor.provider, ProviderKind::Graph);
    let value: serde_json::Value = serde_json::from_str(&cursor.value).unwrap();
    let ids = value["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    (value["received"].as_str().unwrap().to_owned(), ids)
}

fn graph_cursor(received: &str, ids: &[&str]) -> Cursor {
    Cursor {
        provider: ProviderKind::Graph,
        value: json!({ "received": received, "ids": ids }).to_string(),
    }
}

/// An approved action on `id`. Outgoing mail carries `message_id`.
fn approved(kind: ActionKind, id: &str, message_id: Option<&str>) -> Approved {
    let folder = match kind {
        ActionKind::Trash => Some(("deleteditems", FolderRole::Trash)),
        ActionKind::Spam => Some(("junkemail", FolderRole::Junk)),
        ActionKind::Archive => Some(("archive", FolderRole::Archive)),
        ActionKind::Draft => Some(("drafts", FolderRole::Drafts)),
        ActionKind::Send => Some(("sentitems", FolderRole::Sent)),
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
            reference: graph_ref(id),
            identity: parse::api_identity("graph", id),
            locator: parse::api_identity("graph", id),
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

fn message_json(reply_name: &str, class: &str) -> serde_json::Value {
    json!({
        "id": MSG,
        "internetMessageId": "<m1@example.com>",
        "subject": "Café notes",
        "from": {"emailAddress": {"name": "Ada Lovelace", "address": "ada@example.com"}},
        "replyTo": [{"emailAddress": {"name": reply_name, "address": "ada@example.com"}}],
        "toRecipients": [
            {"emailAddress": {"name": "Me", "address": "me@example.com"}},
            {"emailAddress": {"name": "", "address": "bob@example.org"}}
        ],
        "ccRecipients": [
            {"emailAddress": {"name": "Bill", "address": "bill@example.com"}}
        ],
        "receivedDateTime": T0,
        "internetMessageHeaders": [
            {"name": "References", "value": "<a@example.com> <b@example.com>"},
            {"name": "List-Id", "value": "=?UTF-8?Q?Caf=C3=A9?= <list.example.com>"},
            {"name": "List-Unsubscribe", "value": "<mailto:u@example.com>"},
            {"name": "Precedence", "value": " Bulk "},
            {"name": "Auto-Submitted", "value": "auto-generated"},
            {"name": "Return-Path", "value": "< >"},
            {"name": "Date", "value": "Fri, 4 Oct 2024 09:00:00 +0000"}
        ],
        "inferenceClassification": class,
        "hasAttachments": false
    })
}

#[tokio::test]
async fn a_source_accepts_only_the_inbox_and_names_well_known_folders() {
    let home = tempfile::tempdir().unwrap();
    let origin = "http://127.0.0.1:9";
    let api = || {
        Api::new(
            Flavor::Graph,
            origin.into(),
            tokens(home.path(), origin, OAuthProvider::Graph, GrantKind::Reader),
            Mode::Read,
        )
        .unwrap()
    };
    for mailbox in ["INBOX", "Inbox", " inbox "] {
        let source = GraphSource::new(api(), mailbox).unwrap();
        assert_eq!(source.folder, "inbox", "{mailbox}");
    }
    for mailbox in ["sentitems", "archive", "", "INBOX/Secret", "inbox extra"] {
        assert!(
            GraphSource::new(api(), mailbox).is_err(),
            "{mailbox} is not the inbox"
        );
    }
    let mut source = GraphSource::new(api(), "Inbox").unwrap();
    assert_eq!(
        source.folders(&FolderNames::default()).await.unwrap(),
        graph_folders()
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
fn times_parse_as_unix_seconds_and_odata_strings_double_quotes() {
    assert!(valid_time("2024-02-29T00:00:00Z"));
    assert!(valid_time("2024-02-29T00:00:00.123456Z"));
    assert!(valid_time("2024-10-04T09:00:00+00:00"));
    assert!(!valid_time(""));
    assert!(!valid_time("2024-02-29 00:00:00Z"));
    assert!(!valid_time("yesterday"));
    assert!(!valid_time(&"1".repeat(41)));
    assert!(!valid_time("2024-10-04T09:00:00Z' or 1 eq 1"));

    assert_eq!(unix_of("1970-01-01T00:00:00Z"), 0);
    assert_eq!(unix_of("2024-02-29T00:00:00Z"), 1_709_164_800);
    assert_eq!(
        unix_of("2024-02-29T12:30:45Z"),
        1_709_164_800 + 12 * 3600 + 30 * 60 + 45
    );
    assert_eq!(
        unix_of("2024-02-29T00:00:00.999Z"),
        1_709_164_800,
        "a fraction does not change the second"
    );
    assert_eq!(unix_of(T0), 1_728_032_400);
    assert_eq!(unix_of("garbage"), 0);
    assert_eq!(unix_of(""), 0);
    assert_eq!(unix_of("2024"), 0);
    assert_eq!(unix_of("1969-12-31T00:00:00Z"), 0);

    assert_eq!(odata_string("plain"), "'plain'");
    assert_eq!(odata_string("a'b"), "'a''b'");
    assert_eq!(odata_string(QUOTED_ID), "'<o''brien@example.com>'");
}

#[test]
fn meta_of_reads_recipients_drops_an_equal_reply_to_and_bounds_fields() {
    let attachments = vec![AttachmentInfo {
        name: "notes.pdf".into(),
        mime: "application/pdf".into(),
        size: 5000,
    }];
    let meta = meta_of(
        MSG,
        &message_json("Ada Lovelace", "other"),
        attachments.clone(),
    );
    let identity = parse::api_identity("graph", MSG);
    assert_eq!(meta.source, graph_ref(MSG));
    assert_eq!(meta.identity, identity);
    assert_eq!(meta.locator, identity);
    assert_eq!(meta.received_at, 1_728_032_400);
    assert_eq!(meta.size, 0);
    assert_eq!(
        meta.from,
        Some(Address {
            name: "Ada Lovelace".into(),
            address: "ada@example.com".into(),
        })
    );
    assert_eq!(meta.reply_to, None, "a reply-to equal to from is dropped");
    assert_eq!(
        meta.to,
        vec![
            Address {
                name: "Me".into(),
                address: "me@example.com".into(),
            },
            Address {
                name: String::new(),
                address: "bob@example.org".into(),
            },
        ]
    );
    assert_eq!(
        meta.cc,
        vec![Address {
            name: "Bill".into(),
            address: "bill@example.com".into(),
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
    assert_eq!(meta.category.as_deref(), Some("OTHER"));
    let text = meta.text.unwrap();
    assert_eq!(text.id, "body");
    assert_eq!(text.mime, "text/plain");
    assert_eq!(text.charset.as_deref(), Some("utf-8"));
    assert_eq!(text.encoding, TransferEncoding::Binary);
    assert_eq!(meta.attachments, attachments);

    let kept = meta_of(MSG, &message_json("Someone Else", "other"), Vec::new());
    assert_eq!(
        kept.reply_to,
        Some(Address {
            name: "Someone Else".into(),
            address: "ada@example.com".into(),
        })
    );
    let focused = meta_of(MSG, &message_json("Ada Lovelace", "focused"), Vec::new());
    assert_eq!(focused.category, None);

    let tos: Vec<_> = (0..70)
        .map(|n| {
            json!({
                "emailAddress": {"name": format!("P{n}"), "address": format!("p{n}@example.com")}
            })
        })
        .collect();
    let references = (0..12)
        .map(|n| format!("<m{n}@example.com>"))
        .collect::<Vec<_>>()
        .join(" ");
    let inflated_attachments = (0..20)
        .map(|n| AttachmentInfo {
            name: "A".repeat(400),
            mime: "application/octet-stream".into(),
            size: n,
        })
        .collect();
    let inflated = meta_of(
        MSG,
        &json!({
            "subject": "S".repeat(2000),
            "from": {"emailAddress": {"name": "N".repeat(400), "address": "ada@example.com"}},
            "toRecipients": tos,
            "receivedDateTime": T0,
            "internetMessageHeaders": [
                {"name": "References", "value": references},
                {"name": "List-Id", "value": "L".repeat(400)}
            ]
        }),
        inflated_attachments,
    );
    assert_eq!(inflated.subject.len(), 1024);
    assert!(inflated.subject.ends_with("…"));
    assert_eq!(inflated.from.unwrap().name.len(), 256);
    assert_eq!(inflated.to.len(), 64);
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
async fn the_first_check_takes_the_newest_received_time_and_lists_nothing() {
    let empty = GraphBox::open(|request| {
        assert_eq!(request.path, "/v1.0/me/mailFolders/inbox/messages");
        assert_eq!(request.query("$select"), Some("id,receivedDateTime"));
        assert_eq!(request.query("$orderby"), Some("receivedDateTime desc"));
        assert_eq!(request.query("$top"), Some("1"));
        assert_eq!(request.query("$filter"), None);
        assert_prefer(request);
        assert_token(request, "Bearer at-reader");
        (200, r#"{"value":[]}"#.into())
    })
    .await;
    let mut source = empty.source();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let changes = source.changes(None, 10, 86_400).await.unwrap();
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let Changes::New { refs, next } = changes else {
        panic!("the first check lists nothing");
    };
    assert!(refs.is_empty());
    let (received, ids) = position(&next);
    assert!(ids.is_empty());
    let unix = unix_of(&received);
    assert!(
        (before..=after).contains(&unix),
        "an empty folder anchors the cursor at the current time ({received})"
    );
    let nasty = Cursor {
        provider: ProviderKind::Graph,
        value: json!({"received": "2024-10-04T09:00:00Z' or 1 eq 1", "ids": []}).to_string(),
    };
    let foreign = Cursor {
        provider: ProviderKind::Gmail,
        value: "10".into(),
    };
    for cursor in [Some(nasty), Some(foreign)] {
        let changes = source.changes(cursor.as_ref(), 10, 0).await.unwrap();
        let Changes::New { refs, .. } = changes else {
            panic!("a cursor that is not a Graph time is a first check");
        };
        assert!(refs.is_empty());
    }

    let newest = GraphBox::open(|request| {
        assert_eq!(request.query("$orderby"), Some("receivedDateTime desc"));
        assert_eq!(request.query("$top"), Some("1"));
        (
            200,
            json!({"value": [{"id": "newest-1", "receivedDateTime": T0}]}).to_string(),
        )
    })
    .await;
    let mut source = newest.source();
    let changes = source.changes(None, 5, 0).await.unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("the first check lists nothing");
    };
    assert!(refs.is_empty());
    assert_eq!(
        position(&next),
        (T0.to_owned(), vec!["newest-1".to_owned()])
    );
}

#[tokio::test]
async fn a_later_check_skips_ids_already_seen_at_that_time_and_honors_the_limit() {
    let rows = json!({
        "value": [
            {"id": "seen", "receivedDateTime": T0},
            {"id": "a/b", "receivedDateTime": T0},
            {"id": "good-old", "receivedDateTime": "not a time"},
            {"id": "same-1", "receivedDateTime": T0},
            {"id": "same-2", "receivedDateTime": T0},
            {"id": "later-1", "receivedDateTime": T1},
            {"id": "later-2", "receivedDateTime": T1},
            {"id": "later-3", "receivedDateTime": "2024-10-04T09:00:09Z"}
        ]
    });
    let mailbox = GraphBox::open(move |request| {
        assert_eq!(request.path, "/v1.0/me/mailFolders/inbox/messages");
        assert_eq!(request.query("$select"), Some("id,receivedDateTime"));
        assert_eq!(request.query("$orderby"), Some("receivedDateTime asc"));
        assert_prefer(request);
        assert_token(request, "Bearer at-reader");
        (200, rows.to_string())
    })
    .await;
    let mut source = mailbox.source();
    let changes = source
        .changes(Some(&graph_cursor(T0, &["seen"])), 2, 86_400)
        .await
        .unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("a cursor resumes");
    };
    assert_eq!(refs, vec![graph_ref("same-1"), graph_ref("same-2")]);
    assert_eq!(
        position(&next),
        (
            T0.to_owned(),
            vec!["seen".to_owned(), "same-1".to_owned(), "same-2".to_owned()]
        )
    );
    let changes = source.changes(Some(&next), 2, 86_400).await.unwrap();
    let Changes::New { refs, next } = changes else {
        panic!("a cursor resumes");
    };
    assert_eq!(refs, vec![graph_ref("later-1"), graph_ref("later-2")]);
    assert_eq!(
        position(&next),
        (
            T1.to_owned(),
            vec!["later-1".to_owned(), "later-2".to_owned()]
        )
    );
    let calls = mailbox.calls();
    assert_eq!(
        calls[0].query("$filter"),
        Some("receivedDateTime ge 2024-10-04T09:00:00Z")
    );
    assert_eq!(calls[0].query("$top"), Some("3"));
    assert_eq!(
        calls[1].query("$filter"),
        Some("receivedDateTime ge 2024-10-04T09:00:00Z")
    );
    assert_eq!(calls[1].query("$top"), Some("5"));
    assert!(calls.iter().all(|call| !call.path.contains("a/b")));
}

#[tokio::test]
async fn metadata_lists_attachments_and_text_stops_on_a_character_boundary() {
    let with_files = message_json("Ada Lovelace", "other");
    let mut with_files = with_files;
    with_files["hasAttachments"] = json!(true);
    let mut plain = message_json("Someone Else", "focused");
    plain["id"] = json!("plain-1");
    plain["hasAttachments"] = json!(false);
    let mailbox = GraphBox::open(move |request| {
        assert_prefer(request);
        assert_token(request, "Bearer at-reader");
        match request.path.as_str() {
            "/v1.0/me/messages/AAMkAGI_1" => {
                assert!(
                    request
                        .query("$select")
                        .unwrap()
                        .contains("internetMessageHeaders")
                );
                assert!(request.query("$select").unwrap().contains("hasAttachments"));
                (200, with_files.to_string())
            }
            "/v1.0/me/messages/AAMkAGI_1/attachments" => {
                assert_eq!(request.query("$select"), Some("name,contentType,size"));
                (
                    200,
                    json!({
                        "value": [
                            {"name": "notes.pdf", "contentType": "Application/PDF", "size": 5000},
                            {"name": "pic.png", "contentType": "image/png", "size": 12}
                        ]
                    })
                    .to_string(),
                )
            }
            "/v1.0/me/messages/plain-1" => (200, plain.to_string()),
            "/v1.0/me/messages/missing" => (404, r#"{"error":"SECRET-PHRASE"}"#.into()),
            "/v1.0/me/messages/html" => {
                assert_eq!(request.query("$select"), Some("body"));
                (
                    200,
                    json!({"body": {"contentType": "html", "content": "éé"}}).to_string(),
                )
            }
            "/v1.0/me/messages/text" => {
                assert_eq!(request.query("$select"), Some("body"));
                (
                    200,
                    json!({"body": {"contentType": "text", "content": "Hello"}}).to_string(),
                )
            }
            "/v1.0/me/messages/gone" => (404, r#"{"error":{"message":"SECRET-PHRASE"}}"#.into()),
            "/v1.0/me/messages/boom" => (
                500,
                r#"{"error":{"message":"Bearer at-reader saw SECRET-PHRASE"}}"#.into(),
            ),
            other => panic!("unexpected path {other}"),
        }
    })
    .await;
    let mut source = mailbox.source();
    let found = source
        .metadata(&[
            graph_ref(MSG),
            graph_ref("plain-1"),
            graph_ref("missing"),
            SourceRef::Gmail { id: "g".into() },
        ])
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].source, graph_ref(MSG));
    assert_eq!(found[0].category.as_deref(), Some("OTHER"));
    assert_eq!(found[0].attachments.len(), 2);
    assert_eq!(found[0].attachments[0].name, "notes.pdf");
    assert_eq!(found[0].attachments[0].mime, "application/pdf");
    assert_eq!(found[0].attachments[0].size, 5000);
    assert_eq!(found[0].attachments[1].mime, "image/png");
    assert_eq!(
        found[1]
            .reply_to
            .as_ref()
            .map(|address| address.name.as_str()),
        Some("Someone Else")
    );
    assert!(found[1].attachments.is_empty());
    assert_eq!(found[1].category, None);
    assert!(
        !mailbox
            .calls()
            .iter()
            .any(|call| call.path.contains("plain-1/attachments"))
    );

    let html = source
        .text(&graph_ref("html"), &found[0].text.clone().unwrap(), 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(html.text, "é");
    assert!(html.html);
    assert!(html.truncated);
    let text = source
        .text(&graph_ref("text"), &found[0].text.clone().unwrap(), 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text.text, "Hello");
    assert!(!text.html);
    assert!(!text.truncated);
    let part = found[0].text.clone().unwrap();
    assert!(
        source
            .text(&graph_ref("gone"), &part, 10)
            .await
            .unwrap()
            .is_none()
    );
    let error = source.metadata(&[graph_ref("boom")]).await.unwrap_err();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("500"), "{rendered}");
    assert_no_secret(&rendered);
}

fn state(parent: &str, read: bool) -> String {
    json!({"parentFolderId": parent, "isRead": read}).to_string()
}

#[tokio::test]
async fn a_move_compares_folder_ids_then_posts_the_destination_with_the_writer() {
    let cases = [
        (ActionKind::Trash, "deleteditems"),
        (ActionKind::Spam, "junkemail"),
        (ActionKind::Archive, "archive"),
    ];
    for (kind, folder) in cases {
        let folder_path = format!("/v1.0/me/mailFolders/{folder}");
        let mailbox = GraphBox::open(move |request| {
            assert_prefer(request);
            if request.method == "GET" && request.path.ends_with(MSG) {
                assert_eq!(request.query("$select"), Some("parentFolderId,isRead"));
                assert_token(request, "Bearer at-reader");
                (200, state("inbox-id", false))
            } else if request.method == "GET" {
                assert_eq!(request.path, folder_path);
                assert_token(request, "Bearer at-reader");
                (200, r#"{"id":"folder-id"}"#.into())
            } else {
                (200, r#"{"id":"other-id"}"#.into())
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
        let calls = mailbox.calls();
        assert_eq!(calls.len(), 3, "{kind:?}");
        let post = &calls[2];
        assert_eq!(post.method, "POST");
        assert_eq!(post.path, format!("/v1.0/me/messages/{MSG}/move"));
        assert_token(post, "Bearer at-writer");
        assert_eq!(post.json(), json!({"destinationId": folder}));
        assert!(!post.path.contains("other-id"));
    }

    let already = GraphBox::open(|request| {
        assert_eq!(request.method, "GET");
        if request.path.ends_with(MSG) {
            (200, state("trash-id", false))
        } else {
            (200, r#"{"id":"trash-id"}"#.into())
        }
    })
    .await;
    let mut effects = already.effects(true, true);
    assert_eq!(
        effects
            .change(&approved(ActionKind::Trash, MSG, None))
            .await,
        Execution::Applied {
            code: OutcomeCode::AlreadyDone,
            sent_copy: None,
        }
    );
    assert!(already.calls().iter().all(|call| call.method == "GET"));

    let gone = GraphBox::open(|_| (404, r#"{"error":"SECRET-PHRASE"}"#.into())).await;
    let mut effects = gone.effects(true, true);
    assert_eq!(
        effects.change(&approved(ActionKind::Spam, MSG, None)).await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Gone,
        }
    );
    assert_eq!(gone.calls().len(), 1);

    let failed = GraphBox::open(|request| {
        if request.method == "GET" && request.path.ends_with(MSG) {
            (200, state("inbox-id", false))
        } else if request.method == "GET" {
            (200, r#"{"id":"folder-id"}"#.into())
        } else {
            (
                503,
                r#"{"error":"Bearer at-writer saw SECRET-PHRASE"}"#.into(),
            )
        }
    })
    .await;
    let mut effects = failed.effects(true, true);
    let done = effects
        .change(&approved(ActionKind::Trash, MSG, None))
        .await;
    assert_eq!(
        done,
        Execution::NotApplied {
            retry: true,
            code: OutcomeCode::Refused,
        }
    );
    assert_no_secret(&format!("{done:?}"));
    assert_eq!(failed.calls().len(), 3);
}

#[tokio::test]
async fn marking_read_patches_is_read_and_a_draft_posts_mime_with_quotes_doubled() {
    let unread = GraphBox::open(|request| {
        if request.method == "GET" {
            assert_token(request, "Bearer at-reader");
            (200, state("inbox-id", false))
        } else {
            (200, r#"{"id":"other-id"}"#.into())
        }
    })
    .await;
    let mut effects = unread.effects(true, true);
    assert_eq!(
        effects
            .change(&approved(ActionKind::MarkRead, MSG, None))
            .await,
        Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        }
    );
    let calls = unread.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].method, "PATCH");
    assert_eq!(calls[1].path, format!("/v1.0/me/messages/{MSG}"));
    assert_token(&calls[1], "Bearer at-writer");
    assert_eq!(calls[1].json(), json!({"isRead": true}));

    let read = GraphBox::open(|request| {
        assert_eq!(request.method, "GET");
        (200, state("inbox-id", true))
    })
    .await;
    let mut effects = read.effects(true, true);
    assert_eq!(
        effects
            .change(&approved(ActionKind::MarkRead, MSG, None))
            .await,
        Execution::Applied {
            code: OutcomeCode::AlreadyDone,
            sent_copy: None,
        }
    );
    assert_eq!(read.calls().len(), 1);

    let found = GraphBox::open(|request| {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v1.0/me/mailFolders/drafts/messages");
        assert_eq!(
            request.query("$filter"),
            Some("internetMessageId eq '<o''brien@example.com>'")
        );
        assert_token(request, "Bearer at-reader");
        (200, r#"{"value":[{"id":"draft-1"}]}"#.into())
    })
    .await;
    let mut effects = found.effects(true, true);
    assert_eq!(
        effects
            .save_draft(&approved(ActionKind::Draft, MSG, Some(QUOTED_ID)), RAW)
            .await,
        Execution::Applied {
            code: OutcomeCode::AlreadyDone,
            sent_copy: None,
        }
    );
    assert!(found.calls().iter().all(|call| call.method == "GET"));

    let created = GraphBox::open(|request| {
        if request.method == "GET" {
            assert_eq!(
                request.query("$filter"),
                Some("internetMessageId eq '<o''brien@example.com>'")
            );
            (200, r#"{"value":[]}"#.into())
        } else {
            (200, r#"{"id":"draft-9"}"#.into())
        }
    })
    .await;
    let mut effects = created.effects(true, true);
    assert_eq!(
        effects
            .save_draft(&approved(ActionKind::Draft, MSG, Some(QUOTED_ID)), RAW)
            .await,
        Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        }
    );
    let posts: Vec<_> = created
        .calls()
        .into_iter()
        .filter(|call| call.method == "POST")
        .collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].path, "/v1.0/me/messages");
    assert_token(&posts[0], "Bearer at-writer");
    assert_eq!(posts[0].header("content-type"), Some("text/plain"));
    assert_prefer(&posts[0]);
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&posts[0].body)
            .unwrap(),
        RAW
    );

    let no_writer = GraphBox::open(|_| (200, r#"{"value":[]}"#.into())).await;
    let mut effects = no_writer.effects(false, true);
    assert_eq!(
        effects
            .save_draft(&approved(ActionKind::Draft, MSG, Some(OUT_ID)), RAW)
            .await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::AuthFailed,
        }
    );
    assert!(no_writer.calls().iter().all(|call| call.method == "GET"));
    let mut effects = no_writer.effects(true, true);
    assert_eq!(
        effects
            .save_draft(&approved(ActionKind::Draft, MSG, None), RAW)
            .await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Internal,
        }
    );
}

#[tokio::test]
async fn send_uses_the_sender_grant_and_treats_doubt_as_ambiguous() {
    let mailbox = GraphBox::open(|request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1.0/me/sendMail");
        (200, r#"{"id":"sent-1"}"#.into())
    })
    .await;
    let mut effects = mailbox.effects(true, true);
    let approved = approved(ActionKind::Send, MSG, Some(OUT_ID));
    assert!(effects.copy_sent(&approved, RAW).await);
    assert!(mailbox.calls().is_empty(), "Graph files sent mail itself");
    assert_eq!(
        effects.send(&approved, RAW).await,
        Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        }
    );
    let calls = mailbox.calls();
    assert_eq!(calls.len(), 1);
    assert_token(&calls[0], "Bearer at-sender");
    assert_eq!(calls[0].header("content-type"), Some("text/plain"));
    assert_prefer(&calls[0]);
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&calls[0].body)
            .unwrap(),
        RAW
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
        let mailbox = GraphBox::open(move |_| {
            (
                status,
                r#"{"error":"Bearer at-sender saw SECRET-PHRASE"}"#.into(),
            )
        })
        .await;
        let mut effects = mailbox.effects(true, true);
        let done = effects.send(&approved, RAW).await;
        assert_eq!(done, expected, "status {status}");
        assert_no_secret(&format!("{done:?}"));
        assert_eq!(mailbox.calls().len(), 1);
    }

    let mailbox = GraphBox::open(|_| (500, "{}".into())).await;
    let mut effects = mailbox.effects(true, false);
    assert_eq!(
        effects.send(&approved, RAW).await,
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::AuthFailed,
        }
    );
    assert!(mailbox.fake.requests().is_empty());
}

#[tokio::test]
async fn probes_call_a_missing_send_unknown_and_a_missing_move_not_done() {
    let search = GraphBox::open(|request| {
        assert_eq!(request.method, "GET");
        assert_token(request, "Bearer at-reader");
        assert_prefer(request);
        if request.path.contains("sentitems") {
            assert_eq!(
                request.query("$filter"),
                Some("internetMessageId eq '<reply-1@example.com>'")
            );
            return (200, r#"{"value":[{"id":"sent-1"}]}"#.into());
        }
        assert!(request.path.contains("drafts"));
        (200, r#"{"value":[]}"#.into())
    })
    .await;
    let mut effects = search.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Send, MSG, Some(OUT_ID)).content())
            .await,
        Probe::Done
    );
    assert_eq!(
        effects
            .probe(approved(ActionKind::Draft, MSG, Some(OUT_ID)).content())
            .await,
        Probe::NotDone
    );
    let missed = GraphBox::open(|request| {
        assert!(request.path.contains("sentitems"));
        (200, r#"{"value":[]}"#.into())
    })
    .await;
    let mut effects = missed.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Send, MSG, Some(OUT_ID)).content())
            .await,
        Probe::Unknown
    );

    let done = GraphBox::open(|request| {
        assert_eq!(request.method, "GET");
        if request.path.ends_with(MSG) {
            (200, state("trash-id", false))
        } else {
            assert!(request.path.ends_with("/mailFolders/deleteditems"));
            (200, r#"{"id":"trash-id"}"#.into())
        }
    })
    .await;
    let mut effects = done.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Trash, MSG, None).content())
            .await,
        Probe::Done
    );
    let pending = GraphBox::open(|request| {
        if request.path.ends_with(MSG) {
            (200, state("inbox-id", false))
        } else {
            (200, r#"{"id":"trash-id"}"#.into())
        }
    })
    .await;
    let mut effects = pending.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Trash, MSG, None).content())
            .await,
        Probe::NotDone
    );
    let read = GraphBox::open(|_| (200, state("inbox-id", true))).await;
    let mut effects = read.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::MarkRead, MSG, None).content())
            .await,
        Probe::Done
    );
    let unread = GraphBox::open(|_| (200, state("inbox-id", false))).await;
    let mut effects = unread.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::MarkRead, MSG, None).content())
            .await,
        Probe::NotDone
    );
    let gone = GraphBox::open(|_| (404, r#"{"error":"SECRET-PHRASE"}"#.into())).await;
    let mut effects = gone.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Spam, MSG, None).content())
            .await,
        Probe::Gone
    );
    let down = GraphBox::open(|_| (0, String::new())).await;
    let mut effects = down.effects(true, true);
    assert_eq!(
        effects
            .probe(approved(ActionKind::Archive, MSG, None).content())
            .await,
        Probe::Unreachable
    );
    assert!(down.calls().iter().all(|call| {
        assert_token(call, "Bearer at-reader");
        call.method == "GET"
    }));
}

#[tokio::test]
async fn a_hostile_id_is_refused_before_it_is_requested_and_errors_omit_secrets() {
    let mailbox = GraphBox::open(|_| (200, r#"{"error":"SECRET-PHRASE at-reader"}"#.into())).await;
    let mut source = mailbox.source();
    let error = source
        .metadata(&[graph_ref("../secret")])
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

    let broken = GraphBox::open(|_| {
        (
            500,
            r#"{"error":{"message":"Bearer at-reader saw SECRET-PHRASE"}}"#.into(),
        )
    })
    .await;
    let mut source = broken.source();
    let error = source.changes(None, 1, 0).await.unwrap_err();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("500"), "{rendered}");
    assert_no_secret(&rendered);
}
