//! Unit tests for `src/confirm.rs`.

use std::path::PathBuf;

use scv_channels::hub::{Link, Registration};
use scv_client::Layout;
use scv_protocol::{ComponentState, DaemonCommand, DaemonStatus};
use tokio::sync::Mutex as AsyncMutex;

use super::*;
use crate::components::Components;
use crate::control::{ControlFailure, daemon_control};

/// Notices a bridge stand-in stored, as (to, text).
type Stored = Arc<Mutex<Vec<(String, String)>>>;

/// What the stand-in platform does with a question's text.
#[derive(Clone, Copy)]
enum Platform {
    Delivers,
    Refuses,
    /// Keeps failing, so the text waits in the outbox.
    Stalls,
}

/// Run a bridge stand-in for `component` that stores every notice it
/// receives and delivers questions at once.
fn bridge(hub: &Arc<Hub>, component: &str, owner: &str) -> (Arc<Registration>, Stored) {
    bridge_on(hub, component, owner, Platform::Delivers)
}

/// [`bridge`], reporting each question's delivery as `platform` has it.
fn bridge_on(
    hub: &Arc<Hub>,
    component: &str,
    owner: &str,
    platform: Platform,
) -> (Arc<Registration>, Stored) {
    let link = Link::new(Arc::clone(hub), component, Some(owner.into()));
    let (registration, mut notices) = link.register();
    let registration = Arc::new(registration);
    let delivery = Arc::clone(&registration);
    let stored = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&stored);
    tokio::spawn(async move {
        while let Some(notice) = notices.recv().await {
            sink.lock()
                .unwrap()
                .push((notice.to.clone(), notice.text.clone()));
            let question = notice.question.clone();
            notice.stored();
            match (question, platform) {
                (Some(id), Platform::Delivers) => delivery.question_delivered(&id),
                (Some(id), Platform::Refuses) => delivery.question_undelivered(&id),
                _ => {}
            }
        }
    });
    (registration, stored)
}

fn texts(stored: &Stored) -> Vec<String> {
    stored
        .lock()
        .unwrap()
        .iter()
        .map(|(_, text)| text.clone())
        .collect()
}

/// A confirmer whose notify target is `list`, every account in it connected.
fn confirmer(
    hub: &Arc<Hub>,
    registry: &Arc<DelegationRegistry>,
    list: &[&str],
) -> (Arc<Confirmer>, CancellationToken) {
    let states = list
        .iter()
        .map(|id| ((*id).to_owned(), ComponentState::Connected))
        .collect();
    let notifier = Notifier::fixed(
        hub,
        list.iter().map(|id| (*id).to_owned()).collect(),
        states,
    );
    let cancel = CancellationToken::new();
    (
        Confirmer::new(
            Arc::clone(hub),
            Arc::clone(registry),
            notifier,
            cancel.clone(),
        ),
        cancel,
    )
}

/// The daemon's control path with `confirmer` in place.
fn components(hub: &Arc<Hub>, confirmer: &Arc<Confirmer>) -> Arc<AsyncMutex<Components>> {
    let mut components = Components::with_hub(
        crate::test_support::test_instance("/unused"),
        PathBuf::from("/"),
        Arc::clone(hub),
    );
    components.set_confirmer(Arc::clone(confirmer));
    Arc::new(AsyncMutex::new(components))
}

