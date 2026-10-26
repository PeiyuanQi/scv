//! How a connection keeps its session's queue on disk: each change is
//! recorded before it is announced, and a saved queue is restored once.

use super::*;
use crate::outbound::OutboundFrame;
use scv_client::Layout;
use scv_core::{AgentConfig, AgentRuntime, BudgetContextPolicy, ContextConfig, ToolRegistry};
use scv_protocol::QueueEntry;
use scv_tools::delegation::ProcessIdentity;
use std::sync::atomic::AtomicU64;

/// A model that never answers, so a turn runs until it is dropped.
struct Silent;

#[async_trait::async_trait]
impl scv_core::Provider for Silent {
    fn model(&self) -> &'static str {
        "silent"
    }

    async fn complete(
        &self,
        _request: scv_core::ProviderRequest,
        _deltas: Arc<dyn scv_core::TextDeltaSink>,
        _cancellation: CancellationToken,
    ) -> Result<scv_core::AssistantResponse, scv_core::ProviderError> {
        pending().await
    }
}

/// The outbound queue's bytes.
const ROOM: u32 = 64 * 1024;

struct Fixture {
    connection: Connection,
    frames: mpsc::Receiver<OutboundFrame>,
    layout: Layout,
    _home: tempfile::TempDir,
}

fn entry(prompt: &str) -> QueueEntry {
    QueueEntry {
        queue_id: format!("q-{prompt}"),
        revision: 1,
        prompt: prompt.into(),
        submitter: format!("r-{prompt}"),
        attachments: Vec::new(),
    }
}

/// An idle connection whose session `s`, in `/w`, keeps its queue of
/// `prompts` on disk under a scratch home.
fn fixture(prompts: &[&str]) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let (output, frames) = outbound_channel(ROOM as usize);
    let (done, _done) = mpsc::channel(4);
    let runtime = Arc::new(AgentRuntime::new(
        Arc::new(Silent),
        Arc::new(ToolRegistry::default()),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: String::new(),
            max_steps: 1,
            history_limits: scv_core::HistoryLimits::default(),
        },
        PathBuf::from("/w"),
    ));
    let instance = crate::test_support::test_instance(home.path());
    let layout = instance.layout.clone();
    let connection = Connection {
        turns: TurnStarter {
            output: output.clone(),
            approvals: Arc::new(ApprovalBroker::default()),
            done,
            tasks: TaskTracker::new(),
            cancellation: CancellationToken::new(),
        },
        output,
        instance,
        components: None,
        registry: test_registry(),
        hosts: None,
        cancellation: CancellationToken::new(),
        initialized: true,
        client_name: "scv-tui".into(),
        session: Some(Session {
            id: "s".into(),
            workspace: PathBuf::from("/w"),
            config: Config::default(),
            runtime,
            history: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicU64::new(0)),
            queue: Arc::new(Mutex::new(
                prompts.iter().map(|prompt| entry(prompt)).collect(),
            )),
            paused: Arc::new(AtomicBool::new(false)),
            background: None,
            tools: false,
            label: "TUI".into(),
            workbench: None,
        }),
        activity: None,
        active: None,
        active_prompt: None,
        persist: true,
        restored: None,
        background_rx: None,
        background_ready: false,
        fatal: false,
    };
    Fixture {
        connection,
        frames,
        layout,
        _home: home,
    }
}

fn event(frame: OutboundFrame) -> ServerEvent {
    serde_json::from_slice(&frame.bytes).unwrap()
}

fn prompts(saved: &persist::SavedQueue) -> Vec<&str> {
    saved
        .entries
        .iter()
        .map(|entry| entry.prompt.as_str())
        .collect()
}

/// Drive `step`, which has no room to announce anything, until session
/// `s`'s record satisfies `recorded`, and return the record.
async fn recorded_unannounced(
    step: &mut (impl Future<Output = Result<()>> + Unpin),
    layout: &Layout,
    recorded: impl Fn(&persist::SavedQueue) -> bool,
) -> persist::SavedQueue {
    // Well under SHUTDOWN_GRACE, when a held-up announcement gives up.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                biased;
                result = &mut *step => panic!("announced with no room to: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            if let Some(saved) = persist::load(layout, "s").filter(&recorded) {
                return saved;
            }
        }
    })
    .await
    .expect("recorded before it was announced")
}

