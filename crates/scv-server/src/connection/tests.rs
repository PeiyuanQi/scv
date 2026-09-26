//! Unit tests for `src/connection.rs`.

use std::{
    future::pending,
    io::Cursor,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use tokio::sync::oneshot;

use super::*;
use crate::test_support::{DropSignal, test_registry};

async fn read_bounded_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<FrameRead> {
    FrameBuffer::default().read(reader, max_bytes).await
}

#[tokio::test]
async fn nonreading_management_client_does_not_hold_component_lock() {
    let (mut input, server_input) = tokio::io::duplex(65536);
    let (server_output, _blocked_output) = tokio::io::duplex(1);
    let tasks = TaskTracker::new();
    let components = Arc::new(Mutex::new(components::Components::new(
        crate::test_support::test_instance("/unused"),
        PathBuf::from("/"),
    )));
    let cancel = CancellationToken::new();
    let handler = tokio::spawn(run_managed(
        server_input,
        server_output,
        crate::test_support::test_instance("/unused"),
        Some(components.clone()),
        test_registry(),
        cancel.clone(),
        tasks.clone(),
    ));
    // A supported version, so the handler answers every request below.
    let initialize = format!(
        "{{\"type\":\"initialize\",\"request_id\":\"init\",\"protocol_version\":{PROTOCOL_VERSION},\"client\":{{\"name\":\"test\",\"version\":\"0\"}}}}\n"
    );
    input.write_all(initialize.as_bytes()).await.unwrap();
    // More answers than the outbound queue holds, so the handler ends up
    // blocked sending one while nobody reads its output.
    for _ in 0..300 {
        input.write_all(b"{\"type\":\"daemon.control\",\"request_id\":\"s\",\"command\":{\"action\":\"status\"}}\n").await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    // A blocked send gives up after SHUTDOWN_GRACE (3 s); a handler that
    // kept the lock while blocked would hold it that long, so wait well under.
    let status = tokio::time::timeout(Duration::from_secs(1), async {
        components.lock().await.status()
    })
    .await
    .unwrap();
    assert_eq!(status.pid, std::process::id());
    cancel.cancel();
    handler.abort();
    let _ = handler.await;
    tasks.close();
    tokio::time::timeout(Duration::from_secs(1), tasks.wait())
        .await
        .unwrap();
}

#[tokio::test]
async fn forced_connection_abort_drops_and_joins_writer_descendants() {
    let (mut input, server_input) = tokio::io::duplex(512);
    let (server_output, _blocked_output) = tokio::io::duplex(1);
    let tasks = TaskTracker::new();
    let handler = tokio::spawn(run_managed(
        server_input,
        server_output,
        crate::test_support::test_instance("/unused"),
        None,
        test_registry(),
        CancellationToken::new(),
        tasks.clone(),
    ));
    input.write_all(b"{\"type\":\"initialize\",\"request_id\":\"init\",\"protocol_version\":2,\"client\":{\"name\":\"test\",\"version\":\"0\"}}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while tasks.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handler.abort();
    let _ = handler.await;
    tasks.close();
    tokio::time::timeout(Duration::from_secs(1), tasks.wait())
        .await
        .unwrap();
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn frame_buffer_preserves_partial_and_discard_state_across_cancellation() {
    let (mut input, output) = tokio::io::duplex(64);
    let mut reader = BufReader::new(output);
    let mut frames = FrameBuffer::default();
    input.write_all(b"12").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), frames.read(&mut reader, 4))
            .await
            .is_err()
    );
    input.write_all(b"34\n").await.unwrap();
    assert!(
        matches!(frames.read(&mut reader, 4).await.unwrap(), FrameRead::Frame(value) if value == b"1234")
    );
    input.write_all(b"123456789").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), frames.read(&mut reader, 4))
            .await
            .is_err()
    );
    input.write_all(b"\n{}\n").await.unwrap();
    assert!(matches!(
        frames.read(&mut reader, 4).await.unwrap(),
        FrameRead::TooLarge
    ));
    assert!(
        matches!(frames.read(&mut reader, 4).await.unwrap(), FrameRead::Frame(value) if value == b"{}")
    );
}

#[tokio::test]
async fn bounded_reader_discards_an_oversized_line() {
    let input = format!("{}\n{{}}\n", "x".repeat(10));
    let mut reader = BufReader::new(Cursor::new(input.into_bytes()));
    assert!(matches!(
        read_bounded_frame(&mut reader, 4).await.unwrap(),
        FrameRead::TooLarge
    ));
    match read_bounded_frame(&mut reader, 4).await.unwrap() {
        FrameRead::Frame(frame) => assert_eq!(frame, b"{}"),
        _ => panic!("expected the frame following the oversized line"),
    }
}

#[tokio::test]
async fn bounded_reader_accepts_exact_crlf_limit() {
    let mut reader = BufReader::new(Cursor::new(b"1234\r\n".to_vec()));
    match read_bounded_frame(&mut reader, 4).await.unwrap() {
        FrameRead::Frame(frame) => assert_eq!(frame, b"1234"),
        _ => panic!("expected an exact-limit frame"),
    }
}

#[tokio::test]
async fn writer_shutdown_aborts_after_grace_period() {
    let dropped = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = oneshot::channel();
    let writer = tokio::spawn({
        let dropped = Arc::clone(&dropped);
        async move {
            let _signal = DropSignal(dropped);
            let _ = started_tx.send(());
            pending::<std::io::Result<()>>().await
        }
    });
    started_rx.await.unwrap();

    let result = shutdown_writer(writer, Duration::from_millis(10)).await;

    assert!(result.is_err());
    assert!(dropped.load(Ordering::Acquire));
}
