//! Durable channel credentials, settings, and delivery state.
//!
//! A channel's accounts follow the instance layout ([`Layout`]): credentials
//! under `credentials/<channel>`, settings in `config.toml`, and delivery
//! state and locks under `state/channels/<channel>`. The channel supplies the
//! credential type and how credentials bind delivery state.

use anyhow::{Result, anyhow, bail};
use scv_client::Layout;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Background jobs direct-chat sessions are running, so the next run can
    /// tell each chat which of its jobs a restart stopped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<RunningJob>,
}

/// A background job a direct chat's session started and has not reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningJob {
    /// The chat partner it reports to.
    pub to_user_id: String,
    /// The session's job handle, such as `job-1`.
    pub job: String,
    /// The delegating tool, such as `agent_codex`.
    pub tool: String,
    /// The first line of the delegated prompt, shortened.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// Unix seconds when it was first recorded.
    pub started_at: u64,
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

/// Create `directory` privately. Every directory between the instance home
/// and it is made private too, because channel and account names are.
pub fn private_directory(home: &Path, directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in directory.ancestors() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            if path == home || !path.starts_with(home) {
                break;
            }
        }
    }
    Ok(())
}

/// Replace `path` with `contents` atomically, as a private file in a private
/// directory.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent"))?;
    private_directory(parent, parent)?;
    replace_file(path, contents)
}

