//! Unit tests for `src/email/api.rs`.

use super::*;
use crate::email::credentials::GrantKind;
use crate::email::ledger::actions::{Execution, OutcomeCode};
use crate::email::oauth::OAuthProvider;
use crate::email::test_support::{FakeHttp, token_answer, tokens};
use base64::Engine as _;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const ID: &str = "msg.1_A-b";
const OTHER: &str = "msg.2";

/// A client allowed to perform `kind` on `id`, moving Graph mail to `destination`.
fn targets(kind: ActionKind, id: Option<&str>, destination: Option<&str>) -> Mode {
    Mode::Write(Targets {
        kind,
        id: id.map(str::to_owned),
        destination: destination.map(str::to_owned),
    })
}

fn assert_allowed(mode: &Mode, method: Method, path: &str, body: &Body) {
    assert!(
        check(mode, method, path, body).is_ok(),
        "refused {method:?} {path} {body:?}"
    );
}

fn assert_refused(mode: &Mode, method: Method, path: &str, body: &Body) {
    assert!(
        check(mode, method, path, body).is_err(),
        "allowed {method:?} {path} {body:?}"
    );
}

fn token_posts(fake: &FakeHttp) -> usize {
    fake.requests()
        .iter()
        .filter(|request| {
            request.method == "POST" && request.path == crate::email::test_support::TOKEN_PATH
        })
        .count()
}

