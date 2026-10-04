//! A minimal SMTP submission client, written for SCV so it always knows how
//! far a submission got.
//!
//! A send passes through its stages: connecting, the greeting and `EHLO`,
//! `STARTTLS` when asked for, `AUTH`, the envelope (`MAIL FROM`, each
//! `RCPT TO`), and the data. Once the terminating `.` of the data has been
//! written the message may have been accepted even if no answer comes, so
//! from then on any failure is ambiguous: the executor never sends that
//! message again, it only checks. Before that point a failure is safe to
//! retry, and the server's `5xx` is final. A command the guard refused is
//! final too: nothing was written, and repeating it would be refused again.
//!
//! Every command passes [`guard::check`] before it is written: at sign-in
//! only `EHLO`, `STARTTLS`, `AUTH`, and `QUIT`; when sending, also the one
//! approved sender, each approved recipient once, and `DATA`. Replies are
//! read by their codes; the server's text is never shown or logged, since
//! it may echo the credentials just sent. A server that refuses `STARTTLS`
//! is never used in plaintext.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::time::timeout;

use super::credentials::SmtpSecurity;
use super::ledger::actions::{Execution, OutcomeCode};

pub(crate) mod guard;

use guard::{Guard, Mode, SmtpViolation};

/// The greeting, and each command's reply.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// The reply to the end of the data, which servers may take long over.
const DATA_TIMEOUT: Duration = Duration::from_secs(45);
/// One reply line, and the lines of one reply.
const MAX_LINE: usize = 2048;
const MAX_LINES: usize = 128;
/// What SCV calls itself in `EHLO`.
const HELO_NAME: &str = "localhost";

/// Where and how to submit mail.
#[derive(Clone)]
pub(crate) struct SmtpConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) security: SmtpSecurity,
    pub(crate) username: String,
    pub(crate) password: String,
}

impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .finish_non_exhaustive()
    }
}

/// How far a submission got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stage {
    Connect,
    Auth,
    Envelope,
    /// The data is being or was written: the message may have been taken.
    DataSent,
}

/// A reply: its code and whether the server said anything more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) code: u16,
}

impl Reply {
    fn class(self) -> u16 {
        self.code / 100
    }
}

