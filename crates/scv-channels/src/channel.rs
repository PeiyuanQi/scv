//! The chat channels this crate carries, and running one account of each.
//!
//! Each channel implements [`Channel`]: signing an account in, what its
//! credentials say about the account, and running it over the shared
//! bridge. The daemon sees channels only through [`ChannelKind`],
//! [`ChannelCredentials`], [`Accounts`], and [`run`], so it never names a
//! platform's own types.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use scv_client::Layout;

use crate::state::{self, AccountSettings};
use crate::{MediaOptions, ToolOwner, hub};

/// A chat platform SCV answers on.
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    const KIND: ChannelKind;
    /// What a signed-in account saves.
    type Credentials: state::Credentials + Send + Sync + 'static;
    /// How an account signs in.
    type Login: Send;

    /// Sign `account` in and save its credentials under `layout`.
    async fn login(layout: &Layout, account: &str, login: Self::Login) -> Result<()>;

    /// The account owner's sender ID, recorded at sign-in: shown in status,
    /// the only sender an owner-only account answers, and the only one
    /// remote tools may reach.
    fn owner(credentials: &Self::Credentials) -> Option<&str>;

    /// The bot's identity shown in status.
    fn bot_id(credentials: &Self::Credentials) -> Option<String>;

    /// The platform's name as the account's users know it.
    fn title(credentials: &Self::Credentials) -> &'static str;

    /// Run one account until it fails; dropping the future stops it.
    async fn run(run: AccountRun<'_>, credentials: &Self::Credentials) -> Result<()>;
}

/// The channels this build carries, by their `scv channels` name.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelKind {
    /// WeChat, through its ClawBot (iLink) bot: `wechat`.
    #[cfg(feature = "wechat")]
    Wechat,
    /// Feishu and Lark, through a bot app: `feishu`.
    #[cfg(feature = "feishu")]
    Feishu,
}

impl ChannelKind {
    /// Every channel, in the order the daemon reconciles and lists them.
    pub const ALL: &'static [Self] = &[
        #[cfg(feature = "wechat")]
        Self::Wechat,
        #[cfg(feature = "feishu")]
        Self::Feishu,
    ];

    /// The channel's name in commands, component IDs, and paths.
    pub fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "wechat")]
            Self::Wechat => crate::wechat::CHANNEL,
            #[cfg(feature = "feishu")]
            Self::Feishu => crate::feishu::CHANNEL,
        }
    }

    /// The channel's name in the daemon's log lines.
    pub fn title(self) -> &'static str {
        match self {
            #[cfg(feature = "wechat")]
            Self::Wechat => "WeChat",
            #[cfg(feature = "feishu")]
            Self::Feishu => "Feishu",
        }
    }

    /// The channel named `name`.
    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.name() == name)
            .ok_or_else(|| anyhow!("Unknown channel {name:?}"))
    }

    /// This channel's accounts in the instance at `layout`.
    pub fn accounts(self, layout: &Layout) -> Accounts {
        match self {
            #[cfg(feature = "wechat")]
            Self::Wechat => Accounts::new(self, crate::wechat::Store::new(layout, self.name())),
            #[cfg(feature = "feishu")]
            Self::Feishu => Accounts::new(self, crate::feishu::Store::new(layout, self.name())),
        }
    }
}

/// A channel account's saved credentials.
#[derive(Clone, PartialEq)]
pub enum ChannelCredentials {
    #[cfg(feature = "wechat")]
    Wechat(crate::wechat::Account),
    #[cfg(feature = "feishu")]
    Feishu(crate::feishu::Account),
}

impl ChannelCredentials {
    /// Which channel these credentials sign in to.
    pub fn kind(&self) -> ChannelKind {
        match *self {
            #[cfg(feature = "wechat")]
            Self::Wechat(_) => ChannelKind::Wechat,
            #[cfg(feature = "feishu")]
            Self::Feishu(_) => ChannelKind::Feishu,
        }
    }

    /// See [`Channel::owner`].
    pub fn owner(&self) -> Option<&str> {
        match *self {
            #[cfg(feature = "wechat")]
            Self::Wechat(ref credentials) => crate::wechat::WeChat::owner(credentials),
            #[cfg(feature = "feishu")]
            Self::Feishu(ref credentials) => crate::feishu::Feishu::owner(credentials),
        }
    }

