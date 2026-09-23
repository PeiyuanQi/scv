use super::*;
use serde_json::json;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};

async fn request(listener: &TcpListener) -> (TcpStream, String, Value) {
    let (stream, _) = listener.accept().await.unwrap();
    read_request(stream).await
}

async fn read_request(stream: TcpStream) -> (TcpStream, String, Value) {
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
        in_flight: vec![state::InFlight {
            message_id: "incoming".into(),
            to_user_id: "sender".into(),
            context_token: "context".into(),
            key: String::new(),
        }],
        ..Default::default()
    };
    store.save_state("default", &original).unwrap();
    let mut recovered = store.load_state("default").unwrap();
    recover_interrupted(&store, "default", &mut recovered).unwrap();
    let mut restarted = store.load_state("default").unwrap();
    assert!(restarted.in_flight.is_empty());
    assert!(restarted.seen.is_empty());
    let pending = restarted.pending[0].clone();
    assert_eq!(pending.message_id, "incoming");
    assert_eq!(pending.to_user_id, "sender");
    assert_eq!(pending.context_token, "context");
    assert_eq!(pending.reply, FAILURE_REPLY);
    assert_eq!(pending.client_ids.len(), 1);
    recover_interrupted(&store, "default", &mut restarted).unwrap();
    assert_eq!(restarted.pending.len(), 1);
    assert_eq!(restarted.pending[0].client_ids, pending.client_ids);
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
                in_flight: vec![state::InFlight {
                    message_id: "incoming".into(),
                    to_user_id: "sender".into(),
                    context_token: "context".into(),
                    key: String::new(),
                }],
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
            None,
            &store,
            &report,
        ),
    );
    let peer = async {
        let (mut stream, route, body) = request(&listener).await;
        assert!(route.ends_with("sendmessage"));
        assert!(reports.lock().unwrap().is_empty());
        let pending = store.load_state("default").unwrap().pending[0].clone();
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
        assert!(saved.pending.is_empty());
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
                        pending: vec![pending.clone()],
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
                None,
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
            let restored = &saved.pending[0];
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
                None,
                &store,
                &|_| {},
            ),
        );
        let peer = async {
            let (mut stream, _, _) = request(&listener).await;
            let numeric_message_id = 7_445_729_589_862_608_648_u64;
            let mut message = inbound();
            message["message_id"] = json!(numeric_message_id);
            respond(
                &mut stream,
                "200 OK",
                &json!({"ret":0,"msgs":[message]}).to_string(),
                "",
            )
            .await;
            let (stream, _) = daemon.accept().await.unwrap();
            let saved = store.load_state("default").unwrap();
            assert_eq!(
                saved.in_flight[0].message_id,
                numeric_message_id.to_string()
            );
            assert_eq!(saved.in_flight[0].key, "sender");
            assert!(saved.pending.is_empty());
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
        assert_eq!(saved.pending[0].reply, FAILURE_REPLY);
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
                None,
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
                pending: vec![pending.clone()],
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
            None,
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
            assert_eq!(saved.pending[0].client_ids, pending.client_ids);
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
    assert!(store.load_state("default").unwrap().pending.is_empty());
}

