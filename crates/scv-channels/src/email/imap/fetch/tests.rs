//! Unit tests for `src/email/imap/fetch.rs`.

use super::*;
use crate::email::imap::wire::{self, Response};

/// The FETCH list of `* 1 FETCH <items>`.
fn items(raw: &str) -> Vec<Value> {
    match wire::parse(format!("* 1 FETCH {raw}\r\n").as_bytes()).unwrap() {
        Response::Message { values, .. } => values[0].list().unwrap().to_vec(),
        other => panic!("not a FETCH: {other:?}"),
    }
}

/// A single value, parsed.
fn value(raw: &str) -> Value {
    items(&format!("(X {raw})")).pop().unwrap()
}

fn address(name: &str, address: &str) -> Address {
    Address {
        name: name.to_owned(),
        address: address.to_owned(),
    }
}

#[test]
fn merges_the_items_of_a_fetch_response() {
    let mut fetched = Fetched::default();
    fetched.merge(&items(
        "(uid 42 FLAGS (\\Seen $Forwarded) INTERNALDATE \" 7-Jul-1996 02:44:25 -0700\" \
         rfc822.size 3028 ENVELOPE (NIL \"hi\" NIL NIL NIL NIL NIL NIL NIL NIL) \
         BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 10 1) \
         BODY[HEADER.FIELDS (\"Message-ID\"  LIST-ID)] {8}\r\nX: y\r\n\r\n \
         BODY[1.2]<0> \"part\" BODY[TEXT] NIL MODSEQ (12))",
    ));
    assert_eq!(fetched.uid, Some(42));
    assert_eq!(
        fetched.flags,
        Some(vec!["\\Seen".to_owned(), "$Forwarded".to_owned()])
    );
    assert_eq!(
        fetched.internal_date.as_deref(),
        Some(" 7-Jul-1996 02:44:25 -0700")
    );
    assert_eq!(fetched.size, Some(3028));
    assert!(fetched.envelope.is_some());
    assert!(fetched.structure.is_some());
    assert_eq!(fetched.header_fields(), Some(&b"X: y\r\n\r\n"[..]));
    assert_eq!(fetched.section("1.2"), Some(&b"part"[..]));
    assert_eq!(fetched.section("text"), None);
    assert_eq!(fetched.sections.len(), 3);
}

#[test]
fn later_responses_add_to_earlier_ones() {
    let mut fetched = Fetched::default();
    fetched.merge(&items("(UID 5 RFC822.SIZE 10)"));
    fetched.merge(&items("(FLAGS () BODY[1] \"one\")"));
    fetched.merge(&items("(BODY[1] \"two\")"));
    assert_eq!(fetched.uid, Some(5));
    assert_eq!(fetched.size, Some(10));
    assert_eq!(fetched.flags, Some(vec![]));
    assert_eq!(fetched.section("1"), Some(&b"two"[..]));
    assert_eq!(fetched.sections.len(), 1);
    fetched.merge(&items("(BODY.PEEK[2]<0> \"echoed\")"));
    assert_eq!(fetched.section("2"), Some(&b"echoed"[..]));
}

#[test]
fn the_non_extensible_body_form_stands_in_for_bodystructure() {
    let mut fetched = Fetched::default();
    fetched.merge(&items(
        "(UID 1 BODY (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1))",
    ));
    assert!(fetched.structure.is_some());
    // Odd trailing keys and unknown shapes are skipped.
    fetched.merge(&items("(UID NIL RFC822.SIZE \"x\" FLAGS)"));
    assert_eq!(fetched.uid, Some(1));
}

#[test]
fn section_specs_normalize_case_quotes_and_spacing() {
    assert_eq!(
        normalize_spec("header.fields ( \"Message-ID\"   list-id )"),
        "HEADER.FIELDS (MESSAGE-ID LIST-ID)"
    );
    assert_eq!(normalize_spec("1.2"), "1.2");
    assert_eq!(normalize_spec(""), "");
    let mut fetched = Fetched::default();
    fetched.merge(&items("(BODY[HEADER.FIELDS.NOT (RECEIVED)] \"a\")"));
    assert_eq!(fetched.header_fields(), None);
}

