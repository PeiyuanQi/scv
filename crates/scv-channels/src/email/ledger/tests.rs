//! Unit tests for `src/email/ledger.rs`.

use super::*;
use scv_client::Layout;

fn account() -> Account {
    Account::Imap {
        host: "imap.example.com".into(),
        port: 993,
        username: "me".into(),
        password: "secret".into(),
        address: None,
        smtp: None,
    }
}

fn store(directory: &std::path::Path) -> Store {
    let store = Store::new(&Layout::new(directory), "email");
    store.save_account("default", &account()).unwrap();
    store
}

fn ledger(directory: &std::path::Path) -> Ledger {
    ledger_with(directory, 2048 * 1024, 256)
}

fn ledger_with(directory: &std::path::Path, max_bytes: usize, max_queue: usize) -> Ledger {
    let store = store(directory);
    let lock = store.lock("default").unwrap();
    let state = store.bind_state("default", |_| Ok(true)).unwrap();
    Ledger::open(store, lock, "default", state, max_bytes, max_queue).unwrap()
}

fn uid(uid: u32) -> SourceRef {
    SourceRef::Imap {
        mailbox: "INBOX".into(),
        uidvalidity: 7,
        uid,
    }
}

fn cursor(next: u32) -> Cursor {
    Cursor {
        provider: super::super::source::ProviderKind::Imap,
        value: format!("{next}"),
    }
}

fn report(source: SourceRef, text: &str) -> Decided {
    Decided {
        identity: Some(format!("id-{}", source.label())),
        message_id: None,
        outcome: Outcome::Report {
            key: format!("report:{}", source.label()),
            text: text.into(),
            urgent: false,
            handle: None,
            actions: Vec::new(),
        },
        source,
        turned: true,
        tokens: 900,
    }
}

fn saved(directory: &std::path::Path) -> MailState {
    Store::new(&Layout::new(directory), "email")
        .load_state("default")
        .unwrap()
}

#[tokio::test]
async fn a_new_state_gets_an_epoch_and_a_newer_one_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    let epoch = ledger.snapshot().epoch;
    assert_eq!(epoch.len(), 16);
    assert_eq!(saved(directory.path()).epoch, epoch);
    drop(ledger);
    let store = Store::new(&Layout::new(directory.path()), "email");
    let mut newer = saved(directory.path());
    newer.v = STATE_VERSION + 1;
    let lock = store.lock("default").unwrap();
    let error = Ledger::open(store, lock, "default", newer, 1 << 20, 256)
        .err()
        .unwrap();
    assert!(error.to_string().contains("newer SCV"), "{error}");
    // Fields a later release adds are refused too, not silently dropped.
    let path = directory.path().join("state/channels/email/default.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let with_later = text.replacen('{', "{\"later\":[],", 1);
    crate::state::atomic_write(&path, &with_later).unwrap();
    assert!(
        Store::new(&Layout::new(directory.path()), "email")
            .load_state("default")
            .is_err()
    );
}

#[tokio::test]
async fn claims_move_the_cursor_in_the_same_write_and_decisions_release_them() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    ledger
        .claim(vec![uid(5), uid(6), uid(5)], cursor(7), None, 100)
        .await
        .unwrap();
    let state = saved(directory.path());
    assert_eq!(state.cursor, Some(cursor(7)));
    assert_eq!(
        state.claims.len(),
        2,
        "a repeated reference is claimed once"
    );
    assert_eq!(ledger.last_check(), Some(100));
    assert_eq!(ledger.attempt(&uid(5)).await.unwrap(), Some(1));
    assert_eq!(ledger.attempt(&uid(5)).await.unwrap(), Some(2));
    assert_eq!(ledger.attempt(&uid(9)).await.unwrap(), None);
    ledger
        .finish(report(uid(5), "│ Subject: hi"), "2026-09-28", 110)
        .await
        .unwrap();
    ledger
        .finish(
            Decided {
                source: uid(6),
                identity: Some("dup".into()),
                message_id: None,
                outcome: Outcome::Counted,
                turned: false,
                tokens: 0,
            },
            "2026-09-28",
            111,
        )
        .await
        .unwrap();
    let state = saved(directory.path());
    assert!(state.claims.is_empty());
    assert_eq!(state.queue.len(), 1);
    assert_eq!(state.queue[0].key, "report:imap:7:5");
    assert_eq!(state.counts.skipped, 1);
    assert_eq!(state.arrival("dup", None), Arrival::Again);
    assert_eq!(state.day.date, "2026-09-28");
    assert_eq!(
        (state.day.seen, state.day.triaged, state.day.reported),
        (2, 1, 1)
    );
    assert_eq!(state.spent("2026-09-28", 120), (1, 900));
    assert_eq!(state.spent("2026-09-29", 120).1, 0);
    // A new local day starts its counts afresh.
    ledger
        .finish(report(uid(8), "x"), "2026-09-29", 200)
        .await
        .unwrap();
    assert_eq!(saved(directory.path()).day.seen, 1);
}