async fn control(
    components: &Arc<AsyncMutex<Components>>,
    registry: &DelegationRegistry,
    command: DaemonCommand,
) -> Result<DaemonStatus, String> {
    daemon_control(components, registry, command)
        .await
        .map_err(|failure| match failure {
            ControlFailure::Confirm(message) => message,
            _ => panic!("not a question failure"),
        })
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

/// Record a delegation this process runs for session `session`, as the
/// daemon does for an agent a chat started; returns its `SCV_PARENT` chain.
fn delegation(registry: &DelegationRegistry, session: &str) -> String {
    use scv_tools::delegation::{DelegationRecord, ProcessIdentity};
    let own = ProcessIdentity::current().unwrap();
    let record = DelegationRecord {
        handle: "codex-a1b2c3".into(),
        agent: "codex".into(),
        instance: registry.instance().into(),
        session: session.into(),
        owner: own,
        process: own,
        pgid: own.pid,
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
    format!(
        "elsewhere/x/scv-000000;{}/{session}/codex-a1b2c3",
        registry.instance()
    )
}

#[test]
fn the_question_says_how_long_it_waits() {
    assert_eq!(
        question_text("Publish?", 1800),
        "Publish?\n\nReply yes or no. No answer in 30 minutes counts as no."
    );
    assert_eq!(
        question_text("Publish?", 5),
        "Publish?\n\nReply yes or no. No answer in 1 minute counts as no."
    );
    assert_eq!(
        question_text("Publish?", 61),
        "Publish?\n\nReply yes or no. No answer in 2 minutes counts as no."
    );
}

#[tokio::test]
async fn a_question_goes_to_the_chat_that_started_the_work_and_the_answer_is_read_back() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&Layout::new(home.path())));
    let hub = Hub::new(None);
    // The notify target is Feishu, but the work started on WeChat.
    let (wechat, asked) = bridge(&hub, "wechat:default", "wx-owner");
    let (feishu, notified) = bridge(&hub, "feishu:default", "ou-owner");
    let chat = wechat.conversation("wx-owner");
    chat.update(Some("s"), 1);
    let chain = delegation(&registry, "s");
    let (confirmer, _cancel) = confirmer(&hub, &registry, &["feishu:default"]);
    let components = components(&hub, &confirmer);

    let status = control(
        &components,
        &registry,
        DaemonCommand::ConfirmAsk {
            question: "  Publish SCV 0.3.0 to crates.io?  ".into(),
            parent: Some(chain),
            timeout_seconds: Some(600),
        },
    )
    .await
    .unwrap();
    let info = status.confirm.unwrap();
    assert_eq!(info.state, ConfirmState::Pending);
    assert_eq!(info.chat, "wechat:default");
    assert!(info.deadline_unix_seconds >= unix_now() + 590);
    eventually(|| !asked.lock().unwrap().is_empty()).await;
    assert_eq!(
        *asked.lock().unwrap(),
        [(
            "wx-owner".to_owned(),
            "Publish SCV 0.3.0 to crates.io?\n\nReply yes or no. No answer in 10 minutes counts as no."
                .to_owned()
        )]
    );
    assert!(notified.lock().unwrap().is_empty());
    let poll = || {
        control(
            &components,
            &registry,
            DaemonCommand::ConfirmStatus {
                id: info.id.clone(),
            },
        )
    };
    assert_eq!(
        poll().await.unwrap().confirm.unwrap().state,
        ConfirmState::Pending
    );
    eventually(|| wechat.asking("wx-owner").is_some()).await;
    wechat
        .take_question("wx-owner", u64::MAX)
        .unwrap()
        .give(true);
    let mut state = ConfirmState::Pending;
    for _ in 0..200 {
        state = poll().await.unwrap().confirm.unwrap().state;
        if state != ConfirmState::Pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state, ConfirmState::Yes);
    // The bridge acknowledged it; the daemon adds nothing to the chat.
    assert_eq!(asked.lock().unwrap().len(), 1);

    // The chat is free again; this time the owner says no.
    let info = confirmer.ask("Deploy it?", None, Some(60)).await.unwrap();
    assert_eq!(
        info.chat, "feishu:default",
        "a terminal asks the notify target"
    );
    eventually(|| notified.lock().unwrap().len() == 1).await;
    eventually(|| feishu.asking("ou-owner").is_some()).await;
    feishu
        .take_question("ou-owner", u64::MAX)
        .unwrap()
        .give(false);
    eventually(|| confirmer.status(&info.id).unwrap().state == ConfirmState::No).await;
}

#[tokio::test]
async fn no_answer_in_time_counts_as_no_and_the_chat_is_told() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&Layout::new(home.path())));
    let hub = Hub::new(None);
    let (feishu, stored) = bridge(&hub, "feishu:default", "ou-owner");
    let (confirmer, _cancel) = confirmer(&hub, &registry, &["feishu:default"]);
    let info = confirmer.ask("Publish?", None, Some(1)).await.unwrap();
    eventually(|| confirmer.status(&info.id).unwrap().state == ConfirmState::Expired).await;
    eventually(|| stored.lock().unwrap().len() == 2).await;
    assert_eq!(
        texts(&stored),
        [
            "Publish?\n\nReply yes or no. No answer in 1 minute counts as no.",
            NO_ANSWER
        ]
    );
    // A late answer finds nothing to answer.
    assert!(feishu.take_question("ou-owner", u64::MAX).is_none());
}

#[tokio::test]
async fn a_question_the_platform_refuses_fails_without_an_answer() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&Layout::new(home.path())));
    let hub = Hub::new(None);
    let (_feishu, stored) = bridge_on(&hub, "feishu:default", "ou-owner", Platform::Refuses);
    let (confirmer, _cancel) = confirmer(&hub, &registry, &["feishu:default"]);
    let info = confirmer.ask("Publish?", None, Some(600)).await.unwrap();
    // `scv confirm` exits 2: the owner was never asked.
    eventually(|| confirmer.status(&info.id).unwrap().state == ConfirmState::Failed).await;
    assert_eq!(stored.lock().unwrap().len(), 1, "only the question itself");
    // The chat is free for the next question.
    assert!(hub.ask("next", "feishu:default", "ou-owner").is_some());
}

