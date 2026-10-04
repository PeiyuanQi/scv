//! Unit tests for `src/email/executor.rs`.

use super::*;
use crate::email::api::EffectParts;
use crate::email::content::{
    ActionContent, ActionKind, CONTENT_VERSION, Display, Folder, FolderRole, Form, Mailbox, Origin,
    Outgoing, Source,
};
use crate::email::credentials::GrantKind;
use crate::email::gmail::GmailEffects;
use crate::email::graph::GraphEffects;
use crate::email::ledger::actions::{
    ActionState, MAX_ATTEMPTS, NewAction, OutcomeCode, Policy, Probe, SentCopyState, World,
    preview_key,
};
use crate::email::ledger::{Counts, Ledger};
use crate::email::oauth::OAuthProvider;
use crate::email::plan::SendClass;
use crate::email::settings::{ActionMode, ActionSettings, SentCopy};
use crate::email::source::SourceRef;
use crate::email::test_support::{
    self, FakeHttp, TOKEN_PATH, account, ledger, token_answer, tokens,
};
use crate::hub::{Hub, KeyedOutcome};
use crate::mail_chat::ChatEvidence;
use crate::state::Credentials as _;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const ROUTE: &str = "fake:mail";
const OWNER: &str = "owner";
const START: u64 = test_support::START;

fn settings() -> ActionSettings {
    ActionSettings {
        draft: ActionMode::Approve,
        send: ActionMode::Approve,
        forward: ActionMode::Approve,
        archive: ActionMode::Approve,
        mark_read: ActionMode::Approve,
        trash: ActionMode::Approve,
        spam: ActionMode::Approve,
        ..ActionSettings::default()
    }
}

fn policy(home: &std::path::Path) -> Policy {
    let directory = home.join("state/mail/default/actions");
    std::fs::create_dir_all(&directory).unwrap();
    Policy {
        actions: settings(),
        routes: vec![ROUTE.into()],
        fingerprint: account().fingerprint().unwrap(),
        account: "default".into(),
        audit: home.join("state/mail/default/audit.jsonl"),
        content: crate::email::content::ContentStore::new(directory),
        tombstone_seconds: 30 * 86_400,
        unknown_keep_seconds: 3 * 86_400,
        possible: Mutex::new(super::super::ledger::actions::ALL_KINDS.to_vec()),
    }
}

struct FakeWorld;

impl World for FakeWorld {
    fn owner(&self, route: &str) -> Option<String> {
        (route == ROUTE).then(|| OWNER.to_owned())
    }

    fn delivered(&self, _route: &str, _key: &str) -> Option<KeyedOutcome> {
        None
    }
}

/// Scripted effects. Each connection shares the queues; a call that is not
/// scripted panics, so an extra send or a call after a mismatch fails the test.
#[derive(Clone)]
struct Effects {
    calls: Arc<Mutex<Vec<&'static str>>>,
    changes: Arc<Mutex<VecDeque<Execution>>>,
    sends: Arc<Mutex<VecDeque<Execution>>>,
    probes: Arc<Mutex<VecDeque<Probe>>>,
    copies: Arc<Mutex<VecDeque<bool>>>,
    hold_change: Arc<AtomicBool>,
    hold_copy: Arc<AtomicBool>,
    release_change: Arc<tokio::sync::Notify>,
    release_copy: Arc<tokio::sync::Notify>,
    entered: Arc<AtomicBool>,
}

impl Effects {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            changes: Arc::new(Mutex::new(VecDeque::new())),
            sends: Arc::new(Mutex::new(VecDeque::new())),
            probes: Arc::new(Mutex::new(VecDeque::new())),
            copies: Arc::new(Mutex::new(VecDeque::new())),
            hold_change: Arc::new(AtomicBool::new(false)),
            hold_copy: Arc::new(AtomicBool::new(false)),
            release_change: Arc::new(tokio::sync::Notify::new()),
            release_copy: Arc::new(tokio::sync::Notify::new()),
            entered: Arc::new(AtomicBool::new(false)),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    fn script_change(&self, results: impl IntoIterator<Item = Execution>) {
        self.changes.lock().unwrap().extend(results);
    }

    fn script_send(&self, results: impl IntoIterator<Item = Execution>) {
        self.sends.lock().unwrap().extend(results);
    }

    fn script_probe(&self, results: impl IntoIterator<Item = Probe>) {
        self.probes.lock().unwrap().extend(results);
    }

    fn script_copy(&self, results: impl IntoIterator<Item = bool>) {
        self.copies.lock().unwrap().extend(results);
    }
}

fn pop<T>(queue: &Mutex<VecDeque<T>>, what: &str) -> T {
    queue
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| panic!("effects had no scripted {what}"))
}

