//! Durable ClawBot credentials, settings, and delivery state.

use anyhow::{Result, anyhow, bail};
pub use scv_protocol::RemoteTools;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub token: String,
    pub base_url: String,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountSettings {
    pub enabled: bool,
    pub workspace: Option<PathBuf>,
    /// Remote tool authority; only local CLI/daemon control can change it.
    pub remote_tools: RemoteTools,
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            workspace: None,
            remote_tools: RemoteTools::None,
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PendingDelivery {
    #[serde(default)]
    pub message_id: String,
    pub to_user_id: String,
    pub context_token: String,
    pub reply: String,
    /// Stable client IDs make retries of each chunk idempotent.
    #[serde(default)]
    pub client_ids: Vec<String>,
    #[serde(default)]
    pub next_chunk: usize,
}

/// Written before connecting or submitting a turn. Recovery must never replay it.
#[derive(Clone, Serialize, Deserialize)]
pub struct InFlight {
    pub message_id: String,
    pub to_user_id: String,
    pub context_token: String,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct BridgeState {
    #[serde(default)]
    pub credential_fingerprint: Option<String>,
    pub cursor: String,
    pub seen: Vec<String>,
    #[serde(default)]
    pub pending: Option<PendingDelivery>,
    #[serde(default)]
    pub in_flight: Option<InFlight>,
}

fn credential_fingerprint(account: &Account) -> Result<String> {
    let origin = reqwest::Url::parse(&account.base_url)
        .map_err(|_| anyhow!("invalid ClawBot credential origin"))?
        .origin()
        .ascii_serialization();
    // Stable authenticated identities permit token rotation. Legacy credentials
    // have no such proof and stay bound to their original token.
    let encoded = match (&account.bot_id, &account.user_id) {
        (Some(bot), Some(user)) if !bot.is_empty() && !user.is_empty() => {
            serde_json::to_vec(&("identity", origin, bot, user))?
        }
        _ => serde_json::to_vec(&(
            "legacy",
            origin,
            &account.token,
            &account.bot_id,
            &account.user_id,
        ))?,
    };
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn check_binding(state: &BridgeState, account: Option<&Account>) -> Result<()> {
    if let Some(binding) = &state.credential_fingerprint {
        let matches = account.map(credential_fingerprint).transpose()?.as_ref() == Some(binding);
        if !matches {
            bail!("ClawBot state does not match saved credentials")
        }
    }
    Ok(())
}

pub fn root() -> Result<PathBuf> {
    std::env::var_os("SCV_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(".scv")))
        .map(|p| p.join("clawbot"))
        .ok_or_else(|| anyhow!("cannot determine SCV home"))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid ClawBot account name")
    }
    Ok(())
}

fn check_private(path: &Path, kind: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        bail!("ClawBot {kind} is not a regular file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("ClawBot {kind} file is accessible by other users")
        }
    }
    Ok(())
}

fn private_directory(parent: &Path) -> Result<()> {
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        if matches!(
            parent.file_name().and_then(|name| name.to_str()),
            Some("accounts" | "state" | "settings" | "locks" | "transactions")
        ) && let Some(root) = parent.parent()
        {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent"))?;
    private_directory(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temp.write_all(contents.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|e| anyhow!("atomic state replace failed: {}", e.error))?;
    // Persist the rename and newly created state directory before starting a turn.
    std::fs::File::open(parent)?.sync_all()?;
    if let Some(root) = parent.parent() {
        std::fs::File::open(root)?.sync_all()?;
        if let Some(home) = root.parent() {
            std::fs::File::open(home)?.sync_all()?;
        }
    }
    Ok(())
}

/// Explicit paths keep recovery tests independent of process environment.
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, directory: &str, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(directory).join(format!("{name}.json")))
    }

    fn legacy_path(&self) -> Result<PathBuf> {
        Ok(self
            .root
            .parent()
            .ok_or_else(|| anyhow!("ClawBot state path has no parent"))?
            .join("clawbot.toml"))
    }

    /// Keep the file open for the entire account run. Never unlink lock files:
    /// competing open descriptors must always refer to the same inode.
    pub(crate) fn lock(&self, name: &str) -> Result<std::fs::File> {
        self.file_lock(name, "locks")
    }

    // Only synchronous, short filesystem transactions hold this lock. Never hold
    // it across HTTP, protocol I/O, or a lifetime lock acquisition.
    fn transaction(&self, name: &str) -> Result<std::fs::File> {
        self.file_lock(name, "transactions")
    }

    fn file_lock(&self, name: &str, directory: &str) -> Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.path(directory, name)?;
        private_directory(path.parent().unwrap())?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        check_private(&path, "lock")?;
        // SAFETY: the descriptor is valid for this call; flock neither retains
        // pointers nor closes it. Dropping the file releases the lock.
        let flags = libc::LOCK_EX | libc::LOCK_NB;
        if unsafe { libc::flock(file.as_raw_fd(), flags) } != 0 {
            return Err(anyhow::Error::new(std::io::Error::last_os_error())
                .context("ClawBot account is busy or cannot be locked; retry shortly"));
        }
        Ok(file)
    }

    fn account(&self, name: &str) -> Result<Option<Account>> {
        let _transaction = self.transaction(name)?;
        self.account_unlocked(name)
    }

    fn account_unlocked(&self, name: &str) -> Result<Option<Account>> {
        let path = self.path("accounts", name)?;
        if path.try_exists()? {
            check_private(&path, "account")?;
            return Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?));
        }
        if name != "default" {
            return Ok(None);
        }
        let legacy = self.legacy_path()?;
        if !legacy.try_exists()? {
            return Ok(None);
        }
        check_private(&legacy, "legacy credential")?;
        let account: Account = toml::from_str(&std::fs::read_to_string(&legacy)?)
            .map_err(|_| anyhow!("parse legacy ClawBot credentials"))?;
        atomic_write(&path, &serde_json::to_string(&account)?)?;
        std::fs::remove_file(legacy)?;
        Ok(Some(account))
    }

    pub(crate) fn save_account(&self, name: &str, value: &Account) -> Result<()> {
        let _transaction = self.transaction(name)?;
        let previous = self.account_unlocked(name)?;
        let mut state = self.load_state_unlocked(name)?;
        check_binding(&state, previous.as_ref())?;
        let fingerprint = credential_fingerprint(value)?;
        let changed = previous
            .as_ref()
            .map(credential_fingerprint)
            .transpose()?
            .as_ref()
            != Some(&fingerprint);
        if changed
            && (previous.is_some()
                || state.pending.is_some()
                || state.in_flight.is_some()
                || !state.cursor.is_empty()
                || !state.seen.is_empty())
        {
            bail!("ClawBot credentials cannot replace this account; logout first")
        }
        state.credential_fingerprint = Some(fingerprint);
        // Persist the binding before credentials. A crash between files fails
        // closed; identity replacement never resets or archives a runner's state.
        self.save_state_unlocked(name, &state)?;
        atomic_write(
            &self.path("accounts", name)?,
            &serde_json::to_string(value)?,
        )
    }

    fn settings(&self, name: &str) -> Result<AccountSettings> {
        let _transaction = self.transaction(name)?;
        self.settings_unlocked(name)
    }

    fn settings_unlocked(&self, name: &str) -> Result<AccountSettings> {
        let path = self.path("settings", name)?;
        if !path.try_exists()? {
            return Ok(AccountSettings::default());
        }
        check_private(&path, "settings")?;
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    fn save_settings(&self, name: &str, value: &AccountSettings) -> Result<()> {
        let _transaction = self.transaction(name)?;
        atomic_write(
            &self.path("settings", name)?,
            &serde_json::to_string(value)?,
        )
    }

    fn account_names(&self) -> Result<Vec<String>> {
        const MAX_ENTRIES: usize = 128;
        let mut names = BTreeSet::new();
        if self.legacy_path()?.try_exists()? {
            names.insert("default".to_owned());
        }
        let entries = match std::fs::read_dir(self.root.join("accounts")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(names.into_iter().collect());
            }
            Err(error) => return Err(error.into()),
        };
        // Discovery never reads credentials: one broken account must not hide others.
        let budget = MAX_ENTRIES - names.len();
        for (index, entry) in entries.enumerate() {
            let entry = entry?;
            if index >= budget {
                bail!("ClawBot account discovery exceeds 128 entries")
            }
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && validate_name(name).is_ok()
            {
                names.insert(name.to_owned());
            }
        }
        Ok(names.into_iter().collect())
    }

    pub(crate) fn load_state(&self, name: &str) -> Result<BridgeState> {
        let _transaction = self.transaction(name)?;
        self.load_state_unlocked(name)
    }

    fn load_state_unlocked(&self, name: &str) -> Result<BridgeState> {
        let path = self.path("state", name)?;
        if !path.try_exists()? {
            return Ok(BridgeState::default());
        }
        check_private(&path, "state")?;
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub(crate) fn save_state(&self, name: &str, value: &BridgeState) -> Result<()> {
        let _transaction = self.transaction(name)?;
        check_binding(value, self.account_unlocked(name)?.as_ref())?;
        self.save_state_unlocked(name, value)
    }

    fn save_state_unlocked(&self, name: &str, value: &BridgeState) -> Result<()> {
        atomic_write(&self.path("state", name)?, &serde_json::to_string(value)?)
    }

    pub(crate) fn bind_state(
        &self,
        name: &str,
        token: &str,
        base_url: &str,
    ) -> Result<BridgeState> {
        let _transaction = self.transaction(name)?;
        let account = self
            .account_unlocked(name)?
            .ok_or_else(|| anyhow!("ClawBot saved credentials are unavailable"))?;
        let mut supplied = account.clone();
        supplied.token = token.into();
        supplied.base_url = base_url.into();
        let fingerprint = credential_fingerprint(&account)?;
        if account.token != token || credential_fingerprint(&supplied)? != fingerprint {
            bail!("ClawBot state does not match saved credentials")
        }
        let mut state = self.load_state_unlocked(name)?;
        check_binding(&state, Some(&account))?;
        if state.credential_fingerprint.is_none() {
            state.credential_fingerprint = Some(fingerprint);
            self.save_state_unlocked(name, &state)?;
        }
        Ok(state)
    }

    fn account_snapshot(&self, name: &str) -> Result<(Option<Account>, AccountSettings)> {
        let _transaction = self.transaction(name)?;
        Ok((self.account_unlocked(name)?, self.settings_unlocked(name)?))
    }

    fn remove(&self, name: &str) -> Result<()> {
        let _lock = self.lock(name)?;
        let _transaction = self.transaction(name)?;
        // Both credential layouts must be durably gone before settings can
        // disappear and fall back to enabled-by-default on the next startup.
        remove_if_present(&self.path("accounts", name)?)?;
        if name == "default" {
            remove_if_present(&self.legacy_path()?)?;
        }
        for directory in ["state", "settings"] {
            remove_if_present(&self.path(directory, name)?)?;
        }
        Ok(())
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("ClawBot deletion path has no parent"))?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn account(name: &str) -> Result<Option<Account>> {
    Store::new(root()?).account(name)
}
pub fn save_account(name: &str, value: &Account) -> Result<()> {
    Store::new(root()?).save_account(name, value)
}
/// Read credentials and settings together under the account transaction lock.
pub fn account_snapshot(name: &str) -> Result<(Option<Account>, AccountSettings)> {
    Store::new(root()?).account_snapshot(name)
}
pub fn settings(name: &str) -> Result<AccountSettings> {
    Store::new(root()?).settings(name)
}
pub fn save_settings(name: &str, value: &AccountSettings) -> Result<()> {
    Store::new(root()?).save_settings(name, value)
}
/// Discover accounts, failing on entry errors or more than 128 entries including
/// an unmigrated legacy default account. Credentials are validated separately.
pub fn account_names() -> Result<Vec<String>> {
    Store::new(root()?).account_names()
}
pub fn load_state(name: &str) -> Result<BridgeState> {
    Store::new(root()?).load_state(name)
}
pub fn save_state(name: &str, value: &BridgeState) -> Result<()> {
    Store::new(root()?).save_state(name, value)
}
/// The caller must stop the account's running component before removing its files.
pub fn remove(name: &str) -> Result<()> {
    Store::new(root()?).remove(name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let store = Store::new(directory.path().join("clawbot"));
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let mut state = store
            .bind_state("default", &original.token, &original.base_url)
            .unwrap();
        state.pending = Some(crate::new_pending(
            "message",
            "sender",
            "context",
            "private reply",
            1024,
        ));
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
        assert!(store.load_state("default").unwrap().pending.is_none());
    }

    #[test]
    fn binding_rejects_externally_replaced_credentials_without_changing_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let mut state = store
            .bind_state("default", &original.token, &original.base_url)
            .unwrap();
        state.pending = Some(crate::new_pending(
            "message",
            "sender",
            "context",
            "private reply",
            1024,
        ));
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
            .bind_state("default", &replacement.token, &replacement.base_url)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "ClawBot state does not match saved credentials"
        );
        assert_eq!(
            serde_json::to_value(store.load_state("default").unwrap()).unwrap(),
            serde_json::to_value(state).unwrap()
        );
    }

    #[test]
    fn known_identity_token_rotation_preserves_pending_while_running() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let _running = store.lock("default").unwrap();
        let mut state = store
            .bind_state("default", &original.token, &original.base_url)
            .unwrap();
        state.cursor = "cursor".into();
        state.pending = Some(crate::new_pending(
            "message", "sender", "context", "reply", 1024,
        ));
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
                    .bind_state("default", &rotated.token, &rotated.base_url)
                    .unwrap()
            )
            .unwrap(),
            before
        );
        assert!(
            store
                .bind_state("default", &original.token, &original.base_url)
                .is_err()
        );
        assert!(store.account_snapshot("default").unwrap().0.unwrap() == rotated);
    }

    #[test]
    fn legacy_binding_requires_original_credentials_and_refuses_login_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
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
            in_flight: Some(InFlight {
                message_id: "m".into(),
                to_user_id: "sender".into(),
                context_token: "ctx".into(),
            }),
            ..Default::default()
        };
        store.save_state("default", &unbound).unwrap();
        assert!(
            store
                .bind_state("default", "different", &legacy.base_url)
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
            .bind_state("default", &legacy.token, &legacy.base_url)
            .unwrap();
        assert_eq!(
            bound.credential_fingerprint,
            Some(credential_fingerprint(&legacy).unwrap())
        );
        assert_eq!(bound.in_flight.unwrap().message_id, "m");
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
        let store = Store::new(directory.path().join("clawbot"));
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
    fn transaction_contention_fails_promptly_without_partial_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("clawbot");
        let store = Store::new(root.clone());
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let transaction = store.transaction("default").unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let store = Store::new(root);
            let results = [
                store
                    .save_account(
                        "default",
                        &Account {
                            token: "rotated".into(),
                            ..known_account()
                        },
                    )
                    .err()
                    .unwrap(),
                store.remove("default").err().unwrap(),
                store.account_snapshot("default").err().unwrap(),
                store
                    .save_settings(
                        "default",
                        &AccountSettings {
                            enabled: false,
                            ..Default::default()
                        },
                    )
                    .err()
                    .unwrap(),
            ];
            let kinds = results.map(|error| error.downcast_ref::<std::io::Error>().unwrap().kind());
            send.send(kinds).unwrap();
        });
        let result = receive.recv_timeout(std::time::Duration::from_secs(1));
        drop(transaction);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), [std::io::ErrorKind::WouldBlock; 4]);
        assert!(store.account("default").unwrap().unwrap() == original);
        assert!(store.settings("default").unwrap().enabled);
        store.remove("default").unwrap();
        assert!(store.account_snapshot("default").unwrap().0.is_none());
        store.save_account("default", &known_account()).unwrap();
    }

    #[test]
    fn pending_state_reads_legacy_shape() {
        let state: BridgeState = serde_json::from_str(
            r#"{"cursor":"c","seen":["m"],"pending":{"to_user_id":"u","context_token":"x","reply":"hello"}}"#,
        ).unwrap();
        assert!(state.in_flight.is_none());
        let pending = state.pending.unwrap();
        assert!(pending.client_ids.is_empty());
        assert_eq!(pending.next_chunk, 0);
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

    #[test]
    fn settings_default_to_enabled_and_roundtrip_privately() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
        assert!(store.settings("default").unwrap() == AccountSettings::default());
        let empty: AccountSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.enabled);
        assert!(empty.workspace.is_none());
        assert_eq!(empty.remote_tools, RemoteTools::None);
        assert!(serde_json::from_str::<AccountSettings>(r#"{"enabeld":false}"#).is_err());
        assert!(serde_json::from_str::<AccountSettings>(r#"{"remote_tools":"everyone"}"#).is_err());
        let settings = AccountSettings {
            enabled: false,
            workspace: Some(directory.path().join("workspace")),
            remote_tools: RemoteTools::Owner,
        };
        store.save_settings("default", &settings).unwrap();
        assert!(store.settings("default").unwrap() == settings);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for (path, mode) in [
                (store.root.clone(), 0o700),
                (store.root.join("settings"), 0o700),
                (store.path("settings", "default").unwrap(), 0o600),
            ] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    mode
                );
            }
        }
        assert!(store.save_settings("../escape", &settings).is_err());
    }

    #[test]
    fn discovery_is_bounded_and_ignores_bad_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
        atomic_write(&store.legacy_path().unwrap(), "invalid legacy").unwrap();
        atomic_write(&store.path("accounts", "broken").unwrap(), "not JSON").unwrap();
        atomic_write(&store.root.join("accounts/invalid.name.json"), "{}").unwrap();
        assert_eq!(store.account_names().unwrap(), vec!["broken", "default"]);
        assert!(store.account("broken").is_err());
        for index in 0..140 {
            atomic_write(
                &store.path("accounts", &format!("account-{index}")).unwrap(),
                "{}",
            )
            .unwrap();
        }
        assert!(store.account_names().is_err());
    }

    #[test]
    fn legacy_migration_and_removal_preserve_other_accounts() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
        atomic_write(
            &store.legacy_path().unwrap(),
            "token = 'secret'\nbase_url = 'https://example.test'\n",
        )
        .unwrap();
        assert_eq!(store.account_names().unwrap(), vec!["default"]);
        let account = store.account("default").unwrap().unwrap();
        assert!(account.bot_id.is_none());
        assert!(!store.legacy_path().unwrap().exists());
        store.save_account("other", &account).unwrap();
        store
            .save_settings("default", &AccountSettings::default())
            .unwrap();
        store
            .save_state("default", &BridgeState::default())
            .unwrap();
        store.remove("default").unwrap();
        assert!(store.account("default").unwrap().is_none());
        for directory in ["accounts", "settings", "state"] {
            assert!(!store.path(directory, "default").unwrap().exists());
        }
        assert!(store.account("other").unwrap().unwrap() == account);
    }

    #[test]
    fn account_lock_excludes_other_runs_and_removal() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("clawbot"));
        let lock = store.lock("default").unwrap();
        assert!(store.lock("default").is_err());
        assert!(store.remove("default").is_err());
        assert!(store.lock("other").is_ok());
        drop(lock);
        assert!(store.lock("default").is_ok());
        store.remove("default").unwrap();
    }
}
