//! Which adapter an account reads and acts through, made from its
//! credentials.
//!
//! Reading always goes through the provider's read-only connection or
//! reader grant. A writer is made only for an account whose settings let
//! some action happen: an IMAP account then opens a write connection per
//! approved action (and an SMTP session per send), and an OAuth account
//! loads its writer or sender grant. Without `mail.actions` no writing
//! credential is loaded at all.

use anyhow::{Result, bail};
use futures_util::future::BoxFuture;
use std::sync::Arc;

use super::credentials::{Account, GrantKind, Grants};
use super::effects::MailEffects;
use super::imap::{self, ImapConfig};
use super::oauth::{self, OAuthProvider, TokenSource};
use super::settings::MailSettings;
use super::smtp::SmtpConfig;
use super::source::{MailSource, ProviderKind};
use super::{api, gmail, graph};

/// Opens the account's read-only source.
pub(crate) type Connect =
    Box<dyn Fn() -> BoxFuture<'static, Result<Box<dyn MailSource>>> + Send + Sync>;
/// Makes the effects of one approved action.
pub(crate) type MakeEffects = Box<dyn Fn() -> Box<dyn MailEffects> + Send + Sync>;

/// An account's adapter.
pub(crate) struct Provider {
    pub(crate) kind: ProviderKind,
    pub(crate) connect: Connect,
    /// Present only when some action may happen.
    pub(crate) effects: Option<MakeEffects>,
    /// Whether the account can send at all.
    pub(crate) can_send: bool,
}

/// The IMAP configuration of `credentials` for `mailbox`.
pub(crate) fn imap_config(credentials: &Account, mailbox: &str) -> Option<ImapConfig> {
    let Account::Imap {
        host,
        port,
        username,
        password,
        ..
    } = credentials
    else {
        return None;
    };
    Some(ImapConfig {
        host: host.clone(),
        port: *port,
        username: username.clone(),
        password: password.expose().to_owned(),
        mailbox: mailbox.to_owned(),
    })
}

/// The SMTP configuration of `credentials`, when it has one.
fn smtp_config(credentials: &Account) -> Option<SmtpConfig> {
    let Account::Imap {
        username,
        password,
        smtp: Some(smtp),
        ..
    } = credentials
    else {
        return None;
    };
    Some(SmtpConfig {
        host: smtp.host.clone(),
        port: smtp.port,
        security: smtp.security,
        username: username.clone(),
        password: password.expose().to_owned(),
    })
}

/// What an OAuth account's tokens are made from.
pub(crate) struct OAuthParts {
    pub(crate) provider: OAuthProvider,
    pub(crate) endpoints: oauth::Endpoints,
    pub(crate) client_id: String,
    pub(crate) client_secret: Option<scv_client::Secret>,
    pub(crate) grants: std::path::PathBuf,
    pub(crate) lock: Arc<dyn Fn() -> Result<std::fs::File> + Send + Sync>,
}

impl OAuthParts {
    fn tokens(&self, kind: GrantKind) -> Result<Arc<TokenSource>> {
        Ok(Arc::new(TokenSource::new(
            self.provider,
            kind,
            self.endpoints.clone(),
            self.client_id.clone(),
            self.client_secret.clone(),
            self.grants.clone(),
            Arc::clone(&self.lock),
        )?))
    }
}

