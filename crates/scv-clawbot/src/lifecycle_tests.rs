use super::*;
use serde_json::json;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};

async fn request(listener: &TcpListener) -> (TcpStream, String, Value) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let route = line.split_whitespace().nth(1).unwrap().to_owned();
    let mut length = 0;
    let mut authenticated = false;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("authorization")
        {
            authenticated = value.trim() == "Bearer token";
        }
    }
    if route.starts_with("/ilink/") {
        assert!(authenticated);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    (
        reader.into_inner(),
        route,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    )
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str, headers: &str) {
    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()).as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

fn inbound() -> Value {
    json!({"message_id":"incoming", "message_type":1, "from_user_id":"sender", "context_token":"context", "item_list":[{"text_item":{"text":"hello"}}]})
}

fn saved_store(directory: &Path, base_url: &str) -> state::Store {
    let store = state::Store::new(directory.join("clawbot"));
    store
        .save_account(
            "default",
            &state::Account {
                token: "token".into(),
                base_url: base_url.into(),
                bot_id: None,
                user_id: None,
            },
        )
        .unwrap();
    store
}

#[test]
fn interrupted_recovery_is_durable_and_preserves_retry_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = state::Store::new(directory.path().join("clawbot"));
    let original = state::BridgeState {
        in_flight: Some(state::InFlight {
            message_id: "incoming".into(),
            to_user_id: "sender".into(),
            context_token: "context".into(),
        }),
        ..Default::default()
    };
    store.save_state("default", &original).unwrap();
    let mut recovered = store.load_state("default").unwrap();
    recover_interrupted(&store, "default", &mut recovered).unwrap();
    let mut restarted = store.load_state("default").unwrap();
    assert!(restarted.in_flight.is_none());
    assert!(restarted.seen.is_empty());
    let pending = restarted.pending.clone().unwrap();
    assert_eq!(pending.message_id, "incoming");
    assert_eq!(pending.to_user_id, "sender");
    assert_eq!(pending.context_token, "context");
    assert_eq!(pending.reply, FAILURE_REPLY);
    assert_eq!(pending.client_ids.len(), 1);
    recover_interrupted(&store, "default", &mut restarted).unwrap();
    assert_eq!(restarted.pending.unwrap().client_ids, pending.client_ids);
}

#[test]
fn dedup_eviction_keeps_newest_entries() {
    let mut state = state::BridgeState {
        seen: (0..4096).map(|i| i.to_string()).collect(),
        ..Default::default()
    };
    mark_seen(&mut state, "newest");
    assert_eq!(state.seen.len(), 4096);
    assert_eq!(state.seen.first().unwrap(), "1");
    assert_eq!(state.seen.last().unwrap(), "newest");
    mark_seen(&mut state, "newest");
    assert_eq!(state.seen.len(), 4096);
}

