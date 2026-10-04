//! Unit tests for `src/email/smtp.rs`.

use super::*;
use tokio::io::{BufReader, DuplexStream};
use tokio::task::JoinHandle;

const SECRET: &str = "hunter2-SECRET";
const GREETING: &str = "220 mail.example.com ESMTP ready\r\n";
/// `\0user\0pass`.
const PLAIN_TOKEN: &str = "AHVzZXIAcGFzcw==";

/// What the scripted server recorded.
#[derive(Debug)]
struct Transcript {
    lines: Vec<String>,
    body: Vec<u8>,
}

enum Step {
    /// Read a command line and send `reply`.
    Answer(String),
    /// Read the DATA body, including the terminating dot, and send `reply`.
    AnswerBody(String),
    /// Read a command line and close.
    Drop,
    /// Read the DATA body and close.
    DropBody,
    /// Read the DATA body and leave the connection open.
    HangBody,
}

fn answer(reply: &str) -> Step {
    Step::Answer(reply.to_owned())
}

fn answer_body(reply: &str) -> Step {
    Step::AnswerBody(reply.to_owned())
}

fn ok() -> Step {
    answer("250 2.0.0 ok\r\n")
}

/// EHLO whose banner is fixed and whose extension lines are `extensions`.
fn ehlo(extensions: &[&str]) -> Step {
    let mut lines = Vec::with_capacity(extensions.len() + 1);
    lines.push("250-mail.example.com Hello".to_owned());
    lines.extend(extensions.iter().map(|line| format!("250-{line}")));
    let last = lines.pop().unwrap().replacen("250-", "250 ", 1);
    lines.push(last);
    answer(&format!("{}\r\n", lines.join("\r\n")))
}

struct Fake {
    task: JoinHandle<Transcript>,
}

/// A scripted SMTP peer. `greeting` is sent first, when present.
fn serve(greeting: Option<&str>, script: Vec<Step>) -> (DuplexStream, Fake) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let greeting = greeting.map(str::to_owned);
    let task = tokio::spawn(run(server, greeting, script));
    (client, Fake { task })
}

impl Fake {
    async fn finish(self) -> Transcript {
        match self.task.await {
            Ok(transcript) => transcript,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
}

async fn run(stream: DuplexStream, greeting: Option<String>, script: Vec<Step>) -> Transcript {
    let mut stream = BufReader::new(stream);
    let mut lines = Vec::new();
    let mut body = Vec::new();
    if let Some(greeting) = greeting {
        write_all(&mut stream, greeting.as_bytes()).await;
    }
    for step in script {
        match step {
            Step::Answer(reply) => {
                let Some(line) = read_line(&mut stream).await else {
                    return Transcript { lines, body };
                };
                lines.push(line);
                write_all(&mut stream, reply.as_bytes()).await;
            }
            Step::AnswerBody(reply) => {
                body = read_body(&mut stream).await;
                write_all(&mut stream, reply.as_bytes()).await;
            }
            Step::Drop => {
                if let Some(line) = read_line(&mut stream).await {
                    lines.push(line);
                }
                return Transcript { lines, body };
            }
            Step::DropBody => {
                body = read_body(&mut stream).await;
                return Transcript { lines, body };
            }
            Step::HangBody => {
                body = read_body(&mut stream).await;
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest).await;
                return Transcript { lines, body };
            }
        }
    }
    while let Some(line) = read_line(&mut stream).await {
        lines.push(line);
    }
    Transcript { lines, body }
}

async fn write_all(stream: &mut BufReader<DuplexStream>, bytes: &[u8]) {
    let writer = stream.get_mut();
    writer.write_all(bytes).await.unwrap();
    writer.flush().await.unwrap();
}

async fn read_line(stream: &mut BufReader<DuplexStream>) -> Option<String> {
    let mut line = Vec::new();
    if stream.read_until(b'\n', &mut line).await.unwrap() == 0 {
        return None;
    }
    let line = String::from_utf8_lossy(&line);
    Some(line.trim_end_matches(['\r', '\n']).to_owned())
}

async fn read_body(stream: &mut BufReader<DuplexStream>) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let mut line = Vec::new();
        if stream.read_until(b'\n', &mut line).await.unwrap() == 0 {
            break;
        }
        let bare = line.strip_suffix(b"\n").unwrap_or(&line);
        let bare = bare.strip_suffix(b"\r").unwrap_or(bare);
        let done = bare == b".";
        body.extend_from_slice(&line);
        if done {
            break;
        }
    }
    body
}