#[tokio::test]
async fn the_same_key_is_queued_once_and_long_items_are_cut() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    ledger
        .note("system:reset:2026-09-28", Class::System, "reset".into(), 1)
        .await
        .unwrap();
    ledger
        .note("system:reset:2026-09-28", Class::System, "again".into(), 2)
        .await
        .unwrap();
    ledger
        .finish(report(uid(1), &"x".repeat(5000)), "d", 3)
        .await
        .unwrap();
    let state = saved(directory.path());
    assert_eq!(state.queue.len(), 2);
    assert!(state.queue[1].text.len() <= MAX_ITEM_BYTES);
}

#[tokio::test]
async fn a_full_queue_drops_its_oldest_reports_into_a_count_but_keeps_scv_lines() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger_with(directory.path(), 1 << 20, 16);
    ledger
        .note("system:budget:d", Class::System, "budget".into(), 1)
        .await
        .unwrap();
    for n in 0..20 {
        ledger
            .finish(report(uid(n), "r"), "d", 10 + u64::from(n))
            .await
            .unwrap();
    }
    let state = saved(directory.path());
    assert_eq!(state.queue.len(), 16);
    assert!(state.queue.iter().any(|item| item.class == Class::System));
    assert_eq!(state.counts.unlisted, 5);
    assert_eq!(state.counts.unlisted_since, Some(10));
    assert_eq!(state.queue[1].key, "report:imap:7:5");
}

#[tokio::test]
async fn a_report_that_cannot_fit_the_state_file_is_counted_instead() {
    let directory = tempfile::tempdir().unwrap();
    // Room for the empty state and a little more, not for a big report.
    let ledger = ledger_with(directory.path(), 900, 16);
    ledger
        .claim(vec![uid(1)], cursor(2), None, 1)
        .await
        .unwrap();
    ledger
        .finish(report(uid(1), &"y".repeat(1400)), "d", 2)
        .await
        .unwrap();
    let state = saved(directory.path());
    assert!(state.queue.is_empty());
    assert!(state.claims.is_empty(), "the claim is still released");
    // Trimming drops the report into a count rather than refusing it.
    assert_eq!(state.counts.skipped + state.counts.unlisted, 1);
    assert!(serde_json::to_string(&state).unwrap().len() <= 900);
}

#[tokio::test]
async fn a_batch_is_stored_once_and_its_counts_leave_the_next_digest() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    for n in 0..3 {
        ledger.finish(report(uid(n), "r"), "d", 10).await.unwrap();
    }
    ledger
        .finish(
            Decided {
                source: uid(9),
                identity: None,
                message_id: None,
                outcome: Outcome::Counted,
                turned: false,
                tokens: 0,
            },
            "d",
            11,
        )
        .await
        .unwrap();
    let snapshot = ledger.snapshot();
    let batch = ledger
        .begin_batch(
            vec![0, 1],
            SendClass::Digest,
            "text".into(),
            snapshot.counts,
            20,
        )
        .await
        .unwrap();
    assert!(
        batch
            .key
            .starts_with(&format!("mail:{}:batch:", snapshot.epoch))
    );
    // While it is handed over, its items are not planned again.
    let queued: Vec<u64> = ledger.snapshot().queued().iter().map(|q| q.seq).collect();
    assert_eq!(queued, [2]);
    // A later count stays for the next digest.
    ledger
        .finish(
            Decided {
                source: uid(10),
                identity: None,
                message_id: None,
                outcome: Outcome::Counted,
                turned: false,
                tokens: 0,
            },
            "d",
            21,
        )
        .await
        .unwrap();
    ledger.batch_retry(22).await.unwrap();
    let retried = saved(directory.path()).batch.unwrap();
    assert_eq!((retried.attempts, retried.next_attempt_at), (1, 22 + 60));
    assert_eq!(retried.key, batch.key, "a retry hands over the same key");
    ledger
        .batch_stored("feishu:mail", "owner", 30)
        .await
        .unwrap();
    let state = saved(directory.path());
    assert!(state.batch.is_none());
    assert_eq!(state.queue.len(), 1);
    assert_eq!(state.counts.skipped, 1);
    assert_eq!(
        state
            .sent_keys
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>(),
        ["report:imap:7:0", "report:imap:7:1"]
    );
    assert_eq!(
        state.log,
        [Logged {
            at: 30,
            class: SendClass::Digest
        }]
    );
    assert_eq!(state.watched[0].route, "feishu:mail");
    ledger
        .settle_watched(&[], std::slice::from_ref(&batch.key), &mut || None)
        .await
        .unwrap();
    let state = saved(directory.path());
    assert!(state.watched.is_empty());
    assert_eq!(state.counts.undelivered, 1);
}

