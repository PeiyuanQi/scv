//! Unit tests for `src/email/imap/guard.rs`.

use super::*;

fn text(command: &str) -> Vec<Part> {
    vec![Part::Text(command.to_owned())]
}

fn verdict(command: &str) -> Result<String, GuardViolation> {
    check(&Mode::ReadOnly, &text(command))
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
    assert_eq!(check(&Mode::ReadOnly, &append), refused("APPEND"));
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
    assert_eq!(check(&Mode::ReadOnly, &literal), refused("UID SEARCH"));
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
        assert!(check(&Mode::ReadOnly, parts).is_ok(), "{parts:?}");
    }
    // A literal where no string belongs is refused.
    let misplaced = [
        Part::Text("A1 NOOP ".to_owned()),
        Part::Literal(b"x".to_vec()),
    ];
    assert_eq!(check(&Mode::ReadOnly, &misplaced), refused("NOOP"));
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
        check(&Mode::ReadOnly, &[Part::Literal(b"A1 NOOP".to_vec())]),
        refused(MALFORMED)
    );
    assert_eq!(check(&Mode::ReadOnly, &[]), refused(MALFORMED));
    assert_eq!(
        check(
            &Mode::ReadOnly,
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
        check(&Mode::ReadOnly, &parts)
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

fn writing() -> Targets {
    Targets {
        source: Some("INBOX".to_owned()),
        uid: Some(42),
        folder: Some("Trash".to_owned()),
        append: Some(AppendFlags::Draft),
        store: Some(StoreFlag::Seen),
        moves: true,
        copies: true,
    }
}

fn mode(targets: Targets) -> Mode {
    Mode::Write(targets)
}

fn allows(mode: &Mode, command: &str) {
    assert!(
        check(mode, &text(command)).is_ok(),
        "{command} under {mode:?}"
    );
}

fn denies(mode: &Mode, command: &str, verb: &str) {
    assert_eq!(
        check(mode, &text(command)),
        refused(verb),
        "{command} under {mode:?}"
    );
}

fn appended(command: &str, message: &[u8]) -> Vec<Part> {
    vec![
        Part::Text(command.to_owned()),
        Part::Literal(message.to_vec()),
    ]
}

#[test]
fn write_mode_selects_only_the_bound_source() {
    let write = mode(writing());
    allows(&write, "A1 SELECT \"INBOX\"");
    allows(&write, "A1 SELECT INBOX");
    for command in [
        "A1 SELECT \"Trash\"",
        "A1 SELECT Sent",
        "A1 SELECT \"inbox\"",
    ] {
        denies(&write, command, "SELECT");
    }
    denies(&Mode::ReadOnly, "A1 SELECT \"INBOX\"", "SELECT");
    denies(
        &Mode::Write(Targets::default()),
        "A1 SELECT \"INBOX\"",
        "SELECT",
    );
}

#[test]
fn write_mode_moves_and_copies_only_the_bound_uid_into_the_bound_folder() {
    let write = mode(writing());
    for command in [
        "A1 UID MOVE 42 \"Trash\"",
        "A1 UID MOVE 42 Trash",
        "A1 UID COPY 42 \"Trash\"",
        "A1 UID COPY 42 Trash",
    ] {
        allows(&write, command);
    }
    for command in [
        "A1 UID MOVE 1:* \"Trash\"",
        "A1 UID MOVE 5,6 \"Trash\"",
        "A1 UID MOVE 43 \"Trash\"",
        "A1 UID MOVE 042 \"Trash\"",
        "A1 UID MOVE 42 \"Archive\"",
        "A1 UID MOVE 42 Archive",
        "A1 UID COPY 1:* \"Trash\"",
        "A1 UID COPY 5,6 \"Trash\"",
        "A1 UID COPY 42 \"Sent\"",
    ] {
        let verb = if command.to_ascii_uppercase().contains("COPY") {
            "UID COPY"
        } else {
            "UID MOVE"
        };
        denies(&write, command, verb);
    }
    let mut no_move = writing();
    no_move.moves = false;
    let no_move = mode(no_move);
    denies(&no_move, "A1 UID MOVE 42 \"Trash\"", "UID MOVE");
    allows(&no_move, "A1 UID COPY 42 \"Trash\"");
    let mut no_copy = writing();
    no_copy.copies = false;
    let no_copy = mode(no_copy);
    denies(&no_copy, "A1 UID COPY 42 \"Trash\"", "UID COPY");
    allows(&no_copy, "A1 UID MOVE 42 \"Trash\"");
}

#[test]
fn write_mode_adds_only_the_bound_flag_and_only_silently() {
    let write = mode(writing());
    allows(&write, "A1 UID STORE 42 +FLAGS.SILENT (\\Seen)");
    allows(&write, "A1 UID STORE 42 +flags.silent (\\seen)");
    for command in [
        "A1 UID STORE 42 -FLAGS.SILENT (\\Seen)",
        "A1 UID STORE 42 -FLAGS (\\Seen)",
        "A1 UID STORE 42 FLAGS (\\Seen)",
        "A1 UID STORE 42 FLAGS.SILENT (\\Seen)",
        "A1 UID STORE 42 +FLAGS (\\Seen)",
        "A1 UID STORE 42 +FLAGS.SILENT (\\Deleted)",
        "A1 UID STORE 42 +FLAGS.SILENT (\\Flagged)",
        "A1 UID STORE 42 +FLAGS.SILENT (\\Seen \\Deleted)",
        "A1 UID STORE 42 +FLAGS.SILENT \\Seen",
        "A1 UID STORE 1:* +FLAGS.SILENT (\\Seen)",
        "A1 UID STORE 5,6 +FLAGS.SILENT (\\Seen)",
        "A1 UID STORE 43 +FLAGS.SILENT (\\Seen)",
    ] {
        denies(&write, command, "UID STORE");
    }
    let mut deleted = writing();
    deleted.store = Some(StoreFlag::Deleted);
    allows(
        &mode(deleted.clone()),
        "A1 UID STORE 42 +FLAGS.SILENT (\\Deleted)",
    );
    denies(
        &mode(deleted),
        "A1 UID STORE 42 +FLAGS.SILENT (\\Seen)",
        "UID STORE",
    );
    denies(
        &Mode::ReadOnly,
        "A1 UID STORE 42 +FLAGS.SILENT (\\Seen)",
        "UID STORE",
    );
}

#[test]
fn write_mode_expunges_only_the_bound_uid_when_a_copy_is_allowed() {
    let write = mode(writing());
    allows(&write, "A1 UID EXPUNGE 42");
    for command in [
        "A1 UID EXPUNGE 1:*",
        "A1 UID EXPUNGE 5,6",
        "A1 UID EXPUNGE 43",
        "A1 UID EXPUNGE 42:42",
    ] {
        denies(&write, command, "UID EXPUNGE");
    }
    let mut no_copy = writing();
    no_copy.copies = false;
    denies(&mode(no_copy), "A1 UID EXPUNGE 42", "UID EXPUNGE");
    denies(&Mode::ReadOnly, "A1 UID EXPUNGE 42", "UID EXPUNGE");
}

#[test]
fn write_mode_appends_only_the_bound_literal_to_the_bound_folder() {
    let message = b"From: me\r\n\r\nHi";
    let write = mode(writing());
    for command in [
        "A1 APPEND \"Trash\" (\\Draft \\Seen) ",
        "A1 APPEND Trash (\\draft \\seen) ",
    ] {
        assert_eq!(
            check(&write, &appended(command, message)),
            Ok("APPEND".to_owned()),
            "{command}"
        );
    }
    for command in [
        "A1 APPEND \"Sent\" (\\Draft \\Seen) ",
        "A1 APPEND \"Trash\" (\\Seen \\Draft) ",
        "A1 APPEND \"Trash\" (\\Draft) ",
        "A1 APPEND \"Trash\" (\\Seen) ",
        "A1 APPEND \"Trash\" (\\Draft \\Seen \\Flagged) ",
        "A1 APPEND \"Trash\" ",
    ] {
        assert_eq!(
            check(&write, &appended(command, message)),
            refused("APPEND"),
            "{command}"
        );
    }
    assert_eq!(
        check(&write, &text("A1 APPEND \"Trash\" (\\Draft \\Seen) \"hi\"")),
        refused("APPEND"),
        "a quoted message is not the literal"
    );
    let trailed = vec![
        Part::Text("A1 APPEND \"Trash\" (\\Draft \\Seen) ".to_owned()),
        Part::Literal(message.to_vec()),
        Part::Text(" extra".to_owned()),
    ];
    assert_eq!(check(&write, &trailed), refused("APPEND"));
    let mut seen = writing();
    seen.folder = Some("Sent".to_owned());
    seen.append = Some(AppendFlags::Seen);
    let seen = mode(seen);
    assert!(check(&seen, &appended("A1 APPEND \"Sent\" (\\Seen) ", message)).is_ok());
    assert_eq!(
        check(
            &seen,
            &appended("A1 APPEND \"Sent\" (\\Draft \\Seen) ", message)
        ),
        refused("APPEND")
    );
    assert_eq!(
        check(
            &Mode::ReadOnly,
            &appended("A1 APPEND \"Trash\" (\\Draft \\Seen) ", message)
        ),
        refused("APPEND")
    );
}

#[test]
fn write_mode_and_read_only_refuse_every_other_mailbox_change() {
    let write = mode(writing());
    let banned = [
        ("A1 EXPUNGE", "EXPUNGE"),
        ("A1 CLOSE", "CLOSE"),
        ("A1 DELETE Foo", "DELETE"),
        ("A1 CREATE Foo", "CREATE"),
        ("A1 RENAME Foo Bar", "RENAME"),
        ("A1 STORE 42 +FLAGS.SILENT (\\Seen)", "STORE"),
        ("A1 COPY 42 Trash", "COPY"),
        ("A1 MOVE 42 Trash", "MOVE"),
        ("A1 SELECT \"Archive\"", "SELECT"),
        ("A1 UID MOVE 1:* \"Trash\"", "UID MOVE"),
        ("A1 UID COPY 5,6 \"Trash\"", "UID COPY"),
    ];
    for mode in [&Mode::ReadOnly, &write] {
        for (command, verb) in banned {
            denies(mode, command, verb);
        }
    }
}

#[test]
fn read_only_listing_and_message_id_search_stay_allowed() {
    let write = mode(writing());
    for mode in [&Mode::ReadOnly, &write] {
        allows(mode, "A1 LIST \"\" \"*\" RETURN (SPECIAL-USE)");
        allows(mode, "A1 XLIST \"\" \"*\"");
        allows(mode, "A1 UID SEARCH HEADER MESSAGE-ID \"<x@y>\"");
        denies(mode, "A1 UID SEARCH HEADER SUBJECT \"x\"", "UID SEARCH");
        denies(mode, "A1 UID SEARCH HEADER FROM \"<x@y>\"", "UID SEARCH");
        denies(mode, "A1 LIST \"\" \"*\" RETURN (SUBSCRIBED)", "LIST");
    }
}

#[test]
fn a_backslash_is_accepted_only_as_the_start_of_a_flag() {
    let write = mode(writing());
    allows(&write, "A1 UID STORE 42 +FLAGS.SILENT (\\Seen)");
    for (command, verb) in [
        ("A1 UID STORE 42 +FLAGS.SILENT (\\)", "UID STORE"),
        ("A1 UID STORE 42 +FLAGS.SILENT (\\9)", "UID STORE"),
        ("A1 UID STORE 42 +FLAGS.SILENT (\\\\Seen)", "UID STORE"),
        ("A1 EXAMINE \\", "EXAMINE"),
    ] {
        denies(&Mode::ReadOnly, command, verb);
        denies(&write, command, verb);
    }
}
