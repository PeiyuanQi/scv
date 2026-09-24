//! Durable channel credentials, settings, and delivery state.
//!
//! Each channel keeps its accounts under one private directory,
//! `<SCV home>/channels/<channel>`, with `accounts`, `settings`, `state`,
//! `locks`, and `transactions` beneath it. The channel supplies the
//! credential type and how credentials bind delivery state.

use anyhow::{Result, anyhow, bail};
pub use scv_protocol::RemoteTools;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// A channel account's saved credentials.
pub trait Credentials: Clone + PartialEq + Serialize + DeserializeOwned {
    /// Fingerprint binding delivery state to these credentials. Credentials
    /// that may replace each other without a logout, such as a rotated token
    /// for the same identity, share it.
    fn fingerprint(&self) -> Result<String>;
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
    /// The transport's handle for answering `message_id`; empty for a
    /// message that answers nothing.
    pub context_token: String,
    pub reply: String,
    /// Stable client IDs make retries of each chunk idempotent.
    #[serde(default)]
    pub client_ids: Vec<String>,
    #[serde(default)]
    pub next_chunk: usize,
    /// Conversation that holds this reply if the transport refuses it. Empty
    /// in older state, meaning the direct chat with `to_user_id`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Earlier refused replies this delivery carries ahead of `own`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carried: Vec<HeldReply>,
    /// The reply to `message_id` alone, when `reply` also carries `carried`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<String>,
    /// A notice that is not worth holding when the transport refuses it.
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

/// A reply the transport refused. It is delivered with the conversation's
/// next reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldReply {
    pub key: String,
    pub to_user_id: String,
    pub reply: String,
    /// Unix seconds when the transport refused it.
    pub held_at: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct BridgeState {
    #[serde(default)]
    pub credential_fingerprint: Option<String>,
    /// The transport's checkpoint of what it has received.
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

fn check_binding<C: Credentials>(state: &BridgeState, account: Option<&C>) -> Result<()> {
    if let Some(binding) = &state.credential_fingerprint {
        let matches = account.map(C::fingerprint).transpose()?.as_ref() == Some(binding);
        if !matches {
            bail!("channel state does not match saved credentials")
        }
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid channel account name")
    }
    Ok(())
}

/// Whether an error is lock contention that clears once the holder's short
/// transaction ends.
pub fn is_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
}

fn check_private(path: &Path, kind: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        bail!("channel {kind} is not a regular file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("channel {kind} file is accessible by other users")
        }
    }
    Ok(())
}

