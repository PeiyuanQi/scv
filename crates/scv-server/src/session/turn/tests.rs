//! Unit tests for `src/session/turn.rs`.

use std::{
    future::pending,
    sync::atomic::{AtomicBool, Ordering},
};

use tokio::sync::oneshot;

use super::*;
use crate::test_support::DropSignal;

#[tokio::test]
async fn forced_handler_abort_cancels_and_joins_active_turn() {
    let tasks = TaskTracker::new();
    let cancellation = CancellationToken::new();
    let child_cancel = cancellation.child_token();
    let observed_cancel = child_cancel.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tasks.spawn({
        let dropped = dropped.clone();
        async move {
            let _guard = DropSignal(dropped);
            let _ = ready_tx.send(());
            pending::<()>().await;
        }
    });
    ready_rx.await.unwrap();
    let (owned_tx, owned_rx) = oneshot::channel();
    let handler = tokio::spawn(async move {
        let _active = ActiveTurn {
            turn_id: "test".into(),
            cancellation: child_cancel,
            task,
        };
        let _ = owned_tx.send(());
        pending::<()>().await;
    });
    owned_rx.await.unwrap();
    handler.abort();
    let _ = handler.await;
    tasks.close();
    tokio::time::timeout(Duration::from_secs(1), tasks.wait())
        .await
        .unwrap();
    assert!(observed_cancel.is_cancelled());
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn active_turn_shutdown_aborts_after_grace_period() {
    let cancellation = CancellationToken::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = oneshot::channel();
    let task = tokio::spawn({
        let dropped = Arc::clone(&dropped);
        async move {
            let _signal = DropSignal(dropped);
            let _ = started_tx.send(());
            pending::<()>().await;
        }
    });
    started_rx.await.unwrap();

    let graceful = shutdown_active_turn(
        ActiveTurn {
            turn_id: "turn".into(),
            cancellation,
            task,
        },
        Duration::from_millis(10),
    )
    .await;

    assert!(!graceful);
    assert!(dropped.load(Ordering::Acquire));
}
