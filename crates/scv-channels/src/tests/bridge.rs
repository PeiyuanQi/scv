//! Tests of `run` in `src/lib.rs` through an in-memory transport and a fake
//! SCV daemon, so the shared bridge is exercised without any platform's
//! HTTP API: claims, ordering, concurrency, busy replies, and redelivery.

use super::*;
use serde_json::{Value, json};
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// One text part the bridge sent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    to: String,
    reply_to: String,
    text: String,
}

/// A transport that receives the batches a test pushes and records every
/// part the bridge sends. Checkpoints count batches: `c1`, `c2`, ...
struct FakeTransport {
    batches: tokio::sync::Mutex<UnboundedReceiver<Vec<Inbound>>>,
    received: StdMutex<u32>,
    sent: UnboundedSender<Sent>,
}

#[async_trait]
impl Transport for FakeTransport {
    fn label(&self) -> &'static str {
        "Fake"
    }

    fn channel(&self) -> &'static str {
        "Fake"
    }

    async fn receive(&self, _checkpoint: &str) -> Result<Batch> {
        let Some(messages) = self.batches.lock().await.recv().await else {
            // The test pushes nothing more: wait until it cancels the run.
            return std::future::pending().await;
        };
        let mut received = self.received.lock().unwrap();
        *received += 1;
        Ok(Batch {
            messages,
            checkpoint: Some(format!("c{received}")),
        })
    }

    async fn send(
        &self,
        message: &Outbound<'_>,
        _report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome> {
        self.sent
            .send(Sent {
                to: message.to.into(),
                reply_to: message.reply_to.into(),
                text: message.text.into(),
            })
            .unwrap();
        Ok(SendOutcome::Delivered)
    }
}

/// One account's bridge under test: its store and the transport it runs.
struct Bench {
    directory: tempfile::TempDir,
    store: state::Store<Test>,
    socket: std::path::PathBuf,
    transport: FakeTransport,
}

/// The test's side: the fake daemon, and the platform's two directions.
struct Peer {
    daemon: UnixListener,
    push: UnboundedSender<Vec<Inbound>>,
    sent: UnboundedReceiver<Sent>,
}

impl Bench {
    fn new() -> (Self, Peer) {
        let directory = tempfile::tempdir().unwrap();
        let store = test_store(directory.path());
        store.save_account("default", &Test).unwrap();
        let socket = directory.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let (push, batches) = unbounded_channel();
        let (sender, sent) = unbounded_channel();
        let transport = FakeTransport {
            batches: tokio::sync::Mutex::new(batches),
            received: StdMutex::new(0),
            sent: sender,
        };
        let bench = Self {
            directory,
            store,
            socket,
            transport,
        };
        (bench, Peer { daemon, push, sent })
    }

    /// Run the bridge next to `peer`, which plays the platform and the
    /// daemon, until `peer` returns; fails the test after 15 seconds.
    async fn run<T>(&self, peer: impl std::future::Future<Output = T>) -> T {
        let media = MediaOptions::new(
            &self.directory.path().join("media"),
            "test",
            "default",
            MediaSettings::default(),
        );
        let bridge = run(
            &self.transport,
            "default",
            self.directory.path(),
            &self.socket,
            None,
            &media,
            &self.store,
            |_| Ok(true),
            &|_| {},
        );
        let outcome = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::select! {
                result = bridge => panic!("the bridge stopped early: {result:?}"),
                output = peer => output,
            }
        })
        .await;
        outcome.expect("the test finishes in time")
    }

    fn state(&self) -> state::BridgeState {
        self.store.load_state("default").unwrap()
    }
}

impl Peer {
    fn push(&self, messages: Vec<Inbound>) {
        self.push.send(messages).unwrap();
    }

    async fn sent(&mut self) -> Sent {
        self.sent.recv().await.unwrap()
    }
}

fn message(id: &str, sender: &str, text: &str) -> Inbound {
    Inbound::Text(Message::text(id, sender, text, &format!("re-{id}"), None))
}