#[tokio::test]
async fn a_dequeued_prompt_is_recorded_as_running_before_it_is_announced() {
    let Fixture {
        mut connection,
        mut frames,
        layout,
        _home,
    } = fixture(&["first", "second"]);
    connection.save_queue().await;
    let held = Arc::clone(&connection.output.budget)
        .acquire_many_owned(ROOM)
        .await
        .unwrap();
    let mut starting = Box::pin(connection.start_queued());
    let saved = recorded_unannounced(&mut starting, &layout, |saved| saved.active.is_some()).await;
    // A daemon that died now would name it, and never send it again.
    assert_eq!(saved.active.as_deref(), Some("first"));
    assert_eq!(prompts(&saved), ["second"]);
    assert!(frames.try_recv().is_err());
    drop(held);
    starting.await.unwrap();
    assert!(matches!(
        event(frames.recv().await.unwrap()),
        ServerEvent::QueueDequeued { queue_id, .. } if queue_id == "q-first"
    ));
    assert!(matches!(
        event(frames.recv().await.unwrap()),
        ServerEvent::TurnStarted { .. }
    ));
    assert!(connection.active.is_some());
    assert_eq!(connection.active_prompt.as_deref(), Some("first"));
}

#[tokio::test]
async fn a_started_prompt_is_recorded_before_its_turn_is_announced_and_cleared_before_its_end_is() {
    let Fixture {
        mut connection,
        mut frames,
        layout,
        _home,
    } = fixture(&["next"]);
    // Paused, so the queued prompt waits.
    connection
        .session
        .as_ref()
        .unwrap()
        .paused
        .store(true, Ordering::Release);
    let held = Arc::clone(&connection.output.budget)
        .acquire_many_owned(ROOM)
        .await
        .unwrap();
    let mut starting =
        Box::pin(connection.turn_start("r".into(), "s".into(), "build it".into(), Vec::new()));
    let saved = recorded_unannounced(&mut starting, &layout, |saved| saved.active.is_some()).await;
    assert_eq!(saved.active.as_deref(), Some("build it"));
    assert_eq!(prompts(&saved), ["next"]);
    drop(held);
    starting.await.unwrap();
    let ServerEvent::TurnStarted { turn_id, .. } = event(frames.recv().await.unwrap()) else {
        panic!("no turn.started");
    };
    // The turn ends; its task is done.
    connection.active = Some(ActiveTurn {
        turn_id: turn_id.clone(),
        cancellation: CancellationToken::new(),
        task: tokio::spawn(async {}),
    });
    let done = TurnDone {
        request_id: "r".into(),
        session_id: "s".into(),
        turn_id,
        origin: None,
        result: Err(AgentError::Cancelled),
        acted: false,
    };
    let held = Arc::clone(&connection.output.budget)
        .acquire_many_owned(ROOM)
        .await
        .unwrap();
    let mut ending = Box::pin(connection.on_turn_done(Some(done)));
    let saved = recorded_unannounced(&mut ending, &layout, |saved| saved.active.is_none()).await;
    assert_eq!(prompts(&saved), ["next"]);
    drop(held);
    ending.await.unwrap();
    assert!(matches!(
        event(frames.recv().await.unwrap()),
        ServerEvent::TurnCancelled { .. }
    ));
    assert!(connection.active_prompt.is_none());
}

#[tokio::test]
async fn a_prompt_whose_turn_could_not_start_stays_queued() {
    let Fixture {
        mut connection,
        mut frames,
        layout,
        _home,
    } = fixture(&["first", "second"]);
    // No announcement fits under the frame limit.
    connection
        .session
        .as_mut()
        .unwrap()
        .config
        .protocol
        .max_server_frame_bytes = 1;
    assert!(connection.start_queued().await.is_err());
    assert!(frames.try_recv().is_err());
    assert!(connection.active.is_none() && connection.active_prompt.is_none());
    let saved = persist::load(&layout, "s").unwrap();
    assert_eq!(saved.active, None);
    assert_eq!(prompts(&saved), ["first", "second"]);
    let queue = connection.session.as_ref().unwrap().queue.lock().await;
    assert_eq!(
        queue.front().map(|entry| entry.prompt.as_str()),
        Some("first")
    );
}

/// Save the queue of session `old`, in `/w`, as a process that has exited
/// left it: one queued prompt and one that was running.
fn save_stopped(layout: &Layout) {
    // The PID of a process that has exited (this one's child).
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let gone = ProcessIdentity::of(child.id()).unwrap();
    child.wait().unwrap();
    persist::save(
        layout,
        &persist::SavedQueue {
            v: 1,
            session: "old".into(),
            label: "TUI".into(),
            workspace: PathBuf::from("/w"),
            owner: gone,
            saved_unix: persist::unix_now(),
            paused: false,
            entries: vec![entry("queued")],
            active: Some("running".into()),
        },
    );
}

