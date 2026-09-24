//! Unit tests for `src/outbound.rs`.

use super::*;

#[tokio::test]
async fn outbound_byte_backpressure_is_cancellation_aware() {
    let (output, mut receiver) = outbound_channel(5);
    output.send(vec![0; 4], None).await.unwrap();

    let cancellation = CancellationToken::new();
    let blocked = tokio::spawn({
        let output = output.clone();
        let cancellation = cancellation.clone();
        async move { output.send(vec![1; 4], Some(&cancellation)).await }
    });
    tokio::task::yield_now().await;
    assert!(!blocked.is_finished());

    cancellation.cancel();
    assert_eq!(blocked.await.unwrap(), Err(OutboundSendError::Cancelled));

    drop(receiver.recv().await.unwrap());
    output.send(vec![2; 4], None).await.unwrap();
}

#[tokio::test]
async fn outbound_control_send_times_out_under_byte_backpressure() {
    let (output, _receiver) = outbound_channel(5);
    output.send(vec![0; 4], None).await.unwrap();
    let result = output
        .send_with_timeout(vec![1; 4], None, Duration::from_millis(10))
        .await;
    assert_eq!(result, Err(OutboundSendError::TimedOut));
}
