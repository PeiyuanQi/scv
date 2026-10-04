//! Email (feature `email`): mail triage reported to a mail chat, and the
//! mail actions the owner approves there.
//!
//! An email account runs as the daemon component `email:<account>`. It
//! watches one mailbox through a read-only provider adapter ([`imap`], the
//! Gmail API, or Microsoft Graph), decides each new message
//! deterministically first (rules, identity, age, budget) and with one
//! fresh tool-free model turn only when that is worth it, and reports to the
//! owner in digests that only mail chats (`purpose = "mail"`) may carry.
//! Mail text lives only in memory, in that turn, in the bounded state queue,
//! in a pending action's content file, and in the mail chat's outbox.
//!
//! Without a `mail.actions` table that turns some kind on, the account only
//! reads: no writing credential is loaded and no executor runs. With one,
//! the owner can have SCV draft, send, forward, archive, mark read, or move
//! mail to Trash or Spam, each action only after the owner approves it with
//! its own code in the mail chat ([`ledger::actions`]); only the executor
//! ([`executor`], through [`effects`]) writes to a mailbox or sends mail.

use anyhow::{Context, Result};
use async_trait::async_trait;
use scv_client::Layout;
use scv_protocol::MailCounts;
use std::sync::{Arc, Mutex};

use crate::channel::{AccountRun, Channel, ChannelKind};
use crate::state::{self, Credentials as _};

mod api;
mod audit;
mod authority;
pub(crate) mod clean;
mod compose;
pub(crate) mod content;
mod credentials;
mod effects;
mod executor;
mod gmail;
mod graph;
pub(crate) mod imap;
mod janitor;
mod ledger;
mod message;
mod model;
mod notify;
pub mod oauth;
pub(crate) mod parse;
mod plan;
mod prepare;
mod preview;
mod provider;
mod render;
mod rules;
mod settings;
mod smtp;
pub(crate) mod source;
#[cfg(test)]
mod test_support;
mod triage;
mod worker;

pub use credentials::{Account, Login, Smtp, SmtpSecurity};
pub(crate) use janitor::LowSpace;
pub(crate) use settings::MailSettings;

/// The channel's name in commands, component IDs, and paths.
pub const CHANNEL: &str = "email";

/// How long an email account with mail actions may take to stop: its
/// action under way runs to its end (within [`executor::ACTION_BUDGET`]).
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(65);

/// Mailboxes read by SCV.
pub struct Email;

#[async_trait]
impl Channel for Email {
    const KIND: ChannelKind = ChannelKind::Email;
    type Credentials = Account;
    type Login = Login;

    /// Check the mailbox by signing in and opening its inbox read-only (for
    /// IMAP, and its SMTP server when given), or by OAuth for each grant
    /// asked for, then save the credentials. Signing in again with a new
    /// password or authorization code for the same mailbox keeps the
    /// account's state.
    async fn login(layout: &Layout, account: &str, login: Login) -> Result<()> {
        state::validate_name(account)?;
        match login {
            Login::Imap {
                host,
                port,
                username,
                password,
                address,
                smtp,
            } => {
                let credentials = Account::Imap {
                    host: host.trim().to_ascii_lowercase(),
                    port,
                    username: username.trim().to_owned(),
                    password,
                    address: address.map(|address| address.trim().to_owned()),
                    smtp: smtp.map(|smtp| Smtp {
                        host: smtp.host.trim().to_ascii_lowercase(),
                        ..smtp
                    }),
                };
                credentials.validate()?;
                let config = provider::imap_config(&credentials, "INBOX")
                    .context("an IMAP account has an IMAP configuration")?;
                imap::verify(&config)
                    .await
                    .context("could not sign in to the mailbox")?;
                if let Account::Imap {
                    smtp: Some(smtp),
                    username,
                    password,
                    ..
                } = &credentials
                {
                    smtp::verify(&smtp::SmtpConfig {
                        host: smtp.host.clone(),
                        port: smtp.port,
                        security: smtp.security,
                        username: username.clone(),
                        password: password.expose().to_owned(),
                    })
                    .await?;
                }
                store(layout).save_account(account, &credentials)
            }
            Login::OAuth(request) => {
                let endpoints = oauth::Endpoints::for_provider(request.provider, &request.tenant);
                let origins = api::Origins::default();
                login_oauth(layout, account, &request, &endpoints, &origins).await
            }
        }
    }

    /// A mailbox has no chat owner: it answers nobody and holds no tools.
    fn owner(_credentials: &Account) -> Option<&str> {
        None
    }

