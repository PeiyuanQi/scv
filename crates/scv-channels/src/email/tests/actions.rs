//! End-to-end mail actions: a report, an owner command, one approval, one effect.

use super::*;
use crate::email::MailSettings;
use crate::email::authority::{Authority, HubWorld};
use crate::email::effects::MailEffects;
use crate::email::executor::Executor;
use crate::email::ledger::actions::{ActionState, Policy};
use crate::email::prepare::Actions;
use crate::email::settings::{ActionMode, ActionSettings};
use crate::email::source::{Folders, ProviderKind};
use crate::email::test_support::{self, Stored, account};
use crate::email::{LowSpace, notify, worker};
use crate::hub::{KeyedOutcome, MailRegistration};
use crate::mail_chat::ChatEvidence;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

const SENDER: &str = "canary-sender@example.com";
const SUBJECT: &str = "CanarySubject";
const BODY: &str = "CanaryBodyText";
const ROUTE: &str = "fake:mail";

fn action_settings() -> (MailSettings, ActionSettings) {
    let settings = MailSettings::parse(Some(
        &"[notify]\nroute = [\"fake:mail\"]\nsettle_seconds = 0\n[actions]\ntrash = \"approve\"\n"
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let actions = settings.actions.clone().unwrap();
    assert_eq!(actions.trash, ActionMode::Approve);
    (settings, actions)
}

fn policy(home: &std::path::Path, actions: ActionSettings) -> Policy {
    let directory = home.join("state/mail/default/actions");
    std::fs::create_dir_all(&directory).unwrap();
    Policy {
        actions,
        routes: vec![ROUTE.into()],
        fingerprint: account().fingerprint().unwrap(),
        account: "default".into(),
        audit: home.join("state/mail/default/audit.jsonl"),
        content: crate::email::content::ContentStore::new(directory),
        tombstone_seconds: 30 * 86_400,
        unknown_keep_seconds: 3 * 86_400,
        possible: Mutex::new(crate::email::ledger::actions::ALL_KINDS.to_vec()),
    }
}

/// The shared `mail_chat` helper records delivery at millisecond 1, which is
/// already outside the approval window of [`test_support::START`]. This chat
/// records delivery at `at_ms` instead.
fn delivering_chat(
    hub: &Arc<Hub>,
    component: &str,
    at_ms: u64,
) -> (Stored, tokio::task::JoinHandle<()>) {
    let link = crate::hub::Link::new(Arc::clone(hub), component, Some("owner".into()));
    let (registration, mut notices) = link.register_as(crate::state::Purpose::Mail);
    let texts = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&texts);
    let task = tokio::spawn(async move {
        let registration = registration;
        while let Some(notice) = notices.recv().await {
            let key = notice.key.clone().unwrap_or_default();
            record
                .lock()
                .unwrap()
                .push((key.clone(), notice.text.clone()));
            registration.record_keyed(&key, KeyedOutcome::Delivered { at_ms });
            notice.stored();
        }
    });
    (texts, task)
}

struct Counting {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl MailEffects for Counting {
    async fn save_draft(
        &mut self,
        _: &crate::email::ledger::Approved,
        _: &[u8],
    ) -> crate::email::ledger::actions::Execution {
        panic!("a trash action tried to save a draft");
    }

    async fn change(
        &mut self,
        _: &crate::email::ledger::Approved,
    ) -> crate::email::ledger::actions::Execution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::email::ledger::actions::Execution::Applied {
            code: crate::email::ledger::actions::OutcomeCode::Applied,
            sent_copy: None,
        }
    }

    async fn send(
        &mut self,
        _: &crate::email::ledger::Approved,
        _: &[u8],
    ) -> crate::email::ledger::actions::Execution {
        panic!("a trash action tried to send");
    }

    async fn copy_sent(&mut self, _: &crate::email::ledger::Approved, _: &[u8]) -> bool {
        panic!("a trash action tried to file a sent copy");
    }

    async fn probe(
        &mut self,
        _: &crate::email::content::ActionContent,
    ) -> crate::email::ledger::actions::Probe {
        panic!("nothing was left to probe");
    }
}

async fn check(worker: &worker::Worker<'_>, mailbox: &FakeMailbox) {
    let mut mailbox = mailbox.clone();
    match worker.check(&mut mailbox).await {
        Ok(()) => {}
        Err(worker::CheckError::Source(error) | worker::CheckError::State(error)) => {
            panic!("{error:#}");
        }
    }
}

async fn pump(notifier: &notify::Notifier<'_>) {
    for _ in 0..8 {
        let wait = notifier.step().await.unwrap();
        let snapshot = notifier.ledger.snapshot();
        if snapshot.queue.is_empty() && snapshot.batch.is_none() && wait >= Duration::from_secs(30)
        {
            break;
        }
    }
}

async fn command(hub: &Hub, route: &str, text: &str, id: &str, sent_ms: Option<u64>) -> String {
    let crate::mail_chat::Command::Mail(command) = crate::mail_chat::parse(text) else {
        panic!("not a mail command: {text}");
    };
    hub.mail_command(
        ChatEvidence {
            route: route.into(),
            peer: "owner".into(),
            message_id: id.into(),
            sent_ms,
        },
        command,
    )
    .await
}

fn notices(texts: &Stored) -> String {
    texts
        .lock()
        .unwrap()
        .iter()
        .map(|(_, text)| text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_owner_approves_one_trash_and_a_replay_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let (settings, actions) = action_settings();
    let clock = Box::leak(Box::new(FixedClock::new()));
    let ledger = Box::leak(Box::new(
        ledger(home.path()).with_actions(policy(home.path(), actions)),
    ));
    let hub = Hub::new(None);
    let delivered_ms = test_support::START * 1000;
    let (texts, _chat) = delivering_chat(&hub, ROUTE, delivered_ms);
    let registration: &'static MailRegistration = Box::leak(Box::new(
        hub.register_mail("email:default", vec![ROUTE.into()]),
    ));
    let world: &'static HubWorld<'static> = Box::leak(Box::new(HubWorld(registration)));
    let (commands, requests) = tokio::sync::mpsc::channel(16);
    registration.serve(commands);
    let authority = Authority {
        ledger,
        world,
        component: "email:default",
        clock,
    };
    tokio::spawn(async move {
        let _ = authority.serve(requests).await;
    });

    let low_space = LowSpace::default();
    let socket = home.path().join("daemon.sock");
    let (_seen, _daemon) = daemon(UnixListener::bind(&socket).unwrap(), |_| {
        r#"{"notify": true, "urgent": false, "summary": ["Needs a look."]}"#.to_owned()
    });
    let prepared = Actions {
        registration: Some(registration),
        provider: ProviderKind::Imap,
        own: Some("me@example.com".into()),
        frame: crate::email::compose::frame("default", ""),
        folders: Mutex::new(Folders {
            trash: Some("Trash".into()),
            ..Folders::default()
        }),
        can_send: false,
    };
    let worker = worker::Worker {
        ledger,
        settings: &settings,
        socket: &socket,
        cwd: home.path(),
        clock,
        low_space: &low_space,
        frame: crate::email::triage::frame("default", ""),
        options: crate::email::triage::Options::default(),
        actions: Some(&prepared),
    };
    let notifier = notify::Notifier {
        ledger,
        settings: &settings,
        account: "default",
        clock,
        hub: Some(&hub),
        previews: Some((&prepared, ledger.actions_policy().unwrap())),
    };
    let mailbox = FakeMailbox::default();
    check(&worker, &mailbox).await;

    mailbox.add(meta(1, SENDER, SUBJECT), BODY);
    check(&worker, &mailbox).await;
    pump(&notifier).await;
    let handle = {
        let handles = ledger.snapshot().handles;
        assert_eq!(handles.len(), 1, "{handles:?}");
        handles[0].handle.clone()
    };
    let reported = notices(&texts);
    assert!(
        reported.contains(&format!("#{handle}")),
        "the report did not carry the handle: {reported}"
    );
    assert!(reported.contains(SUBJECT), "{reported}");

    let preparing = command(
        &hub,
        ROUTE,
        &format!("mail trash #{handle}"),
        "trash-1",
        None,
    )
    .await;
    assert!(
        preparing.contains("Preparing") && preparing.contains(&format!("#{handle}")),
        "{preparing}"
    );
    assert!(
        !preparing.contains(SENDER) && !preparing.contains(SUBJECT),
        "{preparing}"
    );
    check(&worker, &mailbox).await;
    let code = {
        let actions = ledger.snapshot().actions;
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].state, ActionState::Proposed);
        assert_eq!(actions[0].handle.as_deref(), Some(handle.as_str()));
        actions[0].code.clone()
    };

    let early = command(
        &hub,
        ROUTE,
        &format!("approve {code}"),
        "too-soon",
        Some(delivered_ms),
    )
    .await;
    assert!(early.contains("has not reached you yet"), "{early}");
    assert_eq!(action_state(ledger, &code), ActionState::Proposed);
    pump(&notifier).await;
    let preview = notices(&texts);
    assert!(
        preview.contains(&format!("approve {code}")) && preview.contains(SUBJECT),
        "the preview did not reach the mail chat: {preview}"
    );
    assert_eq!(action_state(ledger, &code), ActionState::Open);

    let other = command(
        &hub,
        "wechat:mail",
        &format!("approve {code}"),
        "other-chat",
        Some(delivered_ms),
    )
    .await;
    assert!(other.contains("another chat"), "{other}");
    let before = command(
        &hub,
        ROUTE,
        &format!("approve {code}"),
        "before-delivery",
        Some(delivered_ms - 1),
    )
    .await;
    assert!(before.contains("sent before"), "{before}");
    assert_eq!(action_state(ledger, &code), ActionState::Open);

    let approved = command(
        &hub,
        ROUTE,
        &format!("approve {code}"),
        "approve-1",
        Some(delivered_ms),
    )
    .await;
    assert!(
        approved.starts_with(&format!("Approved {code}")),
        "{approved}"
    );
    assert!(
        !approved.contains(SENDER) && !approved.contains(SUBJECT),
        "{approved}"
    );
    assert_eq!(action_state(ledger, &code), ActionState::Approved);
    let replay = command(
        &hub,
        ROUTE,
        &format!("approve {code}"),
        "approve-1",
        Some(delivered_ms),
    )
    .await;
    assert_eq!(replay, format!("{code} was already approved."));
    assert_eq!(action_state(ledger, &code), ActionState::Approved);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_run = Arc::clone(&calls);
    let factory = move || {
        Box::new(Counting {
            calls: Arc::clone(&calls_for_run),
        }) as Box<dyn MailEffects>
    };
    let executor = Executor {
        ledger,
        content: &ledger.actions_policy().unwrap().content,
        effects: &factory,
        registration: Some(registration),
        clock,
    };
    let stop = tokio::sync::Notify::new();
    let run = executor.run(stop.notified());
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            result = &mut run => panic!("the executor returned before the trash was done: {result:?}"),
            () = tokio::task::yield_now() => {
                if action_state(ledger, &code) == ActionState::Done {
                    stop.notify_one();
                    break;
                }
            }
        }
    }
    tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("the executor did not finish the action")
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the trash ran more than once"
    );
    pump(&notifier).await;
    let told = notices(&texts);
    let done = format!("Done: {code} moved #{handle} to Trash.");
    assert!(told.contains(&done), "{told}");
    command(
        &hub,
        ROUTE,
        &format!("approve {code}"),
        "approve-1",
        Some(delivered_ms + 5),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(action_state(ledger, &code), ActionState::Done);

    mailbox.add(meta(2, SENDER, SUBJECT), BODY);
    check(&worker, &mailbox).await;
    pump(&notifier).await;
    let second = ledger
        .snapshot()
        .handles
        .into_iter()
        .find(|known| known.handle != handle)
        .unwrap()
        .handle;
    command(
        &hub,
        ROUTE,
        &format!("mail trash #{second}"),
        "trash-2",
        None,
    )
    .await;
    check(&worker, &mailbox).await;
    let second_code = ledger
        .snapshot()
        .actions
        .iter()
        .find(|entry| entry.handle.as_deref() == Some(second.as_str()))
        .unwrap()
        .code
        .clone();
    pump(&notifier).await;
    assert_eq!(action_state(ledger, &second_code), ActionState::Open);
    clock.advance(24 * 3600);
    let expired = command(
        &hub,
        ROUTE,
        &format!("approve {second_code}"),
        "approve-late",
        Some(delivered_ms + 24 * 3600 * 1000),
    )
    .await;
    assert!(expired.contains("expired"), "{expired}");
    assert_eq!(action_state(ledger, &second_code), ActionState::Expired);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an expired action was carried out"
    );

    let audit =
        std::fs::read_to_string(home.path().join("state/mail/default/audit.jsonl")).unwrap();
    assert!(
        audit.contains("\"approved\"") && audit.contains("\"done\""),
        "{audit}"
    );
    for secret in [
        SENDER,
        SUBJECT,
        BODY,
        code.as_str(),
        handle.as_str(),
        second.as_str(),
        second_code.as_str(),
    ] {
        assert!(
            !audit.contains(secret),
            "{secret} is in the audit log: {audit}"
        );
    }
    let state =
        std::fs::read_to_string(home.path().join("state/channels/email/default.json")).unwrap();
    assert!(state.contains(&code) && state.contains(&handle), "{state}");
    for secret in [SENDER, SUBJECT, BODY] {
        assert!(
            !state.contains(secret),
            "{secret} is in the state file: {state}"
        );
    }
    let done_line = told
        .lines()
        .find(|line| line.contains("Done:"))
        .unwrap_or(&told);
    assert!(
        !done_line.contains(SENDER) && !done_line.contains(SUBJECT),
        "{done_line}"
    );
}