/// Drop `session` so the server observes end-of-file, then collect its transcript.
async fn finish(session: impl Sized, fake: Fake) -> Transcript {
    drop(session);
    fake.finish().await
}

/// `start` failed; `Session` is not `Debug`, so this cannot be `expect_err`.
fn refused_start(result: Result<Session<DuplexStream>>, what: &str) -> anyhow::Error {
    match result {
        Ok(_) => panic!("{what} should have been refused"),
        Err(error) => error,
    }
}

async fn start_session(mode: Mode, script: Vec<Step>) -> (Session<DuplexStream>, Fake) {
    let (stream, fake) = serve(Some(GREETING), script);
    let session = Session::start(stream, mode, false).await.unwrap();
    (session, fake)
}

fn send_to(from: &str, to: &[String]) -> Mode {
    Mode::Send {
        from: from.to_owned(),
        to: to.to_vec(),
    }
}

fn config(username: &str, password: &str) -> SmtpConfig {
    SmtpConfig {
        host: "127.0.0.1".into(),
        port: 2525,
        security: SmtpSecurity::Tls,
        username: username.to_owned(),
        password: password.to_owned(),
    }
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().copied().map(str::to_owned).collect()
}

/// Every way an error may be shown.
fn shown(error: &anyhow::Error) -> String {
    format!("{error}\n{error:#}\n{error:?}")
}

fn assert_clean(text: &str) {
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
}

fn assert_lines(lines: &[String], expected: &[&str]) {
    assert_eq!(lines, strings(expected), "{lines:?}");
}

fn applied() -> Execution {
    Execution::Applied {
        code: OutcomeCode::Applied,
        sent_copy: None,
    }
}

fn not_applied(retry: bool, code: OutcomeCode) -> Execution {
    Execution::NotApplied { retry, code }
}

/// The rules `classify` is checked against, written out separately.
fn expected(stage: Stage, reply: Option<u16>) -> Execution {
    match (stage, reply) {
        (Stage::DataSent, Some(250)) => applied(),
        (Stage::DataSent, Some(_)) => not_applied(false, OutcomeCode::Refused),
        (Stage::DataSent, None) => Execution::Ambiguous,
        (Stage::Auth, Some(code)) if (500..600).contains(&code) => {
            not_applied(false, OutcomeCode::AuthFailed)
        }
        (_, Some(code)) if (400..500).contains(&code) => not_applied(true, OutcomeCode::Refused),
        (_, Some(_)) => not_applied(false, OutcomeCode::Refused),
        (_, None) => not_applied(true, OutcomeCode::Unreachable),
    }
}

#[tokio::test]
async fn a_greeting_other_than_220_is_refused_without_the_servers_text() {
    for code in [200, 221, 421, 554] {
        let (stream, fake) = serve(
            Some(&format!("{code}-try later\r\n{code} {SECRET} closing\r\n")),
            vec![],
        );
        let error = refused_start(
            Session::start(stream, Mode::Verify, false).await,
            "greeting",
        );
        let message = shown(&error);
        assert!(
            message.contains(&format!("refused the connection ({code})")),
            "{message}"
        );
        assert_clean(&message);
        assert!(fake.finish().await.lines.is_empty(), "no EHLO after {code}");
    }
}