#[tokio::test]
async fn a_saved_queue_another_restore_holds_is_left_to_it_and_restored_once() {
    let Fixture {
        connection,
        layout,
        _home,
        ..
    } = fixture(&[]);
    save_stopped(&layout);
    let session = connection.session.as_ref().unwrap();
    let (claim, _, restored) = connection.restore_queue(session, "old").await.unwrap();
    assert_eq!(restored.queued, 1);
    assert_eq!(restored.interrupted.as_deref(), Some("running"));
    // An overlapping restore of the same queue gets none of it.
    assert!(connection.restore_queue(session, "old").await.is_none());
    assert_eq!(session.queue.lock().await.len(), 1);
    claim.remove();
    assert!(persist::load(&layout, "old").is_none());
    assert!(connection.restore_queue(session, "old").await.is_none());
    assert_eq!(session.queue.lock().await.len(), 1);
}

/// Restore session `old`'s saved queue into the fixture's session `s` as
/// `session.start` does: kept under `s`, the old record gone.
async fn restore_old(connection: &mut Connection, layout: &Layout) {
    save_stopped(layout);
    let session = connection.session.as_ref().unwrap();
    let (claim, old, _) = connection.restore_queue(session, "old").await.unwrap();
    connection.save_queue_of(session, None).await;
    claim.remove();
    connection.restored = Some(Restored {
        session: "s".into(),
        old,
    });
    assert!(persist::load(layout, "old").is_none());
    assert_eq!(prompts(&persist::load(layout, "s").unwrap()), ["queued"]);
}

#[tokio::test]
async fn a_restored_queue_goes_back_when_its_client_leaves_before_it_has_the_session() {
    let Fixture {
        mut connection,
        layout,
        _home,
        ..
    } = fixture(&[]);
    restore_old(&mut connection, &layout).await;
    // The client went before it saw `session.started`: a retry restoring
    // `old` finds the prompts again.
    connection.leave(false);
    let back = persist::load(&layout, "old").unwrap();
    assert_eq!(prompts(&back), ["queued"]);
    assert_eq!(back.active.as_deref(), Some("running"));
    assert!(persist::stopped(&back));
    assert!(persist::load(&layout, "s").is_none());
}

#[tokio::test]
async fn a_restored_queue_stays_with_a_client_that_spoke_or_a_daemon_that_stops() {
    let Fixture {
        mut connection,
        frames: _frames,
        layout,
        _home,
    } = fixture(&[]);
    restore_old(&mut connection, &layout).await;
    // Anything the client sends shows it has the session.
    connection
        .handle(ClientMessage::SessionPause {
            request_id: "p".into(),
            session_id: "s".into(),
            paused: true,
        })
        .await
        .unwrap();
    assert!(connection.restored.is_none());
    connection.leave(false);
    assert!(persist::load(&layout, "old").is_none());
    assert!(persist::load(&layout, "s").is_none());

    let Fixture {
        mut connection,
        layout,
        _home,
        ..
    } = fixture(&[]);
    restore_old(&mut connection, &layout).await;
    connection.leave(true);
    assert!(persist::load(&layout, "old").is_none());
    assert_eq!(prompts(&persist::load(&layout, "s").unwrap()), ["queued"]);
}

#[tokio::test]
async fn only_the_daemon_restores_a_saved_queue() {
    let Fixture {
        mut connection,
        mut frames,
        layout,
        _home,
    } = fixture(&[]);
    save_stopped(&layout);
    // A stdio connection: no workbench hosts.
    connection.session = None;
    let request = SessionStartRequest {
        cwd: "/w".into(),
        provider: None,
        model: None,
        base_url: None,
        no_tools: None,
        delegation_depth: None,
        channel: None,
        auto_approve: None,
        chat: None,
        system_prompt: None,
        persist_queue: Some(true),
        restore: Some("old".into()),
    };
    connection.session_start("r".into(), request).await.unwrap();
    let ServerEvent::Error {
        request_id,
        message,
        ..
    } = event(frames.recv().await.unwrap())
    else {
        panic!("not refused");
    };
    assert_eq!(request_id.as_deref(), Some("r"));
    assert!(message.contains("daemon"), "{message}");
    assert!(connection.session.is_none());
    assert_eq!(prompts(&persist::load(&layout, "old").unwrap()), ["queued"]);
}
