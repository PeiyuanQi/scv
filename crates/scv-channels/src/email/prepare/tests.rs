//! Unit tests for `src/email/prepare.rs`.

use super::*;
use crate::email::content::ActionKind;
use crate::email::ledger::Ledger;
use crate::email::ledger::actions::{ActionState, Handle, Refusal};
use crate::email::settings::{ActionMode, ActionSettings, ReplyTo};
use crate::email::source::{AttachmentInfo, Caps, Folders, ProviderKind};
use crate::email::test_support::{self, FakeMailbox, Seen, account, daemon, ledger, meta};
use crate::state::Credentials as _;
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;

const ROUTE: &str = "fake:mail";
const HANDLE: &str = "4K7P";
const START: u64 = test_support::START;

fn action_settings() -> ActionSettings {
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
        content: crate::email::content::ContentStore::new(directory),
        tombstone_seconds: 30 * 86_400,
        unknown_keep_seconds: 3 * 86_400,
        possible: Mutex::new(crate::email::ledger::actions::ALL_KINDS.to_vec()),
    }
}

fn mail_settings() -> MailSettings {
    MailSettings::parse(Some(
        &"[notify]\nroute = [\"fake:mail\"]\n".parse().unwrap(),
    ))
    .unwrap()
}

fn folders() -> Folders {
    Folders {
        drafts: Some("Drafts".into()),
        sent: Some("Sent".into()),
        trash: Some("Trash".into()),
        junk: Some("Junk".into()),
        archive: Some("Archive".into()),
    }
}

struct Bench {
    home: tempfile::TempDir,
    ledger: Ledger,
    clock: test_support::FixedClock,
    low_space: LowSpace,
    settings: MailSettings,
    socket: std::path::PathBuf,
    mailbox: FakeMailbox,
    actions: Actions<'static>,
}

impl Bench {
    fn new(actions: ActionSettings) -> Self {
        let home = tempfile::tempdir().unwrap();
        let ledger = ledger(home.path()).with_actions(policy(home.path(), actions));
        Self {
            socket: home.path().join("daemon.sock"),
            ledger,
            clock: test_support::FixedClock::new(),
            low_space: LowSpace::default(),
            settings: mail_settings(),
            mailbox: FakeMailbox::default(),
            actions: Actions {
                registration: None,
                provider: ProviderKind::Imap,
                own: Some("me@example.com".into()),
                frame: compose::frame("default", ""),
                folders: Mutex::new(folders()),
                can_send: true,
            },
            home,
        }
    }

    fn preparer(&self) -> Preparer<'_> {
        Preparer {
            ledger: &self.ledger,
            policy: self.ledger.actions_policy().unwrap(),
            actions: &self.actions,
            settings: &self.settings,
            socket: &self.socket,
            cwd: self.home.path(),
            clock: &self.clock,
            low_space: &self.low_space,
        }
    }

    /// Remember `handle` as naming `meta`, the message a request will re-read.
    async fn remember(&self, meta: &crate::email::source::Meta) {
        let handle = Handle {
            handle: HANDLE.into(),
            source: meta.source.clone(),
            identity: meta.identity.clone(),
            at: START,
        };
        self.ledger
            .commit(|state| state.handles.push(handle))
            .await
            .unwrap();
    }

    fn daemon(
        &self,
        answer: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<Seen>>> {
        let (seen, task) = daemon(UnixListener::bind(&self.socket).unwrap(), answer);
        drop(task);
        seen
    }

    async fn prepare(&self, work: RequestWork, message: &str) -> Prepared {
        let request = Request {
            key: format!("owner:{ROUTE}:{message}"),
            work,
            route: ROUTE.into(),
            attempts: 0,
            at: START,
        };
        let mut mailbox = self.mailbox.clone();
        self.preparer()
            .request(&mut mailbox, &request)
            .await
            .unwrap_or_else(|lost| panic!("{}", lost.0))
    }
}

