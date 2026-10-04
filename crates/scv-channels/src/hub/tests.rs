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
        assert_eq!(notice.question, None, "a plain notice asks nothing");
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
    // Nothing answers a question before its text reaches the owner.
    assert!(registration.question_waiting("q1"));
    assert_eq!(registration.asking("owner"), None);
    assert!(registration.take_question("owner", u64::MAX).is_none());
    let before = unix_ms();
    registration.question_delivered("q1");
    let delivered = registration.asking("owner").unwrap();
    assert!(delivered >= before && delivered <= unix_ms());
    assert_eq!(registration.asking("other"), None);
    // A second delivery report keeps the first time.
    registration.question_delivered("q1");
    assert_eq!(registration.asking("owner"), Some(delivered));
    // A second question in the same chat is refused; another chat may ask.
    assert!(hub.ask("q2", "wechat:default", "owner").is_none());
    let elsewhere = hub.ask("q3", "feishu:default", "owner").unwrap();
    // Only the account that holds a question opens it.
    registration.question_delivered("q3");
    assert!(!registration.question_waiting("q3"));
    let other = Link::new(Arc::clone(&hub), "wechat:other", None);
    assert_eq!(other.register().0.asking("owner"), None);

    // A message sent before the question reached the chat cannot take it.
    assert!(registration.take_question("owner", delivered - 1).is_none());
    let answer = registration.take_question("owner", delivered).unwrap();
    assert_eq!(registration.asking("owner"), None, "taken once");
    assert!(registration.take_question("owner", u64::MAX).is_none());
    assert!(!registration.question_waiting("q1"));
    assert_eq!(
        hub.withdraw("q1"),
        Withdrawal::Settled,
        "an answer on its way is not withdrawn"
    );
    answer.give(true);
    assert_eq!(answered.await, Ok(true));

    // Withdrawing frees the chat and says whether the owner saw it; a
    // dropped answer tells the asker.
    assert_eq!(hub.withdraw("q3"), Withdrawal::Unsent);
    assert_eq!(hub.withdraw("q3"), Withdrawal::Settled);
    assert!(elsewhere.await.is_err());
    let _seen = hub.ask("q4", "wechat:default", "owner").unwrap();
    registration.question_delivered("q4");
    assert_eq!(hub.withdraw("q4"), Withdrawal::Unanswered);
    let lost = hub.ask("q5", "wechat:default", "owner").unwrap();
    registration.question_delivered("q5");
    drop(registration.take_question("owner", u64::MAX));
    assert!(lost.await.is_err());
}

#[tokio::test]
async fn an_undelivered_question_fails_and_frees_the_chat() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "wechat:default", Some("owner".into()));
    let (registration, mut notices) = link.register();
    let answered = hub.ask("q1", "wechat:default", "owner").unwrap();
    // The question's text is a notice tagged with it.
    let sent = hub.send_question("q1", "wechat:default", "owner", "Publish?");
    let store = async {
        let notice = notices.recv().await.unwrap();
        assert_eq!(notice.question.as_deref(), Some("q1"));
        assert_eq!(notice.text, "Publish?");
        notice.stored();
    };
    let (result, ()) = tokio::join!(sent, store);
    assert_eq!(result, Ok(()));
    registration.question_undelivered("q1");
    assert!(
        answered.await.is_err(),
        "the asker learns no answer will come"
    );
    assert!(!registration.question_waiting("q1"));
    assert!(hub.ask("q2", "wechat:default", "owner").is_some());
}

#[test]
fn a_detached_link_holds_no_question() {
    let link = Link::detached();
    let (registration, _notices) = link.register();
    assert_eq!(registration.asking("peer"), None);
    assert!(registration.take_question("peer", u64::MAX).is_none());
    assert!(!registration.question_waiting("q1"));
}

/// An email account registered on a hub and taking its commands.
struct Serving {
    registration: MailRegistration,
    requests: mpsc::Receiver<MailRequest>,
}

/// Register `component`, reporting to `routes`, and take its commands.
fn serving(hub: &Arc<Hub>, component: &str, routes: &[&str]) -> Serving {
    let registration = hub.register_mail(
        component,
        routes.iter().copied().map(str::to_owned).collect(),
    );
    let (commands, requests) = mpsc::channel(8);
    registration.serve(commands);
    Serving {
        registration,
        requests,
    }
}

