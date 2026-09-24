//! Unit tests for `src/retry.rs`.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

const LABELS: SendLabels<'static> = SendLabels {
    refused: "test refused",
    failed: "test failed",
    gave_up: "test gave up",
};

#[test]
fn backoff_doubles_up_to_a_minute_and_resets() {
    let mut backoff = Backoff::new();
    let delays: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
    backoff.reset();
    assert_eq!(backoff.next_delay(), Backoff::FIRST);
}

#[tokio::test(start_paused = true)]
async fn a_send_that_fails_twice_then_lands_waits_one_then_two_seconds() {
    let tries = AtomicU32::new(0);
    let reports = Mutex::new(Vec::new());
    let start = tokio::time::Instant::now();
    let sent = retry_send(
        LABELS,
        &|healthy| reports.lock().unwrap().push(healthy),
        || async {
            if tries.fetch_add(1, Ordering::SeqCst) < 2 {
                Attempt::Retry("down".into())
            } else {
                Attempt::Done("id")
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(sent, Some("id"));
    assert_eq!(tries.load(Ordering::SeqCst), 3);
    assert_eq!(*reports.lock().unwrap(), [false, false]);
    assert_eq!(start.elapsed(), Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn a_refusal_stops_at_once_without_reporting_unhealthy() {
    let tries = AtomicU32::new(0);
    let sent = retry_send(
        LABELS,
        &|_| panic!("a refusal is not a failure"),
        || async {
            tries.fetch_add(1, Ordering::SeqCst);
            Attempt::<()>::Refused("bad request".into())
        },
    )
    .await
    .unwrap();
    assert_eq!(sent, None);
    assert_eq!(tries.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn three_transient_failures_give_up_with_the_callers_error() {
    let tries = AtomicU32::new(0);
    let reports = Mutex::new(Vec::new());
    let start = tokio::time::Instant::now();
    let error = retry_send(
        LABELS,
        &|healthy| reports.lock().unwrap().push(healthy),
        || async {
            tries.fetch_add(1, Ordering::SeqCst);
            Attempt::<()>::Retry("down".into())
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "test gave up");
    assert_eq!(tries.load(Ordering::SeqCst), SEND_ATTEMPTS);
    assert_eq!(*reports.lock().unwrap(), [false, false, false]);
    // No wait after the last try: the caller's own backoff takes over.
    assert_eq!(start.elapsed(), Duration::from_secs(3));
}