#[tokio::test]
async fn recovered_delivery_deduplicates_first_poll_without_reexecuting() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let store = saved_store(directory.path(), &base);
    store
        .save_state(
            "default",
            &state::BridgeState {
                seen: (0..4096).map(|i| i.to_string()).collect(),
                in_flight: Some(state::InFlight {
                    message_id: "incoming".into(),
                    to_user_id: "sender".into(),
                    context_token: "context".into(),
                }),
                ..Default::default()
            },
        )
        .unwrap();
    let socket = directory.path().join("missing.sock");
    let cancel = CancellationToken::new();
    let reports = Mutex::new(Vec::new());
    let report = |healthy| reports.lock().unwrap().push(healthy);
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            &store,
            &report,
        ),
    );
    let peer = async {
        let (mut stream, route, body) = request(&listener).await;
        assert!(route.ends_with("sendmessage"));
        assert!(reports.lock().unwrap().is_empty());
        let pending = store.load_state("default").unwrap().pending.unwrap();
        assert_eq!(body["msg"]["client_id"], pending.client_ids[0]);
        assert_eq!(
            body["msg"]["item_list"][0]["text_item"]["text"],
            FAILURE_REPLY
        );
        respond(&mut stream, "200 OK", r#"{"ret":0}"#, "").await;
        let (mut stream, route, _) = request(&listener).await;
        assert!(route.ends_with("getupdates"));
        respond(
            &mut stream,
            "200 OK",
            &json!({"msgs":[inbound()],"sync_buf":"sync","get_updates_buf":"next"}).to_string(),
            "",
        )
        .await;
        let (_stream, route, body) = request(&listener).await;
        assert!(route.ends_with("getupdates"));
        assert_eq!(body["get_updates_buf"], "next");
        let saved = store.load_state("default").unwrap();
        assert!(saved.pending.is_none());
        assert_eq!(saved.seen.last().unwrap(), "incoming");
        assert_eq!(saved.seen.len(), 4096);
        assert_eq!(*reports.lock().unwrap(), vec![true]);
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    assert!(store.lock("default").is_ok());
}

#[tokio::test]
async fn cancellation_drops_long_poll_and_pending_send() {
    for sending in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let store = saved_store(directory.path(), &base);
        let pending = new_pending("incoming", "sender", "context", "reply", MAX_REPLY_BYTES);
        if sending {
            store
                .save_state(
                    "default",
                    &state::BridgeState {
                        pending: Some(pending.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let socket = directory.path().join("missing.sock");
        let cancel = CancellationToken::new();
        let report = |_| panic!("no completed authenticated poll");
        let work = until_cancelled(
            cancel.clone(),
            run_loop(
                "token",
                &base,
                "default",
                directory.path(),
                &socket,
                &store,
                &report,
            ),
        );
        let peer = async {
            let (mut stream, route, _) = request(&listener).await;
            assert!(route.ends_with(if sending { "sendmessage" } else { "getupdates" }));
            cancel.cancel();
            assert_eq!(stream.read(&mut [0; 1]).await.unwrap(), 0);
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(work, peer) })
                .await
                .unwrap();
        result.unwrap();
        assert!(store.lock("default").is_ok());
        if sending {
            let saved = store.load_state("default").unwrap();
            assert!(saved.seen.is_empty());
            let restored = saved.pending.unwrap();
            assert_eq!(restored.client_ids, pending.client_ids);
            assert_eq!(restored.next_chunk, 0);
        }
    }
}

#[tokio::test]
async fn cancellation_drops_handshake_and_active_turn_with_durable_marker() {
    for active_turn in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let store = saved_store(directory.path(), &base);
        let socket = directory.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let cancel = CancellationToken::new();
        let work = until_cancelled(
            cancel.clone(),
            run_loop(
                "token",
                &base,
                "default",
                directory.path(),
                &socket,
                &store,
                &|_| {},
            ),
        );
        let peer = async {
            let (mut stream, _, _) = request(&listener).await;
            respond(
                &mut stream,
                "200 OK",
                &json!({"ret":0,"msgs":[inbound()]}).to_string(),
                "",
            )
            .await;
            let (stream, _) = daemon.accept().await.unwrap();
            let saved = store.load_state("default").unwrap();
            assert_eq!(saved.in_flight.unwrap().message_id, "incoming");
            assert!(saved.pending.is_none());
            assert!(saved.seen.is_empty());
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if active_turn {
                let initialized = json!({"type":"initialized","request_id":"clawbot-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"test","version":"0"}});
                reader
                    .get_mut()
                    .write_all(format!("{initialized}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["no_tools"],
                    true
                );
                let started = json!({"type":"session.started","request_id":"clawbot-session","session_id":"s","cwd":"/","model":"test","context_max_tokens":1024,"max_server_frame_bytes":1024,"max_transcript_bytes":1024,"max_transcript_items":10,"max_prompt_history_bytes":1024,"max_prompt_history_items":10});
                reader
                    .get_mut()
                    .write_all(format!("{started}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["type"],
                    "turn.start"
                );
            }
            cancel.cancel();
            assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(work, peer) })
                .await
                .unwrap();
        result.unwrap();
        let mut saved = store.load_state("default").unwrap();
        recover_interrupted(&store, "default", &mut saved).unwrap();
        assert_eq!(saved.pending.unwrap().reply, FAILURE_REPLY);
        assert!(store.lock("default").is_ok());
    }
}

#[tokio::test]
async fn failed_poll_reports_unhealthy_and_cancels_backoff() {
    for (status, body) in [
        ("503 Unavailable", r#"{"ret":0}"#),
        ("200 OK", "invalid JSON"),
        ("200 OK", r#"{"ret":-14}"#),
        ("200 OK", r#"{"ret":0,"msgs":{}}"#),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let store = saved_store(directory.path(), &base);
        let socket = directory.path().join("missing.sock");
        let cancel = CancellationToken::new();
        let reports = Mutex::new(Vec::new());
        let report = |healthy| {
            reports.lock().unwrap().push(healthy);
            cancel.cancel();
        };
        let work = until_cancelled(
            cancel.clone(),
            run_loop(
                "token",
                &base,
                "default",
                directory.path(),
                &socket,
                &store,
                &report,
            ),
        );
        let peer = async {
            let (mut stream, _, _) = request(&listener).await;
            respond(&mut stream, status, body, "").await;
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(work, peer) })
                .await
                .unwrap();
        result.unwrap();
        assert_eq!(*reports.lock().unwrap(), vec![false]);
    }
}

#[tokio::test]
async fn failed_send_retries_same_client_id_and_only_poll_restores_health() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let store = saved_store(directory.path(), &base);
    let pending = new_pending("incoming", "sender", "context", "reply", MAX_REPLY_BYTES);
    store
        .save_state(
            "default",
            &state::BridgeState {
                pending: Some(pending.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let socket = directory.path().join("missing.sock");
    let cancel = CancellationToken::new();
    let reports = Mutex::new(Vec::new());
    let report = |healthy| {
        reports.lock().unwrap().push(healthy);
        if healthy {
            cancel.cancel();
        }
    };
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            &store,
            &report,
        ),
    );
    let peer = async {
        for status in ["503 Unavailable", "200 OK"] {
            let (mut stream, route, body) = request(&listener).await;
            assert!(route.ends_with("sendmessage"));
            assert_eq!(body["msg"]["client_id"], pending.client_ids[0]);
            let saved = store.load_state("default").unwrap();
            assert!(saved.seen.is_empty());
            assert_eq!(saved.pending.unwrap().client_ids, pending.client_ids);
            respond(&mut stream, status, r#"{"ret":0}"#, "").await;
        }
        let (mut stream, route, _) = request(&listener).await;
        assert!(route.ends_with("getupdates"));
        assert_eq!(*reports.lock().unwrap(), vec![false]);
        respond(&mut stream, "200 OK", r#"{"ret":0,"msgs":[]}"#, "").await;
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    assert_eq!(*reports.lock().unwrap(), vec![false, true]);
    assert!(store.load_state("default").unwrap().pending.is_none());
}

#[tokio::test]
async fn oversized_batch_never_executes_or_advances_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let store = saved_store(directory.path(), &base);
    let mut state = store.load_state("default").unwrap();
    state.cursor = "before".into();
    store.save_state("default", &state).unwrap();
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let report = |healthy: bool| {
        assert!(!healthy);
        cancel.cancel();
    };
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            &store,
            &report,
        ),
    );
    let peer = async {
        let (mut stream, route, _) = request(&listener).await;
        assert!(route.ends_with("getupdates"));
        respond(
            &mut stream,
            "200 OK",
            &json!({"ret":0,"msgs":vec![inbound(); 4097],"get_updates_buf":"after"}).to_string(),
            "",
        )
        .await;
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    assert_eq!(
        serde_json::to_value(store.load_state("default").unwrap()).unwrap(),
        serde_json::to_value(state).unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), daemon.accept())
            .await
            .is_err()
    );
    assert!(validate_updates(&json!({"ret":0,"msgs":vec![inbound(); 4096]})).is_ok());
}

#[test]
fn already_seen_batch_ids_survive_until_cursor_commit() {
    let mut state = state::BridgeState {
        seen: (0..4096).map(|i| i.to_string()).collect(),
        ..Default::default()
    };
    mark_seen(&mut state, "0");
    for index in 0..4095 {
        mark_seen(&mut state, &format!("batch-{index}"));
    }
    assert_eq!(state.seen.len(), 4096);
    assert_eq!(state.seen.first().unwrap(), "0");
}

#[tokio::test]
async fn mismatched_credentials_never_contact_poll_or_send() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let store = saved_store(directory.path(), &base);
    let mut state = store.load_state("default").unwrap();
    state.pending = Some(new_pending(
        "incoming",
        "sender",
        "context",
        "private reply",
        MAX_REPLY_BYTES,
    ));
    store.save_state("default", &state).unwrap();
    let result = run_loop(
        "replacement",
        &base,
        "default",
        directory.path(),
        &directory.path().join("missing.sock"),
        &store,
        &|_| panic!("no contact"),
    )
    .await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "ClawBot state does not match saved credentials"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(store.load_state("default").unwrap()).unwrap(),
        serde_json::to_value(state).unwrap()
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = http_client().unwrap();
    let fetch = async {
        let response = client
            .get(format!("http://{}", origin.local_addr().unwrap()))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(response_json(response).await.is_err());
    };
    let peer = async {
        let (mut stream, _, _) = request(&origin).await;
        respond(
            &mut stream,
            "302 Found",
            "",
            &format!(
                "Location: http://{}/redirected\r\n",
                target.local_addr().unwrap()
            ),
        )
        .await;
    };
    tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(fetch, peer) })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), target.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pre_cancelled_public_runner_does_not_touch_state() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    run_supervised(
        "token",
        "invalid",
        "../invalid",
        Path::new("/"),
        Path::new("/missing"),
        cancellation,
        Arc::new(|_| panic!("cancelled before startup")),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn login_validates_account_before_network_or_url() {
    assert_eq!(
        login("invalid", "../invalid")
            .await
            .unwrap_err()
            .to_string(),
        "invalid ClawBot account name"
    );
}