/// Evidence for a command the owner sent in the mail chat `route`.
fn evidence(route: &str) -> ChatEvidence {
    ChatEvidence {
        route: route.to_owned(),
        peer: "owner".into(),
        message_id: "m-owner".into(),
        sent_ms: Some(1_700_000_000_000),
    }
}

/// The chat command a request carries, and where its answer goes.
fn chat_command(request: MailRequest) -> (MailCommand, ChatEvidence, oneshot::Sender<MailReply>) {
    let MailWork::Chat { command, evidence } = request.work else {
        panic!("a mail-chat command arrived as a daemon order");
    };
    (command, evidence, request.reply)
}

#[tokio::test]
async fn approve_and_deny_codes_go_to_the_account_that_claimed_each() {
    let hub = Hub::new(None);
    let mut first = serving(&hub, "email:first", &["fake:mail"]);
    let mut second = serving(&hub, "email:second", &["fake:mail"]);
    assert!(first.registration.claim_code("Q7M2KD"));
    assert!(first.registration.claim_code("M2KDQ7"));
    assert!(second.registration.claim_code("XYZ789"));
    let evidence = evidence("fake:mail");

    let approve = hub.mail_command(
        evidence.clone(),
        MailCommand::Approve(vec![
            "Q7M2KD".into(),
            "XYZ789".into(),
            "M2KDQ7".into(),
            "NOCODE".into(),
        ]),
    );
    let answers = async {
        let (command, got, reply) = chat_command(first.requests.recv().await.unwrap());
        assert_eq!(got, evidence, "the owner's evidence reaches the account");
        assert_eq!(
            command,
            MailCommand::Approve(vec!["Q7M2KD".into(), "M2KDQ7".into()]),
            "one account's codes travel together"
        );
        reply
            .send(MailReply::Text("first: approved 2".into()))
            .unwrap();
        let (command, got, reply) = chat_command(second.requests.recv().await.unwrap());
        assert_eq!(got, evidence);
        assert_eq!(command, MailCommand::Approve(vec!["XYZ789".into()]));
        reply
            .send(MailReply::Text("second: approved 1".into()))
            .unwrap();
    };
    let (text, ()) = tokio::join!(approve, answers);
    assert_eq!(
        text, "No mail action has code NOCODE.\nfirst: approved 2\nsecond: approved 1",
        "an unknown code is one line, then one line per account"
    );

    let deny = hub.mail_command(
        evidence,
        MailCommand::Deny(vec!["XYZ789".into(), "Q7M2KD".into()]),
    );
    let answers = async {
        let (command, _, reply) = chat_command(first.requests.recv().await.unwrap());
        assert_eq!(command, MailCommand::Deny(vec!["Q7M2KD".into()]));
        reply.send(MailReply::Text("first: denied".into())).unwrap();
        let (command, _, reply) = chat_command(second.requests.recv().await.unwrap());
        assert_eq!(command, MailCommand::Deny(vec!["XYZ789".into()]));
        reply
            .send(MailReply::Text("second: denied".into()))
            .unwrap();
    };
    let (text, ()) = tokio::join!(deny, answers);
    assert_eq!(
        text, "second: denied\nfirst: denied",
        "lines follow the order the owner named the codes"
    );
}

#[tokio::test]
async fn an_unknown_code_or_handle_names_itself_when_some_account_is_running() {
    let hub = Hub::new(None);
    let running = serving(&hub, "email:work", &["fake:mail"]);
    assert!(running.registration.claim_code("Q7M2KD"));
    assert!(running.registration.claim_handle("4K7P"));
    let evidence = evidence("fake:mail");
    assert_eq!(
        hub.mail_command(
            evidence.clone(),
            MailCommand::Approve(vec!["ABC234".into()])
        )
        .await,
        "No mail action has code ABC234."
    );
    assert_eq!(
        hub.mail_command(
            evidence.clone(),
            MailCommand::Revise {
                code: "ABC234".into(),
                text: "shorter".into(),
            },
        )
        .await,
        "No mail action has code ABC234."
    );
    let missing = "No reported mail has handle #ZZZZ; handles work for mail reported in the \
         last days only.";
    for command in [
        MailCommand::Reply {
            handle: "ZZZZ".into(),
            text: String::new(),
        },
        MailCommand::Forward {
            handle: "ZZZZ".into(),
            to: vec!["a@example.com".into()],
            note: String::new(),
        },
        MailCommand::Message {
            handle: "ZZZZ".into(),
            action: crate::mail_chat::MessageAction::Trash,
        },
    ] {
        assert_eq!(
            hub.mail_command(evidence.clone(), command).await,
            missing,
            "a handle no running account claimed"
        );
    }
    assert!(
        running.requests.is_empty(),
        "an unknown code or handle is not offered to another account"
    );
}

