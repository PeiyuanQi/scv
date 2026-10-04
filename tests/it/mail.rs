//! Mail from the command line: signing a mailbox in, refusing what a
//! mailbox or a mail chat must not take, and showing their settings.

use crate::support::{Isolated, write_private};
use std::io::Write as _;
use std::process::{Command, Stdio};

fn scv(home: &std::path::Path, args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_scv"))
        .isolated(home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    match child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(error) => panic!("write stdin: {error}"),
    }
    let output = child.wait_with_output().unwrap();
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains("code-secret"), "{text}");
    }
    output
}

fn stderr(output: &std::process::Output) -> String {
    assert!(!output.status.success(), "the command should fail");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Email sign-in takes the password only from stdin or a hidden prompt,
/// refuses the chat channels' options and mismatched IMAP, SMTP, and OAuth
/// options, and saves nothing it could not check; `scv mail cancel` names
/// one action or all of them.
#[test]
fn email_sign_in_checks_the_mailbox_before_saving_and_takes_no_chat_options() {
    let home = tempfile::tempdir().unwrap();
    // A port nothing listens on: the check fails, quickly and offline.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let failed = scv(
        home.path(),
        &[
            "channels",
            "login",
            "email",
            "--imap-host",
            "127.0.0.1",
            "--imap-port",
            &port,
            "--user",
            "me@example.com",
        ],
        "code-secret\n",
    );
    assert!(
        stderr(&failed).contains("could not sign in to the mailbox"),
        "{}",
        stderr(&failed)
    );
    assert!(!home.path().join("credentials/email/default.json").exists());
    for (args, expected) in [
        (
            vec!["channels", "login", "email", "--user", "me"],
            "--imap-host",
        ),
        (
            vec![
                "channels",
                "login",
                "email",
                "--imap-host",
                "imap.example.com",
                "--user",
                "me",
                "--login-url",
                "https://x.invalid",
            ],
            "chat options",
        ),
        (
            vec![
                "channels",
                "login",
                "wechat",
                "--imap-host",
                "imap.example.com",
            ],
            "email options",
        ),
        (
            vec!["channels", "run", "email", "--remote-tools", "owner"],
            "email account takes no",
        ),
        (
            vec!["channels", "run", "email", "--purpose", "mail"],
            "email account takes no",
        ),
        (
            vec![
                "channels",
                "login",
                "email",
                "--imap-host",
                "h",
                "--password",
                "code-secret",
            ],
            "--password",
        ),
        (
            vec![
                "channels",
                "login",
                "email",
                "--oauth",
                "gmail",
                "--imap-host",
                "imap.example.com",
            ],
            "cannot be used with",
        ),
        (
            vec![
                "channels",
                "login",
                "email",
                "--smtp-host",
                "smtp.example.com",
            ],
            "--imap-host",
        ),
        (vec!["channels", "login", "email", "--write"], "--oauth"),
        (
            vec!["channels", "login", "email", "--oauth", "gmail"],
            "--client-id",
        ),
        (
            vec![
                "channels",
                "login",
                "email",
                "--oauth",
                "gmail",
                "--client-id",
                "client",
                "--tenant",
                "consumers",
            ],
            "--tenant is for --oauth outlook",
        ),
        (vec!["mail", "cancel"], "<ID>"),
        (vec!["mail", "cancel", "a1", "--all"], "cannot be used with"),
    ] {
        let error = stderr(&scv(home.path(), &args, "code-secret\n"));
        assert!(error.contains(expected), "{args:?}: {error}");
    }
}

/// `scv config show` says what each mail account and mail chat is set to
/// do, reports a broken mail table against that account alone, and never
/// prints the mailbox's secret.
#[test]
fn config_show_describes_mail_accounts_and_mail_chats() {
    let home = tempfile::tempdir().unwrap();
    write_private(
        &home.path().join("credentials/email/default.json"),
        r#"{"provider":"imap","host":"imap.example.com","port":993,"username":"me@example.com","password":"code-secret"}"#,
    );
    write_private(
        &home.path().join("credentials/email/broken.json"),
        r#"{"provider":"imap","host":"imap.example.com","port":993,"username":"you@example.com","password":"code-secret"}"#,
    );
    write_private(
        &home.path().join("credentials/email/acting.json"),
        r#"{"provider":"imap","host":"imap.example.com","port":993,"username":"them@example.com","password":"code-secret","smtp":{"host":"smtp.example.com","port":465,"security":"tls"}}"#,
    );
    write_private(
        &home.path().join("config.toml"),
        "[channels.email.default.mail]\nmax_tokens_per_day = 50000\n\
         [channels.email.default.mail.notify]\nroute = [\"feishu:mail\"]\n\
         [channels.email.broken.mail]\nmax_body_kib = 999\n\
         [channels.email.acting.mail]\nmax_tokens_per_day = 0\n\
         [channels.email.acting.mail.notify]\nroute = [\"feishu:mail\"]\n\
         [channels.email.acting.mail.actions]\nsend = \"approve\"\ntrash = \"approve\"\n\
         [channels.feishu.mail]\npurpose = \"mail\"\n",
    );
    let output = scv(home.path(), &["config", "show"], "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "email:default",
        "enabled, reads mail, watches \"INBOX\", reports to feishu:mail; triage up to 50000 \
         tokens a day, 8 KiB of each body; 0 rules; read-only",
        "email:acting",
        "enabled, reads mail, watches \"INBOX\", reports to feishu:mail; no model triage; 0 \
         rules; actions on approval: send, trash",
        "email:broken",
        "invalid mail settings: mail.max_body_kib must be between 1 and 64",
        "feishu:mail",
        "mail chat: carries only mail reports, no model answers",
    ] {
        assert!(
            shown.contains(expected),
            "missing {expected:?} in:\n{shown}"
        );
    }
    for address in ["me@example.com", "them@example.com"] {
        assert!(!shown.contains(address), "{shown}");
    }
}