async fn next_frame(side: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    side.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn send_frame(side: &mut BufReader<UnixStream>, frame: Value) {
    side.get_mut()
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
}

/// Accept one conversation's session on the fake daemon and complete its
/// handshake.
async fn accept_session(daemon: &UnixListener) -> BufReader<UnixStream> {
    let (stream, _) = daemon.accept().await.unwrap();
    let mut side = BufReader::new(stream);
    assert_eq!(next_frame(&mut side).await["type"], "initialize");
    send_frame(&mut side, json!({"type":"initialized","request_id":"channel-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"test","version":"0"}})).await;
    assert_eq!(next_frame(&mut side).await["type"], "session.start");
    send_frame(&mut side, json!({"type":"session.started","request_id":"channel-session","session_id":"s","cwd":"/","model":"test","context_max_tokens":1024,"max_server_frame_bytes":1024,"max_transcript_bytes":1024,"max_transcript_items":10,"max_prompt_history_bytes":1024,"max_prompt_history_items":10})).await;
    side
}

/// The prompt of the session's next turn.
async fn next_turn(side: &mut BufReader<UnixStream>) -> String {
    let frame = next_frame(side).await;
    assert_eq!(frame["type"], "turn.start");
    frame["prompt"].as_str().unwrap().to_owned()
}

async fn finish_turn(side: &mut BufReader<UnixStream>, content: &str) {
    send_frame(side, json!({"type":"assistant.completed","request_id":"r","session_id":"s","turn_id":"t","seq":2,"content":content})).await;
    send_frame(side, json!({"type":"turn.completed","request_id":"r","session_id":"s","turn_id":"t","seq":3,"steps":1,"usage":{}})).await;
}

/// Poll `condition` until it holds; the caller's overall timeout bounds it.
async fn eventually(mut condition: impl FnMut() -> bool) {
    while !condition() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_claimed_message_runs_one_turn_and_its_reply_answers_it() {
    let (bench, mut peer) = Bench::new();
    peer.push(vec![message("m1", "alice", "hello")]);
    let reply = bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "hello");
            // The claim and the batch's checkpoint are durable before the turn.
            let saved = bench.state();
            assert_eq!(saved.in_flight.len(), 1);
            assert_eq!(saved.in_flight[0].message_id, "m1");
            assert_eq!(saved.in_flight[0].to_user_id, "alice");
            assert_eq!(saved.cursor, "c1");
            finish_turn(&mut side, "hi alice").await;
            let reply = peer.sent().await;
            eventually(|| {
                let saved = bench.state();
                saved.in_flight.is_empty() && saved.pending.is_empty() && saved.seen == ["m1"]
            })
            .await;
            reply
        })
        .await;
    assert_eq!(
        reply,
        Sent {
            to: "alice".into(),
            reply_to: "re-m1".into(),
            text: "hi alice".into(),
        }
    );
}

#[tokio::test]
async fn one_senders_messages_run_in_order_while_another_sender_runs_alongside() {
    let (bench, mut peer) = Bench::new();
    peer.push(vec![
        message("a1", "alice", "first"),
        message("a2", "alice", "second"),
        message("b1", "bob", "other"),
    ]);
    bench
        .run(async {
            let mut first = accept_session(&peer.daemon).await;
            let mut second = accept_session(&peer.daemon).await;
            let (mut alice, mut bob) = match next_turn(&mut first).await.as_str() {
                "first" => {
                    assert_eq!(next_turn(&mut second).await, "other");
                    (first, second)
                }
                "other" => {
                    assert_eq!(next_turn(&mut second).await, "first");
                    (second, first)
                }
                prompt => panic!("unexpected first turn {prompt:?}"),
            };
            // Bob's turn is running while Alice's first is: finishing Bob's
            // first proves Alice's queue does not hold it back.
            finish_turn(&mut bob, "to bob").await;
            assert_eq!(peer.sent().await.to, "bob");
            // Alice's second message waits for her first turn to end.
            assert_eq!(bench.state().in_flight.len(), 2);
            finish_turn(&mut alice, "one").await;
            assert_eq!(next_turn(&mut alice).await, "second");
            finish_turn(&mut alice, "two").await;
            let replies = [peer.sent().await, peer.sent().await];
            assert_eq!(
                replies.map(|sent| (sent.reply_to, sent.text)),
                [
                    ("re-a1".to_owned(), "one".to_owned()),
                    ("re-a2".to_owned(), "two".to_owned()),
                ]
            );
        })
        .await;
}

#[tokio::test]
async fn a_full_conversation_queue_gets_the_busy_reply_without_a_turn() {
    let (bench, mut peer) = Bench::new();
    peer.push(
        (0..=MAX_QUEUED_PER_CONVERSATION)
            .map(|i| message(&format!("m{i}"), "alice", &format!("p{i}")))
            .collect(),
    );
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "p0");
            let overflow = format!("m{MAX_QUEUED_PER_CONVERSATION}");
            let busy = peer.sent().await;
            assert_eq!(busy.reply_to, format!("re-{overflow}"));
            assert_eq!(busy.text, BUSY_REPLY);
            eventually(|| {
                let saved = bench.state();
                saved.in_flight.len() == MAX_QUEUED_PER_CONVERSATION
                    && saved.seen == [overflow.clone()]
                    && saved.pending.is_empty()
            })
            .await;
        })
        .await;
}

#[tokio::test]
async fn a_redelivered_message_is_answered_once() {
    let (bench, mut peer) = Bench::new();
    peer.push(vec![message("m1", "alice", "hello")]);
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "hello");
            // The platform sends it again mid-turn, then once more after the
            // reply; neither starts another turn or claim.
            peer.push(vec![message("m1", "alice", "hello")]);
            eventually(|| bench.state().cursor == "c2").await;
            assert_eq!(bench.state().in_flight.len(), 1);
            finish_turn(&mut side, "hi").await;
            assert_eq!(peer.sent().await.text, "hi");
            peer.push(vec![
                message("m1", "alice", "hello"),
                message("m2", "alice", "next"),
            ]);
            // The next turn is the new message's: the repeat was dropped.
            assert_eq!(next_turn(&mut side).await, "next");
            finish_turn(&mut side, "ok").await;
            assert_eq!(peer.sent().await.reply_to, "re-m2");
            eventually(|| bench.state().seen == ["m1", "m2"]).await;
        })
        .await;
}
