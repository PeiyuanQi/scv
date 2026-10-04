//! Unit tests for `src/email/ledger/actions.rs`.

use super::*;
use crate::email::content::{
    ActionContent, CONTENT_VERSION, Display, Folder, FolderRole, Mailbox, Origin, Outgoing, Source,
};
use crate::email::ledger::{Counts, Ledger};
use crate::email::plan::SendClass;
use crate::email::settings::{ActionMode, SentCopy};
use crate::email::source::SourceRef;
use crate::email::test_support::{account, ledger};
use crate::state::Credentials as _;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::Ordering;

const ROUTE: &str = "feishu:mail";
const OWNER: &str = "ou_owner";
const T0: u64 = 1_728_032_400;

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

fn policy(home: &std::path::Path, actions: ActionSettings) -> Policy {
    let directory = home.join("state/mail/default/actions");
    std::fs::create_dir_all(&directory).unwrap();
    Policy {
        actions,
        routes: vec![ROUTE.into()],
        fingerprint: account().fingerprint().unwrap(),
        account: "default".into(),
        audit: home.join("state/mail/default/audit.jsonl"),
        content: ContentStore::new(directory),
        tombstone_seconds: 30 * DAY,
        unknown_keep_seconds: 3 * DAY,
        possible: std::sync::Mutex::new(ALL_KINDS.to_vec()),
    }
}

/// What the hub tells the ledger, as a test sets it.
struct FakeWorld {
    owners: StdMutex<HashMap<String, String>>,
    delivered: StdMutex<Option<KeyedOutcome>>,
}

impl FakeWorld {
    fn new() -> Self {
        let mut owners = HashMap::new();
        owners.insert(ROUTE.into(), OWNER.into());
        Self {
            owners: StdMutex::new(owners),
            delivered: StdMutex::new(None),
        }
    }

    fn set_owner(&self, route: &str, owner: Option<&str>) {
        let mut owners = self.owners.lock().unwrap();
        match owner {
            Some(owner) => {
                owners.insert(route.to_owned(), owner.to_owned());
            }
            None => {
                owners.remove(route);
            }
        }
    }
}

impl World for FakeWorld {
    fn owner(&self, route: &str) -> Option<String> {
        self.owners.lock().unwrap().get(route).cloned()
    }

    fn delivered(&self, _route: &str, _key: &str) -> Option<KeyedOutcome> {
        *self.delivered.lock().unwrap()
    }
}

struct Bench {
    home: tempfile::TempDir,
    ledger: Ledger,
    world: FakeWorld,
}

impl Bench {
    fn new() -> Self {
        Self::with(settings())
    }

    fn with(actions: ActionSettings) -> Self {
        Self::with_routes(actions, vec![ROUTE.into()])
    }

    fn with_routes(actions: ActionSettings, routes: Vec<String>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let mut built = policy(home.path(), actions);
        built.routes = routes;
        let ledger = ledger(home.path()).with_actions(built);
        Self {
            home,
            ledger,
            world: FakeWorld::new(),
        }
    }

    fn policy(&self) -> &Policy {
        self.ledger.actions_policy().unwrap()
    }

    fn entry(&self, id: &str) -> ActionEntry {
        self.ledger
            .snapshot()
            .actions
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap()
    }

    fn reply(&self, kind: ActionKind, group: Option<&str>) -> ActionContent {
        ActionContent {
            v: CONTENT_VERSION,
            id: crate::email::content::new_id(),
            account: "default".into(),
            fingerprint: self.policy().fingerprint.clone(),
            kind,
            group: group.map(str::to_owned),
            origin: Origin::Owner {
                route: ROUTE.into(),
                message_id: "om_request".into(),
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
            folder: Some(Folder {
                role: if kind == ActionKind::Draft {
                    FolderRole::Drafts
                } else {
                    FolderRole::Sent
                },
                name: "Drafts".into(),
            }),
            message: Some(Outgoing {
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
                message_id: "<x@example.com>".into(),
                sent_copy: SentCopy::Provider,
                notes: Vec::new(),
            }),
            created_at: T0,
            hard_expiry: T0 + 72 * HOUR,
            digest: String::new(),
        }
        .seal_digest()
    }

    /// Propose `contents` as one request's alternatives, with codes `codes`.
    async fn propose(&self, key: &str, contents: &[ActionContent], codes: &[&str]) {
        for content in contents {
            self.policy().content.write_new(content).unwrap();
        }
        let proposed = contents
            .iter()
            .zip(codes)
            .enumerate()
            .map(|(n, (content, code))| {
                NewAction::new(content, format!("{key}:{n}"), (*code).into())
            })
            .collect();
        self.ledger
            .propose(
                None,
                proposed,
                (preview_key(&contents[0].id, 1), "preview".into()),
                Vec::new(),
                T0,
            )
            .await
            .unwrap()
            .unwrap();
    }

    /// A draft and a send of one reply, offered as `D2D2D2` and `S3S3S3`.
    async fn reply_pair(&self) -> (String, String) {
        let group = new_group();
        let draft = self.reply(ActionKind::Draft, Some(&group));
        let send = self.reply(ActionKind::Send, Some(&group));
        self.propose(
            "owner:feishu:mail:om_request",
            &[draft.clone(), send.clone()],
            &["D2D2D2", "S3S3S3"],
        )
        .await;
        (draft.id, send.id)
    }

    /// Hand every queued preview to the mail chat, stored at `at` and
    /// delivered at `delivered_ms`.
    async fn deliver(&self, at: u64, delivered_ms: u64) -> String {
        self.deliver_to(ROUTE, OWNER, at, delivered_ms).await
    }

    /// Hand every queued preview to `route` for `peer`.
    async fn deliver_to(&self, route: &str, peer: &str, at: u64, delivered_ms: u64) -> String {
        let snapshot = self.ledger.snapshot();
        let seqs = snapshot.queue.iter().map(|item| item.seq).collect();
        let batch = self
            .ledger
            .begin_batch(
                seqs,
                SendClass::Response,
                "text".into(),
                Counts::default(),
                at,
            )
            .await
            .unwrap();
        self.ledger.batch_stored(route, peer, at).await.unwrap();
        self.ledger
            .settle_watched(&[(batch.key.clone(), delivered_ms)], &[], &mut || None)
            .await
            .unwrap();
        batch.key
    }

    async fn deny_codes(&self, codes: Option<&[&str]>, now: u64) -> Vec<String> {
        self.deny_as(codes, &evidence("om_deny", None), now).await
    }

    async fn deny_as(
        &self,
        codes: Option<&[&str]>,
        evidence: &ChatEvidence,
        now: u64,
    ) -> Vec<String> {
        let owned = codes.map(|codes| {
            codes
                .iter()
                .map(|code| (*code).to_owned())
                .collect::<Vec<_>>()
        });
        self.ledger
            .deny(owned.as_deref(), evidence, &self.world, now)
            .await
            .unwrap()
    }

    async fn approve(
        &self,
        codes: &[&str],
        message: &str,
        sent_ms: Option<u64>,
        now: u64,
    ) -> Vec<String> {
        let codes: Vec<String> = codes.iter().map(|code| (*code).to_owned()).collect();
        self.ledger
            .approve(&codes, &evidence(message, sent_ms), &self.world, now)
            .await
            .unwrap()
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

const DELIVERED_MS: u64 = (T0 + 10) * 1000;

#[tokio::test]
async fn an_approved_draft_runs_once_and_its_sibling_is_superseded() {
    let bench = Bench::new();
    let (draft, send) = bench.reply_pair().await;
    assert_eq!(bench.entry(&draft).state, ActionState::Proposed);
    bench.deliver(T0 + 5, DELIVERED_MS).await;
    let open = bench.entry(&draft);
    assert_eq!(open.state, ActionState::Open);
    assert_eq!(
        open.expires_at,
        T0 + 10 + 24 * HOUR,
        "the approval window starts at delivery"
    );

    let answers = bench
        .approve(&["D2D2D2"], "om_2", Some(DELIVERED_MS + 1), T0 + 20)
        .await;
    assert_eq!(
        answers,
        ["Approved D2D2D2: saving the reply to #4K7P in Drafts."]
    );
    let approved = bench.entry(&draft);
    assert_eq!(approved.state, ActionState::Approved);
    assert_eq!(approved.execute_by, Some(T0 + 20 + 15 * 60));
    assert_eq!(bench.entry(&send).state, ActionState::Superseded);
    assert_eq!(bench.ledger.snapshot().reservations.len(), 1);

    assert_eq!(bench.ledger.next_approved(T0 + 21), Some(draft.clone()));
    let content = bench.policy().content.read(&draft).unwrap();
    let running = bench
        .ledger
        .begin_execution(&draft, content, T0 + 21)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.attempt(), 1);
    assert_eq!(bench.entry(&draft).state, ActionState::Executing);
    // Nothing else can start it again while it runs.
    let again = bench.policy().content.read(&draft).unwrap();
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, again, T0 + 22)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::NotReady)
    );
    bench
        .ledger
        .finish_execution(
            running,
            Execution::Applied {
                code: OutcomeCode::Applied,
                sent_copy: None,
            },
            T0 + 23,
        )
        .await
        .unwrap();
    let done = bench.entry(&draft);
    assert_eq!(done.state, ActionState::Done);
    let told = bench.ledger.snapshot().queue;
    assert!(
        told.iter()
            .any(|item| item.key == format!("outcome:{draft}")
                && item.text == "Done: D2D2D2 saved the reply to #4K7P in Drafts."),
        "{told:?}"
    );

    // Once over, both are tombstoned and their codes still answer.
    let swept = bench.ledger.sweep_actions(14 * DAY, T0 + 24).await.unwrap();
    assert_eq!(swept.tombstoned.len(), 2);
    assert!(bench.ledger.snapshot().actions.is_empty());
    assert_eq!(
        bench
            .approve(
                &["D2D2D2", "S3S3S3"],
                "om_3",
                Some(DELIVERED_MS + 5),
                T0 + 25
            )
            .await,
        [
            "D2D2D2 is done.",
            "S3S3S3 was not needed: another choice for the same mail was approved."
        ]
    );
}

