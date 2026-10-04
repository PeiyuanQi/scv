//! Unit tests for `src/email/message.rs`.

use super::*;
use crate::email::content::{Form, Mailbox};
use crate::email::settings::SentCopy;

fn outgoing() -> Outgoing {
    Outgoing {
        form: Form::Reply,
        from: Mailbox {
            name: "Mei Chen".into(),
            address: "me@example.com".into(),
        },
        to: vec!["alice@example.com".into()],
        cc: vec!["bob@example.org".into()],
        subject: "Re: Contract renewal".into(),
        body: "Hi Alice,\nThanks = received.\n.hidden dot line\ntrailing space \n".into(),
        in_reply_to: Some("<m1@example.com>".into()),
        references: vec!["<m0@example.com>".into(), "<m1@example.com>".into()],
        message_id: "<x@example.com>".into(),
        sent_copy: SentCopy::Provider,
        notes: Vec::new(),
    }
}

#[test]
fn a_message_is_its_bound_fields_and_the_date_only() {
    let unix = 20_000 * 86_400 + 9 * 3600;
    let bytes = build(&outgoing(), unix, 8 * 3600);
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert_eq!(
        text,
        "Date: Fri, 04 Oct 2024 17:00:00 +0800\r\n\
         From: \"Mei Chen\" <me@example.com>\r\n\
         To: alice@example.com\r\n\
         Cc: bob@example.org\r\n\
         Subject: Re: Contract renewal\r\n\
         Message-ID: <x@example.com>\r\n\
         In-Reply-To: <m1@example.com>\r\n\
         References: <m0@example.com> <m1@example.com>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\
         \r\n\
         Hi Alice,\r\n\
         Thanks =3D received.\r\n\
         .hidden dot line\r\n\
         trailing space=20\r\n"
    );
    // The same content and date give the same bytes.
    assert_eq!(build(&outgoing(), unix, 8 * 3600), bytes);
}

#[test]
fn non_ascii_headers_are_encoded_words_and_long_ones_fold() {
    let mut message = outgoing();
    message.from.name = "陈美".into();
    message.subject = format!("回复: {}", "合同续签".repeat(12));
    message.to = (0..8).map(|n| format!("person{n}@example.com")).collect();
    message.cc.clear();
    let text = String::from_utf8(build(&message, 0, 0)).unwrap();
    let header: Vec<&str> = text
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .split("\r\n")
        .collect();
    assert!(header.iter().all(|line| line.len() <= 998));
    assert!(header.iter().all(|line| line.is_ascii()), "{header:?}");
    assert!(
        text.contains("From: =?UTF-8?B?6ZmI576O?= <me@example.com>"),
        "{text}"
    );
    let subject_lines: Vec<&&str> = header
        .iter()
        .skip_while(|line| !line.starts_with("Subject:"))
        .take_while(|line| line.starts_with("Subject:") || line.starts_with(' '))
        .collect();
    assert!(
        subject_lines.len() > 1,
        "a long subject folds: {subject_lines:?}"
    );
    assert!(subject_lines.iter().all(|line| line.len() <= 78 + 10));
    let to_lines = header
        .iter()
        .skip_while(|line| !line.starts_with("To:"))
        .take_while(|line| line.starts_with("To:") || line.starts_with(' '))
        .count();
    assert!(to_lines > 1);
    assert!(
        header
            .iter()
            .all(|line| !line.contains('\n') && !line.contains('\r'))
    );
}

#[test]
fn quoted_printable_keeps_lines_short_and_encodes_the_rest() {
    let long = "é".repeat(60);
    let encoded = quoted_printable(&format!("{long}\nplain"));
    for line in encoded.split("\r\n") {
        assert!(line.len() <= 76, "{line}");
    }
    assert!(encoded.starts_with("=C3=A9"));
    assert!(encoded.ends_with("plain\r\n"));
    assert_eq!(quoted_printable(""), "\r\n");
    assert_eq!(quoted_printable("a\r\nb\rc"), "a\r\nb\r\nc\r\n");
}

#[test]
fn dates_follow_rfc_5322_in_any_offset() {
    assert_eq!(date(0, 0), "Thu, 01 Jan 1970 00:00:00 +0000");
    assert_eq!(
        date(951_782_400, -5 * 3600 - 1800),
        "Mon, 28 Feb 2000 18:30:00 -0530"
    );
    assert_eq!(date(951_868_800, 0), "Wed, 01 Mar 2000 00:00:00 +0000");
}