#[tokio::test]
async fn live_send_ack_without_ret_completes_pending_delivery_once() {
    // Live acknowledgements omit `ret`; they may be `{}` or an empty body.
    for ack in ["{}", ""] {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let store = saved_store(directory.path(), &base);
        let pending = new_pending("incoming", "sender", "context", "reply", MAX_REPLY_BYTES);
        store
            .save_state(
                "default",
                &state::BridgeState {
                    pending: vec![pending.clone()],
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
                None,
                &store,
                &report,
            ),
        );
        let peer = async {
            let (mut stream, route, body) = request(&listener).await;
            assert!(route.ends_with("sendmessage"));
            assert_eq!(body["msg"]["client_id"], pending.client_ids[0]);
            respond(&mut stream, "200 OK", ack, "").await;
            // The next request is a poll, not a resend of the acknowledged reply.
            let (mut stream, route, _) = request(&listener).await;
            assert!(route.ends_with("getupdates"));
            let saved = store.load_state("default").unwrap();
            assert!(saved.pending.is_empty());
            assert_eq!(saved.seen, vec!["incoming".to_owned()]);
            respond(
                &mut stream,
                "200 OK",
                r#"{"msgs":[],"get_updates_buf":"next"}"#,
                "",
            )
            .await;
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(work, peer) })
                .await
                .unwrap();
        result.unwrap();
        assert_eq!(*reports.lock().unwrap(), vec![true]);
    }
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
            None,
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
    state.pending = vec![new_pending(
        "incoming",
        "sender",
        "context",
        "private reply",
        MAX_REPLY_BYTES,
    )];
    store.save_state("default", &state).unwrap();
    let result = run_loop(
        "replacement",
        &base,
        "default",
        directory.path(),
        &directory.path().join("missing.sock"),
        None,
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
async fn oversized_responses_are_rejected_before_parsing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = http_client().unwrap();
    let fetch = async {
        let response = client
            .get(format!("http://{}", listener.local_addr().unwrap()))
            .send()
            .await
            .unwrap();
        assert!(response_json(response).await.is_err());
    };
    let peer = async {
        let (mut stream, _, _) = request(&listener).await;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_RESPONSE_BYTES + 1
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(fetch, peer) })
        .await
        .unwrap();
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
        None,
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