#[tokio::test]
async fn with_no_account_taking_actions_a_command_says_nothing_was_done() {
    let hub = Hub::new(None);
    // Registered, but not taking actions: it has no authority.
    let _idle = hub.register_mail("email:idle", vec!["fake:mail".into()]);
    let evidence = evidence("fake:mail");
    let commands = [
        MailCommand::Approve(vec!["Q7M2KD".into()]),
        MailCommand::Deny(vec!["Q7M2KD".into()]),
        MailCommand::DenyAll,
        MailCommand::Reply {
            handle: "4K7P".into(),
            text: String::new(),
        },
        MailCommand::Forward {
            handle: "4K7P".into(),
            to: vec!["a@example.com".into()],
            note: String::new(),
        },
        MailCommand::Message {
            handle: "4K7P".into(),
            action: crate::mail_chat::MessageAction::Archive,
        },
        MailCommand::Compose {
            account: None,
            to: vec!["a@example.com".into()],
            text: "hello".into(),
        },
        MailCommand::Revise {
            code: "Q7M2KD".into(),
            text: "shorter".into(),
        },
    ];
    for command in commands {
        assert_eq!(
            hub.mail_command(evidence.clone(), command).await,
            crate::mail_chat::NOT_RUNNING_REPLY,
            "nothing on the route takes actions"
        );
    }
    // Status has nothing to ask; the bridge says so from the counts.
    assert_eq!(
        hub.mail_command(evidence, MailCommand::Status).await,
        "",
        "status with nobody taking actions adds no line"
    );
    let link = Link::detached();
    let (registration, _notices) = link.register_as(Purpose::Mail);
    assert_eq!(
        registration
            .mail_command(
                "owner",
                "m1",
                Some(1),
                MailCommand::Approve(vec!["Q7M2KD".into()])
            )
            .await,
        crate::mail_chat::NOT_RUNNING_REPLY,
        "a chat with no daemon cannot carry a command"
    );
}

#[tokio::test]
async fn reply_forward_and_message_follow_the_handle() {
    let hub = Hub::new(None);
    let mut work = serving(&hub, "email:work", &["fake:mail"]);
    let other = serving(&hub, "email:other", &["fake:mail"]);
    assert!(work.registration.claim_handle("4K7P"));
    assert!(
        !other.registration.claim_handle("4K7P"),
        "a second account cannot take a live handle"
    );
    let evidence = evidence("fake:mail");
    let commands = [
        MailCommand::Reply {
            handle: "4K7P".into(),
            text: "thanks".into(),
        },
        MailCommand::Forward {
            handle: "4K7P".into(),
            to: vec!["a@example.com".into()],
            note: "see this".into(),
        },
        MailCommand::Message {
            handle: "4K7P".into(),
            action: crate::mail_chat::MessageAction::Read,
        },
    ];
    for command in commands {
        let pending = hub.mail_command(evidence.clone(), command.clone());
        let expected = command.clone();
        let answer = async {
            let (got, evidence, reply) = chat_command(work.requests.recv().await.unwrap());
            assert_eq!(got, expected, "the handle's account receives the command");
            assert_eq!(evidence.peer, "owner");
            reply.send(MailReply::Text("done".into())).unwrap();
        };
        let (text, ()) = tokio::join!(pending, answer);
        assert_eq!(text, "done");
        assert!(
            other.requests.is_empty(),
            "the other account is not asked about this handle"
        );
    }
    work.registration.forget_handles(&["4K7P".into()]);
    assert_eq!(
        hub.mail_command(
            evidence,
            MailCommand::Reply {
                handle: "4K7P".into(),
                text: String::new(),
            },
        )
        .await,
        "No reported mail has handle #4K7P; handles work for mail reported in the last days only.",
        "a forgotten handle is no longer claimed"
    );
}

