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
    /// Conversation that holds this reply if iLink refuses it. Empty in older
    /// state, meaning the direct chat with `to_user_id`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Earlier refused replies this delivery carries ahead of `own`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carried: Vec<HeldReply>,
    /// The reply to `message_id` alone, when `reply` also carries `carried`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<String>,
    /// A notice that is not worth holding when iLink refuses it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transient: bool,
}

/// Written before connecting or submitting a turn. Recovery must never replay it.
#[derive(Clone, Serialize, Deserialize)]
pub struct InFlight {
    pub message_id: String,
    pub to_user_id: String,
    pub context_token: String,
    /// The sender's conversation; empty in older state (direct chat).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

/// A reply iLink refused. It is delivered with the conversation's next reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldReply {
    pub key: String,
    pub to_user_id: String,
    pub reply: String,
    /// Unix seconds when iLink refused it.
    pub held_at: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct BridgeState {
    #[serde(default)]
    pub credential_fingerprint: Option<String>,
    pub cursor: String,
    pub seen: Vec<String>,
    /// Completed replies awaiting delivery, oldest first.
    #[serde(default, with = "one_or_many")]
    pub pending: Vec<PendingDelivery>,
    /// Claimed messages whose turns have not completed, oldest first.
    #[serde(default, with = "one_or_many")]
    pub in_flight: Vec<InFlight>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<HeldReply>,
}

/// Older bridges stored at most one pending reply and one claim as a single
/// object or null. Lists keep that shape while they hold at most one entry,
/// so older binaries still read state that has no concurrent work.
mod one_or_many {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ptr_arg)]
    pub fn serialize<T: Serialize, S: Serializer>(
        items: &Vec<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match items.as_slice() {
            [] => serializer.serialize_none(),
            [one] => one.serialize(serializer),
            many => many.serialize(serializer),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<T>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany<T> {
            Many(Vec<T>),
            One(T),
        }
        Ok(match Option::<OneOrMany<T>>::deserialize(deserializer)? {
            None => Vec::new(),
            Some(OneOrMany::One(one)) => vec![one],
            Some(OneOrMany::Many(many)) => many,
        })
    }
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

/// The WeChat channel's directory, `<SCV home>/channels/wechat`. State saved
/// by releases before channels, in `<SCV home>/clawbot`, is moved there first.
pub fn root() -> Result<PathBuf> {
    let home = scv_home()?;
    migrate_legacy_root(&home)?;
    Ok(channel_root(&home))
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
    let old = Store::new(legacy.clone());
    let mut held = Vec::new();
    for name in old.account_names()? {
        let busy =
            || anyhow!("the WeChat account {name} is in use by a running SCV; stop it, then retry");
        held.push(old.lock(&name).map_err(|_| busy())?);
        held.push(old.transaction(&name).map_err(|_| busy())?);
    }
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("channel path has no parent"))?;
    private_directory(parent)?;
    std::fs::rename(&legacy, &target)?;
    std::fs::File::open(parent)?.sync_all()?;
    std::fs::File::open(home)?.sync_all()?;
    Ok(true)
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

/// Whether an error is lock contention that clears once the holder's short
/// transaction ends.
pub(crate) fn is_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
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
            // Channel names are private too.
            if let Some(channels) = root.parent()
                && channels.file_name().and_then(|name| name.to_str()) == Some("channels")
            {
                std::fs::set_permissions(channels, std::fs::Permissions::from_mode(0o700))?;
            }
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
    // Persist the rename and any newly created directories (the file's own,
    // the channel's, `channels`, and the SCV home) before starting a turn.
    for directory in parent.ancestors().take(4) {
        std::fs::File::open(directory)?.sync_all()?;
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

    /// The single-account credential file of the earliest releases,
    /// `<SCV home>/clawbot.toml`, beside both the old and the channel layout.
    fn legacy_path(&self) -> Result<PathBuf> {
        let parent = self
            .root
            .parent()
            .ok_or_else(|| anyhow!("ClawBot state path has no parent"))?;
        let home = if parent.file_name().and_then(|name| name.to_str()) == Some("channels") {
            parent
                .parent()
                .ok_or_else(|| anyhow!("ClawBot state path has no parent"))?
        } else {
            parent
        };
        Ok(home.join("clawbot.toml"))
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
                || !state.pending.is_empty()
                || !state.in_flight.is_empty()
                || !state.held.is_empty()
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
        let store = Store::new(directory.path().join("channels/wechat"));
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let mut state = store
            .bind_state("default", &original.token, &original.base_url)
            .unwrap();
        state.pending = vec![crate::new_pending(
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
        let store = Store::new(directory.path().join("channels/wechat"));
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let mut state = store
            .bind_state("default", &original.token, &original.base_url)
            .unwrap();
        state.pending = vec![crate::new_pending(
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
        let store = Store::new(directory.path().join("channels/wechat"));
        let original = known_account();
        store.save_account("default", &original).unwrap();
        let _running = store.lock("default").unwrap();
        let mut state = store
            .bind_state("default", &original.token, &original.base_url)
            .unwrap();
        state.cursor = "cursor".into();
        state.pending = vec![crate::new_pending(
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
        let store = Store::new(directory.path().join("channels/wechat"));
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
        let store = Store::new(directory.path().join("channels/wechat"));
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
        let root = directory.path().join("channels/wechat");
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
        assert!(state.in_flight.is_empty());
        let [pending] = state.pending.as_slice() else {
            panic!("one legacy pending reply")
        };
        assert!(pending.client_ids.is_empty());
        assert_eq!(pending.next_chunk, 0);
        assert!(pending.key.is_empty() && pending.carried.is_empty() && !pending.transient);
        let claim: BridgeState = serde_json::from_str(
            r#"{"cursor":"c","seen":[],"pending":null,"in_flight":{"message_id":"m","to_user_id":"u","context_token":"x"}}"#,
        )
        .unwrap();
        assert_eq!(claim.in_flight.len(), 1);
        assert!(claim.pending.is_empty());
    }

    #[test]
    fn single_entries_keep_the_legacy_shape_and_many_become_lists() {
        let claim = |id: &str| InFlight {
            message_id: id.into(),
            to_user_id: "u".into(),
            context_token: "x".into(),
            key: String::new(),
        };
        let mut state = BridgeState::default();
        let value = serde_json::to_value(&state).unwrap();
        assert!(value["pending"].is_null() && value["in_flight"].is_null());
        assert!(value.get("held").is_none());
        state.in_flight.push(claim("a"));
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["in_flight"]["message_id"], "a");
        assert!(value["in_flight"].get("key").is_none());
        state.in_flight.push(claim("b"));
        let restored: BridgeState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let ids: Vec<_> = restored
            .in_flight
            .iter()
            .map(|c| c.message_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
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
        let store = Store::new(directory.path().join("channels/wechat"));
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
        let store = Store::new(directory.path().join("channels/wechat"));
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
        let store = Store::new(directory.path().join("channels/wechat"));
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
            .bind_state("default", &account.token, &account.base_url)
            .unwrap();
        state.cursor = "cursor".into();
        state.seen = vec!["seen-1".into(), "seen-2".into()];
        state.pending = vec![crate::new_pending(
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
            .bind_state("default", &account.token, &account.base_url)
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
        let store = Store::new(channel_root(directory.path()));
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

    #[test]
    fn account_lock_excludes_other_runs_and_removal() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("channels/wechat"));
        let lock = store.lock("default").unwrap();
        assert!(store.lock("default").is_err());
        assert!(store.remove("default").is_err());
        assert!(store.lock("other").is_ok());
        drop(lock);
        assert!(store.lock("default").is_ok());
        store.remove("default").unwrap();
    }
}