#[tokio::test]
async fn every_approval_check_answers_exactly_and_changes_nothing() {
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    assert_eq!(
        bench
            .approve(&["ZZZZZZ"], "om_a", Some(DELIVERED_MS), T0)
            .await,
        ["No mail action has code ZZZZZZ."]
    );
    assert_eq!(
        bench
            .approve(&["D2D2D2"], "om_b", Some(DELIVERED_MS), T0)
            .await,
        ["D2D2D2 has not reached you yet; approve it once it has."]
    );
    bench.deliver(T0 + 5, DELIVERED_MS).await;
    let other_chat = ChatEvidence {
        route: "wechat:mail".into(),
        ..evidence("om_c", Some(DELIVERED_MS + 1))
    };
    let codes = vec!["D2D2D2".to_owned()];
    assert_eq!(
        bench
            .ledger
            .approve(&codes, &other_chat, &bench.world, T0 + 20)
            .await
            .unwrap(),
        ["D2D2D2 was sent to another chat; answer it there."]
    );
    let stranger = ChatEvidence {
        peer: "ou_other".into(),
        ..evidence("om_d", Some(DELIVERED_MS + 1))
    };
    assert_eq!(
        bench
            .ledger
            .approve(&codes, &stranger, &bench.world, T0 + 20)
            .await
            .unwrap(),
        ["D2D2D2 was sent to another chat; answer it there."]
    );
    // The chat's owner changed since the preview went out.
    bench.world.set_owner(ROUTE, Some("ou_new"));
    assert_eq!(
        bench
            .approve(&["D2D2D2"], "om_e", Some(DELIVERED_MS + 1), T0 + 20)
            .await,
        ["D2D2D2 was sent to another chat; answer it there."]
    );
    bench.world.set_owner(ROUTE, Some(OWNER));
    assert!(
        bench.approve(&["D2D2D2"], "om_f", None, T0 + 20).await[0]
            .starts_with("Not approved: this chat did not say when")
    );
    assert_eq!(
        bench
            .approve(&["D2D2D2"], "om_g", Some(DELIVERED_MS - 1), T0 + 20)
            .await,
        ["Not approved: this message was sent before D2D2D2 reached you."]
    );
    assert_eq!(bench.entry(&draft).state, ActionState::Open);
    assert!(bench.ledger.snapshot().reservations.is_empty());
    assert!(bench.ledger.snapshot().approvals.is_empty());
    // Past its window it expires.
    assert_eq!(
        bench
            .approve(
                &["D2D2D2"],
                "om_h",
                Some(DELIVERED_MS + 1),
                T0 + 10 + 24 * HOUR
            )
            .await,
        ["D2D2D2 expired."]
    );
    assert_eq!(bench.entry(&draft).state, ActionState::Expired);
    assert_eq!(bench.ledger.snapshot().counts.expired, 1);
}

#[tokio::test]
async fn settings_and_limits_can_refuse_an_approval() {
    let off = Bench::with(ActionSettings {
        draft: ActionMode::Off,
        ..settings()
    });
    let content = off.reply(ActionKind::Draft, None);
    off.propose("owner:x:1", &[content], &["D2D2D2"]).await;
    off.deliver(T0, DELIVERED_MS).await;
    assert_eq!(
        off.approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
            .await,
        ["Not approved: draft is off in this account's settings now."]
    );

    let capped = Bench::with(ActionSettings {
        max_drafts_per_day: 1,
        ..settings()
    });
    let first = capped.reply(ActionKind::Draft, None);
    let second = capped.reply(ActionKind::Draft, None);
    capped.propose("owner:x:1", &[first], &["AAAAAA"]).await;
    capped
        .propose("owner:x:2", std::slice::from_ref(&second), &["BBBBBB"])
        .await;
    capped.deliver(T0, DELIVERED_MS).await;
    assert_eq!(
        capped
            .approve(&["AAAAAA", "BBBBBB"], "om_2", Some(DELIVERED_MS), T0 + 1)
            .await,
        [
            "Approved AAAAAA: saving the reply to #4K7P in Drafts.".to_owned(),
            "Not approved: today's limit for drafts is reached; BBBBBB stays open until it \
             expires."
                .to_owned()
        ]
    );
    assert_eq!(capped.entry(&second.id).state, ActionState::Open);

    // The provider cannot do it (say, no archive folder).
    let bench = Bench::new();
    let content = bench.reply(ActionKind::Draft, None);
    bench.propose("owner:x:1", &[content], &["D2D2D2"]).await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench.policy().set_possible(vec![ActionKind::Send]);
    assert_eq!(
        bench
            .approve(&["D2D2D2"], "om_3", Some(DELIVERED_MS), T0 + 1)
            .await,
        ["Not approved: draft is off in this account's settings now."]
    );
}