    /// The mail server or API, never the user name or address.
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

/// Sign in with OAuth for the grants `request` asks for, learn the
/// mailbox's address with the reader's token, and save the grants, then the
/// credentials.
async fn login_oauth(
    layout: &Layout,
    account: &str,
    request: &oauth::Request,
    endpoints: &oauth::Endpoints,
    origins: &api::Origins,
) -> Result<()> {
    let placeholder = match request.provider {
        oauth::OAuthProvider::Gmail => Account::Gmail {
            address: "owner@example.com".into(),
            client_id: request.client_id.trim().to_owned(),
            client_secret: request.client_secret.clone(),
        },
        oauth::OAuthProvider::Graph => Account::Graph {
            address: "owner@example.com".into(),
            client_id: request.client_id.trim().to_owned(),
            tenant: request.tenant.trim().to_owned(),
        },
    };
    // The client ID and tenant are checked before the browser is involved.
    placeholder.validate()?;
    let signed = oauth::sign_in(request, endpoints).await?;
    let address = whoami(request.provider, origins, &signed.reader_token).await?;
    let credentials = match placeholder {
        Account::Gmail {
            client_id,
            client_secret,
            ..
        } => Account::Gmail {
            address,
            client_id,
            client_secret,
        },
        Account::Graph {
            client_id, tenant, ..
        } => Account::Graph {
            address,
            client_id,
            tenant,
        },
        Account::Imap { .. } => unreachable!("an OAuth sign-in"),
    };
    credentials.validate()?;
    let store = store(layout);
    let grants = credentials::Grants::path(layout, account)?;
    {
        let _transaction = store.transaction(account)?;
        signed.grants.save(&grants)?;
    }
    store.save_account(account, &credentials)
}

/// The signed-in mailbox's own address, asked of the API.
async fn whoami(
    provider: oauth::OAuthProvider,
    origins: &api::Origins,
    token: &scv_client::Secret,
) -> Result<String> {
    let (url, fields): (String, &[&str]) = match provider {
        oauth::OAuthProvider::Gmail => (
            format!("{}/gmail/v1/users/me/profile", origins.gmail),
            &["emailAddress"],
        ),
        oauth::OAuthProvider::Graph => (
            format!("{}/v1.0/me", origins.graph),
            &["mail", "userPrincipalName"],
        ),
    };
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = http
        .get(url)
        .bearer_auth(token.expose())
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("could not reach the mail API to learn the address"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "the mail API refused to say whose mailbox this is ({})",
            response.status().as_u16()
        );
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("the mail API's answer was unreadable"))?;
    fields
        .iter()
        .find_map(|field| body[*field].as_str().and_then(render::valid_address))
        .context("the mail API did not name the mailbox's address")
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
    let actions = match settings.actions.as_ref().filter(|actions| actions.any()) {
        None => "read-only".to_owned(),
        Some(actions) => {
            use settings::ActionMode::Approve;
            let on: Vec<&str> = [
                ("draft", actions.draft),
                ("send", actions.send),
                ("forward", actions.forward),
                ("archive", actions.archive),
                ("mark_read", actions.mark_read),
                ("trash", actions.trash),
                ("spam", actions.spam),
            ]
            .into_iter()
            .filter(|(_, mode)| *mode == Approve)
            .map(|(name, _)| name)
            .collect();
            format!("actions on approval: {}", on.join(", "))
        }
    };
    Ok(format!(
        "watches {:?}, reports to {}; {budget}; {} rules; {actions}",
        settings.mailbox,
        settings.notify.route.join(", "),
        settings.rules.len()
    ))
}

