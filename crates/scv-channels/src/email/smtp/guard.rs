//! The SMTP command guard: defence in depth under the typed session.
//!
//! Every command line is checked before it is written. At sign-in
//! ([`Mode::Verify`]) only `EHLO`, `STARTTLS`, `AUTH`, its continuation
//! lines, and `QUIT` pass. When sending ([`Mode::Send`]) the approved
//! sender's `MAIL FROM` passes once, each approved recipient's `RCPT TO`
//! once, then `DATA` and the data once; `RSET` and `NOOP` pass too. `VRFY`,
//! `EXPN`, `ETRN`, `BDAT`, another sender, and any recipient not approved
//! never do.

/// What a session may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Sign in and nothing more.
    Verify,
    /// Send one approved message from `from` to exactly `to`.
    Send { from: String, to: Vec<String> },
}

/// A command the guard refused; `verb` is only its first word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmtpViolation {
    pub(crate) verb: String,
}

impl std::fmt::Display for SmtpViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "refused to send an SMTP {} command this session may not send",
            self.verb
        )
    }
}

impl std::error::Error for SmtpViolation {}

/// The guard of one session.
#[derive(Debug)]
pub(crate) struct Guard {
    mode: Mode,
    /// `MAIL FROM` was sent.
    sender: bool,
    /// Recipients already given.
    recipients: Vec<String>,
    /// `DATA` was accepted and its data not yet written.
    data_open: bool,
    /// The data was written.
    data_sent: bool,
    /// An `AUTH` waits for continuation lines.
    auth_lines: u8,
}

impl Guard {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
            mode,
            sender: false,
            recipients: Vec::new(),
            data_open: false,
            data_sent: false,
            auth_lines: 0,
        }
    }

    /// Check one command line (without its CRLF) before it is written.
    pub(crate) fn check(&mut self, line: &str) -> Result<(), SmtpViolation> {
        let verb: String = line
            .split([' ', ':'])
            .next()
            .unwrap_or_default()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect::<String>()
            .to_ascii_uppercase();
        let refuse = || {
            tracing::error!(verb = %verb, "refused an SMTP command");
            SmtpViolation { verb: verb.clone() }
        };
        if !line.bytes().all(|byte| (0x20..0x7f).contains(&byte)) || line.len() > 1000 {
            return Err(refuse());
        }
        let upper = line.to_ascii_uppercase();
        let allowed = match verb.as_str() {
            "EHLO" | "QUIT" | "NOOP" => true,
            "STARTTLS" => upper == "STARTTLS" && !self.sender,
            "AUTH" => {
                let plain = upper.starts_with("AUTH PLAIN ") && line.split(' ').count() == 3;
                let login = upper == "AUTH LOGIN";
                let allowed = (plain || login) && !self.sender;
                if allowed && login {
                    self.auth_lines = 2;
                }
                allowed
            }
            "RSET" => !self.data_sent,
            "MAIL" => match &self.mode {
                Mode::Send { from, .. } => {
                    let ok = !self.sender && upper.starts_with("MAIL FROM:") && {
                        let address = &line["MAIL FROM:".len()..];
                        address == format!("<{from}>")
                    };
                    self.sender |= ok;
                    ok
                }
                Mode::Verify => false,
            },
            "RCPT" => match &self.mode {
                Mode::Send { to, .. } => {
                    let address = upper
                        .starts_with("RCPT TO:<")
                        .then(|| line["RCPT TO:<".len()..].strip_suffix('>'))
                        .flatten();
                    let ok = self.sender
                        && address.is_some_and(|address| {
                            to.iter().any(|allowed| allowed == address)
                                && !self.recipients.iter().any(|given| given == address)
                        });
                    if ok && let Some(address) = address {
                        self.recipients.push(address.to_owned());
                    }
                    ok
                }
                Mode::Verify => false,
            },
            "DATA" => {
                let ok = matches!(&self.mode, Mode::Send { to, .. } if self.sender
                    && upper == "DATA"
                    && !self.data_open
                    && !self.data_sent
                    && self.recipients.len() == to.len());
                self.data_open |= ok;
                ok
            }
            _ => false,
        };
        if allowed { Ok(()) } else { Err(refuse()) }
    }

    /// Check a continuation line of `AUTH LOGIN`: base64 only.
    pub(crate) fn continuation(&mut self, line: &str) -> Result<(), SmtpViolation> {
        let base64 = !line.is_empty()
            && line
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='));
        if self.auth_lines == 0 || !base64 {
            return Err(SmtpViolation {
                verb: "(AUTH line)".into(),
            });
        }
        self.auth_lines -= 1;
        Ok(())
    }

    /// The data may be written now, once.
    pub(crate) fn data(&mut self) -> Result<(), SmtpViolation> {
        if !self.data_open || self.data_sent {
            return Err(SmtpViolation {
                verb: "(data)".into(),
            });
        }
        self.data_open = false;
        self.data_sent = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