#[tokio::test]
async fn ehlo_keeps_every_extension_after_the_banner_line() {
    let banner = format!("250-mail.example.com {SECRET}\r\n");
    let reply = format!(
        "{banner}250-PIPELINING\r\n250-SIZE 35882577\r\n250-STARTTLS\r\n\
         250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n"
    );
    let (session, fake) = start_session(Mode::Verify, vec![answer(&reply)]).await;
    assert_eq!(
        session.extensions,
        strings(&[
            "PIPELINING",
            "SIZE 35882577",
            "STARTTLS",
            "AUTH PLAIN LOGIN",
            "8BITMIME"
        ])
    );
    assert!(session.extensions.iter().all(|line| !line.contains(SECRET)));
    assert!(session.offers("STARTTLS"));
    assert!(session.offers("SIZE"));
    assert!(!session.offers("35882577"));
    assert!(!session.offers("SMTPUTF8"));
    assert!(session.offers_auth("PLAIN"));
    assert!(session.offers_auth("LOGIN"));
    assert!(!session.offers_auth("CRAM-MD5"));
    assert_lines(&finish(session, fake).await.lines, &["EHLO localhost"]);
}

#[tokio::test]
async fn a_session_marked_already_greeted_sends_ehlo_without_waiting() {
    let (stream, fake) = serve(None, vec![ehlo(&["AUTH PLAIN"])]);
    let session = Session::start(stream, Mode::Verify, true).await.unwrap();
    assert_eq!(session.extensions, strings(&["AUTH PLAIN"]));
    assert_lines(&finish(session, fake).await.lines, &["EHLO localhost"]);
}

#[tokio::test]
async fn auth_plain_sends_the_base64_of_nul_user_nul_password() {
    let token = base64::engine::general_purpose::STANDARD.encode("\0user\0pass");
    assert_eq!(token, PLAIN_TOKEN);
    let (mut session, fake) = start_session(
        Mode::Verify,
        vec![ehlo(&["AUTH PLAIN LOGIN"]), answer("235 2.7.0 ok\r\n")],
    )
    .await;
    let reply = session.auth("user", "pass").await.unwrap();
    assert_eq!(reply.code, 235);
    assert_lines(
        &finish(session, fake).await.lines,
        &["EHLO localhost", &format!("AUTH PLAIN {PLAIN_TOKEN}")],
    );
}

#[tokio::test]
async fn auth_login_is_used_only_when_it_is_the_only_mechanism() {
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode("user"),
        "dXNlcg=="
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode("pass"),
        "cGFzcw=="
    );
    let (mut session, fake) = start_session(
        Mode::Verify,
        vec![
            ehlo(&["Auth Login"]),
            answer("334 VXNlcm5hbWU6\r\n"),
            answer("334 UGFzc3dvcmQ6\r\n"),
            answer("235 2.7.0 ok\r\n"),
        ],
    )
    .await;
    assert_eq!(session.extensions, strings(&["AUTH LOGIN"]));
    assert!(!session.offers_auth("PLAIN"));
    let reply = session.auth("user", "pass").await.unwrap();
    assert_eq!(reply.code, 235);
    assert_lines(
        &finish(session, fake).await.lines,
        &["EHLO localhost", "AUTH LOGIN", "dXNlcg==", "cGFzcw=="],
    );

    let (mut session, fake) = start_session(
        Mode::Verify,
        vec![
            ehlo(&["AUTH LOGIN"]),
            answer(&format!("535 5.7.8 {SECRET}\r\n")),
        ],
    )
    .await;
    let reply = session.auth("user", "pass").await.unwrap();
    assert_eq!(reply.code, 535);
    assert_clean(&format!("{reply:?}"));
    assert_lines(
        &finish(session, fake).await.lines,
        &["EHLO localhost", "AUTH LOGIN"],
    );
}

#[tokio::test]
async fn a_nul_in_the_credentials_is_refused_before_anything_is_sent() {
    let (mut session, fake) = start_session(Mode::Verify, vec![ehlo(&["AUTH PLAIN"])]).await;
    for (username, password) in [("us\0er", SECRET), ("user", "hun\0ter2-SECRET")] {
        let error = session.auth(username, password).await.expect_err("NUL");
        let message = shown(&error);
        assert!(message.contains("cannot hold a NUL character"), "{message}");
        assert_clean(&message);
    }
    assert_lines(&finish(session, fake).await.lines, &["EHLO localhost"]);
}

