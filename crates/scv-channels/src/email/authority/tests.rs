//! Unit tests for `src/email/authority.rs`.

use super::*;
use crate::email::content::{
    ActionContent, ActionKind, CONTENT_VERSION, Display, Folder, FolderRole, Form, Origin, Source,
};
use crate::email::ledger::actions::{
    ActionState, MAX_REQUESTS, NewAction, Policy, World, preview_key,
};
use crate::email::ledger::{Counts, Ledger};
use crate::email::plan::SendClass;
use crate::email::settings::{ActionMode, ActionSettings};
use crate::email::source::SourceRef;
use crate::email::test_support::{self, account, ledger};
use crate::hub::KeyedOutcome;
use crate::state::Credentials as _;
use std::sync::Mutex;

const ROUTE: &str = "fake:mail";
const OWNER: &str = "owner";
/// Another chat an account may report to, with an owner of its own.
const SECOND: &str = "fake:second";
const SECOND_OWNER: &str = "second-owner";
const HANDLE: &str = "4K7P";
const START: u64 = test_support::START;
/// An address and a subject the answers must not repeat.
const SECRET_ADDRESS: &str = "secret-owner@hidden.example";
const SECRET_SUBJECT: &str = "SecretSubjectQ";

fn modes(on: bool) -> ActionSettings {
    let mode = if on {
        ActionMode::Approve
    } else {
        ActionMode::Off
    };
    ActionSettings {
        draft: mode,
        send: mode,
        forward: mode,
        archive: mode,
        mark_read: mode,
        trash: mode,
        spam: mode,
        ..ActionSettings::default()
    }
}

fn policy(home: &std::path::Path, actions: ActionSettings, routes: &[&str]) -> Policy {
    let directory = home.join("state/mail/default/actions");
    std::fs::create_dir_all(&directory).unwrap();
    Policy {
        actions,
        routes: routes.iter().map(|route| (*route).to_owned()).collect(),
        fingerprint: account().fingerprint().unwrap(),
        account: "default".into(),
        audit: home.join("state/mail/default/audit.jsonl"),
        content: crate::email::content::ContentStore::new(directory),
        tombstone_seconds: 30 * 86_400,
        unknown_keep_seconds: 3 * 86_400,
        possible: Mutex::new(crate::email::ledger::actions::ALL_KINDS.to_vec()),
    }
}

struct FakeWorld;

impl World for FakeWorld {
    fn owner(&self, route: &str) -> Option<String> {
        match route {
            ROUTE => Some(OWNER.to_owned()),
            SECOND => Some(SECOND_OWNER.to_owned()),
            _ => None,
        }
    }

    fn delivered(&self, _route: &str, _key: &str) -> Option<KeyedOutcome> {
        None
    }
}

struct Bench {
    ledger: Ledger,
    clock: test_support::FixedClock,
    world: FakeWorld,
}

impl Bench {
    fn new(actions: ActionSettings) -> Self {
        Self::reporting_to(actions, &[ROUTE])
    }

    fn reporting_to(actions: ActionSettings, routes: &[&str]) -> Self {
        let home = tempfile::tempdir().unwrap();
        // The directory outlives the ledger; the temp dir is leaked for the
        // process so a spawned `serve` can keep the ledger.
        let home = Box::leak(Box::new(home));
        let ledger = ledger(home.path()).with_actions(policy(home.path(), actions, routes));
        Self {
            ledger,
            clock: test_support::FixedClock::new(),
            world: FakeWorld,
        }
    }

    fn bare() -> Ledger {
        let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        ledger(home.path())
    }

