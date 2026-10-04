//! Message tests for `src/email/imap/mod.rs`: metadata and text fetched
//! from the scripted server, only ever with `BODY.PEEK`.

use super::*;
use crate::email::source::{Address, AttachmentInfo};

const METADATA_ITEMS: &str = "(UID INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE \
BODY.PEEK[HEADER.FIELDS (MESSAGE-ID REFERENCES LIST-ID LIST-UNSUBSCRIBE PRECEDENCE AUTO-SUBMITTED RETURN-PATH)])";

const HEADER_KEY: &str = "BODY[HEADER.FIELDS (MESSAGE-ID REFERENCES LIST-ID LIST-UNSUBSCRIBE PRECEDENCE AUTO-SUBMITTED RETURN-PATH)]";

fn address(name: &str, address: &str) -> Address {
    Address {
        name: name.to_owned(),
        address: address.to_owned(),
    }
}

fn plain(id: &str, size: u64) -> PartRef {
    PartRef {
        id: id.to_owned(),
        mime: "text/plain".to_owned(),
        charset: Some("utf-8".to_owned()),
        encoding: parse::TransferEncoding::SevenBit,
        size,
    }
}

#[tokio::test]
async fn metadata_reads_headers_envelope_and_structure_in_ref_order() {
    let header = "Message-ID: <hdr@example.com>\r\n\
                  List-Id: Billing <billing.example.com>\r\n\
                  List-Unsubscribe: <mailto:u@example.com>\r\n\
                  Precedence:  Bulk \r\n\
                  Auto-Submitted: Auto-Generated\r\n\
                  Return-Path: < >\r\n\r\n";
    let reply = format!(
        "* 2 FETCH (UID 7 INTERNALDATE \" 7-Sep-2026 08:05:09 +0000\" RFC822.SIZE 2048 \
         ENVELOPE (\"Mon, 7 Sep 2026 08:05:00 +0000\" \"Invoice 42\" \
         ((\"Alice\" NIL \"alice\" \"example.com\")) NIL \
         ((\"Billing\" NIL \"billing\" \"example.com\")) ((\"Me\" NIL \"me\" \"example.com\")) \
         ((NIL NIL \"cc\" \"example.org\")) NIL NIL \"<env@example.com>\") \
         BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 100 3 NIL NIL NIL NIL)\
         (\"APPLICATION\" \"PDF\" (\"NAME\" \"invoice.pdf\") NIL NIL \"BASE64\" 5000 NIL \
         (\"ATTACHMENT\" (\"FILENAME\" \"invoice.pdf\")) NIL NIL) \"MIXED\" (\"BOUNDARY\" \"x\") NIL NIL) \
         {HEADER_KEY} {{{}}}\r\n{header})\r\n\
         * 1 FETCH (UID 3 INTERNALDATE \"01-Sep-2026 00:00:00 +0000\" RFC822.SIZE 10 \
         ENVELOPE (NIL NIL ((\"Bob\" NIL \"bob\" \"example.net\")) NIL \
         ((\"Bob\" NIL \"bob\" \"example.net\")) NIL NIL NIL NIL NIL) \
         BODYSTRUCTURE (\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 10 1) {HEADER_KEY} \"\")\r\n\
         {{tag}} OK done\r\n",
        header.len()
    );
    let (mut source, fake) = open(session(vec![command(
        &format!("UID FETCH 3,7,99 {METADATA_ITEMS}"),
        &reply,
    )]))
    .await;
    let other_validity = SourceRef::Imap {
        mailbox: "INBOX".to_owned(),
        uidvalidity: 6,
        uid: 5,
    };
    let other_mailbox = SourceRef::Imap {
        mailbox: "Archive".to_owned(),
        uidvalidity: 7,
        uid: 4,
    };
    let metas = source
        .metadata(&[
            inbox(7),
            inbox(3),
            inbox(99),
            other_validity,
            other_mailbox,
            inbox(7),
        ])
        .await
        .unwrap();
    let uids: Vec<SourceRef> = metas.iter().map(|meta| meta.source.clone()).collect();
    assert_eq!(uids, [inbox(7), inbox(3), inbox(7)]);

    assert_eq!(
        metas[0],
        Meta {
            source: inbox(7),
            identity: parse::identity(
                "INBOX",
                Some(b"<hdr@example.com>"),
                " 7-Sep-2026 08:05:09 +0000",
                2048,
                "alice@example.com",
                "Invoice 42",
            ),
            received_at: 1788768309,
            size: 2048,
            from: Some(address("Alice", "alice@example.com")),
            reply_to: Some(address("Billing", "billing@example.com")),
            to: vec![address("Me", "me@example.com")],
            cc: vec![address("", "cc@example.org")],
            subject: "Invoice 42".to_owned(),
            message_id: Some("<hdr@example.com>".to_owned()),
            locator: parse::locator(
                Some(b"<hdr@example.com>"),
                " 7-Sep-2026 08:05:09 +0000",
                2048,
                "alice@example.com",
                "Invoice 42",
            ),
            references: Vec::new(),
            date: metas[0].date.clone(),
            signals: Signals {
                list_id: Some("Billing <billing.example.com>".to_owned()),
                list_unsubscribe: true,
                precedence: Some("bulk".to_owned()),
                auto_submitted: Some("auto-generated".to_owned()),
                null_return_path: true,
            },
            category: None,
            text: Some(plain("1", 100)),
            attachments: vec![AttachmentInfo {
                name: "invoice.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 5000,
            }],
        }
    );

    let bob = &metas[1];
    assert_eq!(
        bob.identity,
        parse::identity(
            "INBOX",
            None,
            "01-Sep-2026 00:00:00 +0000",
            10,
            "bob@example.net",
            ""
        )
    );
    assert_eq!(bob.message_id, None);
    assert_eq!(bob.from, Some(address("Bob", "bob@example.net")));
    // A Reply-To the server copied from From adds nothing.
    assert_eq!(bob.reply_to, None);
    assert_eq!(bob.signals, Signals::default());
    assert_eq!(bob.text.as_ref().unwrap().mime, "text/html");
    assert_eq!(bob.subject, "");
    finish(source, fake).await;
}

