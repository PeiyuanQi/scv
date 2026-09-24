//! Unit tests for `src/components.rs`.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        initial_health(Channel::Wechat, "test", None, true),
    );
    supervisor.start(
        fake.clone(),
        initial_health(Channel::Wechat, "test", None, true),
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
    supervisor.start(fake, initial_health(Channel::Wechat, "test", None, true));
    wait_until(|| starts.load(Ordering::SeqCst) == 3).await;
    supervisor.shutdown().await;
    assert_eq!(starts.load(Ordering::SeqCst), 3);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[test]
fn credentials_are_not_connection_evidence() {
    let health = initial_health(Channel::Wechat, "saved", None, true);
    assert_eq!(health.state, ComponentState::Starting);
    assert_eq!(health.last_success_unix_seconds, None);
}

#[tokio::test]
async fn busy_account_snapshot_preserves_live_work_but_invalid_settings_stop_it() {
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let mut components = Components::new(PathBuf::from("/unused.sock"), PathBuf::from("/"));
    components.supervisor.start(
        Arc::new(Fake {
            starts: starts.clone(),
            stops: stops.clone(),
            fail_first: false,
        }),
        initial_health(Channel::Wechat, "test", None, true),
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
            Channel::Wechat,
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
        .account_error(Channel::Wechat, "test", anyhow::anyhow!("invalid settings"))
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
        initial_health(Channel::Wechat, "stubborn", None, true),
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
    supervisor.start(fake, initial_health(Channel::Wechat, "backoff", None, true));
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
        Credentials::Wechat(scv_clawbot::state::Account {
            token: "token".into(),
            base_url: "https://example.invalid".into(),
            bot_id: Some("bot".into()),
            user_id: user_id.map(Into::into),
        })
    };
    let feishu = |owner: Option<&str>| {
        Credentials::Feishu(scv_feishu::state::Account {
            app_id: "cli_a1b2".into(),
            app_secret: "secret".into(),
            brand: scv_feishu::state::Brand::Feishu,
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
    assert_eq!(Channel::parse("wechat").unwrap(), Channel::Wechat);
    assert_eq!(Channel::parse("feishu").unwrap(), Channel::Feishu);
    assert!(Channel::parse("lark").is_err());
    let credentials = Credentials::Feishu(scv_feishu::state::Account {
        app_id: "cli_a1b2".into(),
        app_secret: "secret".into(),
        brand: scv_feishu::state::Brand::Feishu,
        owner_open_id: Some("ou_owner".into()),
    });
    let health = initial_health(Channel::Feishu, "default", Some(&credentials), true);
    assert_eq!(health.id, "feishu:default");
    assert_eq!(health.channel, "feishu");
    assert_eq!(health.bot_id.as_deref(), Some("cli_a1b2"));
    assert_eq!(health.user_id.as_deref(), Some("ou_owner"));
    assert!(!serde_json::to_string(&health).unwrap().contains("secret"));
}