#[tokio::test]
async fn rejected_send_is_final_and_polling_resumes() {
    for (status, rejection) in [
        ("200 OK", r#"{"ret":-2,"errmsg":"prepare failed"}"#),
        ("400 Bad Request", ""),
    ] {
        rejected_send_case(status, rejection).await;
    }
}

async fn rejected_send_case(status: &str, rejection: &str) {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let store = saved_store(directory.path(), &base);
    let pending = new_pending("incoming", "sender", "context", "reply", MAX_REPLY_BYTES);
    store
        .save_state(
            "default",
            &state::BridgeState {
                pending: vec![pending],
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
            None,
            &store,
            &report,
        ),
    );
    let peer = async {
        let (mut stream, route, _) = request(&listener).await;
        assert!(route.ends_with("sendmessage"));
        respond(&mut stream, status, rejection, "").await;
        // The refusal is final: the next request polls instead of resending.
        let (mut stream, route, _) = request(&listener).await;
        assert!(route.ends_with("getupdates"));
        let saved = store.load_state("default").unwrap();
        assert!(saved.pending.is_empty());
        assert_eq!(saved.seen, vec!["incoming".to_owned()]);
        // The refused reply waits for the sender's next message.
        assert_eq!(saved.held.len(), 1);
        assert_eq!(saved.held[0].key, "sender");
        assert_eq!(saved.held[0].reply, "reply");
        respond(
            &mut stream,
            "200 OK",
            r#"{"msgs":[],"get_updates_buf":"next"}"#,
            "",
        )
        .await;
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    assert_eq!(*reports.lock().unwrap(), vec![true]);
}

async fn next_frame(reader: &mut BufReader<tokio::net::UnixStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn send_frame(reader: &mut BufReader<tokio::net::UnixStream>, frame: Value) {
    reader
        .get_mut()
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
}

/// A fake iLink API that routes by path, so polling and sending may interleave
/// freely. Each poll returns the next queued batch, or an empty batch after a
/// short wait; each send is recorded and answered from a script (default 200).
struct FakeIlink {
    base: String,
    batches: tokio::sync::mpsc::UnboundedSender<Vec<Value>>,
    sends: tokio::sync::mpsc::UnboundedReceiver<Value>,
    responses: Arc<Mutex<std::collections::VecDeque<(&'static str, &'static str)>>>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeIlink {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (batches, queue) = tokio::sync::mpsc::unbounded_channel::<Vec<Value>>();
        let queue = Arc::new(tokio::sync::Mutex::new(queue));
        let (sent, sends) = tokio::sync::mpsc::unbounded_channel();
        let responses = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let script = Arc::clone(&responses);
        let server = tokio::spawn(async move {
            let cursor = Arc::new(std::sync::atomic::AtomicU64::new(0));
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (queue, sent, script, cursor) = (
                    Arc::clone(&queue),
                    sent.clone(),
                    Arc::clone(&script),
                    Arc::clone(&cursor),
                );
                tokio::spawn(async move {
                    let (mut stream, route, body) = read_request(stream).await;
                    if route.ends_with("getupdates") {
                        let batch = tokio::time::timeout(Duration::from_millis(50), async {
                            queue.lock().await.recv().await
                        })
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                        let next = cursor.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let body = json!({"msgs":batch,"get_updates_buf":format!("c{next}")});
                        respond(&mut stream, "200 OK", &body.to_string(), "").await;
                    } else {
                        let (status, reply) =
                            script.lock().unwrap().pop_front().unwrap_or(("200 OK", ""));
                        let _ = sent.send(body);
                        respond(&mut stream, status, reply, "").await;
                    }
                });
            }
        });
        Self {
            base,
            batches,
            sends,
            responses,
            server,
        }
    }

    fn push(&self, batch: Vec<Value>) {
        self.batches.send(batch).unwrap();
    }

    async fn sent(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.sends.recv())
            .await
            .expect("a reply is sent")
            .unwrap()
    }
}

impl Drop for FakeIlink {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn text_message(id: &str, sender: &str, text: &str) -> Value {
    json!({"message_id":id, "message_type":1, "from_user_id":sender, "context_token":format!("ctx-{id}"), "item_list":[{"text_item":{"text":text}}]})
}

fn sent_text(body: &Value) -> &str {
    body["msg"]["item_list"][0]["text_item"]["text"]
        .as_str()
        .unwrap()
}

/// Accept one ClawBot session on the fake daemon and complete its handshake,
/// returning the daemon side and the `session.start` frame.
async fn accept_session(daemon: &UnixListener) -> (BufReader<tokio::net::UnixStream>, Value) {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), daemon.accept())
        .await
        .expect("a session connects")
        .unwrap();
    let mut side = BufReader::new(stream);
    assert_eq!(next_frame(&mut side).await["type"], "initialize");
    send_frame(&mut side, json!({"type":"initialized","request_id":"clawbot-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"test","version":"0"}})).await;
    let start = next_frame(&mut side).await;
    send_frame(&mut side, json!({"type":"session.started","request_id":"clawbot-session","session_id":"s","cwd":"/","model":"test","context_max_tokens":1024,"max_server_frame_bytes":1024,"max_transcript_bytes":1024,"max_transcript_items":10,"max_prompt_history_bytes":1024,"max_prompt_history_items":10})).await;
    (side, start)
}

/// Read the next `turn.start` and return its prompt.
async fn next_turn(side: &mut BufReader<tokio::net::UnixStream>) -> String {
    let frame = next_frame(side).await;
    assert_eq!(frame["type"], "turn.start");
    frame["prompt"].as_str().unwrap().to_owned()
}

async fn finish_turn(side: &mut BufReader<tokio::net::UnixStream>, content: &str) {
    send_frame(side, json!({"type":"assistant.completed","request_id":"r","session_id":"s","turn_id":"t","seq":2,"content":content})).await;
    send_frame(side, json!({"type":"turn.completed","request_id":"r","session_id":"s","turn_id":"t","seq":3,"steps":1,"usage":{}})).await;
}

async fn quiet(side: &mut BufReader<tokio::net::UnixStream>) -> bool {
    tokio::time::timeout(Duration::from_millis(300), next_frame(side))
        .await
        .is_err()
}

fn owner_of(user_id: &str) -> ToolOwner {
    ToolOwner {
        user_id: user_id.into(),
        turn_timeout: owner_turn_timeout(Duration::from_secs(1800)),
    }
}

#[tokio::test]
async fn only_the_owner_gets_tools_and_auto_approval() {
    for (owner, group, tools) in [
        (Some("sender"), None, true),
        (Some("sender"), Some(json!("room")), false),
        (Some("sender"), Some(json!(42)), false),
        (Some("someone-else"), None, false),
        (None, None, false),
    ] {
        let mut message = text_message("incoming", "sender", "hello");
        if let Some(group) = group {
            message["group_id"] = group;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut ilink = FakeIlink::start().await;
        let store = saved_store(directory.path(), &ilink.base);
        let socket = directory.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let cancel = CancellationToken::new();
        let owner = owner.map(owner_of);
        let base = ilink.base.clone();
        let work = until_cancelled(
            cancel.clone(),
            run_loop(
                "token",
                &base,
                "default",
                directory.path(),
                &socket,
                owner.as_ref(),
                &store,
                &|_| {},
            ),
        );
        let peer = async {
            ilink.push(vec![message]);
            let (mut side, start) = accept_session(&daemon).await;
            assert_eq!(start["no_tools"], !tools);
            assert_eq!(next_turn(&mut side).await, "hello");
            send_frame(&mut side, json!({"type":"approval.requested","request_id":"r","session_id":"s","turn_id":"t","seq":1,"approval_id":"a1","call_id":"c1","name":"agent_claude","risk":"delegate","cwd":"/","summary":"Launch claude"})).await;
            let resolved = next_frame(&mut side).await;
            assert_eq!(resolved["approval_id"], "a1");
            assert_eq!(resolved["approved"], tools);
            finish_turn(&mut side, "done").await;
            let body = ilink.sent().await;
            assert_eq!(sent_text(&body), "done");
            assert_eq!(body["msg"]["context_token"], "ctx-incoming");
            // Delivery is saved before the next reply could be queued.
            wait_until(|| store.load_state("default").unwrap().pending.is_empty()).await;
            cancel.cancel();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
                .await
                .unwrap();
        result.unwrap();
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition holds in time");
}

#[tokio::test]
async fn a_long_owner_turn_neither_blocks_other_senders_nor_reorders_the_owners_messages() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = owner_of("owner");
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            Some(&owner),
            &store,
            &|_| {},
        ),
    );
    let peer = async {
        ilink.push(vec![text_message("o1", "owner", "first")]);
        let (mut owner_side, start) = accept_session(&daemon).await;
        assert_eq!(start["no_tools"], false);
        assert_eq!(next_turn(&mut owner_side).await, "first");
        // The owner's turn stays open while polling continues.
        ilink.push(vec![
            text_message("x1", "other", "hi"),
            text_message("o2", "owner", "second"),
        ]);
        let (mut other_side, start) = accept_session(&daemon).await;
        assert_eq!(start["no_tools"], true);
        assert_eq!(next_turn(&mut other_side).await, "hi");
        finish_turn(&mut other_side, "hello other").await;
        let body = ilink.sent().await;
        assert_eq!(body["msg"]["to_user_id"], "other");
        assert_eq!(body["msg"]["context_token"], "ctx-x1");
        assert_eq!(sent_text(&body), "hello other");
        // The owner's second message is claimed but waits its turn.
        wait_until(|| {
            let saved = store.load_state("default").unwrap();
            let claimed: Vec<_> = saved
                .in_flight
                .iter()
                .map(|c| c.message_id.clone())
                .collect();
            claimed == ["o1", "o2"]
        })
        .await;
        assert!(quiet(&mut owner_side).await);
        finish_turn(&mut owner_side, "done first").await;
        let body = ilink.sent().await;
        assert_eq!(body["msg"]["context_token"], "ctx-o1");
        assert_eq!(sent_text(&body), "done first");
        assert_eq!(next_turn(&mut owner_side).await, "second");
        finish_turn(&mut owner_side, "done second").await;
        let body = ilink.sent().await;
        assert_eq!(body["msg"]["context_token"], "ctx-o2");
        assert_eq!(sent_text(&body), "done second");
        wait_until(|| {
            let saved = store.load_state("default").unwrap();
            saved.in_flight.is_empty()
                && saved.pending.is_empty()
                && ["o1", "x1", "o2"]
                    .iter()
                    .all(|id| saved.seen.iter().any(|seen| seen == id))
        })
        .await;
        // Both conversations reused their sessions.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), daemon.accept())
                .await
                .is_err()
        );
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn at_most_four_senders_run_turns_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            None,
            &store,
            &|_| {},
        ),
    );
    let peer = async {
        ilink.push(
            (0..=MAX_CONCURRENT_TURNS)
                .map(|i| text_message(&format!("m{i}"), &format!("s{i}"), &format!("p{i}")))
                .collect(),
        );
        let mut running = Vec::new();
        for _ in 0..MAX_CONCURRENT_TURNS {
            let (mut side, _) = accept_session(&daemon).await;
            let prompt = next_turn(&mut side).await;
            running.push((side, prompt));
        }
        // Every message is claimed, but the fifth sender waits for a slot.
        wait_until(|| {
            store.load_state("default").unwrap().in_flight.len() == MAX_CONCURRENT_TURNS + 1
        })
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), daemon.accept())
                .await
                .is_err()
        );
        let (mut side, prompt) = running.remove(0);
        finish_turn(&mut side, &format!("re {prompt}")).await;
        assert_eq!(sent_text(&ilink.sent().await), format!("re {prompt}"));
        let (mut last, _) = accept_session(&daemon).await;
        let waited = next_turn(&mut last).await;
        let mut prompts: Vec<_> = running.iter().map(|(_, prompt)| prompt.clone()).collect();
        prompts.push(prompt);
        prompts.push(waited);
        prompts.sort();
        assert_eq!(prompts, ["p0", "p1", "p2", "p3", "p4"]);
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn a_full_conversation_queue_gets_a_busy_reply_without_a_turn() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            None,
            &store,
            &|_| {},
        ),
    );
    let peer = async {
        ilink.push(
            (0..=MAX_QUEUED_PER_CONVERSATION)
                .map(|i| text_message(&format!("m{i}"), "sender", &format!("p{i}")))
                .collect(),
        );
        let (mut side, _) = accept_session(&daemon).await;
        assert_eq!(next_turn(&mut side).await, "p0");
        let overflow = format!("m{MAX_QUEUED_PER_CONVERSATION}");
        let body = ilink.sent().await;
        assert_eq!(body["msg"]["context_token"], format!("ctx-{overflow}"));
        assert_eq!(sent_text(&body), BUSY_REPLY);
        wait_until(|| {
            let saved = store.load_state("default").unwrap();
            saved.in_flight.len() == MAX_QUEUED_PER_CONVERSATION
                && saved.seen == [overflow.clone()]
                && saved.pending.is_empty()
        })
        .await;
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn recovery_answers_every_claim_once_and_never_replays_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let claim = |id: &str, key: &str| state::InFlight {
        message_id: id.into(),
        to_user_id: "sender".into(),
        context_token: format!("ctx-{id}"),
        key: key.into(),
    };
    let answered = new_pending("done", "sender", "ctx-done", "answer", MAX_REPLY_BYTES);
    store
        .save_state(
            "default",
            &state::BridgeState {
                pending: vec![answered.clone()],
                in_flight: vec![claim("a", "room\0sender"), claim("b", "sender")],
                ..Default::default()
            },
        )
        .unwrap();
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            None,
            &store,
            &|_| {},
        ),
    );
    let peer = async {
        let first = ilink.sent().await;
        assert_eq!(first["msg"]["client_id"], answered.client_ids[0]);
        assert_eq!(sent_text(&first), "answer");
        for id in ["a", "b"] {
            let body = ilink.sent().await;
            assert_eq!(body["msg"]["context_token"], format!("ctx-{id}"));
            assert_eq!(sent_text(&body), FAILURE_REPLY);
        }
        // The same messages polled again start no turn; a new one does.
        ilink.push(vec![
            text_message("a", "sender", "again"),
            text_message("b", "sender", "again"),
            text_message("c", "third", "fresh"),
        ]);
        let (mut side, _) = accept_session(&daemon).await;
        assert_eq!(next_turn(&mut side).await, "fresh");
        let saved = store.load_state("default").unwrap();
        let claimed: Vec<_> = saved
            .in_flight
            .iter()
            .map(|c| c.message_id.as_str())
            .collect();
        assert_eq!(claimed, ["c"]);
        assert!(saved.pending.is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(300), daemon.accept())
                .await
                .is_err()
        );
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn shutdown_closes_every_running_turn_and_keeps_the_claims() {
    let directory = tempfile::tempdir().unwrap();
    let ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            None,
            &store,
            &|_| {},
        ),
    );
    let peer = async {
        ilink.push(vec![
            text_message("a", "first", "one"),
            text_message("b", "second", "two"),
        ]);
        let mut sides = Vec::new();
        for _ in 0..2 {
            let (mut side, _) = accept_session(&daemon).await;
            next_turn(&mut side).await;
            sides.push(side);
        }
        cancel.cancel();
        for side in &mut sides {
            assert_eq!(side.read(&mut [0; 1]).await.unwrap(), 0);
        }
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    assert!(store.lock("default").is_ok());
    let mut saved = store.load_state("default").unwrap();
    assert_eq!(saved.in_flight.len(), 2);
    recover_interrupted(&store, "default", &mut saved).unwrap();
    let replies: Vec<_> = saved.pending.iter().map(|p| p.reply.as_str()).collect();
    assert_eq!(replies, [FAILURE_REPLY, FAILURE_REPLY]);
}