#[tokio::test]
async fn one_message_approves_many_codes_once_and_a_replay_changes_nothing() {
    let bench = Bench::new();
    let trash = ActionContent {
        kind: ActionKind::Trash,
        message: None,
        folder: Some(Folder {
            role: FolderRole::Trash,
            name: "Trash".into(),
        }),
        ..bench.reply(ActionKind::Trash, None)
    }
    .seal_digest();
    let spam = ActionContent {
        kind: ActionKind::Spam,
        message: None,
        id: crate::email::content::new_id(),
        folder: Some(Folder {
            role: FolderRole::Junk,
            name: "Junk".into(),
        }),
        ..bench.reply(ActionKind::Spam, None)
    }
    .seal_digest();
    bench
        .propose("owner:x:1", std::slice::from_ref(&trash), &["TTTTTT"])
        .await;
    bench
        .propose("owner:x:2", std::slice::from_ref(&spam), &["PPPPPP"])
        .await;
    bench.deliver(T0, DELIVERED_MS).await;
    let answers = bench
        .approve(&["TTTTTT", "PPPPPP"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    assert_eq!(
        answers,
        [
            "Approved TTTTTT: moving #4K7P to Trash.",
            "Approved PPPPPP: moving #4K7P to Spam."
        ]
    );
    let before = bench.ledger.snapshot();
    let replayed = bench
        .approve(&["TTTTTT", "PPPPPP"], "om_1", Some(DELIVERED_MS), T0 + 2)
        .await;
    assert_eq!(
        replayed,
        [
            "TTTTTT was already approved.",
            "PPPPPP was already approved."
        ]
    );
    let after = bench.ledger.snapshot();
    assert_eq!(before.actions, after.actions);
    assert_eq!(before.reservations, after.reservations);
}

#[tokio::test]
async fn deny_and_cancel_exclude_execution_in_either_order() {
    let bench = Bench::new();
    let (draft, send) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    // Denied after approval, before it starts: it never runs.
    assert_eq!(
        bench.deny_codes(Some(&["D2D2D2"]), T0 + 2).await,
        ["Denied D2D2D2 (saving the reply to #4K7P in Drafts)."]
    );
    assert!(bench.ledger.snapshot().reservations.is_empty());
    let content = bench.policy().content.read(&draft).unwrap();
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, content, T0 + 3)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::NotReady)
    );
    assert_eq!(bench.entry(&send).state, ActionState::Superseded);

    // Started first: a deny cannot stop it.
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let content = bench.policy().content.read(&draft).unwrap();
    let running = bench
        .ledger
        .begin_execution(&draft, content, T0 + 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        bench.deny_codes(Some(&["D2D2D2"]), T0 + 3).await,
        ["D2D2D2 is being carried out."]
    );
    assert_eq!(
        bench.ledger.cancel(Some(&draft), T0 + 3).await.unwrap(),
        "Cancelled 0 mail action(s); 1 being carried out cannot be cancelled now."
    );
    assert_eq!(
        bench.deny_codes(None, T0 + 3).await,
        ["Nothing was waiting. 1 being carried out cannot be stopped now."]
    );
    bench
        .ledger
        .finish_execution(
            running,
            Execution::Applied {
                code: OutcomeCode::Applied,
                sent_copy: None,
            },
            T0 + 4,
        )
        .await
        .unwrap();
    assert_eq!(bench.entry(&draft).state, ActionState::Done);

    // `scv mail cancel --all` withdraws what has not started.
    let bench = Bench::new();
    bench.reply_pair().await;
    assert_eq!(
        bench.ledger.cancel(None, T0).await.unwrap(),
        "Cancelled 2 mail action(s)."
    );
    assert_eq!(
        bench.ledger.cancel(None, T0).await.unwrap(),
        "No mail action was waiting."
    );
    assert_eq!(
        bench.ledger.cancel(Some("a0"), T0).await.unwrap(),
        "No such mail action is waiting."
    );
}

#[tokio::test]
async fn execution_rechecks_content_credentials_deadline_and_settings() {
    // Content changed on disk after approval.
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let mut changed = bench.policy().content.read(&draft).unwrap().unwrap();
    changed.message.as_mut().unwrap().to = vec!["mallory@example.com".into()];
    let changed = changed.seal_digest();
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, Some(changed), T0 + 2)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::Ended(ActionState::Invalid))
    );
    assert!(bench.ledger.snapshot().reservations.is_empty());

    // A missing content file.
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, None, T0 + 2)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::Ended(ActionState::Invalid))
    );

    // Another mailbox's content.
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let mut other = bench.policy().content.read(&draft).unwrap().unwrap();
    other.fingerprint = "0".repeat(64);
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, Some(other.seal_digest()), T0 + 2)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::Ended(ActionState::Invalid))
    );

    // Too late after approval: it never fires after an outage.
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let content = bench.policy().content.read(&draft).unwrap();
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, content, T0 + 1 + 15 * 60 + 1)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::Ended(ActionState::Failed))
    );
    assert_eq!(
        bench.entry(&draft).outcome.unwrap().code,
        OutcomeCode::Stale
    );

    // Recipients the settings no longer allow.
    let bench = Bench::with(ActionSettings {
        recipient_domains: vec!["example.org".into()],
        ..settings()
    });
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let content = bench.policy().content.read(&draft).unwrap();
    assert_eq!(
        bench
            .ledger
            .begin_execution(&draft, content, T0 + 2)
            .await
            .unwrap()
            .err(),
        Some(NotStarted::Ended(ActionState::Failed))
    );
    assert_eq!(
        bench.entry(&draft).outcome.unwrap().code,
        OutcomeCode::Policy
    );
}

async fn running(bench: &Bench, kind: ActionKind) -> (String, Approved) {
    let content = bench.reply(kind, None);
    bench
        .propose("owner:x:1", std::slice::from_ref(&content), &["RRRRRR"])
        .await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["RRRRRR"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let read = bench.policy().content.read(&content.id).unwrap();
    let approved = bench
        .ledger
        .begin_execution(&content.id, read, T0 + 2)
        .await
        .unwrap()
        .unwrap();
    (content.id, approved)
}

#[tokio::test]
async fn retries_are_bounded_and_backed_off() {
    let bench = Bench::new();
    let (id, approved) = running(&bench, ActionKind::Draft).await;
    let retry = Execution::NotApplied {
        retry: true,
        code: OutcomeCode::Unreachable,
    };
    bench
        .ledger
        .finish_execution(approved, retry, T0 + 3)
        .await
        .unwrap();
    let entry = bench.entry(&id);
    assert_eq!(
        (entry.state, entry.not_before),
        (ActionState::Approved, T0 + 33)
    );
    assert_eq!(bench.ledger.next_approved(T0 + 32), None);
    for (attempt, at) in [(2, T0 + 40), (3, T0 + 200)] {
        let content = bench.policy().content.read(&id).unwrap();
        let approved = bench
            .ledger
            .begin_execution(&id, content, at)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approved.attempt(), attempt);
        bench
            .ledger
            .finish_execution(approved, retry, at + 1)
            .await
            .unwrap();
    }
    let failed = bench.entry(&id);
    assert_eq!(failed.state, ActionState::Failed);
    assert!(bench.ledger.snapshot().reservations.is_empty());
    assert!(
        bench
            .ledger
            .snapshot()
            .queue
            .iter()
            .any(|item| item.key == format!("outcome:{id}")
                && item.text.starts_with(
                    "Not done: RRRRRR (saving the reply to #4K7P in Drafts) did not \
                                  happen because the mail server could not be reached"
                ))
    );
}