#[tokio::test]
async fn metadata_falls_back_to_the_envelope_message_id() {
    let reply = format!(
        "* 1 FETCH (UID 4 INTERNALDATE \"01-Sep-2026 00:00:00 +0000\" RFC822.SIZE 10 \
         ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL \"<env@example.com>\") {HEADER_KEY} NIL)\r\n\
         {{tag}} OK\r\n"
    );
    let (mut source, fake) = open(session(vec![command(
        &format!("UID FETCH 4 {METADATA_ITEMS}"),
        &reply,
    )]))
    .await;
    let metas = source.metadata(&[inbox(4)]).await.unwrap();
    assert_eq!(metas[0].message_id.as_deref(), Some("<env@example.com>"));
    assert_eq!(
        metas[0].identity,
        parse::identity(
            "INBOX",
            Some(b"<env@example.com>"),
            "01-Sep-2026 00:00:00 +0000",
            10,
            "",
            ""
        )
    );
    assert_eq!(metas[0].text, None);
    finish(source, fake).await;
}

#[tokio::test]
async fn oversized_metadata_is_cut_to_its_bounds() {
    use crate::email::source::{
        CUT, MAX_ADDRESS_BYTES, MAX_LABEL_BYTES, MAX_NAME_BYTES, MAX_SUBJECT_BYTES,
    };
    let literal = |fill: &str, len: usize| format!("{{{len}}}\r\n{}", fill.repeat(len));
    let header = format!("List-Id: {}\r\n\r\n", "L".repeat(30_000));
    let reply = format!(
        "* 1 FETCH (UID 4 INTERNALDATE \"01-Sep-2026 00:00:00 +0000\" RFC822.SIZE 10 \
         ENVELOPE (NIL {subject} (({name} NIL {mailbox} \"example.com\")) NIL NIL \
         ((NIL NIL \"me\" \"example.com\")) NIL NIL NIL NIL) \
         BODYSTRUCTURE (\"APPLICATION\" {subtype} NIL NIL NIL \"BASE64\" 5000 NIL \
         (\"ATTACHMENT\" (\"FILENAME\" {file})) NIL NIL) \
         {HEADER_KEY} {{{}}}\r\n{header})\r\n{{tag}} OK\r\n",
        header.len(),
        subject = literal("S", 100_000),
        name = literal("N", 50_000),
        mailbox = literal("m", 10_000),
        subtype = literal("x", 5_000),
        file = literal("F", 20_000),
    );
    let (mut source, fake) = open(session(vec![command(
        &format!("UID FETCH 4 {METADATA_ITEMS}"),
        &reply,
    )]))
    .await;
    let metas = source.metadata(&[inbox(4)]).await.unwrap();
    let meta = &metas[0];
    let from = meta.from.as_ref().unwrap();
    let attachment = &meta.attachments[0];
    for (field, max) in [
        (&meta.subject, MAX_SUBJECT_BYTES),
        (&from.name, MAX_NAME_BYTES),
        (&from.address, MAX_ADDRESS_BYTES),
        (&attachment.name, MAX_NAME_BYTES),
        (&attachment.mime, MAX_LABEL_BYTES),
        (meta.signals.list_id.as_ref().unwrap(), MAX_NAME_BYTES),
    ] {
        assert!(field.len() <= max, "{} > {max}", field.len());
        assert!(field.ends_with(CUT), "{field}");
    }
    assert!(meta.subject.starts_with("SSS"));
    assert_eq!(meta.to[0].address, "me@example.com");
    finish(source, fake).await;
}