fn proposal(prepared: Prepared) -> (Vec<ActionContent>, Vec<String>, String, Vec<String>) {
    match prepared {
        Prepared::Proposal {
            contents,
            codes,
            preview,
            revised,
        } => (contents, codes, preview, revised),
        Prepared::Refused(why) => panic!("refused: {why}"),
    }
}

fn reply_answer(prompt: &str) -> String {
    if prompt.contains("Revise this draft") {
        r#"{"body": "Shorter reply."}"#.to_owned()
    } else if prompt.contains("Draft new mail") {
        r#"{"subject": "Lunch Friday", "body": "Are you free?"}"#.to_owned()
    } else {
        "{\n  \"body\": \"Glad to help.\\nReally.\"\n}".to_owned()
    }
}

#[tokio::test]
async fn a_reply_rereads_the_message_drafts_once_and_offers_both_alternatives() {
    let mut honored = action_settings();
    honored.reply_to = ReplyTo::Honor;
    let bench = Bench::new(honored);
    let seen = bench.daemon(reply_answer);
    let mut mail = meta(1, "alice@example.com", "Hello");
    mail.reply_to = Some(crate::email::source::Address {
        name: String::new(),
        address: "desk@example.com".into(),
    });
    bench.mailbox.add(mail.clone(), "the original body");
    bench.remember(&mail).await;

    let (contents, codes, preview, revised) = proposal(
        bench
            .prepare(
                RequestWork::Reply {
                    handle: HANDLE.into(),
                    text: "say thanks".into(),
                },
                "reply-1",
            )
            .await,
    );
    assert!(revised.is_empty());
    assert_eq!(contents.len(), 2, "a reply offers a draft and a send");
    assert_eq!(codes.len(), 2);
    assert_ne!(codes[0], codes[1], "the alternatives have distinct codes");
    let group = contents[0]
        .group
        .clone()
        .expect("the alternatives share a group");
    assert_eq!(contents[1].group.as_deref(), Some(group.as_str()));
    assert_eq!(
        contents
            .iter()
            .map(|content| content.kind)
            .collect::<Vec<_>>(),
        [ActionKind::Draft, ActionKind::Send]
    );
    let message = contents[0].message.as_ref().unwrap();
    assert_eq!(message.to, ["desk@example.com".to_owned()]);
    assert_eq!(message.subject, "Re: Hello");
    assert_eq!(message.body, "Glad to help.\nReally.");
    assert!(
        preview.contains("Reply to #4K7P")
            && preview.contains("From: me@example.com")
            && preview.contains("To: desk@example.com")
            && preview.contains("Replies go to the Reply-To address desk@example.com")
            && preview.contains("│ Re: Hello")
            && preview.contains("│ Glad to help.")
            && preview.contains("│ Really.")
            && preview.contains(&format!("approve {}", codes[0]))
            && preview.contains(&format!("approve {}", codes[1])),
        "{preview}"
    );
    {
        let turns = seen.lock().unwrap();
        assert_eq!(turns.len(), 1, "a reply takes one drafting turn");
        assert!(
            turns[0].prompt.contains("say thanks"),
            "{}",
            turns[0].prompt
        );
        assert!(
            turns[0].prompt.contains("the original body"),
            "the turn was shown the message re-read from the mailbox"
        );
    }

    let mut ignored = action_settings();
    ignored.reply_to = ReplyTo::Ignore;
    let bench = Bench::new(ignored);
    let _seen = bench.daemon(reply_answer);
    bench.mailbox.add(mail.clone(), "the original body");
    bench.remember(&mail).await;
    let (_, _, preview, _) = proposal(
        bench
            .prepare(
                RequestWork::Reply {
                    handle: HANDLE.into(),
                    text: String::new(),
                },
                "reply-ignore",
            )
            .await,
    );
    assert!(preview.contains("To: alice@example.com"), "{preview}");
    assert!(
        !preview.contains("Reply-To"),
        "ignoring Reply-To keeps the sender: {preview}"
    );
}

