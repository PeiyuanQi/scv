//! Fakes the email module's tests share: a clock that stands still, a
//! scripted mailbox, a daemon that answers triage turns, and a mail chat on
//! a real hub.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use super::Clock;
use super::credentials::Account;
use super::ledger::{Ledger, Store};
use super::source::{
    Address, Caps, Changes, Cursor, FolderNames, Folders, MailSource, Meta, PartRef, PartText,
    ProviderKind, Signals, SourceRef, TransferEncoding,
};
use crate::hub;
use crate::state::Purpose;

/// 2024-10-04 09:00 UTC, a Friday.
pub(crate) const START: u64 = 20_000 * 86_400 + 9 * 3600;

/// A clock tests move by hand, at UTC+8.
pub(crate) struct FixedClock(pub(crate) AtomicU64);

impl FixedClock {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(START))
    }

    pub(crate) fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::Relaxed);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn offset(&self, _now: u64) -> i32 {
        8 * 3600
    }
}

pub(crate) fn account() -> Account {
    Account::Imap {
        host: "imap.example.com".into(),
        port: 993,
        username: "me@example.com".into(),
        password: "authorization-code".into(),
        address: None,
        smtp: None,
    }
}

/// A signed-in account's ledger under `home`.
pub(crate) fn ledger(home: &std::path::Path) -> Ledger {
    let store: Store = super::store(&scv_client::Layout::new(home));
    store.save_account("default", &account()).unwrap();
    let state = store.bind_state("default", |_| Ok(true)).unwrap();
    let lock = store.lock("default").unwrap();
    Ledger::open(store, lock, "default", state, 2048 * 1024, 256).unwrap()
}

/// A message's metadata as the fake mailbox reports it.
pub(crate) fn meta(uid: u32, from: &str, subject: &str) -> Meta {
    Meta {
        source: SourceRef::Imap {
            mailbox: "INBOX".into(),
            uidvalidity: 7,
            uid,
        },
        identity: format!("identity-{uid}"),
        received_at: START,
        size: 1000,
        from: Some(Address {
            name: String::new(),
            address: from.into(),
        }),
        reply_to: None,
        to: vec![Address {
            name: String::new(),
            address: "me@example.com".into(),
        }],
        cc: Vec::new(),
        subject: subject.into(),
        message_id: None,
        locator: "locator".into(),
        references: Vec::new(),
        date: None,
        signals: Signals::default(),
        category: None,
        text: Some(PartRef {
            id: "1".into(),
            mime: "text/plain".into(),
            charset: Some("utf-8".into()),
            encoding: TransferEncoding::SevenBit,
            size: 100,
        }),
        attachments: Vec::new(),
    }
}

/// A mailbox the test fills: messages with their bodies, read-only.
#[derive(Clone, Default)]
pub(crate) struct FakeMailbox {
    pub(crate) messages: Arc<Mutex<Vec<(Meta, String)>>>,
    /// Bodies fetched so far.
    pub(crate) fetched: Arc<AtomicU64>,
    /// Every call fails while set, as a lost connection does.
    pub(crate) broken: Arc<AtomicBool>,
    /// The next `changes` reports a reset.
    pub(crate) reset: Arc<AtomicBool>,
    /// The special folders it reports.
    pub(crate) folders: Arc<Mutex<Folders>>,
}

impl FakeMailbox {
    pub(crate) fn add(&self, meta: Meta, body: &str) {
        self.messages.lock().unwrap().push((meta, body.to_owned()));
    }

    fn uid(source: &SourceRef) -> u32 {
        match source {
            SourceRef::Imap { uid, .. } => *uid,
            SourceRef::Gmail { .. } | SourceRef::Graph { .. } => 0,
        }
    }
}

fn position(next: u32) -> Cursor {
    Cursor {
        provider: ProviderKind::Imap,
        value: next.to_string(),
    }
}

#[async_trait]
impl MailSource for FakeMailbox {
    async fn changes(&mut self, cursor: Option<&Cursor>, limit: usize, _: u64) -> Result<Changes> {
        if self.broken.load(Ordering::Relaxed) {
            bail!("connection lost");
        }
        let messages = self.messages.lock().unwrap();
        let top = messages
            .iter()
            .map(|(meta, _)| Self::uid(&meta.source) + 1)
            .max()
            .unwrap_or(1);
        if self.reset.swap(false, Ordering::Relaxed) {
            return Ok(Changes::Reset {
                recent: messages
                    .iter()
                    .map(|(meta, _)| meta.source.clone())
                    .collect(),
                beyond: 3,
                next: position(top),
            });
        }
        let Some(cursor) = cursor else {
            return Ok(Changes::New {
                refs: Vec::new(),
                next: position(top),
            });
        };
        let from: u32 = cursor.value.parse().unwrap();
        let refs: Vec<SourceRef> = messages
            .iter()
            .map(|(meta, _)| meta.source.clone())
            .filter(|source| Self::uid(source) >= from)
            .take(limit)
            .collect();
        let next = refs
            .last()
            .map_or(from.max(top), |last| Self::uid(last) + 1);
        Ok(Changes::New {
            refs,
            next: position(next),
        })
    }

