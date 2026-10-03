//! The transport against fake Slack services on loopback: the Web API, the
//! file hosts, and a Socket Mode socket. No test contacts Slack.

use super::*;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite;

pub(super) const WAIT: Duration = Duration::from_secs(5);

/// Scripted answers: a method or route prefix, a status, extra headers,
/// and a body.
type Script = Arc<StdMutex<VecDeque<(&'static str, u16, &'static str, String)>>>;
type Files = Arc<StdMutex<HashMap<String, (String, Vec<u8>)>>>;

#[derive(Debug)]
pub(super) struct Request {
    pub(super) method: String,
    /// The path and query, such as `/chat.postMessage`.
    pub(super) route: String,
    pub(super) authorization: Option<String>,
    /// Form fields or query parameters as a JSON object; raw bytes as a
    /// string.
    pub(super) body: Value,
}

/// Fake Slack: Web API methods answer from a script (per route prefix) or
/// a default, files are served from `/files/`, uploads taken at
/// `/upload/`, and a Socket Mode server says hello, relays the test's
/// frames, and reports what the client sends.
pub(super) struct Fake {
    pub(super) origin: String,
    requests: mpsc::UnboundedReceiver<Request>,
    script: Script,
    files: Files,
    to_client: mpsc::UnboundedSender<String>,
    from_client: mpsc::UnboundedReceiver<String>,
    /// Closes the current socket connection.
    kick: mpsc::UnboundedSender<()>,
    pub(super) connections: Arc<AtomicUsize>,
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
    pub(super) async fn start() -> Self {
        Self::with_hello(json!({"type": "hello", "connection_info": {"app_id": "A123"}})).await
    }

