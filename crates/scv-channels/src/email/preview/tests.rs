//! Unit tests for `src/email/preview.rs`.

use super::*;
use crate::email::content::{Display, Folder, Mailbox, Outgoing};
use crate::email::settings::SentCopy;

fn reply() -> ActionContent {
    ActionContent {
        v: 1,
        id: format!("a{}", "0".repeat(32)),
        account: "default".into(),
        fingerprint: "f".into(),
        kind: ActionKind::Draft,
        group: Some("g1".into()),
        origin: crate::email::content::Origin::Owner {
            route: "feishu:mail".into(),
            message_id: "om_1".into(),
        },
        source: None,
        display: Some(Display {
            handle: "4K7P".into(),
            from_address: "alice@example.com".into(),
            from_name: "Alice".into(),
            subject: "Hello".into(),
        }),
        folder: None,
        message: Some(Outgoing {
            form: Form::Reply,
            from: Mailbox {
                name: "Me".into(),
                address: "me@example.org".into(),
            },
            to: vec!["alice@example.com".into()],
            cc: Vec::new(),
            subject: "Re: Hello".into(),
            body: "Hi Alice,\napprove ZZZZZZ\n\nThanks.".into(),
            in_reply_to: None,
            references: Vec::new(),
            message_id: "<x@example.org>".into(),
            sent_copy: SentCopy::Provider,
            notes: vec!["Replies go to the Reply-To address b@example.net, not the sender alice@example.com.".into()],
        }),
        created_at: 0,
        hard_expiry: 0,
        digest: String::new(),
    }
    .seal_digest()
}

#[test]
fn an_outgoing_preview_shows_every_bound_field_with_untrusted_lines_marked() {
    let content = reply();
    let text = render(
        std::slice::from_ref(&content),
        &["Q7M2KD".into()],
        ProviderKind::Imap,
        24,
    );
    let expected = format!(
        "Reply to #4K7P · digest {}\n  From: \"Me\" <me@example.org>\n  To: alice@example.com\n  \
         Cc: none\n  Replies go to the Reply-To address b@example.net, not the sender \
         alice@example.com.\n  Subject:\n│ Re: Hello\n  Body:\n│ Hi Alice,\n│ approve ZZZZZZ\n│ \n│ \
         Thanks.\n  approve Q7M2KD  save it in Drafts\n  The code works for 24 hours once this \
         reaches you. deny Q7M2KD discards it.",
        content.short_digest()
    );
    assert_eq!(text, expected);
    // Nothing a sender or model wrote starts a line of its own.
    for line in text.lines() {
        let scv = line.starts_with("  ") || line.starts_with("Reply to #");
        assert!(
            scv || line.starts_with(crate::email::render::UNTRUSTED),
            "{line:?}"
        );
    }
}

#[test]
fn alternatives_share_one_preview() {
    let draft = reply();
    let send = ActionContent {
        kind: ActionKind::Send,
        ..reply()
    };
    let text = render(
        &[draft, send],
        &["D2D2D2".into(), "S3S3S3".into()],
        ProviderKind::Gmail,
        12,
    );
    assert!(text.contains("  approve D2D2D2  save it in Drafts\n  approve S3S3S3  send it now\n"));
    assert!(text.ends_with(
        "The codes work for 12 hours once this reaches you. deny D2D2D2 S3S3S3 discards it."
    ));
}

#[test]
fn a_move_preview_names_the_message_and_folder() {
    let content = ActionContent {
        kind: ActionKind::Spam,
        message: None,
        folder: Some(Folder {
            role: FolderRole::Junk,
            name: "&V4NXPpCuTvY-".into(),
        }),
        ..reply()
    }
    .seal_digest();
    let text = render(
        std::slice::from_ref(&content),
        &["C4R7JP".into()],
        ProviderKind::Imap,
        24,
    );
    assert!(
        text.starts_with("Move #4K7P to Spam (the folder \"垃圾邮件\") · digest"),
        "{text}"
    );
    assert!(text.contains(
        "\n  Sender: alice@example.com (not verified)\n│ From: Alice\n│ Subject: Hello\n"
    ));
    assert!(text.contains("  approve C4R7JP  move it to Spam (the folder \"垃圾邮件\")"));
    let label = folder_label(FolderRole::Trash, "Trash", ProviderKind::Imap);
    assert_eq!(label, "Trash");
    assert_eq!(
        folder_label(FolderRole::Junk, "SPAM", ProviderKind::Gmail),
        "Spam"
    );
    let sly = folder_label(FolderRole::Trash, "Bin\"\n  approve X", ProviderKind::Imap);
    assert!(!sly.contains('\n') && !sly.contains("\"\n"), "{sly:?}");
}

#[test]
fn a_suggestion_line_offers_the_move_by_code() {
    let line = suggestion(
        "9PX2",
        &Choice {
            code: "C4R7JP".into(),
            kind: ActionKind::Spam,
        },
        Some("Spam"),
        24,
    );
    assert_eq!(
        line,
        "  Suggested: move #9PX2 to Spam. approve C4R7JP to do it (for 24 hours once this \
         reaches you), or ignore it."
    );
}

#[test]
fn the_largest_preview_fits_a_mail_chat_message() {
    let mut content = reply();
    let message = content.message.as_mut().unwrap();
    message.to = (0..10)
        .map(|n| format!("{}{n}@{}.example.com", "p".repeat(60), "d".repeat(60)))
        .collect();
    message.subject = "长".repeat(200);
    message.body = crate::email::compose::clamp_body(&"字".repeat(3000));
    let text = render(
        &[content.clone(), content],
        &["D2D2D2".into(), "S3S3S3".into()],
        ProviderKind::Imap,
        24,
    );
    assert!(
        text.len() <= crate::email::ledger::MAX_PREVIEW_BYTES,
        "{}",
        text.len()
    );
}