#[tokio::test]
async fn an_unclear_send_is_checked_and_never_sent_again() {
    let bench = Bench::new();
    let (id, approved) = running(&bench, ActionKind::Send).await;
    bench
        .ledger
        .finish_execution(approved, Execution::Ambiguous, T0 + 3)
        .await
        .unwrap();
    let entry = bench.entry(&id);
    assert_eq!(entry.state, ActionState::Executing);
    assert_eq!(bench.ledger.due_probes(T0 + 3), std::slice::from_ref(&id));
    assert_eq!(bench.ledger.next_approved(T0 + 3), None);
    // A check that cannot reach the provider waits and tries again.
    bench
        .ledger
        .probed(&id, Probe::Unreachable, T0 + 4)
        .await
        .unwrap();
    assert!(bench.ledger.due_probes(T0 + 5).is_empty());
    assert_eq!(bench.ledger.due_probes(T0 + 40), std::slice::from_ref(&id));
    // Not found is not proof it was not sent: unknown, never retried.
    bench
        .ledger
        .probed(&id, Probe::NotDone, T0 + 40)
        .await
        .unwrap();
    let unknown = bench.entry(&id);
    assert_eq!(unknown.state, ActionState::Unknown);
    assert_eq!(
        bench.ledger.snapshot().reservations.len(),
        1,
        "it may have happened"
    );
    assert!(
        bench
            .ledger
            .snapshot()
            .queue
            .iter()
            .any(|item| item.text.starts_with(
                "SCV lost track of RRRRRR (sending the reply to #4K7P) while carrying it out"
            ))
    );
    // Kept past the tombstone sweep until its retention ends.
    bench
        .ledger
        .sweep_actions(14 * DAY, T0 + DAY)
        .await
        .unwrap();
    assert_eq!(bench.entry(&id).state, ActionState::Unknown);
    let swept = bench
        .ledger
        .sweep_actions(14 * DAY, T0 + 40 + 3 * DAY)
        .await
        .unwrap();
    assert_eq!(swept.tombstoned, [id]);
}

#[tokio::test]
async fn a_check_can_find_an_unclear_draft_done_or_retry_it() {
    let bench = Bench::new();
    let (id, approved) = running(&bench, ActionKind::Draft).await;
    bench
        .ledger
        .finish_execution(approved, Execution::Ambiguous, T0 + 3)
        .await
        .unwrap();
    bench
        .ledger
        .probed(&id, Probe::NotDone, T0 + 4)
        .await
        .unwrap();
    assert_eq!(bench.entry(&id).state, ActionState::Approved);
    let content = bench.policy().content.read(&id).unwrap();
    let approved = bench
        .ledger
        .begin_execution(&id, content, T0 + 5)
        .await
        .unwrap()
        .unwrap();
    bench
        .ledger
        .finish_execution(approved, Execution::Ambiguous, T0 + 6)
        .await
        .unwrap();
    bench.ledger.probed(&id, Probe::Done, T0 + 7).await.unwrap();
    let done = bench.entry(&id);
    assert_eq!(done.state, ActionState::Done);
    assert_eq!(done.outcome.unwrap().code, OutcomeCode::AlreadyDone);
}

#[tokio::test]
async fn a_mutation_401_stays_unknown_when_a_check_does_not_find_it() {
    for kind in [ActionKind::Draft, ActionKind::MarkRead, ActionKind::Send] {
        let bench = Bench::new();
        let (id, approved) = running(&bench, kind).await;
        bench
            .ledger
            .finish_execution(approved, Execution::Uncertain, T0 + 3)
            .await
            .unwrap();
        let executing = bench.entry(&id);
        assert_eq!(executing.state, ActionState::Executing, "{kind:?}");
        assert!(executing.uncertain, "{kind:?}");
        assert_eq!(bench.ledger.due_probes(T0 + 3), std::slice::from_ref(&id));
        bench
            .ledger
            .probed(&id, Probe::NotDone, T0 + 4)
            .await
            .unwrap();
        let unknown = bench.entry(&id);
        assert_eq!(unknown.state, ActionState::Unknown, "{kind:?} {unknown:?}");
        assert_eq!(unknown.outcome.unwrap().code, OutcomeCode::AuthUncertain);
        assert!(unknown.uncertain, "the 401 is remembered");
        assert_eq!(unknown.attempts, 1, "it was not carried out again");
        assert_eq!(bench.ledger.next_approved(T0 + 4), None);
        assert_eq!(
            bench.ledger.snapshot().reservations.len(),
            1,
            "it may have happened, so the daily reservation stays"
        );
        let told = bench
            .ledger
            .snapshot()
            .queue
            .iter()
            .find(|item| item.key == format!("outcome:{id}"))
            .map(|item| item.text.clone())
            .unwrap_or_default();
        assert!(
            told.contains("refused the access token")
                && told.contains("will not retry")
                && told.contains("Propose it again only if it did not happen"),
            "{kind:?}: {told}"
        );
        let again = bench
            .approve(&["RRRRRR"], "om_again", Some(DELIVERED_MS), T0 + 5)
            .await;
        assert!(
            again[0].contains("will not retry") && again[0].contains("Propose it again"),
            "{kind:?}: {again:?}"
        );
        assert_eq!(bench.entry(&id).state, ActionState::Unknown);
        let content = bench.policy().content.read(&id).unwrap();
        assert_eq!(
            bench
                .ledger
                .begin_execution(&id, content, T0 + 6)
                .await
                .unwrap()
                .err(),
            Some(NotStarted::NotReady)
        );
        let log = std::fs::read_to_string(bench.home.path().join("state/mail/default/audit.jsonl"))
            .unwrap();
        let unknown_lines: Vec<_> = log
            .lines()
            .filter(|line| line.contains(&id) && line.contains("\"unknown\""))
            .collect();
        assert_eq!(unknown_lines.len(), 1, "{kind:?} {log}");
        assert!(
            unknown_lines[0].contains("\"auth_uncertain\""),
            "{kind:?} {log}"
        );
        assert!(!log.contains("RRRRRR"), "{kind:?} code in the audit log");
    }
}

#[tokio::test]
async fn a_mutation_401_a_check_finds_done_is_recorded_done() {
    let bench = Bench::new();
    let (id, approved) = running(&bench, ActionKind::MarkRead).await;
    bench
        .ledger
        .finish_execution(approved, Execution::Uncertain, T0 + 3)
        .await
        .unwrap();
    bench.ledger.probed(&id, Probe::Done, T0 + 4).await.unwrap();
    let done = bench.entry(&id);
    assert_eq!(done.state, ActionState::Done);
    assert_eq!(done.outcome.unwrap().code, OutcomeCode::AlreadyDone);
    assert_eq!(bench.ledger.next_approved(T0 + 4), None);
    assert_eq!(done.attempts, 1);
}

#[tokio::test]
async fn a_mutation_401_does_not_resume_or_claim_the_change_failed() {
    for probe in [
        Probe::Gone,
        Probe::Unknown,
        Probe::Resume(Resume::AfterCopy),
    ] {
        let bench = Bench::new();
        let (id, approved) = running(&bench, ActionKind::Draft).await;
        bench
            .ledger
            .finish_execution(approved, Execution::Uncertain, T0 + 3)
            .await
            .unwrap();
        bench.ledger.probed(&id, probe, T0 + 4).await.unwrap();
        let unknown = bench.entry(&id);
        assert_eq!(unknown.state, ActionState::Unknown, "{probe:?} {unknown:?}");
        assert_eq!(unknown.outcome.unwrap().code, OutcomeCode::AuthUncertain);
        assert_eq!(unknown.resume, None, "a 401 is not taken up as a move");
        assert_eq!(bench.ledger.next_approved(T0 + 4), None);
    }
}

#[tokio::test]
async fn recovery_keeps_a_mutation_401_unknown_after_a_missed_check() {
    let home = tempfile::tempdir().unwrap();
    let first = ledger(home.path()).with_actions(policy(home.path(), settings()));
    let bench = Bench {
        home,
        ledger: first,
        world: FakeWorld::new(),
    };
    let (id, approved) = running(&bench, ActionKind::Draft).await;
    bench
        .ledger
        .finish_execution(approved, Execution::Uncertain, T0 + 3)
        .await
        .unwrap();
    bench
        .ledger
        .probed(&id, Probe::Unreachable, T0 + 4)
        .await
        .unwrap();
    let waiting = bench.entry(&id);
    assert_eq!(waiting.state, ActionState::Executing);
    assert!(waiting.uncertain);
    assert!(waiting.probe_after.unwrap() > T0 + 4);
    let Bench {
        home, ledger: old, ..
    } = bench;
    drop(old);
    let restarted = ledger(home.path()).with_actions(policy(home.path(), settings()));
    restarted.recover(T0 + 5).await.unwrap();
    let recovered = restarted
        .snapshot()
        .actions
        .into_iter()
        .find(|entry| entry.id == id)
        .unwrap();
    assert_eq!(recovered.state, ActionState::Executing);
    assert!(recovered.uncertain, "restart keeps the 401");
    restarted.probed(&id, Probe::NotDone, T0 + 6).await.unwrap();
    let unknown = restarted
        .snapshot()
        .actions
        .into_iter()
        .find(|entry| entry.id == id)
        .unwrap();
    assert_eq!(unknown.state, ActionState::Unknown);
    assert_eq!(unknown.outcome.unwrap().code, OutcomeCode::AuthUncertain);
    assert_eq!(restarted.next_approved(T0 + 6), None);
}

