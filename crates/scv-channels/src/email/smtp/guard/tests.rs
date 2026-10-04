//! Unit tests for `src/email/smtp/guard.rs`.

use super::*;

const SECRET: &str = "hunter2-SECRET";

/// A send-mode guard for one approved envelope.
fn envelope(from: &str, to: &[&str]) -> Guard {
    Guard::new(Mode::Send {
        from: from.to_owned(),
        to: to.iter().copied().map(str::to_owned).collect(),
    })
}

fn refused(verb: &str) -> Result<(), SmtpViolation> {
    Err(SmtpViolation {
        verb: verb.to_owned(),
    })
}

fn assert_display(violation: &SmtpViolation, verb: &str) {
    let message = violation.to_string();
    assert_eq!(
        message,
        format!("refused to send an SMTP {verb} command this session may not send"),
        "{violation:?}"
    );
    assert!(!message.contains(SECRET), "{message}");
    assert_eq!(violation.verb, verb);
}

/// Verbs that must never be written, in either mode.
const REFUSED: &[(&str, &str)] = &[
    ("MAIL FROM:<stranger@example.com>", "MAIL"),
    ("mail from:<stranger@example.com>", "MAIL"),
    ("RCPT TO:<stranger@example.com>", "RCPT"),
    ("rcpt to:<stranger@example.com>", "RCPT"),
    ("DATA", "DATA"),
    ("data", "DATA"),
    ("VRFY stranger@example.com", "VRFY"),
    ("EXPN list", "EXPN"),
    ("ETRN example.com", "ETRN"),
    ("BDAT 1 LAST", "BDAT"),
    ("HELP", "HELP"),
    ("HELO localhost", "HELO"),
    ("XYZZY", "XYZZY"),
    ("AUTH PLAIN", "AUTH"),
    ("AUTH PLAIN one two", "AUTH"),
    ("AUTH CRAM-MD5", "AUTH"),
    ("STARTTLS NOW", "STARTTLS"),
];

#[test]
fn verify_mode_allows_ehlo_starttls_auth_quit_and_noop() {
    for line in [
        "EHLO",
        "EHLO localhost",
        "ehlo localhost",
        "STARTTLS",
        "starttls",
        "QUIT",
        "quit",
        "NOOP",
        "noop",
        "AUTH PLAIN dGVzdA==",
        "auth plain dGVzdA==",
        "AUTH LOGIN",
        "auth login",
    ] {
        assert!(
            Guard::new(Mode::Verify).check(line).is_ok(),
            "{line} should be allowed while signing in"
        );
    }
}

#[test]
fn verify_and_send_mode_refuse_mail_probes_and_unknown_verbs() {
    let mut verify = Guard::new(Mode::Verify);
    for (line, verb) in REFUSED {
        assert_eq!(verify.check(line), refused(verb), "{line}");
    }

    let mut before = envelope("a@b.c", &["c@d.e"]);
    for (line, verb) in REFUSED {
        assert_eq!(before.check(line), refused(verb), "before MAIL: {line}");
    }
    assert!(before.check("MAIL FROM:<a@b.c>").is_ok());
    for (line, verb) in REFUSED {
        assert_eq!(before.check(line), refused(verb), "after MAIL: {line}");
    }
}

#[test]
fn auth_login_accepts_exactly_two_base64_continuations() {
    let mut guard = Guard::new(Mode::Verify);
    assert_eq!(
        guard.continuation("YQ=="),
        refused("(AUTH line)"),
        "a continuation needs a preceding AUTH LOGIN"
    );
    assert!(guard.check("AUTH LOGIN").is_ok());
    assert_eq!(
        guard.continuation("not base64!"),
        refused("(AUTH line)"),
        "a bad line must not use up a continuation"
    );
    assert_eq!(guard.continuation(""), refused("(AUTH line)"));
    assert_eq!(guard.continuation("YQ==\n"), refused("(AUTH line)"));
    assert_eq!(guard.continuation("has space"), refused("(AUTH line)"));
    assert!(guard.continuation("YQ==").is_ok(), "username");
    assert!(guard.continuation("ab+/12==").is_ok(), "password");
    assert_eq!(
        guard.continuation("YQ=="),
        refused("(AUTH line)"),
        "AUTH LOGIN takes two continuation lines, not three"
    );

    let mut again = Guard::new(Mode::Verify);
    assert!(again.check("auth login").is_ok());
    assert!(again.continuation("++//").is_ok());
    assert!(again.continuation("YQ==").is_ok());
    assert!(again.continuation("YQ==").is_err());
}

