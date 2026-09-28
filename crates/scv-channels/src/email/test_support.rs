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
    Address, Caps, Changes, Cursor, MailSource, Meta, PartRef, PartText, ProviderKind, Signals,
    SourceRef, TransferEncoding,
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
}

impl FakeMailbox {
    pub(crate) fn add(&self, meta: Meta, body: &str) {
        self.messages.lock().unwrap().push((meta, body.to_owned()));
    }

    fn uid(source: &SourceRef) -> u32 {
        let SourceRef::Imap { uid, .. } = source;
        *uid
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
