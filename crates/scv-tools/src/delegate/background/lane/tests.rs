//! Unit tests for `src/delegate/background/lane.rs`.

use std::time::Duration;

use super::*;

/// Whether `place` becomes first in its lane within a short wait.
async fn comes_first(place: &Place) -> bool {
    tokio::time::timeout(
        Duration::from_millis(50),
        place.wait_first(&CancellationToken::new()),
    )
    .await
    .is_ok()
}

#[tokio::test]
async fn a_takeover_is_reserved_behind_the_running_turn_and_ahead_of_queued_calls() {
    let lanes = Arc::new(Lanes::default());
    let running = lanes.join("codex-1");
    running.run_as("job-1");
    let queued = lanes.join("codex-1");
    queued.run_as("job-2");
    let (takeover, ahead) = lanes.reserve_takeover("codex-1").unwrap();
    assert_eq!(ahead.as_deref(), Some("job-1"));
    assert_eq!(takeover.ahead, 1);
    // A call arriving now waits behind the takeover too.
    let late = lanes.join("codex-1");
    assert_eq!(late.ahead, 3);
    assert!(lanes.has_takeover("codex-1"));
    assert!(!lanes.held_by_takeover("codex-1"));
    assert_eq!(lanes.front("codex-1"), Some(Front::Job("job-1".into())));
    // Reserved once: another reservation, or a second takeover, waits.
    assert_eq!(
        lanes.reserve_takeover("codex-1").unwrap_err(),
        Unreserved::TakenOver
    );
    // The running turn ends: the takeover, not the queued call, is next.
    drop(running);
    assert!(lanes.held_by_takeover("codex-1"));
    assert!(comes_first(&takeover).await);
    assert!(!comes_first(&queued).await);
    drop(takeover);
    assert!(comes_first(&queued).await);
    assert!(!lanes.has_takeover("codex-1"));
    drop((queued, late));
    assert_eq!(lanes.front("codex-1"), None);
}

#[tokio::test]
async fn nothing_is_reserved_behind_a_call_that_is_no_job() {
    let lanes = Arc::new(Lanes::default());
    let call = lanes.join("codex-1");
    assert_eq!(
        lanes.reserve_takeover("codex-1").unwrap_err(),
        Unreserved::NotAJob
    );
    // The refusal left the lane as it was.
    assert!(!lanes.has_takeover("codex-1"));
    assert_eq!(lanes.front("codex-1"), Some(Front::Call));
    assert_eq!(lanes.join("codex-1").ahead, 1);
    drop(call);
    // An idle conversation's lane is the takeover's at once.
    let (takeover, ahead) = lanes.reserve_takeover("codex-1").unwrap();
    assert_eq!((takeover.ahead, ahead), (0, None));
    assert!(lanes.held_by_takeover("codex-1"));
    // One holding a lane by `join_takeover` keeps reservations out too.
    let held = lanes.join_takeover("codex-2");
    assert_eq!(
        lanes.reserve_takeover("codex-2").unwrap_err(),
        Unreserved::TakenOver
    );
    drop(held);
}
