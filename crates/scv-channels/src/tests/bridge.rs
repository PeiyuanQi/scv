//! Tests of `serve` in `src/lib.rs` through an in-memory transport and a fake
//! SCV daemon, so the shared bridge is exercised without any platform's
//! HTTP API: claims, ordering, concurrency, busy replies, and redelivery.

use super::*;
use crate::media::MediaSettings;
use scv_client::Layout;
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
    /// Files the bridge asked to download, all of which fail.
    downloads: StdMutex<u32>,
    /// Texts the platform refuses.
    refuse: StdMutex<Vec<String>>,
    /// A recipient the platform cannot reach for now: sends to it fail.
    unreachable: StdMutex<Option<String>>,
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
        if self.unreachable.lock().unwrap().as_deref() == Some(message.to) {
            bail!("the fake platform cannot reach this chat");
        }
        if self
            .refuse
            .lock()
            .unwrap()
            .iter()
            .any(|text| text == message.text)
        {
            return Ok(SendOutcome::Rejected);
        }
        self.sent
            .send(Sent {
                to: message.to.into(),
                reply_to: message.reply_to.into(),
                text: message.text.into(),
            })
            .unwrap();
        Ok(SendOutcome::Delivered)
    }

    async fn download(&self, _media: &Media, _max_bytes: u64) -> Result<Downloaded> {
        *self.downloads.lock().unwrap() += 1;
        bail!("the fake platform serves no files")
    }

    /// A reference resolves to itself, in brackets.
    async fn resolve(&self, _message_id: &str, reference: &str) -> Result<Resolved> {
        Ok(Resolved {
            context: format!("[{reference}]"),
            media: Vec::new(),
        })
    }
}

/// One account's bridge under test: its store and the transport it runs.
struct Bench {
    directory: tempfile::TempDir,
    store: state::Store<Test>,
    socket: std::path::PathBuf,
    transport: FakeTransport,
    /// The account owner; `None` unless a test sets one.
    owner: Option<&'static str>,
    /// Whose messages the account answers: anyone, unless a test says so.
    senders: state::Senders,
    /// What the account carries: an ordinary chat unless a test says so.
    purpose: state::Purpose,
    /// The owner holds remote tools; off unless a test says so.
    tools: bool,
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
            downloads: StdMutex::new(0),
            refuse: StdMutex::new(Vec::new()),
            unreachable: StdMutex::new(None),
        };
        let bench = Self {
            directory,
            store,
            socket,
            transport,
            owner: None,
            senders: state::Senders::Anyone,
            purpose: state::Purpose::Chat,
            tools: false,
        };
        (bench, Peer { daemon, push, sent })
    }

    /// An account that answers only `owner`.
    fn owned_by(owner: &'static str) -> (Self, Peer) {
        let (bench, peer) = Self::new();
        let bench = Self {
            owner: Some(owner),
            senders: state::Senders::Owner,
            ..bench
        };
        (bench, peer)
    }

    /// A mail chat owned by `owner`.
    fn mail_chat(owner: &'static str) -> (Self, Peer) {
        let (bench, peer) = Self::owned_by(owner);
        let bench = Self {
            purpose: state::Purpose::Mail,
            ..bench
        };
        (bench, peer)
    }

    /// Run the bridge next to `peer`, which plays the platform and the
    /// daemon, until `peer` returns; fails the test after 15 seconds.
    async fn run<T>(&self, peer: impl std::future::Future<Output = T>) -> T {
        self.run_linked(&hub::Link::detached(), peer).await
    }

    /// [`Bench::run`], linked to a daemon's hub through `link`.
    async fn run_linked<T>(
        &self,
        link: &hub::Link,
        peer: impl std::future::Future<Output = T>,
    ) -> T {
        let media = MediaOptions::new(
            &Layout::new(self.directory.path()),
            "test",
            "default",
            MediaSettings::default(),
        );
        let bridge = serve(
            &self.transport,
            BridgeRun {
                account: "default",
                workspace: self.directory.path(),
                socket: &self.socket,
                owner: self.owner,
                tool_owner: self.owner.filter(|_| self.tools).map(|owner| ToolOwner {
                    user_id: owner.into(),
                    turn_timeout: OWNER_TURN_TIMEOUT,
                }),
                senders: self.senders,
                purpose: self.purpose,
                media,
                log: crate::chatlog::LogOptions::test(self.directory.path(), "test"),
                link,
                report: &|_| {},
            },
            &self.store,
            |_| Ok(true),
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

/// A direct message the platform says was sent now.
fn message(id: &str, sender: &str, text: &str) -> Inbound {
    sent_at(id, sender, text, hub::unix_ms())
}

/// A direct message the platform says was sent at `sent_ms`.
fn sent_at(id: &str, sender: &str, text: &str, sent_ms: u64) -> Inbound {
    let mut message = Message::text(id, sender, text, &format!("re-{id}"), None);
    message.sent_ms = Some(sent_ms);
    Inbound::Text(message)
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

#[tokio::test]
async fn an_owner_only_account_checkpoints_anothers_message_without_answering_it() {
    let (bench, mut peer) = Bench::owned_by("alice");
    peer.push(vec![message("m1", "mallory", "hello")]);
    bench
        .run(async {
            // Handled like any message: seen, and the checkpoint moves past
            // it, so it is never replayed. No claim and no reply.
            eventually(|| {
                let saved = bench.state();
                saved.cursor == "c1" && saved.seen == ["m1"]
            })
            .await;
            let saved = bench.state();
            assert!(saved.in_flight.is_empty() && saved.pending.is_empty());
            // The owner's next message is the first to reach the daemon.
            peer.push(vec![message("m2", "alice", "hi")]);
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "hi");
            finish_turn(&mut side, "hello alice").await;
            assert_eq!(peer.sent().await.to, "alice");
            eventually(|| bench.state().seen == ["m1", "m2"]).await;
            // Nothing ever went to the other sender.
            assert!(peer.sent.try_recv().is_err());
        })
        .await;
}

#[tokio::test]
async fn a_voice_message_without_a_transcript_gets_the_voice_reply_and_nothing_else() {
    let (bench, mut peer) = Bench::owned_by("alice");
    let Inbound::Text(mut voice) = message("v1", "alice", "") else {
        unreachable!()
    };
    voice.media.push(Media {
        kind: MediaKind::Audio,
        name: String::new(),
        size: Some(1024),
        mime: Some("audio/opus".into()),
        transcript: None,
        source: "file_v3_voice".into(),
    });
    peer.push(vec![Inbound::Text(voice)]);
    bench
        .run(async {
            let reply = peer.sent().await;
            assert_eq!(
                reply,
                Sent {
                    to: "alice".into(),
                    reply_to: "re-v1".into(),
                    text: VOICE_REPLY.into(),
                }
            );
            eventually(|| {
                let saved = bench.state();
                saved.cursor == "c1" && saved.seen == ["v1"] && saved.pending.is_empty()
            })
            .await;
            assert!(bench.state().in_flight.is_empty());
            // No session was opened: the next message's is the first.
            peer.push(vec![message("m2", "alice", "hi")]);
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "hi");
            finish_turn(&mut side, "hello").await;
            assert_eq!(peer.sent().await.reply_to, "re-m2");
        })
        .await;
    assert_eq!(*bench.transport.downloads.lock().unwrap(), 0);
}

/// An account that answers anyone and is owned by `owner`, linked to a hub,
/// so others' messages reach the question check too.
fn asked_bench() -> (Bench, Peer, Arc<hub::Hub>, hub::Link) {
    let (bench, peer) = Bench::new();
    let bench = Bench {
        owner: Some("owner"),
        ..bench
    };
    let hub = hub::Hub::new(None);
    let link = hub::Link::new(Arc::clone(&hub), "fake:default", Some("owner".into()));
    (bench, peer, hub, link)
}

/// Ask question `id` in the owner's chat and return its answer receiver
/// once the question's text is stored.
async fn ask(hub: &hub::Hub, id: &str, text: &str) -> tokio::sync::oneshot::Receiver<bool> {
    eventually(|| hub.owner("fake:default").is_some()).await;
    let answered = hub.ask(id, "fake:default", "owner").unwrap();
    hub.send_question(id, "fake:default", "owner", text)
        .await
        .unwrap();
    answered
}

/// Wait until everything queued is delivered, which for a question means
/// it reached the chat and is open.
async fn delivered(bench: &Bench) {
    eventually(|| bench.state().pending.is_empty()).await;
}

#[tokio::test]
async fn a_question_reaches_the_owners_chat_and_a_plain_answer_resolves_it_without_a_turn() {
    let (bench, mut peer, hub, link) = asked_bench();
    let question = "Publish SCV 0.3.0?\n\nReply yes or no. No answer in 30 minutes counts as no.";
    bench
        .run_linked(&link, async {
            let mut answered = ask(&hub, "q1", question).await;
            // The question goes to the owner's direct chat, answering nothing.
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: String::new(),
                    text: question.into(),
                }
            );
            delivered(&bench).await;
            // Other words from the owner run a turn, casual ones included;
            // the question keeps waiting.
            peer.push(vec![message("m1", "owner", "what is this about?")]);
            let mut owner = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut owner).await, "what is this about?");
            finish_turn(&mut owner, "A release.").await;
            assert_eq!(peer.sent().await.text, "A release.");
            peer.push(vec![message("m0", "owner", "ok")]);
            assert_eq!(next_turn(&mut owner).await, "ok");
            finish_turn(&mut owner, "Noted.").await;
            assert_eq!(peer.sent().await.text, "Noted.");
            // Another sender, and the owner in a group, cannot answer.
            peer.push(vec![message("b1", "bob", "yes")]);
            let mut bob = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut bob).await, "yes");
            finish_turn(&mut bob, "to bob").await;
            assert_eq!(peer.sent().await.to, "bob");
            let Inbound::Text(mut in_group) = message("g1", "owner", "yes") else {
                unreachable!()
            };
            in_group.group = Some("group".into());
            peer.push(vec![Inbound::Text(in_group)]);
            let mut group = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut group).await, "yes");
            finish_turn(&mut group, "in the group").await;
            assert_eq!(peer.sent().await.reply_to, "re-g1");
            assert!(answered.try_recv().is_err(), "still waiting");

            // The owner's plain yes answers it and is acknowledged, no turn.
            peer.push(vec![message("m2", "owner", "Yes!")]);
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: "re-m2".into(),
                    text: ANSWERED_YES.into(),
                }
            );
            assert_eq!(answered.await, Ok(true));
            peer.push(vec![message("m3", "owner", "thanks")]);
            assert_eq!(next_turn(&mut owner).await, "thanks", "m2 ran no turn");
            finish_turn(&mut owner, "welcome").await;
            assert_eq!(peer.sent().await.text, "welcome");

            // A later question, answered no.
            let answered = ask(&hub, "q2", "Deploy?").await;
            assert_eq!(peer.sent().await.text, "Deploy?");
            delivered(&bench).await;
            peer.push(vec![message("m4", "owner", "不")]);
            let sent = peer.sent().await;
            assert_eq!(
                (sent.reply_to.as_str(), sent.text.as_str()),
                ("re-m4", ANSWERED_NO)
            );
            assert_eq!(answered.await, Ok(false));
            // A yes with no question waiting is an ordinary message again.
            peer.push(vec![message("m5", "owner", "yes")]);
            assert_eq!(next_turn(&mut owner).await, "yes");
            finish_turn(&mut owner, "yes to what?").await;
            assert_eq!(peer.sent().await.text, "yes to what?");
            eventually(|| {
                let saved = bench.state();
                saved.in_flight.is_empty()
                    && saved.pending.is_empty()
                    && ["m2", "m4"]
                        .iter()
                        .all(|id| saved.seen.iter().any(|seen| seen == id))
            })
            .await;
        })
        .await;
}