#[tokio::test]
async fn compose_goes_to_the_named_account_or_the_only_one() {
    let hub = Hub::new(None);
    let mut only = serving(&hub, "email:work", &["fake:mail"]);
    let evidence = evidence("fake:mail");
    let compose = |account: Option<&str>| MailCommand::Compose {
        account: account.map(str::to_owned),
        to: vec!["a@example.com".into()],
        text: "ask about Friday".into(),
    };
    let pending = hub.mail_command(evidence.clone(), compose(None));
    let answer = async {
        let (command, _, reply) = chat_command(only.requests.recv().await.unwrap());
        assert_eq!(command, compose(None), "the only account is the default");
        reply.send(MailReply::Text("drafted".into())).unwrap();
    };
    let (text, ()) = tokio::join!(pending, answer);
    assert_eq!(text, "drafted");

    let mut other = serving(&hub, "email:other", &["fake:mail"]);
    assert_eq!(
        hub.mail_command(evidence.clone(), compose(None)).await,
        "Say which account: mail compose ACCOUNT ADDRESS what to write.",
        "several accounts and no name"
    );
    assert!(only.requests.is_empty() && other.requests.is_empty());
    assert_eq!(
        hub.mail_command(evidence.clone(), compose(Some("missing")))
            .await,
        "No mail account by that name takes actions here; nothing was done."
    );

    let pending = hub.mail_command(evidence, compose(Some("other")));
    let answer = async {
        let (command, _, reply) = chat_command(other.requests.recv().await.unwrap());
        assert_eq!(command, compose(Some("other")));
        reply.send(MailReply::Text("from other".into())).unwrap();
    };
    let (text, ()) = tokio::join!(pending, answer);
    assert_eq!(text, "from other");
    assert!(
        only.requests.is_empty(),
        "the account that was not named does nothing"
    );
}

#[tokio::test]
async fn deny_all_and_status_go_to_every_account_on_the_route() {
    let hub = Hub::new(None);
    let mut alpha = serving(&hub, "email:alpha", &["fake:mail"]);
    let mut zeta = serving(&hub, "email:zeta", &["fake:mail", "fake:other"]);
    let elsewhere = serving(&hub, "email:elsewhere", &["fake:other"]);
    let evidence = evidence("fake:mail");
    for command in [MailCommand::DenyAll, MailCommand::Status] {
        let pending = hub.mail_command(evidence.clone(), command.clone());
        let expected = command.clone();
        let answers = async {
            let (got, _, reply) = chat_command(alpha.requests.recv().await.unwrap());
            assert_eq!(got, expected);
            reply.send(MailReply::Text("alpha line".into())).unwrap();
            let (got, _, reply) = chat_command(zeta.requests.recv().await.unwrap());
            assert_eq!(got, expected);
            reply.send(MailReply::Text("zeta line".into())).unwrap();
        };
        let (text, ()) = tokio::join!(pending, answers);
        assert_eq!(
            text, "alpha line\nzeta line",
            "accounts on the route answer in component order"
        );
        assert!(
            elsewhere.requests.is_empty(),
            "an account that does not report here is not asked"
        );
    }
}