#[tokio::test]
async fn messages_sharing_a_message_id_keep_their_own_identities() {
    let header = "Message-ID: <same@example.com>\r\n\r\n";
    let message = |uid: u32, date: &str| {
        format!(
            "* {uid} FETCH (UID {uid} INTERNALDATE \"{date}\" RFC822.SIZE 10 \
             ENVELOPE (NIL \"Hi\" ((NIL NIL \"a\" \"example.com\")) NIL NIL NIL NIL NIL NIL NIL) \
             {HEADER_KEY} {{{}}}\r\n{header})\r\n",
            header.len()
        )
    };
    let reply = format!(
        "{}{}{{tag}} OK\r\n",
        message(4, "01-Sep-2026 00:00:00 +0000"),
        message(5, "01-Sep-2026 00:00:07 +0000")
    );
    let (mut source, fake) = open(session(vec![command(
        &format!("UID FETCH 4,5 {METADATA_ITEMS}"),
        &reply,
    )]))
    .await;
    let metas = source.metadata(&[inbox(4), inbox(5)]).await.unwrap();
    assert_eq!(metas[0].message_id, metas[1].message_id);
    assert_ne!(metas[0].identity, metas[1].identity);
    finish(source, fake).await;
}

#[tokio::test]
async fn metadata_fetches_at_most_64_messages_per_command() {
    let batch = |uids: std::ops::RangeInclusive<u32>| {
        let set: Vec<String> = uids.clone().map(|uid| uid.to_string()).collect();
        let reply: String = uids
            .map(|uid| format!("* {uid} FETCH (UID {uid} RFC822.SIZE {uid})\r\n"))
            .collect();
        command(
            &format!("UID FETCH {} {METADATA_ITEMS}", set.join(",")),
            &format!("{reply}{{tag}} OK\r\n"),
        )
    };
    let (mut source, fake) = open(session(vec![batch(1..=64), batch(65..=70)])).await;
    let refs: Vec<SourceRef> = (1..=70).rev().map(inbox).collect();
    let metas = source.metadata(&refs).await.unwrap();
    assert_eq!(metas.len(), 70);
    assert_eq!(metas[0].source, inbox(70));
    assert_eq!(metas[0].size, 70);
    assert_eq!(metas[69].source, inbox(1));
    finish(source, fake).await;
}

#[tokio::test]
async fn metadata_for_another_selection_sends_nothing() {
    let (mut source, fake) = open(session(vec![])).await;
    let stale = SourceRef::Imap {
        mailbox: "INBOX".to_owned(),
        uidvalidity: 1,
        uid: 3,
    };
    assert!(source.metadata(&[stale]).await.unwrap().is_empty());
    assert!(source.metadata(&[]).await.unwrap().is_empty());
    assert_eq!(finish(source, fake).await.len(), 3);
}

#[tokio::test]
async fn text_peeks_at_a_partial_section() {
    let (mut source, fake) = open(session(vec![
        command(
            "UID FETCH 7 (UID BODY.PEEK[1.2]<0.100>)",
            "* 5 FETCH (UID 7 BODY[1.2]<0> {5}\r\nhello)\r\n{tag} OK\r\n",
        ),
        command(
            "UID FETCH 7 (UID BODY.PEEK[2]<0.100>)",
            "* 5 FETCH (UID 7 BODY[2]<0> \"<p>hi</p>\")\r\n{tag} OK\r\n",
        ),
        command(
            "UID FETCH 7 (UID BODY.PEEK[1]<0.3>)",
            "* 5 FETCH (UID 7 BODY[1]<0> \"hello\")\r\n{tag} OK\r\n",
        ),
    ]))
    .await;
    assert_eq!(
        source
            .text(&inbox(7), &plain("1.2", 300), 100)
            .await
            .unwrap(),
        Some(PartText {
            text: "hello".to_owned(),
            html: false,
            truncated: true,
        })
    );
    let html = PartRef {
        id: "2".to_owned(),
        mime: "text/html".to_owned(),
        charset: None,
        encoding: parse::TransferEncoding::SevenBit,
        size: 50,
    };
    assert_eq!(
        source.text(&inbox(7), &html, 100).await.unwrap(),
        Some(PartText {
            text: "<p>hi</p>".to_owned(),
            html: true,
            truncated: false,
        })
    );
    // A server that sends more than asked is cut to the limit.
    assert_eq!(
        source.text(&inbox(7), &plain("1", 3), 3).await.unwrap(),
        Some(PartText {
            text: "hel".to_owned(),
            html: false,
            truncated: true,
        })
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn text_of_a_message_that_is_gone_is_none() {
    let (mut source, fake) = open(session(vec![command(
        "UID FETCH 8 (UID BODY.PEEK[1]<0.100>)",
        "{tag} OK nothing\r\n",
    )]))
    .await;
    assert_eq!(
        source.text(&inbox(8), &plain("1", 10), 100).await.unwrap(),
        None
    );
    let stale = SourceRef::Imap {
        mailbox: "INBOX".to_owned(),
        uidvalidity: 6,
        uid: 8,
    };
    assert_eq!(
        source.text(&stale, &plain("1", 10), 100).await.unwrap(),
        None
    );
    finish(source, fake).await;
}

#[tokio::test]
async fn text_refuses_a_part_that_is_not_a_part_number() {
    let (mut source, fake) = open(session(vec![])).await;
    for id in ["", "TEXT", "1]<0.1> BODY[1", "1 (FLAGS)"] {
        assert!(source.text(&inbox(7), &plain(id, 10), 100).await.is_err());
    }
    assert!(!source.client.is_broken());
    assert_eq!(finish(source, fake).await.len(), 3);
}