#[tokio::test]
async fn a_yes_sent_before_the_question_reached_the_chat_never_answers_it() {
    let (bench, mut peer, hub, link) = asked_bench();
    // The owner said yes to something else a minute ago; the platform hands
    // it over only now, as Feishu's catch-up after a reconnect or a WeChat
    // poll back from its backoff does.
    let earlier = hub::unix_ms() - 60_000;
    bench
        .run_linked(&link, async {
            let mut answered = ask(&hub, "q1", "Publish?").await;
            assert_eq!(peer.sent().await.text, "Publish?");
            delivered(&bench).await;
            peer.push(vec![sent_at("m1", "owner", "yes", earlier)]);
            let mut owner = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut owner).await, "yes");
            finish_turn(&mut owner, "Yes to what?").await;
            assert_eq!(peer.sent().await.text, "Yes to what?");
            // A message whose platform gives no time cannot answer either.
            peer.push(vec![Inbound::Text(Message::text(
                "m2", "owner", "yes", "re-m2", None,
            ))]);
            assert_eq!(next_turn(&mut owner).await, "yes");
            finish_turn(&mut owner, "Still unsure.").await;
            assert_eq!(peer.sent().await.text, "Still unsure.");
            assert!(answered.try_recv().is_err(), "still waiting");
            // Written after the question reached the chat: the answer.
            peer.push(vec![message("m3", "owner", "yes")]);
            assert_eq!(peer.sent().await.text, ANSWERED_YES);
            assert_eq!(answered.await, Ok(true));
        })
        .await;
}

#[tokio::test]
async fn a_question_still_waiting_to_be_delivered_cannot_be_answered() {
    let (bench, mut peer, hub, link) = asked_bench();
    // The platform cannot reach the owner for now, so the question waits in
    // the outbox, as it would behind a reply that keeps failing.
    *bench.transport.unreachable.lock().unwrap() = Some("owner".into());
    bench
        .run_linked(&link, async {
            let mut answered = ask(&hub, "q1", "Publish?").await;
            // The owner cannot have seen it: their yes is about something
            // else and runs a turn.
            peer.push(vec![message("m1", "owner", "yes")]);
            let mut owner = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut owner).await, "yes");
            finish_turn(&mut owner, "Yes to what?").await;
            eventually(|| bench.state().pending.len() == 2).await;
            assert!(answered.try_recv().is_err(), "still waiting");
            // Once the question is delivered, the owner's next yes answers.
            *bench.transport.unreachable.lock().unwrap() = None;
            assert_eq!(peer.sent().await.text, "Publish?");
            assert_eq!(peer.sent().await.text, "Yes to what?");
            delivered(&bench).await;
            peer.push(vec![message("m2", "owner", "yes")]);
            assert_eq!(peer.sent().await.text, ANSWERED_YES);
            assert_eq!(answered.await, Ok(true));
        })
        .await;
}

#[tokio::test]
async fn a_question_the_platform_refuses_fails_and_is_never_held() {
    let (bench, mut peer, hub, link) = asked_bench();
    bench
        .transport
        .refuse
        .lock()
        .unwrap()
        .push("Publish?".into());
    bench
        .run_linked(&link, async {
            let answered = ask(&hub, "q1", "Publish?").await;
            // The asker learns at once that no answer will come.
            assert!(answered.await.is_err());
            delivered(&bench).await;
            assert!(bench.state().held.is_empty(), "never held for later");
            // So a yes afterwards is an ordinary message, and its reply
            // carries no held question.
            peer.push(vec![message("m1", "owner", "yes")]);
            let mut owner = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut owner).await, "yes");
            finish_turn(&mut owner, "Yes to what?").await;
            assert_eq!(peer.sent().await.text, "Yes to what?");
            // The chat is free for the next question.
            assert!(hub.ask("q2", "fake:default", "owner").is_some());
        })
        .await;
}

#[tokio::test]
async fn a_question_that_no_longer_waits_is_never_sent() {
    let (bench, mut peer, hub, link) = asked_bench();
    *bench.transport.unreachable.lock().unwrap() = Some("owner".into());
    bench
        .run_linked(&link, async {
            let _answered = ask(&hub, "q1", "Publish?").await;
            // It ran out, or its asker left, before the platform took it.
            assert_eq!(hub.withdraw("q1"), hub::Withdrawal::Unsent);
            *bench.transport.unreachable.lock().unwrap() = None;
            hub.notify("fake:default", "owner", "Later news")
                .await
                .unwrap();
            // Outbox order would send the question first.
            assert_eq!(peer.sent().await.text, "Later news");
            delivered(&bench).await;
            assert!(peer.sent.try_recv().is_err(), "nothing else was sent");
        })
        .await;
}

/// Hold the account's transaction, as a daemon command or the reconciler
/// does, and have the poller save a batch meanwhile: its save waits out the
/// busy account while holding the state lock.
async fn park_the_poller(bench: &Bench, peer: &Peer) -> std::fs::File {
    let transaction = bench.store.transaction("default").unwrap();
    let batches = *bench.transport.received.lock().unwrap();
    peer.push(vec![Inbound::Ignored { id: "i1".into() }]);
    eventually(|| *bench.transport.received.lock().unwrap() > batches).await;
    transaction
}