#[tokio::test]
async fn submit_sends_the_envelope_then_dot_stuffed_data_and_a_250_is_applied() {
    let from = "ann@example.com";
    let to = vec!["bob@example.com".to_owned(), "cara@example.com".to_owned()];
    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            ok(),
            ok(),
            ok(),
            answer("354 go ahead\r\n"),
            answer_body("250 2.0.0 queued\r\n"),
        ],
    )
    .await;
    let result = session.submit(from, &to, b"Hello\n.secret\n").await;
    assert_eq!(result.as_ref().unwrap().code, 250);
    assert_eq!(session.stage(), Stage::DataSent);
    assert_eq!(classify(&result, session.stage()), applied());
    let transcript = finish(session, fake).await;
    assert_lines(
        &transcript.lines,
        &[
            "EHLO localhost",
            "MAIL FROM:<ann@example.com>",
            "RCPT TO:<bob@example.com>",
            "RCPT TO:<cara@example.com>",
            "DATA",
        ],
    );
    assert_eq!(transcript.body, b"Hello\r\n..secret\r\n.\r\n");
}

#[tokio::test]
async fn a_4xx_to_mail_may_be_retried_and_a_5xx_to_rcpt_is_final() {
    let from = "ann@example.com";
    let to = vec!["bob@example.com".to_owned()];
    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            answer(&format!("451 4.3.0 {SECRET} try later\r\n")),
        ],
    )
    .await;
    let result = session.submit(from, &to, b"hi\n").await;
    assert_eq!(result.as_ref().unwrap().code, 451);
    assert_eq!(session.stage(), Stage::Envelope);
    assert_eq!(
        classify(&result, session.stage()),
        not_applied(true, OutcomeCode::Refused)
    );
    assert_clean(&format!("{:?}", result.unwrap()));
    assert_lines(
        &finish(session, fake).await.lines,
        &["EHLO localhost", "MAIL FROM:<ann@example.com>"],
    );

    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            ok(),
            answer(&format!("550 5.1.1 {SECRET} no such user\r\n")),
        ],
    )
    .await;
    let result = session.submit(from, &to, b"hi\n").await;
    assert_eq!(result.as_ref().unwrap().code, 550);
    assert_eq!(session.stage(), Stage::Envelope);
    assert_eq!(
        classify(&result, session.stage()),
        not_applied(false, OutcomeCode::Refused)
    );
    assert_clean(&format!("{:?}", result.unwrap()));
    let lines = finish(session, fake).await.lines;
    assert_lines(
        &lines,
        &[
            "EHLO localhost",
            "MAIL FROM:<ann@example.com>",
            "RCPT TO:<bob@example.com>",
        ],
    );
    assert!(lines.iter().all(|line| line != "DATA"), "{lines:?}");
}

#[tokio::test]
async fn the_connection_dropping_before_the_data_may_be_retried() {
    let from = "ann@example.com";
    let to = vec!["bob@example.com".to_owned()];
    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![ehlo(&["AUTH PLAIN"]), ok(), Step::Drop],
    )
    .await;
    let result = session.submit(from, &to, b"hi\n").await;
    let message = shown(result.as_ref().unwrap_err());
    assert!(message.contains("closed the connection"), "{message}");
    assert_eq!(session.stage(), Stage::Envelope);
    assert_eq!(
        classify(&result, session.stage()),
        not_applied(true, OutcomeCode::Unreachable)
    );
    assert_lines(
        &finish(session, fake).await.lines,
        &[
            "EHLO localhost",
            "MAIL FROM:<ann@example.com>",
            "RCPT TO:<bob@example.com>",
        ],
    );
}