#[tokio::test]
async fn a_half_done_move_is_taken_up_where_it_stopped() {
    let bench = Bench::new();
    let (id, approved) = running(&bench, ActionKind::Trash).await;
    bench
        .ledger
        .finish_execution(approved, Execution::Ambiguous, T0 + 3)
        .await
        .unwrap();
    bench
        .ledger
        .probed(&id, Probe::Resume(Resume::AfterCopy), T0 + 4)
        .await
        .unwrap();
    let content = bench.policy().content.read(&id).unwrap();
    let approved = bench
        .ledger
        .begin_execution(&id, content, T0 + 5)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approved.resume(), Some(Resume::AfterCopy));
    assert_eq!(bench.entry(&id).resume, None, "a resume is taken once");

    // The mailbox cannot be reached this time: nothing changed, so the
    // next attempt takes the move up where it stood rather than copying
    // the message again.
    bench
        .ledger
        .finish_execution(
            approved,
            Execution::NotApplied {
                retry: true,
                code: OutcomeCode::Unreachable,
            },
            T0 + 6,
        )
        .await
        .unwrap();
    let waiting = bench.entry(&id);
    assert_eq!(waiting.state, ActionState::Approved);
    assert_eq!(waiting.resume, Some(Resume::AfterCopy));
    let content = bench.policy().content.read(&id).unwrap();
    let again = bench
        .ledger
        .begin_execution(&id, content, waiting.not_before)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.resume(), Some(Resume::AfterCopy));
}

#[tokio::test]
async fn a_send_copied_to_sent_by_scv_is_done_before_the_copy() {
    let bench = Bench::new();
    let (id, approved) = running(&bench, ActionKind::Send).await;
    bench.ledger.mark_sent(&approved, T0 + 3).await.unwrap();
    let sent = bench.entry(&id);
    assert_eq!(sent.state, ActionState::Done);
    assert_eq!(
        sent.outcome.as_ref().unwrap().sent_copy,
        Some(SentCopyState::Pending)
    );
    // A sweep while the copy is being saved leaves the send to be told.
    let swept = bench.ledger.sweep_actions(DAY, T0 + 3).await.unwrap();
    assert!(swept.tombstoned.is_empty(), "{swept:?}");
    assert_eq!(bench.entry(&id).state, ActionState::Done);
    bench
        .ledger
        .finish_execution(
            approved,
            Execution::Applied {
                code: OutcomeCode::Applied,
                sent_copy: Some(SentCopyState::Failed),
            },
            T0 + 4,
        )
        .await
        .unwrap();
    let done = bench.entry(&id);
    assert_eq!(done.outcome.unwrap().sent_copy, Some(SentCopyState::Failed));
    assert!(bench.ledger.snapshot().queue.iter().any(|item| item.text
        == "Done: RRRRRR sent the reply to #4K7P; its copy in Sent could not be saved."));
}

#[tokio::test]
async fn previews_expire_before_delivery_at_their_hard_limit() {
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    assert_eq!(bench.ledger.expire(T0 + 72 * HOUR - 1).await.unwrap(), 0);
    assert_eq!(bench.ledger.expire(T0 + 72 * HOUR).await.unwrap(), 2);
    assert_eq!(bench.entry(&draft).state, ActionState::Expired);
    assert_eq!(bench.ledger.snapshot().counts.expired, 2);
}

