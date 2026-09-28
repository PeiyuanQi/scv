//! A signed-in mailbox, saved as `credentials/email/<account>.json` (mode
//! `0600`). Its secret reads the mailbox; this release never uses it to
//! write or send.

use anyhow::{Result, bail};
use scv_client::Secret;
use serde::{Deserialize, Serialize};

use crate::state;

/// A mailbox's saved credentials, by provider.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum Account {
    /// IMAP over implicit TLS, signed in with a password or, as QQ and 163
    /// require, an authorization code.
    Imap {
        host: String,
        port: u16,
        username: String,
        password: Secret,
    },
}

impl std::fmt::Debug for Account {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Imap { host, port, .. } => formatter
                .debug_struct("Imap")
                .field("host", host)
                .field("port", port)
                .finish_non_exhaustive(),
        }
    }
}

impl state::Credentials for Account {
    /// The provider, server, and user name: a new password or authorization
    /// code for the same mailbox keeps the account's state, and any other
    /// mailbox needs a logout.
    fn fingerprint(&self) -> Result<String> {
        use sha2::Digest as _;
        let Self::Imap {
            host,
            port,
            username,
            ..
        } = self;
        let mut hasher = sha2::Sha256::new();
        for part in [
            "imap",
            &host.to_ascii_lowercase(),
            &port.to_string(),
            username,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }
}

impl Account {
    /// The server, shown in status in place of a bot: never the user name,
    /// which is an address.
    pub(crate) fn host(&self) -> &str {
        match self {
            Self::Imap { host, .. } => host,
        }
    }

    /// Check what was saved before contacting the server.
    pub(crate) fn validate(&self) -> Result<()> {
        let Self::Imap {
            host,
            port,
            username,
            password,
        } = self;
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            && !host.starts_with(['.', '-']);
        if !host_ok {
            bail!("the IMAP host must be a DNS name, such as imap.qq.com");
        }
        if *port == 0 {
            bail!("the IMAP port must not be 0");
        }
        if username.is_empty() || username.len() > 320 || username.chars().any(char::is_control) {
            bail!("the IMAP user name must be one line of text");
        }
        if password.is_empty() || password.len() > 1024 || password.chars().any(char::is_control) {
            bail!("the IMAP password or authorization code must be one line of text");
        }
        Ok(())
    }
}

/// How an email account signs in: the server and the mailbox's password or
/// authorization code. Sign-in checks it by logging in and opening the
/// inbox read-only before anything is saved.
pub struct Login {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Secret,
}

#[cfg(test)]
mod tests;