    /// See [`Channel::bot_id`].
    pub fn bot_id(&self) -> Option<String> {
        match *self {
            #[cfg(feature = "wechat")]
            Self::Wechat(ref credentials) => crate::wechat::WeChat::bot_id(credentials),
            #[cfg(feature = "feishu")]
            Self::Feishu(ref credentials) => crate::feishu::Feishu::bot_id(credentials),
        }
    }

    /// See [`Channel::title`].
    pub fn title(&self) -> &'static str {
        match *self {
            #[cfg(feature = "wechat")]
            Self::Wechat(ref credentials) => crate::wechat::WeChat::title(credentials),
            #[cfg(feature = "feishu")]
            Self::Feishu(ref credentials) => crate::feishu::Feishu::title(credentials),
        }
    }
}

#[cfg(feature = "wechat")]
impl From<crate::wechat::Account> for ChannelCredentials {
    fn from(credentials: crate::wechat::Account) -> Self {
        Self::Wechat(credentials)
    }
}

#[cfg(feature = "feishu")]
impl From<crate::feishu::Account> for ChannelCredentials {
    fn from(credentials: crate::feishu::Account) -> Self {
        Self::Feishu(credentials)
    }
}

/// One channel's saved accounts: credentials under `credentials/<channel>`,
/// settings as `[channels.<channel>.<account>]` in `config.toml`, and
/// delivery state under `state/channels/<channel>` (see [`state::Store`]).
pub struct Accounts {
    kind: ChannelKind,
    store: Box<dyn Stored>,
}

impl Accounts {
    fn new(kind: ChannelKind, store: impl Stored + 'static) -> Self {
        Self {
            kind,
            store: Box::new(store),
        }
    }

    pub fn kind(&self) -> ChannelKind {
        self.kind
    }

    /// Accounts with saved credentials, failing on entry errors or more than
    /// 128 entries. Credentials are validated separately.
    pub fn names(&self) -> Result<Vec<String>> {
        self.store.names()
    }

    /// Accounts `config.toml` has settings for, signed in or not.
    pub fn configured(&self) -> Result<Vec<String>> {
        self.store.configured()
    }

    /// Credentials and settings read together under the account's
    /// transaction lock.
    pub fn snapshot(&self, name: &str) -> Result<(Option<ChannelCredentials>, AccountSettings)> {
        self.store.snapshot(name)
    }

    /// Whether `name` has saved credentials.
    pub fn signed_in(&self, name: &str) -> Result<bool> {
        self.store.signed_in(name)
    }

    pub fn settings(&self, name: &str) -> Result<AccountSettings> {
        self.store.settings(name)
    }

    pub fn save_settings(&self, name: &str, settings: &AccountSettings) -> Result<()> {
        self.store.save_settings(name, settings)
    }

    /// Remove an account's credentials, settings, and delivery state; refuses
    /// while the account runs.
    pub fn remove(&self, name: &str) -> Result<()> {
        self.store.remove(name)
    }

    /// Credentials and settings for display, read without any lock.
    pub fn inspect(
        &self,
        name: &str,
    ) -> (Result<Option<ChannelCredentials>>, Result<AccountSettings>) {
        self.store.inspect(name)
    }

    pub fn credentials_path(&self, name: &str) -> Result<PathBuf> {
        self.store.credentials_path(name)
    }
}

/// A channel's [`state::Store`] with its credential type erased.
trait Stored: Send + Sync {
    fn names(&self) -> Result<Vec<String>>;
    fn configured(&self) -> Result<Vec<String>>;
    fn snapshot(&self, name: &str) -> Result<(Option<ChannelCredentials>, AccountSettings)>;
    fn signed_in(&self, name: &str) -> Result<bool>;
    fn settings(&self, name: &str) -> Result<AccountSettings>;
    fn save_settings(&self, name: &str, settings: &AccountSettings) -> Result<()>;
    fn remove(&self, name: &str) -> Result<()>;
    fn inspect(&self, name: &str) -> (Result<Option<ChannelCredentials>>, Result<AccountSettings>);
    fn credentials_path(&self, name: &str) -> Result<PathBuf>;
}