/// Replace `path` with `contents` atomically, as a private file, in its
/// existing directory.
fn replace_file(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent"))?;
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
    // `channels`, `state` or `credentials`, and the SCV home) before starting
    // a turn.
    for directory in parent.ancestors().take(4) {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

/// Largest `config.toml` SCV reads.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The instance's `config.toml`, or `None` when there is none. It holds the
/// provider key, so it must be private, and its text is never echoed.
fn read_config(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "{} is readable by group or others; run chmod 600",
                path.display()
            )
        }
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!("{} exceeds 1 MiB", path.display())
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

/// Where a TOML error is, without the source text it would otherwise quote.
fn toml_line(text: &str, span: Option<std::ops::Range<usize>>) -> String {
    span.map(|span| {
        let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
        format!(" (line {line})")
    })
    .unwrap_or_default()
}

/// One channel's accounts in an SCV instance:
///
/// - credentials in `credentials/<channel>/<account>.json`;
/// - settings in `config.toml`, as `[channels.<channel>.<account>]`;
/// - delivery state in `state/channels/<channel>/<account>.json`, beside the
///   account's `.lock` (held for a whole run) and `.transaction` (held for
///   each short filesystem transaction).
pub struct Store<C> {
    channel: String,
    home: PathBuf,
    credentials: PathBuf,
    state: PathBuf,
    config: PathBuf,
    config_lock: PathBuf,
    kind: PhantomData<fn() -> C>,
}

impl<C: Credentials> Store<C> {
    pub fn new(layout: &Layout, channel: &str) -> Self {
        Self {
            channel: channel.to_owned(),
            home: layout.home().to_owned(),
            credentials: layout.channel_credentials(channel),
            state: layout.channel_state(channel),
            config: layout.config(),
            config_lock: layout.config_lock(),
            kind: PhantomData,
        }
    }

    /// The store of the instance selected by `SCV_HOME`.
    pub fn from_env(channel: &str) -> Result<Self> {
        Ok(Self::new(&Layout::from_env()?, channel))
    }

    pub fn credentials_path(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.credentials.join(format!("{name}.json")))
    }

    pub fn state_path(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.state.join(format!("{name}.json")))
    }

    /// Keep the file open for the entire account run. Never unlink lock files:
    /// competing open descriptors must always refer to the same inode.
    pub fn lock(&self, name: &str) -> Result<std::fs::File> {
        validate_name(name)?;
        self.file_lock(&self.state.join(format!("{name}.lock")))
    }

    /// Only synchronous, short filesystem transactions hold this lock. Never
    /// hold it across HTTP, protocol I/O, or a lifetime lock acquisition.
    pub fn transaction(&self, name: &str) -> Result<std::fs::File> {
        validate_name(name)?;
        self.file_lock(&self.state.join(format!("{name}.transaction")))
    }

    fn file_lock(&self, path: &Path) -> Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        private_directory(&self.home, path.parent().unwrap())?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        check_private(path, "lock")?;
        // SAFETY: the descriptor is valid for this call; flock neither retains
        // pointers nor closes it. Dropping the file releases the lock.
        let flags = libc::LOCK_EX | libc::LOCK_NB;
        if unsafe { libc::flock(file.as_raw_fd(), flags) } != 0 {
            return Err(anyhow::Error::new(std::io::Error::last_os_error())
                .context("channel account is busy or cannot be locked; retry shortly"));
        }
        Ok(file)
    }

    fn write_private(&self, path: &Path, contents: &str) -> Result<()> {
        private_directory(&self.home, path.parent().unwrap())?;
        atomic_write(path, contents)
    }

    pub fn account(&self, name: &str) -> Result<Option<C>> {
        let _transaction = self.transaction(name)?;
        self.account_unlocked(name)
    }

    fn account_unlocked(&self, name: &str) -> Result<Option<C>> {
        let path = self.credentials_path(name)?;
        if !path.try_exists()? {
            return Ok(None);
        }
        check_private(&path, "account")?;
        Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
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
        self.write_private(
            &self.credentials_path(name)?,
            &serde_json::to_string(value)?,
        )
    }

    pub fn settings(&self, name: &str) -> Result<AccountSettings> {
        let _transaction = self.transaction(name)?;
        self.settings_unlocked(name)
    }

    /// `[channels.<channel>.<name>]` from `config.toml`; defaults when absent.
    fn settings_unlocked(&self, name: &str) -> Result<AccountSettings> {
        validate_name(name)?;
        let Some(text) = read_config(&self.config)? else {
            return Ok(AccountSettings::default());
        };
        let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
            anyhow!(
                "{} is not valid TOML{}",
                self.config.display(),
                toml_line(&text, error.span())
            )
        })?;
        let section = format!("[channels.{}.{name}]", self.channel);
        let Some(channels) = table.get("channels") else {
            return Ok(AccountSettings::default());
        };
        let Some(accounts) = channels
            .as_table()
            .ok_or_else(|| anyhow!("[channels] in config.toml must be a table"))?
            .get(&self.channel)
        else {
            return Ok(AccountSettings::default());
        };
        let Some(settings) = accounts
            .as_table()
            .ok_or_else(|| anyhow!("[channels.{}] in config.toml must be a table", self.channel))?
            .get(name)
        else {
            return Ok(AccountSettings::default());
        };
        // Account settings hold no secrets, so their own errors are safe to show.
        settings
            .clone()
            .try_into()
            .map_err(|error: toml::de::Error| {
                anyhow!("{section} in config.toml: {}", error.message())
            })
    }

    pub fn save_settings(&self, name: &str, value: &AccountSettings) -> Result<()> {
        let _transaction = self.transaction(name)?;
        self.edit_settings(name, Some(value))
    }

    /// Set or remove `[channels.<channel>.<name>]` in `config.toml`, keeping
    /// the rest of the file, comments included, as the person wrote it.
    fn edit_settings(&self, name: &str, value: Option<&AccountSettings>) -> Result<()> {
        use toml_edit::{DocumentMut, Item, Table};
        validate_name(name)?;
        let _config = self.file_lock(&self.config_lock)?;
        let text = read_config(&self.config)?.unwrap_or_default();
        let mut document: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
            anyhow!(
                "{} is not valid TOML{}; fix it before changing channel settings",
                self.config.display(),
                toml_line(&text, error.span())
            )
        })?;
        let implicit = || {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        };
        match value {
            Some(settings) => {
                let accounts = document
                    .entry("channels")
                    .or_insert_with(implicit)
                    .as_table_mut()
                    .ok_or_else(|| anyhow!("[channels] in config.toml must be a table"))?
                    .entry(&self.channel)
                    .or_insert_with(implicit)
                    .as_table_mut()
                    .ok_or_else(|| {
                        anyhow!("[channels.{}] in config.toml must be a table", self.channel)
                    })?;
                let table = accounts
                    .entry(name)
                    .or_insert_with(|| Item::Table(Table::new()))
                    .as_table_mut()
                    .ok_or_else(|| {
                        anyhow!(
                            "[channels.{}.{name}] in config.toml must be a table",
                            self.channel
                        )
                    })?;
                table["enabled"] = toml_edit::value(settings.enabled);
                match &settings.workspace {
                    Some(path) => {
                        let path = path
                            .to_str()
                            .ok_or_else(|| anyhow!("channel workspace path is not UTF-8"))?;
                        table["workspace"] = toml_edit::value(path);
                    }
                    None => {
                        table.remove("workspace");
                    }
                }
                table["remote_tools"] = toml_edit::value(match settings.remote_tools {
                    RemoteTools::None => "none",
                    RemoteTools::Owner => "owner",
                });
            }
            None => {
                let Some(channels) = document.get_mut("channels").and_then(Item::as_table_mut)
                else {
                    return Ok(());
                };
                let Some(accounts) = channels.get_mut(&self.channel).and_then(Item::as_table_mut)
                else {
                    return Ok(());
                };
                if accounts.remove(name).is_none() {
                    return Ok(());
                }
                if accounts.is_empty() {
                    channels.remove(&self.channel);
                }
                if channels.is_empty() {
                    document.remove("channels");
                }
            }
        }
        // Write through a symlinked config.toml rather than replacing the
        // link, leaving the directory that holds it as it is.
        let target = std::fs::canonicalize(&self.config).unwrap_or_else(|_| self.config.clone());
        replace_file(&target, &document.to_string())
    }

    /// Discover accounts from saved credentials, failing on entry errors or
    /// more than 128 entries. Credentials are validated separately.
    pub fn account_names(&self) -> Result<Vec<String>> {
        const MAX_ENTRIES: usize = 128;
        let mut names = BTreeSet::new();
        let entries = match std::fs::read_dir(&self.credentials) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        };
        // Discovery never reads credentials: one broken account must not hide others.
        for (index, entry) in entries.enumerate() {
            let entry = entry?;
            if index >= MAX_ENTRIES {
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
        let path = self.state_path(name)?;
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
        self.write_private(&self.state_path(name)?, &serde_json::to_string(value)?)
    }

    /// Load the account's delivery state for a run with the credentials the
    /// runner holds. `running` says whether they are the saved credentials;
    /// unbound state is bound to them before first use.
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

    /// A lock-free look at one account for display: its credentials and its
    /// settings, each parsed. It never holds a lock a running bridge needs;
    /// every file is replaced atomically, so each read is whole.
    pub fn inspect(&self, name: &str) -> (Result<Option<C>>, Result<AccountSettings>) {
        (self.account_unlocked(name), self.settings_unlocked(name))
    }

    /// Accounts that have a `[channels.<channel>.<account>]` table.
    pub fn configured_accounts(&self) -> Result<Vec<String>> {
        let Some(text) = read_config(&self.config)? else {
            return Ok(Vec::new());
        };
        let table: toml::Table = text
            .parse()
            .map_err(|_| anyhow!("{} is not valid TOML", self.config.display()))?;
        Ok(table
            .get("channels")
            .and_then(|channels| channels.get(&self.channel))
            .and_then(toml::Value::as_table)
            .map(|accounts| accounts.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Remove the account's credentials, delivery state, and settings. The
    /// caller must stop the account's running component first.
    pub fn remove(&self, name: &str) -> Result<()> {
        let _lock = self.lock(name)?;
        let _transaction = self.transaction(name)?;
        // Credentials must be durably gone before settings can disappear and
        // fall back to enabled-by-default on the next startup.
        remove_if_present(&self.credentials_path(name)?)?;
        remove_if_present(&self.state_path(name)?)?;
        self.edit_settings(name, None)
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
        Store::new(&Layout::new(directory), "test")
    }

    fn mode(path: impl AsRef<Path>) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
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
    fn settings_live_in_config_toml_and_keep_the_rest_of_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        assert!(store.settings("default").unwrap() == AccountSettings::default());
        let empty: AccountSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.enabled);
        assert!(empty.workspace.is_none());
        assert_eq!(empty.remote_tools, RemoteTools::None);

        let config = directory.path().join("config.toml");
        let original = "# my provider\n[provider]\nactive = \"openai\" # keep this\n";
        atomic_write(&config, original).unwrap();
        let settings = AccountSettings {
            enabled: false,
            workspace: Some(directory.path().join("workspace")),
            remote_tools: RemoteTools::Owner,
        };
        store.save_settings("default", &settings).unwrap();
        assert!(store.settings("default").unwrap() == settings);
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with(original), "{text}");
        assert!(
            text.contains("[channels.test.default]\nenabled = false\n"),
            "{text}"
        );
        assert!(!text.contains("[channels]\n"), "{text}");
        assert_eq!(mode(&config), 0o600);

        // A person's edit is read back, and their own keys and comments survive
        // SCV's next change.
        let edited = text.replace("enabled = false", "enabled = true # on again");
        atomic_write(&config, &edited).unwrap();
        assert!(store.settings("default").unwrap().enabled);
        store
            .save_settings(
                "default",
                &AccountSettings {
                    workspace: None,
                    ..settings.clone()
                },
            )
            .unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(
            text.contains("# keep this") && !text.contains("workspace"),
            "{text}"
        );

        for invalid in [
            "[channels.test.default]\nenabeld = false\n",
            "[channels.test.default]\nremote_tools = \"everyone\"\n",
            "[channels]\ntest = 1\n",
        ] {
            atomic_write(&config, invalid).unwrap();
            assert!(store.settings("default").is_err(), "{invalid}");
        }
        // Parse errors name the line but never quote the file, which holds keys.
        atomic_write(&config, "api_key = \"sk-secret\"\nbroken =\n").unwrap();
        let error = store.settings("default").unwrap_err().to_string();
        assert!(
            error.contains("line 2") && !error.contains("sk-secret"),
            "{error}"
        );
        assert!(store.save_settings("default", &settings).is_err());
        assert!(store.save_settings("../escape", &settings).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            atomic_write(&config, "").unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(store.settings("default").is_err());
        }
    }

    #[test]
    fn files_follow_the_instance_layout_privately() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let store = store(home);
        store.save_account("default", &credentials("a")).unwrap();
        store
            .save_settings("default", &AccountSettings::default())
            .unwrap();
        let _lock = store.lock("default").unwrap();
        for (path, expected) in [
            (home.join("credentials"), 0o700),
            (home.join("credentials/test"), 0o700),
            (home.join("credentials/test/default.json"), 0o600),
            (home.join("state"), 0o700),
            (home.join("state/channels"), 0o700),
            (home.join("state/channels/test"), 0o700),
            (home.join("state/channels/test/default.json"), 0o600),
            (home.join("state/channels/test/default.lock"), 0o600),
            (home.join("state/channels/test/default.transaction"), 0o600),
            (home.join("config.toml"), 0o600),
        ] {
            assert_eq!(mode(&path), expected, "{}", path.display());
        }
    }

    #[test]
    fn discovery_is_bounded_and_ignores_bad_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        assert!(store.account_names().unwrap().is_empty());
        private_directory(directory.path(), &store.credentials).unwrap();
        atomic_write(&store.credentials_path("broken").unwrap(), "not JSON").unwrap();
        atomic_write(&store.credentials.join("invalid.name.json"), "{}").unwrap();
        assert_eq!(store.account_names().unwrap(), vec!["broken"]);
        assert!(store.account("broken").is_err());
        for index in 0..140 {
            atomic_write(
                &store.credentials_path(&format!("account-{index}")).unwrap(),
                "{}",
            )
            .unwrap();
        }
        assert!(store.account_names().is_err());
    }

    #[test]
    fn removal_clears_one_account_and_preserves_others() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let account = credentials("a");
        store.save_account("default", &account).unwrap();
        store.save_account("other", &account).unwrap();
        for name in ["default", "other"] {
            store
                .save_settings(name, &AccountSettings::default())
                .unwrap();
        }
        store
            .save_state("default", &BridgeState::default())
            .unwrap();
        store.remove("default").unwrap();
        assert!(store.account("default").unwrap().is_none());
        assert!(!store.credentials_path("default").unwrap().exists());
        assert!(!store.state_path("default").unwrap().exists());
        let config = std::fs::read_to_string(directory.path().join("config.toml")).unwrap();
        assert!(!config.contains("[channels.test.default]"), "{config}");
        assert!(config.contains("[channels.test.other]"), "{config}");
        assert!(store.account("other").unwrap().unwrap() == account);
        store.remove("other").unwrap();
        let config = std::fs::read_to_string(directory.path().join("config.toml")).unwrap();
        assert!(!config.contains("channels"), "{config}");
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
}
