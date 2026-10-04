//! A signed-in mailbox, saved as `credentials/email/<account>.json` (mode
//! `0600`), and, for providers signed in with OAuth, its grants beside it
//! in `<account>.grants`.
//!
//! What the saved credentials allow depends on the provider. An IMAP
//! password or authorization code reads and writes the whole mailbox, and
//! with an SMTP server it also sends. An OAuth account keeps one refresh
//! token per grant: the reader's can only read, and a writer's or sender's
//! exists only when the owner signed in for it. The account's code loads a
//! writing credential only when an approved action needs it.
//!
//! Grants live in a file of their own because providers rotate refresh
//! tokens: the daemon restarts an account whenever its credentials change,
//! and a rotated token must not do that.

use anyhow::{Context as _, Result, bail};
use scv_client::Secret;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::state;

/// A mailbox's saved credentials, by provider.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum Account {
    /// IMAP over implicit TLS, signed in with a password or, as QQ and 163
    /// require, an authorization code; with `smtp`, it sends through that
    /// server with the same user name and secret.
    Imap {
        host: String,
        port: u16,
        username: String,
        password: Secret,
        /// The mailbox's own address, when the user name is not one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        /// Where approved mail is sent; absent, nothing can be sent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smtp: Option<Smtp>,
    },
    /// Gmail through the Gmail API, signed in with the owner's own OAuth
    /// client (an installed app).
    Gmail {
        address: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<Secret>,
    },
    /// Outlook.com or Microsoft 365 through Microsoft Graph, signed in with
    /// the owner's own public client.
    Graph {
        address: String,
        client_id: String,
        /// `consumers`, `organizations`, `common`, or a tenant ID.
        tenant: String,
    },
}

/// An SMTP submission server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
}

/// How an SMTP connection is secured; never plaintext.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// TLS from the first byte (port 465).
    Tls,
    /// `STARTTLS` before anything else (port 587); a server that refuses it
    /// is never used in plaintext.
    Starttls,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Imap {
                host, port, smtp, ..
            } => formatter
                .debug_struct("Imap")
                .field("host", host)
                .field("port", port)
                .field("smtp", smtp)
                .finish_non_exhaustive(),
            Self::Gmail { .. } => formatter.debug_struct("Gmail").finish_non_exhaustive(),
            Self::Graph { tenant, .. } => formatter
                .debug_struct("Graph")
                .field("tenant", tenant)
                .finish_non_exhaustive(),
        }
    }
}

impl state::Credentials for Account {
    /// The provider and the mailbox: for IMAP the server, port, and user
    /// name, so a new password or authorization code, or another SMTP
    /// server, keeps the account's state; for OAuth the address and the
    /// client. Any other mailbox needs a logout.
    fn fingerprint(&self) -> Result<String> {
        use sha2::Digest as _;
        let parts: Vec<String> = match self {
            Self::Imap {
                host,
                port,
                username,
                ..
            } => vec![
                "imap".into(),
                host.to_ascii_lowercase(),
                port.to_string(),
                username.clone(),
            ],
            Self::Gmail {
                address, client_id, ..
            } => vec![
                "gmail".into(),
                address.to_ascii_lowercase(),
                client_id.clone(),
            ],
            Self::Graph {
                address,
                client_id,
                tenant,
            } => vec![
                "graph".into(),
                address.to_ascii_lowercase(),
                client_id.clone(),
                tenant.to_ascii_lowercase(),
            ],
        };
        let mut hasher = sha2::Sha256::new();
        for part in parts {
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
    /// The server, shown in status in place of a bot: never the user name
    /// or address.
    pub(crate) fn host(&self) -> &str {
        match self {
            Self::Imap { host, .. } => host,
            Self::Gmail { .. } => "gmail.googleapis.com",
            Self::Graph { .. } => "graph.microsoft.com",
        }
    }

    /// The mailbox's own address, lowercased in its domain, when known: the
    /// sender of mail it writes and the address replies leave out.
    pub(crate) fn own_address(&self) -> Option<String> {
        let address = match self {
            Self::Imap {
                username, address, ..
            } => address.as_deref().unwrap_or(username),
            Self::Gmail { address, .. } | Self::Graph { address, .. } => address,
        };
        super::render::valid_address(address)
    }

    /// Check what was saved before contacting the server.
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Imap {
                host,
                port,
                username,
                password,
                address,
                smtp,
            } => {
                if !valid_host(host) {
                    bail!("the IMAP host must be a DNS name, such as imap.qq.com");
                }
                if *port == 0 {
                    bail!("the IMAP port must not be 0");
                }
                if username.is_empty()
                    || username.len() > 320
                    || username.chars().any(char::is_control)
                {
                    bail!("the IMAP user name must be one line of text");
                }
                if password.is_empty()
                    || password.len() > 1024
                    || password.chars().any(char::is_control)
                {
                    bail!("the IMAP password or authorization code must be one line of text");
                }
                if let Some(address) = address
                    && super::render::valid_address(address).is_none()
                {
                    bail!("the mailbox address must look like name@example.com");
                }
                if let Some(smtp) = smtp {
                    if !valid_host(&smtp.host) {
                        bail!("the SMTP host must be a DNS name, such as smtp.qq.com");
                    }
                    if smtp.port == 0 {
                        bail!("the SMTP port must not be 0");
                    }
                }
            }
            Self::Gmail {
                address,
                client_id,
                client_secret,
            } => {
                check_address(address)?;
                check_client_id(client_id)?;
                if let Some(secret) = client_secret
                    && (secret.len() > 512 || secret.chars().any(char::is_control))
                {
                    bail!("the OAuth client secret must be one line of text");
                }
            }
            Self::Graph {
                address,
                client_id,
                tenant,
            } => {
                check_address(address)?;
                check_client_id(client_id)?;
                if tenant.is_empty()
                    || tenant.len() > 64
                    || !tenant
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
                {
                    bail!(
                        "the Microsoft tenant must be consumers, organizations, common, or an ID"
                    );
                }
            }
        }
        Ok(())
    }
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        && !host.starts_with(['.', '-'])
}