#[tokio::test]
async fn a_notice_arriving_while_a_save_waits_out_a_busy_account_is_stored() {
    let (bench, mut peer) = Bench::new();
    let hub = hub::Hub::new(None);
    let link = hub::Link::new(Arc::clone(&hub), "fake:default", Some("owner".into()));
    bench
        .run_linked(&link, async {
            eventually(|| hub.owner("fake:default").is_some()).await;
            let transaction = park_the_poller(&bench, &peer).await;
            let release = async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(transaction);
            };
            let notify = tokio::time::timeout(
                Duration::from_secs(5),
                hub.notify("fake:default", "owner", "Heads up"),
            );
            let (stored, ()) = tokio::join!(notify, release);
            assert_eq!(
                stored.expect("the notice is stored once the account is free"),
                Ok(())
            );
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: String::new(),
                    text: "Heads up".into(),
                }
            );
            // The poller went on too: its batch is checkpointed.
            eventually(|| {
                let saved = bench.state();
                saved.cursor == "c1" && saved.seen == ["i1"] && saved.pending.is_empty()
            })
            .await;
            // And it still receives.
            peer.push(vec![message("m1", "alice", "hello")]);
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "hello");
            finish_turn(&mut side, "hi").await;
            assert_eq!(peer.sent().await.text, "hi");
        })
        .await;
}

#[tokio::test]
async fn a_notice_its_sender_gave_up_on_is_never_sent() {
    let (bench, mut peer) = Bench::new();
    let hub = hub::Hub::new(None);
    let link = hub::Link::new(Arc::clone(&hub), "fake:default", Some("owner".into()));
    bench
        .run_linked(&link, async {
            eventually(|| hub.owner("fake:default").is_some()).await;
            // The account cannot store it before its sender stops waiting,
            // as `Hub::notify` does after 30 seconds.
            let transaction = park_the_poller(&bench, &peer).await;
            let late = tokio::time::timeout(
                Duration::from_millis(50),
                hub.notify("fake:default", "owner", "Too late"),
            )
            .await;
            assert!(late.is_err(), "the sender gave up");
            drop(transaction);
            hub.notify("fake:default", "owner", "In time")
                .await
                .unwrap();
            // Notices are stored in order, so the dropped one would be first.
            assert_eq!(peer.sent().await.text, "In time");
            eventually(|| {
                let saved = bench.state();
                saved.pending.is_empty() && saved.seen == ["i1"]
            })
            .await;
            assert!(peer.sent.try_recv().is_err(), "nothing else was sent");
        })
        .await;
}

/// The owner's direct chat log in `bench`'s instance.
fn owner_log(bench: &Bench, owner: &str) -> std::path::PathBuf {
    bench
        .directory
        .path()
        .join("history/test/default")
        .join(conversation_dir(owner))
}

/// Accept a session like [`accept_session`], returning its `session.start`.
async fn accept_session_start(daemon: &UnixListener) -> (BufReader<UnixStream>, Value) {
    let (stream, _) = daemon.accept().await.unwrap();
    let mut side = BufReader::new(stream);
    assert_eq!(next_frame(&mut side).await["type"], "initialize");
    send_frame(&mut side, json!({"type":"initialized","request_id":"channel-init","protocol_version":scv_protocol::PROTOCOL_VERSION,"server":{"name":"test","version":"0"}})).await;
    let start = next_frame(&mut side).await;
    assert_eq!(start["type"], "session.start");
    send_frame(&mut side, json!({"type":"session.started","request_id":"channel-session","session_id":"s","cwd":"/","model":"test","context_max_tokens":1024,"max_server_frame_bytes":1024,"max_transcript_bytes":1024,"max_transcript_items":10,"max_prompt_history_bytes":1024,"max_prompt_history_items":10})).await;
    (side, start)
}

fn logged(dir: &std::path::Path) -> Vec<Vec<(history::Role, String)>> {
    let (episodes, _) = history::episodes(dir, None, None, 100).unwrap();
    episodes
        .iter()
        .rev()
        .map(|episode| {
            history::read_episode(dir, &episode.id, 0, 100)
                .unwrap()
                .unwrap()
                .0
                .into_iter()
                .map(|entry| (entry.role, entry.text))
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn the_owners_direct_chat_is_logged_and_new_starts_a_fresh_episode() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "hello")]);
    let dir = owner_log(&bench, "owner");
    bench
        .run(async {
            let (mut side, start) = accept_session_start(&peer.daemon).await;
            // The session names the log, so the server reloads its open episode.
            assert_eq!(
                start["chat"],
                json!({"channel":"test","account":"default","conversation":conversation_dir("owner")})
            );
            assert_eq!(next_turn(&mut side).await, "hello");
            // The owner's message is in the log before the turn ends.
            assert_eq!(logged(&dir), [vec![(history::Role::Owner, "hello".to_owned())]]);
            finish_turn(&mut side, "hi owner").await;
            assert_eq!(peer.sent().await.text, "hi owner");
            // `/new` clears the session and ends the episode, without a turn.
            peer.push(vec![message("m2", "owner", " /NEW ")]);
            let clear = next_frame(&mut side).await;
            assert_eq!(clear["type"], "session.clear");
            send_frame(&mut side, json!({"type":"session.cleared","request_id":clear["request_id"],"session_id":"s","seq":4})).await;
            assert_eq!(peer.sent().await.text, NEW_LOGGED_REPLY);
            peer.push(vec![message("m3", "owner", "again")]);
            assert_eq!(next_turn(&mut side).await, "again");
            finish_turn(&mut side, "fresh").await;
            assert_eq!(peer.sent().await.text, "fresh");
        })
        .await;
    let owner = |text: &str| (history::Role::Owner, text.to_owned());
    let scv = |text: &str| (history::Role::Scv, text.to_owned());
    assert_eq!(
        logged(&dir),
        [
            vec![owner("hello"), scv("hi owner")],
            vec![owner("again"), scv("fresh")],
        ]
    );
    let (episodes, _) = history::episodes(&dir, None, None, 10).unwrap();
    assert!(episodes[1].ended && !episodes[0].ended);
}

#[tokio::test]
async fn other_senders_are_not_logged_and_their_sessions_name_no_log() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let bench = Bench {
        senders: state::Senders::Anyone,
        ..bench
    };
    peer.push(vec![message("m1", "alice", "hello")]);
    bench
        .run(async {
            let (mut side, start) = accept_session_start(&peer.daemon).await;
            assert!(start.get("chat").is_none(), "{start}");
            assert_eq!(next_turn(&mut side).await, "hello");
            finish_turn(&mut side, "hi alice").await;
            peer.sent().await;
        })
        .await;
    assert!(!bench.directory.path().join("history").exists());
}

#[tokio::test]
async fn a_nearly_full_disk_saves_no_files_from_chat_but_logs_the_text() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let hub = hub::Hub::new(None);
    hub.set_low_disk(true);
    let link = hub::Link::new(Arc::clone(&hub), "test:default", Some("owner".into()));
    let with_photo = |id: &str, text: &str| {
        let mut message = Message::text(id, "owner", text, &format!("re-{id}"), None);
        message.media.push(Media {
            kind: MediaKind::Image,
            name: "cat.jpg".into(),
            size: Some(10),
            mime: None,
            transcript: None,
            source: "cat".into(),
        });
        Inbound::Text(message)
    };
    peer.push(vec![with_photo("m1", "look")]);
    bench
        .run_linked(&link, async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(
                next_turn(&mut side).await,
                "look\n[image cat.jpg: not saved, the disk is nearly full]"
            );
            finish_turn(&mut side, "I cannot see it").await;
            peer.sent().await;
            // A photo alone gets the fixed reply instead of a turn.
            peer.push(vec![with_photo("m2", "")]);
            assert_eq!(peer.sent().await.text, LOW_DISK_REPLY);
        })
        .await;
    assert_eq!(*bench.transport.downloads.lock().unwrap(), 0);
    let entries = logged(&owner_log(&bench, "owner"));
    assert_eq!(
        entries[0].iter().map(|(role, _)| *role).collect::<Vec<_>>(),
        [
            history::Role::Owner,
            history::Role::Scv,
            history::Role::Owner,
            history::Role::System
        ]
    );
}

#[tokio::test]
async fn new_lets_a_running_background_report_finish_and_sends_it_first() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "hello")]);
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "hello");
            finish_turn(&mut side, "hi").await;
            assert_eq!(peer.sent().await.text, "hi");
            // The server starts reporting a finished job just as `/new` comes.
            let origin = json!({"kind":"background","jobs":["job-1"]});
            send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
            peer.push(vec![message("m2", "owner", "/new")]);
            let clear = next_frame(&mut side).await;
            assert_eq!(clear["type"], "session.clear");
            send_frame(&mut side, json!({"type":"error","request_id":clear["request_id"],"code":"invalid_request","message":"the session is busy","fatal":false})).await;
            send_frame(&mut side, json!({"type":"assistant.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"content":"job-1 is done"})).await;
            send_frame(&mut side, json!({"type":"turn.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":12,"steps":1,"usage":{},"origin":origin})).await;
            let clear = next_frame(&mut side).await;
            assert_eq!(clear["type"], "session.clear");
            send_frame(&mut side, json!({"type":"session.cleared","request_id":clear["request_id"],"session_id":"s","seq":13})).await;
            // The report reaches the owner before the fresh start does.
            assert_eq!(peer.sent().await.text, "job-1 is done");
            assert_eq!(peer.sent().await.text, NEW_LOGGED_REPLY);
        })
        .await;
    let entries = logged(&owner_log(&bench, "owner"));
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].last().unwrap(),
        &(history::Role::Scv, "job-1 is done".to_owned())
    );
}

