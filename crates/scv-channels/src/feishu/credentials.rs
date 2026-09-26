//! Feishu credentials: the bot app, its brand, and the owner it answers.

use anyhow::{Result, bail};
use scv_client::Secret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The Feishu channel's account store.
pub(crate) type Store = crate::state::Store<Account>;

/// Which deployment an app lives in: Feishu (China) or Lark (international).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Brand {
    #[default]
    Feishu,
    Lark,
}

impl Brand {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "feishu" => Some(Self::Feishu),
            "lark" => Some(Self::Lark),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Feishu => "Feishu",
            Self::Lark => "Lark",
        }
    }
}

/// A Feishu bot app and the owner it answers with tools.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub app_id: String,
    pub app_secret: Secret,
    #[serde(default)]
    pub brand: Brand,
    /// The owner's `open_id` for this app, recorded at sign-in. Feishu
    /// issues `open_id` per app, so it is the sender ID of the owner's
    /// messages to this bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_open_id: Option<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("app_id", &self.app_id)
            .field("app_secret", &"<redacted>")
            .field("brand", &self.brand)
            .field("owner_open_id", &self.owner_open_id)
            .finish()
    }
}

impl Account {
    /// Check the IDs' shape before they reach a request or a file.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_app_id(&self.app_id)?;
        if self.app_secret.is_empty()
            || self.app_secret.len() > 256
            || self.app_secret.chars().any(char::is_control)
        {
            bail!("invalid Feishu app secret")
        }
        if let Some(owner) = &self.owner_open_id {
            validate_open_id(owner)?;
        }
        Ok(())
    }
}

impl crate::state::Credentials for Account {
    /// The app and its owner, not the secret: a rotated secret keeps the
    /// account's delivery state, while another app or owner needs a logout.
    fn fingerprint(&self) -> Result<String> {
        let encoded =
            serde_json::to_vec(&("feishu", self.brand, &self.app_id, &self.owner_open_id))?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }
}

pub(crate) fn validate_app_id(app_id: &str) -> Result<()> {
    let valid = app_id.len() <= 64
        && app_id.strip_prefix("cli_").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        });
    if !valid {
        bail!("invalid Feishu app ID; it looks like cli_ followed by letters and digits")
    }
    Ok(())
}

pub(crate) fn validate_open_id(open_id: &str) -> Result<()> {
    let valid = open_id.len() <= 128
        && open_id.strip_prefix("ou_").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    if !valid {
        bail!("invalid Feishu open_id; it looks like ou_ followed by letters and digits")
    }
    Ok(())
}

/// Save `account` as `name` in `store` once its IDs are checked.
pub(crate) fn save(store: &Store, name: &str, account: &Account) -> Result<()> {
    account.validate()?;
    store.save_account(name, account)
}

#[cfg(test)]
mod tests;
