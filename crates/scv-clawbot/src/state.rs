//! WeChat channel credentials and where the channel keeps its state.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use scv_channels::state::{
    AccountSettings, BridgeState, HeldReply, InFlight, PendingDelivery, RemoteTools, validate_name,
};

/// The WeChat channel's account store.
pub type Store = scv_channels::state::Store<Account>;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub token: String,
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
    Ok(saved.token == token && supplied.fingerprint()? == saved.fingerprint()?)
}

/// The WeChat channel's account store in the instance selected by `SCV_HOME`.
pub fn store() -> Result<Store> {
    Store::from_env(crate::CHANNEL)
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
mod tests {
    use super::*;
    use scv_channels::state::{Credentials as _, atomic_write};

    fn known_account() -> Account {
        Account {
            token: "secret".into(),
            base_url: "https://example.test".into(),
            bot_id: Some("bot".into()),
            user_id: Some("user".into()),
        }
    }

    #[test]
    fn replacement_requires_logout_and_preserves_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let mut state = store
            .bind_state("default", |saved| {
                runs_as(saved, &original.token, &original.base_url)
            })
            .unwrap();
        state.pending = vec![scv_channels::new_pending(
            "message",
            "sender",
            "context",
            "private reply",
            1024,
        )];
        store.save_state("default", &state).unwrap();
        let before = std::fs::read(store.state_path("default").unwrap()).unwrap();
        for replacement in [
            Account {
                bot_id: Some("other".into()),
                ..original.clone()
            },
            Account {
                base_url: "https://other.test".into(),
                ..original.clone()
            },
            Account {
                user_id: Some("other".into()),
                ..original.clone()
            },
        ] {
            assert!(
                store
                    .save_account("default", &replacement)
                    .unwrap_err()
                    .to_string()
                    .contains("logout first")
            );
            assert!(store.account("default").unwrap().unwrap() == original);
            assert_eq!(
                std::fs::read(store.state_path("default").unwrap()).unwrap(),
                before
            );
        }
        store.remove("default").unwrap();
        let replacement = Account {
            bot_id: Some("other".into()),
            ..original
        };
        store.save_account("default", &replacement).unwrap();
        assert!(store.save_state("default", &state).is_err());
        assert!(store.load_state("default").unwrap().pending.is_empty());
    }

    #[test]
    fn binding_rejects_externally_replaced_credentials_without_changing_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let mut state = store
            .bind_state("default", |saved| {
                runs_as(saved, &original.token, &original.base_url)
            })
            .unwrap();
        state.pending = vec![scv_channels::new_pending(
            "message",
            "sender",
            "context",
            "private reply",
            1024,
        )];
        store.save_state("default", &state).unwrap();
        let replacement = Account {
            bot_id: Some("replacement".into()),
            ..original
        };
        atomic_write(
            &store.credentials_path("default").unwrap(),
            &serde_json::to_string(&replacement).unwrap(),
        )
        .unwrap();
        let error = store
            .bind_state("default", |saved| {
                runs_as(saved, &replacement.token, &replacement.base_url)
            })
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "channel state does not match saved credentials"
        );
        assert_eq!(
            serde_json::to_value(store.load_state("default").unwrap()).unwrap(),
            serde_json::to_value(state).unwrap()
        );
    }

    #[test]
    fn known_identity_token_rotation_preserves_pending_while_running() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let _running = store.lock("default").unwrap();
        let mut state = store
            .bind_state("default", |saved| {
                runs_as(saved, &original.token, &original.base_url)
            })
            .unwrap();
        state.cursor = "cursor".into();
        state.pending = vec![scv_channels::new_pending(
            "message", "sender", "context", "reply", 1024,
        )];
        store.save_state("default", &state).unwrap();
        let before = serde_json::to_value(&state).unwrap();
        let rotated = Account {
            token: "new secret".into(),
            base_url: "https://EXAMPLE.test:443/".into(),
            ..original.clone()
        };
        store.save_account("default", &rotated).unwrap();
        assert_eq!(
            serde_json::to_value(
                store
                    .bind_state("default", |saved| runs_as(
                        saved,
                        &rotated.token,
                        &rotated.base_url
                    ))
                    .unwrap()
            )
            .unwrap(),
            before
        );
        assert!(
            store
                .bind_state("default", |saved| runs_as(
                    saved,
                    &original.token,
                    &original.base_url
                ))
                .is_err()
        );
        assert!(store.account_snapshot("default").unwrap().0.unwrap() == rotated);
    }

    #[test]
    fn legacy_binding_requires_original_credentials_and_refuses_login_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
        let legacy = Account {
            bot_id: None,
            user_id: None,
            ..known_account()
        };
        atomic_write(
            &store.credentials_path("default").unwrap(),
            &serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        let unbound = BridgeState {
            in_flight: vec![InFlight {
                message_id: "m".into(),
                to_user_id: "sender".into(),
                context_token: "ctx".into(),
                key: String::new(),
            }],
            ..Default::default()
        };
        store.save_state("default", &unbound).unwrap();
        assert!(
            store
                .bind_state("default", |saved| runs_as(
                    saved,
                    "different",
                    &legacy.base_url
                ))
                .is_err()
        );
        assert!(
            store
                .load_state("default")
                .unwrap()
                .credential_fingerprint
                .is_none()
        );
        assert!(
            store
                .save_account("default", &known_account())
                .unwrap_err()
                .to_string()
                .contains("logout first")
        );
        let bound = store
            .bind_state("default", |saved| {
                runs_as(saved, &legacy.token, &legacy.base_url)
            })
            .unwrap();
        assert_eq!(
            bound.credential_fingerprint,
            Some(legacy.fingerprint().unwrap())
        );
        assert_eq!(bound.in_flight[0].message_id, "m");
        assert!(
            store
                .save_account(
                    "default",
                    &Account {
                        token: "rotated".into(),
                        ..legacy.clone()
                    }
                )
                .is_err()
        );
        store.save_account("default", &legacy).unwrap();
    }

    #[test]
    fn replacement_is_rejected_even_without_pending_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
        store.save_account("default", &known_account()).unwrap();
        assert!(
            store
                .save_account(
                    "default",
                    &Account {
                        bot_id: Some("replacement".into()),
                        ..known_account()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn identities_are_backward_compatible() {
        let mut account: Account =
            serde_json::from_str(r#"{"token":"secret","base_url":"https://example.test"}"#)
                .unwrap();
        assert!(account.bot_id.is_none());
        assert!(account.user_id.is_none());
        account.bot_id = Some("bot".into());
        account.user_id = Some("user".into());
        let restored: Account =
            serde_json::from_str(&serde_json::to_string(&account).unwrap()).unwrap();
        assert!(account == restored);
    }
}