/// A reviewed job's outcome as the server sends it, and SCV's lines for it.
fn reviewed_outcome() -> (serde_json::Value, &'static str) {
    (
        json!({"job":"job-1",
               "review":{"outcome":"approved","round":2,"rounds":3,"reviewer":"claude",
                         "journal":"rev-1-abcdef"},
               "landing":{"mode":"none","status":"not_requested"}}),
        "job-1 · Review: approved · round 2 of 3 · reviewer claude\n\
         job-1 · Landing: not requested",
    )
}

/// The `agent` call that starts reviewed job `job-1`, then the turn's end.
async fn start_reviewed_job(side: &mut BufReader<UnixStream>, reply: &str) {
    let output = json!({"job":"job-1","agent":"codex","status":"running","background":true,
                        "review":{"journal":"rev-1-abcdef"}})
    .to_string();
    let started = json!({"job":"job-1","tool":"agent","agent":"codex","status":"running",
                         "task":"Fix it","journal":"rev-1-abcdef"});
    send_frame(side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":1,"call_id":"c","name":"agent","success":true,"output":output,"truncated":false,"jobs":[started]})).await;
    finish_turn(side, reply).await;
}

#[tokio::test]
async fn a_reviewed_jobs_outcome_reaches_the_owner_as_scvs_lines_before_the_report() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "fix it, reviewed")]);
    let (outcome, lines) = reviewed_outcome();
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "fix it, reviewed");
            start_reviewed_job(&mut side, "Started job-1.").await;
            assert_eq!(peer.sent().await.text, "Started job-1.");
            // The record a restart reads names the journal.
            eventually(|| {
                bench
                    .state()
                    .jobs
                    .iter()
                    .any(|job| job.journal == "rev-1-abcdef")
            })
            .await;
            let origin = json!({"kind":"background","jobs":["job-1"],"outcomes":[outcome]});
            send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
            send_frame(&mut side, json!({"type":"assistant.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"content":"The fix was approved."})).await;
            send_frame(&mut side, json!({"type":"turn.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":12,"steps":1,"usage":{},"origin":origin})).await;
            assert_eq!(peer.sent().await.text, lines);
            assert_eq!(peer.sent().await.text, "The fix was approved.");
            eventually(|| bench.state().jobs.is_empty()).await;
        })
        .await;
}

#[tokio::test]
async fn a_reviewed_jobs_lines_come_alone_for_a_cancelled_report_and_after_a_settling_turn() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "fix it, reviewed")]);
    let (outcome, lines) = reviewed_outcome();
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "fix it, reviewed");
            start_reviewed_job(&mut side, "Started job-1.").await;
            assert_eq!(peer.sent().await.text, "Started job-1.");
            // The owner stops the report turn: SCV's lines still arrive.
            let origin = json!({"kind":"background","jobs":["job-1"],"outcomes":[outcome]});
            send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
            send_frame(&mut side, json!({"type":"turn.cancelled","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"origin":origin})).await;
            assert_eq!(peer.sent().await.text, lines);
            // A job the model settles itself: the lines follow its reply.
            peer.push(vec![message("m2", "owner", "and the other one?")]);
            assert_eq!(next_turn(&mut side).await, "and the other one?");
            let settled = json!({"job":"job-1","tool":"agent","agent":"codex","status":"completed","outcome":outcome});
            send_frame(&mut side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":20,"call_id":"c2","name":"agent_wait","success":true,"output":"{}","truncated":false,"jobs":[settled]})).await;
            finish_turn(&mut side, "Approved, see above.").await;
            assert_eq!(peer.sent().await.text, "Approved, see above.");
            assert_eq!(peer.sent().await.text, lines);
        })
        .await;
}

#[tokio::test]
async fn a_cancel_stated_before_the_job_stopped_is_followed_by_its_update() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "fix it, reviewed")]);
    let (outcome, lines) = reviewed_outcome();
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "fix it, reviewed");
            start_reviewed_job(&mut side, "Started job-1.").await;
            assert_eq!(peer.sent().await.text, "Started job-1.");
            // The owner has it stopped; it is still stopping when the cancel
            // returns, so its journal is still open.
            peer.push(vec![message("m2", "owner", "stop it")]);
            assert_eq!(next_turn(&mut side).await, "stop it");
            let mut pending = outcome.clone();
            pending["review"]["journal_pending"] = true.into();
            let settled = json!({"job":"job-1","tool":"agent","agent":"codex","status":"cancelled","outcome":pending});
            send_frame(&mut side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":20,"call_id":"c2","name":"agent_cancel","success":true,"output":"{}","truncated":false,"jobs":[settled]})).await;
            finish_turn(&mut side, "Stopped it.").await;
            assert_eq!(peer.sent().await.text, "Stopped it.");
            let said = peer.sent().await.text;
            assert!(said.contains("still open"), "{said}");
            // Still tracked, and listened for, until its update.
            eventually(|| bench.state().jobs.iter().any(|job| job.job == "job-1")).await;
            send_frame(&mut side, json!({"type":"background.updated","session_id":"s","seq":30,"outcomes":[outcome]})).await;
            assert_eq!(peer.sent().await.text, lines);
            eventually(|| bench.state().jobs.is_empty()).await;
        })
        .await;
}

#[tokio::test]
async fn a_final_update_is_never_followed_by_a_stale_still_open_notice() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "fix it, reviewed")]);
    let (outcome, _) = reviewed_outcome();
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "fix it, reviewed");
            start_reviewed_job(&mut side, "Started job-1.").await;
            assert_eq!(peer.sent().await.text, "Started job-1.");
            peer.push(vec![message("m2", "owner", "stop it")]);
            assert_eq!(next_turn(&mut side).await, "stop it");
            let mut pending = outcome.clone();
            pending["review"]["journal_pending"] = true.into();
            let mut last = outcome;
            last["review"]["journal_incomplete"] = true.into();
            // Should a final update ever arrive ahead of the snapshot it
            // supersedes, the snapshot is dropped.
            send_frame(&mut side, json!({"type":"background.updated","session_id":"s","seq":20,"outcomes":[last]})).await;
            let settled = json!({"job":"job-1","tool":"agent","agent":"codex","status":"cancelled","outcome":pending});
            send_frame(&mut side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":21,"call_id":"c2","name":"agent_cancel","success":true,"output":"{}","truncated":false,"jobs":[settled]})).await;
            finish_turn(&mut side, "Stopped it.").await;
            assert_eq!(peer.sent().await.text, "Stopped it.");
            let said = peer.sent().await.text;
            assert!(said.contains("INCOMPLETE"), "{said}");
            eventually(|| bench.state().jobs.is_empty()).await;
            // Nothing else follows, such as a stale notice.
            peer.push(vec![message("m3", "owner", "thanks")]);
            assert_eq!(next_turn(&mut side).await, "thanks");
            finish_turn(&mut side, "welcome").await;
            assert_eq!(peer.sent().await.text, "welcome");
        })
        .await;
}

#[tokio::test]
async fn a_failed_report_turn_that_will_be_tried_again_sends_nothing_and_keeps_its_job() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "land it")]);
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "land it");
            start_background_job(&mut side, "job-1", "Started job-1.").await;
            assert_eq!(peer.sent().await.text, "Started job-1.");
            eventually(|| bench.state().jobs.iter().any(|job| job.job == "job-1")).await;
            // The model is unreachable; the server will try again.
            let origin = json!({"kind":"background","jobs":["job-1"]});
            let retried = json!({"kind":"background","jobs":["job-1"],"retry_seconds":30});
            send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
            send_frame(&mut side, json!({"type":"turn.failed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"code":"provider_error","message":"provider returned HTTP 503 Service Unavailable","origin":retried})).await;
            // The owner's next message runs as usual, and the job still
            // keeps the conversation and its record.
            peer.push(vec![message("m2", "owner", "still there?")]);
            assert_eq!(next_turn(&mut side).await, "still there?");
            finish_turn(&mut side, "yes").await;
            assert_eq!(peer.sent().await.text, "yes");
            assert!(bench.state().jobs.iter().any(|job| job.job == "job-1"));
            // The next try works, and only its report reaches the owner.
            send_frame(&mut side, json!({"type":"turn.started","request_id":"background:2","session_id":"s","turn_id":"t3","seq":14,"origin":origin})).await;
            send_frame(&mut side, json!({"type":"assistant.completed","request_id":"background:2","session_id":"s","turn_id":"t3","seq":15,"content":"job-1 is done"})).await;
            send_frame(&mut side, json!({"type":"turn.completed","request_id":"background:2","session_id":"s","turn_id":"t3","seq":16,"steps":1,"usage":{},"origin":origin})).await;
            assert_eq!(peer.sent().await.text, "job-1 is done");
            eventually(|| {
                let saved = bench.state();
                saved.jobs.is_empty() && saved.pending.is_empty()
            })
            .await;
        })
        .await;
}