    /// A fake whose socket greets every connection with `hello`.
    pub(super) async fn with_hello(hello: Value) -> Self {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", http.local_addr().unwrap());
        let socket_url = format!("ws://{}/link/?ticket=secret", ws.local_addr().unwrap());
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let script: Script = Arc::default();
        let files: Files = Arc::default();
        let (routes, served, base) = (Arc::clone(&script), Arc::clone(&files), origin.clone());
        let http_task = tokio::spawn(async move {
            loop {
                let (stream, _) = http.accept().await.unwrap();
                let (requests_tx, routes, served, socket_url, base) = (
                    requests_tx.clone(),
                    Arc::clone(&routes),
                    Arc::clone(&served),
                    socket_url.clone(),
                    base.clone(),
                );
                tokio::spawn(async move {
                    let (mut stream, request) = read_request(stream).await;
                    let file = served.lock().unwrap().get(&request.route).cloned();
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
                            .position(|(prefix, ..)| request.route.starts_with(prefix));
                        index.and_then(|index| routes.remove(index))
                    };
                    let (status, headers, body) = match scripted {
                        Some((_, status, headers, body)) => (status, headers.to_owned(), body),
                        None => default_response(&request, &socket_url, &base),
                    };
                    let _ = requests_tx.send(request);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        let (to_client, mut outgoing) = mpsc::unbounded_channel::<String>();
        let (incoming, from_client) = mpsc::unbounded_channel();
        let (kick, mut kicked) = mpsc::unbounded_channel::<()>();
        let connections = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&connections);
        let ws_task = tokio::spawn(async move {
            // One connection at a time; a new one replaces the old.
            loop {
                let (stream, _) = ws.accept().await.unwrap();
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let hello = tungstenite::Message::Text(hello.to_string().into());
                if socket.send(hello).await.is_err() {
                    continue;
                }
                loop {
                    tokio::select! {
                        _ = kicked.recv() => {
                            let _ = socket.close(None).await;
                            break;
                        }
                        text = outgoing.recv() => {
                            let Some(text) = text else { return };
                            if socket.send(tungstenite::Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        message = socket.next() => match message {
                            Some(Ok(tungstenite::Message::Text(text))) => {
                                let _ = incoming.send(text.to_string());
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
            files,
            to_client,
            from_client,
            kick,
            connections,
            tasks: vec![http_task, ws_task],
        }
    }

    pub(super) fn script(&self, prefix: &'static str, status: u16, body: Value) {
        self.script_with(prefix, status, "", body);
    }

    /// Script an answer with extra header lines, each ending in `\r\n`.
    pub(super) fn script_with(
        &self,
        prefix: &'static str,
        status: u16,
        headers: &'static str,
        body: Value,
    ) {
        self.script
            .lock()
            .unwrap()
            .push_back((prefix, status, headers, body.to_string()));
    }

    /// Serve a file at `route` with its content type.
    pub(super) fn file(&self, route: &str, content_type: &str, bytes: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(route.into(), (content_type.into(), bytes.to_vec()));
    }

    pub(super) fn api(&self) -> Api {
        Api::local(&self.origin)
    }

    pub(super) fn transport(&self) -> SocketMode {
        SocketMode {
            api: self.api(),
            credentials: account(),
            connection: Mutex::default(),
            window: Duration::from_millis(300),
        }
    }

    /// The next request to a route with this prefix, skipping others.
    pub(super) async fn request(&mut self, prefix: &str) -> Request {
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

    /// Requests to a route with this prefix already made, without waiting.
    pub(super) fn made(&mut self, prefix: &str) -> usize {
        let mut count = 0;
        while let Ok(request) = self.requests.try_recv() {
            count += usize::from(request.route.starts_with(prefix));
        }
        count
    }

    /// Send a Socket Mode envelope.
    fn send(&self, frame: Value) {
        self.to_client.send(frame.to_string()).unwrap();
    }

    fn send_event(&self, envelope_id: &str, payload: Value) {
        self.send(json!({"type": "events_api", "envelope_id": envelope_id, "payload": payload}));
    }

    /// The next text frame the client sends, such as an acknowledgement.
    async fn frame(&mut self) -> Value {
        let text = tokio::time::timeout(WAIT, self.from_client.recv())
            .await
            .expect("a frame from the client")
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// Whether the client sent nothing for a moment.
    async fn quiet(&mut self) -> bool {
        tokio::time::timeout(Duration::from_millis(100), self.from_client.recv())
            .await
            .is_err()
    }

    fn kick(&self) {
        self.kick.send(()).unwrap();
    }
}

fn default_response(request: &Request, socket_url: &str, origin: &str) -> (u16, String, String) {
    let route = request.route.as_str();
    let mut headers = String::new();
    let body = if route.starts_with("/auth.test") {
        headers = format!("X-OAuth-Scopes: {}\r\n", api::BOT_SCOPES.join(","));
        json!({"ok": true, "team_id": "T123", "user_id": "UBOT", "bot_id": "B123"})
    } else if route.starts_with("/bots.info") {
        json!({"ok": true, "bot": {"app_id": "A123", "user_id": "UBOT"}})
    } else if route.starts_with("/apps.connections.open") {
        json!({"ok": true, "url": socket_url})
    } else if route.starts_with("/conversations.history")
        || route.starts_with("/conversations.replies")
    {
        json!({"ok": true, "messages": [], "has_more": false})
    } else if route.starts_with("/conversations.open") {
        json!({"ok": true, "channel": {"id": "D999"}})
    } else if route.starts_with("/chat.postMessage") {
        json!({"ok": true, "ts": "1700000099.000100"})
    } else if route.starts_with("/files.getUploadURLExternal") {
        json!({"ok": true, "upload_url": format!("{origin}/upload/F1"), "file_id": "F1"})
    } else if route.starts_with("/files.completeUploadExternal") {
        json!({"ok": true, "files": [{"id": "F1"}]})
    } else if route.starts_with("/upload/") {
        return (200, headers, "OK - 4".into());
    } else {
        return (404, headers, "{}".into());
    };
    (200, headers, body.to_string())
}

/// Form-encoded pairs as a JSON object.
fn pairs(encoded: &str) -> Value {
    let url = reqwest::Url::parse(&format!("http://fake/?{encoded}")).unwrap();
    Value::Object(
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
            .collect(),
    )
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
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    let body = if form {
        pairs(&String::from_utf8(body).unwrap())
    } else if !body.is_empty() {
        Value::String(String::from_utf8_lossy(&body).into_owned())
    } else {
        route
            .split_once('?')
            .map_or(Value::Null, |(_, query)| pairs(query))
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

pub(super) fn account() -> Account {
    Account {
        bot_token: "xoxb-test".into(),
        app_token: "xapp-test".into(),
        team_id: "T123".into(),
        app_id: "A123".into(),
        bot_user_id: "UBOT".into(),
        owner_user_id: Some("U123".into()),
    }
}

/// A Socket Mode `events_api` payload for a direct message.
pub(super) fn event(channel: &str) -> Value {
    json!({"type": "event_callback", "team_id": "T123", "api_app_id": "A123",
        "event": {"type": "message", "user": "U123", "channel": channel,
        "channel_type": "im", "ts": "1700000000.000001", "text": "hello"}})
}

/// A history item from the owner, sent at `ts`.
fn item(text: &str, ts: &str) -> Value {
    json!({"type": "message", "user": "U123", "text": text, "ts": ts})
}

/// A Slack timestamp `ago` seconds before now.
fn ts_ago(ago: u64) -> String {
    inbound::ts(unix_micros() - ago * 1_000_000)
}

/// A receive's error, which it must have.
fn failed(result: Result<Batch>) -> String {
    match result {
        Ok(_) => panic!("the receive succeeded"),
        Err(error) => error.to_string(),
    }
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

/// A checkpoint that knows `channel`, last seen `ago` seconds before now.
fn known(channel: &str, group: bool, ago: u64) -> (Checkpoint, u64) {
    let last = unix_micros() - ago * 1_000_000;
    let mut checkpoint = Checkpoint::default();
    checkpoint.listed_chat(channel, Mark { group, last });
    (checkpoint, last)
}

#[tokio::test]
async fn catch_up_then_socket_events_acknowledged_only_after_the_next_receive() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let (checkpoint, last) = known("D123", false, 60);
    let (away, answer) = (ts_ago(30), ts_ago(20));
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": true, "messages": [
            {"type": "message", "bot_id": "B123", "user": "UBOT", "text": "earlier answer", "ts": answer},
            item("status?", &away),
        ]}),
    );
    let batch = transport.receive(&checkpoint.to_json()).await.unwrap();
    assert_eq!(texts(&batch), [(format!("D123:{away}"), "status?".into())]);
    let history = fake.request("/conversations.history").await;
    assert_eq!(history.method, "GET");
    assert_eq!(history.body["channel"], "D123");
    assert_eq!(history.body["oldest"], inbound::ts(last));
    assert_eq!(history.authorization.as_deref(), Some("Bearer xoxb-test"));
    // The mark moves past everything listed, the bot's own answer included.
    let caught_up = Checkpoint::parse(batch.checkpoint.as_deref().unwrap());
    assert_eq!(
        caught_up.chats["D123"].last,
        inbound::micros(&answer).unwrap()
    );

    fake.send_event("e1", event("D123"));
    let cursor = batch.checkpoint.unwrap();
    let batch = transport.receive(&cursor).await.unwrap();
    assert_eq!(
        texts(&batch),
        [("D123:1700000000.000001".into(), "hello".into())]
    );
    // Not acknowledged until the bridge asks again.
    assert!(fake.quiet().await);
    let empty = transport.receive(&cursor).await.unwrap();
    assert!(empty.messages.is_empty());
    assert_eq!(fake.frame().await, json!({"envelope_id": "e1"}));
    assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_dropped_socket_reconnects_and_catches_up_again() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let (checkpoint, _) = known("D123", false, 60);
    let cursor = checkpoint.to_json();
    assert!(
        transport
            .receive(&cursor)
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    fake.request("/conversations.history").await;
    fake.kick();
    let error = failed(transport.receive(&cursor).await);
    assert!(error.contains("reconnecting"), "{error}");
    let ts = ts_ago(5);
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": true, "messages": [item("while away", &ts)]}),
    );
    let batch = transport.receive(&cursor).await.unwrap();
    assert_eq!(texts(&batch), [(format!("D123:{ts}"), "while away".into())]);
    assert_eq!(fake.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_requests_and_disabled_socket_mode_drop_the_connection() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    transport.receive("").await.unwrap();
    fake.send(json!({"type": "disconnect", "reason": "refresh_requested"}));
    let error = failed(transport.receive("").await);
    assert!(error.contains("fresh URL"), "{error}");
    transport.receive("").await.unwrap();
    assert_eq!(fake.connections.load(Ordering::SeqCst), 2);
    fake.send(json!({"type": "disconnect", "reason": "link_disabled"}));
    let error = failed(transport.receive("").await);
    assert!(error.contains("Enable Socket Mode"), "{error}");
    assert!(transport.connection.lock().await.socket.is_none());
    assert_eq!(fake.made("/apps.connections.open"), 2);
}

#[tokio::test]
async fn socket_mode_disabled_before_hello_is_reported_as_such() {
    let fake = Fake::with_hello(json!({"type": "disconnect", "reason": "link_disabled"})).await;
    let error = failed(fake.transport().receive("").await);
    assert!(error.contains("Socket Mode disabled"), "{error}");
}

#[tokio::test]
async fn unsupported_envelopes_are_acknowledged_at_once() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    transport.receive("").await.unwrap();
    // An interactive payload shaped like a message never reaches a session.
    fake.send(json!({"type": "interactive", "envelope_id": "e2", "payload": event("D123")}));
    fake.send_event(
        "e3",
        json!({"type": "event_callback", "team_id": "TOTHER", "api_app_id": "A123", "event": {}}),
    );
    assert!(transport.receive("").await.unwrap().messages.is_empty());
    assert_eq!(fake.frame().await, json!({"envelope_id": "e2"}));
    assert_eq!(fake.frame().await, json!({"envelope_id": "e3"}));
}

#[tokio::test]
async fn malformed_frames_and_cancellation_discard_the_socket() {
    for frame in [json!("{bad"), json!({"type": "events_api"})] {
        let fake = Fake::start().await;
        let transport = fake.transport();
        transport.receive("").await.unwrap();
        match frame {
            Value::String(text) => fake.to_client.send(text).unwrap(),
            frame => fake.send(frame),
        }
        assert!(transport.receive("").await.is_err());
        assert!(transport.connection.lock().await.socket.is_none());
    }
    let fake = Fake::start().await;
    let mut transport = fake.transport();
    transport.receive("").await.unwrap();
    transport.window = Duration::from_secs(25);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), transport.receive(""))
            .await
            .is_err()
    );
    assert!(transport.connection.lock().await.socket.is_none());
}

#[tokio::test]
async fn group_catch_up_hands_over_mentions_and_the_threads_they_open() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let (checkpoint, last) = known("C123", true, 120);
    let (chatter, mention, root, reply) = (ts_ago(100), ts_ago(90), ts_ago(80), ts_ago(10));
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": true, "messages": [
            {"type": "message", "user": "U456", "text": "lunch?", "ts": root,
             "thread_ts": root, "reply_count": 1, "latest_reply": reply},
            item("<@UBOT> deploy status", &mention),
            item("not for the bot", &chatter),
        ]}),
    );
    fake.script(
        "/conversations.replies",
        200,
        json!({"ok": true, "messages": [
            {"type": "message", "user": "U456", "text": "lunch?", "ts": root, "thread_ts": root},
            {"type": "message", "user": "U123", "text": "<@UBOT> book it", "ts": reply, "thread_ts": root},
        ]}),
    );
    let batch = transport.receive(&checkpoint.to_json()).await.unwrap();
    let Inbound::Text(first) = &batch.messages[0] else {
        panic!()
    };
    assert_eq!(first.text, "deploy status");
    assert_eq!(first.group.as_deref(), Some("C123"));
    assert!(first.thread.is_none());
    let Inbound::Text(second) = &batch.messages[1] else {
        panic!()
    };
    assert_eq!(second.text, "book it");
    assert_eq!(second.reply_to, format!("slack:C123/thread:{root}"));
    assert_eq!(second.thread.as_ref().unwrap().id, format!("C123:{root}"));
    assert_eq!(batch.messages.len(), 2);
    let replies = fake.request("/conversations.replies").await;
    assert_eq!(replies.body["ts"], root);
    assert_eq!(replies.body["oldest"], inbound::ts(last));
    let saved = Checkpoint::parse(batch.checkpoint.as_deref().unwrap());
    assert_eq!(saved.chats["C123"].last, inbound::micros(&root).unwrap());
    assert_eq!(
        saved.threads[&format!("C123:{root}")].last,
        inbound::micros(&reply).unwrap()
    );
}

#[tokio::test]
async fn refused_listings_are_skipped_and_a_rate_limit_retries_on_the_same_socket() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    let (mut checkpoint, _) = known("C123", true, 60);
    checkpoint.listed_chat(
        "D123",
        Mark {
            group: false,
            last: unix_micros() - 30_000_000,
        },
    );
    // The bot left one channel; the other conversation still catches up.
    fake.script(
        "/conversations.history?channel=C123",
        200,
        json!({"ok": false, "error": "not_in_channel"}),
    );
    let ts = ts_ago(10);
    fake.script(
        "/conversations.history?channel=D123",
        200,
        json!({"ok": true, "messages": [item("still here", &ts)]}),
    );
    let batch = transport.receive(&checkpoint.to_json()).await.unwrap();
    assert_eq!(texts(&batch).len(), 1);

    let fake = Fake::start().await;
    let transport = fake.transport();
    fake.script_with(
        "/conversations.history",
        429,
        "Retry-After: 1\r\n",
        json!({"ok": false, "error": "ratelimited"}),
    );
    let cursor = checkpoint.to_json();
    let error = failed(transport.receive(&cursor).await);
    assert!(error.contains("rate limited"), "{error}");
    // Within Retry-After the catch-up fails without asking Slack.
    let error = failed(transport.receive(&cursor).await);
    assert!(error.contains("cooldown"), "{error}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    transport.receive(&cursor).await.unwrap();
    assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replies_go_into_their_thread_without_markup_and_refusals_are_final() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let part = Outbound {
        to: "U123",
        reply_to: "slack:C123/thread:1700000000.000001",
        part: 0,
        text: "<!channel> <@U456> a & b",
        client_id: "stable-id",
    };
    assert_eq!(
        transport.send(&part, &|_| {}).await.unwrap(),
        SendOutcome::Delivered
    );
    let posted = fake.request("/chat.postMessage").await;
    assert_eq!(posted.authorization.as_deref(), Some("Bearer xoxb-test"));
    assert_eq!(posted.body["channel"], "C123");
    assert_eq!(posted.body["thread_ts"], "1700000000.000001");
    assert_eq!(
        posted.body["text"],
        "&lt;!channel&gt; &lt;@U456&gt; a &amp; b"
    );
    assert_eq!(posted.body["parse"], "none");
    assert_eq!(posted.body["link_names"], "false");
    assert_eq!(posted.body["unfurl_links"], "false");

    // A report that answers nothing goes to the user's direct conversation.
    let report = Outbound {
        reply_to: "",
        ..part
    };
    transport.send(&report, &|_| {}).await.unwrap();
    let posted = fake.request("/chat.postMessage").await;
    assert_eq!(posted.body["channel"], "U123");
    assert!(posted.body.get("thread_ts").is_none());

    fake.script(
        "/chat.postMessage",
        200,
        json!({"ok": false, "error": "not_in_channel"}),
    );
    assert_eq!(
        transport.send(&part, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
    let bad = Outbound {
        reply_to: "feishu:om_1",
        ..part
    };
    assert_eq!(
        transport.send(&bad, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
}

#[tokio::test]
async fn transient_send_failures_retry_then_give_up() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    for _ in 0..3 {
        fake.script(
            "/chat.postMessage",
            200,
            json!({"ok": false, "error": "internal_error"}),
        );
    }
    let reports = StdMutex::new(Vec::new());
    let part = Outbound {
        to: "U123",
        reply_to: "slack:D123",
        part: 0,
        text: "hi",
        client_id: "c",
    };
    let error = transport
        .send(&part, &|healthy| reports.lock().unwrap().push(healthy))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("could not deliver"));
    assert_eq!(*reports.lock().unwrap(), [false, false, false]);
}

fn media(url: &str, mime: &str) -> Media {
    Media {
        kind: MediaKind::Image,
        name: "a.png".into(),
        size: None,
        mime: Some(mime.into()),
        transcript: None,
        source: json!({"url": url}).to_string(),
    }
}

#[tokio::test]
async fn files_download_with_the_bot_token_within_limits_from_slack_only() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    fake.file("/files/a.png", "image/png", b"\x89PNGbytes");
    let image = media(&format!("{}/files/a.png", fake.origin), "image/png");
    let downloaded = transport.download(&image, 1024).await.unwrap();
    assert_eq!(downloaded.bytes, b"\x89PNGbytes");
    assert_eq!(downloaded.mime.as_deref(), Some("image/png"));
    let request = fake.request("/files/a.png").await;
    assert_eq!(request.authorization.as_deref(), Some("Bearer xoxb-test"));
    assert!(transport.download(&image, 4).await.is_err());

    // Without files:read Slack serves its sign-in page.
    fake.file("/files/b.pdf", "text/html; charset=utf-8", b"<html>");
    let pdf = media(&format!("{}/files/b.pdf", fake.origin), "application/pdf");
    let error = transport
        .download(&pdf, 1024)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("files:read"), "{error}");
    let page = media(&format!("{}/files/b.pdf", fake.origin), "text/html");
    assert!(transport.download(&page, 1024).await.is_ok());
    fake.script_with(
        "/files/c.png",
        302,
        "Location: https://example.slack.com/?redir=%2Ffiles-pri\r\n",
        json!({}),
    );
    let redirected = media(&format!("{}/files/c.png", fake.origin), "image/png");
    let error = transport
        .download(&redirected, 1024)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("files:read"), "{error}");

    // The token never goes anywhere else.
    fake.made("/");
    let elsewhere = media("http://localhost:1/files/a.png", "image/png");
    let error = transport
        .download(&elsewhere, 1024)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("untrusted"), "{error}");
    assert_eq!(fake.made("/"), 0);
}

#[tokio::test]
async fn files_upload_then_share_in_the_thread_or_the_users_direct_conversation() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    std::fs::write(&path, b"\x89PNG").unwrap();
    let file = OutboundFile {
        to: "U123",
        reply_to: "slack:C123/thread:1700000000.000001",
        part: 1,
        path: &path,
        name: "chart.png",
        kind: MediaKind::Image,
        client_id: "file-1",
    };
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Delivered
    );
    let asked = fake.request("/files.getUploadURLExternal").await;
    assert_eq!(asked.body["filename"], "chart.png");
    assert_eq!(asked.body["length"], "4");
    let upload = fake.request("/upload/F1").await;
    assert_eq!(upload.method, "POST");
    assert_eq!(upload.authorization, None);
    assert_eq!(upload.body, "\u{fffd}PNG");
    let shared = fake.request("/files.completeUploadExternal").await;
    assert_eq!(shared.body["channel_id"], "C123");
    assert_eq!(shared.body["thread_ts"], "1700000000.000001");
    let files: Value = serde_json::from_str(shared.body["files"].as_str().unwrap()).unwrap();
    assert_eq!(files, json!([{"id": "F1", "title": "chart.png"}]));

    let direct = OutboundFile {
        reply_to: "",
        ..file
    };
    assert_eq!(
        transport.send_file(&direct, &|_| {}).await.unwrap(),
        SendOutcome::Delivered
    );
    let opened = fake.request("/conversations.open").await;
    assert_eq!(opened.body["users"], "U123");
    let shared = fake.request("/files.completeUploadExternal").await;
    assert_eq!(shared.body["channel_id"], "D999");
    assert!(shared.body.get("thread_ts").is_none());

    // A refusal is final; an upload URL off Slack is never used.
    fake.script(
        "/files.getUploadURLExternal",
        200,
        json!({"ok": false, "error": "missing_scope"}),
    );
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
    fake.script(
        "/files.getUploadURLExternal",
        200,
        json!({"ok": true, "upload_url": "http://localhost:1/upload", "file_id": "F2"}),
    );
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
}

