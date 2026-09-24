//! Feishu channel credentials and where the channel keeps its state.

use anyhow::{Result, bail};
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
    pub app_secret: String,
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
mod tests {
    use super::*;
    use scv_channels::state::Credentials as _;

    fn account() -> Account {
        Account {
            app_id: "cli_a1b2c3d4e5f60718".into(),
            app_secret: "secret".into(),
            brand: Brand::Feishu,
            owner_open_id: Some("ou_0123abcd".into()),
        }
    }

    #[test]
    fn secret_rotation_keeps_the_binding_but_app_or_owner_changes_do_not() {
        let base = account();
        let rotated = Account {
            app_secret: "other".into(),
            ..account()
        };
        assert_eq!(base.fingerprint().unwrap(), rotated.fingerprint().unwrap());
        for changed in [
            Account {
                app_id: "cli_ffff".into(),
                ..account()
            },
            Account {
                owner_open_id: None,
                ..account()
            },
            Account {
                brand: Brand::Lark,
                ..account()
            },
        ] {
            assert_ne!(base.fingerprint().unwrap(), changed.fingerprint().unwrap());
        }
    }

    #[test]
    fn debug_never_shows_the_secret() {
        assert!(!format!("{:?}", account()).contains("secret\""));
        assert!(format!("{:?}", account()).contains("<redacted>"));
    }

    #[test]
    fn ids_are_checked_before_use() {
        assert!(account().validate().is_ok());
        for app_id in ["", "cli_", "app_123", "cli_12/34", "cli_1 2"] {
            assert!(validate_app_id(app_id).is_err(), "{app_id}");
        }
        for open_id in ["", "ou_", "on_123", "ou_a b", "ou_a\"b"] {
            assert!(validate_open_id(open_id).is_err(), "{open_id}");
        }
        let bad_secret = Account {
            app_secret: "a\nb".into(),
            ..account()
        };
        assert!(bad_secret.validate().is_err());
    }

    #[test]
    fn saved_accounts_are_private_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_channels::Layout::new(dir.path()), crate::CHANNEL);
        store.save_account("default", &account()).unwrap();
        assert!(store.account("default").unwrap() == Some(account()));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(store.credentials_path("default").unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Another owner is another identity: it needs a logout first.
        let other = Account {
            owner_open_id: Some("ou_other".into()),
            ..account()
        };
        assert!(store.save_account("default", &other).is_err());
    }
}