fn action_state(ledger: &Ledger, code: &str) -> ActionState {
    ledger
        .snapshot()
        .actions
        .into_iter()
        .find(|entry| entry.code == code)
        .unwrap_or_else(|| panic!("no action for {code}"))
        .state
}

#[tokio::test]
async fn a_read_only_account_never_gets_a_handle_or_stores_an_action() {
    let home = tempfile::tempdir().unwrap();
    let ledger: Ledger = ledger(home.path());
    let settings = settings();
    let clock = FixedClock::new();
    let low_space = LowSpace::default();
    let socket = home.path().join("daemon.sock");
    let (_seen, _daemon) = daemon(UnixListener::bind(&socket).unwrap(), |_| {
        r#"{"notify": true, "urgent": false, "summary": ["Needs a look."]}"#.to_owned()
    });
    let hub = Hub::new(None);
    let (texts, _chat) = mail_chat(&hub, ROUTE, crate::state::Purpose::Mail);
    let worker = worker::Worker {
        ledger: &ledger,
        settings: &settings,
        socket: &socket,
        cwd: home.path(),
        clock: &clock,
        low_space: &low_space,
        frame: crate::email::triage::frame("default", ""),
        options: crate::email::triage::Options::default(),
        actions: None,
    };
    let notifier = notify::Notifier {
        ledger: &ledger,
        settings: &settings,
        account: "default",
        clock: &clock,
        hub: Some(&hub),
        previews: None,
    };
    let mailbox = FakeMailbox::default();
    check(&worker, &mailbox).await;
    mailbox.add(meta(1, SENDER, SUBJECT), BODY);
    check(&worker, &mailbox).await;
    pump(&notifier).await;

    let snapshot = ledger.snapshot();
    assert!(snapshot.handles.is_empty(), "{:?}", snapshot.handles);
    assert!(snapshot.actions.is_empty(), "{:?}", snapshot.actions);
    assert!(snapshot.requests.is_empty());
    let told = notices(&texts);
    assert!(told.contains(SUBJECT), "{told}");
    assert!(
        !told.contains('#'),
        "a read-only report carried a handle: {told}"
    );
    assert!(!told.contains("approve "), "{told}");
    let state =
        std::fs::read_to_string(home.path().join("state/channels/email/default.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&state).unwrap();
    let object = value.as_object().unwrap();
    for key in [
        "handles",
        "actions",
        "requests",
        "tombstones",
        "approvals",
        "reservations",
    ] {
        assert!(!object.contains_key(key), "{key} in {state}");
    }
    assert!(!home.path().join("state/mail/default/audit.jsonl").exists());
}
