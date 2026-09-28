//! Unit tests for `src/email/imap/guard.rs`.

use super::*;

fn text(command: &str) -> Vec<Part> {
    vec![Part::Text(command.to_owned())]
}

fn verdict(command: &str) -> Result<String, GuardViolation> {
    check(Mode::ReadOnly, &text(command))
}

fn refused(verb: &str) -> Result<String, GuardViolation> {
    Err(GuardViolation {
        verb: verb.to_owned(),
    })
}

#[test]
fn allows_the_read_only_commands() {
    for command in [
        "A1 CAPABILITY",
        "a0001 capability",
        "A1 NOOP",
        "A1 LOGOUT",
        "A1 ID NIL",
        "A1 ID (\"name\" \"SCV\" \"version\" \"0.3.4\")",
        "A1 ID (\"name\" NIL)",
        "A1 LOGIN user \"pa\\\"ss\\\\word\"",
        "A1 LOGIN \"me@example.com\" \"\"",
        "A1 AUTHENTICATE PLAIN",
        "A1 authenticate plain AG1lAHNlY3JldA==",
        "A1 EXAMINE INBOX",
        "A1 examine \"Sent Items\"",
        "A1 EXAMINE \"&XfJT0ZAB-\"",
        "A1 LIST \"\" \"*\"",
        "A1 LIST \"\" %",
        "A1 STATUS INBOX (MESSAGES UIDNEXT UIDVALIDITY)",
        "A1 UID SEARCH ALL",
        "A1 UID SEARCH UID 42:*",
        "A1 UID SEARCH SINCE 1-Feb-2026",
        "A1 uid search since 28-Sep-2026 UID 1:*",
        "A1 UID FETCH 1,2,5:7 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE RFC822.HEADER)",
        "A1 UID FETCH 5 (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID LIST-ID LIST-UNSUBSCRIBE)])",
        "A1 UID FETCH 5 (UID BODY.PEEK[1.2]<0.4096>)",
        "A1 UID FETCH 5 BODY.PEEK[]",
        "A1 UID FETCH 5:* uid",
        "A1 UID FETCH 5 (body.peek[text] BODY.PEEK[1.MIME] BODY.PEEK[2.HEADER] \
         BODY.PEEK[HEADER.FIELDS.NOT (RECEIVED)] BODY.PEEK[3.1.TEXT])",
    ] {
        assert!(verdict(command).is_ok(), "{command}");
    }
}

#[test]
fn returns_the_verb() {
    assert_eq!(verdict("a1 uid fetch 1 (UID)"), Ok("UID FETCH".to_owned()));
    assert_eq!(verdict("A1 examine INBOX"), Ok("EXAMINE".to_owned()));
}

#[test]
fn refuses_every_mailbox_changing_verb() {
    for (command, verb) in [
        ("A1 SELECT INBOX", "SELECT"),
        ("A1 select INBOX", "SELECT"),
        ("A1 CLOSE", "CLOSE"),
        ("A1 UNSELECT", "UNSELECT"),
        ("A1 EXPUNGE", "EXPUNGE"),
        ("A1 UID EXPUNGE 1:5", "UID EXPUNGE"),
        ("A1 STORE 1 +FLAGS (\\Seen)", "STORE"),
        ("A1 UID STORE 1 +FLAGS.SILENT (\\Deleted)", "UID STORE"),
        ("A1 uid store 1 FLAGS ()", "UID STORE"),
        ("A1 COPY 1 Trash", "COPY"),
        ("A1 UID COPY 1 Trash", "UID COPY"),
        ("A1 MOVE 1 Trash", "MOVE"),
        ("A1 UID MOVE 1 Trash", "UID MOVE"),
        ("A1 APPEND INBOX (\\Seen) \"x\"", "APPEND"),
        ("A1 CREATE Foo", "CREATE"),
        ("A1 DELETE Foo", "DELETE"),
        ("A1 RENAME Foo Bar", "RENAME"),
        ("A1 SUBSCRIBE Foo", "SUBSCRIBE"),
        ("A1 UNSUBSCRIBE Foo", "UNSUBSCRIBE"),
        ("A1 SETACL INBOX user lrswipcda", "SETACL"),
        ("A1 DELETEACL INBOX user", "DELETEACL"),
        ("A1 ENABLE UTF8=ACCEPT", "ENABLE"),
        ("A1 IDLE", "IDLE"),
        ("A1 FETCH 1 (UID)", "FETCH"),
        ("A1 SEARCH ALL", "SEARCH"),
        ("A1 STARTTLS", "STARTTLS"),
        ("A1 AUTHENTICATE XOAUTH2 dGVzdA==", "AUTHENTICATE"),
        ("A1 AUTHENTICATE LOGIN", "AUTHENTICATE"),
        ("A1 AUTHENTICATE CRAM-MD5", "AUTHENTICATE"),
        ("A1 UID XYZZY 1", "UID XYZZY"),
        ("A1 XYZZY", "XYZZY"),
        ("A1 UID", "UID"),
    ] {
        assert_eq!(verdict(command), refused(verb), "{command}");
    }
}