#[tokio::test]
async fn a_refused_preview_is_offered_again_with_new_codes_at_most_twice() {
    let bench = Bench::new();
    let content = bench.reply(ActionKind::Draft, None);
    bench
        .propose("owner:x:1", std::slice::from_ref(&content), &["FIRST2"])
        .await;
    let mut codes = vec!["SECND2".to_owned(), "THIRD2".to_owned()].into_iter();
    for expected in ["SECND2", "THIRD2"] {
        let snapshot = bench.ledger.snapshot();
        let seqs = snapshot.queue.iter().map(|item| item.seq).collect();
        let batch = bench
            .ledger
            .begin_batch(seqs, SendClass::Response, "t".into(), Counts::default(), T0)
            .await
            .unwrap();
        bench.ledger.batch_stored(ROUTE, OWNER, T0).await.unwrap();
        let again = bench
            .ledger
            .settle_watched(&[], std::slice::from_ref(&batch.key), &mut || codes.next())
            .await
            .unwrap();
        assert_eq!(again, std::slice::from_ref(&content.id));
        let entry = bench.entry(&content.id);
        assert_eq!(
            (entry.state, entry.code.as_str()),
            (ActionState::Proposed, expected)
        );
        bench
            .ledger
            .queue_preview(
                again,
                preview_key(&content.id, entry.generation),
                "again".into(),
                T0,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        bench
            .approve(&["FIRST2"], "om_1", Some(DELIVERED_MS), T0)
            .await,
        ["FIRST2 was replaced by a newer request."]
    );
    let snapshot = bench.ledger.snapshot();
    let seqs = snapshot.queue.iter().map(|item| item.seq).collect();
    let batch = bench
        .ledger
        .begin_batch(seqs, SendClass::Response, "t".into(), Counts::default(), T0)
        .await
        .unwrap();
    bench.ledger.batch_stored(ROUTE, OWNER, T0).await.unwrap();
    let again = bench
        .ledger
        .settle_watched(&[], std::slice::from_ref(&batch.key), &mut || {
            Some("NEVER2".into())
        })
        .await
        .unwrap();
    assert!(again.is_empty());
    assert_eq!(bench.entry(&content.id).state, ActionState::Expired);
}

#[tokio::test]
async fn a_fast_approval_finds_the_delivery_the_notifier_has_not_seen_yet() {
    let bench = Bench::new();
    let content = bench.reply(ActionKind::Draft, None);
    bench
        .propose("owner:x:1", std::slice::from_ref(&content), &["QUICK2"])
        .await;
    let snapshot = bench.ledger.snapshot();
    let seqs = snapshot.queue.iter().map(|item| item.seq).collect();
    bench
        .ledger
        .begin_batch(seqs, SendClass::Response, "t".into(), Counts::default(), T0)
        .await
        .unwrap();
    bench.ledger.batch_stored(ROUTE, OWNER, T0).await.unwrap();
    *bench.world.delivered.lock().unwrap() = Some(KeyedOutcome::Delivered {
        at_ms: DELIVERED_MS,
    });
    assert_eq!(
        bench
            .approve(&["QUICK2"], "om_1", Some(DELIVERED_MS + 1), T0 + 11)
            .await,
        ["Approved QUICK2: saving the reply to #4K7P in Drafts."]
    );
}

#[tokio::test]
async fn recovery_applies_the_settings_the_account_starts_with() {
    let home = tempfile::tempdir().unwrap();
    let first = ledger(home.path()).with_actions(policy(home.path(), settings()));
    let bench = Bench {
        home,
        ledger: first,
        world: FakeWorld::new(),
    };
    let (draft, send) = bench.reply_pair().await;
    let trash = bench.reply(ActionKind::Trash, None);
    bench
        .propose("owner:x:2", std::slice::from_ref(&trash), &["TRASH2"])
        .await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["TRASH2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let content = bench.policy().content.read(&trash.id).unwrap();
    let _interrupted = bench
        .ledger
        .begin_execution(&trash.id, content, T0 + 2)
        .await
        .unwrap()
        .unwrap();
    let Bench {
        home, ledger: old, ..
    } = bench;
    drop(old);
    // Restarted with sending turned off and a shorter approval window.
    let restarted = ledger(home.path()).with_actions(policy(
        home.path(),
        ActionSettings {
            send: ActionMode::Off,
            approval_hours: 1,
            ..settings()
        },
    ));
    let recovered = restarted.recover(T0 + 3).await.unwrap();
    let state = restarted.snapshot();
    let find = |id: &str| {
        state
            .actions
            .iter()
            .find(|entry| entry.id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(find(&send).state, ActionState::Cancelled);
    assert_eq!(find(&draft).expires_at, T0 + 10 + HOUR);
    assert_eq!(find(&trash.id).state, ActionState::Executing);
    assert_eq!(
        restarted.due_probes(T0 + 3),
        std::slice::from_ref(&trash.id)
    );
    let mut codes = recovered.codes.clone();
    codes.sort();
    assert_eq!(codes, ["D2D2D2", "S3S3S3", "TRASH2"]);
    assert!(recovered.unpreviewed.is_empty());
}

#[tokio::test]
async fn the_audit_log_records_each_change_without_codes_or_mail() {
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["D2D2D2"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let content = bench.policy().content.read(&draft).unwrap();
    let approved = bench
        .ledger
        .begin_execution(&draft, content, T0 + 2)
        .await
        .unwrap()
        .unwrap();
    bench
        .ledger
        .finish_execution(
            approved,
            Execution::Applied {
                code: OutcomeCode::Applied,
                sent_copy: None,
            },
            T0 + 3,
        )
        .await
        .unwrap();
    let log =
        std::fs::read_to_string(bench.home.path().join("state/mail/default/audit.jsonl")).unwrap();
    let events: Vec<String> = log
        .lines()
        .map(|line| serde_json::from_str::<crate::email::audit::Line>(line).unwrap())
        .filter(|line| line.id == draft)
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
            "done"
        ]
    );
    for secret in ["D2D2D2", "4K7P", "alice@example.com", "Thanks", "Hello"] {
        assert!(!log.contains(secret), "{secret} in the audit log");
    }
    assert!(log.contains(ROUTE), "the approving chat is recorded");
}

#[tokio::test]
async fn a_saved_transition_keeps_its_audit_line_when_the_fold_is_interrupted() {
    let bench = Bench::new();
    let (draft, _) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench.ledger.hold_audit.store(true, Ordering::Relaxed);
    assert_eq!(
        bench.deny_codes(Some(&["D2D2D2"]), T0 + 1).await,
        ["Denied D2D2D2 (saving the reply to #4K7P in Drafts)."]
    );
    assert_eq!(bench.entry(&draft).state, ActionState::Denied);
    let pending = bench
        .home
        .path()
        .join("state/mail/default/audit.jsonl.pending");
    assert!(
        pending.is_file(),
        "the audit journal survives the state write"
    );
    let log =
        std::fs::read_to_string(bench.home.path().join("state/mail/default/audit.jsonl")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("\"denied\""))
            .count(),
        0,
        "the fold did not land: {log}"
    );
    bench.ledger.hold_audit.store(false, Ordering::Relaxed);
    bench.ledger.recover(T0 + 2).await.unwrap();
    assert!(!pending.exists(), "recovery folds the journal");
    let log =
        std::fs::read_to_string(bench.home.path().join("state/mail/default/audit.jsonl")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains(&draft) && line.contains("\"denied\""))
            .count(),
        1,
        "{log}"
    );
    assert_eq!(bench.entry(&draft).state, ActionState::Denied);
}

#[tokio::test]
async fn deny_stays_inside_the_chat_that_was_asked_and_ignores_an_old_code() {
    const OTHER_ROUTE: &str = "wechat:mail";
    const OTHER_OWNER: &str = "wx_owner";
    let bench = Bench::with_routes(settings(), vec![ROUTE.into(), OTHER_ROUTE.into()]);
    bench.world.set_owner(OTHER_ROUTE, Some(OTHER_OWNER));
    let (here, sibling) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    let there = bench.reply(ActionKind::Archive, None);
    bench
        .propose(
            "owner:wechat:mail:om_other",
            std::slice::from_ref(&there),
            &["ARCHV2"],
        )
        .await;
    bench
        .deliver_to(OTHER_ROUTE, OTHER_OWNER, T0 + 1, DELIVERED_MS)
        .await;

    let stranger = ChatEvidence {
        route: OTHER_ROUTE.into(),
        peer: OTHER_OWNER.into(),
        ..evidence("om_other", None)
    };
    assert_eq!(
        bench.deny_as(Some(&["D2D2D2"]), &stranger, T0 + 2).await,
        ["D2D2D2 was sent to another chat; answer it there."]
    );
    assert_eq!(
        bench.deny_as(None, &stranger, T0 + 2).await,
        ["Denied the one action that was waiting."]
    );
    assert_eq!(bench.entry(&here).state, ActionState::Open);
    assert_eq!(bench.entry(&sibling).state, ActionState::Open);
    assert_eq!(bench.entry(&there.id).state, ActionState::Denied);

    let former = evidence("om_former", None);
    bench.world.set_owner(ROUTE, Some("ou_new"));
    assert_eq!(
        bench.deny_as(Some(&["D2D2D2"]), &former, T0 + 3).await,
        ["D2D2D2 was sent to another chat; answer it there."]
    );
    assert_eq!(bench.entry(&here).state, ActionState::Open);
    bench.world.set_owner(ROUTE, Some(OWNER));
    assert_eq!(
        bench.deny_as(Some(&["S3S3S3"]), &former, T0 + 4).await,
        ["Denied S3S3S3 (sending the reply to #4K7P)."]
    );
    assert_eq!(bench.entry(&sibling).state, ActionState::Denied);
    assert_eq!(bench.entry(&here).state, ActionState::Open);

    // Proposed before a preview is stored, and already bound to this chat.
    // Someone who is not an owner cannot withdraw it, and neither can an
    // owner of another chat this account reports to.
    let waiting = bench.reply(ActionKind::MarkRead, None);
    bench
        .propose(
            "owner:feishu:mail:om_wait",
            std::slice::from_ref(&waiting),
            &["MARKR2"],
        )
        .await;
    assert_eq!(
        bench.entry(&waiting.id).preview.route.as_deref(),
        Some(ROUTE)
    );
    assert!(bench.entry(&waiting.id).preview.peer.is_none());
    let outsider = ChatEvidence {
        peer: "ou_stranger".into(),
        ..evidence("om_out", None)
    };
    assert_eq!(
        bench.deny_as(Some(&["MARKR2"]), &outsider, T0 + 5).await,
        ["MARKR2 was sent to another chat; answer it there."]
    );
    assert_eq!(bench.entry(&waiting.id).state, ActionState::Proposed);
    assert_eq!(
        bench.deny_as(None, &outsider, T0 + 5).await,
        ["Nothing was waiting."]
    );
    assert_eq!(bench.entry(&waiting.id).state, ActionState::Proposed);
    assert_eq!(
        bench.deny_as(Some(&["MARKR2"]), &stranger, T0 + 5).await,
        ["MARKR2 was sent to another chat; answer it there."]
    );
    assert_eq!(
        bench.deny_as(None, &stranger, T0 + 5).await,
        ["Nothing was waiting."]
    );
    assert_eq!(bench.entry(&waiting.id).state, ActionState::Proposed);
    assert_eq!(
        bench.deny_codes(Some(&["MARKR2"]), T0 + 5).await,
        ["Denied MARKR2 (marking #4K7P read)."]
    );
    assert_eq!(bench.entry(&waiting.id).state, ActionState::Denied);

    // An earlier generation's code does not deny the action it used to name.
    let reissued = bench.reply(ActionKind::Trash, None);
    bench
        .propose(
            "owner:feishu:mail:om_old",
            std::slice::from_ref(&reissued),
            &["FIRST2"],
        )
        .await;
    let snapshot = bench.ledger.snapshot();
    let seqs = snapshot.queue.iter().map(|item| item.seq).collect();
    let batch = bench
        .ledger
        .begin_batch(
            seqs,
            SendClass::Response,
            "t".into(),
            Counts::default(),
            T0 + 6,
        )
        .await
        .unwrap();
    bench
        .ledger
        .batch_stored(ROUTE, OWNER, T0 + 6)
        .await
        .unwrap();
    bench
        .ledger
        .settle_watched(&[], std::slice::from_ref(&batch.key), &mut || {
            Some("SECND2".into())
        })
        .await
        .unwrap();
    assert_eq!(bench.entry(&reissued.id).code, "SECND2");
    assert_eq!(
        bench.deny_codes(Some(&["FIRST2"]), T0 + 7).await,
        ["FIRST2 was replaced by a newer request."]
    );
    assert_eq!(bench.entry(&reissued.id).state, ActionState::Proposed);
    assert_eq!(bench.entry(&reissued.id).code, "SECND2");
    assert_eq!(
        bench.deny_codes(Some(&["SECND2"]), T0 + 7).await,
        ["Denied SECND2 (moving #4K7P to Trash)."]
    );
    assert_eq!(bench.entry(&reissued.id).state, ActionState::Denied);

    let removed = Bench::with_routes(settings(), Vec::new());
    let gone = removed.reply(ActionKind::Draft, None);
    removed
        .propose("owner:x:1", std::slice::from_ref(&gone), &["D2D2D2"])
        .await;
    removed.deliver(T0, DELIVERED_MS).await;
    assert_eq!(
        removed.deny_codes(Some(&["D2D2D2"]), T0 + 1).await,
        ["Not denied: this chat is no longer one default reports to."]
    );
    assert_eq!(removed.entry(&gone.id).state, ActionState::Open);
    assert_eq!(
        removed.deny_codes(None, T0 + 1).await,
        ["Nothing was waiting."]
    );
    assert_eq!(removed.entry(&gone.id).state, ActionState::Open);
}

#[tokio::test]
async fn deny_before_a_preview_is_stored_cannot_cross_routes() {
    const OTHER_ROUTE: &str = "wechat:mail";
    const OTHER_OWNER: &str = "wx_owner";
    let bench = Bench::with_routes(settings(), vec![ROUTE.into(), OTHER_ROUTE.into()]);
    bench.world.set_owner(OTHER_ROUTE, Some(OTHER_OWNER));
    let here = bench.reply(ActionKind::Trash, None);
    let there = bench.reply(ActionKind::Archive, None);
    let triage = bench.reply(ActionKind::MarkRead, None);
    let elsewhere = bench.reply(ActionKind::Spam, None);
    bench
        .propose(
            "owner:feishu:mail:om_here",
            std::slice::from_ref(&here),
            &["HERE22"],
        )
        .await;
    bench
        .propose(
            "owner:wechat:mail:om_there",
            std::slice::from_ref(&there),
            &["THER22"],
        )
        .await;
    bench
        .propose(
            "triage:imap:7:42",
            std::slice::from_ref(&triage),
            &["TRIAG2"],
        )
        .await;
    // Asked in a chat this account does not report to: the first chat it
    // does report to is the binding, not every configured chat.
    bench
        .propose(
            "owner:fake:chat:om_else",
            std::slice::from_ref(&elsewhere),
            &["ELSE22"],
        )
        .await;
    assert_eq!(bench.entry(&here.id).preview.route.as_deref(), Some(ROUTE));
    assert_eq!(
        bench.entry(&there.id).preview.route.as_deref(),
        Some(OTHER_ROUTE)
    );
    assert_eq!(
        bench.entry(&triage.id).preview.route.as_deref(),
        Some(ROUTE)
    );
    assert_eq!(
        bench.entry(&elsewhere.id).preview.route.as_deref(),
        Some(ROUTE)
    );
    assert!(bench.entry(&here.id).preview.peer.is_none());

    let home = evidence("om_home", None);
    let other = ChatEvidence {
        route: OTHER_ROUTE.into(),
        peer: OTHER_OWNER.into(),
        ..evidence("om_other", None)
    };
    assert_eq!(
        bench.deny_as(None, &other, T0).await,
        ["Denied the one action that was waiting."]
    );
    assert_eq!(bench.entry(&there.id).state, ActionState::Denied);
    assert_eq!(bench.entry(&here.id).state, ActionState::Proposed);
    assert_eq!(bench.entry(&triage.id).state, ActionState::Proposed);
    assert_eq!(bench.entry(&elsewhere.id).state, ActionState::Proposed);
    assert_eq!(
        bench.deny_as(Some(&["HERE22"]), &other, T0).await,
        ["HERE22 was sent to another chat; answer it there."]
    );
    assert_eq!(bench.entry(&here.id).state, ActionState::Proposed);

    let snapshot = bench.ledger.snapshot();
    let seqs = snapshot.queue.iter().map(|item| item.seq).collect();
    let _batch = bench
        .ledger
        .begin_batch(seqs, SendClass::Response, "t".into(), Counts::default(), T0)
        .await
        .unwrap();
    assert_eq!(bench.entry(&here.id).preview.route.as_deref(), Some(ROUTE));
    assert_eq!(bench.entry(&here.id).state, ActionState::Previewing);
    assert_eq!(
        bench.deny_as(None, &other, T0).await,
        ["Nothing was waiting."]
    );
    assert_eq!(bench.entry(&here.id).state, ActionState::Previewing);
    assert_eq!(bench.entry(&triage.id).state, ActionState::Previewing);

    // The chat that stores the preview replaces the proposal binding.
    bench
        .ledger
        .batch_stored(OTHER_ROUTE, OTHER_OWNER, T0)
        .await
        .unwrap();
    assert_eq!(
        bench.entry(&here.id).preview.route.as_deref(),
        Some(OTHER_ROUTE)
    );
    assert_eq!(
        bench.entry(&here.id).preview.peer.as_deref(),
        Some(OTHER_OWNER)
    );
    assert_eq!(
        bench.deny_as(Some(&["HERE22"]), &home, T0).await,
        ["HERE22 was sent to another chat; answer it there."]
    );
    assert_eq!(bench.entry(&here.id).state, ActionState::Previewing);
    assert_eq!(
        bench.deny_as(Some(&["HERE22"]), &other, T0).await,
        ["Denied HERE22 (moving #4K7P to Trash)."]
    );
    assert_eq!(
        bench.deny_as(None, &other, T0).await,
        ["Denied the 2 actions that were waiting."]
    );
    assert_eq!(bench.entry(&triage.id).state, ActionState::Denied);
    assert_eq!(bench.entry(&elsewhere.id).state, ActionState::Denied);

    let missing = bench.reply(ActionKind::Spam, None);
    bench
        .propose(
            "owner:feishu:mail:om_none",
            std::slice::from_ref(&missing),
            &["NONE22"],
        )
        .await;
    bench
        .ledger
        .commit(|state| {
            let entry = state
                .actions
                .iter_mut()
                .find(|entry| entry.id == missing.id)
                .unwrap();
            entry.preview.route = None;
            entry.preview.peer = None;
        })
        .await
        .unwrap();
    for who in [&home, &other] {
        assert_eq!(
            bench.deny_as(Some(&["NONE22"]), who, T0).await,
            ["NONE22 has not reached a chat yet; deny it once it has."]
        );
        assert_eq!(bench.deny_as(None, who, T0).await, ["Nothing was waiting."]);
    }
    assert_eq!(bench.entry(&missing.id).state, ActionState::Proposed);
    assert!(bench.entry(&missing.id).preview.route.is_none());
}

#[tokio::test]
async fn a_revision_replaces_the_waiting_draft_unless_it_was_approved() {
    let bench = Bench::new();
    let (draft, send) = bench.reply_pair().await;
    let revised = bench.reply(ActionKind::Draft, None);
    bench.policy().content.write_new(&revised).unwrap();
    let result = bench
        .ledger
        .propose(
            Some("owner:x:9"),
            vec![NewAction::new(
                &revised,
                "owner:x:9:0".into(),
                "REVIS2".into(),
            )],
            (preview_key(&revised.id, 1), "p".into()),
            vec![draft.clone(), send.clone()],
            T0 + 1,
        )
        .await
        .unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(bench.entry(&draft).state, ActionState::Superseded);
    assert_eq!(bench.entry(&send).state, ActionState::Superseded);

    let bench = Bench::new();
    let (draft, send) = bench.reply_pair().await;
    bench.deliver(T0, DELIVERED_MS).await;
    bench
        .approve(&["S3S3S3"], "om_1", Some(DELIVERED_MS), T0 + 1)
        .await;
    let revised = bench.reply(ActionKind::Draft, None);
    bench.policy().content.write_new(&revised).unwrap();
    let result = bench
        .ledger
        .propose(
            None,
            vec![NewAction::new(
                &revised,
                "owner:x:9:0".into(),
                "REVIS2".into(),
            )],
            (preview_key(&revised.id, 1), "p".into()),
            vec![draft, send.clone()],
            T0 + 2,
        )
        .await
        .unwrap();
    assert_eq!(result, Err(Refusal::Started));
    assert_eq!(bench.entry(&send).state, ActionState::Approved);
}

#[tokio::test]
async fn requests_are_queued_once_and_bounded() {
    let bench = Bench::new();
    let request = |key: &str| Request {
        key: key.into(),
        work: RequestWork::Message {
            handle: "4K7P".into(),
            kind: ActionKind::Trash,
        },
        route: ROUTE.into(),
        attempts: 0,
        at: T0,
    };
    assert_eq!(
        bench
            .ledger
            .add_request(request("owner:r:1"))
            .await
            .unwrap(),
        Admission::Added
    );
    assert_eq!(
        bench
            .ledger
            .add_request(request("owner:r:1"))
            .await
            .unwrap(),
        Admission::Duplicate
    );
    for n in 2..=MAX_REQUESTS {
        assert_eq!(
            bench
                .ledger
                .add_request(request(&format!("owner:r:{n}")))
                .await
                .unwrap(),
            Admission::Added
        );
    }
    assert_eq!(
        bench
            .ledger
            .add_request(request("owner:r:99"))
            .await
            .unwrap(),
        Admission::Full
    );
    assert_eq!(
        bench.ledger.request_attempt("owner:r:1").await.unwrap(),
        Some(1)
    );
    bench
        .ledger
        .drop_request("owner:r:1", "why".into(), T0)
        .await
        .unwrap();
    assert_eq!(
        bench.ledger.request_attempt("owner:r:1").await.unwrap(),
        None
    );
    // A request that already made its actions is not queued again.
    let (_, _) = bench.reply_pair().await;
    assert_eq!(
        bench
            .ledger
            .add_request(request("owner:feishu:mail:om_request"))
            .await
            .unwrap(),
        Admission::Duplicate
    );
}

#[tokio::test]
async fn open_actions_are_bounded() {
    let bench = Bench::with(ActionSettings {
        max_open: 2,
        ..settings()
    });
    bench.reply_pair().await;
    let third = bench.reply(ActionKind::Draft, None);
    bench.policy().content.write_new(&third).unwrap();
    let result = bench
        .ledger
        .propose(
            None,
            vec![NewAction::new(
                &third,
                "owner:x:3:0".into(),
                "THIRD2".into(),
            )],
            (preview_key(&third.id, 1), "p".into()),
            Vec::new(),
            T0,
        )
        .await
        .unwrap();
    assert_eq!(result, Err(Refusal::Full));
}

/// A tiny deterministic generator for the property test.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

#[tokio::test]
async fn nothing_starts_without_an_owner_approval_whatever_else_happens() {
    for seed in 0..12 {
        let mut random = Lcg(seed);
        let bench = Bench::new();
        let (draft, send) = bench.reply_pair().await;
        let mut now = T0;
        let mut approved = false;
        for _ in 0..30 {
            now += random.next(4 * HOUR);
            match random.next(7) {
                0 => {
                    bench.deliver(now, now * 1000).await;
                }
                1 => {
                    bench.ledger.expire(now).await.unwrap();
                }
                2 => {
                    // An approval that fails some check: wrong chat or early.
                    let wrong = ChatEvidence {
                        route: "wechat:mail".into(),
                        ..evidence(&format!("om_{now}"), Some(now * 1000))
                    };
                    bench
                        .ledger
                        .approve(&["D2D2D2".to_owned()], &wrong, &bench.world, now)
                        .await
                        .unwrap();
                    bench
                        .approve(&["S3S3S3"], &format!("om_e{now}"), Some(1), now)
                        .await;
                }
                3 => {
                    bench.deny_codes(None, now).await;
                }
                4 => {
                    bench.ledger.recover(now).await.unwrap();
                }
                5 if !approved && random.next(4) == 0 => {
                    let answers = bench
                        .approve(
                            &["D2D2D2"],
                            &format!("om_ok{now}"),
                            Some(now * 1000 + 999),
                            now,
                        )
                        .await;
                    approved = answers[0].starts_with("Approved");
                }
                _ => {}
            }
            for id in [&draft, &send] {
                let content = bench.policy().content.read(id).unwrap();
                if let Ok(approved_action) = bench
                    .ledger
                    .begin_execution(id, content, now)
                    .await
                    .unwrap()
                {
                    assert!(approved, "seed {seed}: {id} started without an approval");
                    assert_eq!(id, &draft, "seed {seed}: the superseded sibling started");
                    bench
                        .ledger
                        .finish_execution(
                            approved_action,
                            Execution::Applied {
                                code: OutcomeCode::Applied,
                                sent_copy: None,
                            },
                            now,
                        )
                        .await
                        .unwrap();
                }
            }
        }
    }
}

#[test]
fn a_state_without_actions_keeps_the_read_only_shape() {
    let state = MailState::default();
    let text = serde_json::to_string(&state).unwrap();
    for key in [
        "handles",
        "actions",
        "tombstones",
        "requests",
        "approvals",
        "reservations",
    ] {
        assert!(!text.contains(&format!("\"{key}\"")), "{key} in {text}");
    }
    assert!(!text.contains("\"expired\""));
    assert!(!text.contains("\"composed\""));
}

#[test]
fn an_owner_request_names_its_chat_even_when_the_message_id_holds_a_colon() {
    let routes = vec!["feishu:mail".to_owned(), "slack:mail".to_owned()];
    assert_eq!(
        owner_route("owner:slack:mail:D0123:1700000000.000100:0"),
        Some("slack:mail")
    );
    assert_eq!(
        intended_route("owner:slack:mail:D0123:1700000000.000100:0", &routes).as_deref(),
        Some("slack:mail"),
        "a Slack request is bound to the Slack chat it came from"
    );
    assert_eq!(owner_route("owner:feishu:mail:om_1:0"), Some("feishu:mail"));
    assert_eq!(owner_route("owner:feishu:mail"), None);
    assert_eq!(owner_route("triage:feishu:mail:om_1:0"), None);
    assert_eq!(
        intended_route("owner:wechat:gone:m1:0", &routes).as_deref(),
        Some("feishu:mail"),
        "a chat this account no longer reports to falls back to the first"
    );
}