#[tokio::test]
async fn the_connection_dropping_after_the_data_was_written_is_ambiguous() {
    let from = "ann@example.com";
    let to = vec!["bob@example.com".to_owned()];
    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            ok(),
            ok(),
            answer(&format!("354 {SECRET} go ahead\r\n")),
            Step::DropBody,
        ],
    )
    .await;
    let result = session.submit(from, &to, b"hi\n").await;
    let message = shown(result.as_ref().unwrap_err());
    assert!(message.contains("closed the connection"), "{message}");
    assert_clean(&message);
    assert_eq!(session.stage(), Stage::DataSent);
    assert_eq!(classify(&result, session.stage()), Execution::Ambiguous);
    let transcript = finish(session, fake).await;
    assert_eq!(transcript.body, b"hi\r\n.\r\n");
    assert!(
        transcript.lines.iter().any(|line| line == "DATA"),
        "{:?}",
        transcript.lines
    );
}

#[tokio::test]
async fn a_554_after_the_data_is_not_applied_and_is_not_retried() {
    let from = "ann@example.com";
    let to = vec!["bob@example.com".to_owned()];
    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            ok(),
            ok(),
            answer("354 go ahead\r\n"),
            answer_body(&format!("554 5.7.1 {SECRET} rejected\r\n")),
        ],
    )
    .await;
    let result = session.submit(from, &to, b"hi\n").await;
    assert_eq!(result.as_ref().unwrap().code, 554);
    assert_eq!(session.stage(), Stage::DataSent);
    assert_eq!(
        classify(&result, session.stage()),
        not_applied(false, OutcomeCode::Refused)
    );
    assert_clean(&format!("{:?}", result.as_ref().unwrap()));
    let transcript = finish(session, fake).await;
    assert_eq!(transcript.body, b"hi\r\n.\r\n");
}

