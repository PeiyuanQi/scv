//! Manually entered Slack tokens and their verified installation identity.
use anyhow::{Result, bail};
use scv_client::Secret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Api, Login, api, socket, valid_id};

pub(crate) type Store = crate::state::Store<Account>;

/// Tokens supplied by the operator; IDs checked with Slack before saving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub bot_token: Secret,
    pub app_token: Secret,
    pub team_id: String,
    pub app_id: String,
    pub bot_user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<String>,
}

impl Account {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_tokens(&self.bot_token, &self.app_token)?;
        validate_owner(self.owner_user_id.as_deref())?;
        if !valid_id(&self.team_id, "T")
            || !valid_id(&self.app_id, "A")
            || !valid_id(&self.bot_user_id, "UW")
        {
            bail!("invalid Slack installation identity; sign in again")
        }
        Ok(())
    }
}

impl crate::state::Credentials for Account {
    fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                "slack",
                &self.team_id,
                &self.app_id,
                &self.bot_user_id,
                &self.owner_user_id,
            ))?)
        ))
    }
}

pub(crate) fn validate_owner(owner: Option<&str>) -> Result<()> {
    if owner.is_some_and(|id| !valid_id(id, "UW")) {
        bail!("invalid Slack owner user ID; copy the member ID (U... or W...), not a display name")
    }
    Ok(())
}

pub(crate) fn validate_tokens(bot: &str, app: &str) -> Result<()> {
    let valid = |token: &str, prefix: &str| {
        token.len() <= 512
            && token.strip_prefix(prefix).is_some_and(|rest| {
                !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
    };
    if !valid(bot, "xoxb-") {
        bail!(
            "invalid Slack bot token; manually enter a non-rotating xoxb- token from OAuth & Permissions"
        )
    }
    if !valid(app, "xapp-") {
        bail!(
            "invalid Slack app-level token; manually enter a non-rotating xapp- token with connections:write from Basic Information"
        )
    }
    Ok(())
}

pub(crate) async fn login(store: &Store, name: &str, login: Login) -> Result<()> {
    let api = Api::new(login.bot_token.clone(), login.app_token.clone())?;
    login_with_api(store, name, login, &api).await
}

/// Check the tokens with Slack (the bot's installation, then a Socket Mode
/// hello for the same app) before saving anything.
async fn login_with_api(store: &Store, name: &str, login: Login, api: &Api) -> Result<()> {
    crate::state::validate_name(name)?;
    validate_tokens(&login.bot_token, &login.app_token)?;
    validate_owner(login.owner_user_id.as_deref())?;
    let identity = api.identity().await?;
    let mut link = socket::Link::connect(api, &identity.app_id).await?;
    link.close().await;
    let account = Account {
        bot_token: login.bot_token,
        app_token: login.app_token,
        team_id: identity.team_id,
        app_id: identity.app_id,
        bot_user_id: identity.bot_user_id,
        owner_user_id: login.owner_user_id,
    };
    account.validate()?;
    // The store's binding lets tokens change only for the same identity.
    store.save_account(name, &account)?;
    println!("Slack tokens verified and saved; Socket Mode connected.");
    let missing = identity
        .scopes
        .as_deref()
        .map(|granted| missing_scopes(granted))
        .unwrap_or_default();
    if !missing.is_empty() {
        println!(
            "Warning: the bot token lacks {}. Add them under OAuth & Permissions and reinstall \
             the app; until then what needs them fails (see docs/channels.md#slack-contract).",
            missing.join(", ")
        );
    }
    if account.owner_user_id.is_none() {
        println!(
            "No owner recorded: an owner-only account answers nobody and tools stay off. Sign in \
             again with --slack-owner-user-id, or set senders = \"anyone\"."
        );
    }
    Ok(())
}

/// The scopes SCV uses that `granted`, `auth.test`'s comma-separated list,
/// leaves out.
fn missing_scopes(granted: &str) -> Vec<&'static str> {
    let granted: Vec<&str> = granted.split(',').map(str::trim).collect();
    api::BOT_SCOPES
        .into_iter()
        .filter(|scope| !granted.contains(scope))
        .collect()
}

#[cfg(test)]
mod tests;