#[tokio::test]
async fn a_job_the_server_reports_directly_reaches_the_owner_with_the_error_and_reply() {
    let (bench, mut peer) = Bench::owned_by("owner");
    peer.push(vec![message("m1", "owner", "land it")]);
    let error = "provider returned HTTP 503 Service Unavailable: MODEL_NOT_AVAILABLE \
                 (gave up after 3 attempts)";
    let report = json!({"job":"job-1","agent":"codex","task":"Land it","status":"completed",
        "session":"codex-1","reply":"Landed 0.9.9."});
    let expected =
        session::direct_report(error, 3, &[serde_json::from_value(report.clone()).unwrap()]);
    bench
        .run(async {
            let mut side = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut side).await, "land it");
            start_background_job(&mut side, "job-1", "Started job-1.").await;
            assert_eq!(peer.sent().await.text, "Started job-1.");
            // The last try failed: the server reports the job itself, then
            // ends the turn without a retry.
            let origin = json!({"kind":"background","jobs":["job-1"]});
            send_frame(&mut side, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
            send_frame(&mut side, json!({"type":"background.reported","session_id":"s","seq":11,"code":"provider_error","message":error,"attempts":3,"reports":[report]})).await;
            send_frame(&mut side, json!({"type":"turn.failed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":12,"code":"provider_error","message":error,"origin":origin})).await;
            let sent = peer.sent().await;
            assert_eq!(sent.text, expected);
            assert!(sent.text.contains("Landed 0.9.9."), "{}", sent.text);
            assert!(sent.text.contains("MODEL_NOT_AVAILABLE"), "{}", sent.text);
            eventually(|| {
                let saved = bench.state();
                saved.jobs.is_empty() && saved.pending.is_empty()
            })
            .await;
            // Nothing else follows, such as a generic failure.
            peer.push(vec![message("m2", "owner", "thanks")]);
            assert_eq!(next_turn(&mut side).await, "thanks");
            finish_turn(&mut side, "welcome").await;
            assert_eq!(peer.sent().await.text, "welcome");
        })
        .await;
    let entries = logged(&owner_log(&bench, "owner"));
    assert_eq!(
        entries[0][2..4],
        [
            (history::Role::System, expected),
            (history::Role::Owner, "thanks".to_owned())
        ]
    );
}

#[tokio::test]
async fn replies_written_while_recovering_from_a_crash_are_logged() {
    let (bench, mut peer) = Bench::owned_by("owner");
    bench
        .store
        .save_state(
            "default",
            &state::BridgeState {
                in_flight: vec![state::InFlight {
                    message_id: "lost".into(),
                    to_user_id: "owner".into(),
                    context_token: "re-lost".into(),
                    key: String::new(),
                }],
                ..Default::default()
            },
        )
        .unwrap();
    bench
        .run(async {
            assert_eq!(peer.sent().await.text, FAILURE_REPLY);
        })
        .await;
    assert_eq!(
        logged(&owner_log(&bench, "owner")),
        [vec![(history::Role::System, FAILURE_REPLY.to_owned())]]
    );
}

/// A mail chat linked to a fresh hub as `fake:mail`.
fn mail_bench() -> (Bench, Peer, Arc<hub::Hub>, hub::Link) {
    let (bench, peer) = Bench::mail_chat("owner");
    let hub = hub::Hub::new(None);
    let link = hub::Link::new(Arc::clone(&hub), "fake:mail", Some("owner".into()));
    (bench, peer, hub, link)
}

/// Run the bridge until it stops by itself, as a refused start does.
async fn run_to_end(bench: &Bench, purpose: state::Purpose) -> Result<()> {
    let media = MediaOptions::new(
        &Layout::new(bench.directory.path()),
        "test",
        "default",
        MediaSettings::default(),
    );
    let link = hub::Link::detached();
    let run = serve(
        &bench.transport,
        BridgeRun {
            account: "default",
            workspace: bench.directory.path(),
            socket: &bench.socket,
            owner: bench.owner,
            tool_owner: None,
            senders: bench.senders,
            purpose,
            media,
            log: crate::chatlog::LogOptions::test(bench.directory.path(), "test"),
            link: &link,
            report: &|_| {},
        },
        &bench.store,
        |_| Ok(true),
    );
    match tokio::time::timeout(Duration::from_millis(500), run).await {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

#[tokio::test]
async fn a_mail_chat_answers_its_owner_with_fixed_replies_and_never_runs_a_model() {
    let (bench, mut peer, hub, link) = mail_bench();
    let mut quoted = Message::text("m3", "owner", "approve Q7M2KD", "re-m3", None);
    quoted.quoted = true;
    let group = Message::text("m5", "owner", "mail status", "re-m5", Some("g1"));
    peer.push(vec![
        message("m1", "owner", "hello"),
        message("m2", "owner", "mail status"),
        Inbound::Text(quoted),
        message("m4", "owner", "approve Q7M2KD"),
        Inbound::Text(group),
        message("m6", "stranger", "mail status"),
    ]);
    bench
        .run_linked(&link, async {
            let expected = [
                ("re-m1", mail_chat::HELP_REPLY),
                ("re-m2", mail_chat::NO_ACCOUNTS_REPLY),
                ("re-m3", mail_chat::HELP_REPLY),
                ("re-m4", mail_chat::NOT_RUNNING_REPLY),
            ];
            for (reply_to, text) in expected {
                assert_eq!(
                    peer.sent().await,
                    Sent {
                        to: "owner".into(),
                        reply_to: reply_to.into(),
                        text: text.into(),
                    }
                );
            }
            eventually(|| bench.state().seen.len() == 6).await;
            assert!(peer.sent.try_recv().is_err(), "nothing else was sent");
            // No daemon session was ever opened, and writing here never
            // makes this the owner's last chat.
            let connected =
                tokio::time::timeout(Duration::from_millis(100), peer.daemon.accept()).await;
            assert!(connected.is_err(), "a mail chat opened a session");
            assert_eq!(hub.last_owner(), None);
            assert_eq!(hub.purpose("fake:mail"), Some(state::Purpose::Mail));
        })
        .await;
    // It keeps no chat log, and it is marked a mail chat for good.
    assert!(!bench.directory.path().join("history").exists());
    assert!(bench.state().mail_chat);
}

#[tokio::test]
async fn mail_status_counts_the_email_accounts_reporting_here() {
    let (bench, mut peer, hub, link) = mail_bench();
    let registration = hub.register_mail("email:default", vec!["fake:mail".into()]);
    registration.set_counts(scv_protocol::MailCounts {
        seen_today: 3,
        token_budget: 0,
        ..Default::default()
    });
    let _elsewhere = hub.register_mail("email:other", vec!["fake:other".into()]);
    peer.push(vec![message("m1", "owner", "mail status")]);
    bench
        .run_linked(&link, async {
            let text = peer.sent().await.text;
            assert!(text.starts_with("email:default: 3 new today"), "{text}");
            assert!(!text.contains("email:other"), "{text}");
        })
        .await;
}

#[tokio::test]
async fn a_mail_chat_stores_each_keyed_notice_once_and_records_its_delivery() {
    let (bench, mut peer, hub, link) = mail_bench();
    bench
        .run_linked(&link, async {
            eventually(|| hub.is_mail_chat("fake:mail")).await;
            // SCV's own notices and questions never go to a mail chat.
            assert_eq!(
                hub.notify("fake:mail", "owner", "SCV updated").await,
                Err(hub::NotifyError::WrongPurpose)
            );
            assert!(hub.ask("q1", "fake:mail", "owner").is_some());
            assert_eq!(
                hub.send_question("q1", "fake:mail", "owner", "Publish?")
                    .await,
                Err(hub::NotifyError::WrongPurpose)
            );
            let text = "Mail · default · 1 new\n│ Subject: hi";
            hub.notify_keyed("fake:mail", "owner", text, "mail:e1:batch:1")
                .await
                .unwrap();
            // Mail text goes out as written: never marked as SCV's words.
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: String::new(),
                    text: text.into(),
                }
            );
            eventually(|| {
                matches!(
                    hub.keyed_outcome("fake:mail", "mail:e1:batch:1"),
                    Some(hub::KeyedOutcome::Delivered { .. })
                )
            })
            .await;
            // Handed over again, as after a lost acknowledgement: stored once.
            hub.notify_keyed("fake:mail", "owner", text, "mail:e1:batch:1")
                .await
                .unwrap();
            assert_eq!(hub.keyed_outcome("fake:mail", "other"), None);
            eventually(|| bench.state().pending.is_empty()).await;
            assert!(peer.sent.try_recv().is_err(), "a key was sent twice");
            let saved = bench.state();
            assert_eq!(saved.recent_keys.len(), 1);
            assert!(matches!(
                saved.recent_keys[0].outcome,
                hub::KeyedOutcome::Delivered { .. }
            ));
        })
        .await;
}