#[tokio::test]
async fn a_batch_no_route_took_is_dissolved_and_counted() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    ledger.finish(report(uid(1), "r"), "d", 10).await.unwrap();
    ledger
        .note("system:x:d", Class::System, "line".into(), 11)
        .await
        .unwrap();
    ledger
        .begin_batch(
            vec![0, 1],
            SendClass::Digest,
            "t".into(),
            Counts::default(),
            12,
        )
        .await
        .unwrap();
    for _ in 0..12 {
        ledger.batch_retry(13).await.unwrap();
    }
    assert_eq!(
        saved(directory.path()).batch.unwrap().next_attempt_at,
        13 + 30 * 60,
        "backoff stops at 30 minutes"
    );
    ledger.batch_dissolve().await.unwrap();
    let state = saved(directory.path());
    assert!(state.batch.is_none() && state.queue.is_empty());
    assert_eq!(state.counts.undelivered, 1, "only reports are counted");
}

#[tokio::test]
async fn rings_are_pruned_by_age_and_count() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    for n in 0..1100u32 {
        ledger
            .finish(
                Decided {
                    source: uid(n),
                    identity: Some(format!("i{n}")),
                    message_id: None,
                    outcome: Outcome::Gone,
                    turned: n % 2 == 0,
                    tokens: 0,
                },
                "d",
                1000 + u64::from(n),
            )
            .await
            .unwrap();
    }
    let state = saved(directory.path());
    assert_eq!(state.identities.len(), 1024);
    assert_eq!(state.arrival("i0", None), Arrival::New);
    assert_eq!(state.arrival("i1099", None), Arrival::Again);
    assert_eq!(
        state.day.seen, 0,
        "a message gone before its decision is not seen"
    );
    ledger.prune(1000 + 8 * 86_400).await.unwrap();
    let state = saved(directory.path());
    assert!(state.identities.is_empty() && state.turns.is_empty());
}

#[tokio::test]
async fn a_failed_write_changes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = ledger(directory.path());
    // Another mailbox's credentials now sit in the file: the binding check
    // refuses the write, and the state in memory stays as it was.
    let other = Account::Imap {
        host: "imap.other.example".into(),
        port: 993,
        username: "me".into(),
        password: "secret".into(),
        address: None,
        smtp: None,
    };
    let path = directory.path().join("credentials/email/default.json");
    crate::state::atomic_write(&path, &serde_json::to_string(&other).unwrap()).unwrap();
    assert!(
        ledger
            .claim(vec![uid(1)], cursor(2), None, 1)
            .await
            .is_err()
    );
    let state = ledger.snapshot();
    assert!(state.claims.is_empty() && state.cursor.is_none());
}

#[tokio::test]
async fn a_slow_disk_holds_up_no_reader_and_no_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let mut ledger = ledger(directory.path());
    let (entered, entering) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (entered, released) = (Mutex::new(entered), Mutex::new(released));
    // The disk takes until the test says so, or ten seconds.
    ledger.before_write = Some(Arc::new(move || {
        let _ = entered.lock().unwrap().send(());
        let _ = released
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
    }));
    let mut change = Box::pin(ledger.note("system:x:d", Class::System, "x".into(), 1));
    let writing = tokio::task::spawn_blocking(move || entering.recv().is_ok());
    tokio::select! {
        _ = &mut change => panic!("the write should be stalled"),
        writing = writing => assert!(writing.unwrap()),
    }
    // The write is stalled on a blocking thread: the async thread runs on,
    // and a reader sees the state as last saved without waiting.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(ledger.snapshot().queue.is_empty());

    // Shutdown drops the change at once. The write still lands whole, and
    // until it has, no run can take the account over.
    drop(change);
    drop(ledger);
    let store = Store::new(&Layout::new(directory.path()), "email");
    assert!(store.lock("default").is_err());
    release.send(()).unwrap();
    let mut taken = None;
    for _ in 0..500 {
        if let Ok(lock) = store.lock("default") {
            taken = Some(lock);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        taken.is_some(),
        "the run lock is released once the write lands"
    );
    assert_eq!(saved(directory.path()).queue.len(), 1);
}

#[tokio::test]
async fn a_cancelled_write_keeps_its_writer_lock_and_publishes_before_the_next_change() {
    let directory = tempfile::tempdir().unwrap();
    let mut ledger = ledger(directory.path());
    let entered = Arc::new(tokio::sync::Notify::new());
    let entering = Arc::clone(&entered);
    let (release, released) = std::sync::mpsc::channel();
    let released = Mutex::new(released);
    let first = std::sync::atomic::AtomicBool::new(true);
    ledger.before_write = Some(Arc::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            entering.notify_one();
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }));
    let mut change = Box::pin(ledger.note("system:first", Class::System, "first".into(), 1));
    tokio::select! {
        result = &mut change => panic!("write did not wait: {result:?}"),
        () = entered.notified() => {}
    }
    drop(change);
    assert!(
        ledger.writer.try_lock().is_err(),
        "the blocking write still owns serialization"
    );
    assert!(ledger.snapshot().queue.is_empty());
    release.send(()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        ledger.note("system:second", Class::System, "second".into(), 2),
    )
    .await
    .unwrap()
    .unwrap();
    let snapshot = ledger.snapshot();
    assert_eq!(
        snapshot.queue.len(),
        2,
        "the cancelled caller's write must not be overwritten"
    );
    assert_eq!(saved(directory.path()), snapshot);
}