/// Collects formatted log output for one test thread.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_refused_reply_rides_ahead_of_the_senders_next_reply_and_never_reaches_logs() {
    let logs = Logs::default();
    let writer = logs.clone();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let mut earlier = new_pending(
        "old",
        "sender",
        "ctx-old",
        "secret earlier answer",
        MAX_REPLY_BYTES,
    );
    earlier.key = "sender".into();
    let mut busy = new_pending("busy", "sender", "ctx-busy", BUSY_REPLY, MAX_REPLY_BYTES);
    busy.transient = true;
    store
        .save_state(
            "default",
            &state::BridgeState {
                pending: vec![earlier, busy],
                ..Default::default()
            },
        )
        .unwrap();
    ilink
        .responses
        .lock()
        .unwrap()
        .extend([("200 OK", r#"{"ret":-2}"#), ("400 Bad Request", "")]);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            None,
            &store,
            &|_| {},
        ),
    );
    let peer = async {
        assert_eq!(sent_text(&ilink.sent().await), "secret earlier answer");
        assert_eq!(sent_text(&ilink.sent().await), BUSY_REPLY);
        // Only the real reply is held; the refused busy notice is not.
        let saved = store.load_state("default").unwrap();
        assert_eq!(saved.held.len(), 1);
        assert_eq!(saved.held[0].reply, "secret earlier answer");
        ilink.push(vec![text_message("new", "sender", "status?")]);
        let (mut side, _) = accept_session(&daemon).await;
        assert_eq!(next_turn(&mut side).await, "status?");
        finish_turn(&mut side, "secret latest answer").await;
        let body = ilink.sent().await;
        assert_eq!(body["msg"]["context_token"], "ctx-new");
        assert_eq!(
            sent_text(&body),
            format!("{HELD_HEADER}secret earlier answer\n\n{LATEST_HEADER}secret latest answer")
        );
        wait_until(|| {
            let saved = store.load_state("default").unwrap();
            saved.held.is_empty() && saved.pending.is_empty()
        })
        .await;
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("rejected by iLink"), "{logs}");
    assert!(!logs.contains("secret"), "{logs}");
    assert!(!logs.contains("status?"), "{logs}");
}