/// The adapter for `credentials` under `settings`; `oauth` says where an
/// OAuth account's grants are. Fails when the settings allow an action the
/// credentials cannot do, naming how to sign in for it.
pub(crate) fn provider(
    credentials: &Account,
    settings: &MailSettings,
    oauth: Option<OAuthParts>,
    origins: &api::Origins,
) -> Result<Provider> {
    let actions = settings.actions.as_ref().filter(|actions| actions.any());
    let sends = actions.is_some_and(|actions| actions.send == super::settings::ActionMode::Approve);
    let writes = actions.is_some_and(super::settings::ActionSettings::changes_mailbox);
    match credentials {
        Account::Imap { host, port, .. } => {
            let config = imap_config(credentials, &settings.mailbox).expect("an IMAP account");
            let smtp = smtp_config(credentials);
            if sends && smtp.is_none() {
                bail!(
                    "mail.actions.send is \"approve\", but this mailbox was signed in without an \
                     SMTP server; sign it in again with --smtp-host, or turn send off"
                );
            }
            let connect_config = config.clone();
            let connect: Connect = Box::new(move || {
                let config = connect_config.clone();
                Box::pin(async move {
                    let source = imap::ImapSource::connect(&config).await?;
                    Ok(Box::new(source) as Box<dyn MailSource>)
                })
            });
            let can_send = smtp.is_some();
            let effects = actions.map(|_| {
                let host = host.clone();
                let port = *port;
                let make: MakeEffects = Box::new(move || {
                    Box::new(imap::writer::ImapEffects {
                        connect: imap::writer::TlsConnect {
                            host: host.clone(),
                            port,
                        },
                        config: config.clone(),
                        smtp: smtp.clone(),
                    }) as Box<dyn MailEffects>
                });
                make
            });
            Ok(Provider {
                kind: ProviderKind::Imap,
                connect,
                effects,
                can_send,
            })
        }
        Account::Gmail { .. } | Account::Graph { .. } => {
            let Some(oauth) = oauth else {
                bail!("the mail account's grants are not available");
            };
            let grants = Grants::load(&oauth.grants)?;
            if grants.reader.is_none() {
                bail!("the mail account has no reading grant; sign it in again");
            }
            let gmail = matches!(credentials, Account::Gmail { .. });
            let writer_kind = GrantKind::Writer;
            let sender_kind = if gmail {
                GrantKind::Writer
            } else {
                GrantKind::Sender
            };
            if writes && grants.get(writer_kind).is_none() {
                bail!(
                    "mail.actions lets SCV change the mailbox, but it was signed in only to read; \
                     sign it in again with --write, or turn those actions off"
                );
            }
            if sends && grants.get(sender_kind).is_none() {
                bail!(
                    "mail.actions.send is \"approve\", but the mailbox was signed in without \
                     sending; sign it in again with --send, or turn send off"
                );
            }
            let can_send = grants.get(sender_kind).is_some();
            let reader = oauth.tokens(GrantKind::Reader)?;
            let mailbox = settings.mailbox.clone();
            let (kind, connect): (ProviderKind, Connect) = if gmail {
                let origin = origins.gmail.clone();
                (
                    ProviderKind::Gmail,
                    Box::new(move || {
                        let api = api::Api::new(
                            api::Flavor::Gmail,
                            origin.clone(),
                            Arc::clone(&reader),
                            api::Mode::Read,
                        );
                        let mailbox = mailbox.clone();
                        Box::pin(async move {
                            Ok(Box::new(gmail::GmailSource::new(api?, &mailbox)?)
                                as Box<dyn MailSource>)
                        })
                    }),
                )
            } else {
                let origin = origins.graph.clone();
                (
                    ProviderKind::Graph,
                    Box::new(move || {
                        let api = api::Api::new(
                            api::Flavor::Graph,
                            origin.clone(),
                            Arc::clone(&reader),
                            api::Mode::Read,
                        );
                        let mailbox = mailbox.clone();
                        Box::pin(async move {
                            Ok(Box::new(graph::GraphSource::new(api?, &mailbox)?)
                                as Box<dyn MailSource>)
                        })
                    }),
                )
            };
            let effects = if actions.is_none() {
                None
            } else {
                // Only the grants the settings can use are loaded; each was
                // checked above to exist.
                let writer = (writes || (gmail && sends))
                    .then(|| oauth.tokens(writer_kind))
                    .transpose()?;
                let sender = sends.then(|| oauth.tokens(sender_kind)).transpose()?;
                let reader = oauth.tokens(GrantKind::Reader)?;
                let origin = if gmail {
                    origins.gmail.clone()
                } else {
                    origins.graph.clone()
                };
                let make: MakeEffects = Box::new(move || {
                    let parts = api::EffectParts {
                        origin: origin.clone(),
                        reader: Arc::clone(&reader),
                        writer: writer.clone(),
                        sender: sender.clone(),
                    };
                    if gmail {
                        Box::new(gmail::GmailEffects::new(parts)) as Box<dyn MailEffects>
                    } else {
                        Box::new(graph::GraphEffects::new(parts)) as Box<dyn MailEffects>
                    }
                });
                Some(make)
            };
            Ok(Provider {
                kind,
                connect,
                effects,
                can_send,
            })
        }
    }
}

#[cfg(test)]
mod tests;
