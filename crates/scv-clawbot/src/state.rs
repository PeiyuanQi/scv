//! WeChat channel credentials and where the channel keeps its state.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use scv_channels::state::{
    AccountSettings, BridgeState, HeldReply, InFlight, PendingDelivery, RemoteTools, RunningJob,
    validate_name,
};

/// The WeChat channel's account store.
pub type Store = scv_channels::state::Store<Account>;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub token: scv_client::Secret,
    pub base_url: String,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

impl scv_channels::state::Credentials for Account {
    fn fingerprint(&self) -> Result<String> {
        let origin = reqwest::Url::parse(&self.base_url)
            .map_err(|_| anyhow!("invalid ClawBot credential origin"))?
            .origin()
            .ascii_serialization();
        // Stable authenticated identities permit token rotation. Legacy
        // credentials have no such proof and stay bound to their original token.
        let encoded = match (&self.bot_id, &self.user_id) {
            (Some(bot), Some(user)) if !bot.is_empty() && !user.is_empty() => {
                serde_json::to_vec(&("identity", origin, bot, user))?
            }
            _ => serde_json::to_vec(&("legacy", origin, &self.token, &self.bot_id, &self.user_id))?,
        };
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }
}

/// Whether a runner holding `token` for `base_url` runs as `saved`: the
/// same token, and the same identity at the same normalized origin.
pub fn runs_as(saved: &Account, token: &str, base_url: &str) -> Result<bool> {
    use scv_channels::state::Credentials as _;
    let supplied = Account {
        token: token.into(),
        base_url: base_url.into(),
        ..saved.clone()
    };
    Ok(saved.token.expose() == token && supplied.fingerprint()? == saved.fingerprint()?)
}

/// The WeChat channel's account store in the instance selected by `SCV_HOME`.
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
/// The caller must stop the account's running component before removing its files.
pub fn remove(name: &str) -> Result<()> {
    store()?.remove(name)
}

#[cfg(test)]
mod tests;