    fn authority(&self) -> Authority<'_> {
        Authority {
            ledger: &self.ledger,
            world: &self.world,
            component: "email:default",
            clock: &self.clock,
        }
    }

    async fn remember_handle(&self) {
        let handle = crate::email::ledger::actions::Handle {
            handle: HANDLE.into(),
            source: SourceRef::Imap {
                mailbox: "INBOX".into(),
                uidvalidity: 7,
                uid: 42,
            },
            identity: "identity".into(),
            at: START,
        };
        self.ledger
            .commit(|state| state.handles.push(handle))
            .await
            .unwrap();
    }

    fn trash(&self) -> ActionContent {
        ActionContent {
            v: CONTENT_VERSION,
            id: crate::email::content::new_id(),
            account: "default".into(),
            fingerprint: self.ledger.actions_policy().unwrap().fingerprint.clone(),
            kind: ActionKind::Trash,
            group: None,
            origin: Origin::Owner {
                route: ROUTE.into(),
                message_id: "request".into(),
            },
            source: Some(Source {
                reference: SourceRef::Imap {
                    mailbox: "INBOX".into(),
                    uidvalidity: 7,
                    uid: 42,
                },
                identity: "identity".into(),
                locator: "locator".into(),
                message_id: None,
            }),
            display: Some(Display {
                handle: HANDLE.into(),
                from_address: SECRET_ADDRESS.into(),
                from_name: "Alice".into(),
                subject: SECRET_SUBJECT.into(),
            }),
            folder: Some(Folder {
                role: FolderRole::Trash,
                name: "Trash".into(),
            }),
            message: None,
            created_at: START,
            hard_expiry: START + 72 * 3600,
            digest: String::new(),
        }
        .seal_digest()
    }

    fn draft(&self) -> ActionContent {
        let mut content = self.trash();
        content.id = crate::email::content::new_id();
        content.kind = ActionKind::Draft;
        content.group = None;
        content.message = Some(crate::email::content::Outgoing {
            form: Form::Reply,
            from: crate::email::content::Mailbox {
                name: String::new(),
                address: "me@example.com".into(),
            },
            to: vec![SECRET_ADDRESS.into()],
            cc: Vec::new(),
            subject: SECRET_SUBJECT.into(),
            body: "Thanks.".into(),
            in_reply_to: None,
            references: Vec::new(),
            message_id: "<out@example.com>".into(),
            sent_copy: crate::email::settings::SentCopy::Provider,
            notes: Vec::new(),
        });
        content.folder = Some(Folder {
            role: FolderRole::Drafts,
            name: "Drafts".into(),
        });
        content.seal_digest()
    }

    /// Propose `content` under `code` and, when `open` , deliver it for approval.
    async fn propose(&self, content: &ActionContent, code: &str, open: bool) {
        self.ledger
            .actions_policy()
            .unwrap()
            .content
            .write_new(content)
            .unwrap();
        self.ledger
            .propose(
                None,
                vec![NewAction::new(
                    content,
                    format!("owner:{ROUTE}:propose-{code}:0"),
                    code.into(),
                )],
                (preview_key(&content.id, 1), "preview".into()),
                Vec::new(),
                self.clock.now(),
            )
            .await
            .unwrap()
            .unwrap();
        if open {
            self.deliver().await;
        }
    }

    async fn deliver(&self) {
        let now = self.clock.now();
        let seqs = self
            .ledger
            .snapshot()
            .queue
            .iter()
            .map(|item| item.seq)
            .collect();
        let batch = self
            .ledger
            .begin_batch(
                seqs,
                SendClass::Response,
                "text".into(),
                Counts::default(),
                now,
            )
            .await
            .unwrap();
        self.ledger.batch_stored(ROUTE, OWNER, now).await.unwrap();
        self.ledger
            .settle_watched(&[(batch.key, now * 1000)], &[], &mut || None)
            .await
            .unwrap();
    }

    async fn ask(&self, command: MailCommand, message: &str, sent_ms: Option<u64>) -> String {
        let MailReply::Text(text) = self
            .authority()
            .handle(MailWork::Chat {
                command,
                evidence: evidence(message, sent_ms),
            })
            .await
            .unwrap()
        else {
            panic!("a chat command answered with a list");
        };
        assert_no_mail(&text);
        text
    }
}