    async fn metadata(&mut self, refs: &[SourceRef]) -> Result<Vec<Meta>> {
        if self.broken.load(Ordering::Relaxed) {
            bail!("connection lost");
        }
        let messages = self.messages.lock().unwrap();
        Ok(refs
            .iter()
            .filter_map(|source| {
                messages
                    .iter()
                    .find(|(meta, _)| &meta.source == source)
                    .map(|(meta, _)| meta.clone())
            })
            .collect())
    }

    async fn text(
        &mut self,
        source: &SourceRef,
        _part: &PartRef,
        max_bytes: usize,
    ) -> Result<Option<PartText>> {
        if self.broken.load(Ordering::Relaxed) {
            bail!("connection lost");
        }
        self.fetched.fetch_add(1, Ordering::Relaxed);
        let messages = self.messages.lock().unwrap();
        Ok(messages
            .iter()
            .find(|(meta, _)| &meta.source == source)
            .map(|(_, body)| PartText {
                text: scv_client::text::utf8_prefix(body, max_bytes).to_owned(),
                html: false,
                truncated: body.len() > max_bytes,
            }))
    }

    fn caps(&self) -> Caps {
        Caps::default()
    }

    async fn folders(&mut self, _names: &FolderNames) -> Result<Folders> {
        Ok(self.folders.lock().unwrap().clone())
    }
}

/// What the fake daemon saw of one triage session.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) start: Value,
    pub(crate) prompt: String,
}

/// A daemon on `listener` that answers every triage turn with
/// `answer(prompt)` and records each session.
pub(crate) fn daemon(
    listener: UnixListener,
    answer: impl Fn(&str) -> String + Send + Sync + 'static,
) -> (Arc<Mutex<Vec<Seen>>>, tokio::task::JoinHandle<()>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut side = tokio::io::BufReader::new(stream);
            let mut hello = String::new();
            if side.read_line(&mut hello).await.is_err() {
                continue;
            }
            let Some(start) = read_after(&mut side, json!({"type":"initialized","request_id":"mail-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"t","version":"0"}})).await else { continue };
            let Some(turn) = read_after(&mut side, json!({"type":"session.started","request_id":"mail-session","session_id":"s","cwd":"/","model":"m","context_max_tokens":1,"max_server_frame_bytes":1,"max_transcript_bytes":1,"max_transcript_items":1,"max_prompt_history_bytes":1,"max_prompt_history_items":1})).await else { continue };
            let prompt = turn["prompt"].as_str().unwrap_or_default().to_owned();
            let content = answer(&prompt);
            record.lock().unwrap().push(Seen { start, prompt });
            for event in [
                json!({"type":"assistant.completed","request_id":"mail-turn","session_id":"s","turn_id":"t","seq":1,"content":content}),
                json!({"type":"turn.completed","request_id":"mail-turn","session_id":"s","turn_id":"t","seq":2,"steps":1,"usage":{"input_tokens":600,"output_tokens":50}}),
            ] {
                let _ = side
                    .get_mut()
                    .write_all(format!("{event}\n").as_bytes())
                    .await;
            }
        }
    });
    (seen, task)
}

/// Write `event`, then read the client's next frame.
async fn read_after(
    side: &mut tokio::io::BufReader<tokio::net::UnixStream>,
    event: Value,
) -> Option<Value> {
    side.get_mut()
        .write_all(format!("{event}\n").as_bytes())
        .await
        .ok()?;
    let mut line = String::new();
    side.read_line(&mut line).await.ok()?;
    serde_json::from_str(&line).ok()
}

/// The texts a fake mail chat stored, by key.
pub(crate) type Stored = Arc<Mutex<Vec<(String, String)>>>;

/// A mail chat on `hub` as `component`, owned by `owner`, that stores every
/// notice at once, delivers it, and records its text.
pub(crate) fn mail_chat(
    hub: &Arc<hub::Hub>,
    component: &str,
    purpose: Purpose,
) -> (Stored, tokio::task::JoinHandle<()>) {
    let link = hub::Link::new(Arc::clone(hub), component, Some("owner".into()));
    let (registration, mut notices) = link.register_as(purpose);
    let texts = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&texts);
    let task = tokio::spawn(async move {
        let registration = registration;
        while let Some(notice) = notices.recv().await {
            let key = notice.key.clone().unwrap_or_default();
            record
                .lock()
                .unwrap()
                .push((key.clone(), notice.text.clone()));
            registration.record_keyed(&key, hub::KeyedOutcome::Delivered { at_ms: 1 });
            notice.stored();
        }
    });
    (texts, task)
}

