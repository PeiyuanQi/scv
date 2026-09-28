//! Unit tests for `src/email/worker.rs`.

use super::*;
use crate::email::ledger::MailState;
use crate::email::test_support::{FakeMailbox, FixedClock, START, daemon, ledger, meta};
use crate::email::{LowSpace, MailSettings};
use std::sync::atomic::Ordering;
use tokio::net::UnixListener;

struct Bench {
    home: tempfile::TempDir,
    socket: std::path::PathBuf,
    ledger: Ledger,
    clock: FixedClock,
    low_space: LowSpace,
    mailbox: FakeMailbox,
    settings: MailSettings,
}

impl Bench {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("daemon.sock");
        let ledger = ledger(home.path());
        let table: toml::Table = "[notify]\nroute = [\"feishu:mail\"]\n".parse().unwrap();
        Self {
            socket,
            ledger,
            clock: FixedClock::new(),
            low_space: LowSpace::default(),
            mailbox: FakeMailbox::default(),
            settings: MailSettings::parse(Some(&table)).unwrap(),
            home,
        }
    }

    fn worker(&self) -> Worker<'_> {
        Worker {
            ledger: &self.ledger,
            settings: &self.settings,
            socket: &self.socket,
            cwd: self.home.path(),
            clock: &self.clock,
            low_space: &self.low_space,
            frame: triage::frame("default", ""),
        }
    }

    async fn check(&self) -> MailState {
        let mut mailbox = self.mailbox.clone();
        match self.worker().check(&mut mailbox).await {
            Ok(()) => {}
            Err(CheckError::Source(error) | CheckError::State(error)) => panic!("{error:#}"),
        }
        self.ledger.snapshot()
    }

    /// Start from an empty mailbox: the first check only takes the position.
    async fn started(self) -> Self {
        let state = self.check().await;
        assert!(state.cursor.is_some() && state.queue.is_empty());
        self
    }
}

fn answer(notify: bool, urgent: bool) -> impl Fn(&str) -> String + Send + Sync + 'static {
    move |_| {
        format!("{{\"notify\": {notify}, \"urgent\": {urgent}, \"summary\": [\"Wants a reply.\"]}}")
    }
}

#[tokio::test]
async fn the_first_check_takes_the_position_and_reads_no_old_mail() {
    let bench = Bench::new();
    bench
        .mailbox
        .add(meta(1, "old@example.com", "old"), "old body");
    let state = bench.check().await;
    assert!(state.claims.is_empty() && state.queue.is_empty());
    assert_eq!(bench.mailbox.fetched.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn new_mail_is_triaged_in_a_fresh_tool_free_turn_and_reported() {
    let bench = Bench::new().started().await;
    let listener = UnixListener::bind(&bench.socket).unwrap();
    let (seen, _daemon) = daemon(listener, answer(true, true));
    bench.mailbox.add(
        meta(1, "alice@example.com", "Invoice"),
        "Please pay 100 by Friday.",
    );
    let state = bench.check().await;
    assert!(state.claims.is_empty());
    assert_eq!(state.queue.len(), 1);
    let item = &state.queue[0];
    assert!(item.urgent, "the model's urgent counts by default");
    assert!(item.text.contains("│ Subject: Invoice"), "{}", item.text);
    assert!(item.text.contains("│ Wants a reply."), "{}", item.text);
    assert_eq!(
        (state.day.seen, state.day.triaged, state.day.reported),
        (1, 1, 1)
    );
    assert_eq!(state.day.tokens, 650, "the provider's count is charged");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].start["no_tools"], true);
    assert_eq!(
        seen[0].start["cwd"],
        bench.home.path().display().to_string()
    );
    assert!(seen[0].prompt.contains("Please pay 100 by Friday."));
}

#[tokio::test]
async fn mail_the_model_passes_over_is_counted_not_reported() {
    let bench = Bench::new().started().await;
    let (_seen, _daemon) = daemon(
        UnixListener::bind(&bench.socket).unwrap(),
        answer(false, false),
    );
    bench
        .mailbox
        .add(meta(1, "alice@example.com", "fyi"), "fyi");
    let state = bench.check().await;
    assert!(state.queue.is_empty());
    assert_eq!(state.counts.skipped, 1);
    assert_eq!(state.day.triaged, 1);
}