fn evidence(message: &str, sent_ms: Option<u64>) -> ChatEvidence {
    ChatEvidence {
        route: ROUTE.into(),
        peer: OWNER.into(),
        message_id: message.into(),
        sent_ms,
    }
}

fn assert_no_mail(text: &str) {
    assert!(
        !text.contains(SECRET_ADDRESS) && !text.contains(SECRET_SUBJECT),
        "an answer echoed mail: {text}"
    );
}

#[tokio::test]
async fn approve_deny_and_status_name_codes_and_never_the_mail() {
    let bench = Bench::new(modes(true));
    let keep = bench.trash();
    let drop = bench.trash();
    bench.propose(&keep, "KEEP22", true).await;
    bench.propose(&drop, "DROP22", true).await;
    let sent_ms = Some(bench.clock.now() * 1000);

    let status = bench.ask(MailCommand::Status, "status-1", None).await;
    assert!(status.contains("KEEP22"), "{status}");
    assert!(status.contains("DROP22"), "{status}");
    assert!(status.contains("moving #4K7P to Trash"), "{status}");
    assert!(status.contains("2 waiting"), "{status}");

    let denied = bench
        .ask(MailCommand::Deny(vec!["DROP22".into()]), "deny-1", None)
        .await;
    assert_eq!(denied, "Denied DROP22 (moving #4K7P to Trash).");
    assert_eq!(bench.entry_state(&drop.id), ActionState::Denied);

    let approved = bench
        .ask(
            MailCommand::Approve(vec!["KEEP22".into()]),
            "approve-1",
            sent_ms,
        )
        .await;
    assert_eq!(approved, "Approved KEEP22: moving #4K7P to Trash.");
    assert_eq!(bench.entry_state(&keep.id), ActionState::Approved);

    let again = bench
        .ask(
            MailCommand::Approve(vec!["KEEP22".into()]),
            "approve-1",
            sent_ms,
        )
        .await;
    assert_eq!(again, "KEEP22 was already approved.");
    assert_eq!(bench.entry_state(&keep.id), ActionState::Approved);

    // An approved action that has not started is still waiting, so deny all
    // withdraws it together with the one still proposed.
    let extra = bench.trash();
    bench.propose(&extra, "MORE22", false).await;
    let all = bench.ask(MailCommand::DenyAll, "deny-all", None).await;
    assert_eq!(all, "Denied the 2 actions that were waiting.");
    assert_eq!(bench.entry_state(&extra.id), ActionState::Denied);
    assert_eq!(bench.entry_state(&keep.id), ActionState::Denied);
}

impl Bench {
    fn entry_state(&self, id: &str) -> ActionState {
        self.ledger
            .snapshot()
            .actions
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap()
            .state
    }
}

#[tokio::test]
async fn a_kind_that_is_off_is_refused_and_nothing_is_queued() {
    let bench = Bench::new(modes(false));
    bench.remember_handle().await;
    let cases = [
        (
            MailCommand::Reply {
                handle: HANDLE.into(),
                text: "hello".into(),
            },
            "replies",
        ),
        (
            MailCommand::Forward {
                handle: HANDLE.into(),
                to: vec![SECRET_ADDRESS.into()],
                note: String::new(),
            },
            "forwards",
        ),
        (
            MailCommand::Compose {
                account: None,
                to: vec![SECRET_ADDRESS.into()],
                text: "write".into(),
            },
            "new mail",
        ),
        (
            MailCommand::Message {
                handle: HANDLE.into(),
                action: MessageAction::Archive,
            },
            "archiving",
        ),
        (
            MailCommand::Message {
                handle: HANDLE.into(),
                action: MessageAction::Read,
            },
            "marking mail read",
        ),
        (
            MailCommand::Message {
                handle: HANDLE.into(),
                action: MessageAction::Trash,
            },
            "moving mail to Trash",
        ),
        (
            MailCommand::Message {
                handle: HANDLE.into(),
                action: MessageAction::Spam,
            },
            "moving mail to Spam",
        ),
    ];
    for (index, (command, what)) in cases.into_iter().enumerate() {
        let answer = bench.ask(command, &format!("off-{index}"), None).await;
        assert!(
            answer.contains(&format!("does not take {what}"))
                && answer.contains("nothing was done"),
            "{answer}"
        );
    }
    assert!(
        bench.ledger.snapshot().requests.is_empty(),
        "an off kind queued a request"
    );
}