#[async_trait]
impl MailEffects for Effects {
    async fn save_draft(
        &mut self,
        _approved: &super::super::ledger::Approved,
        _message: &[u8],
    ) -> Execution {
        self.calls.lock().unwrap().push("save_draft");
        Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Internal,
        }
    }

    async fn change(&mut self, _approved: &super::super::ledger::Approved) -> Execution {
        self.calls.lock().unwrap().push("change");
        if self.hold_change.load(Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            self.release_change.notified().await;
        }
        pop(&self.changes, "change")
    }

    async fn send(
        &mut self,
        _approved: &super::super::ledger::Approved,
        _message: &[u8],
    ) -> Execution {
        self.calls.lock().unwrap().push("send");
        pop(&self.sends, "send")
    }

    async fn copy_sent(
        &mut self,
        _approved: &super::super::ledger::Approved,
        _message: &[u8],
    ) -> bool {
        self.calls.lock().unwrap().push("copy");
        if self.hold_copy.load(Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            self.release_copy.notified().await;
        }
        pop(&self.copies, "copy")
    }

    async fn probe(&mut self, _content: &ActionContent) -> Probe {
        self.calls.lock().unwrap().push("probe");
        pop(&self.probes, "probe")
    }
}

struct Bench {
    home: tempfile::TempDir,
    ledger: Ledger,
    clock: test_support::FixedClock,
    world: FakeWorld,
    effects: Effects,
}

impl Bench {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ledger = ledger(home.path()).with_actions(policy(home.path()));
        Self {
            home,
            ledger,
            clock: test_support::FixedClock::new(),
            world: FakeWorld,
            effects: Effects::new(),
        }
    }

    fn now(&self) -> u64 {
        self.clock.now()
    }

    fn entry(&self, id: &str) -> super::super::ledger::actions::ActionEntry {
        self.ledger
            .snapshot()
            .actions
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("no action {id}"))
    }

    fn outcomes(&self) -> Vec<String> {
        self.ledger
            .snapshot()
            .queue
            .into_iter()
            .filter(|item| item.key.starts_with("outcome:"))
            .map(|item| item.text)
            .collect()
    }

    fn trash(&self) -> ActionContent {
        self.bound(ActionKind::Trash, None, None)
    }

    fn send(&self, copy: SentCopy) -> ActionContent {
        self.bound(
            ActionKind::Send,
            Some(Folder {
                role: FolderRole::Sent,
                name: "Sent".into(),
            }),
            Some(Outgoing {
                form: Form::Reply,
                from: Mailbox {
                    name: String::new(),
                    address: "me@example.com".into(),
                },
                to: vec!["alice@example.com".into()],
                cc: Vec::new(),
                subject: "Re: Hello".into(),
                body: "Thanks.".into(),
                in_reply_to: Some("<m@example.com>".into()),
                references: vec!["<m@example.com>".into()],
                message_id: "<out@example.com>".into(),
                sent_copy: copy,
                notes: Vec::new(),
            }),
        )
    }

    fn bound(
        &self,
        kind: ActionKind,
        folder: Option<Folder>,
        message: Option<Outgoing>,
    ) -> ActionContent {
        ActionContent {
            v: CONTENT_VERSION,
            id: crate::email::content::new_id(),
            account: "default".into(),
            fingerprint: self.ledger.actions_policy().unwrap().fingerprint.clone(),
            kind,
            group: None,
            origin: Origin::Owner {
                route: ROUTE.into(),
                message_id: crate::email::content::new_id(),
            },
            source: Some(Source {
                reference: SourceRef::Imap {
                    mailbox: "INBOX".into(),
                    uidvalidity: 7,
                    uid: 42,
                },
                identity: "identity".into(),
                locator: "locator".into(),
                message_id: Some("<m@example.com>".into()),
            }),
            display: Some(Display {
                handle: "4K7P".into(),
                from_address: "alice@example.com".into(),
                from_name: "Alice".into(),
                subject: "Hello".into(),
            }),
            folder,
            message,
            created_at: START,
            hard_expiry: START + 72 * 3600,
            digest: String::new(),
        }
        .seal_digest()
    }

    /// Propose `content` as `code`, deliver the preview, and approve it.
    async fn approve(&self, content: &ActionContent, code: &str, message: &str) {
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
                    format!("owner:{ROUTE}:{message}:0"),
                    code.into(),
                )],
                (preview_key(&content.id, 1), "preview".into()),
                Vec::new(),
                self.now(),
            )
            .await
            .unwrap()
            .unwrap();
        self.deliver_open().await;
        let now = self.now();
        let answers = self
            .ledger
            .approve(
                &[code.to_owned()],
                &evidence(message, Some(now * 1000)),
                &self.world,
                now,
            )
            .await
            .unwrap();
        assert!(
            answers[0].starts_with(&format!("Approved {code}")),
            "{answers:?}"
        );
        assert_eq!(self.entry(&content.id).state, ActionState::Approved);
    }

    async fn deliver_open(&self) {
        let now = self.now();
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

    fn executor<'a>(
        &'a self,
        effects: &'a (dyn Fn() -> Box<dyn MailEffects> + Send + Sync),
    ) -> Executor<'a> {
        Executor {
            ledger: &self.ledger,
            content: &self.ledger.actions_policy().unwrap().content,
            effects,
            registration: None,
            clock: &self.clock,
        }
    }

    async fn execute(&self, id: &str) {
        let effects = self.effects.clone();
        let factory = move || Box::new(effects.clone()) as Box<dyn MailEffects>;
        self.executor(&factory).execute(id).await.unwrap();
    }
}