#[tokio::test]
async fn a_reading_client_sends_the_readers_token_and_asks_graph_for_immutable_ids() {
    let fake = FakeHttp::start(|request| {
        token_answer(request).unwrap_or((200, r#"{"value":[]}"#.to_owned()))
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Graph,
        GrantKind::Reader,
    );
    let api = Api::new(Flavor::Graph, fake.origin.clone(), reader, Mode::Read).unwrap();
    let reply = api
        .get("/v1.0/me/mailFolders/inbox/messages", &[("$top", "1")])
        .await
        .unwrap();
    assert!(reply.ok());
    assert_eq!(reply.body["value"], serde_json::json!([]));
    let requests = fake.requests();
    let sent = requests.last().unwrap();
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.path, "/v1.0/me/mailFolders/inbox/messages");
    assert_eq!(sent.query("$top"), Some("1"));
    assert_eq!(sent.header("authorization"), Some("Bearer at-reader"));
    assert!(sent.header("prefer").unwrap().contains("ImmutableId"));
}

#[test]
fn read_only_gets_of_the_listed_paths_pass_and_bad_ids_are_refused() {
    let reads = [
        "/gmail/v1/users/me/profile",
        "/gmail/v1/users/me/history",
        "/gmail/v1/users/me/messages",
        &format!("/gmail/v1/users/me/messages/{ID}"),
        &format!("/gmail/v1/users/me/messages/{ID}/attachments/{ID}"),
        "/v1.0/me",
        "/v1.0/me/mailFolders/inbox",
        "/v1.0/me/mailFolders/drafts/messages",
        "/v1.0/me/mailFolders/sentitems/messages",
        "/v1.0/me/mailFolders/deleteditems/messages",
        "/v1.0/me/mailFolders/junkemail/messages",
        "/v1.0/me/mailFolders/archive/messages",
        &format!("/v1.0/me/messages/{ID}"),
        &format!("/v1.0/me/messages/{ID}/attachments"),
    ];
    let writing = targets(ActionKind::Draft, None, None);
    for path in reads {
        assert_allowed(&Mode::Read, Method::Get, path, &Body::None);
        assert_allowed(&writing, Method::Get, path, &Body::None);
    }

    assert_refused(
        &Mode::Read,
        Method::Get,
        "/v1.0/me/mailFolders/Inbox/messages",
        &Body::None,
    );
    assert_refused(
        &Mode::Read,
        Method::Get,
        "/gmail/v1/users/me/messages/id/trash",
        &Body::None,
    );
    let long = "x".repeat(513);
    for id in [
        "ab/cd",
        ".",
        "..",
        "a..b",
        "a%2e",
        "a?b",
        "a#b",
        "",
        long.as_str(),
    ] {
        assert_refused(
            &Mode::Read,
            Method::Get,
            &format!("/gmail/v1/users/me/messages/{id}"),
            &Body::None,
        );
    }
    assert_refused(
        &Mode::Read,
        Method::Get,
        &format!("/gmail/v1/users/me/messages/{ID}/attachments/bad%id"),
        &Body::None,
    );
}

#[test]
fn read_mode_refuses_any_write_or_a_get_that_carries_a_body() {
    let body = Body::Json(json!({"message": {"raw": "abc"}}));
    assert_refused(
        &Mode::Read,
        Method::Post,
        "/gmail/v1/users/me/profile",
        &Body::None,
    );
    assert_refused(&Mode::Read, Method::Patch, "/v1.0/me", &Body::None);
    assert_refused(
        &Mode::Read,
        Method::Get,
        "/gmail/v1/users/me/profile",
        &Body::Json(json!({})),
    );
    assert_refused(
        &Mode::Read,
        Method::Post,
        "/gmail/v1/users/me/drafts",
        &body,
    );
    assert_refused(
        &Mode::Read,
        Method::Post,
        "/v1.0/me/sendMail",
        &Body::Mime("TUlNRQ==".into()),
    );
    assert_refused(
        &targets(ActionKind::Draft, None, None),
        Method::Get,
        "/gmail/v1/users/me/profile",
        &Body::Json(json!({})),
    );
}

#[test]
fn gmail_write_mode_allows_only_the_approved_request() {
    let draft = targets(ActionKind::Draft, None, None);
    let drafted = Body::Json(json!({"message": {"raw": "abc"}}));
    assert_allowed(&draft, Method::Post, "/gmail/v1/users/me/drafts", &drafted);
    assert_allowed(
        &draft,
        Method::Post,
        "/gmail/v1/users/me/drafts",
        &Body::Json(json!({"message": {"raw": "abc", "threadId": "t"}})),
    );
    assert_refused(
        &draft,
        Method::Post,
        "/gmail/v1/users/me/drafts",
        &Body::Json(json!({"message": {"raw": "abc"}, "extra": 1})),
    );
    assert_refused(
        &draft,
        Method::Post,
        "/gmail/v1/users/me/drafts",
        &Body::Json(json!({"message": {}})),
    );
    assert_refused(&draft, Method::Post, "/gmail/v1/users/me/drafts/", &drafted);
    assert_refused(
        &draft,
        Method::Post,
        "/gmail/v1/users/me/messages/send",
        &Body::Json(json!({"raw": "abc"})),
    );

    let send = targets(ActionKind::Send, None, None);
    assert_allowed(
        &send,
        Method::Post,
        "/gmail/v1/users/me/messages/send",
        &Body::Json(json!({"raw": "abc"})),
    );
    assert_allowed(
        &send,
        Method::Post,
        "/gmail/v1/users/me/messages/send",
        &Body::Json(json!({"raw": "abc", "threadId": "t"})),
    );
    assert_refused(
        &send,
        Method::Post,
        "/gmail/v1/users/me/messages/send",
        &Body::Json(json!({})),
    );
    assert_refused(&send, Method::Post, "/gmail/v1/users/me/drafts", &drafted);

    let trash = targets(ActionKind::Trash, Some(ID), None);
    let trash_path = format!("/gmail/v1/users/me/messages/{ID}/trash");
    assert_allowed(&trash, Method::Post, &trash_path, &Body::None);
    assert_refused(
        &trash,
        Method::Post,
        &format!("/gmail/v1/users/me/messages/{OTHER}/trash"),
        &Body::None,
    );
    assert_refused(&trash, Method::Post, &trash_path, &Body::Json(json!({})));
    assert_refused(
        &trash,
        Method::Post,
        &format!("{trash_path}?x=1"),
        &Body::None,
    );

    let modify = |kind| targets(kind, Some(ID), None);
    let path = format!("/gmail/v1/users/me/messages/{ID}/modify");
    assert_allowed(
        &modify(ActionKind::Archive),
        Method::Post,
        &path,
        &Body::Json(json!({"removeLabelIds": ["INBOX"]})),
    );
    assert_allowed(
        &modify(ActionKind::Spam),
        Method::Post,
        &path,
        &Body::Json(json!({"addLabelIds": ["SPAM"], "removeLabelIds": ["INBOX"]})),
    );
    assert_allowed(
        &modify(ActionKind::MarkRead),
        Method::Post,
        &path,
        &Body::Json(json!({"removeLabelIds": ["UNREAD"]})),
    );
    for (kind, body) in [
        (
            ActionKind::Archive,
            json!({"removeLabelIds": ["INBOX"], "extra": true}),
        ),
        (
            ActionKind::Archive,
            json!({"removeLabelIds": ["INBOX", "UNREAD"]}),
        ),
        (ActionKind::Archive, json!({"removeLabelIds": ["SPAM"]})),
        (ActionKind::Spam, json!({"addLabelIds": ["SPAM"]})),
        (
            ActionKind::Spam,
            json!({"addLabelIds": ["INBOX", "SPAM"], "removeLabelIds": ["INBOX"]}),
        ),
        (
            ActionKind::MarkRead,
            json!({"addLabelIds": ["INBOX"], "removeLabelIds": ["UNREAD"]}),
        ),
        (
            ActionKind::Archive,
            json!({"addLabelIds": ["SPAM"], "removeLabelIds": ["INBOX"]}),
        ),
    ] {
        assert_refused(&modify(kind), Method::Post, &path, &Body::Json(body));
    }
    assert_refused(
        &modify(ActionKind::Archive),
        Method::Patch,
        &path,
        &Body::Json(json!({"removeLabelIds": ["INBOX"]})),
    );
    assert_refused(
        &modify(ActionKind::Archive),
        Method::Post,
        &format!("/gmail/v1/users/me/messages/{OTHER}/modify"),
        &Body::Json(json!({"removeLabelIds": ["INBOX"]})),
    );
}

#[test]
fn a_label_change_refuses_anything_but_the_kinds_labels() {
    let archive = targets(ActionKind::Archive, Some(ID), None);
    let path = format!("/gmail/v1/users/me/messages/{ID}/modify");
    for body in [
        json!({"removeLabelIds": ["INBOX", 1]}),
        json!({"removeLabelIds": ["INBOX"], "addLabelIds": "SPAM"}),
    ] {
        assert_refused(&archive, Method::Post, &path, &Body::Json(body));
    }
    let spam = targets(ActionKind::Spam, Some(ID), None);
    assert_refused(
        &spam,
        Method::Post,
        &path,
        &Body::Json(json!({"addLabelIds": ["SPAM", null], "removeLabelIds": ["INBOX"]})),
    );
}

#[test]
fn graph_write_mode_allows_only_the_approved_request() {
    let mime = Body::Mime("TUlNRQ==".into());
    let draft = targets(ActionKind::Draft, None, None);
    let send = targets(ActionKind::Send, None, None);
    assert_allowed(&draft, Method::Post, "/v1.0/me/messages", &mime);
    assert_refused(
        &draft,
        Method::Post,
        "/v1.0/me/messages",
        &Body::Json(json!({"body": "x"})),
    );
    assert_refused(&draft, Method::Post, "/v1.0/me/sendMail", &mime);
    assert_allowed(&send, Method::Post, "/v1.0/me/sendMail", &mime);
    assert_refused(&send, Method::Post, "/v1.0/me/messages", &mime);

    for (kind, folder) in [
        (ActionKind::Archive, "archive"),
        (ActionKind::Trash, "deleteditems"),
        (ActionKind::Spam, "junkemail"),
    ] {
        let mode = targets(kind, Some(ID), Some(folder));
        let path = format!("/v1.0/me/messages/{ID}/move");
        assert_allowed(
            &mode,
            Method::Post,
            &path,
            &Body::Json(json!({"destinationId": folder})),
        );
        assert_refused(
            &mode,
            Method::Post,
            &path,
            &Body::Json(json!({"destinationId": "inbox"})),
        );
        assert_refused(
            &mode,
            Method::Post,
            &path,
            &Body::Json(json!({"destinationId": folder, "extra": 1})),
        );
        assert_refused(
            &mode,
            Method::Post,
            &path,
            &Body::Json(json!({"destinationId": null})),
        );
        assert_refused(
            &mode,
            Method::Post,
            &format!("/v1.0/me/messages/{OTHER}/move"),
            &Body::Json(json!({"destinationId": folder})),
        );
    }

    let mark = targets(ActionKind::MarkRead, Some(ID), None);
    let path = format!("/v1.0/me/messages/{ID}");
    assert_allowed(
        &mark,
        Method::Patch,
        &path,
        &Body::Json(json!({"isRead": true})),
    );
    assert_refused(
        &mark,
        Method::Post,
        &path,
        &Body::Json(json!({"isRead": true})),
    );
    assert_refused(
        &mark,
        Method::Patch,
        &path,
        &Body::Json(json!({"isRead": false})),
    );
    assert_refused(
        &mark,
        Method::Patch,
        &path,
        &Body::Json(json!({"isRead": true, "flag": 1})),
    );
    assert_refused(
        &mark,
        Method::Patch,
        &format!("/v1.0/me/messages/{OTHER}"),
        &Body::Json(json!({"isRead": true})),
    );
}

#[test]
fn a_move_with_no_bound_folder_is_refused() {
    let mode = targets(ActionKind::Trash, Some(ID), None);
    let path = format!("/v1.0/me/messages/{ID}/move");
    for body in [
        json!({"nope": "deleteditems"}),
        json!({"destinationId": null}),
    ] {
        assert_refused(&mode, Method::Post, &path, &Body::Json(body));
    }
}

#[test]
fn a_draft_target_cannot_send_and_a_send_target_cannot_draft() {
    let draft = targets(ActionKind::Draft, None, None);
    let send = targets(ActionKind::Send, None, None);
    let drafted = Body::Json(json!({"message": {"raw": "abc"}}));
    let sent = Body::Json(json!({"raw": "abc"}));
    let mime = Body::Mime("TUlNRQ==".into());
    assert_allowed(&draft, Method::Post, "/gmail/v1/users/me/drafts", &drafted);
    assert_refused(
        &draft,
        Method::Post,
        "/gmail/v1/users/me/messages/send",
        &sent,
    );
    assert_refused(&send, Method::Post, "/gmail/v1/users/me/drafts", &drafted);
    assert_allowed(
        &send,
        Method::Post,
        "/gmail/v1/users/me/messages/send",
        &sent,
    );
    assert_allowed(&draft, Method::Post, "/v1.0/me/messages", &mime);
    assert_refused(&draft, Method::Post, "/v1.0/me/sendMail", &mime);
    assert_refused(&send, Method::Post, "/v1.0/me/messages", &mime);
    assert_allowed(&send, Method::Post, "/v1.0/me/sendMail", &mime);
    assert_refused(
        &draft,
        Method::Post,
        &format!("/gmail/v1/users/me/messages/{ID}/trash"),
        &Body::None,
    );
}

#[test]
fn classify_maps_every_answer_and_failure() {
    let applied = Execution::Applied {
        code: OutcomeCode::Applied,
        sent_copy: None,
    };
    let same = [
        (200, applied),
        (201, applied),
        (204, applied),
        (299, applied),
        (
            300,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Refused,
            },
        ),
        (
            302,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Refused,
            },
        ),
        (
            400,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Refused,
            },
        ),
        (
            401,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::AuthFailed,
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
            404,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Gone,
            },
        ),
        (
            409,
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Refused,
            },
        ),
        (
            429,
            Execution::NotApplied {
                retry: true,
                code: OutcomeCode::Refused,
            },
        ),
    ];
    for (status, execution) in same {
        for send in [false, true] {
            assert_eq!(
                classify(
                    Ok(Reply {
                        status,
                        body: json!(null)
                    }),
                    send
                ),
                execution,
                "status {status} send {send}"
            );
        }
    }
    for status in [500, 503, 599] {
        assert_eq!(
            classify(
                Ok(Reply {
                    status,
                    body: json!(null)
                }),
                false
            ),
            Execution::NotApplied {
                retry: true,
                code: OutcomeCode::Refused,
            },
            "status {status}"
        );
        assert_eq!(
            classify(
                Ok(Reply {
                    status,
                    body: json!(null)
                }),
                true
            ),
            Execution::Ambiguous,
            "status {status} send"
        );
    }

    let refused = Execution::NotApplied {
        retry: false,
        code: OutcomeCode::Internal,
    };
    let auth = Execution::NotApplied {
        retry: false,
        code: OutcomeCode::AuthFailed,
    };
    let later = Execution::NotApplied {
        retry: true,
        code: OutcomeCode::Unreachable,
    };
    for send in [false, true] {
        assert_eq!(
            classify(
                Err(Failure::Refused(ApiViolation {
                    what: "POST /x".into()
                })),
                send
            ),
            refused
        );
        assert_eq!(
            classify(
                Err(Failure::Token(TokenError::SignIn(anyhow::anyhow!("gone")))),
                send
            ),
            auth
        );
        assert_eq!(
            classify(
                Err(Failure::Token(TokenError::Unavailable(anyhow::anyhow!(
                    "down"
                )))),
                send
            ),
            later
        );
        assert_eq!(
            classify(Err(Failure::NotSent(anyhow::anyhow!("refused"))), send),
            later
        );
        assert_eq!(
            classify(Err(Failure::Lost(anyhow::anyhow!("cut"))), send),
            Execution::Ambiguous
        );
        assert_eq!(
            classify(Err(Failure::Uncertain(anyhow::anyhow!("401"))), send),
            Execution::Uncertain
        );
    }
}

