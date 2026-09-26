//! Unit tests for `src/restart.rs`.

use super::*;
use scv_channels::hub::Link;

/// Notices a bridge stand-in stored, as (to, text).
type Stored = Arc<SyncMutex<Vec<(String, String)>>>;
/// Plans a recording restarter would have carried out.
type Launched = Arc<SyncMutex<Vec<Plan>>>;

fn plan(state: PlanState) -> Plan {
    Plan {
        id: "abcd1234".into(),
        state,
        from_version: "0.1.36".into(),
        to_version: "0.1.37".into(),
        commit: Some("abc1234".into()),
        from_layout: 1,
        to_layout: 1,
        unit: "scv.service".into(),
        binary: PathBuf::from("/bin/scv"),
        previous: None,
        requester: None,
        origin: None,
        expected: Vec::new(),
        requested_unix: 1000,
        deadline_unix: 1600,
        restart_unix: Some(1100),
        waited_out: false,
        detail: None,
        verify_seconds: VERIFY_SECONDS,
    }
}

fn states(entries: &[(&str, ComponentState)]) -> HashMap<String, ComponentState> {
    entries
        .iter()
        .map(|(id, state)| ((*id).to_owned(), state.clone()))
        .collect()
}

fn candidates(ids: &[&str]) -> Vec<Candidate> {
    ids.iter()
        .map(|id| Candidate {
            component: (*id).to_owned(),
            peer: None,
        })
        .collect()
}

#[test]
fn notices_go_to_the_first_connected_account_in_order() {
    let owner = |_: &str| Some(Some("owner".to_owned()));
    let list = candidates(&["feishu:default", "wechat:default"]);
    let both = states(&[
        ("feishu:default", ComponentState::Connected),
        ("wechat:default", ComponentState::Connected),
    ]);
    assert_eq!(
        pick(&list, &both, &owner, None, false),
        Pick::Send {
            component: "feishu:default".into(),
            peer: "owner".into()
        }
    );
    // Feishu is still connecting: it keeps its place during the grace.
    let starting = states(&[
        ("feishu:default", ComponentState::Starting),
        ("wechat:default", ComponentState::Connected),
    ]);
    assert_eq!(pick(&list, &starting, &owner, None, false), Pick::Wait);
    assert_eq!(
        pick(&list, &starting, &owner, None, true),
        Pick::Send {
            component: "wechat:default".into(),
            peer: "owner".into()
        }
    );
    // Never on the excluded account, such as the one that is down.
    assert_eq!(
        pick(&list, &both, &owner, Some("feishu:default"), false),
        Pick::Send {
            component: "wechat:default".into(),
            peer: "owner".into()
        }
    );
    let failed = states(&[
        ("feishu:default", ComponentState::Failed),
        ("wechat:default", ComponentState::Disabled),
    ]);
    assert_eq!(pick(&list, &failed, &owner, None, false), Pick::Nothing);
}

#[test]
fn an_account_without_a_known_owner_is_skipped() {
    let owner =
        |component: &str| Some((component == "wechat:default").then(|| "wx-owner".to_owned()));
    let list = candidates(&["feishu:default", "wechat:default"]);
    let both = states(&[
        ("feishu:default", ComponentState::Connected),
        ("wechat:default", ComponentState::Connected),
    ]);
    assert_eq!(
        pick(&list, &both, &owner, None, true),
        Pick::Send {
            component: "wechat:default".into(),
            peer: "wx-owner".into()
        }
    );
}

fn notifier(
    hub: &Arc<Hub>,
    list: Option<Vec<String>>,
) -> (Notifier, Arc<SyncMutex<HashMap<String, ComponentState>>>) {
    let states = Arc::new(SyncMutex::new(HashMap::new()));
    (
        Notifier {
            hub: Arc::clone(hub),
            instance: crate::test_support::test_instance("/unused"),
            states: States::Fixed(Arc::clone(&states)),
            grace: Duration::from_millis(200),
            give_up: Duration::from_secs(5),
            poll: Duration::from_millis(20),
            list,
        },
        states,
    )
}

/// Run a bridge stand-in that stores every notice it receives.
fn bridge(
    hub: &Arc<Hub>,
    component: &str,
    owner: &str,
) -> (scv_channels::hub::Registration, Stored) {
    let link = Link::new(Arc::clone(hub), component, Some(owner.into()));
    let (registration, mut notices) = link.register();
    let stored = Arc::new(SyncMutex::new(Vec::new()));
    let sink = Arc::clone(&stored);
    tokio::spawn(async move {
        while let Some(notice) = notices.recv().await {
            sink.lock()
                .unwrap()
                .push((notice.to.clone(), notice.text.clone()));
            notice.stored();
        }
    });
    (registration, stored)
}