/// One request the fake HTTP server received.
#[derive(Debug, Clone)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    /// The path, without the query.
    pub(crate) path: String,
    /// The query's pairs, decoded.
    pub(crate) query: Vec<(String, String)>,
    /// Header names lowercased.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl HttpRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON; `null` when it is not.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// A field of a form-encoded body, decoded.
    pub(crate) fn form(&self, name: &str) -> Option<String> {
        let body = String::from_utf8_lossy(&self.body);
        reqwest::Url::parse(&format!("http://form.invalid/?{body}"))
            .ok()?
            .query_pairs()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.into_owned())
    }
}

/// A local HTTP server answering each request with `answer(request)`: a
/// status and a body. Status 0 closes the connection without an answer,
/// as a request lost on the way would; a 3xx points elsewhere with a
/// `Location` header. Every request is recorded.
pub(crate) struct FakeHttp {
    /// `http://127.0.0.1:<port>`.
    pub(crate) origin: String,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeHttp {
    pub(crate) async fn start(
        answer: impl Fn(&HttpRequest) -> (u16, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&requests);
        let answer = Arc::new(answer);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (record, answer) = (Arc::clone(&record), Arc::clone(&answer));
                tokio::spawn(async move {
                    let mut stream = tokio::io::BufReader::new(stream);
                    let Some(request) = read_http(&mut stream).await else {
                        return;
                    };
                    let (status, body) = answer(&request);
                    record.lock().unwrap().push(request);
                    if status == 0 {
                        return;
                    }
                    let location = if (300..400).contains(&status) {
                        "Location: http://127.0.0.1:9/elsewhere\r\n"
                    } else {
                        ""
                    };
                    let head = format!(
                        "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\n\
                         {location}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let stream = stream.get_mut();
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            origin,
            requests,
            task,
        }
    }

    /// Every request received so far, in order.
    pub(crate) fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// One HTTP/1.1 request: its head, then a body of its `Content-Length`.
async fn read_http(
    stream: &mut tokio::io::BufReader<tokio::net::TcpStream>,
) -> Option<HttpRequest> {
    use tokio::io::AsyncReadExt as _;
    let mut line = String::new();
    stream.read_line(&mut line).await.ok()?;
    let mut words = line.split_whitespace();
    let method = words.next()?.to_owned();
    let target = words.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        stream.read_line(&mut header).await.ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.ok()?;
    let url = reqwest::Url::parse(&format!("http://fake.invalid{target}")).ok()?;
    Some(HttpRequest {
        method,
        path: url.path().to_owned(),
        query: url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect(),
        headers,
        body,
    })
}

/// The fake token endpoint's path.
pub(crate) const TOKEN_PATH: &str = "/oauth/token";

/// The fake token endpoint's answer to a refresh, when `request` is one:
/// grant `rt-<kind>` is exchanged for access token `at-<kind>`, good for an
/// hour.
pub(crate) fn token_answer(request: &HttpRequest) -> Option<(u16, String)> {
    (request.method == "POST" && request.path == TOKEN_PATH).then(|| {
        let refresh = request.form("refresh_token").unwrap_or_default();
        let access = refresh.replacen("rt-", "at-", 1);
        (
            200,
            json!({ "access_token": access, "expires_in": 3600, "token_type": "Bearer" })
                .to_string(),
        )
    })
}

/// A grants file under `directory` holding a reader, a writer, and a
/// sender grant (refresh tokens `rt-reader`, `rt-writer`, `rt-sender`),
/// and the token source of grant `kind` for `provider`, renewing at
/// `origin`'s [`TOKEN_PATH`].
pub(crate) fn tokens(
    directory: &std::path::Path,
    origin: &str,
    provider: super::oauth::OAuthProvider,
    kind: super::credentials::GrantKind,
) -> Arc<super::oauth::TokenSource> {
    use super::credentials::{Grant, GrantKind, Grants};
    let path = directory.join("test.grants");
    if !path.exists() {
        let mut grants = Grants::default();
        for each in [GrantKind::Reader, GrantKind::Writer, GrantKind::Sender] {
            grants.set(
                each,
                Grant {
                    refresh_token: format!("rt-{}", each.name()).into(),
                    scopes: Vec::new(),
                },
            );
        }
        grants.save(&path).unwrap();
    }
    let lock_path = directory.join("test.lock");
    Arc::new(
        super::oauth::TokenSource::new(
            provider,
            kind,
            super::oauth::Endpoints {
                authorize: format!("{origin}/oauth/authorize"),
                token: format!("{origin}{TOKEN_PATH}"),
            },
            "client".into(),
            None,
            path,
            Arc::new(move || {
                Ok(std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&lock_path)?)
            }),
        )
        .unwrap(),
    )
}