/// The accounts' state files, with their private working directories under
/// `state/mail/`, and an OAuth account's grants file beside its credentials.
pub(crate) fn store(layout: &Layout) -> ledger::Store {
    let grants = layout.channel_credentials(CHANNEL);
    ledger::Store::new(layout, CHANNEL)
        .with_private(layout.state().join("mail"))
        .with_sidecar(move |account| grants.join(format!("{account}.grants")))
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

/// An OAuth account's token parts; `None` for IMAP.
fn oauth_parts(
    layout: &Layout,
    account: &str,
    credentials: &Account,
) -> Result<Option<provider::OAuthParts>> {
    let (provider, client_id, client_secret, tenant) = match credentials {
        Account::Imap { .. } => return Ok(None),
        Account::Gmail {
            client_id,
            client_secret,
            ..
        } => (
            oauth::OAuthProvider::Gmail,
            client_id.clone(),
            client_secret.clone(),
            String::new(),
        ),
        Account::Graph {
            client_id, tenant, ..
        } => (
            oauth::OAuthProvider::Graph,
            client_id.clone(),
            None,
            tenant.clone(),
        ),
    };
    let store = Arc::new(store(layout));
    let name = account.to_owned();
    Ok(Some(provider::OAuthParts {
        provider,
        endpoints: oauth::Endpoints::for_provider(provider, &tenant),
        client_id,
        client_secret,
        grants: credentials::Grants::path(layout, account)?,
        lock: Arc::new(move || store.transaction(&name)),
    }))
}

/// Run one email account until it fails, or until `run.stop` asks it to
/// stop: then an action under way finishes first. A worker, notifier, or
/// janitor that returns does not drop the executor; it is asked to stop
/// and awaited within [`STOP_GRACE`]. Dropping this future still stops
/// both at once.
async fn run_account(run: AccountRun<'_>, credentials: &Account) -> Result<()> {
    state::validate_name(run.account)?;
    credentials.validate()?;
    let settings = MailSettings::parse(run.settings.mail.as_ref())
        .with_context(|| format!("[channels.{CHANNEL}.{}]", run.account))?;
    check_routes(run.layout, &settings.notify.route)?;
    let adapter = provider::provider(
        credentials,
        &settings,
        oauth_parts(run.layout, run.account, credentials)?,
        &api::Origins::default(),
    )?;
    let store = store(run.layout);
    // The ledger keeps the run lock until its last write has landed.
    let run_lock = store.lock(run.account)?;
    let bound = store.bind_state(run.account, |saved| Ok(saved == credentials))?;
    let private = run.layout.mail_state(run.account);
    let cwd = private.join("empty");
    state::private_directory(run.layout.home(), &cwd)?;
    let max_state_bytes =
        usize::try_from(settings.retention.max_state_kib * 1024).unwrap_or(usize::MAX);
    let mut ledger = ledger::Ledger::open(
        store,
        run_lock,
        run.account,
        bound,
        max_state_bytes,
        settings.notify.max_queue,
    )?;
    let actions_on = settings.actions.as_ref().filter(|actions| actions.any());
    if let Some(actions) = actions_on {
        let directory = private.join("actions");
        state::private_directory(run.layout.home(), &directory)?;
        ledger = ledger.with_actions(ledger::actions::Policy {
            actions: actions.clone(),
            routes: settings.notify.route.clone(),
            fingerprint: credentials.fingerprint()?,
            account: run.account.to_owned(),
            audit: private.join("audit.jsonl"),
            content: content::ContentStore::new(directory),
            tombstone_seconds: settings.retention.tombstone_days * 86_400,
            unknown_keep_seconds: settings.retention.unknown_keep_days * 86_400,
            possible: Mutex::new(ledger::actions::ALL_KINDS.to_vec()),
        });
    }
    let clock = SystemClock {
        fixed: settings.notify.fixed_offset()?,
    };
    let low_space = LowSpace::default();
    let registration = run.link.register_mail(settings.notify.route.clone());
    // Recovery runs before anything reads the mailbox, answers a command,
    // or cleans up: interrupted actions are checked first, and every code
    // and handle is known to the hub again.
    let recovered = ledger.recover(clock.now()).await?;
    if let Some(registration) = &registration {
        for code in &recovered.codes {
            if !registration.claim_code(code) {
                tracing::warn!("another running mail account holds one of this account's codes");
            }
        }
        for handle in &recovered.handles {
            registration.claim_handle(handle);
        }
    }
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
        registration: registration.as_ref(),
    };
    // Nothing else runs until the files are known to be in bounds.
    janitor.sweep().await?;
    let policy = ledger.actions_policy();
    let actions = policy.map(|policy| prepare::Actions {
        registration: registration.as_ref(),
        provider: adapter.kind,
        own: credentials.own_address(),
        frame: compose::frame(run.account, &policy.actions.reply_instructions),
        folders: Mutex::new(source::Folders::default()),
        can_send: adapter.can_send,
    });
    let options = match (&actions, policy) {
        (Some(_), Some(policy)) => {
            use content::{ActionKind, Form};
            triage::Options {
                reply_hint: policy.allows(ActionKind::Draft, Some(Form::Reply))
                    || policy.allows(ActionKind::Send, Some(Form::Reply)),
                moves: policy.actions.propose.clone(),
            }
        }
        _ => triage::Options::default(),
    };
    let worker = worker::Worker {
        ledger: &ledger,
        settings: &settings,
        socket: run.socket,
        cwd: &cwd,
        clock: &clock,
        low_space: &low_space,
        frame: triage::frame_with(run.account, &settings.instructions, &options),
        options,
        actions: actions.as_ref(),
    };
    let notifier = notify::Notifier {
        ledger: &ledger,
        settings: &settings,
        account: run.account,
        clock: &clock,
        hub,
        previews: actions.as_ref().zip(policy),
    };
    notifier.preview_again(recovered.unpreviewed).await?;
    let component = format!("{CHANNEL}:{}", run.account);
    let world = registration.as_ref().map(authority::HubWorld);
    let serve = async {
        let (Some(registration), Some(world), Some(_)) = (&registration, &world, policy) else {
            return std::future::pending().await;
        };
        let (commands, requests) = tokio::sync::mpsc::channel(16);
        registration.serve(commands);
        authority::Authority {
            ledger: &ledger,
            world,
            component: &component,
            clock: &clock,
        }
        .serve(requests)
        .await
    };
    let connect = || (adapter.connect)();
    let publish = async {
        let Some(registration) = &registration else {
            return std::future::pending().await;
        };
        loop {
            registration.set_counts(counts(&ledger, &settings, &clock, adapter.kind));
            tokio::time::sleep(COUNTS_EVERY).await;
        }
    };
    tracing::info!(
        account = run.account,
        provider = ?adapter.kind,
        actions = actions_on.is_some(),
        "mail account starts"
    );
    let others = async {
        tokio::select! {
            result = worker.watch(connect, run.health) => result,
            result = notifier.run() => result,
            result = janitor.run() => result,
            result = publish => result,
            result = serve => result,
        }
    };
    let sibling_stop = tokio_util::sync::CancellationToken::new();
    let stop_executor = sibling_stop.clone();
    let account_stop = run.stop;
    let drain = async {
        let stop = async {
            tokio::select! {
                () = stop_executor.cancelled() => {}
                () = account_stop.cancelled() => {}
            }
        };
        let (Some(effects), Some(policy)) = (&adapter.effects, policy) else {
            // A read-only account has nothing to finish.
            stop.await;
            return Ok(());
        };
        executor::Executor {
            ledger: &ledger,
            content: &policy.content,
            effects: effects.as_ref(),
            registration: registration.as_ref(),
            clock: &clock,
        }
        .run(stop)
        .await
    };
    // The executor is not in the select above: a sibling that returns asks
    // it to stop and waits out [`STOP_GRACE`], so the action under way is
    // not dropped. When the executor returns, the others stop with it.
    let result = beside_executor(others, drain, &sibling_stop, STOP_GRACE).await;
    tracing::info!(account = run.account, "mail account stops");
    result
}

