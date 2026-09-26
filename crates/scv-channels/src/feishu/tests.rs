//! The transport against fake Feishu services on loopback: an HTTP API and a
//! long-connection socket. No test contacts Feishu.

use super::*;
use crate::feishu::{
    credentials::{Account, Brand},
    frame::{Frame, Header, METHOD_CONTROL, METHOD_DATA},
};
use futures_util::{SinkExt as _, StreamExt as _};
use prost::Message as _;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite;

const WAIT: Duration = Duration::from_secs(5);

/// Scripted answers: a route prefix, a status, and a body.
type Script = Arc<StdMutex<VecDeque<(&'static str, u16, String)>>>;
type Resources = Arc<StdMutex<std::collections::HashMap<String, (String, Vec<u8>)>>>;

#[derive(Debug)]
struct Request {
    method: String,
    route: String,
    authorization: Option<String>,
    body: Value,
}

/// Fake Feishu: HTTP routes answer from a script (per route prefix) or a
/// default, and a socket server relays frames to and from the test.
struct Fake {
    origin: String,
    requests: mpsc::UnboundedReceiver<Request>,
    script: Script,
    /// Message resources by route: a content type and bytes.
    resources: Resources,
    to_client: mpsc::UnboundedSender<Frame>,
    from_client: mpsc::UnboundedReceiver<Frame>,
    /// Closes the current socket connection.
    kick: mpsc::UnboundedSender<()>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Fake {
    async fn start() -> Self {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", http.local_addr().unwrap());
        let socket_url = format!(
            "ws://{}/ws/v2?device_id=d&service_id=7&access_key=secret-key",
            ws.local_addr().unwrap()
        );
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let script: Script = Arc::new(StdMutex::new(VecDeque::new()));
        let routes = Arc::clone(&script);
        let resources: Resources = Arc::default();
        let files = Arc::clone(&resources);
        let http_task = tokio::spawn(async move {
            loop {
                let (stream, _) = http.accept().await.unwrap();
                let (requests_tx, routes, socket_url, files) = (
                    requests_tx.clone(),
                    Arc::clone(&routes),
                    socket_url.clone(),
                    Arc::clone(&files),
                );
                tokio::spawn(async move {
                    let (mut stream, request) = read_request(stream).await;
                    let file = files.lock().unwrap().get(&request.route).cloned();
                    if let Some((content_type, bytes)) = file {
                        let _ = requests_tx.send(request);
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(&bytes).await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                    let scripted = {
                        let mut routes = routes.lock().unwrap();
                        let index = routes
                            .iter()
                            .position(|(prefix, _, _)| request.route.starts_with(prefix));
                        index.and_then(|index| routes.remove(index))
                    };
                    let (status, body) = match scripted {
                        Some((_, status, body)) => (status, body),
                        None => default_response(&request, &socket_url),
                    };
                    let _ = requests_tx.send(request);
                    respond(&mut stream, status, &body).await;
                });
            }
        });
        let (to_client, mut outgoing) = mpsc::unbounded_channel::<Frame>();
        let (incoming, from_client) = mpsc::unbounded_channel();
        let (kick, mut kicked) = mpsc::unbounded_channel::<()>();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&connections);
        let ws_task = tokio::spawn(async move {
            // One connection at a time; a new one replaces the old.
            loop {
                let (stream, _) = ws.accept().await.unwrap();
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                loop {
                    tokio::select! {
                        _ = kicked.recv() => break,
                        frame = outgoing.recv() => {
                            let Some(frame) = frame else { return };
                            if socket.send(tungstenite::Message::Binary(frame.encode_to_vec().into())).await.is_err() {
                                break;
                            }
                        }
                        message = socket.next() => match message {
                            Some(Ok(tungstenite::Message::Binary(bytes))) => {
                                let _ = incoming.send(Frame::decode(bytes.as_ref()).unwrap());
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                    }
                }
            }
        });
        Self {
            origin,
            requests,
            script,
            resources,
            to_client,
            from_client,
            kick,
            connections,
            tasks: vec![http_task, ws_task],
        }
    }

    fn script(&self, prefix: &'static str, status: u16, body: Value) {
        self.script
            .lock()
            .unwrap()
            .push_back((prefix, status, body.to_string()));
    }

    /// Serve a file at `route` with its content type.
    fn resource(&self, route: &str, content_type: &str, bytes: &[u8]) {
        self.resources
            .lock()
            .unwrap()
            .insert(route.into(), (content_type.into(), bytes.to_vec()));
    }

    fn transport(&self) -> OpenPlatform {
        let mut transport = OpenPlatform::new(Endpoints::local(&self.origin), &account()).unwrap();
        transport.window = Duration::from_millis(300);
        transport
    }

    /// The next request to a route with this prefix, skipping others.
    async fn request(&mut self, prefix: &str) -> Request {
        tokio::time::timeout(WAIT, async {
            loop {
                let request = self.requests.recv().await.unwrap();
                if request.route.starts_with(prefix) {
                    return request;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("a request to {prefix}"))
    }

    /// The next frame the client sends that is not a ping.
    async fn frame(&mut self) -> Frame {
        tokio::time::timeout(WAIT, async {
            loop {
                let frame = self.from_client.recv().await.unwrap();
                if frame.header("type") != Some("ping") {
                    return frame;
                }
            }
        })
        .await
        .expect("a frame from the client")
    }

    fn send_event(&self, message_id: &str, payload: &Value, parts: usize) {
        let bytes = payload.to_string().into_bytes();
        let size = bytes.len().div_ceil(parts);
        for (seq, chunk) in bytes.chunks(size).enumerate() {
            self.to_client
                .send(Frame {
                    seq_id: seq as u64,
                    log_id: 42,
                    service: 7,
                    method: METHOD_DATA,
                    headers: vec![
                        header("type", "event"),
                        header("message_id", message_id),
                        header("sum", &parts.to_string()),
                        header("seq", &seq.to_string()),
                        header("trace_id", "trace"),
                    ],
                    payload: Some(chunk.to_vec()),
                    ..Default::default()
                })
                .unwrap();
        }
    }
}

fn header(key: &str, value: &str) -> Header {
    Header {
        key: key.into(),
        value: value.into(),
    }
}

fn default_response(request: &Request, socket_url: &str) -> (u16, String) {
    let route = request.route.as_str();
    let body = if route.starts_with("/open-apis/auth/v3/tenant_access_token/internal") {
        json!({"code": 0, "tenant_access_token": "t-1", "expire": 7200})
    } else if route.starts_with("/open-apis/bot/v3/info") {
        json!({"code": 0, "bot": {"open_id": "ou_bot"}})
    } else if route.starts_with("/callback/ws/endpoint") {
        json!({"code": 0, "data": {"URL": socket_url, "ClientConfig": {"PingInterval": 90}}})
    } else if route.starts_with("/open-apis/im/v1/messages") && request.method == "GET" {
        json!({"code": 0, "data": {"items": [], "has_more": false}})
    } else if route.starts_with("/open-apis/im/v1/messages") {
        json!({"code": 0, "data": {"message_id": "om_sent"}})
    } else {
        return (404, "{}".into());
    };
    (200, body.to_string())
}

async fn read_request(stream: TcpStream) -> (TcpStream, Request) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap().to_owned();
    let route = parts.next().unwrap().to_owned();
    let mut length = 0;
    let mut authorization = None;
    let mut form = false;
    let mut multipart = false;
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            if key.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap();
            } else if key.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_owned());
            } else if key.eq_ignore_ascii_case("content-type") {
                form = value.starts_with("application/x-www-form-urlencoded");
                multipart = value.starts_with("multipart/form-data");
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else if multipart {
        // Uploads: keep the raw form for assertions.
        Value::String(String::from_utf8_lossy(&body).into_owned())
    } else if form {
        // Registration posts forms; present them as JSON for assertions.
        let text = String::from_utf8(body).unwrap();
        Value::Object(
            text.split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.to_owned(), Value::String(v.replace('+', " "))))
                .collect(),
        )
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (
        reader.into_inner(),
        Request {
            method,
            route,
            authorization,
            body,
        },
    )
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn account() -> Account {
    Account {
        app_id: "cli_a1b2c3d4e5f60718".into(),
        app_secret: "app-secret".into(),
        brand: Brand::Feishu,
        owner_open_id: Some("ou_owner".into()),
    }
}

fn message_event(id: &str, chat_type: &str, text: &str, created_ms: u64) -> Value {
    json!({
        "schema": "2.0",
        "header": {"event_id": format!("ev-{id}"), "event_type": "im.message.receive_v1"},
        "event": {
            "sender": {"sender_id": {"open_id": "ou_owner"}, "sender_type": "user"},
            "message": {
                "message_id": id, "chat_id": "oc_dm", "chat_type": chat_type,
                "create_time": created_ms.to_string(), "message_type": "text",
                "content": json!({"text": text}).to_string(),
            },
        },
    })
}

fn history_item(id: &str, text: &str, created_ms: u64) -> Value {
    json!({
        "message_id": id, "chat_id": "oc_dm", "msg_type": "text",
        "create_time": created_ms.to_string(),
        "sender": {"id": "ou_owner", "id_type": "open_id", "sender_type": "user"},
        "body": {"content": json!({"text": text}).to_string()},
    })
}

fn texts(batch: &Batch) -> Vec<(String, String)> {
    batch
        .messages
        .iter()
        .filter_map(|inbound| match inbound {
            Inbound::Text(message) => Some((message.id.clone(), message.text.clone())),
            Inbound::Ignored { .. } => None,
        })
        .collect()
}

fn now_ms() -> u64 {
    unix_seconds() * 1000
}

#[tokio::test]
async fn catch_up_then_socket_events_acknowledged_only_after_the_next_receive() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let last = now_ms() - 60_000;
    let checkpoint = Checkpoint {
        chats: [(
            "oc_dm".to_owned(),
            inbound::ChatMark {
                group: false,
                last_ms: last,
            },
        )]
        .into(),
    };
    // One message arrived while SCV was away; an older one is already seen.
    fake.script(
        "/open-apis/im/v1/messages?",
        200,
        json!({"code": 0, "data": {"has_more": false, "items": [
            history_item("om_old", "already handled", last - 1),
            history_item("om_away", "sent while offline", last + 5_000),
        ]}}),
    );
    let first = transport.receive(&checkpoint.to_json()).await.unwrap();
    assert_eq!(
        texts(&first),
        vec![("om_away".to_owned(), "sent while offline".to_owned())]
    );
    let saved = Checkpoint::parse(first.checkpoint.as_deref().unwrap());
    assert_eq!(saved.chats["oc_dm"].last_ms, last + 5_000);

    // Catch-up asked for this chat from its checkpoint, with the app token.
    let history = fake.request("/open-apis/im/v1/messages?").await;
    assert_eq!(history.method, "GET");
    assert_eq!(history.authorization.as_deref(), Some("Bearer t-1"));
    assert!(history.route.contains("container_id_type=chat"));
    assert!(history.route.contains("container_id=oc_dm"));
    assert!(
        history
            .route
            .contains(&format!("start_time={}", last / 1000))
    );
    // The socket pinged the connection's service as soon as it connected.
    let ping = tokio::time::timeout(WAIT, fake.from_client.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (ping.method, ping.service, ping.header("type")),
        (METHOD_CONTROL, 7, Some("ping"))
    );

    // A split event arrives whole, and is not acknowledged yet.
    let created = now_ms();
    fake.send_event(
        "frame-1",
        &message_event("om_live", "p2p", "hello", created),
        3,
    );
    let second = transport.receive(&saved.to_json()).await.unwrap();
    assert_eq!(
        texts(&second),
        vec![("om_live".to_owned(), "hello".to_owned())]
    );
    assert!(fake.from_client.try_recv().is_err());

    // Asking for the next batch acknowledges it: the same frame, a success
    // payload, and the handling time. Other events are acknowledged at once
    // and produce no message.
    let read = json!({"header": {"event_type": "im.message.message_read_v1"}, "event": {}});
    fake.send_event("frame-2", &read, 1);
    let third = transport
        .receive(second.checkpoint.as_deref().unwrap())
        .await
        .unwrap();
    assert!(third.messages.is_empty());
    let ack = fake.frame().await;
    assert_eq!(ack.header("message_id"), Some("frame-1"));
    assert!(ack.header("biz_rt").is_some());
    let body: Value = serde_json::from_slice(ack.payload.as_deref().unwrap()).unwrap();
    assert_eq!(body["code"], 200);
    assert_eq!(fake.frame().await.header("message_id"), Some("frame-2"));
    assert_eq!(
        fake.connections.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn a_dropped_socket_reconnects_and_catches_up_again() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let checkpoint = Checkpoint {
        chats: [(
            "oc_dm".to_owned(),
            inbound::ChatMark {
                group: false,
                last_ms: now_ms() - 1_000,
            },
        )]
        .into(),
    }
    .to_json();
    transport.receive(&checkpoint).await.unwrap();
    fake.request("/open-apis/im/v1/messages?").await;
    fake.kick.send(()).unwrap();
    // The lost connection reports unhealthy; the bridge backs off and asks again.
    assert!(transport.receive(&checkpoint).await.is_err());
    fake.script(
        "/open-apis/im/v1/messages?",
        200,
        json!({"code": 0, "data": {"has_more": false, "items": [
            history_item("om_gap", "sent during the gap", now_ms()),
        ]}}),
    );
    let batch = transport.receive(&checkpoint).await.unwrap();
    assert_eq!(
        texts(&batch),
        vec![("om_gap".to_owned(), "sent during the gap".to_owned())]
    );
    fake.request("/callback/ws/endpoint").await;
    fake.request("/open-apis/im/v1/messages?").await;
    assert_eq!(
        fake.connections.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[tokio::test]
async fn replies_and_direct_messages_retry_with_the_same_uuid() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let reports = StdMutex::new(Vec::new());
    let report = |healthy| reports.lock().unwrap().push(healthy);
    fake.script("/open-apis/im/v1/messages/om_1/reply", 503, json!({}));
    let outcome = transport
        .send(
            &Outbound {
                to: "ou_owner",
                reply_to: "om_1",
                part: 0,
                text: "done <at user_id=\"all\">all</at>",
                client_id: "uuid-1",
            },
            &report,
        )
        .await
        .unwrap();
    assert_eq!(outcome, SendOutcome::Delivered);
    let first = fake.request("/open-apis/im/v1/messages/om_1/reply").await;
    let second = fake.request("/open-apis/im/v1/messages/om_1/reply").await;
    assert_eq!(first.body, second.body);
    assert_eq!(first.body["uuid"], "uuid-1");
    assert_eq!(first.body["msg_type"], "text");
    let content: Value = serde_json::from_str(first.body["content"].as_str().unwrap()).unwrap();
    // Mentions from model output never notify anyone.
    assert_eq!(content["text"], "done <\u{200b}at user_id=\"all\">all</at>");
    assert_eq!(*reports.lock().unwrap(), vec![false]);

    // A message that answers nothing goes to the user by open_id.
    let outcome = transport
        .send(
            &Outbound {
                to: "ou_owner",
                reply_to: "",
                part: 0,
                text: "background job finished",
                client_id: "uuid-2",
            },
            &report,
        )
        .await
        .unwrap();
    assert_eq!(outcome, SendOutcome::Delivered);
    let direct = fake.request("/open-apis/im/v1/messages?").await;
    assert_eq!(direct.method, "POST");
    assert!(direct.route.contains("receive_id_type=open_id"));
    assert_eq!(direct.body["receive_id"], "ou_owner");
    assert_eq!(direct.body["uuid"], "uuid-2");
}

#[tokio::test]
async fn a_refusal_is_final_and_a_bad_token_is_renewed() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let outbound = Outbound {
        to: "ou_owner",
        reply_to: "om_1",
        part: 0,
        text: "hi",
        client_id: "uuid-1",
    };
    fake.script(
        "/open-apis/im/v1/messages/om_1/reply",
        400,
        json!({"code": 230002, "msg": "bot not in chat"}),
    );
    assert_eq!(
        transport.send(&outbound, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
    fake.request("/open-apis/auth/v3/tenant_access_token/internal")
        .await;
    fake.request("/open-apis/im/v1/messages/om_1/reply").await;
    // An invalid-token answer drops the cached token; the retry fetches a new one.
    fake.script(
        "/open-apis/im/v1/messages/om_1/reply",
        400,
        json!({"code": 99991663}),
    );
    assert_eq!(
        transport.send(&outbound, &|_| {}).await.unwrap(),
        SendOutcome::Delivered
    );
    fake.request("/open-apis/im/v1/messages/om_1/reply").await;
    fake.request("/open-apis/auth/v3/tenant_access_token/internal")
        .await;
    fake.request("/open-apis/im/v1/messages/om_1/reply").await;
}

#[tokio::test]
async fn an_untrusted_socket_host_is_never_dialed() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    fake.script(
        "/callback/ws/endpoint",
        200,
        json!({"code": 0, "data": {"URL": "wss://attacker.test/ws/v2"}}),
    );
    let error = transport.receive("").await.err().unwrap();
    assert!(format!("{error:#}").contains("untrusted"));
    assert_eq!(
        fake.connections.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn full_bridge_answers_a_caught_up_message_and_saves_the_checkpoint() {
    let mut fake = Fake::start().await;
    let directory = tempfile::tempdir().unwrap();
    let store = credentials::Store::new(
        &scv_client::Layout::new(directory.path()),
        crate::feishu::CHANNEL,
    );
    store.save_account("default", &account()).unwrap();
    let last = now_ms() - 60_000;
    let mut saved = store.load_state("default").unwrap();
    saved.cursor = Checkpoint {
        chats: [(
            "oc_dm".to_owned(),
            inbound::ChatMark {
                group: false,
                last_ms: last,
            },
        )]
        .into(),
    }
    .to_json();
    store.save_state("default", &saved).unwrap();
    fake.script(
        "/open-apis/im/v1/messages?",
        200,
        json!({"code": 0, "data": {"has_more": false, "items": [
            history_item("om_away", "status?", last + 1_000),
        ]}}),
    );
    let transport = fake.transport();
    // No daemon listens here, so the turn fails and the bridge answers with
    // its failure reply, through Feishu's reply API.
    let socket = directory.path().join("missing.sock");
    let reports = StdMutex::new(Vec::new());
    let report = |healthy| reports.lock().unwrap().push(healthy);
    let media = crate::MediaOptions::new(
        &scv_client::Layout::new(directory.path()),
        "feishu",
        "default",
        crate::media::MediaSettings::default(),
    );
    let detached = crate::hub::Link::detached();
    let run = crate::serve(
        &transport,
        crate::BridgeRun {
            account: "default",
            workspace: directory.path(),
            socket: &socket,
            owner: None,
            tool_owner: None,
            senders: crate::state::Senders::Anyone,
            media,
            link: &detached,
            report: &report,
        },
        &store,
        |credentials| Ok(credentials == &account()),
    );
    let peer = async {
        let reply = fake
            .request("/open-apis/im/v1/messages/om_away/reply")
            .await;
        let content: Value = serde_json::from_str(reply.body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["text"], crate::FAILURE_REPLY);
        assert!(!reply.body["uuid"].as_str().unwrap().is_empty());
        // The reply is delivered and remembered; the checkpoint moved.
        tokio::time::timeout(WAIT, async {
            loop {
                let state = store.load_state("default").unwrap();
                if state.pending.is_empty() && state.seen.iter().any(|id| id == "om_away") {
                    let checkpoint = Checkpoint::parse(&state.cursor);
                    assert_eq!(checkpoint.chats["oc_dm"].last_ms, last + 1_000);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    };
    tokio::select! {
        result = run => panic!("the bridge stopped: {result:?}"),
        () = peer => {}
    }
    assert!(reports.lock().unwrap().contains(&true));
}

#[tokio::test]
async fn full_bridge_answers_the_owners_voice_message_with_the_voice_reply() {
    let mut fake = Fake::start().await;
    let directory = tempfile::tempdir().unwrap();
    let store = credentials::Store::new(
        &scv_client::Layout::new(directory.path()),
        crate::feishu::CHANNEL,
    );
    store.save_account("default", &account()).unwrap();
    let last = now_ms() - 60_000;
    let mut saved = store.load_state("default").unwrap();
    saved.cursor = Checkpoint {
        chats: [(
            "oc_dm".to_owned(),
            inbound::ChatMark {
                group: false,
                last_ms: last,
            },
        )]
        .into(),
    }
    .to_json();
    store.save_state("default", &saved).unwrap();
    // Feishu sends a voice message as an Opus file and never a transcript.
    let mut voice = history_item("om_voice", "", last + 1_000);
    voice["msg_type"] = json!("audio");
    voice["body"]["content"] =
        json!(json!({"file_key": "file_v3_voice", "duration": 2000}).to_string());
    fake.script(
        "/open-apis/im/v1/messages?",
        200,
        json!({"code": 0, "data": {"has_more": false, "items": [voice]}}),
    );
    let transport = fake.transport();
    // No daemon listens here: a turn would fail with the failure reply.
    let socket = directory.path().join("missing.sock");
    let media = crate::MediaOptions::new(
        &scv_client::Layout::new(directory.path()),
        "feishu",
        "default",
        crate::media::MediaSettings::default(),
    );
    let detached = crate::hub::Link::detached();
    let run = crate::serve(
        &transport,
        crate::BridgeRun {
            account: "default",
            workspace: directory.path(),
            socket: &socket,
            owner: Some("ou_owner"),
            tool_owner: None,
            senders: crate::state::Senders::Owner,
            media,
            link: &detached,
            report: &|_| {},
        },
        &store,
        |credentials| Ok(credentials == &account()),
    );
    let peer = async {
        // Every request up to the reply, none of them for the voice file.
        let reply = tokio::time::timeout(WAIT, async {
            loop {
                let request = fake.requests.recv().await.unwrap();
                assert!(!request.route.contains("/resources/"), "{}", request.route);
                if request
                    .route
                    .starts_with("/open-apis/im/v1/messages/om_voice/reply")
                {
                    return request;
                }
            }
        })
        .await
        .expect("the voice reply is sent");
        let content: Value = serde_json::from_str(reply.body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["text"], crate::VOICE_REPLY);
        tokio::time::timeout(WAIT, async {
            loop {
                let state = store.load_state("default").unwrap();
                if state.pending.is_empty() && state.seen.iter().any(|id| id == "om_voice") {
                    assert!(state.in_flight.is_empty());
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        while let Ok(request) = fake.requests.try_recv() {
            assert!(!request.route.contains("/resources/"), "{}", request.route);
        }
    };
    tokio::select! {
        result = run => panic!("the bridge stopped: {result:?}"),
        () = peer => {}
    }
}

#[tokio::test]
async fn full_bridge_never_takes_a_caught_up_yes_from_before_the_question_as_its_answer() {
    let mut fake = Fake::start().await;
    let directory = tempfile::tempdir().unwrap();
    let store = credentials::Store::new(
        &scv_client::Layout::new(directory.path()),
        crate::feishu::CHANNEL,
    );
    store.save_account("default", &account()).unwrap();
    // The owner said yes to something else while SCV was disconnected.
    let written = crate::hub::unix_ms() - 30_000;
    let mut saved = store.load_state("default").unwrap();
    saved.cursor = Checkpoint {
        chats: [(
            "oc_dm".to_owned(),
            inbound::ChatMark {
                group: false,
                last_ms: written - 60_000,
            },
        )]
        .into(),
    }
    .to_json();
    store.save_state("default", &saved).unwrap();
    let transport = fake.transport();
    // No daemon listens here: a turn fails with the failure reply.
    let socket = directory.path().join("missing.sock");
    let media = crate::MediaOptions::new(
        &scv_client::Layout::new(directory.path()),
        "feishu",
        "default",
        crate::media::MediaSettings::default(),
    );
    let hub = crate::hub::Hub::new(None);
    let link = crate::hub::Link::new(Arc::clone(&hub), "feishu:default", Some("ou_owner".into()));
    let run = crate::serve(
        &transport,
        crate::BridgeRun {
            account: "default",
            workspace: directory.path(),
            socket: &socket,
            owner: Some("ou_owner"),
            tool_owner: None,
            senders: crate::state::Senders::Owner,
            media,
            link: &link,
            report: &|_| {},
        },
        &store,
        |credentials| Ok(credentials == &account()),
    );
    let delivered = || async {
        tokio::time::timeout(WAIT, async {
            while !store.load_state("default").unwrap().pending.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("everything queued is delivered");
    };
    let text_of = |request: &Request| -> Value {
        serde_json::from_str::<Value>(request.body["content"].as_str().unwrap()).unwrap()["text"]
            .clone()
    };
    let peer = async {
        tokio::time::timeout(WAIT, async {
            while hub.owner("feishu:default").is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let answered = hub.ask("q1", "feishu:default", "ou_owner").unwrap();
        hub.send_question("q1", "feishu:default", "ou_owner", "Publish?")
            .await
            .unwrap();
        let question = fake
            .request("/open-apis/im/v1/messages?receive_id_type=open_id")
            .await;
        assert_eq!(text_of(&question), "Publish?");
        delivered().await;
        // The connection drops, and catch-up hands over the earlier yes.
        fake.script(
            "/open-apis/im/v1/messages?",
            200,
            json!({"code": 0, "data": {"has_more": false, "items": [
                history_item("om_before", "yes", written),
            ]}}),
        );
        fake.kick.send(()).unwrap();
        // It runs a turn, as any other message would.
        let reply = fake
            .request("/open-apis/im/v1/messages/om_before/reply")
            .await;
        assert_eq!(text_of(&reply), crate::FAILURE_REPLY);
        // The owner's yes sent now answers the question, without a turn.
        fake.send_event(
            "frame-1",
            &message_event("om_after", "p2p", "yes", crate::hub::unix_ms()),
            1,
        );
        let reply = fake
            .request("/open-apis/im/v1/messages/om_after/reply")
            .await;
        assert_eq!(text_of(&reply), crate::ANSWERED_YES);
        assert_eq!(answered.await, Ok(true));
    };
    tokio::select! {
        result = run => panic!("the bridge stopped: {result:?}"),
        () = peer => {}
    }
}

#[tokio::test]
async fn registration_waits_for_the_scan_and_follows_a_lark_tenant() {
    let mut fake = Fake::start().await;
    let endpoints = Endpoints::local(&fake.origin);
    let client = api::http_client().unwrap();
    fake.script(
        "/oauth/v1/app/registration",
        200,
        json!({"supported_auth_methods": ["client_secret", "private_key_jwt"]}),
    );
    fake.script(
        "/oauth/v1/app/registration",
        200,
        json!({"device_code": "device", "user_code": "ABCD-1234", "interval": 1, "expire_in": 60}),
    );
    let pending = login::begin(&client, &endpoints, &endpoints).await.unwrap();
    assert_eq!(
        pending.url,
        format!("{}/page/cli?user_code=ABCD-1234", fake.origin)
    );
    assert_eq!(pending.interval, Duration::from_secs(1));
    let init = fake.request("/oauth/v1/app/registration").await;
    assert_eq!(init.body["action"], "init");
    let begin = fake.request("/oauth/v1/app/registration").await;
    assert_eq!(begin.body["archetype"], "PersonalAgent");
    assert_eq!(begin.body["request_user_info"], "open_id tenant_brand");

    fake.script(
        "/oauth/v1/app/registration",
        400,
        json!({"error": "authorization_pending"}),
    );
    fake.script(
        "/oauth/v1/app/registration",
        400,
        json!({"error": "authorization_pending", "user_info": {"tenant_brand": "lark"}}),
    );
    fake.script(
        "/oauth/v1/app/registration",
        200,
        json!({"client_id": "cli_1234abcd", "client_secret": "s", "user_info": {"open_id": "ou_creator", "tenant_brand": "lark"}}),
    );
    let registered = login::poll(&client, &endpoints, &pending).await.unwrap();
    let account = registered.account;
    assert_eq!(account.app_id, "cli_1234abcd");
    assert_eq!(account.brand, Brand::Lark);
    assert_eq!(account.owner_open_id.as_deref(), Some("ou_creator"));
    for _ in 0..3 {
        let poll = fake.request("/oauth/v1/app/registration").await;
        assert_eq!(poll.body["action"], "poll");
        assert_eq!(poll.body["device_code"], "device");
    }
}

#[tokio::test]
async fn registration_stops_when_declined_or_unsupported() {
    let fake = Fake::start().await;
    let endpoints = Endpoints::local(&fake.origin);
    let client = api::http_client().unwrap();
    fake.script(
        "/oauth/v1/app/registration",
        200,
        json!({"supported_auth_methods": ["private_key_jwt"]}),
    );
    assert!(login::begin(&client, &endpoints, &endpoints).await.is_err());
    let pending = login::Pending {
        device_code: "device".into(),
        url: String::new(),
        interval: Duration::from_secs(1),
        expires_in: Duration::from_secs(60),
    };
    fake.script(
        "/oauth/v1/app/registration",
        400,
        json!({"error": "access_denied"}),
    );
    let error = login::poll(&client, &endpoints, &pending)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("declined"));
}

#[tokio::test]
async fn non_token_and_oversized_responses_are_refused() {
    let fake = Fake::start().await;
    let api = Api::new(Endpoints::local(&fake.origin), "cli_1", "s").unwrap();
    fake.script(
        "/open-apis/auth/v3/tenant_access_token/internal",
        302,
        json!({}),
    );
    assert!(api.validate().await.is_err());
    let huge = "x".repeat(5 * 1024 * 1024);
    fake.script(
        "/open-apis/auth/v3/tenant_access_token/internal",
        200,
        Value::String(huge),
    );
    let error = api.validate().await.err().unwrap();
    assert!(error.to_string().contains("exceeds limit"));
}

#[test]
fn the_model_is_told_the_brand_its_users_know() {
    let origin = "http://127.0.0.1:9";
    let feishu = OpenPlatform::new(Endpoints::local(origin), &account()).unwrap();
    assert_eq!(crate::Transport::channel(&feishu), "Feishu");
    let lark = OpenPlatform::new(
        Endpoints::local(origin),
        &Account {
            brand: Brand::Lark,
            ..account()
        },
    )
    .unwrap();
    assert_eq!(crate::Transport::channel(&lark), "Lark");
}

fn media_of(content: Value, kind: &str) -> Media {
    inbound::parse_content("om_1", kind, &content)
        .unwrap()
        .media
        .remove(0)
}

#[tokio::test]
async fn files_download_through_the_resource_api_within_limits() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    fake.resource(
        "/open-apis/im/v1/messages/om_1/resources/img_k?type=image",
        "image/png",
        b"\x89PNGbytes",
    );
    let image = media_of(json!({"image_key": "img_k"}), "image");
    let downloaded = transport.download(&image, 1024).await.unwrap();
    assert_eq!(downloaded.bytes, b"\x89PNGbytes");
    assert_eq!(downloaded.mime.as_deref(), Some("image/png"));
    let request = fake
        .request("/open-apis/im/v1/messages/om_1/resources/")
        .await;
    assert_eq!(request.authorization.as_deref(), Some("Bearer t-1"));
    assert!(transport.download(&image, 4).await.is_err());

    // A missing permission comes back as a JSON error, even with 200.
    fake.script(
        "/open-apis/im/v1/messages/om_1/resources/file_k",
        400,
        json!({"code": 99991672, "msg": "Access denied"}),
    );
    let file = media_of(json!({"file_key": "file_k", "file_name": "a.pdf"}), "file");
    let error = transport
        .download(&file, 1024)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("lacks a permission"), "{error}");
    assert!(error.contains("im:resource"), "{error}");
}

#[tokio::test]
async fn quoted_and_forwarded_messages_are_resolved_into_context_and_files() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    fake.script(
        "/open-apis/im/v1/messages/om_parent",
        200,
        json!({"code": 0, "data": {"items": [
            {"message_id": "om_parent", "msg_type": "text",
             "body": {"content": json!({"text": "ship it?"}).to_string()}},
        ]}}),
    );
    let quoted = transport
        .resolve("om_1", r#"{"parent":"om_parent"}"#)
        .await
        .unwrap();
    assert_eq!(quoted.context, "[Quoting: ship it?]");
    assert!(quoted.media.is_empty());

    fake.script(
        "/open-apis/im/v1/messages/om_bundle",
        200,
        json!({"code": 0, "data": {"items": [
            {"message_id": "om_bundle", "msg_type": "merge_forward",
             "body": {"content": "Merged and Forwarded Message"}},
            {"message_id": "om_a", "msg_type": "text", "upper_message_id": "om_bundle",
             "body": {"content": json!({"text": "first"}).to_string()}},
            {"message_id": "om_b", "msg_type": "file", "upper_message_id": "om_bundle",
             "body": {"content": json!({"file_key": "file_b", "file_name": "b.txt"}).to_string()}},
        ]}}),
    );
    let forwarded = transport
        .resolve("om_bundle", r#"{"forward":true}"#)
        .await
        .unwrap();
    assert_eq!(forwarded.context, "[The forwarded messages:]\n- first");
    assert_eq!(forwarded.media.len(), 1);
    let resource: api::Resource = serde_json::from_str(&forwarded.media[0].source).unwrap();
    // Each file is fetched from the message that holds it.
    assert_eq!(
        (resource.message_id.as_str(), resource.key.as_str()),
        ("om_b", "file_b")
    );
}

#[tokio::test]
async fn files_upload_then_go_out_as_image_or_file_messages() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let directory = tempfile::tempdir().unwrap();
    let png = directory.path().join("chart.png");
    std::fs::write(&png, b"\x89PNGchart").unwrap();
    fake.script(
        "/open-apis/im/v1/images",
        200,
        json!({"code": 0, "data": {"image_key": "img_up"}}),
    );
    let outcome = transport
        .send_file(
            &OutboundFile {
                to: "ou_owner",
                reply_to: "om_1",
                part: 1,
                path: &png,
                name: "chart.png",
                kind: MediaKind::Image,
                client_id: "file-1",
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome, SendOutcome::Delivered);
    let upload = fake.request("/open-apis/im/v1/images").await;
    let form = upload.body.as_str().unwrap();
    assert!(
        form.contains("name=\"image_type\"\r\n\r\nmessage"),
        "{form}"
    );
    assert!(form.contains("filename=\"chart.png\""), "{form}");
    assert!(form.contains("\u{fffd}PNGchart"), "{form}");
    let message = fake.request("/open-apis/im/v1/messages/om_1/reply").await;
    assert_eq!(message.body["msg_type"], "image");
    assert_eq!(message.body["uuid"], "file-1");
    assert_eq!(
        message.body["content"],
        json!({"image_key": "img_up"}).to_string()
    );

    let report = directory.path().join("report.pdf");
    std::fs::write(&report, b"%PDF").unwrap();
    fake.script(
        "/open-apis/im/v1/files",
        200,
        json!({"code": 0, "data": {"file_key": "file_up"}}),
    );
    let file = OutboundFile {
        to: "ou_owner",
        reply_to: "",
        part: 0,
        path: &report,
        name: "report.pdf",
        kind: MediaKind::File,
        client_id: "file-2",
    };
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Delivered
    );
    let upload = fake.request("/open-apis/im/v1/files").await;
    let form = upload.body.as_str().unwrap();
    assert!(form.contains("name=\"file_type\"\r\n\r\npdf"), "{form}");
    let message = fake.request("/open-apis/im/v1/messages?").await;
    assert_eq!(message.body["receive_id"], "ou_owner");
    assert_eq!(message.body["msg_type"], "file");
    assert_eq!(
        message.body["content"],
        json!({"file_key": "file_up"}).to_string()
    );

    // A refused upload is final.
    fake.script("/open-apis/im/v1/files", 400, json!({"code": 234041}));
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
}
