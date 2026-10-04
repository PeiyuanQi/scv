//! Unit tests for `src/email/notify.rs`.

use super::*;
use crate::email::ledger::{Decided, Outcome};
use crate::email::source::SourceRef;
use crate::email::test_support::{FixedClock, START, ledger, mail_chat};
use std::sync::Arc;

struct Bench {
    _home: tempfile::TempDir,
    ledger: Ledger,
    clock: FixedClock,
    settings: MailSettings,
    hub: Arc<Hub>,
}

impl Bench {
    fn new(routes: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let ledger = ledger(home.path());
        let table: toml::Table = format!("[notify]\nroute = {routes}\nsettle_seconds = 60\n")
            .parse()
            .unwrap();
        Self {
            _home: home,
            ledger,
            clock: FixedClock::new(),
            settings: MailSettings::parse(Some(&table)).unwrap(),
            hub: Hub::new(None),
        }
    }

    fn notifier(&self) -> Notifier<'_> {
        Notifier {
            ledger: &self.ledger,
            settings: &self.settings,
            account: "default",
            clock: &self.clock,
            hub: Some(&self.hub),
            previews: None,
        }
    }

    async fn report(&self, uid: u32, urgent: bool) {
        let source = SourceRef::Imap {
            mailbox: "INBOX".into(),
            uidvalidity: 1,
            uid,
        };
        self.ledger
            .finish(
                Decided {
                    identity: None,
                    message_id: None,
                    outcome: Outcome::Report {
                        key: format!("report:{}", source.label()),
                        text: format!(
                            "a{uid}@example.com · 17:00 (sender not verified)\n│ Subject: s{uid}"
                        ),
                        urgent,
                        handle: None,
                        actions: Vec::new(),
                    },
                    source,
                    turned: false,
                    tokens: 0,
                },
                "d",
                self.clock.now(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn reports_wait_for_the_settle_window_then_go_to_the_mail_chat_once() {
    let bench = Bench::new("[\"fake:mail\"]");
    let (texts, _chat) = mail_chat(&bench.hub, "fake:mail", Purpose::Mail);
    bench.report(1, false).await;
    bench.report(2, false).await;
    let wait = bench.notifier().step().await.unwrap();
    assert_eq!(wait, Duration::from_secs(60));
    assert!(texts.lock().unwrap().is_empty());
    bench.clock.advance(60);
    bench.notifier().step().await.unwrap();
    let sent = texts.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let (key, text) = &sent[0];
    let epoch = bench.ledger.snapshot().epoch;
    assert!(key.starts_with(&format!("mail:{epoch}:batch:")), "{key}");
    assert!(
        text.starts_with("Mail · default · 2 new · 17:00 (+08:00)"),
        "{text}"
    );
    assert!(text.contains("│ Subject: s1") && text.contains("│ Subject: s2"));
    let state = bench.ledger.snapshot();
    assert!(state.queue.is_empty() && state.batch.is_none());
    assert_eq!(state.log.len(), 1);
    // Its delivery is then known, and nothing more is sent.
    assert_eq!(bench.notifier().step().await.unwrap(), TICK);
    assert!(bench.ledger.snapshot().watched.is_empty());
    assert_eq!(texts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn urgent_mail_goes_at_once() {
    let bench = Bench::new("[\"fake:mail\"]");
    let (texts, _chat) = mail_chat(&bench.hub, "fake:mail", Purpose::Mail);
    bench.report(1, true).await;
    bench.notifier().step().await.unwrap();
    let text = texts.lock().unwrap()[0].1.clone();
    assert!(text.contains("1 new (1 urgent)"), "{text}");
}

#[tokio::test]
async fn an_ordinary_chat_on_the_route_is_skipped_and_the_next_mail_chat_used() {
    let bench = Bench::new("[\"fake:chat\", \"fake:down\", \"fake:mail\"]");
    let (chat_texts, _chat) = mail_chat(&bench.hub, "fake:chat", Purpose::Chat);
    let (mail_texts, _mail) = mail_chat(&bench.hub, "fake:mail", Purpose::Mail);
    bench.report(1, true).await;
    bench.notifier().step().await.unwrap();
    assert!(
        chat_texts.lock().unwrap().is_empty(),
        "never an ordinary chat"
    );
    assert_eq!(mail_texts.lock().unwrap().len(), 1);
    assert_eq!(bench.ledger.snapshot().watched[0].route, "fake:mail");
}

#[tokio::test]
async fn with_no_mail_chat_running_the_message_waits_and_is_given_up_in_time() {
    let bench = Bench::new("[\"fake:mail\"]");
    bench.report(1, true).await;
    bench.report(2, true).await;
    bench.notifier().step().await.unwrap();
    let batch = bench.ledger.snapshot().batch.unwrap();
    assert_eq!(batch.attempts, 1);
    assert_eq!(batch.next_attempt_at, START + 60);
    // Before then nothing is tried again.
    assert_eq!(
        bench.notifier().step().await.unwrap(),
        Duration::from_secs(60)
    );
    // A mail chat that comes up takes the same key.
    let (texts, _chat) = mail_chat(&bench.hub, "fake:mail", Purpose::Mail);
    bench.clock.advance(60);
    bench.notifier().step().await.unwrap();
    assert_eq!(texts.lock().unwrap()[0].0, batch.key);
    // Past give_up_hours a message nobody took is dropped and counted.
    let other = Bench::new("[\"fake:mail\"]");
    other.report(1, true).await;
    other.notifier().step().await.unwrap();
    other.clock.advance(72 * 3600 + 1);
    other.notifier().step().await.unwrap();
    let state = other.ledger.snapshot();
    assert!(state.batch.is_none() && state.queue.is_empty());
    assert_eq!(state.counts.undelivered, 1);
}

#[tokio::test]
async fn a_refused_digest_is_counted_in_the_next_one() {
    let bench = Bench::new("[\"fake:mail\"]");
    let (_texts, _chat) = mail_chat(&bench.hub, "fake:mail", Purpose::Mail);
    bench.report(1, true).await;
    bench.notifier().step().await.unwrap();
    let key = bench.ledger.snapshot().watched[0].key.clone();
    // The platform refused it after the chat stored it.
    let link = crate::hub::Link::new(Arc::clone(&bench.hub), "fake:mail", Some("owner".into()));
    let (registration, _notices) = link.register_as(Purpose::Mail);
    registration.record_keyed(&key, KeyedOutcome::Refused);
    bench.notifier().step().await.unwrap();
    let state = bench.ledger.snapshot();
    assert!(state.watched.is_empty());
    assert_eq!(state.counts.undelivered, 1);
}

#[tokio::test]
async fn at_the_message_limit_the_owner_is_told_once_and_the_rest_waits() {
    let mut bench = Bench::new("[\"fake:mail\"]");
    bench.settings.notify.max_messages_per_hour = 1;
    let (texts, _chat) = mail_chat(&bench.hub, "fake:mail", Purpose::Mail);
    bench.report(1, false).await;
    bench.clock.advance(60);
    bench.notifier().step().await.unwrap();
    bench.report(2, false).await;
    bench.clock.advance(60);
    // Paused: the pause line is queued as a response and goes at once.
    bench.notifier().step().await.unwrap();
    bench.notifier().step().await.unwrap();
    bench.notifier().step().await.unwrap();
    let sent = texts.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[1]
            .1
            .contains("Mail notifications are paused until 18:01"),
        "{}",
        sent[1].1
    );
    assert_eq!(bench.ledger.snapshot().queue.len(), 1);
}