/// One SMTP session over `stream`, past any TLS.
pub(crate) struct Session<S> {
    stream: BufReader<S>,
    guard: Guard,
    /// The extensions `EHLO` announced, uppercased.
    extensions: Vec<String>,
    stage: Stage,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Session<S> {
    /// Start on `stream`: read the greeting (unless a `STARTTLS` prelude
    /// already did) and say `EHLO`.
    pub(crate) async fn start(stream: S, mode: Mode, greeted: bool) -> Result<Self> {
        let mut session = Self {
            stream: BufReader::new(stream),
            guard: Guard::new(mode),
            extensions: Vec::new(),
            stage: Stage::Connect,
        };
        if !greeted {
            let greeting = session.read(COMMAND_TIMEOUT).await?;
            if greeting.code != 220 {
                bail!("the SMTP server refused the connection ({})", greeting.code);
            }
        }
        session.ehlo().await?;
        Ok(session)
    }

    async fn ehlo(&mut self) -> Result<()> {
        let (reply, lines) = self
            .command_lines(&format!("EHLO {HELO_NAME}"), COMMAND_TIMEOUT)
            .await?;
        if reply.class() != 2 {
            bail!("the SMTP server refused EHLO ({})", reply.code);
        }
        self.extensions = lines
            .iter()
            .skip(1)
            .map(|line| line.to_ascii_uppercase())
            .collect();
        Ok(())
    }

    fn offers(&self, word: &str) -> bool {
        self.extensions.iter().any(|extension| {
            extension
                .split_whitespace()
                .next()
                .is_some_and(|first| first == word)
        })
    }

    fn offers_auth(&self, mechanism: &str) -> bool {
        self.extensions.iter().any(|extension| {
            let mut words = extension.split_whitespace();
            words.next() == Some("AUTH") && words.any(|word| word == mechanism)
        })
    }

    /// Sign in with `AUTH PLAIN`, or `AUTH LOGIN` when that is all the
    /// server offers.
    pub(crate) async fn auth(&mut self, username: &str, password: &str) -> Result<Reply> {
        self.stage = Stage::Auth;
        if username.contains('\0') || password.contains('\0') {
            bail!("an SMTP user name or password cannot hold a NUL character");
        }
        let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        if self.offers_auth("PLAIN") || !self.offers_auth("LOGIN") {
            let token = encode(&format!("\0{username}\0{password}"));
            return self
                .command(&format!("AUTH PLAIN {token}"), COMMAND_TIMEOUT)
                .await;
        }
        let reply = self.command("AUTH LOGIN", COMMAND_TIMEOUT).await?;
        if reply.code != 334 {
            return Ok(reply);
        }
        let reply = self.line(&encode(username)).await?;
        if reply.code != 334 {
            return Ok(reply);
        }
        self.line(&encode(password)).await
    }

    /// `QUIT`, whatever the answer.
    pub(crate) async fn quit(mut self) {
        let _ = self.command("QUIT", COMMAND_TIMEOUT).await;
        let _ = timeout(COMMAND_TIMEOUT, self.stream.get_mut().shutdown()).await;
    }

    /// Submit `message` from `from` to each of `to`: the envelope, then the
    /// data. What the result means depends on [`Session::stage`] when it
    /// failed.
    pub(crate) async fn submit(
        &mut self,
        from: &str,
        to: &[String],
        message: &[u8],
    ) -> Result<Reply> {
        self.stage = Stage::Envelope;
        let reply = self
            .command(&format!("MAIL FROM:<{from}>"), COMMAND_TIMEOUT)
            .await?;
        if reply.class() != 2 {
            return Ok(reply);
        }
        for recipient in to {
            let reply = self
                .command(&format!("RCPT TO:<{recipient}>"), COMMAND_TIMEOUT)
                .await?;
            if reply.class() != 2 {
                return Ok(reply);
            }
        }
        let reply = self.command("DATA", COMMAND_TIMEOUT).await?;
        if reply.code != 354 {
            return Ok(reply);
        }
        self.guard.data()?;
        let data = dot_stuff(message);
        // From the first byte of the data on, the server may end up with the
        // whole message whatever happens here: every failure is ambiguous.
        self.stage = Stage::DataSent;
        let writer = self.stream.get_mut();
        timeout(COMMAND_TIMEOUT, async {
            writer.write_all(&data).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| anyhow!("writing the message to the SMTP server timed out"))??;
        self.read(DATA_TIMEOUT).await
    }

    pub(crate) fn stage(&self) -> Stage {
        self.stage
    }

    /// Ask for `STARTTLS` on a plaintext session; the caller upgrades the
    /// stream only on `220`.
    pub(crate) async fn starttls(mut self) -> Result<S> {
        if !self.offers("STARTTLS") {
            bail!("the SMTP server does not offer STARTTLS, so SCV will not use it");
        }
        let reply = self.command("STARTTLS", COMMAND_TIMEOUT).await?;
        if reply.code != 220 {
            bail!(
                "the SMTP server refused STARTTLS ({}), so SCV will not use it",
                reply.code
            );
        }
        if !self.stream.buffer().is_empty() {
            bail!("the SMTP server sent data before the TLS handshake");
        }
        Ok(self.stream.into_inner())
    }

    async fn command(&mut self, text: &str, wait: Duration) -> Result<Reply> {
        Ok(self.command_lines(text, wait).await?.0)
    }

    async fn command_lines(&mut self, text: &str, wait: Duration) -> Result<(Reply, Vec<String>)> {
        self.guard.check(text)?;
        let verb = text.split([' ', ':']).next().unwrap_or_default().to_owned();
        tracing::debug!(verb = %verb, "SMTP command");
        let writer = self.stream.get_mut();
        timeout(COMMAND_TIMEOUT, async {
            writer.write_all(text.as_bytes()).await?;
            writer.write_all(b"\r\n").await?;
            writer.flush().await
        })
        .await
        .map_err(|_| anyhow!("SMTP {verb} timed out"))??;
        self.read_lines(wait).await
    }

    /// A line sent after a `334` challenge.
    async fn line(&mut self, text: &str) -> Result<Reply> {
        self.guard.continuation(text)?;
        let writer = self.stream.get_mut();
        timeout(COMMAND_TIMEOUT, async {
            writer.write_all(text.as_bytes()).await?;
            writer.write_all(b"\r\n").await?;
            writer.flush().await
        })
        .await
        .map_err(|_| anyhow!("SMTP AUTH timed out"))??;
        self.read(COMMAND_TIMEOUT).await
    }

    async fn read(&mut self, wait: Duration) -> Result<Reply> {
        Ok(self.read_lines(wait).await?.0)
    }

    /// One reply: its code, and the text after the code of each line, which
    /// only `EHLO`'s extensions are read from.
    async fn read_lines(&mut self, wait: Duration) -> Result<(Reply, Vec<String>)> {
        timeout(wait, async {
            let mut lines = Vec::new();
            loop {
                let mut line = Vec::new();
                let read = (&mut self.stream)
                    .take(MAX_LINE as u64)
                    .read_until(b'\n', &mut line)
                    .await?;
                if read == 0 {
                    bail!("the SMTP server closed the connection");
                }
                if !line.ends_with(b"\n") {
                    bail!("the SMTP server sent a line that is too long");
                }
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\r', '\n']);
                let code: u16 = text
                    .get(..3)
                    .and_then(|code| code.parse().ok())
                    .filter(|code| (200..600).contains(code))
                    .context("the SMTP server sent an unreadable reply")?;
                let last = text.as_bytes().get(3) != Some(&b'-');
                lines.push(text.get(4..).unwrap_or_default().to_owned());
                if lines.len() > MAX_LINES {
                    bail!("the SMTP server sent too long a reply");
                }
                if last {
                    return Ok((Reply { code }, lines));
                }
            }
        })
        .await
        .map_err(|_| anyhow!("the SMTP server did not answer in time"))?
    }
}

/// `message` as DATA carries it: CRLF line ends, each line that starts with
/// a dot given another, and the terminating `.` line.
pub(crate) fn dot_stuff(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 64);
    let mut start_of_line = true;
    let mut previous = 0u8;
    for &byte in message {
        if byte == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        if start_of_line && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        start_of_line = byte == b'\n';
        previous = byte;
    }
    if !start_of_line {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// What a submission that ended with `result` at `stage` did, as the
/// executor records it.
pub(crate) fn classify(result: &Result<Reply>, stage: Stage) -> Execution {
    match (result, stage) {
        (Ok(reply), Stage::DataSent) if reply.code == 250 => Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        },
        // Refused after the data: the server kept nothing, but a 4xx there
        // is not retried either, to be safe.
        (Ok(_), Stage::DataSent) => Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Refused,
        },
        (Err(_), Stage::DataSent) => Execution::Ambiguous,
        (Err(error), _) if error.downcast_ref::<SmtpViolation>().is_some() => {
            Execution::NotApplied {
                retry: false,
                code: OutcomeCode::Internal,
            }
        }
        (Ok(reply), Stage::Auth) if reply.class() == 5 => Execution::NotApplied {
            retry: false,
            code: OutcomeCode::AuthFailed,
        },
        (Ok(reply), _) if reply.class() == 4 => Execution::NotApplied {
            retry: true,
            code: OutcomeCode::Refused,
        },
        (Ok(_), _) => Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Refused,
        },
        (Err(_), _) => Execution::NotApplied {
            retry: true,
            code: OutcomeCode::Unreachable,
        },
    }
}

