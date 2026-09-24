//! WeChat channel credentials and where the channel keeps its state.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

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

/// The WeChat channel's directory, `<SCV home>/channels/wechat`. State saved
/// by releases before channels, in `<SCV home>/clawbot`, is moved there first.
pub fn root() -> Result<PathBuf> {
    let home = scv_home()?;
    migrate_legacy_root(&home)?;
    Ok(channel_root(&home))
}

/// The channel's account store, after moving pre-channel state.
pub fn store() -> Result<Store> {
    let home = scv_home()?;
    migrate_legacy_root(&home)?;
    Ok(store_in(&home))
}

/// Move this SCV home's pre-channel state now; see [`migrate_legacy_root`].
pub fn migrate() -> Result<bool> {
    migrate_legacy_root(&scv_home()?)
}

fn scv_home() -> Result<PathBuf> {
    std::env::var_os("SCV_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(".scv")))
        .ok_or_else(|| anyhow!("cannot determine SCV home"))
}

fn channel_root(home: &Path) -> PathBuf {
    home.join("channels").join(crate::CHANNEL)
}

/// The store in `home`, which also reads the single-file credentials of the
/// earliest releases, `<home>/clawbot.toml`, as the `default` account.
fn store_in(home: &Path) -> Store {
    Store::with_legacy(channel_root(home), home.join("clawbot.toml"))
}

/// Move `<home>/clawbot`, the directory releases before channels used, to
/// `<home>/channels/wechat` in one rename. Every account's run and transaction
/// locks are held across it, so a running bridge or login of an older binary
/// makes it fail instead of racing it. Refuses when both directories exist.
/// Returns whether anything moved.
pub fn migrate_legacy_root(home: &Path) -> Result<bool> {
    let legacy = home.join("clawbot");
    match std::fs::symlink_metadata(&legacy) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => bail!("{} is not a directory; move it aside", legacy.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let target = channel_root(home);
    if std::fs::symlink_metadata(&target).is_ok() {
        bail!(
            "both {} (saved before channels) and {} exist; keep one and move the other aside",
            legacy.display(),
            target.display()
        )
    }
    Store::with_legacy(legacy, home.join("clawbot.toml")).relocate(&target)?;
    Ok(true)
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
/// Discover accounts, failing on entry errors or more than 128 entries including
/// an unmigrated legacy default account. Credentials are validated separately.
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
        let store = store_in(directory.path());
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
        let before = std::fs::read(store.path("state", "default").unwrap()).unwrap();
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
                std::fs::read(store.path("state", "default").unwrap()).unwrap(),
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
        let store = store_in(directory.path());
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
            &store.path("accounts", "default").unwrap(),
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
        let store = store_in(directory.path());
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
        let store = store_in(directory.path());
        let legacy = Account {
            bot_id: None,
            user_id: None,
            ..known_account()
        };
        atomic_write(
            &store.path("accounts", "default").unwrap(),
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
        let store = store_in(directory.path());
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

    /// Account, settings, and delivery state as a release before channels
    /// saved them in `<home>/clawbot`.
    fn legacy_layout(home: &Path) -> (Account, AccountSettings, BridgeState) {
        let store = Store::new(home.join("clawbot"));
        let account = known_account();
        store.save_account("default", &account).unwrap();
        let settings = AccountSettings {
            enabled: false,
            workspace: Some(home.join("workspace")),
            remote_tools: RemoteTools::Owner,
        };
        store.save_settings("default", &settings).unwrap();
        let mut state = store
            .bind_state("default", |saved| {
                runs_as(saved, &account.token, &account.base_url)
            })
            .unwrap();
        state.cursor = "cursor".into();
        state.seen = vec!["seen-1".into(), "seen-2".into()];
        state.pending = vec![scv_channels::new_pending(
            "message", "sender", "context", "reply", 1024,
        )];
        state.in_flight = vec![InFlight {
            message_id: "claimed".into(),
            to_user_id: "sender".into(),
            context_token: "ctx".into(),
            key: String::new(),
        }];
        state.held = vec![HeldReply {
            key: "sender".into(),
            to_user_id: "sender".into(),
            reply: "held".into(),
            held_at: 1,
        }];
        store.save_state("default", &state).unwrap();
        (account, settings, state)
    }

    #[test]
    fn legacy_state_moves_into_the_wechat_channel_intact() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let (account, settings, state) = legacy_layout(home);
        let files = ["accounts", "settings", "state"]
            .map(|kind| std::fs::read(home.join(format!("clawbot/{kind}/default.json"))).unwrap());
        assert!(migrate_legacy_root(home).unwrap());
        assert!(!home.join("clawbot").exists());
        let store = Store::new(channel_root(home));
        for (kind, before) in ["accounts", "settings", "state"].iter().zip(&files) {
            let path = store.path(kind, "default").unwrap();
            assert_eq!(&std::fs::read(&path).unwrap(), before, "{kind}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [home.join("channels"), channel_root(home)] {
                assert_eq!(
                    std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        // The moved state still binds to the same credentials, with every
        // cursor, claim, reply, and dedupe ID in place.
        assert!(store.account("default").unwrap().unwrap() == account);
        assert!(store.settings("default").unwrap() == settings);
        let moved = store
            .bind_state("default", |saved| {
                runs_as(saved, &account.token, &account.base_url)
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(&moved).unwrap(),
            serde_json::to_value(&state).unwrap()
        );
        assert!(!migrate_legacy_root(home).unwrap());
    }

    #[test]
    fn legacy_migration_refuses_both_layouts_and_a_running_account() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        legacy_layout(home);
        let running = Store::new(home.join("clawbot")).lock("default").unwrap();
        let error = migrate_legacy_root(home).unwrap_err().to_string();
        assert!(error.contains("in use by a running SCV"), "{error}");
        assert!(home.join("clawbot/accounts/default.json").exists());
        assert!(!channel_root(home).exists());
        drop(running);

        std::fs::create_dir_all(channel_root(home)).unwrap();
        let error = migrate_legacy_root(home).unwrap_err().to_string();
        assert!(error.contains("both"), "{error}");
        assert!(home.join("clawbot/accounts/default.json").exists());
        assert_eq!(std::fs::read_dir(channel_root(home)).unwrap().count(), 0);
    }

    #[test]
    fn earliest_single_file_credentials_stay_readable_from_the_channel() {
        let directory = tempfile::tempdir().unwrap();
        let store = store_in(directory.path());
        atomic_write(
            &directory.path().join("clawbot.toml"),
            "token = 'secret'\nbase_url = 'https://example.test'\n",
        )
        .unwrap();
        assert_eq!(store.account_names().unwrap(), vec!["default"]);
        assert!(store.account("default").unwrap().is_some());
        assert!(!directory.path().join("clawbot.toml").exists());
        assert!(store.path("accounts", "default").unwrap().exists());
    }
}