fn check_address(address: &str) -> Result<()> {
    if super::render::valid_address(address).is_none() {
        bail!("the mailbox address must look like name@example.com");
    }
    Ok(())
}

fn check_client_id(client_id: &str) -> Result<()> {
    if client_id.is_empty()
        || client_id.len() > 256
        || !client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        bail!("the OAuth client ID must be the ID the provider's console shows");
    }
    Ok(())
}

/// How an email account signs in. Sign-in checks each credential with the
/// provider before anything is saved.
pub enum Login {
    /// An IMAP mailbox, read with a password or authorization code, and
    /// with `smtp` also sending through that server.
    Imap {
        host: String,
        port: u16,
        username: String,
        password: Secret,
        address: Option<String>,
        smtp: Option<Smtp>,
    },
    /// OAuth for the Gmail API or Microsoft Graph.
    OAuth(super::oauth::Request),
}

/// One OAuth grant: a refresh token and the scopes it was granted.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Grant {
    pub(crate) refresh_token: Secret,
    pub(crate) scopes: Vec<String>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Grant")
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Which grant a connection uses: each is its own refresh token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrantKind {
    /// Reads mail; every account signed in with OAuth has it.
    Reader,
    /// Saves drafts, moves messages, and marks them read.
    Writer,
    /// Sends mail. Gmail has no scope that only sends, so a Gmail account
    /// sends with its writer grant.
    Sender,
}

impl GrantKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
            Self::Sender => "sender",
        }
    }
}

/// An OAuth account's grants, `credentials/email/<account>.grants`.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Grants {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reader: Option<Grant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) writer: Option<Grant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sender: Option<Grant>,
}

impl std::fmt::Debug for Grants {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Grants")
            .field("reader", &self.reader.is_some())
            .field("writer", &self.writer.is_some())
            .field("sender", &self.sender.is_some())
            .finish()
    }
}

/// The largest grants file read.
const MAX_GRANTS_BYTES: u64 = 64 * 1024;

impl Grants {
    pub(crate) fn get(&self, kind: GrantKind) -> Option<&Grant> {
        match kind {
            GrantKind::Reader => self.reader.as_ref(),
            GrantKind::Writer => self.writer.as_ref(),
            GrantKind::Sender => self.sender.as_ref(),
        }
    }

    pub(crate) fn set(&mut self, kind: GrantKind, grant: Grant) {
        match kind {
            GrantKind::Reader => self.reader = Some(grant),
            GrantKind::Writer => self.writer = Some(grant),
            GrantKind::Sender => self.sender = Some(grant),
        }
    }

    /// Where `account`'s grants live under `layout`.
    pub(crate) fn path(layout: &scv_client::Layout, account: &str) -> Result<PathBuf> {
        state::validate_name(account)?;
        Ok(layout
            .channel_credentials(super::CHANNEL)
            .join(format!("{account}.grants")))
    }

    /// The grants at `path`, or none when there is no file. The file must
    /// be a private regular file.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.len() > MAX_GRANTS_BYTES {
            bail!("the mail account's grants file is not a small regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                bail!("the mail account's grants file is accessible by other users");
            }
        }
        serde_json::from_slice(&std::fs::read(path)?)
            .context("the mail account's grants file is not readable")
    }

    /// Replace the grants at `path` whole, privately.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        state::atomic_write(path, &serde_json::to_string(self)?)
    }
}

#[cfg(test)]
mod tests;
