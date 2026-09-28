//! Email (feature `email`): read-only mail triage reported to a mail chat.
//!
//! An email account runs as the daemon component `email:<account>`. It
//! watches one mailbox through a read-only provider adapter ([`imap`] in
//! this release), decides each new message deterministically first (rules,
//! identity, age, budget) and with one fresh tool-free model turn only when
//! that is worth it, and reports to the owner in digests that only mail
//! chats (`purpose = "mail"`) may carry. Mail text lives only in memory, in
//! that turn, in the bounded state queue, and in the mail chat's outbox.
//!
//! This release takes no action on mail: nothing here can write to a
//! mailbox or send mail, and no code that could is linked.

use anyhow::{Context, Result};
use async_trait::async_trait;
use scv_client::Layout;
use scv_protocol::MailCounts;

use crate::channel::{AccountRun, Channel, ChannelKind};
use crate::state;

pub(crate) mod clean;
mod credentials;
pub(crate) mod imap;
mod janitor;
mod ledger;
mod model;
mod notify;
pub(crate) mod parse;
mod plan;
mod render;
mod rules;
mod settings;
pub(crate) mod source;
#[cfg(test)]
mod test_support;
mod triage;
mod worker;

pub use credentials::{Account, Login};
pub(crate) use janitor::LowSpace;
pub(crate) use settings::MailSettings;

/// The channel's name in commands, component IDs, and paths.
pub const CHANNEL: &str = "email";

/// Mailboxes read by SCV.
pub struct Email;

#[async_trait]
impl Channel for Email {
    const KIND: ChannelKind = ChannelKind::Email;
    type Credentials = Account;
    type Login = Login;

    /// Check the mailbox by signing in and opening its inbox read-only,
    /// then save the credentials. Signing in again with a new password or
    /// authorization code for the same mailbox keeps the account's state.
    async fn login(layout: &Layout, account: &str, login: Login) -> Result<()> {
        state::validate_name(account)?;
        let credentials = Account::Imap {
            host: login.host.trim().to_ascii_lowercase(),
            port: login.port,
            username: login.username.trim().to_owned(),
            password: login.password,
        };
        credentials.validate()?;
        imap::verify(&imap_config(&credentials, "INBOX"))
            .await
            .context("could not sign in to the mailbox")?;
        store(layout).save_account(account, &credentials)
    }

    /// A mailbox has no chat owner: it answers nobody and holds no tools.
    fn owner(_credentials: &Account) -> Option<&str> {
        None
    }

    /// The mail server, never the user name.
    fn bot_id(credentials: &Account) -> Option<String> {
        Some(credentials.host().to_owned())
    }

    fn title(_credentials: &Account) -> &'static str {
        "Email"
    }

    async fn run(run: AccountRun<'_>, credentials: &Account) -> Result<()> {
        run_account(run, credentials).await
    }
}

/// A one-line summary of an email account's `mail` table for display, or
/// why it is invalid. It holds settings only: no mail and no secret.
pub fn describe_settings(mail: Option<&toml::Table>) -> Result<String> {
    let settings = MailSettings::parse(mail)?;
    let budget = if settings.max_tokens_per_day == 0 {
        "no model triage".to_owned()
    } else {
        format!(
            "triage up to {} tokens a day{}",
            settings.max_tokens_per_day,
            if settings.send_body {
                format!(", {} KiB of each body", settings.max_body_kib)
            } else {
                ", headers only".to_owned()
            }
        )
    };
    Ok(format!(
        "watches {:?}, reports to {}; {budget}; {} rules",
        settings.mailbox,
        settings.notify.route.join(", "),
        settings.rules.len()
    ))
}

/// The accounts' state files, with their private working directories under
/// `state/mail/`.
pub(crate) fn store(layout: &Layout) -> ledger::Store {
    ledger::Store::new(layout, CHANNEL).with_private(layout.state().join("mail"))
}

fn imap_config(credentials: &Account, mailbox: &str) -> imap::ImapConfig {
    let Account::Imap {
        host,
        port,
        username,
        password,
    } = credentials;
    imap::ImapConfig {
        host: host.clone(),
        port: *port,
        username: username.clone(),
        password: password.expose().to_owned(),
        mailbox: mailbox.to_owned(),
    }
}

/// The time, and the owner's local offset and day.
pub(crate) trait Clock: Send + Sync {
    /// Unix seconds.
    fn now(&self) -> u64;
    /// Seconds east of UTC at `now`.
    fn offset(&self, now: u64) -> i32;
    /// The local date at `now`, `YYYY-MM-DD`.
    fn today(&self, now: u64) -> String {
        scv_client::history::LocalTime::at(i64::try_from(now).unwrap_or(i64::MAX), self.offset(now))
            .date()
    }
}