#[test]
fn send_mode_accepts_the_approved_envelope_once_and_in_order() {
    let mut guard = envelope("Ann@Example.com", &["bob@example.com", "cara@example.com"]);
    assert!(guard.check("EHLO localhost").is_ok());
    assert!(guard.check("NOOP").is_ok());
    assert!(guard.check("QUIT").is_ok());
    assert_eq!(guard.check("RCPT TO:<bob@example.com>"), refused("RCPT"));
    assert_eq!(guard.check("DATA"), refused("DATA"));
    assert_eq!(guard.data(), refused("(data)"));
    assert_eq!(guard.check("MAIL FROM:<ann@example.com>"), refused("MAIL"));
    assert_eq!(
        guard.check("MAIL FROM:<Ann@Example.com> SIZE=100"),
        refused("MAIL")
    );
    assert!(guard.check("MAIL FROM:<Ann@Example.com>").is_ok());
    assert_eq!(guard.check("MAIL FROM:<Ann@Example.com>"), refused("MAIL"));
    assert_eq!(guard.check("RCPT TO:<dave@example.com>"), refused("RCPT"));
    assert_eq!(guard.check("RCPT TO:<bob@example.com> "), refused("RCPT"));
    assert_eq!(
        guard.check("RCPT TO:<bob@example.com>extra>"),
        refused("RCPT")
    );
    assert!(guard.check("RCPT TO:<cara@example.com>").is_ok());
    assert_eq!(guard.check("RCPT TO:<cara@example.com>"), refused("RCPT"));
    assert_eq!(
        guard.check("DATA"),
        refused("DATA"),
        "DATA waits until every approved recipient was named"
    );
    assert!(guard.check("rcpt to:<bob@example.com>").is_ok());
    assert!(guard.check("DATA").is_ok());
    assert!(guard.data().is_ok());
}

#[test]
fn the_body_is_written_only_once_after_data_and_rset_is_then_refused() {
    let mut guard = envelope("a@b.c", &["c@d.e"]);
    assert_eq!(guard.data(), refused("(data)"));
    assert!(guard.check("RSET").is_ok());
    assert!(guard.check("MAIL FROM:<a@b.c>").is_ok());
    assert!(
        guard.check("RSET").is_ok(),
        "RSET is allowed before the body"
    );
    assert_eq!(guard.data(), refused("(data)"));
    assert!(guard.check("RCPT TO:<c@d.e>").is_ok());
    assert_eq!(guard.data(), refused("(data)"), "the body waits for DATA");
    assert!(guard.check("DATA").is_ok());
    assert!(guard.data().is_ok());
    assert_eq!(guard.data(), refused("(data)"));
    assert_eq!(guard.check("RSET"), refused("RSET"));
    assert_eq!(guard.check("DATA"), refused("DATA"));
    assert_eq!(guard.check("MAIL FROM:<a@b.c>"), refused("MAIL"));
}

#[test]
fn starttls_and_auth_are_refused_after_mail_from() {
    let mut guard = envelope("a@b.c", &["c@d.e"]);
    assert!(guard.check("STARTTLS").is_ok());
    assert!(guard.check("AUTH PLAIN dGVzdA==").is_ok());
    assert!(guard.check("AUTH LOGIN").is_ok());
    assert!(guard.continuation("YQ==").is_ok());
    assert!(guard.continuation("YQ==").is_ok());
    assert!(guard.check("MAIL FROM:<a@b.c>").is_ok());
    assert_eq!(guard.check("STARTTLS"), refused("STARTTLS"));
    assert_eq!(guard.check("AUTH PLAIN dGVzdA=="), refused("AUTH"));
    assert_eq!(guard.check("AUTH LOGIN"), refused("AUTH"));
    assert_eq!(guard.check("auth login"), refused("AUTH"));
}