#[tokio::test]
async fn an_unknown_handle_and_a_code_that_is_not_a_draft_are_refused() {
    let bench = Bench::new(modes(true));
    let missing = bench
        .ask(
            MailCommand::Reply {
                handle: "ZZZZ".into(),
                text: String::new(),
            },
            "missing",
            None,
        )
        .await;
    assert_eq!(
        missing,
        "No reported mail has handle #ZZZZ any more; nothing was done."
    );
    for action in [
        MessageAction::Archive,
        MessageAction::Read,
        MessageAction::Trash,
        MessageAction::Spam,
    ] {
        let answer = bench
            .ask(
                MailCommand::Message {
                    handle: "ZZZZ".into(),
                    action,
                },
                "missing-move",
                None,
            )
            .await;
        assert!(answer.contains("#ZZZZ"), "{answer}");
    }
    let no_code = bench
        .ask(
            MailCommand::Revise {
                code: "ZZZZZZ".into(),
                text: "shorter".into(),
            },
            "revise-missing",
            None,
        )
        .await;
    assert_eq!(no_code, "No draft waiting for your answer has code ZZZZZZ.");

    let trash = bench.trash();
    bench.propose(&trash, "TRASH2", false).await;
    let not_draft = bench
        .ask(
            MailCommand::Revise {
                code: "TRASH2".into(),
                text: "shorter".into(),
            },
            "revise-trash",
            None,
        )
        .await;
    assert_eq!(
        not_draft,
        "TRASH2 is not a reply, forward, or new mail waiting for your answer."
    );
    assert!(bench.ledger.snapshot().requests.is_empty());
}

#[tokio::test]
async fn bad_recipients_are_refused_without_echoing_the_addresses() {
    let mut actions = modes(true);
    actions.recipient_domains = vec!["example.com".into()];
    actions.max_recipients = 1;
    let bench = Bench::new(actions);
    bench.remember_handle().await;

    let not_plain = bench
        .ask(
            MailCommand::Forward {
                handle: HANDLE.into(),
                to: vec!["Bob <bob@hidden.example>".into()],
                note: String::new(),
            },
            "plain",
            None,
        )
        .await;
    assert!(not_plain.contains("plain addresses"), "{not_plain}");
    assert!(!not_plain.contains("bob@hidden.example"), "{not_plain}");
    assert!(!not_plain.contains("hidden.example"), "{not_plain}");

    let outside = bench
        .ask(
            MailCommand::Compose {
                account: None,
                to: vec!["pat@other.test".into()],
                text: "hello".into(),
            },
            "domain",
            None,
        )
        .await;
    assert!(outside.contains("recipient_domains"), "{outside}");
    assert!(!outside.contains("pat@other.test"), "{outside}");
    assert!(!outside.contains("other.test"), "{outside}");

    let too_many = bench
        .ask(
            MailCommand::Forward {
                handle: HANDLE.into(),
                to: vec![
                    "a@example.com".into(),
                    "b@example.com".into(),
                    "c@example.com".into(),
                ],
                note: String::new(),
            },
            "many",
            None,
        )
        .await;
    assert!(too_many.contains("plain addresses"), "{too_many}");
    for address in ["a@example.com", "b@example.com", "c@example.com"] {
        assert!(!too_many.contains(address), "{address} in {too_many}");
    }
    assert!(bench.ledger.snapshot().requests.is_empty());
}

