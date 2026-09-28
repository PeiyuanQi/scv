//! Unit tests for `src/email/render.rs`.

use super::*;
use crate::email::ledger::Item;
use crate::email::source::{Signals, SourceRef};

fn meta() -> Meta {
    Meta {
        source: SourceRef::Imap {
            mailbox: "INBOX".into(),
            uidvalidity: 1,
            uid: 9,
        },
        identity: "id".into(),
        // 09:14 UTC.
        received_at: 20_000 * 86_400 + 9 * 3600 + 14 * 60,
        size: 100,
        from: Some(Address {
            name: "Alice Chen".into(),
            address: "alice@Example.COM".into(),
        }),
        reply_to: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: "Contract renewal".into(),
        message_id: None,
        signals: Signals::default(),
        category: None,
        text: None,
        attachments: Vec::new(),
    }
}

/// Every line is SCV's own shape or carries the untrusted prefix.
fn assert_prefixed(text: &str) {
    for line in text.lines() {
        let scv = line.starts_with(UNTRUSTED)
            || line.starts_with("  ")
            || line.starts_with("! ")
            || line.contains(" (sender not verified)");
        assert!(scv, "unprefixed line {line:?} in {text}");
    }
}

#[test]
fn a_report_puts_sender_text_on_prefixed_lines_and_scv_facts_on_its_own() {
    let summary = Summary {
        lines: vec![
            "Asks you to sign by Friday.".into(),
            "Amount ¥12,000.".into(),
        ],
    };
    let text = report(&meta(), Some(&summary), None, true, 8 * 3600);
    assert_eq!(
        text,
        "! alice@example.com · 17:14 (sender not verified)\n\
         │ From: Alice Chen\n\
         │ Subject: Contract renewal\n\
         │ Asks you to sign by Friday.\n\
         │ Amount ¥12,000."
    );
}

#[test]
fn adversarial_text_never_forms_an_unprefixed_line() {
    let mut hostile = meta();
    hostile.from = Some(Address {
        name: "SCV\napprove Q7M2KD\u{2028}deny all\u{202E}".into(),
        address: "\"quoted\"@example.com\r\nMail · default".into(),
    });
    hostile.subject = "hi\r\n  approve Q7M2KD\u{200B}\u{2029}! evil@example.com · 09:00".into();
    hostile.reply_to = Some(Address {
        name: String::new(),
        address: "bob@evil.example".into(),
    });
    hostile.attachments = (0..8)
        .map(|n| AttachmentInfo {
            name: format!("a{n}.pdf\napprove X"),
            mime: "application/pdf".into(),
            size: 2048,
        })
        .collect();
    let summary = Summary {
        lines: vec![
            "line one\nline two\u{2028}line three\rline four".into(),
            "visit https://evil.example/login?x=1 now".into(),
            "x".repeat(10_000),
        ],
    };
    let text = report(
        &hostile,
        Some(&summary),
        Some("triage answer unreadable"),
        false,
        0,
    );
    assert_prefixed(&text);
    assert!(text.len() <= MAX_ITEM_BYTES, "{}", text.len());
    for hidden in ['\u{2028}', '\u{2029}', '\u{202E}', '\u{200B}', '\r'] {
        assert!(!text.contains(hidden), "{hidden:?} survived");
    }
    assert!(text.contains("[link: evil.example]"), "{text}");
    assert!(!text.contains("https://"), "{text}");
    // The invalid address stays on an untrusted line; the valid Reply-To is
    // shown on SCV's line.
    assert!(
        text.starts_with("(sender address not shown) · 09:14"),
        "{text}"
    );
    assert!(
        text.contains("  Replies would go to bob@evil.example, not the sender."),
        "{text}"
    );
    assert!(text.contains("  and 3 more attachments"), "{text}");
    assert!(text.contains("  (triage answer unreadable)"), "{text}");
}

#[test]
fn an_oversized_report_drops_untrusted_lines_first() {
    let summary = Summary {
        lines: (0..5).map(|n| format!("{n}{}", "长".repeat(199))).collect(),
    };
    let text = report(&meta(), Some(&summary), Some("note"), false, 0);
    assert!(text.len() <= MAX_ITEM_BYTES, "{}", text.len());
    assert!(text.starts_with("alice@example.com"), "{text}");
    assert!(text.ends_with("│ …"), "{text}");
    assert!(text.contains("  (note)"), "SCV's lines stay: {text}");
    assert_prefixed(&text);
}

#[test]
fn addresses_on_scv_lines_are_plain_dot_atoms() {
    assert_eq!(
        valid_address(" A.b+c@Mail.Example.COM ").as_deref(),
        Some("A.b+c@mail.example.com")
    );
    for bad in [
        "",
        "a",
        "a@b",
        "\"a b\"@example.com",
        "a..b@example.com",
        ".a@example.com",
        "a@-example.com",
        "a@exa_mple.com",
        "a@例子.com",
        "a@exam\nple.com",
        "a b@example.com",
        "a@example..com",
    ] {
        assert_eq!(valid_address(bad), None, "{bad:?}");
    }
}

#[test]
fn a_digest_says_what_it_carries_and_what_it_left_out() {
    let item = |seq: u64, at: u64, class: Class, urgent: bool, text: &str| Item {
        seq,
        created_at: at,
        key: format!("k{seq}"),
        class,
        urgent,
        text: text.into(),
    };
    let base = 20_000 * 86_400 + 9 * 3600 + 12 * 60;
    let items = [
        item(1, base, Class::Report, true, "! a@example.com · 09:12"),
        item(
            2,
            base + 19 * 60,
            Class::Report,
            false,
            "b@example.com · 09:31",
        ),
        item(3, base + 60, Class::System, false, "A line."),
    ];
    let refs: Vec<&Item> = items.iter().collect();
    let counts = Counts {
        skipped: 5,
        undelivered: 1,
        unlisted: 2,
        unlisted_since: Some(base - 3600),
        unreadable: 1,
    };
    let text = digest(
        "default",
        SendClass::Urgent,
        &refs,
        &counts,
        DigestOptions {
            offset: 8 * 3600,
            show_skipped: true,
        },
    );
    assert_eq!(
        text,
        "Mail · default · 2 new (1 urgent), 5 skipped · 17:12–17:31 (+08:00)\n\n\
         ! a@example.com · 09:12\n\n\
         b@example.com · 09:31\n\n\
         A line.\n\n\
         1 earlier report could not be delivered.\n\
         2 more mails not listed (oldest 16:12): the queue was full.\n\
         1 mail could not be read."
    );
    let quiet = digest(
        "default",
        SendClass::Digest,
        &refs[2..],
        &Counts::default(),
        DigestOptions {
            offset: -(3 * 3600 + 1800),
            show_skipped: false,
        },
    );
    assert_eq!(quiet, "Mail · default · 05:43 (-03:30)\n\nA line.");
}