#[tokio::test]
async fn a_reply_is_refused_when_the_message_changed_or_must_not_be_answered() {
    let bench = Bench::new(action_settings());
    let mail = meta(1, "alice@example.com", "Hello");
    bench.remember(&mail).await;
    let reply = || RequestWork::Reply {
        handle: HANDLE.into(),
        text: String::new(),
    };

    let Prepared::Refused(missing) = bench.prepare(reply(), "missing").await else {
        panic!("a missing message was prepared");
    };
    assert!(
        missing.contains("#4K7P") && missing.contains("no longer in the mailbox"),
        "{missing}"
    );

    let mut changed = mail.clone();
    changed.identity = "someone-else".into();
    bench.mailbox.add(changed, "body");
    let Prepared::Refused(changed) = bench.prepare(reply(), "changed").await else {
        panic!("a changed message was prepared");
    };
    assert!(changed.contains("not the mail it was"), "{changed}");

    bench.mailbox.messages.lock().unwrap().clear();
    let mut bulk = mail.clone();
    bulk.signals.list_unsubscribe = true;
    bench.mailbox.add(bulk, "unsubscribe");
    let Prepared::Refused(bulk) = bench.prepare(reply(), "bulk").await else {
        panic!("bulk mail was prepared");
    };
    assert!(bulk.contains("bulk or automated"), "{bulk}");

    bench.mailbox.messages.lock().unwrap().clear();
    let mut own = mail.clone();
    own.from.as_mut().unwrap().address = "me@example.com".into();
    bench.mailbox.add(own, "sent it myself");
    let Prepared::Refused(own) = bench.prepare(reply(), "self").await else {
        panic!("self-sent mail was prepared");
    };
    assert!(own.contains("came from this mailbox itself"), "{own}");
}

#[tokio::test]
async fn a_forward_copies_the_original_without_a_model_turn() {
    let bench = Bench::new(action_settings());
    let mut mail = meta(1, "alice@example.com", "Hello");
    mail.date = Some("Fri, 4 Oct 2024 09:00:00 +0000".into());
    mail.attachments.push(AttachmentInfo {
        name: "notes.pdf".into(),
        mime: "application/pdf".into(),
        size: 10,
    });
    bench.mailbox.add(mail.clone(), "the forwarded text");
    bench.remember(&mail).await;

    let (contents, codes, preview, _) = proposal(
        bench
            .prepare(
                RequestWork::Forward {
                    handle: HANDLE.into(),
                    to: vec!["bob@example.com".into()],
                    note: "Please see this.".into(),
                },
                "forward-1",
            )
            .await,
    );
    assert_eq!(codes.len(), 2);
    let message = contents[0].message.as_ref().unwrap();
    assert_eq!(message.to, ["bob@example.com".to_owned()]);
    assert_eq!(message.subject, "Fwd: Hello");
    assert!(
        message.body.contains("Please see this."),
        "{}",
        message.body
    );
    assert!(
        message
            .body
            .contains("---------- Forwarded message ----------"),
        "{}",
        message.body
    );
    assert!(
        message.body.contains("the forwarded text"),
        "{}",
        message.body
    );
    assert!(message.body.contains("Subject: Hello"), "{}", message.body);
    assert!(
        preview.contains("The original's 1 attachment(s) are not forwarded."),
        "{preview}"
    );
    assert!(preview.contains("│ Please see this."), "{preview}");
    assert!(
        preview.contains("│ ---------- Forwarded message ----------"),
        "{preview}"
    );
    assert!(
        preview.contains(&format!("approve {}", codes[0])),
        "{preview}"
    );
}