#[tokio::test]
async fn a_revision_or_handle_goes_only_to_an_account_that_reports_to_the_chat() {
    let hub = Hub::new(None);
    let mut here = serving(&hub, "email:here", &["fake:mail"]);
    let elsewhere = serving(&hub, "email:elsewhere", &["fake:other"]);
    assert!(elsewhere.registration.claim_code("Q7M2KD"));
    assert!(elsewhere.registration.claim_handle("4K7P"));
    let evidence = evidence("fake:mail");
    let commands = [
        (
            MailCommand::Revise {
                code: "Q7M2KD".into(),
                text: "shorter".into(),
            },
            "No mail action has code Q7M2KD.",
        ),
        (
            MailCommand::Reply {
                handle: "4K7P".into(),
                text: String::new(),
            },
            "No reported mail has handle #4K7P; handles work for mail reported in the last \
             days only.",
        ),
        (
            MailCommand::Message {
                handle: "4K7P".into(),
                action: crate::mail_chat::MessageAction::Trash,
            },
            "No reported mail has handle #4K7P; handles work for mail reported in the last \
             days only.",
        ),
    ];
    for (command, expected) in commands {
        assert_eq!(
            hub.mail_command(evidence.clone(), command).await,
            expected,
            "another chat's code or handle is neither used nor confirmed"
        );
    }
    assert!(elsewhere.requests.is_empty());
    assert!(here.requests.try_recv().is_err());

    // From the chat it reports to, the same handle reaches it.
    let pending = hub.mail_command(
        ChatEvidence {
            route: "fake:other".into(),
            ..evidence
        },
        MailCommand::Reply {
            handle: "4K7P".into(),
            text: String::new(),
        },
    );
    let mut elsewhere = elsewhere;
    let answer = async {
        let (_, _, reply) = chat_command(elsewhere.requests.recv().await.unwrap());
        reply.send(MailReply::Text("preparing".into())).unwrap();
    };
    let (text, ()) = tokio::join!(pending, answer);
    assert_eq!(text, "preparing");
}

#[tokio::test(start_paused = true)]
async fn an_account_that_does_not_answer_within_ten_seconds_is_still_working() {
    let hub = Hub::new(None);
    let mut quick = serving(&hub, "email:quick", &["fake:mail"]);
    let mut slow = serving(&hub, "email:slow", &["fake:mail"]);
    assert!(quick.registration.claim_code("Q7M2KD"));
    assert!(slow.registration.claim_code("XYZ789"));
    let pending = hub.mail_command(
        evidence("fake:mail"),
        MailCommand::Approve(vec!["Q7M2KD".into(), "XYZ789".into()]),
    );
    let answers = async {
        let (_, _, reply) = chat_command(quick.requests.recv().await.unwrap());
        reply.send(MailReply::Text("quick: done".into())).unwrap();
        let request = slow.requests.recv().await.unwrap();
        tokio::time::sleep(Duration::from_secs(11)).await;
        drop(request);
    };
    let (text, ()) = tokio::join!(pending, answers);
    assert_eq!(text, format!("quick: done\n{STILL_WORKING_REPLY}"));
}

#[tokio::test]
async fn a_dropped_authority_says_mail_actions_are_not_running() {
    let hub = Hub::new(None);
    let mut live = serving(&hub, "email:live", &["fake:mail"]);
    let gone = serving(&hub, "email:gone", &["fake:mail"]);
    assert!(live.registration.claim_code("Q7M2KD"));
    assert!(gone.registration.claim_code("XYZ789"));
    drop(gone.requests);
    let pending = hub.mail_command(
        evidence("fake:mail"),
        MailCommand::Approve(vec!["Q7M2KD".into(), "XYZ789".into()]),
    );
    let answer = async {
        let (_, _, reply) = chat_command(live.requests.recv().await.unwrap());
        reply.send(MailReply::Text("live: done".into())).unwrap();
    };
    let (text, ()) = tokio::join!(pending, answer);
    assert_eq!(
        text,
        format!("{}\nlive: done", crate::mail_chat::NOT_RUNNING_REPLY)
    );

    let mut dropped = serving(&hub, "email:dropped", &["fake:mail"]);
    assert!(dropped.registration.claim_code("M2KDQ7"));
    let pending = hub.mail_command(
        evidence("fake:mail"),
        MailCommand::Approve(vec!["M2KDQ7".into()]),
    );
    let abandon = async {
        drop(dropped.requests.recv().await.unwrap());
    };
    let (text, ()) = tokio::join!(pending, abandon);
    assert_eq!(
        text,
        crate::mail_chat::NOT_RUNNING_REPLY,
        "an authority that drops the request never answered"
    );
}

#[test]
fn claim_code_refuses_a_code_another_running_account_holds() {
    let hub = Hub::new(None);
    let first = serving(&hub, "email:first", &["fake:mail"]);
    let second = serving(&hub, "email:second", &["fake:mail"]);
    assert!(first.registration.claim_code("Q7M2KD"));
    assert!(
        !second.registration.claim_code("Q7M2KD"),
        "a live code stays with the account that claimed it"
    );
    assert!(
        !first.registration.claim_code("Q7M2KD"),
        "claiming a code the account already holds is not a new claim"
    );
    first
        .registration
        .forget_codes(&["Q7M2KD".into(), "NOCODE".into()]);
    assert!(
        second.registration.claim_code("Q7M2KD"),
        "a forgotten code can be claimed again"
    );
    assert!(!first.registration.claim_code("Q7M2KD"));
}