#[test]
fn a_literal_cannot_carry_a_refused_verb_past_the_guard() {
    let append = [
        Part::Text("A1 APPEND INBOX ".to_owned()),
        Part::Literal(b"From: a@b\r\n\r\nhi".to_vec()),
    ];
    assert_eq!(check(Mode::ReadOnly, &append), refused("APPEND"));
}

#[test]
fn refuses_fetch_items_that_set_seen_or_are_unknown() {
    for items in [
        "(BODY[])",
        "(BODY[1])",
        "BODY[TEXT]",
        "(UID RFC822)",
        "(RFC822.TEXT)",
        "(BINARY[1])",
        "(BINARY.PEEK[1])",
        "(BINARY.SIZE[1])",
        "(BODY)",
        "(ALL)",
        "(FAST)",
        "(FULL)",
        "(X-GM-LABELS)",
        "(MODSEQ)",
        "(UID BODY.PEEK[1]<0.0>)",
        "(BODY.PEEK[1]<5>)",
        "(BODY.PEEK[1]<a.5>)",
        "(BODY.PEEK[0])",
        "(BODY.PEEK[01])",
        "(BODY.PEEK[1.])",
        "(BODY.PEEK[MIME])",
        "(BODY.PEEK[HEADER.FIELDS ()])",
        "(BODY.PEEK[HEADER.FIELDS (A\"B)])",
        "(BODY.PEEK[HEADER.FIELDS MESSAGE-ID])",
        "(BODY.PEEK[1]x)",
        "(BODY.PEEK[1)",
        "(UID FLAGS",
        "UID FLAGS)",
        "(UID (FLAGS))",
        "()",
        "",
        "(UID) (FLAGS)",
        "(\"UID\")",
    ] {
        assert_eq!(
            verdict(&format!("A1 UID FETCH 1 {items}")),
            refused("UID FETCH"),
            "{items}"
        );
    }
    for set in ["0", "1:x", "1,,2", "", "-1", "1:", "(UID)"] {
        assert_eq!(
            verdict(&format!("A1 UID FETCH {set} (UID)")),
            refused("UID FETCH"),
            "{set}"
        );
    }
}

#[test]
fn refuses_searches_outside_the_known_keys() {
    for criteria in [
        "",
        "UID",
        "UID 0:*",
        "SINCE yesterday",
        "SINCE 1-February-2026",
        "CHARSET UTF-8 ALL",
        "RETURN (SAVE) ALL",
        "SUBJECT x",
        "NOT SEEN",
        "(ALL)",
    ] {
        assert_eq!(
            verdict(&format!("A1 UID SEARCH {criteria}")),
            refused("UID SEARCH"),
            "{criteria}"
        );
    }
    let literal = [
        Part::Text("A1 UID SEARCH SINCE ".to_owned()),
        Part::Literal(b"1-Jan-2026".to_vec()),
    ];
    assert_eq!(check(Mode::ReadOnly, &literal), refused("UID SEARCH"));
}