#[test]
fn reads_the_rfc_3501_envelope() {
    let envelope = Envelope::read(&value(
        "(\"Wed, 17 Jul 1996 02:23:25 -0700 (PDT)\" \"IMAP4rev1 WG mtg summary and minutes\" \
         ((\"Terry Gray\" NIL \"gray\" \"cac.washington.edu\")) \
         ((\"Terry Gray\" NIL \"gray\" \"cac.washington.edu\")) \
         ((\"Terry Gray\" NIL \"gray\" \"cac.washington.edu\")) \
         ((NIL NIL \"imap\" \"cac.washington.edu\")) \
         ((NIL NIL \"minutes\" \"CNRI.Reston.VA.US\")(\"John Klensin\" NIL \"KLENSIN\" \"MIT.EDU\")) \
         NIL NIL \"<B27397-0100000@cac.washington.edu>\")",
    ));
    assert_eq!(
        envelope,
        Envelope {
            date: Some("Wed, 17 Jul 1996 02:23:25 -0700 (PDT)".to_owned()),
            subject: "IMAP4rev1 WG mtg summary and minutes".to_owned(),
            from: vec![address("Terry Gray", "gray@cac.washington.edu")],
            reply_to: vec![address("Terry Gray", "gray@cac.washington.edu")],
            to: vec![address("", "imap@cac.washington.edu")],
            cc: vec![
                address("", "minutes@CNRI.Reston.VA.US"),
                address("John Klensin", "KLENSIN@MIT.EDU"),
            ],
            message_id: Some(b"<B27397-0100000@cac.washington.edu>".to_vec()),
        }
    );
}

#[test]
fn skips_group_markers_and_reads_literals_and_lowercase_nil() {
    let envelope = Envelope::read(&value(
        "(nil {7}\r\nSubject NIL NIL NIL \
         ((NIL NIL \"undisclosed-recipients\" NIL)(NIL NIL NIL NIL)(\"A\" nil \"a\" \"b.example\")) \
         ((NIL NIL NIL \"no-mailbox.example\")) NIL nil NIL)",
    ));
    assert_eq!(envelope.date, None);
    assert_eq!(envelope.subject, "Subject");
    assert_eq!(envelope.to, vec![address("A", "a@b.example")]);
    assert_eq!(envelope.cc, vec![address("", "")]);
    assert_eq!(envelope.message_id, None);
}

#[test]
fn a_missing_or_short_envelope_is_empty() {
    assert_eq!(Envelope::read(&Value::Nil), Envelope::default());
    assert_eq!(
        Envelope::read(&value("(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)")),
        Envelope::default()
    );
    let short = Envelope::read(&value("(\"d\" \"s\" (\"not an address\"))"));
    assert_eq!(short.subject, "s");
    assert!(short.from.is_empty());
}

#[test]
fn internal_dates_become_unix_seconds() {
    assert_eq!(
        internal_date_seconds("17-Jul-1996 02:44:25 -0700"),
        Some(837596665)
    );
    assert_eq!(
        internal_date_seconds(" 7-Sep-2026 08:05:09 +0000"),
        Some(1788768309)
    );
    assert_eq!(
        internal_date_seconds("7-sep-2026 16:35:09 +0830"),
        Some(1788768309)
    );
    assert_eq!(internal_date_seconds("01-Jan-1970 00:00:00 +0000"), Some(0));
    assert_eq!(
        internal_date_seconds("29-Feb-2024 00:00:00 +0000"),
        Some(1709164800)
    );
    for bad in [
        "",
        "garbage",
        "17-Jux-1996 02:44:25 -0700",
        "32-Jul-1996 02:44:25 -0700",
        "17-Jul-96 02:44:25 -0700",
        "17-Jul-1996 24:00:00 +0000",
        "17-Jul-1996 02:44:25 0700",
        "17-Jul-1996 02:44 -0700",
        "17-Jul-1996 02:44:25 -07:00",
        "17-Jul-1996-1 02:44:25 -0700",
        "31-Dec-1969 23:59:59 +0000",
        "1-Jan-1970 00:00:00 +0100",
    ] {
        assert_eq!(internal_date_seconds(bad), None, "{bad}");
    }
}

#[test]
fn search_dates_name_the_utc_day() {
    assert_eq!(search_date(0), "1-Jan-1970");
    assert_eq!(search_date(1709164800 + 86_399), "29-Feb-2024");
    assert_eq!(search_date(1790596800 - 3 * 86_400), "25-Sep-2026");
    for days in [-800_000, -1, 0, 59, 60, 365, 11_016, 20_000, 2_932_896] {
        let (year, month, day) = civil_from_days(days);
        assert_eq!(days_from_civil(year, month, day), days);
    }
}