type Tls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Connect to `config`'s server over TLS as it says, in `mode`, up to the
/// point of signing in.
async fn open(config: &SmtpConfig, mode: Mode) -> Result<Session<Tls>> {
    use super::imap::tls;
    Ok(match config.security {
        SmtpSecurity::Tls => {
            let stream = tls::connect_to(&config.host, config.port, "SMTP").await?;
            Session::start(stream, mode, false).await?
        }
        SmtpSecurity::Starttls => {
            let tcp = tls::tcp(&config.host, config.port, "SMTP").await?;
            let plain = Session::start(tcp, Mode::Verify, false).await?;
            let tcp = plain.starttls().await?;
            let stream = tls::upgrade(&config.host, tcp, "SMTP").await?;
            Session::start(stream, mode, true).await?
        }
    })
}

/// Check `config` at sign-in: connect, sign in, and quit. Nothing else is
/// sent.
pub(crate) async fn verify(config: &SmtpConfig) -> Result<()> {
    let mut session = open(config, Mode::Verify)
        .await
        .context("could not reach the SMTP server")?;
    let reply = session.auth(&config.username, &config.password).await?;
    session.quit().await;
    if reply.class() != 2 {
        bail!(
            "SMTP sign-in was refused ({}). Check the user name and the password or \
             authorization code, and that SMTP is turned on for the mailbox",
            reply.code
        );
    }
    Ok(())
}

/// Send `message` from `from` to `to` through `config`'s server: what the
/// executor records. Only `from`, each of `to` once, and the data pass the
/// guard.
pub(crate) async fn send(
    config: &SmtpConfig,
    from: &str,
    to: &[String],
    message: &[u8],
) -> Execution {
    let mode = Mode::Send {
        from: from.to_owned(),
        to: to.to_vec(),
    };
    let mut session = match open(config, mode).await {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(error = %error, "could not reach the SMTP server");
            return classify(&Err(error), Stage::Connect);
        }
    };
    submit_on(&mut session, config, from, to, message).await
}

/// Sign in and submit on an open session, then quit.
pub(crate) async fn submit_on<S: AsyncRead + AsyncWrite + Unpin + Send>(
    session: &mut Session<S>,
    config: &SmtpConfig,
    from: &str,
    to: &[String],
    message: &[u8],
) -> Execution {
    match session.auth(&config.username, &config.password).await {
        Ok(reply) if reply.class() == 2 => {}
        result => return classify(&result, Stage::Auth),
    }
    let result = session.submit(from, to, message).await;
    let execution = classify(&result, session.stage());
    if let Err(error) = &result {
        tracing::warn!(error = %error, stage = ?session.stage(), "an SMTP submission failed");
    }
    execution
}

#[cfg(test)]
mod tests;