#[test]
fn mailbox_and_credential_arguments_may_be_atoms_quoted_or_literals() {
    let allowed: [&[Part]; 4] = [
        &[
            Part::Text("A1 EXAMINE ".to_owned()),
            Part::Literal("Entwürfe".as_bytes().to_vec()),
        ],
        &[
            Part::Text("A1 LOGIN \"me\" ".to_owned()),
            Part::Literal("pässwörd".as_bytes().to_vec()),
        ],
        &[
            Part::Text("A1 LOGIN ".to_owned()),
            Part::Literal(b"me".to_vec()),
            Part::Text(" ".to_owned()),
            Part::Literal(b"\r\nA2 DELETE INBOX".to_vec()),
        ],
        &[
            Part::Text("A1 STATUS ".to_owned()),
            Part::Literal(b"Archive".to_vec()),
            Part::Text(" (MESSAGES)".to_owned()),
        ],
    ];
    for parts in allowed {
        assert!(check(Mode::ReadOnly, parts).is_ok(), "{parts:?}");
    }
    // A literal where no string belongs is refused.
    let misplaced = [
        Part::Text("A1 NOOP ".to_owned()),
        Part::Literal(b"x".to_vec()),
    ];
    assert_eq!(check(Mode::ReadOnly, &misplaced), refused("NOOP"));
}

#[test]
fn refuses_line_breaks_and_malformed_commands() {
    for (command, verb) in [
        (
            "A1 EXAMINE \"INBOX\r\nA2 STORE 1 +FLAGS (\\Deleted)\"",
            "EXAMINE",
        ),
        ("A1 EXAMINE INBOX\r\nA2 EXPUNGE", "EXAMINE"),
        ("A1 EXAMINE INBOX\n", "EXAMINE"),
        ("A1 EXAMINE IN\rBOX", "EXAMINE"),
        ("A1 LOGIN a b\0", "LOGIN"),
        ("A1 EXAMINE caf\u{e9}", "EXAMINE"),
        ("A1 EXAMINE \"unterminated", "EXAMINE"),
        ("A1 EXAMINE \"bad \\x escape\"", "EXAMINE"),
        ("A1 EXAMINE {5}", "EXAMINE"),
        ("A1 EXAMINE INBOX extra", "EXAMINE"),
        ("A1 EXAMINE", "EXAMINE"),
        ("A1 EXAMINE [x]", "EXAMINE"),
        ("A1 NOOP extra", "NOOP"),
        ("A1 LOGIN onlyone", "LOGIN"),
        ("EXAMINE INBOX", "INBOX"),
        ("LOGOUT", MALFORMED),
        ("", MALFORMED),
        ("* NOOP", "NOOP"),
        ("A-1 NOOP", "NOOP"),
        ("\"A1\" NOOP", "NOOP"),
        ("A1 \"NOOP\"", "NOOP"),
        ("A1 NO]OP", "NOOP"),
    ] {
        assert_eq!(verdict(command), refused(verb), "{command:?}");
    }
    assert_eq!(
        check(Mode::ReadOnly, &[Part::Literal(b"A1 NOOP".to_vec())]),
        refused(MALFORMED)
    );
    assert_eq!(check(Mode::ReadOnly, &[]), refused(MALFORMED));
    assert_eq!(
        check(
            Mode::ReadOnly,
            &[
                Part::Text("A1 EXAMINE INBOX".to_owned()),
                Part::Line("x".to_owned())
            ]
        ),
        refused("EXAMINE")
    );
}

#[test]
fn authenticate_takes_one_base64_response() {
    let with_line = |initial: &str, lines: &[&str]| {
        let mut parts = text(&format!("A1 AUTHENTICATE PLAIN{initial}"));
        parts.extend(lines.iter().map(|line| Part::Line((*line).to_owned())));
        check(Mode::ReadOnly, &parts)
    };
    assert!(with_line("", &["AG1lAHNlY3JldA=="]).is_ok());
    for (initial, lines) in [
        (" AG1l", &["AG1l"][..]),
        ("", &["not base64!"]),
        ("", &["AG1l", "AG1l"]),
        ("", &["AG1l\r\nA2 DELETE INBOX"]),
        ("", &[""]),
        (" ***", &[]),
        (" AG1l AG1l", &[]),
    ] {
        assert_eq!(
            with_line(initial, lines),
            refused("AUTHENTICATE"),
            "{initial:?} {lines:?}"
        );
    }
}

#[test]
fn the_violation_names_only_the_verb() {
    let violation = verdict("A1 STORE 1 +FLAGS (secret-subject)").unwrap_err();
    let message = violation.to_string();
    assert!(message.contains("STORE"), "{message}");
    assert!(!message.contains("secret"), "{message}");
    let odd = verdict("A1 ST\u{7f}ORE-ME!! 1").unwrap_err();
    assert!(odd.verb.bytes().all(|byte| byte.is_ascii_graphic()));
}
