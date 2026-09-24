use super::*;
use scv_channels::{
    BUSY_REPLY, FAILURE_REPLY, HELD_HEADER, LATEST_HEADER, MAX_CONCURRENT_TURNS,
    MAX_QUEUED_PER_CONVERSATION, MAX_REPLY_BYTES, MAX_TOTAL_REPLY_BYTES, new_pending,
    recover_interrupted,
};
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
    let store = state::Store::new(&scv_channels::Layout::new(directory), crate::CHANNEL);
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
                let initialized = json!({"type":"initialized","request_id":"channel-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"test","version":"0"}});
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
                let started = json!({"type":"session.started","request_id":"channel-session","session_id":"s","cwd":"/","model":"test","context_max_tokens":1024,"max_server_frame_bytes":1024,"max_transcript_bytes":1024,"max_transcript_items":10,"max_prompt_history_bytes":1024,"max_prompt_history_items":10});
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
        "channel state does not match saved credentials"
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
        scv_channels::hub::Link::detached(),
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
        "invalid channel account name"
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
    send_frame(&mut side, json!({"type":"initialized","request_id":"channel-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"test","version":"0"}})).await;
    let start = next_frame(&mut side).await;
    send_frame(&mut side, json!({"type":"session.started","request_id":"channel-session","session_id":"s","cwd":"/","model":"test","context_max_tokens":1024,"max_server_frame_bytes":1024,"max_transcript_bytes":1024,"max_transcript_items":10,"max_prompt_history_bytes":1024,"max_prompt_history_items":10})).await;
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
            // The model learns it is on WeChat; background jobs of an owner
            // session get the owner's blanket approval.
            assert_eq!(start["channel"], "WeChat");
            assert_eq!(start["auto_approve"], tools);
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

#[tokio::test]
async fn tool_progress_never_reaches_wechat() {
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
        ilink.push(vec![text_message("m1", "sender", "hello")]);
        let (mut side, start) = accept_session(&daemon).await;
        // A direct client declares no delegation depth.
        assert!(start.get("delegation_depth").is_none(), "{start}");
        assert_eq!(next_turn(&mut side).await, "hello");
        send_frame(&mut side, json!({"type":"tool.started","request_id":"r","session_id":"s","turn_id":"t","seq":1,"call_id":"c","name":"agent_codex"})).await;
        for seq in 2..5 {
            send_frame(&mut side, json!({"type":"tool.progress","request_id":"r","session_id":"s","turn_id":"t","seq":seq,"call_id":"c","text":format!("$ step {seq} PROGRESS")})).await;
        }
        send_frame(&mut side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":5,"call_id":"c","name":"agent_codex","success":true,"output":"{}","truncated":false})).await;
        send_frame(&mut side, json!({"type":"assistant.completed","request_id":"r","session_id":"s","turn_id":"t","seq":6,"content":"final answer"})).await;
        send_frame(&mut side, json!({"type":"turn.completed","request_id":"r","session_id":"s","turn_id":"t","seq":7,"steps":2,"usage":{}})).await;
        let body = ilink.sent().await;
        assert_eq!(sent_text(&body), "final answer");
        assert!(!body.to_string().contains("PROGRESS"));
        // Nothing else is sent for this turn.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), ilink.sent())
                .await
                .is_err()
        );
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn a_failed_turn_replies_with_the_generic_failure_message() {
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
        ilink.push(vec![text_message("m1", "sender", "hello")]);
        let (mut side, _) = accept_session(&daemon).await;
        assert_eq!(next_turn(&mut side).await, "hello");
        // The provider's own error text stays on the host.
        send_frame(&mut side, json!({"type":"turn.failed","request_id":"r","session_id":"s","turn_id":"t","seq":1,"code":"provider_error","message":"provider stream error: Our servers are currently overloaded (gave up after 3 attempts)"})).await;
        let body = ilink.sent().await;
        assert_eq!(body["msg"]["context_token"], "ctx-m1");
        assert_eq!(sent_text(&body), FAILURE_REPLY);
        wait_until(|| store.load_state("default").unwrap().pending.is_empty()).await;
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

/// Frames of a server-started turn reporting finished background jobs.
async fn report_turn(
    side: &mut BufReader<tokio::net::UnixStream>,
    request: &str,
    jobs: &[&str],
    content: &str,
) {
    let origin = json!({"kind":"background","jobs":jobs});
    send_frame(side, json!({"type":"turn.started","request_id":request,"session_id":"s","turn_id":format!("{request}-t"),"seq":20,"origin":origin})).await;
    send_frame(side, json!({"type":"assistant.completed","request_id":request,"session_id":"s","turn_id":format!("{request}-t"),"seq":21,"content":content})).await;
    send_frame(side, json!({"type":"turn.completed","request_id":request,"session_id":"s","turn_id":format!("{request}-t"),"seq":22,"steps":1,"usage":{},"origin":origin})).await;
}

/// The owner's turn that starts background job `job`.
async fn start_background_job(
    side: &mut BufReader<tokio::net::UnixStream>,
    job: &str,
    reply: &str,
) {
    let output =
        json!({"job":job,"tool":"agent_codex","status":"running","background":true}).to_string();
    send_frame(side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":1,"call_id":"c","name":"agent_codex","success":true,"output":output,"truncated":false})).await;
    finish_turn(side, reply).await;
}

/// No context token carries more than one message.
fn one_message_per_token(sends: &[Value]) {
    let mut tokens = std::collections::HashSet::new();
    for body in sends {
        if let Some(token) = body["msg"].get("context_token") {
            assert!(tokens.insert(token.to_string()), "second send on {token}");
        }
    }
}

#[tokio::test]
async fn a_finished_background_job_reaches_the_owner_as_one_unprompted_message() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = owner_of("sender");
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
        ilink.push(vec![text_message(
            "m1",
            "sender",
            "land it in the background",
        )]);
        let (mut side, _) = accept_session(&daemon).await;
        next_turn(&mut side).await;
        start_background_job(&mut side, "job-1", "Started job-1.").await;
        let reply = ilink.sent().await;
        assert_eq!(sent_text(&reply), "Started job-1.");
        assert_eq!(reply["msg"]["context_token"], "ctx-m1");
        // Later the server reports the job in a turn of its own; its approval
        // requests follow the owner's policy.
        let origin = json!({"kind":"background","jobs":["job-1"]});
        send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
        send_frame(&mut side, json!({"type":"approval.requested","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"approval_id":"a2","call_id":"c2","name":"bash","risk":"process","cwd":"/","summary":"git log"})).await;
        let resolved = next_frame(&mut side).await;
        assert_eq!(
            (
                resolved["approval_id"].as_str(),
                resolved["approved"].as_bool()
            ),
            (Some("a2"), Some(true))
        );
        send_frame(&mut side, json!({"type":"assistant.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":12,"content":"job-1 landed as 0.1.35."})).await;
        send_frame(&mut side, json!({"type":"turn.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":13,"steps":2,"usage":{},"origin":origin})).await;
        let report = ilink.sent().await;
        assert_eq!(sent_text(&report), "job-1 landed as 0.1.35.");
        assert_eq!(report["msg"]["to_user_id"], "sender");
        assert!(report["msg"].get("context_token").is_none(), "{report}");
        one_message_per_token(&[reply, report]);
        wait_until(|| store.load_state("default").unwrap().pending.is_empty()).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), ilink.sent())
                .await
                .is_err(),
            "nothing else is sent"
        );
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn a_report_finishing_during_the_owners_turn_follows_that_turns_reply() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = owner_of("sender");
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
        ilink.push(vec![text_message("m1", "sender", "start it")]);
        let (mut side, _) = accept_session(&daemon).await;
        next_turn(&mut side).await;
        start_background_job(&mut side, "job-1", "Started.").await;
        let first = ilink.sent().await;
        ilink.push(vec![text_message("m2", "sender", "what else?")]);
        assert_eq!(next_turn(&mut side).await, "what else?");
        // The server ran the report first; its events precede this turn's.
        report_turn(&mut side, "background:1", &["job-1"], "Job report.").await;
        finish_turn(&mut side, "Answer two.").await;
        let answer = ilink.sent().await;
        assert_eq!(sent_text(&answer), "Answer two.");
        assert_eq!(answer["msg"]["context_token"], "ctx-m2");
        let report = ilink.sent().await;
        assert_eq!(sent_text(&report), "Job report.");
        assert!(report["msg"].get("context_token").is_none());
        one_message_per_token(&[first, answer, report]);
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn a_long_reply_uses_its_context_token_once_and_the_rest_goes_unprompted() {
    for (length, truncated) in [
        (MAX_REPLY_BYTES + 5000, false),
        (MAX_TOTAL_REPLY_BYTES * 2, true),
    ] {
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
        let content = "é".repeat(length / 2);
        let peer = async {
            ilink.push(vec![text_message("m1", "sender", "write a lot")]);
            let (mut side, _) = accept_session(&daemon).await;
            next_turn(&mut side).await;
            finish_turn(&mut side, &content).await;
            let mut sends = vec![ilink.sent().await];
            while let Ok(body) =
                tokio::time::timeout(Duration::from_millis(500), ilink.sent()).await
            {
                sends.push(body);
            }
            assert!(sends.len() >= 2, "{} sends", sends.len());
            assert_eq!(sends[0]["msg"]["context_token"], "ctx-m1");
            for body in &sends[1..] {
                assert!(body["msg"].get("context_token").is_none(), "{body}");
                assert_eq!(body["msg"]["to_user_id"], "sender");
            }
            one_message_per_token(&sends);
            let text: String = sends.iter().map(sent_text).collect();
            if truncated {
                assert!(text.len() <= MAX_TOTAL_REPLY_BYTES);
                assert!(
                    text.ends_with("[reply truncated]"),
                    "{}",
                    &text[text.len() - 40..]
                );
            } else {
                assert_eq!(text, content, "a long reply is no longer a failure");
            }
            cancel.cancel();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
                .await
                .unwrap();
        result.unwrap();
    }
}

#[tokio::test]
async fn a_refused_background_report_is_held_for_the_owners_next_reply() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = owner_of("sender");
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
        ilink.push(vec![text_message("m1", "sender", "start it")]);
        let (mut side, _) = accept_session(&daemon).await;
        next_turn(&mut side).await;
        start_background_job(&mut side, "job-1", "Started.").await;
        ilink.sent().await;
        // iLink refuses the unprompted report outright.
        ilink
            .responses
            .lock()
            .unwrap()
            .push_back(("400 Bad Request", ""));
        report_turn(&mut side, "background:1", &["job-1"], "Job report.").await;
        let refused = ilink.sent().await;
        assert!(refused["msg"].get("context_token").is_none());
        wait_until(|| store.load_state("default").unwrap().held.len() == 1).await;
        // It rides ahead of the owner's next reply.
        ilink.push(vec![text_message("m2", "sender", "and now?")]);
        next_turn(&mut side).await;
        finish_turn(&mut side, "Now this.").await;
        let carried = ilink.sent().await;
        assert_eq!(carried["msg"]["context_token"], "ctx-m2");
        let text = sent_text(&carried);
        assert!(
            text.contains("Job report.") && text.ends_with("Now this."),
            "{text}"
        );
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn an_owner_turn_that_times_out_is_cancelled_without_ending_its_background_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = ToolOwner {
        user_id: "sender".into(),
        turn_timeout: Duration::from_millis(500),
    };
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
        ilink.push(vec![text_message("m1", "sender", "start it")]);
        let (mut side, _) = accept_session(&daemon).await;
        next_turn(&mut side).await;
        start_background_job(&mut side, "job-1", "Started.").await;
        ilink.sent().await;
        // The next turn starts but never finishes in time.
        ilink.push(vec![text_message("m2", "sender", "slow question")]);
        let start = next_frame(&mut side).await;
        assert_eq!(start["type"], "turn.start");
        let request = start["request_id"].as_str().unwrap().to_owned();
        send_frame(&mut side, json!({"type":"turn.started","request_id":request,"session_id":"s","turn_id":"slow","seq":30})).await;
        let cancelled = next_frame(&mut side).await;
        assert_eq!(
            (cancelled["type"].as_str(), cancelled["turn_id"].as_str()),
            (Some("turn.cancel"), Some("slow"))
        );
        let failure = ilink.sent().await;
        assert_eq!(sent_text(&failure), FAILURE_REPLY);
        // The late end of the abandoned turn is ignored, and the same session
        // still delivers the job's report: no new session was opened.
        send_frame(&mut side, json!({"type":"turn.cancelled","request_id":request,"session_id":"s","turn_id":"slow","seq":31})).await;
        report_turn(&mut side, "background:1", &["job-1"], "Job report.").await;
        let report = ilink.sent().await;
        assert_eq!(sent_text(&report), "Job report.");
        assert!(report["msg"].get("context_token").is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(300), daemon.accept())
                .await
                .is_err(),
            "the session was replaced"
        );
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

/// Frames of an owner turn whose `agent_codex` call starts background job
/// `job` for `prompt`.
async fn start_described_job(
    side: &mut BufReader<tokio::net::UnixStream>,
    job: &str,
    prompt: &str,
    reply: &str,
) {
    send_frame(side, json!({"type":"tool.proposed","request_id":"r","session_id":"s","turn_id":"t","seq":0,"call_id":"c","name":"agent_codex","arguments":{"prompt":prompt,"background":true}})).await;
    start_background_job(side, job, reply).await;
}

#[tokio::test]
async fn the_hub_sees_owner_work_until_its_report_is_stored_and_can_queue_notices() {
    use scv_channels::hub::{Hub, Link, Origin};
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = owner_of("sender");
    let hub = Hub::new(None);
    let link = Link::new(Arc::clone(&hub), "wechat:default", Some("sender".into()));
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop_linked(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            Some(&owner),
            &store,
            &|_| {},
            &link,
        ),
    );
    let peer = async {
        wait_until(|| hub.owner("wechat:default").is_some()).await;
        ilink.push(vec![text_message("m1", "sender", "land it")]);
        let (mut side, _) = accept_session(&daemon).await;
        next_turn(&mut side).await;
        // The owner's message is claimed and not yet answered.
        assert_eq!(hub.owner_claims(), 1);
        assert_eq!(hub.last_owner().unwrap().component, "wechat:default");
        start_described_job(
            &mut side,
            "job-1",
            "Land the fix\nthen publish",
            "Started job-1.",
        )
        .await;
        assert_eq!(sent_text(&ilink.sent().await), "Started job-1.");
        wait_until(|| hub.owner_claims() == 0).await;
        wait_until(|| hub.session_work("s") == 1).await;
        assert_eq!(
            hub.origin("s"),
            Some(Origin {
                component: "wechat:default".into(),
                peer: "sender".into()
            })
        );
        let jobs = store.load_state("default").unwrap().jobs;
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (
                jobs[0].job.as_str(),
                jobs[0].tool.as_str(),
                jobs[0].task.as_str()
            ),
            ("job-1", "agent_codex", "Land the fix")
        );
        report_turn(&mut side, "background:1", &["job-1"], "job-1 landed.").await;
        assert_eq!(sent_text(&ilink.sent().await), "job-1 landed.");
        wait_until(|| hub.session_work("s") == 0).await;
        wait_until(|| store.load_state("default").unwrap().jobs.is_empty()).await;
        // The daemon queues a notice; it goes out like a report.
        hub.notify("wechat:default", "sender", "SCV updated.")
            .await
            .unwrap();
        let notice = ilink.sent().await;
        assert_eq!(sent_text(&notice), "SCV updated.");
        assert!(notice["msg"].get("context_token").is_none(), "{notice}");
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
    assert!(hub.owner("wechat:default").is_none(), "withdrawn on stop");
}

#[tokio::test]
async fn a_report_turn_starting_during_the_owners_turn_is_still_delivered() {
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    let socket = directory.path().join("daemon.sock");
    let daemon = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let owner = owner_of("sender");
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
        ilink.push(vec![text_message("m1", "sender", "land it")]);
        let (mut side, _) = accept_session(&daemon).await;
        next_turn(&mut side).await;
        start_background_job(&mut side, "job-1", "Started job-1.").await;
        assert_eq!(sent_text(&ilink.sent().await), "Started job-1.");
        ilink.push(vec![text_message("m2", "sender", "status?")]);
        next_turn(&mut side).await;
        // The report turn starts while the owner's turn runs, and finishes
        // after it: the chat must keep reading it.
        let origin = json!({"kind":"background","jobs":["job-1"]});
        send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
        finish_turn(&mut side, "Still working.").await;
        assert_eq!(sent_text(&ilink.sent().await), "Still working.");
        send_frame(&mut side, json!({"type":"assistant.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"content":"job-1 landed."})).await;
        send_frame(&mut side, json!({"type":"turn.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":12,"steps":1,"usage":{},"origin":origin})).await;
        assert_eq!(sent_text(&ilink.sent().await), "job-1 landed.");
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn after_a_planned_restart_interrupted_work_is_described_as_such() {
    use scv_channels::hub::{Hub, Link, Restart};
    let directory = tempfile::tempdir().unwrap();
    let mut ilink = FakeIlink::start().await;
    let store = saved_store(directory.path(), &ilink.base);
    store
        .save_state(
            "default",
            &state::BridgeState {
                in_flight: vec![state::InFlight {
                    message_id: "m1".into(),
                    to_user_id: "sender".into(),
                    context_token: "ctx-m1".into(),
                    key: "sender".into(),
                }],
                jobs: vec![state::RunningJob {
                    to_user_id: "sender".into(),
                    job: "job-2".into(),
                    tool: "agent_claude".into(),
                    task: "Review the PR".into(),
                    started_at: 1,
                }],
                ..Default::default()
            },
        )
        .unwrap();
    let hub = Hub::new(None);
    let restart = Restart {
        to_version: "0.1.37".into(),
    };
    hub.set_restart(Some(restart.clone()));
    let link = Link::new(Arc::clone(&hub), "wechat:default", Some("sender".into()));
    let socket = directory.path().join("daemon.sock");
    let cancel = CancellationToken::new();
    let base = ilink.base.clone();
    let work = until_cancelled(
        cancel.clone(),
        run_loop_linked(
            "token",
            &base,
            "default",
            directory.path(),
            &socket,
            None,
            &store,
            &|_| {},
            &link,
        ),
    );
    let peer = async {
        let answer = ilink.sent().await;
        assert_eq!(answer["msg"]["context_token"], "ctx-m1");
        assert_eq!(sent_text(&answer), scv_channels::restarted_reply(&restart));
        let jobs = ilink.sent().await;
        assert_eq!(
            sent_text(&jobs),
            "SCV restarted to update to v0.1.37, which stopped background work that was \
             still running:\n- job-2 (claude): Review the PR\nAsk again if you still need it."
        );
        assert!(jobs["msg"].get("context_token").is_none());
        wait_until(|| {
            let saved = store.load_state("default").unwrap();
            saved.pending.is_empty() && saved.jobs.is_empty()
        })
        .await;
        cancel.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, peer) })
            .await
            .unwrap();
    result.unwrap();
}