#[test]
fn held_replies_are_bounded_per_conversation_and_expire() {
    let day = 24 * 60 * 60;
    let now = 30 * day;
    let held = |key: &str, reply: String, held_at: u64| state::HeldReply {
        key: key.into(),
        to_user_id: "u".into(),
        reply,
        held_at,
    };
    let mut state = state::BridgeState::default();
    state.held.push(held("a", "expired".into(), now - 7 * day));
    for index in 0..6 {
        state
            .held
            .push(held("a", format!("a{index}"), now - 6 * day + index));
    }
    state.held.push(held("b", "b0".into(), now));
    prune_held(&mut state, now);
    let kept: Vec<_> = state.held.iter().map(|h| h.reply.as_str()).collect();
    assert_eq!(kept, ["a2", "a3", "a4", "a5", "b0"]);

    // Bytes per conversation: the newest replies that fit are kept.
    let mut state = state::BridgeState::default();
    // Three of these exceed the per-conversation byte budget.
    let big = "x".repeat(HELD_MAX_BYTES_PER_CONVERSATION * 3 / 8);
    for index in 0..3 {
        state.held.push(held("a", big.clone(), now + index));
    }
    state.held.push(held("a", "small".into(), now + 3));
    prune_held(&mut state, now);
    assert_eq!(state.held.len(), 3);
    assert_eq!(state.held.last().unwrap().reply, "small");

    // The overall count keeps the newest across conversations.
    let mut state = state::BridgeState::default();
    for index in 0..(HELD_MAX_TOTAL as u64 + 5) {
        state
            .held
            .push(held(&format!("k{index}"), "r".into(), now + index));
    }
    prune_held(&mut state, now);
    assert_eq!(state.held.len(), HELD_MAX_TOTAL);
    assert_eq!(state.held[0].key, "k5");

    // Long refused replies are shortened so they fit beside a new reply.
    let long = "🙂".repeat(MAX_REPLY_BYTES);
    let truncated = truncate_held(long);
    assert!(truncated.len() <= MAX_HELD_REPLY_BYTES);
    assert!(truncated.ends_with("[truncated]"));
}