#[tokio::test]
async fn a_threads_root_and_shared_messages_resolve_into_context_and_files() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let file = format!("{}/files/plan.pdf", fake.origin);
    fake.script(
        "/conversations.replies",
        200,
        json!({"ok": true, "messages": [
            {"type": "message", "user": "U456", "ts": "1700000000.000001",
             "text": "<@UBOT> review this &amp; ship",
             "files": [{"name": "plan.pdf", "mimetype": "application/pdf", "url_private_download": file}]},
            {"type": "message", "user": "U123", "ts": "1700000000.000002", "text": "later"},
        ]}),
    );
    let reference = r#"{"channel":"C123","root":"1700000000.000001","quote":"Ada: ship it?"}"#;
    let resolved = transport.resolve("C123:1", reference).await.unwrap();
    assert_eq!(
        resolved.context,
        "[Thread on: review this & ship]\n\n[Quoting: Ada: ship it?]"
    );
    assert_eq!(resolved.media.len(), 1);
    assert_eq!(resolved.media[0].name, "plan.pdf");
    let asked = fake.request("/conversations.replies").await;
    assert_eq!(asked.body["ts"], "1700000000.000001");
    assert_eq!(asked.body["inclusive"], "true");
    assert!(transport.resolve("C123:1", "{bad").await.is_err());
}