#[tokio::test]
async fn a_request_is_queued_once_and_the_queue_fills() {
    let bench = Bench::new(modes(true));
    bench.remember_handle().await;
    let draft = bench.draft();
    bench.propose(&draft, "DRAFT2", false).await;

    let first = bench
        .ask(
            MailCommand::Reply {
                handle: HANDLE.into(),
                text: "say thanks".into(),
            },
            "same-message",
            None,
        )
        .await;
    assert!(
        first.contains("Preparing a reply to #4K7P") && first.contains("preview"),
        "{first}"
    );
    let duplicate = bench
        .ask(
            MailCommand::Reply {
                handle: HANDLE.into(),
                text: "say it again".into(),
            },
            "same-message",
            None,
        )
        .await;
    assert_eq!(duplicate, "That request is already being prepared.");
    assert_eq!(bench.ledger.snapshot().requests.len(), 1);

    let more = [
        MailCommand::Forward {
            handle: HANDLE.into(),
            to: vec!["bob@example.com".into()],
            note: "note".into(),
        },
        MailCommand::Compose {
            account: None,
            to: vec!["bob@example.com".into()],
            text: "write this".into(),
        },
        MailCommand::Revise {
            code: "DRAFT2".into(),
            text: "shorter".into(),
        },
        MailCommand::Message {
            handle: HANDLE.into(),
            action: MessageAction::Archive,
        },
        MailCommand::Message {
            handle: HANDLE.into(),
            action: MessageAction::Read,
        },
        MailCommand::Message {
            handle: HANDLE.into(),
            action: MessageAction::Trash,
        },
        MailCommand::Message {
            handle: HANDLE.into(),
            action: MessageAction::Spam,
        },
    ];
    for (index, command) in more.into_iter().enumerate() {
        let answer = bench.ask(command, &format!("queued-{index}"), None).await;
        assert!(
            answer.contains("Preparing") && answer.contains("preview"),
            "{answer}"
        );
        assert_no_mail(&answer);
        assert!(!answer.contains("bob@example.com"), "{answer}");
    }
    assert_eq!(bench.ledger.snapshot().requests.len(), MAX_REQUESTS);
    let status = bench.ask(MailCommand::Status, "status-full", None).await;
    assert!(status.contains("being prepared"), "{status}");

    let full = bench
        .ask(
            MailCommand::Reply {
                handle: HANDLE.into(),
                text: "one more".into(),
            },
            "overflow",
            None,
        )
        .await;
    assert_eq!(
        full,
        "Too many requests are being prepared; nothing was done. Try again in a few minutes."
    );
    assert_eq!(bench.ledger.snapshot().requests.len(), MAX_REQUESTS);
}

#[tokio::test]
async fn list_and_cancel_orders_do_not_approve() {
    let bench = Bench::new(modes(true));
    let content = bench.trash();
    bench.propose(&content, "TRASH2", true).await;

    let MailReply::Actions(listed) = bench
        .authority()
        .handle(MailWork::Order(crate::hub::MailOrder::List))
        .await
        .unwrap()
    else {
        panic!("list did not answer with actions");
    };
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, content.id);
    assert_eq!(listed[0].kind, "trash");
    assert_eq!(listed[0].state, "open");
    assert_eq!(listed[0].account, "email:default");

    let MailReply::Text(text) = bench
        .authority()
        .handle(MailWork::Order(crate::hub::MailOrder::Cancel {
            action: Some(content.id.clone()),
        }))
        .await
        .unwrap()
    else {
        panic!("cancel answered with a list");
    };
    assert_eq!(text, "Cancelled 1 mail action(s).");
    assert_eq!(bench.entry_state(&content.id), ActionState::Cancelled);
    assert_no_mail(&text);
}