#[tokio::test]
async fn a_refused_mail_notice_is_recorded_and_never_held_for_later() {
    let (bench, _peer, hub, link) = mail_bench();
    bench
        .transport
        .refuse
        .lock()
        .unwrap()
        .push("refused digest".into());
    bench
        .run_linked(&link, async {
            eventually(|| hub.is_mail_chat("fake:mail")).await;
            hub.notify_keyed("fake:mail", "owner", "refused digest", "k1")
                .await
                .unwrap();
            eventually(|| hub.keyed_outcome("fake:mail", "k1") == Some(hub::KeyedOutcome::Refused))
                .await;
            eventually(|| bench.state().pending.is_empty()).await;
            assert!(bench.state().held.is_empty());
        })
        .await;
}

#[tokio::test]
async fn an_ordinary_chat_takes_no_mail_notice() {
    let (bench, _peer) = Bench::owned_by("owner");
    let hub = hub::Hub::new(None);
    let link = hub::Link::new(Arc::clone(&hub), "fake:default", Some("owner".into()));
    bench
        .run_linked(&link, async {
            eventually(|| hub.owner("fake:default").is_some()).await;
            assert_eq!(
                hub.notify_keyed("fake:default", "owner", "mail text", "k1")
                    .await,
                Err(hub::NotifyError::WrongPurpose)
            );
            assert!(bench.state().pending.is_empty());
        })
        .await;
}

#[tokio::test]
async fn a_mail_chat_reloads_how_its_notices_went_into_the_hub() {
    let (bench, _peer, hub, link) = mail_bench();
    let mut saved = state::BridgeState {
        mail_chat: true,
        ..Default::default()
    };
    saved.recent_keys.push(state::RecentKey {
        key: "k1".into(),
        outcome: hub::KeyedOutcome::Delivered { at_ms: 5 },
        stored_at: unix_now(),
    });
    bench.store.save_state("default", &saved).unwrap();
    bench
        .run_linked(&link, async {
            eventually(|| hub.keyed_outcome("fake:mail", "k1").is_some()).await;
            assert_eq!(
                hub.keyed_outcome("fake:mail", "k1"),
                Some(hub::KeyedOutcome::Delivered { at_ms: 5 })
            );
        })
        .await;
}

#[tokio::test]
async fn a_chat_that_carried_mail_never_runs_as_an_ordinary_chat() {
    let (bench, _peer) = Bench::mail_chat("owner");
    run_to_end(&bench, state::Purpose::Mail).await.unwrap();
    assert!(bench.state().mail_chat);
    let error = run_to_end(&bench, state::Purpose::Chat)
        .await
        .expect_err("a marked mail chat must not start as a chat");
    assert!(
        error.to_string().contains("has carried SCV mail"),
        "{error}"
    );
}

#[tokio::test]
async fn an_account_that_ran_as_an_ordinary_chat_cannot_become_a_mail_chat() {
    const REPORT: &str = "BACKGROUND REPORT from an ordinary session";
    let fills: [fn(&mut state::BridgeState); 6] = [
        |state| {
            state
                .pending
                .push(new_pending("", "owner", "", REPORT, MAX_REPLY_BYTES));
        },
        |state| {
            state.held.push(state::HeldReply {
                key: "owner".into(),
                to_user_id: "owner".into(),
                reply: REPORT.into(),
                held_at: 1,
            });
        },
        |state| {
            state.in_flight.push(state::InFlight {
                message_id: "m1".into(),
                to_user_id: "owner".into(),
                context_token: String::new(),
                key: "owner".into(),
            });
        },
        |state| {
            state.jobs.push(state::RunningJob {
                to_user_id: "owner".into(),
                key: String::new(),
                reply_to: String::new(),
                job: "job-1".into(),
                tool: "agent".into(),
                agent: "codex".into(),
                task: REPORT.into(),
                journal: String::new(),
                started_at: 1,
            });
        },
        |state| state.cursor = "c1".into(),
        |state| state.seen.push("m1".into()),
    ];
    for fill in fills {
        let (bench, mut peer) = Bench::mail_chat("owner");
        let mut saved = bench.state();
        fill(&mut saved);
        bench.store.save_state("default", &saved).unwrap();
        let before = serde_json::to_string(&bench.state()).unwrap();
        let error = run_to_end(&bench, state::Purpose::Mail)
            .await
            .expect_err("an ordinary chat's state must not become a mail chat's");
        assert!(
            error.to_string().contains("run as an ordinary chat"),
            "{error}"
        );
        assert!(!bench.state().mail_chat);
        assert_eq!(
            serde_json::to_string(&bench.state()).unwrap(),
            before,
            "the state is left as it was"
        );
        assert!(peer.sent.try_recv().is_err(), "nothing was delivered");
    }
}

#[tokio::test]
async fn a_chat_log_that_cannot_be_read_keeps_an_account_from_becoming_a_mail_chat() {
    let (bench, mut peer) = Bench::mail_chat("owner");
    // A file where the log's directory belongs cannot be listed.
    let history = bench.directory.path().join("history/test/default");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, "not a directory").unwrap();
    let error = run_to_end(&bench, state::Purpose::Mail)
        .await
        .expect_err("an unreadable log must not count as none");
    assert!(error.to_string().contains("could not check"), "{error}");
    assert!(!bench.state().mail_chat);
    assert!(peer.sent.try_recv().is_err());
}

#[tokio::test]
async fn an_account_with_a_chat_log_cannot_become_a_mail_chat() {
    let (bench, _peer) = Bench::mail_chat("owner");
    let history = bench.directory.path().join("history/test/default/abc");
    std::fs::create_dir_all(&history).unwrap();
    let error = run_to_end(&bench, state::Purpose::Mail)
        .await
        .expect_err("a logged account must not become a mail chat");
    assert!(error.to_string().contains("has a chat log"), "{error}");
    assert!(!bench.state().mail_chat);
    assert!(history.exists(), "nothing of the person's is removed");
}

/// A message of `sender`'s in thread `thread`, which is on the message
/// `origin` names when it is set. Its reply handle answers it inside the
/// thread, and the thread's own handle posts into it.
fn in_thread(id: &str, sender: &str, text: &str, thread: &str, origin: Option<&str>) -> Inbound {
    let Inbound::Text(mut message) = message(id, sender, text) else {
        unreachable!()
    };
    message.reply_to = format!("thread-re-{id}");
    message.thread = Some(Thread {
        id: thread.into(),
        reply_to: format!("into-{thread}"),
        origin: origin.map(str::to_owned),
    });
    Inbound::Text(message)
}

/// The owner's turn whose call starts background job `job`, answered with
/// `reply`.
async fn start_background_job(side: &mut BufReader<UnixStream>, job: &str, reply: &str) {
    let output =
        json!({"job":job,"agent":"codex","status":"running","background":true}).to_string();
    let started =
        json!({"job":job,"tool":"agent","agent":"codex","status":"running","task":"Land it"});
    send_frame(side, json!({"type":"tool.completed","request_id":"r","session_id":"s","turn_id":"t","seq":1,"call_id":"c","name":"agent","success":true,"output":output,"truncated":false,"jobs":[started]})).await;
    finish_turn(side, reply).await;
}