#[test]
fn base64url_round_trips_padded_and_unpadded_text() {
    let raw = b"\xfb\xff\xef\x00hi?";
    let encoded = base64url(raw);
    assert!(
        !encoded
            .chars()
            .any(|symbol| matches!(symbol, '+' | '/' | '=')),
        "{encoded}"
    );
    assert_eq!(from_base64url(&encoded).as_deref(), Some(raw.as_slice()));
    let padded = base64::engine::general_purpose::URL_SAFE.encode(raw);
    assert!(padded.ends_with('='), "{padded}");
    assert_ne!(padded, encoded);
    assert_eq!(from_base64url(&padded).as_deref(), Some(raw.as_slice()));
    assert_eq!(from_base64url("").as_deref(), Some([].as_slice()));
    assert!(from_base64url("@@@@").is_none());
}

#[tokio::test]
async fn a_gmail_client_sends_the_grant_it_was_given_and_no_prefer_header() {
    let fake = FakeHttp::start(|request| {
        token_answer(request).unwrap_or((200, r#"{"emailAddress":"me@gmail.com"}"#.into()))
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let writer = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Writer,
    );
    let reading = Api::new(Flavor::Gmail, fake.origin.clone(), reader, Mode::Read).unwrap();
    reading
        .get("/gmail/v1/users/me/profile", &[])
        .await
        .unwrap();
    let writing = Api::new(
        Flavor::Gmail,
        fake.origin.clone(),
        writer,
        targets(ActionKind::Draft, None, None),
    )
    .unwrap();
    writing
        .request(
            Method::Post,
            "/gmail/v1/users/me/drafts",
            &[],
            Body::Json(json!({"message": {"raw": "abc"}})),
        )
        .await
        .unwrap();

    let requests = fake.requests();
    let profile = requests
        .iter()
        .find(|request| request.path.ends_with("/profile"))
        .unwrap();
    assert_eq!(profile.header("authorization"), Some("Bearer at-reader"));
    assert!(profile.header("prefer").is_none(), "{profile:?}");
    let draft = requests
        .iter()
        .find(|request| request.path.ends_with("/drafts"))
        .unwrap();
    assert_eq!(draft.method, "POST");
    assert_eq!(draft.header("authorization"), Some("Bearer at-writer"));
    assert!(draft.header("prefer").is_none(), "{draft:?}");
    assert!(
        draft
            .header("content-type")
            .unwrap()
            .contains("application/json"),
        "{draft:?}"
    );
    assert_eq!(draft.json()["message"]["raw"], json!("abc"));
}

#[tokio::test]
async fn a_graph_writer_posts_mime_as_text_plain_and_asks_for_text_bodies() {
    let fake =
        FakeHttp::start(|request| token_answer(request).unwrap_or((201, r#"{"id":"m"}"#.into())))
            .await;
    let home = tempfile::tempdir().unwrap();
    let writer = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Graph,
        GrantKind::Writer,
    );
    let api = Api::new(
        Flavor::Graph,
        fake.origin.clone(),
        writer,
        targets(ActionKind::Draft, None, None),
    )
    .unwrap();
    let reply = api
        .request(
            Method::Post,
            "/v1.0/me/messages",
            &[],
            Body::Mime("TUlNRQ==".into()),
        )
        .await
        .unwrap();
    assert!(reply.ok(), "{reply:?}");
    let sent = fake
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1.0/me/messages")
        .unwrap();
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.header("authorization"), Some("Bearer at-writer"));
    let prefer = sent.header("prefer").unwrap();
    assert!(prefer.contains("ImmutableId"), "{prefer}");
    assert!(
        prefer.contains("outlook.body-content-type=\"text\""),
        "{prefer}"
    );
    assert!(
        sent.header("content-type").unwrap().contains("text/plain"),
        "{sent:?}"
    );
    assert_eq!(sent.body, b"TUlNRQ==");
    assert!(sent.json().is_null());
}

#[tokio::test]
async fn a_401_drops_the_cached_token_and_a_second_401_ends_the_sign_in() {
    let fake = FakeHttp::start(|request| {
        token_answer(request).unwrap_or((401, r#"{"error":"unauthorized"}"#.into()))
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Graph,
        GrantKind::Reader,
    );
    let api = Api::new(Flavor::Graph, fake.origin.clone(), reader, Mode::Read).unwrap();
    let result = api.request(Method::Get, "/v1.0/me", &[], Body::None).await;
    let posts = token_posts(&fake);
    let gets = fake
        .requests()
        .iter()
        .filter(|request| request.method == "GET")
        .count();
    assert_eq!(posts, 2, "the refused token is forgotten and renewed once");
    assert_eq!(gets, 2, "{:?}", fake.requests());
    assert!(
        fake.requests()
            .iter()
            .filter(|request| request.method == "GET")
            .all(|request| { request.header("authorization") == Some("Bearer at-reader") }),
        "{:?}",
        fake.requests()
    );
    assert!(
        matches!(result, Err(Failure::Token(TokenError::SignIn(_)))),
        "a second 401 must end the sign-in, got {result:?}"
    );
}

#[tokio::test]
async fn a_mutation_answered_401_is_not_sent_again_and_stays_uncertain() {
    let cases = [
        (
            Flavor::Gmail,
            targets(ActionKind::Send, None, None),
            Method::Post,
            "/gmail/v1/users/me/messages/send".to_owned(),
            Body::Json(json!({"raw": "abc"})),
        ),
        (
            Flavor::Graph,
            targets(ActionKind::MarkRead, Some(ID), None),
            Method::Patch,
            format!("/v1.0/me/messages/{ID}"),
            Body::Json(json!({"isRead": true})),
        ),
    ];
    for (flavor, mode, method, path, body) in cases {
        let fake = FakeHttp::start(|request| {
            token_answer(request).unwrap_or((401, r#"{"error":"unauthorized"}"#.into()))
        })
        .await;
        let home = tempfile::tempdir().unwrap();
        let writer = tokens(
            home.path(),
            &fake.origin,
            OAuthProvider::Gmail,
            GrantKind::Writer,
        );
        let api = Api::new(flavor, fake.origin.clone(), writer, mode).unwrap();
        let result = api.request(method, &path, &[], body).await;
        let posts = fake
            .requests()
            .iter()
            .filter(|request| request.path != crate::email::test_support::TOKEN_PATH)
            .count();
        assert_eq!(posts, 1, "a mutation is not retried after 401: {path}");
        assert_eq!(
            token_posts(&fake),
            1,
            "the refused token is not refreshed to send again"
        );
        assert!(
            fake.requests()
                .iter()
                .all(|request| request.header("idempotency-key").is_none()),
            "a mutation carries no idempotency key: {:?}",
            fake.requests()
        );
        assert!(matches!(&result, Err(Failure::Uncertain(_))), "{result:?}");
        assert_eq!(classify(result, false), Execution::Uncertain);
    }
}

/// Gmail and Graph document no deduplication key for these calls. The client
/// cannot attach `Idempotency-Key`, and a syntactically valid key must not
/// be treated as a reason to send a `POST` or `PATCH` again after HTTP 401.
/// A `GET` still refreshes once; that path is covered separately.
#[tokio::test]
async fn gmail_and_graph_mutations_never_retry_after_401() {
    let cases = [
        (
            Flavor::Gmail,
            targets(ActionKind::Send, None, None),
            Method::Post,
            "/gmail/v1/users/me/messages/send".to_owned(),
            Body::Json(json!({"raw": "abc"})),
            true,
        ),
        (
            Flavor::Graph,
            targets(ActionKind::Send, None, None),
            Method::Post,
            "/v1.0/me/sendMail".to_owned(),
            Body::Mime("TUlNRQ==".into()),
            true,
        ),
        (
            Flavor::Graph,
            targets(ActionKind::MarkRead, Some(ID), None),
            Method::Patch,
            format!("/v1.0/me/messages/{ID}"),
            Body::Json(json!({"isRead": true})),
            false,
        ),
    ];
    for (flavor, mode, method, path, body, send) in cases {
        let fake = FakeHttp::start(|request| {
            token_answer(request).unwrap_or((401, r#"{"error":"unauthorized"}"#.into()))
        })
        .await;
        let home = tempfile::tempdir().unwrap();
        let sender = tokens(
            home.path(),
            &fake.origin,
            OAuthProvider::Gmail,
            GrantKind::Sender,
        );
        let api = Api::new(flavor, fake.origin.clone(), sender, mode).unwrap();
        let result = api.request(method, &path, &[], body).await;
        let calls = fake
            .requests()
            .iter()
            .filter(|request| request.path != crate::email::test_support::TOKEN_PATH)
            .count();
        assert_eq!(calls, 1, "401 must not send {path} again");
        assert_eq!(token_posts(&fake), 1, "401 must not refresh to send {path}");
        assert!(
            fake.requests()
                .iter()
                .all(|request| request.header("idempotency-key").is_none()),
            "{path} must not carry an idempotency key: {:?}",
            fake.requests()
        );
        assert_eq!(classify(result, send), Execution::Uncertain);
    }
}

#[tokio::test]
async fn a_redirect_is_returned_as_it_is_and_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    let fake = FakeHttp::start(move |request| {
        token_answer(request).unwrap_or_else(|| {
            seen.fetch_add(1, Ordering::SeqCst);
            (302, r#"{"via":"redirect"}"#.into())
        })
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let api = Api::new(Flavor::Gmail, fake.origin.clone(), reader, Mode::Read).unwrap();
    let reply = api
        .request(Method::Get, "/gmail/v1/users/me/profile", &[], Body::None)
        .await
        .unwrap();
    assert_eq!(reply.status, 302);
    assert_eq!(reply.body["via"], json!("redirect"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.path != "/elsewhere"),
        "{:?}",
        fake.requests()
    );
}

#[tokio::test]
async fn a_guard_refusal_sends_nothing() {
    let fake = FakeHttp::start(|request| token_answer(request).unwrap_or((200, "{}".into()))).await;
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let api = Api::new(Flavor::Gmail, fake.origin.clone(), reader, Mode::Read).unwrap();
    let result = api
        .request(
            Method::Post,
            "/gmail/v1/users/me/messages/send",
            &[],
            Body::Json(json!({"raw": "abc"})),
        )
        .await;
    assert!(matches!(result, Err(Failure::Refused(_))), "{result:?}");
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[tokio::test]
async fn a_cut_off_answer_is_lost_and_a_send_stays_ambiguous() {
    let fake = FakeHttp::start(|request| token_answer(request).unwrap_or((0, String::new()))).await;
    let home = tempfile::tempdir().unwrap();
    let writer = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Writer,
    );
    let api = Api::new(
        Flavor::Gmail,
        fake.origin.clone(),
        writer,
        targets(ActionKind::Send, None, None),
    )
    .unwrap();
    let result = api
        .request(
            Method::Post,
            "/gmail/v1/users/me/messages/send",
            &[],
            Body::Json(json!({"raw": "abc"})),
        )
        .await;
    assert!(matches!(result, Err(Failure::Lost(_))), "{result:?}");
    assert_eq!(classify(result, true), Execution::Ambiguous);
}

#[tokio::test]
async fn a_refused_connection_is_not_sent() {
    let fake = FakeHttp::start(|request| token_answer(request).unwrap_or((200, "{}".into()))).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let api = Api::new(
        Flavor::Gmail,
        format!("http://127.0.0.1:{port}"),
        reader,
        Mode::Read,
    )
    .unwrap();
    let result = api
        .request(Method::Get, "/gmail/v1/users/me/profile", &[], Body::None)
        .await;
    assert!(matches!(result, Err(Failure::NotSent(_))), "{result:?}");
    assert_eq!(
        classify(result, false),
        Execution::NotApplied {
            retry: true,
            code: OutcomeCode::Unreachable,
        }
    );
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.path != "/gmail/v1/users/me/profile"),
        "{:?}",
        fake.requests()
    );
}

#[tokio::test]
async fn an_answer_over_the_limit_is_lost() {
    let fake = FakeHttp::start(|request| {
        token_answer(request).unwrap_or((200, "x".repeat(MAX_RESPONSE_BYTES + 1)))
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let reader = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Graph,
        GrantKind::Reader,
    );
    let api = Api::new(Flavor::Graph, fake.origin.clone(), reader, Mode::Read).unwrap();
    let result = api.request(Method::Get, "/v1.0/me", &[], Body::None).await;
    assert!(matches!(result, Err(Failure::Lost(_))), "{result:?}");
}
