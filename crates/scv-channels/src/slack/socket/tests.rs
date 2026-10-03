use super::*;
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, connect_async};
async fn pair() -> (Link, WebSocketStream<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (client, server) = tokio::join!(connect_async(format!("ws://{address}")), async {
        let (stream, _) = listener.accept().await.unwrap();
        accept_async(stream).await.unwrap()
    });
    (Link::for_test(client.unwrap().0), server)
}
#[tokio::test]
async fn ack_is_exact_and_ping_is_answered() {
    let (mut link, mut server) = pair().await;
    server.send(Message::Ping(vec![1, 2].into())).await.unwrap();
    server
        .send(Message::Text(
            serde_json::json!({"type":"events_api","envelope_id":"e1","payload":{}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let event = link
        .next(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        server.next().await.unwrap().unwrap(),
        Message::Pong(_)
    ));
    link.ack(&event.envelope_id).await.unwrap();
    let text = server.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        serde_json::json!({"envelope_id":"e1"})
    );
}
#[tokio::test]
async fn close_and_silent_socket_fail_and_no_data_is_a_healthy_window() {
    let (mut link, mut server) = pair().await;
    assert!(
        link.next(Instant::now() + Duration::from_millis(5))
            .await
            .unwrap()
            .is_none()
    );
    link.last_heard = Instant::now() - SILENCE;
    assert!(
        link.next(Instant::now() + Duration::from_secs(1))
            .await
            .is_err()
    );
    let (mut link, _) = pair().await;
    assert!(
        link.next(Instant::now() + Duration::from_secs(1))
            .await
            .is_err()
    );
    let _ = server.close(None).await;
}
#[tokio::test]
async fn hello_must_match_the_bot_app() {
    for app in ["A123", "AOTHER"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("ws://{address}");
        let (result, ()) = tokio::join!(Link::connect_url(&url, "A123"), async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut peer = accept_async(stream).await.unwrap();
            peer.send(Message::Text(
                serde_json::json!({"type":"hello","connection_info":{"app_id":app}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        });
        assert_eq!(result.is_ok(), app == "A123");
    }
}
#[test]
fn disabled_and_refresh_diagnostics_are_specific_and_redacted() {
    assert!(
        disconnect_note(&serde_json::json!({"reason":"link_disabled"}))
            .contains("Enable Socket Mode")
    );
    assert!(
        disconnect_note(&serde_json::json!({"reason":"refresh_requested"})).contains("fresh URL")
    );
    assert!(!disconnect_note(&serde_json::json!({"reason":"xapp-secret"})).contains("secret"));
}