#[tokio::test]
async fn new_mail_uses_the_models_subject_and_body() {
    let bench = Bench::new(action_settings());
    let seen = bench.daemon(reply_answer);
    let (contents, codes, preview, revised) = proposal(
        bench
            .prepare(
                RequestWork::Compose {
                    to: vec!["bob@example.com".into()],
                    text: "ask about Friday".into(),
                },
                "compose-1",
            )
            .await,
    );
    assert!(revised.is_empty());
    assert!(contents.iter().all(|content| content.source.is_none()));
    let message = contents[0].message.as_ref().unwrap();
    assert_eq!(message.subject, "Lunch Friday");
    assert_eq!(message.body, "Are you free?");
    assert_eq!(message.to, ["bob@example.com".to_owned()]);
    assert_eq!(message.from.address, "me@example.com");
    assert!(preview.contains("New mail"), "{preview}");
    assert!(preview.contains("│ Lunch Friday"), "{preview}");
    assert!(preview.contains("│ Are you free?"), "{preview}");
    assert!(preview.contains("To: bob@example.com"), "{preview}");
    for code in &codes {
        assert!(preview.contains(&format!("approve {code}")), "{preview}");
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_revision_supersedes_the_old_group() {
    let bench = Bench::new(action_settings());
    let _seen = bench.daemon(reply_answer);
    let mail = meta(1, "alice@example.com", "Hello");
    bench.mailbox.add(mail.clone(), "the original body");
    bench.remember(&mail).await;
    let (contents, codes, _, _) = proposal(
        bench
            .prepare(
                RequestWork::Reply {
                    handle: HANDLE.into(),
                    text: "say thanks".into(),
                },
                "reply-1",
            )
            .await,
    );
    let old: Vec<String> = contents.iter().map(|content| content.id.clone()).collect();
    let old_codes = codes.clone();
    bench
        .preparer()
        .record(
            Some("owner:fake:mail:reply-1"),
            "owner:fake:mail:reply-1",
            contents,
            codes,
            "preview".into(),
            Vec::new(),
        )
        .await
        .unwrap()
        .unwrap();

    let (revised_contents, new_codes, preview, revised) = proposal(
        bench
            .prepare(
                RequestWork::Revise {
                    action: old[0].clone(),
                    text: "make it shorter".into(),
                },
                "revise-1",
            )
            .await,
    );
    assert_eq!(revised, old, "the whole old group is replaced");
    let body = &revised_contents[0].message.as_ref().unwrap().body;
    assert_eq!(body, "Shorter reply.");
    assert!(preview.contains("│ Shorter reply."), "{preview}");
    assert_ne!(new_codes, old_codes, "the revision draws new codes");
    bench
        .preparer()
        .record(
            Some("owner:fake:mail:revise-1"),
            "owner:fake:mail:revise-1",
            revised_contents,
            new_codes.clone(),
            preview,
            revised,
        )
        .await
        .unwrap()
        .unwrap();
    let state = bench.ledger.snapshot();
    for id in &old {
        let entry = state.actions.iter().find(|entry| entry.id == *id).unwrap();
        assert_eq!(entry.state, ActionState::Superseded, "{entry:?}");
    }
    let live: Vec<_> = state
        .actions
        .iter()
        .filter(|entry| !entry.state.terminal())
        .collect();
    assert_eq!(live.len(), 2, "{:?}", state.actions);
    let live_codes: Vec<_> = live.iter().map(|entry| entry.code.as_str()).collect();
    assert_eq!(
        live_codes,
        new_codes.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn drafting_open_actions_and_low_disk_stop_preparation() {
    let mut limited = action_settings();
    limited.max_compose_per_day = 0;
    let bench = Bench::new(limited);
    let mail = meta(1, "alice@example.com", "Hello");
    bench.mailbox.add(mail.clone(), "body");
    bench.remember(&mail).await;
    let Prepared::Refused(why) = bench
        .prepare(
            RequestWork::Reply {
                handle: HANDLE.into(),
                text: String::new(),
            },
            "limit",
        )
        .await
    else {
        panic!("a turn past the daily limit was prepared");
    };
    assert!(why.contains("today's limit of drafting turns"), "{why}");

    let mut open = action_settings();
    open.max_open = 1;
    let bench = Bench::new(open);
    let mail = meta(1, "alice@example.com", "Hello");
    bench.remember(&mail).await;
    bench.mailbox.add(mail, "body");
    let preparer = bench.preparer();
    let Prepared::Proposal {
        contents,
        codes,
        preview,
        ..
    } = preparer.change(
        Origin::Owner {
            route: ROUTE.into(),
            message_id: "mark".into(),
        },
        &meta(1, "alice@example.com", "Hello"),
        HANDLE,
        ActionKind::MarkRead,
        None,
    )
    else {
        panic!("the mark was refused");
    };
    preparer
        .record(None, "owner:mark", contents, codes, preview, Vec::new())
        .await
        .unwrap()
        .unwrap();
    let Prepared::Refused(why) = bench
        .prepare(
            RequestWork::Message {
                handle: HANDLE.into(),
                kind: ActionKind::Trash,
            },
            "another",
        )
        .await
    else {
        panic!("an action past max_open was prepared");
    };
    assert!(why.contains("max_open"), "{why}");

    let bench = Bench::new(action_settings());
    bench.low_space.set(true);
    let mail = meta(1, "alice@example.com", "Hello");
    bench.remember(&mail).await;
    bench.mailbox.add(mail, "body");
    let Prepared::Refused(why) = bench
        .prepare(
            RequestWork::Message {
                handle: HANDLE.into(),
                kind: ActionKind::Trash,
            },
            "disk",
        )
        .await
    else {
        panic!("low disk still prepared an action");
    };
    assert!(why.contains("short of disk space"), "{why}");
}

#[tokio::test]
async fn a_refused_record_removes_the_content_it_wrote() {
    let mut actions = action_settings();
    actions.max_open = 0;
    let bench = Bench::new(actions);
    let content = compose::change_action(
        &Base {
            account: "default",
            fingerprint: &bench.ledger.actions_policy().unwrap().fingerprint,
            origin: Origin::Owner {
                route: ROUTE.into(),
                message_id: "mark".into(),
            },
            source: None,
            display: None,
            now: START,
            hard_expiry: START + 3600,
        },
        ActionKind::MarkRead,
        None,
    );
    let id = content.id.clone();
    let result = bench
        .preparer()
        .record(
            None,
            "origin",
            vec![content],
            vec!["MARK22".into()],
            "preview".into(),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, Err(Refusal::Full));
    assert!(
        bench
            .ledger
            .actions_policy()
            .unwrap()
            .content
            .read(&id)
            .unwrap()
            .is_none(),
        "the content file survived a refused record"
    );
    assert!(bench.ledger.snapshot().actions.is_empty());
}

#[test]
fn possible_kinds_follow_the_capabilities_the_folders_and_sending() {
    let caps = Caps::default();
    let none = Folders::default();
    assert_eq!(
        possible(&caps, &none, false),
        vec![ActionKind::MarkRead],
        "without folders or sending, only a mark is possible"
    );
    let drafts = Folders {
        drafts: Some("Drafts".into()),
        ..Folders::default()
    };
    assert_eq!(
        possible(&caps, &drafts, false),
        vec![ActionKind::MarkRead, ActionKind::Draft]
    );
    assert!(possible(&caps, &none, true).contains(&ActionKind::Send));

    let special = Folders {
        trash: Some("Trash".into()),
        junk: Some("Junk".into()),
        archive: Some("Archive".into()),
        ..Folders::default()
    };
    let without_move = possible(&caps, &special, false);
    assert!(
        !without_move.contains(&ActionKind::Trash)
            && !without_move.contains(&ActionKind::Spam)
            && !without_move.contains(&ActionKind::Archive),
        "{without_move:?}"
    );
    let mut movable = caps;
    movable.can_move = true;
    let with_move = possible(&movable, &special, false);
    for kind in [ActionKind::Trash, ActionKind::Spam, ActionKind::Archive] {
        assert!(
            with_move.contains(&kind),
            "{kind:?} missing from {with_move:?}"
        );
    }
}
