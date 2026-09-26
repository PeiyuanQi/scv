//! Unit tests for `src/hub.rs`.

use super::*;

#[tokio::test]
async fn bridges_and_chats_are_visible_while_registered() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "wechat:default", Some("owner".into()));
    let (registration, mut notices) = link.register();
    assert_eq!(hub.owner("wechat:default"), Some(Some("owner".into())));
    assert_eq!(hub.owner("feishu:default"), None);

    registration.set_owner_claims(2);
    assert_eq!(hub.owner_claims(), 2);

    let chat = registration.conversation("owner");
    chat.update(Some("session-1"), 3);
    assert_eq!(
        hub.origin("session-1"),
        Some(Origin {
            component: "wechat:default".into(),
            peer: "owner".into()
        })
    );
    assert_eq!(hub.session_work("session-1"), 3);
    drop(chat);
    assert_eq!(hub.origin("session-1"), None);
    assert_eq!(hub.session_work("session-1"), 0);

    let notify = hub.notify("wechat:default", "owner", "hello");
    let store = async {
        let notice = notices.recv().await.unwrap();
        assert_eq!(
            (notice.to.as_str(), notice.text.as_str()),
            ("owner", "hello")
        );
        notice.stored();
    };
    let (result, ()) = tokio::join!(notify, store);
    assert_eq!(result, Ok(()));

    drop(registration);
    assert_eq!(hub.owner("wechat:default"), None);
    assert_eq!(hub.owner_claims(), 0);
    assert_eq!(
        hub.notify("wechat:default", "owner", "late").await,
        Err(NotifyError::NotRunning)
    );
}

#[tokio::test]
async fn a_dropped_notice_is_reported_as_not_stored() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "feishu:default", None);
    let (_registration, mut notices) = link.register();
    let notify = hub.notify("feishu:default", "someone", "text");
    let drop_it = async {
        drop(notices.recv().await.unwrap());
    };
    let (result, ()) = tokio::join!(notify, drop_it);
    assert_eq!(result, Err(NotifyError::NotStored));
}

#[tokio::test(start_paused = true)]
async fn a_notice_not_stored_in_time_is_abandoned() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "wechat:default", None);
    let (_registration, mut notices) = link.register();
    let notify = hub.notify("wechat:default", "owner", "text");
    let waiting = async {
        let notice = notices.recv().await.unwrap();
        assert!(!notice.abandoned(), "its sender is still waiting");
        notice
    };
    let (result, notice) = tokio::join!(notify, waiting);
    assert_eq!(result, Err(NotifyError::NotStored));
    assert!(notice.abandoned(), "the bridge must not store it late");
}

#[test]
fn a_replaced_bridge_is_not_withdrawn_by_its_predecessor() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "wechat:default", None);
    let (old, _old_notices) = link.register();
    let (_new, _new_notices) = link.register();
    drop(old);
    assert!(hub.owner("wechat:default").is_some());
}

#[test]
fn the_owners_last_chat_survives_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("last-owner.json");
    let hub = Hub::new(Some(path.clone()));
    let link = Link::new(Arc::clone(&hub), "feishu:default", Some("ou_1".into()));
    let (registration, _notices) = link.register();
    registration.owner_wrote("ou_1");
    let restarted = Hub::new(Some(path.clone()));
    let last = restarted.last_owner().unwrap();
    assert_eq!(
        (last.component.as_str(), last.peer.as_str()),
        ("feishu:default", "ou_1")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn only_an_accounts_first_recovery_sees_the_planned_restart() {
    let hub = Hub::new(None);
    hub.set_restart(Some(Restart {
        to_version: "9.9.9".into(),
    }));
    let wechat = Link::new(Arc::clone(&hub), "wechat:default", None);
    let feishu = Link::new(Arc::clone(&hub), "feishu:default", None);
    assert_eq!(wechat.take_restart().unwrap().to_version, "9.9.9");
    assert!(wechat.take_restart().is_none(), "a later bridge restart");
    assert!(feishu.take_restart().is_some());
}

#[test]
fn a_detached_link_shares_nothing() {
    let link = Link::detached();
    let (registration, mut notices) = link.register();
    registration.set_owner_claims(1);
    registration.owner_wrote("peer");
    registration.conversation("peer").update(Some("s"), 1);
    assert!(link.take_restart().is_none());
    assert!(notices.try_recv().is_err());
}

#[tokio::test]
async fn one_question_waits_per_chat_until_answered_or_withdrawn() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "wechat:default", Some("owner".into()));
    let (registration, _notices) = link.register();
    let answered = hub.ask("q1", "wechat:default", "owner").unwrap();
    // Nothing answers a question before it is on its way to the owner.
    assert!(!registration.asking("owner"));
    assert!(registration.take_question("owner").is_none());
    hub.open("q1");
    assert!(registration.asking("owner"));
    assert!(!registration.asking("other"));
    // A second question in the same chat is refused; another chat may ask.
    assert!(hub.ask("q2", "wechat:default", "owner").is_none());
    let elsewhere = hub.ask("q3", "feishu:default", "owner").unwrap();
    hub.open("q3");
    assert!(
        !Link::new(Arc::clone(&hub), "wechat:other", None)
            .register()
            .0
            .asking("owner")
    );

    let answer = registration.take_question("owner").unwrap();
    assert!(!registration.asking("owner"), "taken once");
    assert!(registration.take_question("owner").is_none());
    assert!(!hub.withdraw("q1"), "an answer on its way is not withdrawn");
    answer.give(true);
    assert_eq!(answered.await, Ok(true));

    // Withdrawing frees the chat; a dropped answer tells the asker.
    assert!(hub.withdraw("q3"));
    assert!(!hub.withdraw("q3"));
    assert!(elsewhere.await.is_err());
    let lost = hub.ask("q4", "wechat:default", "owner").unwrap();
    hub.open("q4");
    drop(registration.take_question("owner"));
    assert!(lost.await.is_err());
}

#[test]
fn a_detached_link_holds_no_question() {
    let link = Link::detached();
    let (registration, _notices) = link.register();
    assert!(!registration.asking("peer"));
    assert!(registration.take_question("peer").is_none());
}