#[tokio::test]
async fn the_servers_reply_text_never_appears_in_an_error_or_display() {
    let (stream, fake) = serve(Some(&format!("{SECRET}\r\n")), vec![]);
    let error = refused_start(
        Session::start(stream, Mode::Verify, false).await,
        "unreadable greeting",
    );
    let message = shown(&error);
    assert!(message.contains("unreadable reply"), "{message}");
    assert_clean(&message);
    fake.finish().await;

    let (stream, fake) = serve(
        Some(&format!("{SECRET}{}\r\n", "A".repeat(MAX_LINE))),
        vec![],
    );
    let error = refused_start(
        Session::start(stream, Mode::Verify, false).await,
        "overlong greeting",
    );
    let message = shown(&error);
    assert!(message.contains("too long"), "{message}");
    assert_clean(&message);
    fake.finish().await;

    let (stream, fake) = serve(
        Some(GREETING),
        vec![answer(&format!("550 {SECRET} no EHLO\r\n"))],
    );
    let error = refused_start(Session::start(stream, Mode::Verify, false).await, "EHLO");
    let message = shown(&error);
    assert!(message.contains("refused EHLO (550)"), "{message}");
    assert_clean(&message);
    assert_lines(&fake.finish().await.lines, &["EHLO localhost"]);

    let (mut session, fake) = start_session(
        Mode::Verify,
        vec![
            ehlo(&["AUTH PLAIN"]),
            answer(&format!("535 5.7.8 {SECRET} bad credentials\r\n")),
        ],
    )
    .await;
    let execution = submit_on(
        &mut session,
        &config("user", "pass"),
        "ann@example.com",
        &["bob@example.com".to_owned()],
        b"hi\n",
    )
    .await;
    assert_eq!(execution, not_applied(false, OutcomeCode::AuthFailed));
    assert_clean(&format!("{execution:?}"));
    let lines = finish(session, fake).await.lines;
    assert_lines(
        &lines,
        &["EHLO localhost", &format!("AUTH PLAIN {PLAIN_TOKEN}")],
    );
    assert!(
        lines.iter().all(|line| !line.starts_with("MAIL ")),
        "{lines:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_server_that_stops_answering_times_out_and_after_the_data_is_ambiguous() {
    let started = tokio::time::Instant::now();
    let (client, _server) = tokio::io::duplex(64);
    let error = refused_start(
        Session::start(client, Mode::Verify, false).await,
        "silent greeting",
    );
    let message = shown(&error);
    assert!(message.contains("did not answer in time"), "{message}");
    assert!(
        started.elapsed() >= COMMAND_TIMEOUT,
        "{:?}",
        started.elapsed()
    );

    let from = "ann@example.com";
    let to = vec!["bob@example.com".to_owned()];
    let started = tokio::time::Instant::now();
    let (mut session, fake) = start_session(
        send_to(from, &to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            ok(),
            ok(),
            answer(&format!("354 {SECRET} go ahead\r\n")),
            Step::HangBody,
        ],
    )
    .await;
    let result = session.submit(from, &to, b"hi\n").await;
    let message = shown(result.as_ref().unwrap_err());
    assert!(message.contains("did not answer in time"), "{message}");
    assert_clean(&message);
    assert!(started.elapsed() >= DATA_TIMEOUT, "{:?}", started.elapsed());
    assert_eq!(session.stage(), Stage::DataSent);
    assert_eq!(classify(&result, session.stage()), Execution::Ambiguous);
    let transcript = finish(session, fake).await;
    assert_eq!(
        transcript.body, b"hi\r\n.\r\n",
        "the timeout is after the data was written"
    );
}

#[tokio::test]
async fn starttls_requires_an_offer_and_a_bare_220() {
    let (session, fake) = start_session(
        Mode::Verify,
        vec![answer(&format!("250 {SECRET} no extensions\r\n"))],
    )
    .await;
    let error = session.starttls().await.expect_err("not offered");
    let message = shown(&error);
    assert!(message.contains("does not offer STARTTLS"), "{message}");
    assert_clean(&message);
    assert_lines(&fake.finish().await.lines, &["EHLO localhost"]);

    let (session, fake) = start_session(
        Mode::Verify,
        vec![
            ehlo(&["STARTTLS"]),
            answer(&format!("454 4.7.0 {SECRET} tls unavailable\r\n")),
        ],
    )
    .await;
    let error = session.starttls().await.expect_err("454");
    let message = shown(&error);
    assert!(message.contains("refused STARTTLS (454)"), "{message}");
    assert_clean(&message);
    assert_lines(&fake.finish().await.lines, &["EHLO localhost", "STARTTLS"]);

    let (session, fake) = start_session(
        Mode::Verify,
        vec![ehlo(&["STARTTLS"]), answer("220 2.0.0 go ahead\r\n")],
    )
    .await;
    let stream = session.starttls().await.unwrap();
    drop(stream);
    assert_lines(&fake.finish().await.lines, &["EHLO localhost", "STARTTLS"]);

    let (session, fake) = start_session(
        Mode::Verify,
        vec![
            ehlo(&["STARTTLS"]),
            answer(&format!("220 2.0.0 go ahead\r\n{SECRET}")),
        ],
    )
    .await;
    let error = session.starttls().await.expect_err("buffered");
    let message = shown(&error);
    assert!(
        message.contains("sent data before the TLS handshake"),
        "{message}"
    );
    assert_clean(&message);
    fake.finish().await;
}

#[test]
fn dot_stuff_uses_crlf_doubles_a_leading_dot_and_terminates_the_data() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"hello\nworld\n", b"hello\r\nworld\r\n.\r\n"),
        (b"hello\r\nworld\r\n", b"hello\r\nworld\r\n.\r\n"),
        (b"hello\r\nworld", b"hello\r\nworld\r\n.\r\n"),
        (b"a\r\nb\nc\r\n", b"a\r\nb\r\nc\r\n.\r\n"),
        (b".first\n.second\n", b"..first\r\n..second\r\n.\r\n"),
        (b".\r\n", b"..\r\n.\r\n"),
        (b"..\r\n", b"...\r\n.\r\n"),
        (b"a.b\n", b"a.b\r\n.\r\n"),
        (b"\n.", b"\r\n..\r\n.\r\n"),
        (b"", b".\r\n"),
        (b"already\r\n.\r\n", b"already\r\n..\r\n.\r\n"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            dot_stuff(input),
            expected.to_vec(),
            "{}",
            String::from_utf8_lossy(input)
        );
    }
}