#[tokio::test]
async fn rules_and_signals_decide_without_a_model() {
    let mut bench = Bench::new();
    bench.settings = MailSettings::parse(Some(
        &"[notify]\nroute = [\"feishu:mail\"]\n[[rules]]\nfrom = [\"@bank.example\"]\naction = \"header\"\nurgent = true\n"
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let bench = bench.started().await;
    let mut list = meta(1, "news@example.com", "Weekly");
    list.signals.list_id = Some("news.example.com".into());
    bench.mailbox.add(list, "news");
    bench
        .mailbox
        .add(meta(2, "noreply@shop.example", "Receipt"), "r");
    bench
        .mailbox
        .add(meta(3, "alerts@bank.example", "Alert"), "a");
    // No daemon runs: a model turn would fail and say so.
    let state = bench.check().await;
    assert_eq!(state.counts.skipped, 1, "the list mail is counted");
    assert_eq!(state.queue.len(), 2);
    assert!(!state.queue[0].urgent && state.queue[0].text.contains("Receipt"));
    assert!(state.queue[1].urgent, "the rule made it urgent");
    assert!(
        state
            .queue
            .iter()
            .all(|item| !item.text.contains("not triaged"))
    );
    assert_eq!(state.day.triaged, 0);
    assert_eq!(
        bench.mailbox.fetched.load(Ordering::Relaxed),
        0,
        "no body was read"
    );
}

#[tokio::test]
async fn a_second_arrival_and_mail_past_the_catch_up_window_are_counted() {
    let bench = Bench::new().started().await;
    let (_seen, _daemon) = daemon(
        UnixListener::bind(&bench.socket).unwrap(),
        answer(true, false),
    );
    bench.mailbox.add(meta(1, "a@example.com", "one"), "x");
    bench.check().await;
    let mut again = meta(2, "a@example.com", "one again");
    again.identity = "identity-1".into();
    bench.mailbox.add(again, "x");
    let mut old = meta(3, "a@example.com", "old");
    old.received_at = START - 25 * 3600;
    bench.mailbox.add(old, "x");
    let state = bench.check().await;
    assert_eq!(state.queue.len(), 1);
    assert_eq!(state.counts.skipped, 2);
}

#[tokio::test]
async fn a_reused_message_id_hides_no_mail() {
    let bench = Bench::new().started().await;
    let (seen, _daemon) = daemon(
        UnixListener::bind(&bench.socket).unwrap(),
        answer(true, false),
    );
    let mut first = meta(1, "bank@example.com", "Your statement");
    first.message_id = Some("<same@example.com>".into());
    bench.mailbox.add(first, "x");
    bench.check().await;
    // A later message reuses its Message-ID: another delivery, maybe a
    // forgery, and never taken for the first.
    let mut forged = meta(2, "attacker@example.net", "Your statement");
    forged.message_id = Some("<same@example.com>".into());
    bench.mailbox.add(forged, "y");
    let state = bench.check().await;
    assert_eq!(state.queue.len(), 2, "both are reported");
    assert_eq!(state.counts.skipped, 0);
    assert_eq!(seen.lock().unwrap().len(), 2, "both are triaged");
    let digest = crate::email::parse::digest(b"<same@example.com>");
    assert_eq!(
        state
            .identities
            .iter()
            .filter(|known| known.message_id.as_deref() == Some(digest.as_str()))
            .count(),
        2
    );
}

#[tokio::test]
async fn a_prompt_over_its_bound_is_never_sent() {
    let bench = Bench::new().started().await;
    let (seen, _daemon) = daemon(
        UnixListener::bind(&bench.socket).unwrap(),
        answer(true, false),
    );
    bench.mailbox.add(meta(1, "a@example.com", "big"), "body");
    let mut worker = bench.worker();
    worker.frame = "f".repeat(triage::MAX_PROMPT_BYTES);
    let mut mailbox = bench.mailbox.clone();
    assert!(worker.check(&mut mailbox).await.is_ok());
    let state = bench.ledger.snapshot();
    assert!(seen.lock().unwrap().is_empty(), "no turn started");
    assert_eq!(state.queue.len(), 1);
    assert!(
        state.queue[0]
            .text
            .contains("(not triaged: too large to read)"),
        "{}",
        state.queue[0].text
    );
    assert_eq!((state.day.triaged, state.day.tokens), (0, 0));
}

#[tokio::test]
async fn without_budget_or_room_this_hour_mail_is_reported_by_its_headers() {
    let mut bench = Bench::new();
    bench.settings.max_tokens_per_day = 1500;
    bench.settings.max_triage_per_hour = 1;
    let bench = bench.started().await;
    let (seen, _daemon) = daemon(
        UnixListener::bind(&bench.socket).unwrap(),
        answer(true, false),
    );
    bench
        .mailbox
        .add(meta(1, "a@example.com", "one"), "body one");
    bench
        .mailbox
        .add(meta(2, "b@example.com", "two"), "body two");
    let state = bench.check().await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    let limited = state
        .queue
        .iter()
        .find(|item| item.text.contains("two"))
        .unwrap();
    assert!(
        limited
            .text
            .contains("(not triaged: this hour's triage limit is reached)"),
        "{}",
        limited.text
    );
    assert!(
        state
            .queue
            .iter()
            .any(|item| item.key.starts_with("system:hourly:"))
    );
    // An hour later the budget, not the hour, holds it back.
    bench.clock.advance(3600);
    bench
        .mailbox
        .add(meta(3, "c@example.com", "three"), &"x".repeat(4000));
    let state = bench.check().await;
    let over = state
        .queue
        .iter()
        .find(|item| item.text.contains("three"))
        .unwrap();
    assert!(
        over.text.contains("today's model budget is used up"),
        "{}",
        over.text
    );
    assert!(
        state
            .queue
            .iter()
            .any(|item| item.key.starts_with("system:budget:"))
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    // A budget of 0 turns the model off without a note.
    let mut off = Bench::new();
    off.settings.max_tokens_per_day = 0;
    let off = off.started().await;
    off.mailbox.add(meta(1, "a@example.com", "one"), "x");
    let state = off.check().await;
    assert_eq!(state.queue.len(), 1);
    assert!(!state.queue[0].text.contains("not triaged"));
}

#[tokio::test]
async fn a_failed_or_unreadable_turn_still_reports_the_mail() {
    let bench = Bench::new().started().await;
    // No daemon: the turn fails.
    bench.mailbox.add(meta(1, "a@example.com", "one"), "x");
    let state = bench.check().await;
    assert!(
        state.queue[0]
            .text
            .contains("(not triaged: the model did not answer)")
    );
    assert_eq!(state.day.triaged, 1, "a tried turn is charged");
    let bench = Bench::new().started().await;
    let (_seen, _daemon) = daemon(UnixListener::bind(&bench.socket).unwrap(), |_| {
        "I think so.".into()
    });
    bench.mailbox.add(meta(1, "a@example.com", "one"), "x");
    let state = bench.check().await;
    assert!(state.queue[0].text.contains("(triage answer unreadable)"));
}

#[tokio::test]
async fn a_message_that_keeps_failing_is_read_by_headers_then_given_up() {
    let bench = Bench::new().started().await;
    bench.mailbox.add(meta(1, "a@example.com", "one"), "x");
    // Claimed, then three earlier attempts crashed before recording.
    let mut mailbox = bench.mailbox.clone();
    let snapshot = bench.ledger.snapshot();
    let Changes::New { refs, next } = mailbox
        .changes(snapshot.cursor.as_ref(), 64, 0)
        .await
        .unwrap()
    else {
        panic!("new mail");
    };
    bench
        .ledger
        .claim(refs.clone(), next, None, START)
        .await
        .unwrap();
    for _ in 0..2 {
        bench.ledger.attempt(&refs[0]).await.unwrap();
    }
    let state = bench.check().await;
    assert!(
        state.queue[0]
            .text
            .contains("not triaged: reading it failed before")
    );
    bench.mailbox.add(meta(2, "b@example.com", "two"), "x");
    let snapshot = bench.ledger.snapshot();
    let Changes::New { refs, next } = mailbox
        .changes(snapshot.cursor.as_ref(), 64, 0)
        .await
        .unwrap()
    else {
        panic!("new mail");
    };
    bench
        .ledger
        .claim(refs.clone(), next, None, START)
        .await
        .unwrap();
    for _ in 0..4 {
        bench.ledger.attempt(&refs[0]).await.unwrap();
    }
    let state = bench.check().await;
    assert_eq!(state.counts.unreadable, 1);
    assert!(state.claims.is_empty());
}

#[tokio::test]
async fn a_lost_connection_keeps_claims_for_the_next_check() {
    let bench = Bench::new().started().await;
    bench.mailbox.add(meta(1, "a@example.com", "one"), "x");
    bench.mailbox.broken.store(true, Ordering::Relaxed);
    let mut mailbox = bench.mailbox.clone();
    assert!(matches!(
        bench.worker().check(&mut mailbox).await,
        Err(CheckError::Source(_))
    ));
    let state = bench.ledger.snapshot();
    assert!(state.queue.is_empty() && state.claims.is_empty());
    bench.mailbox.broken.store(false, Ordering::Relaxed);
    assert_eq!(bench.check().await.queue.len(), 1);
}

#[tokio::test]
async fn a_reset_mailbox_is_resynced_once_without_repeating_reports() {
    let bench = Bench::new().started().await;
    bench.mailbox.add(meta(1, "a@example.com", "one"), "x");
    let state = bench.check().await;
    assert_eq!(state.queue.len(), 1);
    bench.mailbox.reset.store(true, Ordering::Relaxed);
    let state = bench.check().await;
    assert_eq!(state.queue.len(), 2, "one report and the reset line");
    assert!(
        state
            .queue
            .iter()
            .any(|item| item.key.starts_with("system:reset:"))
    );
    assert_eq!(
        state.counts.skipped, 1,
        "the same message is not reported again"
    );
    assert_eq!(state.counts.unlisted, 3);
}

#[tokio::test]
async fn low_disk_space_counts_mail_instead_of_writing_reports() {
    let bench = Bench::new().started().await;
    bench.low_space.set(true);
    bench
        .mailbox
        .add(meta(1, "noreply@example.com", "one"), "x");
    let state = bench.check().await;
    assert!(state.queue.is_empty());
    assert_eq!(state.counts.skipped, 1);
}