/// Create `parent` privately. A store subdirectory also makes its channel
/// directory, and `channels` above it, private.
pub fn private_directory(parent: &Path) -> Result<()> {
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

/// Replace `path` with `contents` atomically, as a private file.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
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

/// One channel's accounts. Explicit paths keep recovery tests independent of
/// process environment.
pub struct Store<C> {
    root: PathBuf,
    /// Single-file credentials of an earliest release, read as the `default`
    /// account and moved into place on first read.
    legacy: Option<PathBuf>,
    credentials: PhantomData<fn() -> C>,
}

impl<C: Credentials> Store<C> {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            legacy: None,
            credentials: PhantomData,
        }
    }

    /// A store that also reads `legacy`, a single-file credential of the
    /// earliest releases, as its `default` account.
    pub fn with_legacy(root: PathBuf, legacy: PathBuf) -> Self {
        Self {
            legacy: Some(legacy),
            ..Self::new(root)
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file of `name` in one of the store's directories.
    pub fn path(&self, directory: &str, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(directory).join(format!("{name}.json")))
    }

    fn legacy_exists(&self) -> Result<bool> {
        match &self.legacy {
            Some(legacy) => Ok(legacy.try_exists()?),
            None => Ok(false),
        }
    }

    /// Keep the file open for the entire account run. Never unlink lock files:
    /// competing open descriptors must always refer to the same inode.
    pub fn lock(&self, name: &str) -> Result<std::fs::File> {
        self.file_lock(name, "locks")
    }

    /// Only synchronous, short filesystem transactions hold this lock. Never
    /// hold it across HTTP, protocol I/O, or a lifetime lock acquisition.
    pub fn transaction(&self, name: &str) -> Result<std::fs::File> {
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
                .context("channel account is busy or cannot be locked; retry shortly"));
        }
        Ok(file)
    }

    pub fn account(&self, name: &str) -> Result<Option<C>> {
        let _transaction = self.transaction(name)?;
        self.account_unlocked(name)
    }

    fn account_unlocked(&self, name: &str) -> Result<Option<C>> {
        let path = self.path("accounts", name)?;
        if path.try_exists()? {
            check_private(&path, "account")?;
            return Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?));
        }
        let Some(legacy) = self.legacy.as_ref().filter(|_| name == "default") else {
            return Ok(None);
        };
        if !legacy.try_exists()? {
            return Ok(None);
        }
        check_private(legacy, "legacy credential")?;
        let account: C = toml::from_str(&std::fs::read_to_string(legacy)?)
            .map_err(|_| anyhow!("parse legacy channel credentials"))?;
        atomic_write(&path, &serde_json::to_string(&account)?)?;
        std::fs::remove_file(legacy)?;
        Ok(Some(account))
    }

    pub fn save_account(&self, name: &str, value: &C) -> Result<()> {
        let _transaction = self.transaction(name)?;
        let previous = self.account_unlocked(name)?;
        let mut state = self.load_state_unlocked(name)?;
        check_binding(&state, previous.as_ref())?;
        let fingerprint = value.fingerprint()?;
        let changed =
            previous.as_ref().map(C::fingerprint).transpose()?.as_ref() != Some(&fingerprint);
        if changed
            && (previous.is_some()
                || !state.pending.is_empty()
                || !state.in_flight.is_empty()
                || !state.held.is_empty()
                || !state.cursor.is_empty()
                || !state.seen.is_empty())
        {
            bail!("channel credentials cannot replace this account; logout first")
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

    pub fn settings(&self, name: &str) -> Result<AccountSettings> {
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

    pub fn save_settings(&self, name: &str, value: &AccountSettings) -> Result<()> {
        let _transaction = self.transaction(name)?;
        atomic_write(
            &self.path("settings", name)?,
            &serde_json::to_string(value)?,
        )
    }

    /// Discover accounts, failing on entry errors or more than 128 entries
    /// including an unmigrated legacy default account. Credentials are
    /// validated separately.
    pub fn account_names(&self) -> Result<Vec<String>> {
        const MAX_ENTRIES: usize = 128;
        let mut names = BTreeSet::new();
        if self.legacy_exists()? {
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
                bail!("channel account discovery exceeds 128 entries")
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

    pub fn load_state(&self, name: &str) -> Result<BridgeState> {
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

    pub fn save_state(&self, name: &str, value: &BridgeState) -> Result<()> {
        let _transaction = self.transaction(name)?;
        check_binding(value, self.account_unlocked(name)?.as_ref())?;
        self.save_state_unlocked(name, value)
    }

    fn save_state_unlocked(&self, name: &str, value: &BridgeState) -> Result<()> {
        atomic_write(&self.path("state", name)?, &serde_json::to_string(value)?)
    }

    /// Load the account's delivery state for a run with the credentials the
    /// runner holds. `running` says whether they are the saved credentials;
    /// unbound state from older releases is bound to them before first use.
    pub fn bind_state(
        &self,
        name: &str,
        running: impl FnOnce(&C) -> Result<bool>,
    ) -> Result<BridgeState> {
        let _transaction = self.transaction(name)?;
        let account = self
            .account_unlocked(name)?
            .ok_or_else(|| anyhow!("channel saved credentials are unavailable"))?;
        if !running(&account)? {
            bail!("channel state does not match saved credentials")
        }
        let mut state = self.load_state_unlocked(name)?;
        check_binding(&state, Some(&account))?;
        if state.credential_fingerprint.is_none() {
            state.credential_fingerprint = Some(account.fingerprint()?);
            self.save_state_unlocked(name, &state)?;
        }
        Ok(state)
    }

    /// Read credentials and settings together under the account transaction lock.
    pub fn account_snapshot(&self, name: &str) -> Result<(Option<C>, AccountSettings)> {
        let _transaction = self.transaction(name)?;
        Ok((self.account_unlocked(name)?, self.settings_unlocked(name)?))
    }

    /// The caller must stop the account's running component before removing
    /// its files.
    pub fn remove(&self, name: &str) -> Result<()> {
        let _lock = self.lock(name)?;
        let _transaction = self.transaction(name)?;
        // Both credential layouts must be durably gone before settings can
        // disappear and fall back to enabled-by-default on the next startup.
        remove_if_present(&self.path("accounts", name)?)?;
        if name == "default"
            && let Some(legacy) = &self.legacy
        {
            remove_if_present(legacy)?;
        }
        for directory in ["state", "settings"] {
            remove_if_present(&self.path(directory, name)?)?;
        }
        Ok(())
    }

    /// Move the whole store to `target`, which must not exist, in one rename.
    /// Every account's lifetime and transaction locks are held across it, so
    /// a running bridge or login makes it fail instead of racing it.
    pub fn relocate(&self, target: &Path) -> Result<()> {
        let mut held = Vec::new();
        for name in self.account_names()? {
            let busy =
                || anyhow!("the account {name} is in use by a running SCV; stop it, then retry");
            held.push(self.lock(&name).map_err(|_| busy())?);
            held.push(self.transaction(&name).map_err(|_| busy())?);
        }
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("channel path has no parent"))?;
        private_directory(parent)?;
        std::fs::rename(&self.root, target)?;
        std::fs::File::open(parent)?.sync_all()?;
        if let Some(home) = parent.parent() {
            std::fs::File::open(home)?.sync_all()?;
        }
        Ok(())
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("channel deletion path has no parent"))?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credentials identified by `id` alone, so a new `secret` rotates them.
    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    struct Test {
        id: String,
        secret: String,
    }

    impl Credentials for Test {
        fn fingerprint(&self) -> Result<String> {
            Ok(format!("id:{}", self.id))
        }
    }

    fn credentials(id: &str) -> Test {
        Test {
            id: id.into(),
            secret: "secret".into(),
        }
    }

    fn store(directory: &Path) -> Store<Test> {
        Store::with_legacy(
            directory.join("channels/test"),
            directory.join("legacy.toml"),
        )
    }

    #[test]
    fn transaction_contention_fails_promptly_without_partial_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let store = store(&root);
        let original = credentials("a");
        store.save_account("default", &original).unwrap();
        let transaction = store.transaction("default").unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let store = self::store(&root);
            let results = [
                store
                    .save_account(
                        "default",
                        &Test {
                            secret: "rotated".into(),
                            ..credentials("a")
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
        store.save_account("default", &credentials("a")).unwrap();
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
    fn settings_default_to_enabled_and_roundtrip_privately() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
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
                (directory.path().join("channels"), 0o700),
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
        let store = store(directory.path());
        atomic_write(store.legacy.as_ref().unwrap(), "invalid legacy").unwrap();
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
        let store = store(directory.path());
        let legacy = store.legacy.clone().unwrap();
        atomic_write(&legacy, "id = 'a'\nsecret = 'secret'\n").unwrap();
        assert_eq!(store.account_names().unwrap(), vec!["default"]);
        let account = store.account("default").unwrap().unwrap();
        assert!(account == credentials("a"));
        assert!(!legacy.exists());
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
        let store = store(directory.path());
        let lock = store.lock("default").unwrap();
        assert!(store.lock("default").is_err());
        assert!(store.remove("default").is_err());
        assert!(store.lock("other").is_ok());
        drop(lock);
        assert!(store.lock("default").is_ok());
        store.remove("default").unwrap();
    }

    #[test]
    fn relocation_moves_everything_and_refuses_a_running_account() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        store.save_account("default", &credentials("a")).unwrap();
        let target = directory.path().join("moved/test");
        let running = store.lock("default").unwrap();
        let error = store.relocate(&target).unwrap_err().to_string();
        assert!(error.contains("in use by a running SCV"), "{error}");
        assert!(!target.exists());
        drop(running);
        store.relocate(&target).unwrap();
        assert!(!store.root.exists());
        let moved = Store::<Test>::new(target);
        assert!(moved.account("default").unwrap().unwrap() == credentials("a"));
    }
}