#[test]
fn only_held_replies_that_fit_ride_along_and_refusal_restores_them() {
    let claim = state::InFlight {
        message_id: "m".into(),
        to_user_id: "u".into(),
        context_token: "c".into(),
        key: String::new(),
    };
    let held = |key: &str, reply: &str, held_at: u64| state::HeldReply {
        key: key.into(),
        to_user_id: "u".into(),
        reply: reply.into(),
        held_at,
    };
    let now = unix_now();
    let mut state = state::BridgeState {
        held: vec![
            held("u", "one", now - 2),
            held("other", "x", now - 1),
            held("u", "two", now),
        ],
        ..Default::default()
    };
    let pending = compose_pending(&mut state, &claim, "new", now);
    assert_eq!(
        pending.reply,
        format!("{HELD_HEADER}one\n\n{HELD_HEADER}two\n\n{LATEST_HEADER}new")
    );
    assert_eq!(pending.own.as_deref(), Some("new"));
    assert_eq!(state.held, [held("other", "x", now - 1)]);

    // Refused before any chunk arrived: carried replies return ahead of it.
    let chunks = split_utf8(&pending.reply, MAX_REPLY_BYTES);
    hold_refused(&mut state, &pending, &chunks, now + 1);
    let order: Vec<_> = state.held.iter().map(|h| h.reply.as_str()).collect();
    assert_eq!(order, ["one", "x", "two", "new"]);

    // A new reply too large to share a message leaves held replies waiting.
    let own = "y".repeat(MAX_REPLY_BYTES - LATEST_HEADER.len());
    let pending = compose_pending(&mut state, &claim, &own, now + 1);
    assert_eq!(pending.reply, own);
    assert!(pending.carried.is_empty());
    assert_eq!(state.held.len(), 4);
}

#[test]
fn owner_turns_outlast_the_longest_tool_call() {
    assert_eq!(
        owner_turn_timeout(Duration::from_secs(1800)),
        Duration::from_secs(2100)
    );
    assert_eq!(
        owner_turn_timeout(Duration::from_secs(3600)),
        Duration::from_secs(3900)
    );
    // Short tool ceilings keep the 30-minute floor for multi-step turns.
    assert_eq!(
        owner_turn_timeout(Duration::from_secs(60)),
        OWNER_TURN_TIMEOUT
    );
    assert_eq!(owner_turn_timeout(Duration::MAX), Duration::MAX);
}