/// Run `others` beside `executor`. When `others` finishes first, `executor`
/// is asked to stop through `stop_executor` and awaited for `bound`. When
/// `executor` finishes first, `others` is dropped. Dropping this future
/// drops both.
async fn beside_executor(
    others: impl std::future::Future<Output = Result<()>>,
    executor: impl std::future::Future<Output = Result<()>>,
    stop_executor: &tokio_util::sync::CancellationToken,
    bound: std::time::Duration,
) -> Result<()> {
    tokio::pin!(others);
    tokio::pin!(executor);
    tokio::select! {
        result = &mut others => {
            stop_executor.cancel();
            match tokio::time::timeout(bound, &mut executor).await {
                Ok(drained) => {
                    result?;
                    drained
                }
                Err(_elapsed) => match result {
                    Err(error) => Err(error).context(
                        "a mail task stopped and the action under way did not finish in time",
                    ),
                    Ok(()) => anyhow::bail!(
                        "a mail task stopped and the action under way did not finish in time"
                    ),
                },
            }
        }
        result = &mut executor => result,
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
fn counts(
    ledger: &ledger::Ledger,
    settings: &MailSettings,
    clock: &dyn Clock,
    provider: source::ProviderKind,
) -> MailCounts {
    use ledger::ActionState;
    let now = clock.now();
    let today = clock.today(now);
    let snapshot = ledger.snapshot();
    let (_, tokens) = snapshot.spent(&today, now);
    let current = snapshot.day.date == today;
    let today_count = |value: u64| if current { value } else { 0 };
    let in_state = |states: &[ActionState]| {
        snapshot
            .actions
            .iter()
            .filter(|entry| states.contains(&entry.state))
            .count() as u64
    };
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
        provider: Some(
            match provider {
                source::ProviderKind::Imap => "imap",
                source::ProviderKind::Gmail => "gmail",
                source::ProviderKind::Graph => "graph",
            }
            .to_owned(),
        ),
        actions: ledger
            .actions_policy()
            .map(|policy| {
                policy
                    .offered()
                    .into_iter()
                    .map(|kind| kind.name().to_owned())
                    .collect()
            })
            .unwrap_or_default(),
        actions_open: in_state(&[
            ActionState::Proposed,
            ActionState::Previewing,
            ActionState::Open,
            ActionState::Approved,
        ]),
        actions_executing: in_state(&[ActionState::Executing]),
        actions_unknown: in_state(&[ActionState::Unknown]),
        actions_done_24h: snapshot
            .actions
            .iter()
            .filter(|entry| {
                entry.state == ActionState::Done
                    && entry
                        .terminal_at
                        .is_some_and(|at| now.saturating_sub(at) < 86_400)
            })
            .count() as u64,
    }
}

#[cfg(test)]
mod tests;