#[tokio::test]
async fn an_announcement_goes_to_the_chat_that_asked() {
    let hub = Hub::new(None);
    let (notifier, health) = notifier(&hub, Some(vec!["feishu:default".into()]));
    let (_wechat, wechat) = bridge(&hub, "wechat:default", "wx-owner");
    let (_feishu, feishu) = bridge(&hub, "feishu:default", "ou-owner");
    *health.lock().unwrap() = states(&[
        ("wechat:default", ComponentState::Connected),
        ("feishu:default", ComponentState::Connected),
    ]);
    let origin = Origin {
        component: "wechat:default".into(),
        peer: "wx-owner".into(),
    };
    let went = notifier
        .deliver(Some(&origin), "updated", None, &CancellationToken::new())
        .await;
    assert_eq!(went.as_deref(), Some("wechat:default"));
    assert_eq!(
        *wechat.lock().unwrap(),
        [("wx-owner".into(), "updated".into())]
    );
    assert!(feishu.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_announcement_falls_back_when_the_asking_chat_stays_down() {
    let hub = Hub::new(None);
    let (notifier, health) = notifier(
        &hub,
        Some(vec!["feishu:default".into(), "wechat:default".into()]),
    );
    let (_feishu, feishu) = bridge(&hub, "feishu:default", "ou-owner");
    *health.lock().unwrap() = states(&[
        ("wechat:default", ComponentState::Backoff),
        ("feishu:default", ComponentState::Connected),
    ]);
    let origin = Origin {
        component: "wechat:default".into(),
        peer: "wx-owner".into(),
    };
    let went = notifier
        .deliver(Some(&origin), "updated", None, &CancellationToken::new())
        .await;
    assert_eq!(went.as_deref(), Some("feishu:default"));
    let stored = feishu.lock().unwrap().clone();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0, "ou-owner");
    assert!(
        stored[0]
            .1
            .starts_with("(You asked on WeChat, which is not connected"),
        "{}",
        stored[0].1
    );
    assert!(stored[0].1.ends_with("updated"));
}

#[tokio::test]
async fn without_a_notify_list_notices_go_to_the_owners_last_chat() {
    let directory = tempfile::tempdir().unwrap();
    let hub = Hub::new(Some(directory.path().join("last-owner.json")));
    let (notifier, health) = notifier(&hub, Some(Vec::new()));
    let (wechat_registration, wechat) = bridge(&hub, "wechat:default", "wx-owner");
    let (_feishu, feishu) = bridge(&hub, "feishu:default", "ou-owner");
    *health.lock().unwrap() = states(&[
        ("wechat:default", ComponentState::Connected),
        ("feishu:default", ComponentState::Connected),
    ]);
    // Nobody wrote yet: there is nowhere to send it.
    assert_eq!(
        notifier
            .deliver(None, "crashed", None, &CancellationToken::new())
            .await,
        None
    );
    wechat_registration.owner_wrote("wx-owner");
    assert_eq!(
        notifier
            .deliver(None, "crashed", None, &CancellationToken::new())
            .await
            .as_deref(),
        Some("wechat:default")
    );
    assert_eq!(wechat.lock().unwrap().len(), 1);
    assert!(feishu.lock().unwrap().is_empty());
}

#[test]
fn the_next_daemon_announces_each_outcome() {
    let verified = plan(PlanState::Verified);
    assert_eq!(
        decide(&verified, "0.1.37", false),
        Decision::Say("SCV updated: now running v0.1.37 (abc1234).".into())
    );
    let mut rolled_back = plan(PlanState::RolledBack);
    rolled_back.detail = Some("v0.1.37 started, but wechat:default did not reconnect".into());
    assert_eq!(
        decide(&rolled_back, "0.1.36", false),
        Decision::Say(
            "The update to v0.1.37 failed: v0.1.37 started, but wechat:default did not \
                 reconnect. SCV rolled back to v0.1.36."
                .into()
        )
    );
    let mut refused = plan(PlanState::Failed);
    refused.detail = Some("x; not rolled back: y".into());
    assert_eq!(
        decide(&refused, "0.1.37", false),
        Decision::Say("SCV is running v0.1.37 (abc1234), but x; not rolled back: y.".into())
    );
    // The watchdog is still checking: say nothing yet.
    let restarting = plan(PlanState::Restarting);
    assert_eq!(decide(&restarting, "0.1.37", false), Decision::Wait);
    assert!(
        matches!(decide(&restarting, "0.1.37", true), Decision::Say(text) if text.contains("did not report back"))
    );
    assert!(
        matches!(decide(&restarting, "0.1.36", true), Decision::Say(text) if text.contains("did not take effect"))
    );
    assert_eq!(decide(&restarting, "0.2.0", true), Decision::Drop);
    let mut waited = plan(PlanState::Verified);
    waited.waited_out = true;
    assert!(
        matches!(decide(&waited, "0.1.37", false), Decision::Say(text) if text.contains("waited 10 minutes"))
    );
}

#[test]
fn rollback_is_binary_only_and_refused_across_config_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let previous = directory.path().join("scv.prev");
    std::fs::write(&previous, b"old").unwrap();
    let mut same = plan(PlanState::Restarting);
    same.previous = Some(previous.clone());
    assert_eq!(rollback_refusal(&same), None);
    let mut changed = same.clone();
    changed.to_layout = 2;
    assert!(
        rollback_refusal(&changed)
            .unwrap()
            .contains("config layout 2")
    );
    let mut missing = same.clone();
    missing.previous = None;
    assert!(
        rollback_refusal(&missing)
            .unwrap()
            .contains("no copy of v0.1.36")
    );
}