fn scripted(effects: &Effects) -> impl Fn() -> Box<dyn MailEffects> + Send + Sync {
    let effects = effects.clone();
    move || Box::new(effects.clone()) as Box<dyn MailEffects>
}

fn evidence(message: &str, sent_ms: Option<u64>) -> ChatEvidence {
    ChatEvidence {
        route: ROUTE.into(),
        peer: OWNER.into(),
        message_id: message.into(),
        sent_ms,
    }
}

fn applied() -> Execution {
    Execution::Applied {
        code: OutcomeCode::Applied,
        sent_copy: None,
    }
}

fn retryable() -> Execution {
    Execution::NotApplied {
        retry: true,
        code: OutcomeCode::Unreachable,
    }
}

/// Poll `run` until `ready`, panicking if the executor returns first.
async fn until_ready(
    run: &mut std::pin::Pin<&mut impl std::future::Future<Output = anyhow::Result<()>>>,
    ready: impl Fn() -> bool,
) {
    for _ in 0..100 {
        if ready() {
            return;
        }
        tokio::select! {
            biased;
            result = run.as_mut() => panic!("the executor returned early: {result:?}"),
            () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
    }
    panic!("the action never reached the expected point");
}

#[tokio::test]
async fn an_approved_action_runs_once_and_the_owner_is_told_it_is_done() {
    let bench = Bench::new();
    let content = bench.trash();
    bench.approve(&content, "TRASH2", "approve-1").await;
    bench.effects.script_change([applied()]);
    bench.execute(&content.id).await;

    let done = bench.entry(&content.id);
    assert_eq!(done.state, ActionState::Done, "{done:?}");
    assert_eq!(done.attempts, 1);
    assert_eq!(
        done.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::Applied)
    );
    assert_eq!(
        bench.outcomes(),
        ["Done: TRASH2 moved #4K7P to Trash.".to_owned()]
    );
    assert_eq!(bench.effects.calls(), ["change"]);
    assert!(
        bench.ledger.next_approved(bench.now()).is_none(),
        "a finished action is not carried out again"
    );
}

#[tokio::test]
async fn a_retryable_miss_waits_out_its_backoff_and_the_next_attempt_finishes() {
    let bench = Bench::new();
    let content = bench.trash();
    bench.approve(&content, "TRASH2", "approve-1").await;
    bench.effects.script_change([retryable(), applied()]);

    let first = bench.now();
    bench.execute(&content.id).await;
    let waiting = bench.entry(&content.id);
    assert_eq!(waiting.state, ActionState::Approved, "{waiting:?}");
    assert_eq!(waiting.attempts, 1);
    assert_eq!(waiting.not_before, first + 30, "the first retry waits 30s");
    assert!(
        bench.outcomes().is_empty(),
        "a retry is not told as a failure"
    );
    assert_eq!(bench.ledger.next_approved(first), None);

    bench.clock.advance(30);
    bench.execute(&content.id).await;
    let done = bench.entry(&content.id);
    assert_eq!(done.state, ActionState::Done, "{done:?}");
    assert_eq!(done.attempts, 2);
    assert_eq!(
        bench.outcomes(),
        ["Done: TRASH2 moved #4K7P to Trash.".to_owned()]
    );
    assert_eq!(bench.effects.calls(), ["change", "change"]);
}

#[tokio::test]
async fn an_action_fails_once_every_attempt_is_used() {
    let bench = Bench::new();
    let content = bench.trash();
    bench.approve(&content, "TRASH2", "approve-1").await;
    bench
        .effects
        .script_change((0..MAX_ATTEMPTS).map(|_| retryable()));

    bench.execute(&content.id).await;
    assert_eq!(bench.entry(&content.id).not_before, bench.now() + 30);
    bench.clock.advance(30);
    bench.execute(&content.id).await;
    assert_eq!(bench.entry(&content.id).attempts, 2);
    assert_eq!(bench.entry(&content.id).not_before, bench.now() + 120);
    assert_eq!(bench.entry(&content.id).state, ActionState::Approved);
    bench.clock.advance(120);
    bench.execute(&content.id).await;

    let failed = bench.entry(&content.id);
    assert_eq!(failed.state, ActionState::Failed, "{failed:?}");
    assert_eq!(failed.attempts, MAX_ATTEMPTS);
    assert_eq!(
        failed.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::Unreachable)
    );
    assert_eq!(
        bench.outcomes(),
        ["Not done: TRASH2 (moving #4K7P to Trash) did not happen because the mail server could not be reached."
            .to_owned()]
    );
    assert_eq!(bench.effects.calls().len(), MAX_ATTEMPTS as usize);
    assert!(bench.ledger.next_approved(bench.now()).is_none());
}