#[test]
fn a_replaced_registration_cannot_change_the_new_accounts_entry() {
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "fake:mail", Some("owner".into()));
    let (_bridge, _notices) = link.register_as(Purpose::Mail);
    let old = serving(&hub, "email:work", &["fake:mail"]);
    assert!(old.registration.claim_code("Q7M2KD"));
    assert!(old.registration.claim_handle("4K7P"));
    old.registration.set_counts(scv_protocol::MailCounts {
        seen_today: 9,
        ..Default::default()
    });
    assert!(old.registration.begin_execution());
    assert_eq!(
        old.registration.chat_owner("fake:mail").as_deref(),
        Some("owner")
    );

    let new = serving(&hub, "email:work", &["fake:mail"]);
    assert_eq!(
        hub.mail_executing(),
        0,
        "replacing the registration drops what the old one counted"
    );
    assert_eq!(hub.mail_counts("email:work").unwrap().seen_today, 0);
    assert!(!old.registration.claim_code("XYZ789"));
    assert!(!old.registration.claim_handle("M2KD"));
    assert!(!old.registration.begin_execution());
    old.registration.set_counts(scv_protocol::MailCounts {
        seen_today: 3,
        ..Default::default()
    });
    old.registration.forget_codes(&["XYZ789".into()]);
    old.registration.forget_handles(&["M2KD".into()]);
    old.registration.end_execution();
    new.registration.set_counts(scv_protocol::MailCounts {
        seen_today: 4,
        ..Default::default()
    });
    assert!(new.registration.claim_code("XYZ789"));
    assert!(new.registration.claim_handle("M2KD"));
    assert_eq!(hub.mail_counts("email:work").unwrap().seen_today, 4);
    assert!(new.registration.begin_execution());
    assert_eq!(hub.mail_executing(), 1);
    drop(old);
    assert_eq!(
        hub.mail_counts("email:work").unwrap().seen_today,
        4,
        "dropping the old registration leaves the new one"
    );
    assert_eq!(hub.mail_executing(), 1);
    assert_eq!(new.registration.chat_owner("fake:other"), None);
}

#[test]
fn begin_execution_is_refused_while_draining_and_counted_otherwise() {
    let hub = Hub::new(None);
    let account = serving(&hub, "email:work", &["fake:mail"]);
    assert!(!hub.mail_draining());
    assert!(account.registration.begin_execution());
    assert!(account.registration.begin_execution());
    assert_eq!(hub.mail_executing(), 2);
    account.registration.end_execution();
    assert_eq!(hub.mail_executing(), 1);
    account.registration.end_execution();
    account.registration.end_execution();
    assert_eq!(
        hub.mail_executing(),
        0,
        "ending more than began stays at zero"
    );

    hub.set_mail_drain(true);
    assert!(hub.mail_draining());
    assert!(
        !account.registration.begin_execution(),
        "a drain refuses a new action"
    );
    assert_eq!(hub.mail_executing(), 0);
    hub.set_mail_drain(false);
    assert!(account.registration.begin_execution());
    assert_eq!(hub.mail_executing(), 1);
    account.registration.end_execution();
    assert_eq!(hub.mail_executing(), 0);
}