#[tokio::test]
async fn a_thread_runs_on_its_own_session_and_its_answers_and_reports_stay_in_it() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let thread_key = "owner\0thread\0omt_1";
    peer.push(vec![message("m1", "owner", "hello")]);
    bench
        .run(async {
            let (mut direct, start) = accept_session_start(&peer.daemon).await;
            assert_eq!(start["chat"]["conversation"], conversation_dir("owner"));
            assert_eq!(next_turn(&mut direct).await, "hello");
            finish_turn(&mut direct, "hi").await;
            assert_eq!(peer.sent().await.reply_to, "re-m1");

            // A thread gets a session of its own, logged on its own, whose
            // first turn also shows the message the thread is on.
            peer.push(vec![in_thread(
                "t1",
                "owner",
                "expand on that",
                "omt_1",
                Some("root-m0"),
            )]);
            let (mut threaded, start) = accept_session_start(&peer.daemon).await;
            assert_eq!(start["chat"]["conversation"], conversation_dir(thread_key));
            assert_eq!(
                next_turn(&mut threaded).await,
                "[root-m0]\n\nexpand on that"
            );
            assert_eq!(bench.state().in_flight[0].key, thread_key);
            start_background_job(&mut threaded, "job-1", "Started job-1.").await;
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: "thread-re-t1".into(),
                    text: "Started job-1.".into(),
                }
            );
            // The job is recorded as the thread's, with the handle into it.
            eventually(|| {
                bench.state().jobs.iter().any(|job| {
                    job.job == "job-1" && job.key == thread_key && job.reply_to == "into-omt_1"
                })
            })
            .await;

            // The thread's next message runs on the same session, without
            // the thread's origin again; the direct chat keeps its own.
            peer.push(vec![in_thread(
                "t2",
                "owner",
                "and then?",
                "omt_1",
                Some("root-m0"),
            )]);
            assert_eq!(next_turn(&mut threaded).await, "and then?");
            finish_turn(&mut threaded, "then this").await;
            assert_eq!(peer.sent().await.reply_to, "thread-re-t2");
            peer.push(vec![message("m2", "owner", "back here")]);
            assert_eq!(next_turn(&mut direct).await, "back here");
            finish_turn(&mut direct, "welcome back").await;
            assert_eq!(peer.sent().await.reply_to, "re-m2");

            // The job's report goes into the thread, answering nothing there.
            let origin = json!({"kind":"background","jobs":["job-1"]});
            send_frame(&mut threaded, json!({"type":"turn.started","request_id":"background:1","session_id":"s","turn_id":"t2","seq":10,"origin":origin})).await;
            send_frame(&mut threaded, json!({"type":"assistant.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":11,"content":"job-1 is done"})).await;
            send_frame(&mut threaded, json!({"type":"turn.completed","request_id":"background:1","session_id":"s","turn_id":"t2","seq":12,"steps":1,"usage":{},"origin":origin})).await;
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: "into-omt_1".into(),
                    text: "job-1 is done".into(),
                }
            );
            eventually(|| {
                let saved = bench.state();
                saved.jobs.is_empty() && saved.pending.is_empty()
            })
            .await;
        })
        .await;
    let owner = |text: &str| (history::Role::Owner, text.to_owned());
    let scv = |text: &str| (history::Role::Scv, text.to_owned());
    assert_eq!(
        logged(&owner_log(&bench, "owner")),
        [vec![
            owner("hello"),
            scv("hi"),
            owner("back here"),
            scv("welcome back")
        ]]
    );
    assert_eq!(
        logged(&owner_log(&bench, thread_key)),
        [vec![
            owner("expand on that"),
            scv("Started job-1."),
            owner("and then?"),
            scv("then this"),
            scv("job-1 is done"),
        ]]
    );
}

#[tokio::test]
async fn new_in_a_thread_starts_that_thread_over_and_leaves_the_direct_chat_alone() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let thread_key = "owner\0thread\0omt_1";
    peer.push(vec![in_thread("t1", "owner", "first", "omt_1", None)]);
    bench
        .run(async {
            let mut threaded = accept_session(&peer.daemon).await;
            // A message that starts its thread has no origin to show.
            assert_eq!(next_turn(&mut threaded).await, "first");
            finish_turn(&mut threaded, "one").await;
            peer.sent().await;
            peer.push(vec![in_thread("t2", "owner", "/new", "omt_1", None)]);
            let clear = next_frame(&mut threaded).await;
            assert_eq!(clear["type"], "session.clear");
            send_frame(&mut threaded, json!({"type":"session.cleared","request_id":clear["request_id"],"session_id":"s","seq":4})).await;
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: "thread-re-t2".into(),
                    text: NEW_LOGGED_REPLY.into(),
                }
            );
        })
        .await;
    let (episodes, _) = history::episodes(&owner_log(&bench, thread_key), None, None, 5).unwrap();
    assert!(episodes[0].ended);
    assert!(!owner_log(&bench, "owner").exists());
}

#[tokio::test]
async fn a_yes_in_a_thread_runs_a_turn_and_leaves_the_question_waiting() {
    let (bench, mut peer, hub, link) = asked_bench();
    bench
        .run_linked(&link, async {
            let answered = ask(&hub, "q1", "Publish?").await;
            assert_eq!(peer.sent().await.reply_to, "", "asked in the direct chat");
            delivered(&bench).await;
            peer.push(vec![in_thread("t1", "owner", "yes", "omt_1", None)]);
            let mut threaded = accept_session(&peer.daemon).await;
            assert_eq!(next_turn(&mut threaded).await, "yes");
            finish_turn(&mut threaded, "yes to what?").await;
            assert_eq!(peer.sent().await.reply_to, "thread-re-t1");
            peer.push(vec![message("m1", "owner", "yes")]);
            assert_eq!(peer.sent().await.text, ANSWERED_YES);
            assert_eq!(answered.await, Ok(true));
        })
        .await;
}

#[tokio::test]
async fn a_group_thread_is_its_senders_own_conversation_and_is_not_logged() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let Inbound::Text(mut group_thread) = in_thread("g1", "owner", "hi all", "omt_9", None) else {
        unreachable!()
    };
    group_thread.group = Some("group".into());
    peer.push(vec![Inbound::Text(group_thread)]);
    bench
        .run(async {
            let (mut side, start) = accept_session_start(&peer.daemon).await;
            assert!(start.get("chat").is_none(), "{start}");
            assert_eq!(
                bench.state().in_flight[0].key,
                "group\0owner\0thread\0omt_9"
            );
            assert_eq!(next_turn(&mut side).await, "hi all");
            finish_turn(&mut side, "hello").await;
            assert_eq!(peer.sent().await.reply_to, "thread-re-g1");
        })
        .await;
    assert!(!bench.directory.path().join("history").exists());
}

#[tokio::test]
async fn an_owners_thread_is_owner_work_of_their_direct_chat_in_the_hub() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let bench = Bench {
        tools: true,
        ..bench
    };
    let hub = hub::Hub::new(None);
    let link = hub::Link::new(Arc::clone(&hub), "fake:default", Some("owner".into()));
    bench
        .run_linked(&link, async {
            eventually(|| hub.owner("fake:default").is_some()).await;
            peer.push(vec![in_thread("t1", "owner", "land it", "omt_1", None)]);
            let (mut side, start) = accept_session_start(&peer.daemon).await;
            assert_eq!(
                start["no_tools"], false,
                "the owner's tools hold in the thread"
            );
            assert_eq!(next_turn(&mut side).await, "land it");
            // A planned restart waits for it, as for the direct chat.
            assert_eq!(hub.owner_claims(), 1);
            assert_eq!(hub.last_owner().unwrap().peer, "owner");
            start_background_job(&mut side, "job-1", "Started job-1.").await;
            assert_eq!(peer.sent().await.reply_to, "thread-re-t1");
            eventually(|| hub.owner_claims() == 0 && hub.session_work("s") == 1).await;
            // Work the thread started asks and announces in the direct chat.
            assert_eq!(
                hub.origin("s"),
                Some(hub::Origin {
                    component: "fake:default".into(),
                    peer: "owner".into(),
                })
            );
            hub.notify("fake:default", "owner", "SCV updated.")
                .await
                .unwrap();
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: String::new(),
                    text: "SCV updated.".into(),
                }
            );
        })
        .await;
}

#[tokio::test]
async fn a_threads_origin_alone_is_not_enough_for_a_turn() {
    let (bench, mut peer) = Bench::owned_by("owner");
    let Inbound::Text(mut photo) = in_thread("t1", "owner", "", "omt_1", Some("root-m0")) else {
        unreachable!()
    };
    photo.media.push(Media {
        kind: MediaKind::Image,
        name: "cat.jpg".into(),
        size: Some(10),
        mime: None,
        transcript: None,
        source: "cat".into(),
    });
    peer.push(vec![Inbound::Text(photo)]);
    bench
        .run(async {
            // The photo does not come in, so the sender is told why in the
            // thread, and no session starts.
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: "thread-re-t1".into(),
                    text: "SCV could not download that image. Please send it again.".into(),
                }
            );
        })
        .await;
}

/// An email account on `hub` that reports to this bench's mail chat and takes
/// commands.
fn serving_mail(
    hub: &Arc<hub::Hub>,
    component: &str,
) -> (
    hub::MailRegistration,
    tokio::sync::mpsc::Receiver<hub::MailRequest>,
) {
    let registration = hub.register_mail(component, vec!["fake:mail".into()]);
    let (commands, requests) = tokio::sync::mpsc::channel(4);
    registration.serve(commands);
    (registration, requests)
}

/// The chat command `request` carries, and where its answer goes.
fn chat_work(
    request: hub::MailRequest,
) -> (
    mail_chat::MailCommand,
    mail_chat::ChatEvidence,
    tokio::sync::oneshot::Sender<hub::MailReply>,
) {
    let hub::MailWork::Chat { command, evidence } = request.work else {
        panic!("a mail-chat command arrived as a daemon order");
    };
    (command, evidence, request.reply)
}