/// The host's clock, at a fixed offset when the settings name one and in
/// the host's time zone otherwise.
pub(crate) struct SystemClock {
    pub(crate) fixed: Option<i32>,
}

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    fn offset(&self, now: u64) -> i32 {
        self.fixed
            .unwrap_or_else(|| crate::chatlog::utc_offset(i64::try_from(now).unwrap_or(i64::MAX)))
    }
}

/// How often the account's counts are published for status.
const COUNTS_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Run one email account until it fails; dropping the future stops it.
async fn run_account(run: AccountRun<'_>, credentials: &Account) -> Result<()> {
    state::validate_name(run.account)?;
    credentials.validate()?;
    let settings = MailSettings::parse(run.settings.mail.as_ref())
        .with_context(|| format!("[channels.{CHANNEL}.{}]", run.account))?;
    check_routes(run.layout, &settings.notify.route)?;
    let store = store(run.layout);
    // The ledger keeps the run lock until its last write has landed.
    let run_lock = store.lock(run.account)?;
    let bound = store.bind_state(run.account, |saved| Ok(saved == credentials))?;
    let private = run.layout.mail_state(run.account);
    let cwd = private.join("empty");
    state::private_directory(run.layout.home(), &cwd)?;
    let max_state_bytes =
        usize::try_from(settings.retention.max_state_kib * 1024).unwrap_or(usize::MAX);
    let ledger = ledger::Ledger::open(
        store,
        run_lock,
        run.account,
        bound,
        max_state_bytes,
        settings.notify.max_queue,
    )?;
    let clock = SystemClock {
        fixed: settings.notify.fixed_offset()?,
    };
    let low_space = LowSpace::default();
    let registration = run.link.register_mail(settings.notify.route.clone());
    let hub = registration
        .as_ref()
        .map(|registration| registration.hub().as_ref());
    let janitor = janitor::Janitor {
        ledger: &ledger,
        settings: &settings,
        clock: &clock,
        low_space: &low_space,
        private: &private,
        state_dir: &run.layout.channel_state(CHANNEL),
        account: run.account,
        free_space: janitor::free_bytes,
    };
    // Nothing else runs until the files are known to be in bounds.
    janitor.sweep().await?;
    let worker = worker::Worker {
        ledger: &ledger,
        settings: &settings,
        socket: run.socket,
        cwd: &cwd,
        clock: &clock,
        low_space: &low_space,
        frame: triage::frame(run.account, &settings.instructions),
    };
    let notifier = notify::Notifier {
        ledger: &ledger,
        settings: &settings,
        account: run.account,
        clock: &clock,
        hub,
    };
    let config = imap_config(credentials, &settings.mailbox);
    let connect = || imap::ImapSource::connect(&config);
    let publish = async {
        let Some(registration) = &registration else {
            return std::future::pending().await;
        };
        loop {
            registration.set_counts(counts(&ledger, &settings, &clock));
            tokio::time::sleep(COUNTS_EVERY).await;
        }
    };
    tracing::info!(account = run.account, "mail account starts, read-only");
    tokio::select! {
        result = worker.watch(connect, run.health) => result,
        result = notifier.run() => result,
        result = janitor.run() => result,
        result = publish => result,
    }
}

/// Refuse to start unless every route names a chat account configured as a
/// mail chat. The hub and the bridge enforce this again for every message;
/// this makes a mistake visible at once instead of as mail that never goes.
fn check_routes(layout: &Layout, routes: &[String]) -> Result<()> {
    for route in routes {
        let (channel, account) = route.split_once(':').unwrap_or_default();
        let kind = ChannelKind::parse(channel)
            .ok()
            .filter(|kind| kind.is_chat())
            .with_context(|| format!("mail.notify.route names {route}, which is not a chat"))?;
        let settings = kind.accounts(layout).settings(account)?;
        if settings.purpose != state::Purpose::Mail {
            anyhow::bail!(
                "mail.notify.route names {route}, which is not a mail chat; run `scv channels run \
                 {channel} --account {account} --purpose mail` first"
            );
        }
    }
    Ok(())
}

/// The account's activity as counts, for status and `mail status`.
fn counts(ledger: &ledger::Ledger, settings: &MailSettings, clock: &dyn Clock) -> MailCounts {
    let now = clock.now();
    let today = clock.today(now);
    let snapshot = ledger.snapshot();
    let (_, tokens) = snapshot.spent(&today, now);
    let current = snapshot.day.date == today;
    let today_count = |value: u64| if current { value } else { 0 };
    MailCounts {
        claimed: snapshot.claims.len() as u64,
        queued: snapshot.queue.len() as u64,
        seen_today: today_count(snapshot.day.seen),
        triaged_today: today_count(snapshot.day.triaged),
        reported_today: today_count(snapshot.day.reported),
        tokens_today: tokens,
        token_budget: settings.max_tokens_per_day,
        messages_24h: snapshot
            .log
            .iter()
            .filter(|entry| now.saturating_sub(entry.at) < 86_400)
            .count() as u64,
        last_check_unix_seconds: ledger.last_check(),
    }
}

#[cfg(test)]
mod tests;