#[test]
fn seeing_no_mail_action_after_the_drain_flag_means_none_starts() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let hub = Hub::new(None);
    let account = Arc::new(serving(&hub, "email:work", &["fake:mail"]).registration);
    let stop = Arc::new(AtomicBool::new(false));
    let successes = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let account = Arc::clone(&account);
        let stop = Arc::clone(&stop);
        let successes = Arc::clone(&successes);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if account.begin_execution() {
                    successes.fetch_add(1, Ordering::SeqCst);
                    account.end_execution();
                }
            }
        }));
    }
    let mut observed = 0;
    for _ in 0..300 {
        hub.set_mail_drain(true);
        if hub.mail_executing() == 0 {
            observed += 1;
            let seen = successes.load(Ordering::SeqCst);
            assert!(
                !account.begin_execution(),
                "begin_execution succeeded after a drain that saw nothing executing"
            );
            std::thread::sleep(Duration::from_millis(1));
            assert_eq!(
                hub.mail_executing(),
                0,
                "an action was counted after the drain flag was up and none were"
            );
            assert_eq!(
                successes.load(Ordering::SeqCst),
                seen,
                "begin_execution succeeded after a drain that saw nothing executing"
            );
        }
        hub.set_mail_drain(false);
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(observed > 0, "the drain never observed an idle account");
    assert!(
        successes.load(Ordering::SeqCst) > 0,
        "no action started while the flag was down"
    );
    assert_eq!(hub.mail_executing(), 0);
}

#[tokio::test(start_paused = true)]
async fn mail_order_reports_not_running_no_answer_or_the_reply() {
    let hub = Hub::new(None);
    assert_eq!(
        hub.mail_order("email:missing", MailOrder::List).await,
        Err(MailOrderError::NotRunning)
    );
    let idle = hub.register_mail("email:idle", vec!["fake:mail".into()]);
    assert_eq!(
        hub.mail_order("email:idle", MailOrder::List).await,
        Err(MailOrderError::NotRunning),
        "registered without an authority"
    );
    drop(idle);

    let mut account = serving(&hub, "email:work", &["fake:mail"]);
    let order = hub.mail_order("email:work", MailOrder::List);
    let hold = async {
        let request = account.requests.recv().await.unwrap();
        let MailWork::Order(MailOrder::List) = &request.work else {
            panic!("mail status asks for the list");
        };
        tokio::time::sleep(Duration::from_secs(11)).await;
        drop(request);
    };
    let (result, ()) = tokio::join!(order, hold);
    assert_eq!(result, Err(MailOrderError::NoAnswer));

    let (commands, mut requests) = mpsc::channel(1);
    account.registration.serve(commands);
    let action = scv_protocol::MailAction {
        account: "email:work".into(),
        id: format!("a{}", "ab".repeat(16)),
        kind: "draft".into(),
        state: "open".into(),
        created_unix_seconds: 10,
        expires_unix_seconds: None,
    };
    let order = hub.mail_order("email:work", MailOrder::List);
    let listed = action.clone();
    let answer = async move {
        let request = requests.recv().await.unwrap();
        request
            .reply
            .send(MailReply::Actions(vec![listed]))
            .unwrap();
        requests
    };
    let (result, mut requests) = tokio::join!(order, answer);
    assert_eq!(result, Ok(MailReply::Actions(vec![action])));

    let order = hub.mail_order(
        "email:work",
        MailOrder::Cancel {
            action: Some(format!("a{}", "cd".repeat(16))),
        },
    );
    let answer = async {
        let request = requests.recv().await.unwrap();
        let MailWork::Order(MailOrder::Cancel { action: Some(id) }) = &request.work else {
            panic!("cancel names the action");
        };
        assert_eq!(id.len(), 33);
        request
            .reply
            .send(MailReply::Text("Withdrawn.".into()))
            .unwrap();
    };
    let (result, ()) = tokio::join!(order, answer);
    assert_eq!(result, Ok(MailReply::Text("Withdrawn.".into())));

    drop(requests);
    assert_eq!(
        hub.mail_order("email:work", MailOrder::List).await,
        Err(MailOrderError::NotRunning),
        "the authority is gone"
    );
}

#[test]
fn mail_authorities_lists_only_accounts_that_take_actions() {
    let hub = Hub::new(None);
    let _idle = hub.register_mail("email:idle", vec!["fake:mail".into()]);
    let alpha = serving(&hub, "email:zeta", &["fake:mail"]);
    let zeta = serving(&hub, "email:alpha", &["fake:other"]);
    assert_eq!(
        hub.mail_authorities(),
        vec!["email:alpha".to_owned(), "email:zeta".to_owned()],
        "sorted, and an account with no authority is absent"
    );
    drop(alpha);
    assert_eq!(hub.mail_authorities(), vec!["email:alpha".to_owned()]);
    drop(zeta);
    assert!(hub.mail_authorities().is_empty());
}