#[test]
fn classify_maps_every_stage_and_result() {
    let stages = [
        Stage::Connect,
        Stage::Auth,
        Stage::Envelope,
        Stage::DataSent,
    ];
    let codes = [
        200, 220, 235, 250, 251, 334, 354, 421, 450, 454, 500, 535, 550, 554,
    ];
    for stage in stages {
        for code in codes {
            let result = Ok(Reply { code });
            assert_eq!(
                classify(&result, stage),
                expected(stage, Some(code)),
                "{stage:?} {code}"
            );
        }
        let result = Err(anyhow::anyhow!("the SMTP server closed the connection"));
        assert_eq!(
            classify(&result, stage),
            expected(stage, None),
            "{stage:?} error"
        );
    }
    let violation: Result<Reply> = Err(guard::SmtpViolation {
        verb: "RCPT".into(),
    }
    .into());
    for stage in [Stage::Connect, Stage::Auth, Stage::Envelope] {
        assert_eq!(
            classify(&violation, stage),
            not_applied(false, OutcomeCode::Internal),
            "a guard refusal at {stage:?} is final"
        );
    }
    assert_eq!(
        classify(&violation, Stage::DataSent),
        Execution::Ambiguous,
        "after the data, even a guard error is ambiguous"
    );
    assert_eq!(
        classify(&Ok(Reply { code: 250 }), Stage::DataSent),
        applied()
    );
    assert_eq!(
        classify(&Ok(Reply { code: 250 }), Stage::Envelope),
        not_applied(false, OutcomeCode::Refused),
        "250 before the data is not delivery"
    );
}

#[tokio::test]
async fn send_and_verify_to_a_port_with_nothing_listening_are_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut smtp = config("user", SECRET);
    smtp.port = port;
    smtp.security = SmtpSecurity::Tls;

    let execution = send(
        &smtp,
        "ann@example.com",
        &["bob@example.com".to_owned()],
        b"hi\n",
    )
    .await;
    assert_eq!(execution, not_applied(true, OutcomeCode::Unreachable));
    assert_clean(&format!("{execution:?}"));

    let error = verify(&smtp).await.expect_err("verify");
    let message = shown(&error);
    assert!(
        message.contains("could not reach the SMTP server"),
        "{message}"
    );
    assert_clean(&message);
}

#[tokio::test]
async fn the_guard_never_lets_submit_on_write_an_extra_recipient() {
    let from = "ann@example.com";
    let approved = "bob@example.com";
    let extra = format!("{SECRET}@example.net");
    let approved_to = vec![approved.to_owned()];
    let (mut session, fake) = start_session(
        send_to(from, &approved_to),
        vec![
            ehlo(&["AUTH PLAIN"]),
            answer("235 2.7.0 ok\r\n"),
            ok(),
            ok(),
        ],
    )
    .await;
    let asked = vec![approved.to_owned(), extra.clone()];
    let execution = submit_on(&mut session, &config("user", "pass"), from, &asked, b"hi\n").await;
    assert_eq!(
        execution,
        not_applied(false, OutcomeCode::Internal),
        "a guard refusal is final, like an IMAP guard violation"
    );
    let lines = finish(session, fake).await.lines;
    assert_lines(
        &lines,
        &[
            "EHLO localhost",
            &format!("AUTH PLAIN {PLAIN_TOKEN}"),
            "MAIL FROM:<ann@example.com>",
            "RCPT TO:<bob@example.com>",
        ],
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.contains(SECRET) && line != &format!("RCPT TO:<{extra}>")),
        "{lines:?}"
    );
}