#[tokio::test]
async fn an_owners_direct_approve_reaches_the_hub_and_the_answer_is_stored() {
    let (bench, mut peer, hub, link) = mail_bench();
    let (registration, mut requests) = serving_mail(&hub, "email:work");
    assert!(registration.claim_code("Q7M2KD"));
    let sent_ms = 1_700_000_000_123;
    peer.push(vec![sent_at("m1", "owner", "approve Q7M2KD", sent_ms)]);
    bench
        .run_linked(&link, async {
            let request = requests.recv().await.unwrap();
            let (command, evidence, reply) = chat_work(request);
            assert_eq!(
                command,
                mail_chat::MailCommand::Approve(vec!["Q7M2KD".into()])
            );
            assert_eq!(
                (
                    evidence.route.as_str(),
                    evidence.peer.as_str(),
                    evidence.message_id.as_str(),
                    evidence.sent_ms
                ),
                ("fake:mail", "owner", "m1", Some(sent_ms))
            );
            reply
                .send(hub::MailReply::Text("Approved Q7M2KD.".into()))
                .unwrap();
            assert_eq!(
                peer.sent().await,
                Sent {
                    to: "owner".into(),
                    reply_to: "re-m1".into(),
                    text: "Approved Q7M2KD.".into(),
                }
            );
            eventually(|| bench.state().seen.iter().any(|seen| seen == "m1")).await;
            let connected =
                tokio::time::timeout(Duration::from_millis(100), peer.daemon.accept()).await;
            assert!(connected.is_err(), "a mail chat opened a session");
            assert_eq!(hub.last_owner(), None, "writing here is not the last chat");
        })
        .await;
    assert!(!bench.directory.path().join("history").exists());
    assert!(bench.state().mail_chat);
}

#[tokio::test]
async fn a_quoted_forwarded_media_group_or_other_senders_message_is_never_a_command() {
    let (bench, mut peer, hub, link) = mail_bench();
    let (registration, mut requests) = serving_mail(&hub, "email:work");
    assert!(registration.claim_code("Q7M2KD"));
    let mut quoted = Message::text("quoted", "owner", "approve Q7M2KD", "re-quoted", None);
    quoted.quoted = true;
    quoted.sent_ms = Some(1);
    let mut forwarded = Message::text("forwarded", "owner", "approve Q7M2KD", "re-forwarded", None);
    forwarded.reference = Some("earlier".into());
    forwarded.sent_ms = Some(1);
    let mut with_media = Message::text("media", "owner", "approve Q7M2KD", "re-media", None);
    with_media.media.push(Media {
        kind: MediaKind::File,
        name: "note.txt".into(),
        size: None,
        mime: None,
        transcript: None,
        source: "file".into(),
    });
    with_media.sent_ms = Some(1);
    let mut group = Message::text("group", "owner", "approve Q7M2KD", "re-group", Some("room"));
    group.sent_ms = Some(1);
    peer.push(vec![
        Inbound::Text(quoted),
        Inbound::Text(forwarded),
        Inbound::Text(with_media),
        Inbound::Text(group),
        sent_at("stranger", "stranger", "approve Q7M2KD", 1),
        sent_at("ok", "owner", "approve Q7M2KD", 9),
    ]);
    bench
        .run_linked(&link, async {
            let request = requests.recv().await.unwrap();
            let (command, evidence, reply) = chat_work(request);
            assert_eq!(
                command,
                mail_chat::MailCommand::Approve(vec!["Q7M2KD".into()]),
                "only the owner's plain message is a command"
            );
            assert_eq!(evidence.message_id, "ok");
            reply
                .send(hub::MailReply::Text("Approved Q7M2KD.".into()))
                .unwrap();
            for reply_to in ["re-quoted", "re-forwarded", "re-media"] {
                assert_eq!(
                    peer.sent().await,
                    Sent {
                        to: "owner".into(),
                        reply_to: reply_to.into(),
                        text: mail_chat::HELP_REPLY.into(),
                    },
                    "a quote, a forward, or a file is help, not a command"
                );
            }
            assert_eq!(peer.sent().await.text, "Approved Q7M2KD.");
            eventually(|| bench.state().seen.len() == 6).await;
            assert!(
                requests.try_recv().is_err(),
                "no other message was a command"
            );
            assert!(
                peer.sent.try_recv().is_err(),
                "a group or a stranger got a reply"
            );
            assert_eq!(*bench.transport.downloads.lock().unwrap(), 0);
            let connected =
                tokio::time::timeout(Duration::from_millis(100), peer.daemon.accept()).await;
            assert!(connected.is_err(), "a mail chat opened a session");
            assert_eq!(hub.last_owner(), None);
        })
        .await;
    assert!(!bench.directory.path().join("history").exists());
}

#[tokio::test]
async fn mail_status_joins_the_counts_with_each_accounts_status() {
    let (bench, mut peer, hub, link) = mail_bench();
    let (alpha, mut alpha_requests) = serving_mail(&hub, "email:alpha");
    let (zeta, mut zeta_requests) = serving_mail(&hub, "email:zeta");
    alpha.set_counts(scv_protocol::MailCounts {
        seen_today: 4,
        token_budget: 0,
        ..Default::default()
    });
    zeta.set_counts(scv_protocol::MailCounts {
        seen_today: 1,
        queued: 2,
        token_budget: 0,
        ..Default::default()
    });
    let _elsewhere = hub.register_mail("email:elsewhere", vec!["fake:other".into()]);
    peer.push(vec![message("m1", "owner", "mail status")]);
    bench
        .run_linked(&link, async {
            let (alpha_command, alpha_evidence, alpha_reply) =
                chat_work(alpha_requests.recv().await.unwrap());
            let (zeta_command, _, zeta_reply) = chat_work(zeta_requests.recv().await.unwrap());
            assert_eq!(alpha_command, mail_chat::MailCommand::Status);
            assert_eq!(zeta_command, mail_chat::MailCommand::Status);
            assert_eq!(alpha_evidence.message_id, "m1");
            assert_eq!(alpha_evidence.route, "fake:mail");
            assert_eq!(alpha_evidence.peer, "owner");
            alpha_reply
                .send(hub::MailReply::Text("alpha: 1 waiting.".into()))
                .unwrap();
            zeta_reply
                .send(hub::MailReply::Text("zeta: 2 waiting.".into()))
                .unwrap();
            let text = peer.sent().await.text;
            assert!(
                text.starts_with("email:alpha: 4 new today"),
                "counts come first: {text}"
            );
            assert!(
                text.contains("email:zeta: 1 new today") && text.contains("2 waiting to be sent"),
                "every account reporting here is counted: {text}"
            );
            assert!(
                text.contains("alpha: 1 waiting.\nzeta: 2 waiting."),
                "each account's status follows the counts: {text}"
            );
            assert!(!text.contains("email:elsewhere"), "{text}");
            let connected =
                tokio::time::timeout(Duration::from_millis(100), peer.daemon.accept()).await;
            assert!(connected.is_err(), "a mail chat opened a session");
        })
        .await;
    assert!(!bench.directory.path().join("history").exists());
}

#[tokio::test]
async fn the_same_mail_command_handed_over_twice_is_answered_once() {
    let (bench, mut peer, hub, link) = mail_bench();
    let (registration, mut requests) = serving_mail(&hub, "email:work");
    assert!(registration.claim_code("Q7M2KD"));
    let again = || message("m1", "owner", "approve Q7M2KD");
    peer.push(vec![again(), again()]);
    peer.push(vec![again()]);
    bench
        .run_linked(&link, async {
            let request = requests.recv().await.unwrap();
            let (command, evidence, reply) = chat_work(request);
            assert_eq!(
                command,
                mail_chat::MailCommand::Approve(vec!["Q7M2KD".into()])
            );
            assert_eq!(evidence.message_id, "m1");
            reply
                .send(hub::MailReply::Text("Approved Q7M2KD.".into()))
                .unwrap();
            assert_eq!(peer.sent().await.text, "Approved Q7M2KD.");
            eventually(|| bench.state().cursor == "c2").await;
            assert_eq!(
                bench
                    .state()
                    .seen
                    .iter()
                    .filter(|seen| seen.as_str() == "m1")
                    .count(),
                1,
                "the message is remembered once"
            );
            assert!(
                requests.try_recv().is_err(),
                "the same message was read as a command again"
            );
            assert!(
                peer.sent.try_recv().is_err(),
                "the same message was answered again"
            );
            let connected =
                tokio::time::timeout(Duration::from_millis(100), peer.daemon.accept()).await;
            assert!(connected.is_err(), "a mail chat opened a session");
        })
        .await;
    assert!(!bench.directory.path().join("history").exists());
}