#[tokio::test]
async fn full_bridge_answers_a_caught_up_thread_message_inside_its_thread() {
    let mut fake = Fake::start().await;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_client::Layout::new(directory.path()), CHANNEL);
    store.save_account("default", &account()).unwrap();
    let (mut checkpoint, _) = known("D123", false, 120);
    let root = ts_ago(110);
    let reply = ts_ago(30);
    checkpoint.listed_thread(
        &format!("D123:{root}"),
        Mark {
            group: false,
            last: inbound::micros(&root).unwrap(),
        },
    );
    let mut saved = store.load_state("default").unwrap();
    saved.cursor = checkpoint.to_json();
    store.save_state("default", &saved).unwrap();
    fake.script(
        "/conversations.replies",
        200,
        json!({"ok": true, "messages": [
            {"type": "message", "user": "U123", "text": "status?", "ts": reply, "thread_ts": root},
        ]}),
    );
    let transport = fake.transport();
    // No daemon listens here, so the turn fails and the bridge answers with
    // its failure reply, inside the thread.
    let socket = directory.path().join("missing.sock");
    let reports = StdMutex::new(Vec::new());
    let report = |healthy| reports.lock().unwrap().push(healthy);
    let layout = scv_client::Layout::new(directory.path());
    let media = crate::MediaOptions::new(
        &layout,
        CHANNEL,
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
            purpose: crate::state::Purpose::Chat,
            media,
            log: crate::chatlog::LogOptions::test(directory.path(), CHANNEL),
            link: &detached,
            report: &report,
        },
        &store,
        |credentials| Ok(credentials == &account()),
    );
    let peer = async {
        let posted = fake.request("/chat.postMessage").await;
        assert_eq!(posted.body["channel"], "D123");
        assert_eq!(posted.body["thread_ts"], root);
        assert_eq!(posted.body["text"], crate::FAILURE_REPLY);
        tokio::time::timeout(WAIT, async {
            loop {
                let state = store.load_state("default").unwrap();
                let id = format!("D123:{reply}");
                if state.pending.is_empty() && state.seen.contains(&id) {
                    let saved = Checkpoint::parse(&state.cursor);
                    let key = format!("D123:{root}");
                    assert_eq!(saved.threads[&key].last, inbound::micros(&reply).unwrap());
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
async fn a_slow_catch_up_keeps_the_socket_and_reads_events_after_it() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    let (checkpoint, _) = known("D123", false, 60);
    let cursor = checkpoint.to_json();
    fake.script_with(
        "/conversations.history",
        429,
        "Retry-After: 1\r\n",
        json!({"ok": false, "error": "ratelimited"}),
    );
    assert!(failed(transport.receive(&cursor).await).contains("rate limited"));
    // The bridge's backoff passes while nothing reads the socket.
    transport
        .connection
        .lock()
        .await
        .socket
        .as_mut()
        .unwrap()
        .silent_for(Duration::from_secs(60));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    transport.receive(&cursor).await.unwrap();
    fake.send_event("e1", event("D123"));
    assert_eq!(transport.receive(&cursor).await.unwrap().messages.len(), 1);
    assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_catch_up_stopped_part_way_hands_over_its_finds_and_resumes_after_them() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let (mut checkpoint, _) = known("C123", true, 120);
    checkpoint.listed_chat(
        "D123",
        Mark {
            group: false,
            last: unix_micros() - 60_000_000,
        },
    );
    let ts = ts_ago(30);
    fake.script(
        "/conversations.history?channel=D123",
        200,
        json!({"ok": true, "messages": [item("first", &ts)]}),
    );
    fake.script_with(
        "/conversations.history?channel=C123",
        429,
        "Retry-After: 1\r\n",
        json!({"ok": false, "error": "ratelimited"}),
    );
    let batch = transport.receive(&checkpoint.to_json()).await.unwrap();
    assert_eq!(texts(&batch), [(format!("D123:{ts}"), "first".into())]);
    let cursor = batch.checkpoint.unwrap();
    // Within Retry-After nothing is asked of Slack and nothing is new.
    assert!(failed(transport.receive(&cursor).await).contains("cooldown"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let batch = transport.receive(&cursor).await.unwrap();
    assert!(batch.messages.is_empty());
    assert_eq!(fake.made("/conversations.history?channel=D123"), 1);
    let saved = Checkpoint::parse(batch.checkpoint.as_deref().unwrap_or(&cursor));
    assert_eq!(saved.chats["D123"].last, inbound::micros(&ts).unwrap());
}

#[tokio::test]
async fn a_rejected_bot_token_fails_every_receive_and_replies_wait() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    transport.receive("").await.unwrap();
    for _ in 0..3 {
        fake.script(
            "/chat.postMessage",
            200,
            json!({"ok": false, "error": "token_revoked"}),
        );
    }
    let part = Outbound {
        to: "U123",
        reply_to: "slack:D123",
        part: 0,
        text: "hi",
        client_id: "c",
    };
    // Not a refusal of this reply: it waits for a new sign-in.
    assert!(transport.send(&part, &|_| {}).await.is_err());
    fake.script(
        "/auth.test",
        200,
        json!({"ok": false, "error": "token_revoked"}),
    );
    let error = failed(transport.receive("").await);
    assert!(error.contains("rejected the token"), "{error}");

    // Catch-up does not skip past it either.
    let fake = Fake::start().await;
    let transport = fake.transport();
    let (checkpoint, _) = known("D123", false, 60);
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": false, "error": "invalid_auth"}),
    );
    let error = failed(transport.receive(&checkpoint.to_json()).await);
    assert!(error.contains("rejected the token"), "{error}");
}

#[tokio::test]
async fn threads_the_listings_show_new_replies_in_go_first() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let (mut checkpoint, _) = known("C123", true, 600);
    for index in 0..CATCH_UP_THREADS {
        checkpoint.listed_thread(
            &format!("C123:1700000000.{index:06}"),
            Mark {
                group: true,
                last: unix_micros() - 1_000_000,
            },
        );
    }
    let (root, reply) = (ts_ago(500), ts_ago(5));
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": true, "messages": [
            {"type": "message", "user": "U456", "text": "lunch?", "ts": root,
             "thread_ts": root, "reply_count": 1, "latest_reply": reply},
        ]}),
    );
    transport.receive(&checkpoint.to_json()).await.unwrap();
    let mut roots = Vec::new();
    for _ in 0..CATCH_UP_THREADS {
        let request = fake.request("/conversations.replies").await;
        roots.push(request.body["ts"].as_str().unwrap().to_owned());
    }
    assert_eq!(roots[0], root);
    assert_eq!(fake.made("/conversations.replies"), 0);
}