#[test]
fn a_rollback_copy_replaces_the_binary_whole_and_executable() {
    let directory = tempfile::tempdir().unwrap();
    let previous = directory.path().join("scv.prev");
    let binary = directory.path().join("scv");
    std::fs::write(&previous, b"old release").unwrap();
    std::fs::write(&binary, b"new release").unwrap();
    install_copy(&previous, &binary).unwrap();
    assert_eq!(std::fs::read(&binary).unwrap(), b"old release");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&binary).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    let leftovers: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".scv-install")
        })
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn only_a_recent_restart_explains_interrupted_work() {
    let directory = tempfile::tempdir().unwrap();
    let hub = Hub::new(None);
    let mut recent = plan(PlanState::Verified);
    recent.restart_unix = Some(unix_now() - 30);
    save_plan(&Layout::new(directory.path()).update_plan(), &recent).unwrap();
    startup(&Layout::new(directory.path()), &hub);
    assert_eq!(hub.restart().unwrap().to_version, "0.1.37");

    let mut old = recent.clone();
    old.restart_unix = Some(unix_now() - 2 * RESTART_CONTEXT_MAX_AGE);
    save_plan(&Layout::new(directory.path()).update_plan(), &old).unwrap();
    startup(&Layout::new(directory.path()), &hub);
    assert!(hub.restart().is_none());

    let waiting = plan(PlanState::Waiting);
    save_plan(&Layout::new(directory.path()).update_plan(), &waiting).unwrap();
    startup(&Layout::new(directory.path()), &hub);
    assert!(hub.restart().is_none(), "a plan that never restarted");
}