#[tokio::test]
async fn a_question_never_delivered_by_its_deadline_fails_quietly() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&Layout::new(home.path())));
    let hub = Hub::new(None);
    let (feishu, stored) = bridge_on(&hub, "feishu:default", "ou-owner", Platform::Stalls);
    let (confirmer, _cancel) = confirmer(&hub, &registry, &["feishu:default"]);
    let info = confirmer.ask("Publish?", None, Some(1)).await.unwrap();
    // No yes could count meanwhile, since the owner never saw it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(feishu.asking("ou-owner"), None);
    assert!(feishu.take_question("ou-owner", u64::MAX).is_none());
    // Not "no": nothing was asked, so `scv confirm` exits 2. The chat is
    // not told that no answer came to a question it never saw.
    eventually(|| confirmer.status(&info.id).unwrap().state == ConfirmState::Failed).await;
    assert_eq!(
        texts(&stored),
        ["Publish?\n\nReply yes or no. No answer in 1 minute counts as no."]
    );
    assert!(hub.ask("next", "feishu:default", "ou-owner").is_some());
}

#[tokio::test]
async fn a_question_nobody_follows_is_withdrawn() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&Layout::new(home.path())));
    let hub = Hub::new(None);
    let (wechat, stored) = bridge(&hub, "wechat:default", "owner");
    let notifier = Notifier::fixed(
        &hub,
        vec!["wechat:default".into()],
        HashMap::from([("wechat:default".to_owned(), ComponentState::Connected)]),
    );
    let confirmer = Arc::new(Confirmer {
        hub: Arc::clone(&hub),
        registry,
        notifier,
        cancel: CancellationToken::new(),
        lease: Duration::from_millis(100),
        entries: Mutex::default(),
    });
    let info = confirmer.ask("Publish?", None, None).await.unwrap();
    eventually(|| stored.lock().unwrap().len() == 2).await;
    assert_eq!(texts(&stored)[1], WITHDRAWN);
    assert_eq!(
        confirmer.status(&info.id).unwrap().state,
        ConfirmState::Withdrawn
    );
    assert!(wechat.take_question("owner", u64::MAX).is_none());
}

#[tokio::test]
async fn questions_that_cannot_be_asked_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::new(&Layout::new(home.path())));
    let hub = Hub::new(None);
    // No notify target is connected, and the work did not start in a chat.
    let (confirmer, cancel) = confirmer(&hub, &registry, &[]);
    let components = components(&hub, &confirmer);
    let ask = |question: &str| DaemonCommand::ConfirmAsk {
        question: question.into(),
        parent: None,
        timeout_seconds: None,
    };
    let refused = control(&components, &registry, ask("Publish?")).await;
    assert!(
        refused.as_ref().unwrap_err().contains("no owner chat"),
        "{refused:?}"
    );
    for question in [String::new(), " \n".into(), "x".repeat(5000)] {
        assert!(
            control(&components, &registry, ask(&question))
                .await
                .is_err()
        );
    }
    assert!(
        control(
            &components,
            &registry,
            DaemonCommand::ConfirmStatus {
                id: "nosuch".into()
            },
        )
        .await
        .unwrap_err()
        .contains("may have restarted")
    );

    // A chat that is not the owner's cannot be asked.
    let (bridge_of_bob, _stored) = bridge(&hub, "wechat:default", "owner");
    let chat = bridge_of_bob.conversation("bob");
    chat.update(Some("bob-session"), 0);
    let chain = delegation(&registry, "bob-session");
    let refused = confirmer.ask("Publish?", Some(&chain), None).await;
    assert!(
        refused
            .as_ref()
            .unwrap_err()
            .contains("owner's direct chat"),
        "{refused:?}"
    );

    // One question per chat; the daemon stopping withdraws it.
    let owner_chat = bridge_of_bob.conversation("owner");
    owner_chat.update(Some("owner-session"), 0);
    std::fs::remove_file(registry.record_dir().join("codex-a1b2c3.json")).unwrap();
    let chain = delegation(&registry, "owner-session");
    confirmer.ask("Publish?", Some(&chain), None).await.unwrap();
    let again = confirmer.ask("Publish again?", Some(&chain), None).await;
    assert!(
        again.as_ref().unwrap_err().contains("already waiting"),
        "{again:?}"
    );
    eventually(|| bridge_of_bob.asking("owner").is_some()).await;
    cancel.cancel();
    eventually(|| bridge_of_bob.asking("owner").is_none()).await;
}