#[tokio::test]
async fn history_pages_follow_the_cursor_up_to_the_page_limit() {
    let mut fake = Fake::start().await;
    let transport = fake.transport();
    let (checkpoint, _) = known("D123", false, 600);
    let (older, newer) = (ts_ago(300), ts_ago(200));
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": true, "messages": [item("newer", &newer)],
               "has_more": true, "response_metadata": {"next_cursor": "c2"}}),
    );
    fake.script(
        "/conversations.history",
        200,
        json!({"ok": true, "messages": [item("older", &older)],
               "has_more": true, "response_metadata": {"next_cursor": "c3"}}),
    );
    let batch = transport.receive(&checkpoint.to_json()).await.unwrap();
    let texts: Vec<_> = texts(&batch).into_iter().map(|(_, text)| text).collect();
    assert_eq!(texts, ["older", "newer"]);
    fake.request("/conversations.history").await;
    let second = fake.request("/conversations.history").await;
    assert_eq!(second.body["cursor"], "c2");
    assert_eq!(fake.made("/conversations.history"), 0);
}

#[tokio::test]
async fn an_upload_retries_server_failures_and_takes_a_refusal_as_final() {
    let fake = Fake::start().await;
    let transport = fake.transport();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    std::fs::write(&path, b"text").unwrap();
    let file = OutboundFile {
        to: "U123",
        reply_to: "slack:D123",
        part: 1,
        path: &path,
        name: "a.txt",
        kind: MediaKind::File,
        client_id: "file-1",
    };
    fake.script("/upload/", 503, json!({}));
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Delivered
    );
    fake.script("/upload/", 403, json!({}));
    assert_eq!(
        transport.send_file(&file, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
    fake.script(
        "/conversations.open",
        200,
        json!({"ok": false, "error": "missing_scope"}),
    );
    let direct = OutboundFile {
        reply_to: "",
        ..file
    };
    assert_eq!(
        transport.send_file(&direct, &|_| {}).await.unwrap(),
        SendOutcome::Rejected
    );
}