#[test]
fn an_unclean_stop_is_detected_once() {
    let directory = tempfile::tempdir().unwrap();
    let hub = Hub::new(None);
    let first = startup(&Layout::new(directory.path()), &hub);
    assert!(first.unclean.is_none());
    // Pretend the marker was left by another daemon that died.
    let marker = Marker {
        pid: u32::MAX,
        version: "0.1.30".into(),
        started_unix: 5,
    };
    std::fs::write(
        Layout::new(directory.path()).daemon_marker(),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    let second = startup(&Layout::new(directory.path()), &hub);
    assert_eq!(
        second.unclean.map(|(version, _)| version).as_deref(),
        Some("0.1.30")
    );
    clean_shutdown(&Layout::new(directory.path()));
    let third = startup(&Layout::new(directory.path()), &hub);
    assert!(third.unclean.is_none());
}

#[test]
fn plans_are_private_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = Layout::new(directory.path()).update_plan();
    save_plan(&path, &plan(PlanState::Waiting)).unwrap();
    assert_eq!(load_plan(&path).unwrap(), Some(plan(PlanState::Waiting)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(
        load_plan(&directory.path().join("missing.json")).unwrap(),
        None
    );
}

#[test]
fn a_replaced_executable_is_named_by_its_path() {
    assert_eq!(
        strip_deleted(PathBuf::from("/home/u/.cargo/bin/scv (deleted)")),
        PathBuf::from("/home/u/.cargo/bin/scv")
    );
    assert_eq!(
        strip_deleted(PathBuf::from("/usr/bin/scv")),
        PathBuf::from("/usr/bin/scv")
    );
}

/// A restarter whose restarts are recorded, not carried out.
fn recording(
    home: &Path,
    hub: &Arc<Hub>,
    registry: &Arc<DelegationRegistry>,
) -> (Arc<Restarter>, Launched, Arc<Mutex<Components>>) {
    let components = Arc::new(Mutex::new(Components::new(
        crate::test_support::test_instance("/unused"),
        PathBuf::from("/"),
    )));
    let launched = Arc::new(SyncMutex::new(Vec::new()));
    let restarter = Arc::new(Restarter {
        launcher: Launcher::Record(Arc::clone(&launched)),
        notifier: Notifier::new(
            crate::test_support::test_instance(home),
            Arc::clone(hub),
            Arc::downgrade(&components),
        ),
        instance: crate::test_support::test_instance(home),
        hub: Arc::clone(hub),
        registry: Arc::clone(registry),
        components: Arc::downgrade(&components),
        cancel: CancellationToken::new(),
        current: SyncMutex::new(None),
    });
    (restarter, launched, components)
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition holds in time");
}

#[tokio::test]
async fn a_restart_waits_for_the_requesting_job_its_report_and_owner_messages() {
    use scv_tools::delegation::{DelegationRecord, ProcessIdentity};
    use std::os::unix::process::CommandExt as _;
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
    let hub = Hub::new(None);
    let (restarter, launched, _components) = recording(home.path(), &hub, &registry);
    // The delegation running the deploy, started by this daemon.
    let mut agent = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let record = DelegationRecord {
        handle: "codex-a1b2c3".into(),
        agent: "codex".into(),
        instance: registry.instance().into(),
        session: "s".into(),
        owner: ProcessIdentity::current().unwrap(),
        process: ProcessIdentity::of(agent.id()).unwrap(),
        pgid: agent.id(),
        cwd: "/work".into(),
        started_unix: 1,
        depth: 1,
        conversation: None,
        turn: None,
        idle_since_unix: None,
    };
    std::fs::create_dir_all(registry.record_dir()).unwrap();
    std::fs::write(
        registry.record_dir().join("codex-a1b2c3.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let chain = format!("{}/s/codex-a1b2c3", registry.instance());
    assert_eq!(
        restarter.requester(&format!("elsewhere/x/codex-000000;{chain}")),
        Some(Requester {
            handle: "codex-a1b2c3".into(),
            session: "s".into()
        })
    );
    assert_eq!(restarter.requester("elsewhere/s/codex-a1b2c3"), None);
    // Its chat on WeChat, whose report is not stored yet.
    let link = Link::new(Arc::clone(&hub), "wechat:default", Some("owner".into()));
    let (bridge, _notices) = link.register();
    let chat = bridge.conversation("owner");
    chat.update(Some("s"), 1);

    let mut waiting = plan(PlanState::Waiting);
    waiting.requester = restarter.requester(&chain);
    waiting.requested_unix = unix_now();
    waiting.deadline_unix = unix_now() + 600;
    let info = restarter.arm(waiting).unwrap();
    assert_eq!(info.waiting_for.as_deref(), Some("codex-a1b2c3 to finish"));
    assert_eq!(info.origin.as_deref(), Some("wechat:default"));

    agent.kill().unwrap();
    agent.wait().unwrap();
    eventually(|| {
        restarter
            .info()
            .and_then(|info| info.waiting_for)
            .as_deref()
            == Some("codex-a1b2c3's report")
    })
    .await;
    bridge.set_owner_claims(1);
    chat.update(Some("s"), 0);
    eventually(|| {
        restarter
            .info()
            .and_then(|info| info.waiting_for)
            .as_deref()
            == Some("an owner message to be answered")
    })
    .await;
    assert!(launched.lock().unwrap().is_empty());
    bridge.set_owner_claims(0);
    eventually(|| launched.lock().unwrap().len() == 1).await;
    let started = launched.lock().unwrap()[0].clone();
    assert_eq!(started.state, PlanState::Restarting);
    assert!(!started.waited_out);
    assert!(started.restart_unix.is_some());
    assert_eq!(
        started.origin,
        Some(Origin {
            component: "wechat:default".into(),
            peer: "owner".into()
        })
    );
    assert_eq!(
        load_plan(&Layout::new(home.path()).update_plan())
            .unwrap()
            .unwrap()
            .state,
        PlanState::Restarting
    );
}

#[tokio::test]
async fn a_restart_goes_ahead_at_its_deadline_and_says_so() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
    let hub = Hub::new(None);
    let (restarter, launched, _components) = recording(home.path(), &hub, &registry);
    let link = Link::new(Arc::clone(&hub), "feishu:default", Some("owner".into()));
    let (bridge, _notices) = link.register();
    bridge.set_owner_claims(1);
    let mut waiting = plan(PlanState::Waiting);
    waiting.requested_unix = unix_now();
    waiting.deadline_unix = unix_now();
    let info = restarter.arm(waiting).unwrap();
    assert_eq!(
        info.waiting_for.as_deref(),
        Some("an owner message to be answered")
    );
    eventually(|| launched.lock().unwrap().len() == 1).await;
    assert!(launched.lock().unwrap()[0].waited_out);
}

/// A nested SCV this daemon started for conversation `scv-1`, whose process
/// keeps running between turns, recorded mid-turn or `idle_since_unix`.
fn live_agent(
    registry: &DelegationRegistry,
    idle_since_unix: Option<u64>,
) -> (std::process::Child, scv_tools::delegation::DelegationRecord) {
    use scv_tools::delegation::{DelegationRecord, ProcessIdentity};
    use std::os::unix::process::CommandExt as _;
    let agent = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let record = DelegationRecord {
        handle: "scv-92f0c3".into(),
        agent: "scv".into(),
        instance: registry.instance().into(),
        session: "s".into(),
        owner: ProcessIdentity::current().unwrap(),
        process: ProcessIdentity::of(agent.id()).unwrap(),
        pgid: agent.id(),
        cwd: "/work".into(),
        started_unix: 1,
        depth: 1,
        conversation: Some("scv-1".into()),
        turn: Some(1),
        idle_since_unix,
    };
    write_record(registry, &record);
    (agent, record)
}

fn write_record(registry: &DelegationRegistry, record: &scv_tools::delegation::DelegationRecord) {
    std::fs::create_dir_all(registry.record_dir()).unwrap();
    std::fs::write(
        registry
            .record_dir()
            .join(format!("{}.json", record.handle)),
        serde_json::to_vec(record).unwrap(),
    )
    .unwrap();
}

/// Arm a restart that the delegation `handle` of session `s` asked for.
fn arm_for(restarter: &Arc<Restarter>, registry: &DelegationRegistry, handle: &str) -> RestartInfo {
    let mut waiting = plan(PlanState::Waiting);
    waiting.requester = restarter.requester(&format!("{}/s/{handle}", registry.instance()));
    assert!(waiting.requester.is_some());
    waiting.requested_unix = unix_now();
    waiting.deadline_unix = unix_now() + 600;
    restarter.arm(waiting).unwrap()
}

#[tokio::test]
async fn a_live_agent_between_turns_does_not_hold_a_restart() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
    let hub = Hub::new(None);
    let (restarter, launched, _components) = recording(home.path(), &hub, &registry);
    // The turn that ran the deploy has ended; the nested SCV waits for the
    // next one, as the owner's scv-92f0c3 did on 2026-09-26.
    let (mut agent, record) = live_agent(&registry, Some(unix_now()));
    let info = arm_for(&restarter, &registry, &record.handle);
    assert_eq!(info.waiting_for, None);
    eventually(|| launched.lock().unwrap().len() == 1).await;
    assert!(!launched.lock().unwrap()[0].waited_out);
    assert!(record.process.is_alive(), "the agent itself was left alone");
    agent.kill().unwrap();
    agent.wait().unwrap();
}

#[tokio::test]
async fn a_live_agent_mid_turn_holds_a_restart_until_its_turn_ends() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(
        home.path(),
    )));
    let hub = Hub::new(None);
    let (restarter, launched, _components) = recording(home.path(), &hub, &registry);
    let (mut agent, mut record) = live_agent(&registry, None);
    let info = arm_for(&restarter, &registry, &record.handle);
    assert_eq!(info.waiting_for.as_deref(), Some("scv-92f0c3 to finish"));
    // More checks than a restart needs clear pass while the turn runs.
    tokio::time::sleep(Duration::from_millis(u64::from(CLEAR_CHECKS + 1) * 1000)).await;
    assert!(launched.lock().unwrap().is_empty());
    assert_eq!(
        restarter
            .info()
            .and_then(|info| info.waiting_for)
            .as_deref(),
        Some("scv-92f0c3 to finish")
    );
    // Its turn ends and it stays for the next one.
    record.idle_since_unix = Some(unix_now());
    write_record(&registry, &record);
    eventually(|| launched.lock().unwrap().len() == 1).await;
    assert!(!launched.lock().unwrap()[0].waited_out);
    agent.kill().unwrap();
    agent.wait().unwrap();
}

#[test]
fn session_activity_counts_turns_until_the_session_ends() {
    let tracker = SessionTracker::new("session-activity-test", None);
    assert!(!session_busy("session-activity-test"));
    tracker.set_busy(true);
    assert!(session_busy("session-activity-test"));
    drop(tracker);
    assert!(!session_busy("session-activity-test"));
}