#[tokio::test]
async fn an_ambiguous_action_is_probed_on_the_next_pass() {
    let bench = Bench::new();
    let done = bench.trash();
    let unknown = bench.trash();
    bench.approve(&done, "DONE22", "approve-done").await;
    bench.approve(&unknown, "UNKN22", "approve-unknown").await;
    bench
        .effects
        .script_change([Execution::Ambiguous, Execution::Ambiguous]);
    bench.effects.script_probe([Probe::Done, Probe::Unknown]);

    let stop = tokio::sync::Notify::new();
    let factory = scripted(&bench.effects);
    let executor = bench.executor(&factory);
    let run = executor.run(stop.notified());
    tokio::pin!(run);
    until_ready(&mut run, || {
        bench.entry(&done.id).state == ActionState::Done
            && bench.entry(&unknown.id).state == ActionState::Unknown
    })
    .await;
    stop.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("the executor did not return after the probes")
        .unwrap();

    assert_eq!(
        bench.effects.calls(),
        ["change", "probe", "change", "probe"],
        "each ambiguous action is probed before the next one starts"
    );
    let found = bench.entry(&done.id);
    assert_eq!(found.state, ActionState::Done);
    assert_eq!(
        found.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::AlreadyDone)
    );
    let lost = bench.entry(&unknown.id);
    assert_eq!(lost.state, ActionState::Unknown, "{lost:?}");
    let told = bench.outcomes();
    assert!(
        told.iter()
            .any(|line| line == "Done: DONE22 moved #4K7P to Trash."),
        "{told:?}"
    );
    assert!(
        told.iter().any(|line| {
            line.contains("UNKN22")
                && line.contains("Check the mailbox")
                && line.contains("will not retry")
        }),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_send_whose_outcome_is_unclear_is_never_retried() {
    let bench = Bench::new();
    let content = bench.send(SentCopy::Provider);
    bench.approve(&content, "S3S3S3", "approve-send").await;
    bench.effects.script_send([Execution::Ambiguous]);
    bench.effects.script_probe([Probe::NotDone]);

    let stop = tokio::sync::Notify::new();
    let factory = scripted(&bench.effects);
    let executor = bench.executor(&factory);
    let run = executor.run(stop.notified());
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            result = &mut run => panic!("stopped early: {result:?}"),
            () = tokio::task::yield_now() => {
                if bench.entry(&content.id).state == ActionState::Unknown {
                    stop.notify_one();
                    break;
                }
            }
        }
    }
    // The stop is observed on the next idle wait, after the probe.
    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("the executor did not return after the probe")
        .unwrap();

    assert_eq!(bench.effects.calls(), ["send", "probe"]);
    let lost = bench.entry(&content.id);
    assert_eq!(lost.state, ActionState::Unknown, "{lost:?}");
    assert_ne!(lost.state, ActionState::Approved);
    assert!(bench.ledger.next_approved(bench.now()).is_none());
    let told = bench.outcomes();
    assert!(
        told.iter()
            .any(|line| line.contains("S3S3S3") && line.contains("will not retry")),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_content_file_changed_after_approval_is_invalid_and_effects_are_not_called() {
    let bench = Bench::new();
    let content = bench.trash();
    bench.approve(&content, "TRASH2", "approve-1").await;
    let path = bench
        .home
        .path()
        .join(format!("state/mail/default/actions/{}.json", content.id));
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    value["display"]["subject"] = serde_json::json!("rewritten after the owner approved");
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

    bench.execute(&content.id).await;
    let invalid = bench.entry(&content.id);
    assert_eq!(invalid.state, ActionState::Invalid, "{invalid:?}");
    assert_eq!(
        invalid.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::Mismatch)
    );
    assert!(
        bench.effects.calls().is_empty(),
        "effects ran: {:?}",
        bench.effects.calls()
    );
    let told = bench.outcomes();
    assert!(
        told.iter()
            .any(|line| line.contains("TRASH2") && line.contains("no longer matches")),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_kind_turned_off_after_approval_fails_without_an_effect() {
    let bench = Bench::new();
    let content = bench.trash();
    bench.approve(&content, "TRASH2", "approve-1").await;
    bench
        .ledger
        .actions_policy()
        .unwrap()
        .set_possible(vec![ActionKind::MarkRead]);

    bench.execute(&content.id).await;
    let failed = bench.entry(&content.id);
    assert_eq!(failed.state, ActionState::Failed, "{failed:?}");
    assert_eq!(
        failed.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::Policy)
    );
    assert!(bench.effects.calls().is_empty());
    let told = bench.outcomes();
    assert!(
        told.iter()
            .any(|line| line.contains("TRASH2") && line.contains("settings no longer allow")),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_sent_copy_is_marked_before_it_is_appended_and_the_owner_is_told_once() {
    let bench = Bench::new();
    let saved = bench.send(SentCopy::Append);
    let failed = bench.send(SentCopy::Append);
    bench.approve(&saved, "SAVE22", "approve-save").await;
    bench.approve(&failed, "FAIL22", "approve-fail").await;
    bench.effects.script_send([applied(), applied()]);
    bench.effects.script_copy([true, false]);
    bench.effects.hold_copy.store(true, Ordering::SeqCst);

    let factory = scripted(&bench.effects);
    let executor = bench.executor(&factory);
    let run = executor.execute(&saved.id);
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            result = &mut run => panic!("the copy finished before it was observed: {result:?}"),
            () = tokio::task::yield_now() => {
                if bench.effects.entered.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }
    let pending = bench.entry(&saved.id);
    assert_eq!(pending.state, ActionState::Done, "{pending:?}");
    assert_eq!(
        pending
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.sent_copy),
        Some(SentCopyState::Pending),
        "mark_sent records the send before the copy runs"
    );
    assert!(
        bench.outcomes().is_empty(),
        "the owner is not told until the copy's result is known"
    );
    assert_eq!(bench.effects.calls(), ["send", "copy"]);
    bench.effects.hold_copy.store(false, Ordering::SeqCst);
    bench.effects.release_copy.notify_one();
    run.await.unwrap();
    assert_eq!(
        bench
            .entry(&saved.id)
            .outcome
            .as_ref()
            .and_then(|o| o.sent_copy),
        Some(SentCopyState::Saved)
    );
    assert_eq!(
        bench.outcomes(),
        ["Done: SAVE22 sent the reply to #4K7P.".to_owned()]
    );

    bench.effects.entered.store(false, Ordering::SeqCst);
    bench.execute(&failed.id).await;
    assert_eq!(
        bench
            .entry(&failed.id)
            .outcome
            .as_ref()
            .and_then(|o| o.sent_copy),
        Some(SentCopyState::Failed)
    );
    let told = bench.outcomes();
    assert_eq!(told.len(), 2, "{told:?}");
    assert!(
        told.iter().any(|line| line
            == "Done: FAIL22 sent the reply to #4K7P; its copy in Sent could not be saved."),
        "{told:?}"
    );
    assert_eq!(
        bench.effects.calls(),
        ["send", "copy", "send", "copy"],
        "each send is copied once"
    );
}

#[tokio::test]
async fn the_drain_flag_stops_a_new_action_and_one_under_way_is_counted() {
    let bench = Bench::new();
    let first = bench.trash();
    let second = bench.trash();
    bench.approve(&first, "FIRST2", "approve-1").await;
    bench.approve(&second, "SECND2", "approve-2").await;
    bench.effects.script_change([applied()]);
    bench.effects.hold_change.store(true, Ordering::SeqCst);
    let hub = Hub::new(None);
    let registration = hub.register_mail("email:default", vec![ROUTE.into()]);
    let effects = bench.effects.clone();
    let factory = move || Box::new(effects.clone()) as Box<dyn MailEffects>;
    let executor = Executor {
        ledger: &bench.ledger,
        content: &bench.ledger.actions_policy().unwrap().content,
        effects: &factory,
        registration: Some(&registration),
        clock: &bench.clock,
    };

    let stop = tokio::sync::Notify::new();
    let run = executor.run(stop.notified());
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            result = &mut run => panic!("stopped before the action was under way: {result:?}"),
            () = tokio::task::yield_now() => {
                if bench.effects.entered.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }
    assert_eq!(hub.mail_executing(), 1, "the action under way is counted");
    assert_eq!(bench.entry(&first.id).state, ActionState::Executing);
    assert_eq!(bench.entry(&second.id).state, ActionState::Approved);
    assert_eq!(bench.effects.calls(), ["change"]);
    hub.set_mail_drain(true);
    bench.effects.release_change.notify_one();
    until_ready(&mut run, || {
        bench.entry(&first.id).state == ActionState::Done
    })
    .await;
    assert_eq!(hub.mail_executing(), 0);
    assert_eq!(
        bench.entry(&second.id).state,
        ActionState::Approved,
        "drain stopped the next action"
    );
    stop.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("run did not return")
        .unwrap();
    assert_eq!(bench.effects.calls(), ["change"]);
    assert_eq!(bench.entry(&second.id).state, ActionState::Approved);
}

#[tokio::test]
async fn stopping_waits_for_the_action_under_way_and_starts_nothing_after() {
    let bench = Bench::new();
    let first = bench.trash();
    let second = bench.trash();
    bench.approve(&first, "FIRST2", "approve-1").await;
    bench.approve(&second, "SECND2", "approve-2").await;
    bench.effects.script_change([applied()]);
    bench.effects.hold_change.store(true, Ordering::SeqCst);

    let stop = tokio::sync::Notify::new();
    let factory = scripted(&bench.effects);
    let executor = bench.executor(&factory);
    let run = executor.run(stop.notified());
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            result = &mut run => panic!("returned before the action was released: {result:?}"),
            () = tokio::task::yield_now() => {
                if bench.effects.entered.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }
    stop.notify_one();
    tokio::select! {
        biased;
        result = &mut run => panic!("returned while the action was still held: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    assert_eq!(bench.entry(&first.id).state, ActionState::Executing);
    assert_eq!(bench.entry(&second.id).state, ActionState::Approved);
    bench.effects.release_change.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("run did not wait for the action to finish")
        .unwrap();
    assert_eq!(bench.entry(&first.id).state, ActionState::Done);
    assert_eq!(bench.entry(&second.id).state, ActionState::Approved);
    assert_eq!(bench.effects.calls(), ["change"]);
}

#[tokio::test]
async fn an_already_stopped_executor_does_not_start_approved_work() {
    let bench = Bench::new();
    let content = bench.trash();
    bench.approve(&content, "STOP22", "approve-stop").await;
    let factory = scripted(&bench.effects);
    bench
        .executor(&factory)
        .run(std::future::ready(()))
        .await
        .unwrap();
    assert!(bench.effects.calls().is_empty());
    assert_eq!(bench.entry(&content.id).state, ActionState::Approved);
}

#[tokio::test]
async fn a_sibling_failure_drains_the_real_executor_and_preserves_queued_work() {
    let bench = Bench::new();
    let first = bench.trash();
    let second = bench.trash();
    bench.approve(&first, "FIRST2", "approve-1").await;
    bench.approve(&second, "SECND2", "approve-2").await;
    bench.effects.script_change([applied()]);
    bench.effects.hold_change.store(true, Ordering::SeqCst);
    let stop = tokio_util::sync::CancellationToken::new();
    let factory = scripted(&bench.effects);
    let executor = bench.executor(&factory);
    let others = async {
        while !bench.effects.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        Err(anyhow::anyhow!("notifier failed"))
    };
    let release = async {
        stop.cancelled().await;
        assert_eq!(bench.entry(&first.id).state, ActionState::Executing);
        bench.effects.release_change.notify_one();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            super::super::beside_executor(
                others,
                executor.run(stop.cancelled()),
                &stop,
                super::super::STOP_GRACE
            ),
            release
        )
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("notifier failed"));
    assert_eq!(bench.entry(&first.id).state, ActionState::Done);
    assert_eq!(bench.entry(&second.id).state, ActionState::Approved);
    assert_eq!(bench.effects.calls(), ["change"]);
}

#[tokio::test(start_paused = true)]
async fn an_action_past_its_budget_is_ambiguous_and_then_probed() {
    // Paused time auto-advances while the runtime is idle, so the 55s budget
    // elapses without a real wait. The effect never returns; the timeout is
    // what makes the outcome ambiguous.
    let bench: &'static Bench = Box::leak(Box::new(Bench::new()));
    let content = bench.trash();
    bench.approve(&content, "BUDGT2", "approve-1").await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let probes = Arc::new(AtomicUsize::new(0));
    let entered_for_run = Arc::clone(&entered);
    let probes_for_run = Arc::clone(&probes);
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_for_run = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let entered = entered_for_run;
        let probes = probes_for_run;
        let factory = move || {
            let entered = Arc::clone(&entered);
            let probes = Arc::clone(&probes);
            Box::new(Overrun { entered, probes }) as Box<dyn MailEffects>
        };
        let executor = Executor {
            ledger: &bench.ledger,
            content: &bench.ledger.actions_policy().unwrap().content,
            effects: &factory,
            registration: None,
            clock: &bench.clock,
        };
        executor.run(stop_for_run.notified()).await.unwrap();
    });
    entered.notified().await;
    tokio::time::sleep(ACTION_BUDGET + std::time::Duration::from_secs(1)).await;
    for _ in 0..20 {
        if bench.entry(&content.id).state == ActionState::Done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let done = bench.entry(&content.id);
    assert_eq!(done.state, ActionState::Done, "{done:?}");
    assert_eq!(done.attempts, 1, "the overrun is probed, not retried");
    assert_eq!(
        done.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::AlreadyDone)
    );
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    assert_eq!(
        bench.outcomes(),
        ["Done: BUDGT2 moved #4K7P to Trash.".to_owned()]
    );
    stop.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the executor did not return after the probe")
        .unwrap();
}

/// A connection whose `change` never returns, and whose probe finds the action done.
struct Overrun {
    entered: Arc<tokio::sync::Notify>,
    probes: Arc<AtomicUsize>,
}

#[async_trait]
impl MailEffects for Overrun {
    async fn save_draft(&mut self, _: &super::super::ledger::Approved, _: &[u8]) -> Execution {
        panic!("save_draft");
    }

    async fn change(&mut self, _: &super::super::ledger::Approved) -> Execution {
        self.entered.notify_one();
        std::future::pending().await
    }

    async fn send(&mut self, _: &super::super::ledger::Approved, _: &[u8]) -> Execution {
        panic!("send");
    }

    async fn copy_sent(&mut self, _: &super::super::ledger::Approved, _: &[u8]) -> bool {
        panic!("copy");
    }

    async fn probe(&mut self, _: &ActionContent) -> Probe {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Probe::Done
    }
}

#[tokio::test]
async fn a_stop_during_the_sent_copy_leaves_it_pending_and_recovery_tells_the_owner_once() {
    let bench = Bench::new();
    let content = bench.send(SentCopy::Append);
    bench.approve(&content, "COPY22", "approve-copy").await;
    bench.effects.script_send([applied()]);
    bench.effects.script_copy([true]);
    bench.effects.hold_copy.store(true, Ordering::SeqCst);

    let factory = scripted(&bench.effects);
    let executor = bench.executor(&factory);
    {
        let run = executor.execute(&content.id);
        tokio::pin!(run);
        loop {
            tokio::select! {
                biased;
                result = &mut run => panic!("the copy finished: {result:?}"),
                () = tokio::task::yield_now() => {
                    if bench.effects.entered.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
        let pending = bench.entry(&content.id);
        assert_eq!(pending.state, ActionState::Done);
        assert_eq!(
            pending
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.sent_copy),
            Some(SentCopyState::Pending)
        );
        assert!(bench.outcomes().is_empty());
    }

    bench.ledger.recover(bench.now()).await.unwrap();
    let recovered = bench.entry(&content.id);
    assert_eq!(recovered.state, ActionState::Done);
    assert_eq!(
        recovered
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.sent_copy),
        Some(SentCopyState::Failed)
    );
    assert_eq!(
        bench.outcomes(),
        ["Done: COPY22 sent the reply to #4K7P; its copy in Sent could not be saved.".to_owned()]
    );
    bench.ledger.recover(bench.now()).await.unwrap();
    assert_eq!(
        bench.outcomes().len(),
        1,
        "recovery tells the owner once: {:?}",
        bench.outcomes()
    );
}

/// Gmail and Graph draft `POST`s and mark-read writes answered HTTP 401, then
/// a read-only check that does not find the change. The executor must not
/// carry any of them out a second time.
#[tokio::test]
async fn a_gmail_or_graph_mutation_401_is_not_run_again_when_the_probe_is_not_done() {
    for (gmail, draft) in [(true, true), (true, false), (false, true), (false, false)] {
        mutation_401_is_not_run_again(gmail, draft).await;
    }
}

async fn mutation_401_is_not_run_again(gmail: bool, draft: bool) {
    let bench = Bench::new();
    let kind = if draft {
        ActionKind::Draft
    } else {
        ActionKind::MarkRead
    };
    let content = api_action(&bench, kind, gmail);
    let code = match (gmail, draft) {
        (true, true) => "GDRAF2",
        (true, false) => "GREAD2",
        (false, true) => "XDRAF2",
        (false, false) => "XREAD2",
    };
    bench.approve(&content, code, code).await;

    let fake = FakeHttp::start(move |request| {
        token_answer(request).unwrap_or_else(|| {
            if request.method == "GET" {
                let body = match (gmail, draft) {
                    (true, true) => r#"{"messages":[],"resultSizeEstimate":0}"#,
                    (true, false) => r#"{"id":"msg-1","labelIds":["INBOX","UNREAD"]}"#,
                    (false, true) => r#"{"value":[]}"#,
                    (false, false) => r#"{"parentFolderId":"inbox-id","isRead":false}"#,
                };
                (200, body.to_owned())
            } else {
                (401, r#"{"error":"unauthorized"}"#.to_owned())
            }
        })
    })
    .await;
    let provider = if gmail {
        OAuthProvider::Gmail
    } else {
        OAuthProvider::Graph
    };
    let parts = EffectParts {
        origin: fake.origin.clone(),
        reader: tokens(bench.home.path(), &fake.origin, provider, GrantKind::Reader),
        writer: Some(tokens(
            bench.home.path(),
            &fake.origin,
            provider,
            GrantKind::Writer,
        )),
        sender: None,
    };
    let factory = move || {
        let effects: Box<dyn MailEffects> = if gmail {
            Box::new(GmailEffects::new(parts.clone()))
        } else {
            Box::new(GraphEffects::new(parts.clone()))
        };
        effects
    };
    let stop = tokio::sync::Notify::new();
    let executor = bench.executor(&factory);
    let run = executor.run(stop.notified());
    tokio::pin!(run);
    until_ready(&mut run, || {
        bench.entry(&content.id).state == ActionState::Unknown
    })
    .await;
    stop.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("the executor did not return after the probe")
        .unwrap();

    let label = format!("{} {}", if gmail { "gmail" } else { "graph" }, kind.name());
    let lost = bench.entry(&content.id);
    assert_eq!(lost.state, ActionState::Unknown, "{label} {lost:?}");
    assert_ne!(lost.state, ActionState::Approved);
    assert!(lost.uncertain, "{label}");
    assert_eq!(lost.attempts, 1, "{label} was carried out again");
    assert_eq!(
        lost.outcome.as_ref().map(|outcome| outcome.code),
        Some(OutcomeCode::AuthUncertain),
        "{label}"
    );
    assert_eq!(bench.ledger.next_approved(bench.now()), None, "{label}");
    let mutations = fake
        .requests()
        .iter()
        .filter(|request| {
            request.path != TOKEN_PATH && matches!(request.method.as_str(), "POST" | "PATCH")
        })
        .count();
    assert_eq!(mutations, 1, "{label} mutations: {:?}", fake.requests());
    assert!(
        fake.requests()
            .iter()
            .any(|request| request.method == "GET"),
        "{label} was not checked: {:?}",
        fake.requests()
    );
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.header("idempotency-key").is_none()),
        "{label} {:?}",
        fake.requests()
    );
    let told = bench.outcomes();
    assert!(
        told.iter().any(|line| {
            line.contains(code)
                && line.contains("refused the access token")
                && line.contains("will not retry")
                && line.contains("Propose it again only if it did not happen")
        }),
        "{label} {told:?}"
    );
    let log =
        std::fs::read_to_string(bench.home.path().join("state/mail/default/audit.jsonl")).unwrap();
    let events: Vec<String> = log
        .lines()
        .map(|line| serde_json::from_str::<crate::email::audit::Line>(line).unwrap())
        .filter(|line| line.id == content.id)
        .map(|line| line.event)
        .collect();
    assert_eq!(
        events,
        [
            "proposed",
            "previewing",
            "open",
            "approved",
            "executing",
            "unknown"
        ],
        "{label} {log}"
    );
    assert!(
        log.lines()
            .any(|line| line.contains(&content.id) && line.contains("\"auth_uncertain\"")),
        "{label} {log}"
    );
    assert!(!log.contains(code), "{label} code in the audit log");
}

fn api_action(bench: &Bench, kind: ActionKind, gmail: bool) -> ActionContent {
    let reference = if gmail {
        SourceRef::Gmail { id: "msg-1".into() }
    } else {
        SourceRef::Graph { id: "msg-1".into() }
    };
    let message = (kind == ActionKind::Draft).then(|| Outgoing {
        form: Form::Reply,
        from: Mailbox {
            name: String::new(),
            address: "me@example.com".into(),
        },
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        subject: "Re: Hello".into(),
        body: "Thanks.".into(),
        in_reply_to: Some("<m@example.com>".into()),
        references: vec!["<m@example.com>".into()],
        message_id: "<out@example.com>".into(),
        sent_copy: SentCopy::Provider,
        notes: Vec::new(),
    });
    ActionContent {
        v: CONTENT_VERSION,
        id: crate::email::content::new_id(),
        account: "default".into(),
        fingerprint: bench.ledger.actions_policy().unwrap().fingerprint.clone(),
        kind,
        group: None,
        origin: Origin::Owner {
            route: ROUTE.into(),
            message_id: crate::email::content::new_id(),
        },
        source: Some(Source {
            reference,
            identity: "identity".into(),
            locator: "locator".into(),
            message_id: Some("<m@example.com>".into()),
        }),
        display: Some(Display {
            handle: "4K7P".into(),
            from_address: "alice@example.com".into(),
            from_name: "Alice".into(),
            subject: "Hello".into(),
        }),
        folder: (kind == ActionKind::Draft).then(|| Folder {
            role: FolderRole::Drafts,
            name: if gmail { "DRAFT" } else { "drafts" }.into(),
        }),
        message,
        created_at: START,
        hard_expiry: START + 72 * 3600,
        digest: String::new(),
    }
    .seal_digest()
}