#[tokio::test]
async fn serve_answers_and_a_dropped_waiter_is_told_in_the_mail_chat() {
    let bench = Box::leak(Box::new(Bench::new(modes(true))));
    let (commands, requests) = tokio::sync::mpsc::channel(4);
    let authority = Authority {
        ledger: &bench.ledger,
        world: &bench.world,
        component: "email:default",
        clock: &bench.clock,
    };
    tokio::spawn(async move {
        let _ = authority.serve(requests).await;
    });

    let (reply, answer) = tokio::sync::oneshot::channel();
    commands
        .send(MailRequest {
            work: MailWork::Chat {
                command: MailCommand::Status,
                evidence: evidence("status", None),
            },
            reply,
        })
        .await
        .unwrap();
    let MailReply::Text(text) = answer.await.unwrap() else {
        panic!("status was not text");
    };
    assert!(text.contains("waiting for your answer"), "{text}");
    assert_no_mail(&text);

    let (reply, answer) = tokio::sync::oneshot::channel();
    drop(answer);
    commands
        .send(MailRequest {
            work: MailWork::Chat {
                command: MailCommand::Status,
                evidence: evidence("late", None),
            },
            reply,
        })
        .await
        .unwrap();
    let mut late = None;
    for _ in 0..100 {
        late = bench
            .ledger
            .snapshot()
            .queue
            .into_iter()
            .find(|item| item.key.starts_with("response:late:"));
        if late.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let late = late.expect("a dropped waiter was not told in the mail chat");
    assert_eq!(late.text, text);
    assert_no_mail(&late.text);
}

#[tokio::test]
async fn an_account_without_a_policy_says_actions_are_not_running() {
    let ledger = Bench::bare();
    let clock = test_support::FixedClock::new();
    let world = FakeWorld;
    let authority = Authority {
        ledger: &ledger,
        world: &world,
        component: "email:default",
        clock: &clock,
    };
    for command in [
        MailCommand::Status,
        MailCommand::Approve(vec!["ABCD22".into()]),
        MailCommand::Reply {
            handle: HANDLE.into(),
            text: String::new(),
        },
    ] {
        let MailReply::Text(text) = authority
            .handle(MailWork::Chat {
                command,
                evidence: evidence("none", Some(START * 1000)),
            })
            .await
            .unwrap()
        else {
            panic!("not text");
        };
        assert_eq!(text, crate::mail_chat::NOT_RUNNING_REPLY);
        assert_no_mail(&text);
    }
}

#[tokio::test]
async fn deny_from_another_chat_or_owner_changes_nothing() {
    let bench = Bench::new(modes(true));
    let action = bench.trash();
    bench.propose(&action, "KEEP22", true).await;
    let before = bench.ledger.snapshot().actions;

    let other_route = ChatEvidence {
        route: "fake:other".into(),
        peer: "stranger".into(),
        message_id: "other-1".into(),
        sent_ms: None,
    };
    let other_owner = ChatEvidence {
        peer: "stranger".into(),
        message_id: "other-2".into(),
        ..evidence("other-2", None)
    };
    for evidence in [other_route, other_owner] {
        for command in [
            MailCommand::Deny(vec!["KEEP22".into()]),
            MailCommand::DenyAll,
        ] {
            let MailReply::Text(text) = bench
                .authority()
                .handle(MailWork::Chat {
                    command,
                    evidence: evidence.clone(),
                })
                .await
                .unwrap()
            else {
                panic!("a chat command answered with a list");
            };
            assert!(
                text.contains("another chat") || text == "Nothing was waiting.",
                "{text}"
            );
            assert_no_mail(&text);
        }
    }
    assert_eq!(bench.ledger.snapshot().actions, before);
    assert_eq!(bench.entry_state(&action.id), ActionState::Open);

    let denied = bench
        .ask(MailCommand::Deny(vec!["KEEP22".into()]), "deny-owner", None)
        .await;
    assert_eq!(denied, "Denied KEEP22 (moving #4K7P to Trash).");
    assert_eq!(bench.entry_state(&action.id), ActionState::Denied);
}

/// Every command that prepares an action, revising `code`.
fn preparing_commands(code: &str) -> Vec<MailCommand> {
    vec![
        MailCommand::Reply {
            handle: HANDLE.into(),
            text: "say thanks".into(),
        },
        MailCommand::Forward {
            handle: HANDLE.into(),
            to: vec!["bob@example.com".into()],
            note: "note".into(),
        },
        MailCommand::Compose {
            account: None,
            to: vec!["bob@example.com".into()],
            text: "write this".into(),
        },
        MailCommand::Revise {
            code: code.into(),
            text: "add these bank details".into(),
        },
        MailCommand::Message {
            handle: HANDLE.into(),
            action: MessageAction::Trash,
        },
    ]
}

#[tokio::test]
async fn only_the_owner_of_a_chat_the_account_reports_to_can_ask_for_an_action() {
    let bench = Bench::new(modes(true));
    bench.remember_handle().await;
    let draft = bench.draft();
    bench.propose(&draft, "DRAFT2", true).await;
    let before = bench.ledger.snapshot().actions;

    let not_a_route = ChatEvidence {
        route: "fake:other".into(),
        peer: "stranger".into(),
        message_id: "other-1".into(),
        sent_ms: None,
    };
    let not_its_owner = ChatEvidence {
        peer: "stranger".into(),
        ..evidence("other-2", None)
    };
    for evidence in [not_a_route, not_its_owner] {
        for command in preparing_commands("DRAFT2") {
            let MailReply::Text(text) = bench
                .authority()
                .handle(MailWork::Chat {
                    command,
                    evidence: evidence.clone(),
                })
                .await
                .unwrap()
            else {
                panic!("a chat command answered with a list");
            };
            assert_eq!(
                text,
                "email:default does not report to this chat; ask in its own mail chat. Nothing \
                 was done."
            );
        }
    }
    assert!(bench.ledger.snapshot().requests.is_empty());
    assert_eq!(bench.ledger.snapshot().actions, before);

    // The chat's own owner still can.
    for (index, command) in preparing_commands("DRAFT2").into_iter().enumerate() {
        let answer = bench.ask(command, &format!("owner-{index}"), None).await;
        assert!(answer.contains("Preparing"), "{answer}");
    }
    assert_eq!(bench.ledger.snapshot().requests.len(), 5);
}

#[tokio::test]
async fn a_draft_is_revised_only_from_the_chat_it_was_sent_to() {
    let bench = Bench::reporting_to(modes(true), &[ROUTE, SECOND]);
    let draft = bench.draft();
    bench.propose(&draft, "DRAFT2", true).await;
    let from_second = ChatEvidence {
        route: SECOND.into(),
        peer: SECOND_OWNER.into(),
        message_id: "second-1".into(),
        sent_ms: None,
    };
    let MailReply::Text(text) = bench
        .authority()
        .handle(MailWork::Chat {
            command: MailCommand::Revise {
                code: "DRAFT2".into(),
                text: "add these bank details".into(),
            },
            evidence: from_second,
        })
        .await
        .unwrap()
    else {
        panic!("a chat command answered with a list");
    };
    assert_eq!(
        text,
        "DRAFT2 was sent to another chat; revise it there. Nothing was done."
    );
    assert!(bench.ledger.snapshot().requests.is_empty());
    assert_eq!(bench.entry_state(&draft.id), ActionState::Open);

    let revised = bench
        .ask(
            MailCommand::Revise {
                code: "DRAFT2".into(),
                text: "shorter".into(),
            },
            "revise-here",
            None,
        )
        .await;
    assert!(revised.contains("Preparing the revised draft"), "{revised}");
}