#[test]
fn control_characters_non_ascii_and_overlong_lines_are_refused() {
    let mut guard = Guard::new(Mode::Verify);
    for line in [
        "EHLO\rlocalhost",
        "EHLO\nlocalhost",
        "EHLO local\r\nhost",
        "EHLO \u{0}",
        "EHLO \u{1f}",
        "EHLO \u{7f}",
        "EHLO \u{80}",
        "EHLO caf\u{e9}",
        "NOOP\n",
    ] {
        let violation = guard.check(line).expect_err(line);
        let message = violation.to_string();
        assert!(
            !message.contains(['\r', '\n', '\u{0}', '\u{7f}', '\u{e9}']),
            "{line:?} -> {message}"
        );
    }

    let mut fitted = "NOOP".to_owned();
    fitted.push_str(&" ".repeat(1000 - fitted.len()));
    assert_eq!(fitted.len(), 1000);
    assert!(
        guard.check(&fitted).is_ok(),
        "a 1000-byte line is still a command"
    );
    fitted.push(' ');
    assert_eq!(guard.check(&fitted), refused("NOOP"), "1001 bytes");

    let mut sending = envelope("a@b.c", &["c@d.e"]);
    assert!(sending.check("MAIL FROM:<a@b.c>\n").is_err());
    assert!(sending.check("MAIL FROM:<a@b.c>").is_ok());
    assert!(sending.check("RCPT TO:<c@d.e>\r").is_err());
    assert!(
        sending.check("RCPT TO:<c@d.e>").is_ok(),
        "a refused line must not count as that recipient"
    );
}

#[test]
fn a_violation_names_only_the_verb() {
    let mut guard = envelope("a@b.c", &["c@d.e"]);
    for (line, verb) in [
        (&format!("MAIL FROM:<{SECRET}@example.com>")[..], "MAIL"),
        (&format!("RCPT TO:<{SECRET}@example.com>"), "RCPT"),
        (&format!("VRFY {SECRET}"), "VRFY"),
        (&format!("AUTH PLAIN {SECRET} extra"), "AUTH"),
        (&format!("EHLO {SECRET}\r\nRCPT TO:<c@d.e>"), "EHLO"),
    ] {
        let violation = guard.check(line).expect_err(line);
        assert_display(&violation, verb);
        assert!(!format!("{violation:?}").contains(SECRET), "{violation:?}");
    }

    let prefix = format!("RCPT TO:<{SECRET}@example.com>");
    let oversized = format!("{prefix}{}", " ".repeat(1001 - prefix.len()));
    assert_eq!(oversized.len(), 1001);
    let violation = guard.check(&oversized).expect_err("overlong");
    assert_display(&violation, "RCPT");

    let mut signing = Guard::new(Mode::Verify);
    assert!(signing.check("AUTH LOGIN").is_ok());
    let violation = signing.continuation(SECRET).expect_err("token");
    assert_display(&violation, "(AUTH line)");
    assert!(!format!("{violation:?}").contains(SECRET), "{violation:?}");

    let violation = Guard::new(Mode::Verify).data().expect_err("body");
    assert_display(&violation, "(data)");
}

#[test]
fn a_refused_auth_login_does_not_arm_continuation_lines() {
    let mut guard = envelope("a@b.c", &["c@d.e"]);
    assert!(guard.check("MAIL FROM:<a@b.c>").is_ok());
    assert_eq!(guard.check("AUTH LOGIN"), refused("AUTH"));
    assert_eq!(
        guard.continuation("YQ=="),
        refused("(AUTH line)"),
        "AUTH LOGIN was refused, so a continuation line must be refused too"
    );
}

#[test]
fn data_is_accepted_only_once() {
    let mut guard = envelope("a@b.c", &["c@d.e"]);
    assert!(guard.check("MAIL FROM:<a@b.c>").is_ok());
    assert!(guard.check("RCPT TO:<c@d.e>").is_ok());
    assert!(guard.check("DATA").is_ok());
    assert_eq!(
        guard.check("DATA"),
        refused("DATA"),
        "DATA may be sent only once, even before the body"
    );
}