impl<C> Stored for state::Store<C>
where
    C: state::Credentials + Into<ChannelCredentials> + Send + Sync,
{
    fn names(&self) -> Result<Vec<String>> {
        self.account_names()
    }

    fn configured(&self) -> Result<Vec<String>> {
        self.configured_accounts()
    }

    fn snapshot(&self, name: &str) -> Result<(Option<ChannelCredentials>, AccountSettings)> {
        let (credentials, settings) = self.account_snapshot(name)?;
        Ok((credentials.map(Into::into), settings))
    }

    fn signed_in(&self, name: &str) -> Result<bool> {
        Ok(self.account(name)?.is_some())
    }

    fn settings(&self, name: &str) -> Result<AccountSettings> {
        state::Store::settings(self, name)
    }

    fn save_settings(&self, name: &str, settings: &AccountSettings) -> Result<()> {
        state::Store::save_settings(self, name, settings)
    }

    fn remove(&self, name: &str) -> Result<()> {
        state::Store::remove(self, name)
    }

    fn inspect(&self, name: &str) -> (Result<Option<ChannelCredentials>>, Result<AccountSettings>) {
        let (credentials, settings) = state::Store::inspect(self, name);
        (credentials.map(|found| found.map(Into::into)), settings)
    }

    fn credentials_path(&self, name: &str) -> Result<PathBuf> {
        state::Store::credentials_path(self, name)
    }
}

/// What the daemon hands a channel to run one account.
#[derive(Clone, Copy)]
pub struct AccountRun<'a> {
    /// The instance the account belongs to.
    pub layout: &'a Layout,
    pub account: &'a str,
    pub credentials: &'a ChannelCredentials,
    /// The account's whole `[channels.<channel>.<account>]` table.
    pub settings: &'a AccountSettings,
    /// The account owner's sender ID from its credentials, whether or not it
    /// holds the remote-tool grant; `None` when the sign-in names nobody.
    pub owner: Option<&'a str>,
    /// How long one owner turn may run: `Some` exactly when the owner holds
    /// remote tools.
    pub tool_turn_timeout: Option<Duration>,
    /// Where the account's sessions run.
    pub workspace: &'a Path,
    /// The daemon socket the account's sessions connect to.
    pub socket: &'a Path,
    /// The account's connection to the daemon's hub.
    pub link: &'a hub::Link,
    /// Called with `true` after each authenticated contact with the platform,
    /// and with `false` when contact fails.
    pub health: &'a (dyn Fn(bool) + Send + Sync),
}

impl<'a> AccountRun<'a> {
    /// The bridge's view of this run, for channel `kind`: media under the
    /// instance's media directory, within the account's limits.
    pub(crate) fn bridge(&self, kind: ChannelKind) -> Result<crate::BridgeRun<'a>> {
        state::validate_name(self.account)?;
        Ok(crate::BridgeRun {
            account: self.account,
            workspace: self.workspace,
            socket: self.socket,
            owner: self.owner,
            tool_owner: self
                .owner
                .zip(self.tool_turn_timeout)
                .map(|(user_id, turn_timeout)| ToolOwner {
                    user_id: user_id.to_owned(),
                    turn_timeout,
                }),
            senders: self.settings.senders,
            media: MediaOptions::new(
                self.layout,
                kind.name(),
                self.account,
                self.settings.media.clone(),
            ),
            link: self.link,
            report: self.health,
        })
    }
}

/// Run one account on its channel until it fails; dropping the future stops
/// it, and the caller enforces any stop timeout. It launches no process and
/// spawns no tasks. Only authenticated contact with the platform reports the
/// account healthy, and a failure reports it unhealthy.
pub async fn run(run: AccountRun<'_>) -> Result<()> {
    match *run.credentials {
        #[cfg(feature = "wechat")]
        ChannelCredentials::Wechat(ref credentials) => {
            reported(&run, crate::wechat::WeChat::run(run, credentials).await)
        }
        #[cfg(feature = "feishu")]
        ChannelCredentials::Feishu(ref credentials) => {
            reported(&run, crate::feishu::Feishu::run(run, credentials).await)
        }
    }
}

/// A run's result, reporting the account unhealthy when it failed.
fn reported(run: &AccountRun<'_>, result: Result<()>) -> Result<()> {
    if result.is_err() {
        (run.health)(false);
    }
    result
}

#[cfg(all(test, feature = "wechat", feature = "feishu"))]
mod tests;
