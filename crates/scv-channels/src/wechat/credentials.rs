//! WeChat credentials: the iLink bot token and the identity it signed in as.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The WeChat channel's account store.
pub type Store = crate::state::Store<Account>;

/// A WeChat account's saved iLink sign-in.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub token: scv_client::Secret,
    pub base_url: String,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

impl crate::state::Credentials for Account {
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
    use crate::state::Credentials as _;
    let supplied = Account {
        token: token.into(),
        base_url: base_url.into(),
        ..saved.clone()
    };
    Ok(saved.token.expose() == token && supplied.fingerprint()? == saved.fingerprint()?)
}

#[cfg(test)]
mod tests;
