//! Feishu channel credentials and where the channel keeps its state.

use anyhow::{Result, bail};
use scv_client::Secret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use scv_channels::state::{
    AccountSettings, BridgeState, RemoteTools, private_directory, validate_name,
};

/// The Feishu channel's account store.
pub type Store = scv_channels::state::Store<Account>;

/// Which deployment an app lives in: Feishu (China) or Lark (international).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Brand {
    #[default]
    Feishu,
    Lark,
}

impl Brand {
    pub fn parse(value: &str) -> Option<Self> {
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
    pub fn validate(&self) -> Result<()> {
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

impl scv_channels::state::Credentials for Account {
    /// The app and its owner, not the secret: a rotated secret keeps the
    /// account's delivery state, while another app or owner needs a logout.
    fn fingerprint(&self) -> Result<String> {
        let encoded =
            serde_json::to_vec(&("feishu", self.brand, &self.app_id, &self.owner_open_id))?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }
}

pub fn validate_app_id(app_id: &str) -> Result<()> {
    let valid = app_id.len() <= 64
        && app_id.strip_prefix("cli_").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        });
    if !valid {
        bail!("invalid Feishu app ID; it looks like cli_ followed by letters and digits")
    }
    Ok(())
}

pub fn validate_open_id(open_id: &str) -> Result<()> {
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

/// The Feishu channel's account store in the instance selected by `SCV_HOME`.
pub fn store() -> Result<Store> {
    Store::from_env(crate::CHANNEL)
}

/// Where this account's received files go and files to send wait, under
/// the instance's media directory.
pub fn media_options(
    account: &str,
    settings: scv_channels::MediaSettings,
) -> Result<scv_channels::MediaOptions> {
    validate_name(account)?;
    Ok(scv_channels::MediaOptions::new(
        &scv_channels::Layout::from_env()?.media(),
        crate::CHANNEL,
        account,
        settings,
    ))
}

pub fn account(name: &str) -> Result<Option<Account>> {
    store()?.account(name)
}
pub fn save_account(name: &str, value: &Account) -> Result<()> {
    value.validate()?;
    store()?.save_account(name, value)
}
/// Read credentials and settings together under the account transaction lock.
pub fn account_snapshot(name: &str) -> Result<(Option<Account>, AccountSettings)> {
    store()?.account_snapshot(name)
}
pub fn settings(name: &str) -> Result<AccountSettings> {
    store()?.settings(name)
}
pub fn save_settings(name: &str, value: &AccountSettings) -> Result<()> {
    store()?.save_settings(name, value)
}
/// Discover accounts, failing on entry errors or more than 128 entries.
/// Credentials are validated separately.
pub fn account_names() -> Result<Vec<String>> {
    store()?.account_names()
}
pub fn load_state(name: &str) -> Result<BridgeState> {
    store()?.load_state(name)
}
pub fn save_state(name: &str, value: &BridgeState) -> Result<()> {
    store()?.save_state(name, value)
}
/// Remove an account's credentials, settings, and delivery state; refuses
/// while the account runs.
pub fn remove(name: &str) -> Result<()> {
    store()?.remove(name)
}

#[cfg(test)]
mod tests;
