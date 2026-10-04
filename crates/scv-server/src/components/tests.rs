//! Unit tests for `src/components.rs`.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Wait for `condition`, polling, rather than for a fixed time that a busy
/// machine may overrun.
async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition was not met in time");
}

struct Fake {
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    fail_first: bool,
}
#[async_trait]
impl Component for Fake {
    async fn run(&self, cancellation: CancellationToken, health: HealthReporter) -> Result<()> {
        let attempt = self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_first && attempt == 0 {
            bail!("secret error must never enter status");
        }
        health.contact(true);
        cancellation.cancelled().await;
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn starts_once_recovers_reports_contact_and_joins_before_restoration() {
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let fake = Arc::new(Fake {
        starts: starts.clone(),
        stops: stops.clone(),
        fail_first: true,
    });
    let mut supervisor = Supervisor {
        initial_backoff: Duration::from_millis(10),
        ..Supervisor::default()
    };
    supervisor.start(
        fake.clone(),
        initial_health(ChannelKind::Wechat, "test", None, true),
    );
    supervisor.start(
        fake.clone(),
        initial_health(ChannelKind::Wechat, "test", None, true),
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if supervisor.health()[0].state == ComponentState::Connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let health = &supervisor.health()[0];
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(health.restarts, 1);
    assert!(health.last_success_unix_seconds.is_some());
    assert!(health.error.is_none());
    supervisor.shutdown().await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    supervisor.start(
        fake,
        initial_health(ChannelKind::Wechat, "test", None, true),
    );
    wait_until(|| starts.load(Ordering::SeqCst) == 3).await;
    supervisor.shutdown().await;
    assert_eq!(starts.load(Ordering::SeqCst), 3);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[test]
fn credentials_are_not_connection_evidence() {
    let health = initial_health(ChannelKind::Wechat, "saved", None, true);
    assert_eq!(health.state, ComponentState::Starting);
    assert_eq!(health.last_success_unix_seconds, None);
}

#[tokio::test]
async fn busy_account_snapshot_preserves_live_work_but_invalid_settings_stop_it() {
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let mut components = Components::new(
        crate::test_support::test_instance("/unused"),
        PathBuf::from("/"),
    );
    components.supervisor.start(
        Arc::new(Fake {
            starts: starts.clone(),
            stops: stops.clone(),
            fail_first: false,
        }),
        initial_health(ChannelKind::Wechat, "test", None, true),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while starts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    components
        .account_error(
            ChannelKind::Wechat,
            "test",
            std::io::Error::from(std::io::ErrorKind::WouldBlock).into(),
        )
        .await;
    assert_eq!(
        components.status().components[0].state,
        ComponentState::Connected
    );
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    components
        .account_error(
            ChannelKind::Wechat,
            "test",
            anyhow::anyhow!("invalid settings"),
        )
        .await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        components.status().components[0].state,
        ComponentState::Failed
    );
}

struct Stubborn;
#[async_trait]
impl Component for Stubborn {
    async fn run(&self, _: CancellationToken, _: HealthReporter) -> Result<()> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn bounded_stop_aborts_uncooperative_component_and_cancels_backoff() {
    let mut supervisor = Supervisor {
        grace: Duration::from_millis(20),
        ..Supervisor::default()
    };
    supervisor.start(
        Arc::new(Stubborn),
        initial_health(ChannelKind::Wechat, "stubborn", None, true),
    );
    tokio::task::yield_now().await;
    tokio::time::timeout(Duration::from_secs(1), supervisor.shutdown())
        .await
        .unwrap();
    assert!(supervisor.health().is_empty());
    let fake = Arc::new(Fake {
        starts: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        fail_first: true,
    });
    supervisor.start(
        fake,
        initial_health(ChannelKind::Wechat, "backoff", None, true),
    );
    // The first run fails at once; the one-second backoff that follows is
    // long enough to observe.
    wait_until(|| supervisor.health()[0].state == ComponentState::Backoff).await;
    assert_eq!(
        supervisor.health()[0].error.as_deref(),
        Some("Component stopped unexpectedly; retrying")
    );
    tokio::time::timeout(Duration::from_millis(100), supervisor.shutdown())
        .await
        .unwrap();
}

#[test]
fn remote_tools_require_owner_mode_and_known_owner() {
    let wechat = |user_id: Option<&str>| {
        ChannelCredentials::Wechat(scv_channels::wechat::Account {
            token: "token".into(),
            base_url: "https://example.invalid".into(),
            bot_id: Some("bot".into()),
            user_id: user_id.map(Into::into),
        })
    };
    let feishu = |owner: Option<&str>| {
        ChannelCredentials::Feishu(scv_channels::feishu::Account {
            app_id: "cli_a1b2".into(),
            app_secret: "secret".into(),
            brand: scv_channels::feishu::Brand::Feishu,
            owner_open_id: owner.map(Into::into),
        })
    };
    let owner = AccountSettings {
        remote_tools: RemoteTools::Owner,
        ..Default::default()
    };
    assert_eq!(
        tool_owner(&wechat(Some("owner@im.wechat")), &owner).as_deref(),
        Some("owner@im.wechat")
    );
    assert_eq!(tool_owner(&wechat(None), &owner), None);
    assert_eq!(tool_owner(&wechat(Some("")), &owner), None);
    assert_eq!(
        tool_owner(
            &wechat(Some("owner@im.wechat")),
            &AccountSettings::default()
        ),
        None
    );
    assert_eq!(
        tool_owner(&feishu(Some("ou_owner")), &owner).as_deref(),
        Some("ou_owner")
    );
    assert_eq!(tool_owner(&feishu(None), &owner), None);
    assert_eq!(
        tool_owner(&feishu(Some("ou_owner")), &AccountSettings::default()),
        None
    );
}

#[test]
fn channels_are_named_and_health_shows_the_app_and_owner() {
    assert_eq!(ChannelKind::parse("wechat").unwrap(), ChannelKind::Wechat);
    assert_eq!(ChannelKind::parse("feishu").unwrap(), ChannelKind::Feishu);
    assert!(ChannelKind::parse("lark").is_err());
    let credentials = ChannelCredentials::Feishu(scv_channels::feishu::Account {
        app_id: "cli_a1b2".into(),
        app_secret: "secret".into(),
        brand: scv_channels::feishu::Brand::Feishu,
        owner_open_id: Some("ou_owner".into()),
    });
    let health = initial_health(ChannelKind::Feishu, "default", Some(&credentials), true);
    assert_eq!(health.id, "feishu:default");
    assert_eq!(health.channel, "feishu");
    assert_eq!(health.bot_id.as_deref(), Some("cli_a1b2"));
    assert_eq!(health.user_id.as_deref(), Some("ou_owner"));
    assert!(!serde_json::to_string(&health).unwrap().contains("secret"));
}

#[tokio::test]
async fn a_cancelled_channel_account_stops_before_touching_anything() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    // Everything here is invalid, so any work at all would fail the run.
    let account = ChannelAccount {
        kind: ChannelKind::Wechat,
        account: "../invalid".into(),
        credentials: ChannelCredentials::Wechat(scv_channels::wechat::Account {
            token: "token".into(),
            base_url: "invalid".into(),
            bot_id: None,
            user_id: None,
        }),
        settings: AccountSettings::default(),
        workspace: "/".into(),
        instance: crate::test_support::test_instance("/missing"),
        tools: false,
        link: scv_channels::hub::Link::detached(),
    };
    let health = HealthReporter(Arc::new(Mutex::new(initial_health(
        ChannelKind::Wechat,
        "invalid",
        None,
        true,
    ))));
    account.run(cancellation, health.clone()).await.unwrap();
    assert_eq!(health.snapshot().state, ComponentState::Starting);
}

#[test]
fn a_mailbox_and_a_mail_chat_never_take_tools_or_other_senders() {
    use scv_protocol::Purpose;
    let settings = |remote_tools, senders, purpose| AccountSettings {
        remote_tools,
        senders,
        purpose,
        ..Default::default()
    };
    let chat = settings(RemoteTools::Owner, Senders::Anyone, Purpose::Chat);
    assert_eq!(refusal(ChannelKind::Feishu, &chat), None);
    for (remote_tools, senders) in [
        (RemoteTools::Owner, Senders::Owner),
        (RemoteTools::None, Senders::Anyone),
    ] {
        assert!(
            refusal(
                ChannelKind::Feishu,
                &settings(remote_tools, senders, Purpose::Mail)
            )
            .is_some()
        );
        assert!(
            refusal(
                ChannelKind::Email,
                &settings(remote_tools, senders, Purpose::Chat)
            )
            .is_some()
        );
    }
    let quiet = settings(RemoteTools::None, Senders::Owner, Purpose::Mail);
    assert_eq!(refusal(ChannelKind::Feishu, &quiet), None);
    assert!(refusal(ChannelKind::Email, &quiet).is_some());
    assert_eq!(
        refusal(ChannelKind::Email, &AccountSettings::default()),
        None
    );
}

#[tokio::test]
async fn status_shows_an_email_accounts_counts_and_nothing_else() {
    let hub = scv_channels::hub::Hub::new(None);
    let components = Components::with_hub(
        crate::test_support::test_instance("/unused"),
        std::path::PathBuf::from("/"),
        Arc::clone(&hub),
    )
    .unwrap();
    let registration = hub.register_mail("email:default", vec!["feishu:mail".into()]);
    registration.set_counts(scv_protocol::MailCounts {
        queued: 3,
        ..Default::default()
    });
    assert_eq!(hub.mail_counts("email:default").unwrap().queued, 3);
    drop(registration);
    assert_eq!(hub.mail_counts("email:default"), None);
    assert!(components.status().components.is_empty());
}

#[tokio::test]
async fn creating_a_project_starts_the_supervised_orchestrator() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut components = Components::new(
        crate::test_support::test_instance(home.path()),
        workspace.path().to_path_buf(),
    );
    let status = components
        .control(DaemonCommand::ProjectCreate {
            name: "orchestrated".into(),
            workspace: workspace.path().display().to_string(),
        })
        .await
        .unwrap();
    assert!(
        status
            .components
            .iter()
            .any(|component| component.id == "project:orchestrator")
    );
    components.shutdown().await;
}

#[test]
fn slack_contact_failure_has_actionable_status_and_success_clears_it() {
    let health = super::initial_health(scv_channels::ChannelKind::Slack, "test", None, true);
    let reporter = super::HealthReporter(std::sync::Arc::new(std::sync::Mutex::new(health)));
    reporter.contact(false);
    let snapshot = reporter.snapshot();
    assert_eq!(snapshot.state, scv_protocol::ComponentState::Disconnected);
    assert!(snapshot.error.unwrap().contains("Socket Mode is on"));
    reporter.contact(true);
    assert!(reporter.snapshot().error.is_none());
}

struct TimedStop {
    grace: Option<Duration>,
    /// How long the run takes to return after it is cancelled.
    linger: Duration,
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

#[async_trait]
impl Component for TimedStop {
    async fn run(&self, cancellation: CancellationToken, _: HealthReporter) -> Result<()> {
        self.started.store(true, Ordering::SeqCst);
        cancellation.cancelled().await;
        tokio::time::sleep(self.linger).await;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop_grace(&self) -> Option<Duration> {
        self.grace
    }
}

/// Start `component` and stop it once `started` is set. The supervisor's own
/// grace is `supervisor_grace`; the component may override it.
async fn stop_once_started(
    component: Arc<dyn Component>,
    started: &AtomicBool,
    id: &str,
    supervisor_grace: Duration,
) {
    let mut supervisor = Supervisor {
        grace: supervisor_grace,
        ..Supervisor::default()
    };
    supervisor.start(
        component,
        initial_health(ChannelKind::Wechat, id, None, true),
    );
    wait_until(|| started.load(Ordering::SeqCst)).await;
    tokio::time::timeout(Duration::from_secs(2), supervisor.shutdown())
        .await
        .expect("stop finished within the grace under test");
}

#[tokio::test]
async fn a_component_is_stopped_with_its_own_grace_or_the_supervisor_default() {
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    stop_once_started(
        Arc::new(TimedStop {
            // Longer than the supervisor's 40ms, and longer than the linger.
            grace: Some(Duration::from_secs(2)),
            linger: Duration::from_millis(150),
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
        }),
        &started,
        "own-grace",
        Duration::from_millis(40),
    )
    .await;
    assert!(
        finished.load(Ordering::SeqCst),
        "a run that returns within its own grace is joined, not aborted"
    );

    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    stop_once_started(
        Arc::new(TimedStop {
            grace: None,
            linger: Duration::from_secs(30),
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
        }),
        &started,
        "default-grace",
        Duration::from_millis(40),
    )
    .await;
    assert!(
        !finished.load(Ordering::SeqCst),
        "with no grace of its own, the supervisor's grace aborts a run that outlasts it"
    );

    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    stop_once_started(
        Arc::new(TimedStop {
            grace: Some(Duration::from_millis(40)),
            linger: Duration::from_secs(30),
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
        }),
        &started,
        "short-grace",
        Duration::from_secs(30),
    )
    .await;
    assert!(
        !finished.load(Ordering::SeqCst),
        "a run that outlasts its own grace is aborted even when the supervisor default is longer"
    );
    assert_eq!(ChannelKind::Wechat.stop_grace(), None);
    assert_eq!(
        ChannelKind::Email.stop_grace(),
        Some(scv_channels::email::STOP_GRACE)
    );
}

#[tokio::test]
async fn a_cancelled_email_account_is_not_dropped_before_its_own_run_returns() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let account = ChannelAccount {
        kind: ChannelKind::Email,
        account: "../invalid".into(),
        credentials: ChannelCredentials::Email(scv_channels::email::Account::Imap {
            host: "example.invalid".into(),
            port: 993,
            username: "user".into(),
            password: "secret".into(),
            address: None,
            smtp: None,
        }),
        settings: AccountSettings::default(),
        workspace: "/".into(),
        instance: crate::test_support::test_instance("/missing"),
        tools: false,
        link: scv_channels::hub::Link::detached(),
    };
    assert_eq!(
        account.stop_grace(),
        Some(scv_channels::email::STOP_GRACE),
        "an email account asks for long enough to finish an action"
    );
    let health = HealthReporter(Arc::new(Mutex::new(initial_health(
        ChannelKind::Email,
        "invalid",
        None,
        true,
    ))));
    let error = account
        .run(cancellation, health.clone())
        .await
        .expect_err("an invalid account name fails inside the email run");
    let message = format!("{error:#}");
    assert!(
        message.contains("invalid channel account name"),
        "cancellation must not skip the email run: {message}"
    );
    assert!(
        !message.contains("secret"),
        "the failure must not carry the mailbox secret: {message}"
    );
    assert_eq!(health.snapshot().state, ComponentState::Disconnected);
}
