//! How a connection delivers what a session is owed of reviewed jobs
//! `agent_cancel` stopped: the cancel's change, sent with its call's
//! `tool.completed`, and the final outcome in a `background.updated`.

use super::*;
use crate::{events::ProtocolSink, outbound::OutboundFrame, session::TurnMeta};
use scv_core::{
    AgentConfig, AgentRuntime, BudgetContextPolicy, ContextConfig, CoreEvent, EventSink,
    ToolOutput, ToolRegistry,
};
use scv_protocol::{JobChange, JobOutcome};
use scv_tools::background::BackgroundJobs;
use std::{collections::VecDeque, sync::atomic::AtomicU64};

struct NoProvider;

#[async_trait::async_trait]
impl scv_core::Provider for NoProvider {
    fn model(&self) -> &'static str {
        "none"
    }

    async fn complete(
        &self,
        _request: scv_core::ProviderRequest,
        _deltas: Arc<dyn scv_core::TextDeltaSink>,
        _cancellation: CancellationToken,
    ) -> Result<scv_core::AssistantResponse, scv_core::ProviderError> {
        panic!("no model runs here")
    }
}

/// Job-1's final outcome, its journal `incomplete` or not, and the change
/// `agent_cancel` made while the job was still stopping.
fn outcomes(incomplete: bool) -> (JobChange, JobOutcome) {
    let last: JobOutcome = serde_json::from_value(serde_json::json!({
        "job":"job-1",
        "review":{"outcome":"stopped","reason":"cancelled","round":1,"rounds":3,
                  "journal":"rev-1-abcdef","journal_incomplete":incomplete},
        "landing":{"mode":"none","status":"not_requested"}
    }))
    .unwrap();
    let mut pending = last.clone();
    pending.review.journal_incomplete = false;
    pending.review.journal_pending = true;
    let change = serde_json::from_value(serde_json::json!({
        "job":"job-1","tool":"agent","agent":"claude","status":"cancelled","task":"Fix it.",
        "outcome":pending
    }))
    .unwrap();
    (change, last)
}

struct Fixture {
    connection: Connection,
    sink: ProtocolSink,
    jobs: Arc<BackgroundJobs>,
    frames: mpsc::Receiver<OutboundFrame>,
}

/// A connection whose session runs a turn, with its turn's event sink, and
/// an outbound queue of `capacity` bytes.
fn fixture(capacity: usize) -> Fixture {
    let (output, frames) = outbound_channel(capacity);
    let (done, _done) = mpsc::channel(4);
    let jobs = Arc::new(BackgroundJobs::new(2, None));
    let seq = Arc::new(AtomicU64::new(0));
    let config = Config::default();
    let sink = ProtocolSink {
        meta: TurnMeta {
            request_id: "turn".into(),
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: Arc::clone(&seq),
            max_server_frame: config.protocol.max_server_frame_bytes,
        },
        output: output.clone(),
        cancellation: CancellationToken::new(),
        background: Some(Arc::clone(&jobs)),
        acted: Arc::new(AtomicBool::new(false)),
    };
    let runtime = Arc::new(AgentRuntime::new(
        Arc::new(NoProvider),
        Arc::new(ToolRegistry::default()),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: String::new(),
            max_steps: 1,
            history_limits: scv_core::HistoryLimits::default(),
        },
        PathBuf::from("/unused"),
    ));
    let connection = Connection {
        turns: TurnStarter {
            output: output.clone(),
            approvals: Arc::new(ApprovalBroker::default()),
            done,
            tasks: TaskTracker::new(),
            cancellation: CancellationToken::new(),
        },
        output,
        instance: crate::test_support::test_instance("/unused"),
        components: None,
        registry: test_registry(),
        cancellation: CancellationToken::new(),
        initialized: true,
        session: Some(Session {
            id: "s".into(),
            workspace: PathBuf::from("/unused"),
            config,
            runtime,
            history: Arc::new(Mutex::new(Vec::new())),
            seq,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            paused: Arc::new(AtomicBool::new(false)),
            background: Some(Arc::clone(&jobs)),
            tools: true,
        }),
        activity: None,
        // A turn of the session runs.
        active: Some(ActiveTurn {
            turn_id: "t".into(),
            cancellation: CancellationToken::new(),
            task: tokio::spawn(pending()),
        }),
        background_rx: None,
        background_ready: false,
        fatal: false,
    };
    Fixture {
        connection,
        sink,
        jobs,
        frames,
    }
}

/// The `tool.completed` of the call `cancel` through `sink`.
async fn complete_cancel(sink: &ProtocolSink) -> Result<(), scv_core::AgentError> {
    sink.emit(CoreEvent::ToolCompleted {
        call_id: "cancel".into(),
        name: "agent_cancel".into(),
        output: ToolOutput::success("{}"),
    })
    .await
}

fn event(frame: OutboundFrame) -> ServerEvent {
    serde_json::from_slice(&frame.bytes).unwrap()
}

/// The outcomes of reviewed jobs a frame shows, and whether it is an update.
fn shown(event: &ServerEvent) -> Vec<(bool, JobOutcome)> {
    match event {
        ServerEvent::ToolCompleted { jobs, .. } => jobs
            .iter()
            .filter_map(|change| change.outcome.clone())
            .map(|outcome| (false, outcome))
            .collect(),
        ServerEvent::BackgroundUpdated { outcomes, .. } => outcomes
            .iter()
            .cloned()
            .map(|outcome| (true, outcome))
            .collect(),
        _ => Vec::new(),
    }
}

const ROOMY: usize = 16 * 1024 * 1024;

#[tokio::test]
async fn an_update_goes_out_during_a_turn_and_only_once() {
    let mut fixture = fixture(ROOMY);
    let (change, last) = outcomes(true);
    fixture.jobs.owe_for_tests("cancel", change, None);
    // The pending change reaches the client; the job stops later.
    complete_cancel(&fixture.sink).await.unwrap();
    fixture.jobs.stop_for_tests(&last);
    fixture.connection.on_background().await.unwrap();
    let frames = [
        event(fixture.frames.recv().await.unwrap()),
        event(fixture.frames.recv().await.unwrap()),
    ];
    assert!(shown(&frames[0])[0].1.review.journal_pending);
    assert_eq!(shown(&frames[1]), [(true, last)]);
    assert!(fixture.connection.active.is_some(), "not a turn of its own");
    fixture.connection.on_background().await.unwrap();
    assert!(fixture.frames.try_recv().is_err(), "sent once");
}

#[tokio::test]
async fn the_final_outcome_is_never_followed_by_a_stale_pending_one() {
    let mut fixture = fixture(ROOMY);
    let (change, last) = outcomes(true);
    // The job stopped after the cancel's wait, before its change was sent.
    fixture.jobs.owe_for_tests("cancel", change, None);
    fixture.jobs.stop_for_tests(&last);
    fixture.connection.on_background().await.unwrap();
    assert!(
        fixture.frames.try_recv().is_err(),
        "the change still owes it"
    );
    complete_cancel(&fixture.sink).await.unwrap();
    let completed = event(fixture.frames.recv().await.unwrap());
    // The change says the final outcome, superseding its snapshot.
    assert_eq!(shown(&completed), [(false, last)]);
    fixture.connection.on_background().await.unwrap();
    assert!(fixture.frames.try_recv().is_err(), "nothing more is owed");
}

#[tokio::test]
async fn a_change_whose_send_is_cancelled_leaves_its_outcome_to_an_update() {
    let mut fixture = fixture(ROOMY);
    let (change, last) = outcomes(true);
    // The job stopped inside the cancel's wait; then the owner cancels the
    // turn before its tool.completed is sent.
    let mut finished = change.clone();
    finished.outcome = Some(last.clone());
    fixture
        .jobs
        .owe_for_tests("cancel", finished, Some(last.clone()));
    fixture.sink.cancellation.cancel();
    assert!(matches!(
        complete_cancel(&fixture.sink).await,
        Err(scv_core::AgentError::Cancelled)
    ));
    assert!(fixture.frames.try_recv().is_err());
    fixture.connection.on_background().await.unwrap();
    let updated = event(fixture.frames.recv().await.unwrap());
    assert_eq!(shown(&updated), [(true, last)]);
    fixture.connection.on_background().await.unwrap();
    assert!(fixture.frames.try_recv().is_err());
}

#[tokio::test]
async fn a_change_held_up_by_backpressure_is_followed_by_the_update_not_preceded() {
    // Room for one frame at a time; the test holds it all at first.
    let mut fixture = fixture(64 * 1024);
    let budget = Arc::clone(&fixture.connection.output.budget);
    let held = budget.acquire_many_owned(64 * 1024).await.unwrap();
    let (change, last) = outcomes(true);
    fixture.jobs.owe_for_tests("cancel", change, None);
    let sink = fixture.sink;
    let sending = tokio::spawn(async move { complete_cancel(&sink).await });
    tokio::task::yield_now().await;
    // The job stops while its pending change waits to go out: no update
    // may overtake it.
    fixture.jobs.stop_for_tests(&last);
    let jobs = Arc::clone(&fixture.jobs);
    assert!(jobs.take_updates().is_empty());
    drop(held);
    sending.await.unwrap().unwrap();
    fixture.connection.on_background().await.unwrap();
    let first = event(fixture.frames.recv().await.unwrap());
    let second = event(fixture.frames.recv().await.unwrap());
    assert!(shown(&first)[0].1.review.journal_pending);
    assert_eq!(shown(&second), [(true, last)]);
}

#[tokio::test]
async fn a_turn_that_ends_without_sending_a_change_leaves_its_outcome_to_an_update() {
    let mut fixture = fixture(ROOMY);
    let (change, last) = outcomes(false);
    fixture
        .jobs
        .owe_for_tests("cancel", change, Some(last.clone()));
    // The turn's task was aborted before the call's tool.completed.
    fixture.jobs.turn_ended();
    fixture.connection.on_background().await.unwrap();
    let updated = event(fixture.frames.recv().await.unwrap());
    assert_eq!(shown(&updated), [(true, last)]);
}

#[tokio::test]
async fn a_send_dropped_mid_flight_leaves_its_outcome_to_one_update_when_the_turn_ends() {
    let mut fixture = fixture(64 * 1024);
    let held = Arc::clone(&fixture.connection.output.budget)
        .acquire_many_owned(64 * 1024)
        .await
        .unwrap();
    let (change, last) = outcomes(true);
    fixture
        .jobs
        .owe_for_tests("cancel", change, Some(last.clone()));
    // The send starts and is dropped while held up, as when its turn's
    // task is aborted.
    tokio::select! {
        biased;
        result = complete_cancel(&fixture.sink) => panic!("unexpected send: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    assert!(fixture.jobs.take_updates().is_empty());
    drop(held);
    fixture.jobs.turn_ended();
    fixture.connection.on_background().await.unwrap();
    assert_eq!(
        shown(&event(fixture.frames.recv().await.unwrap())),
        [(true, last)]
    );
    fixture.connection.on_background().await.unwrap();
    assert!(fixture.frames.try_recv().is_err());
}

#[tokio::test]
async fn each_calls_changes_settle_only_what_that_call_owed() {
    let mut fixture = fixture(ROOMY);
    let (change, last) = outcomes(true);
    let renamed = |value: &JobOutcome, job: &str| {
        let mut value = value.clone();
        value.job = job.into();
        value
    };
    let rechanged = |job: &str| {
        let mut other = change.clone();
        other.job = job.into();
        other.outcome = other.outcome.as_ref().map(|outcome| renamed(outcome, job));
        other
    };
    fixture
        .jobs
        .owe_for_tests("cancel", change.clone(), Some(last.clone()));
    fixture
        .jobs
        .owe_for_tests("cancel", rechanged("job-2"), None);
    let last3 = renamed(&last, "job-3");
    fixture
        .jobs
        .owe_for_tests("other", rechanged("job-3"), Some(last3.clone()));
    complete_cancel(&fixture.sink).await.unwrap();
    let first = shown(&event(fixture.frames.recv().await.unwrap()));
    assert_eq!(first.len(), 2);
    assert_eq!(first[0], (false, last.clone()));
    assert!(first[1].1.review.journal_pending);
    let last2 = renamed(&last, "job-2");
    fixture.jobs.stop_for_tests(&last2);
    fixture.connection.on_background().await.unwrap();
    assert_eq!(
        shown(&event(fixture.frames.recv().await.unwrap())),
        [(true, last2)]
    );
    fixture
        .sink
        .emit(CoreEvent::ToolCompleted {
            call_id: "other".into(),
            name: "agent_cancel".into(),
            output: ToolOutput::success("{}"),
        })
        .await
        .unwrap();
    assert_eq!(
        shown(&event(fixture.frames.recv().await.unwrap())),
        [(false, last3)]
    );
    fixture.connection.on_background().await.unwrap();
    assert!(fixture.frames.try_recv().is_err());
}

#[tokio::test]
async fn an_update_that_could_not_be_sent_goes_out_once_later() {
    let mut fixture = fixture(ROOMY);
    let (change, last) = outcomes(true);
    fixture
        .jobs
        .owe_for_tests("cancel", change, Some(last.clone()));
    fixture.jobs.turn_ended();
    // A frame limit nothing fits under fails the send.
    let session = fixture.connection.session.as_mut().unwrap();
    let limit = session.config.protocol.max_server_frame_bytes;
    session.config.protocol.max_server_frame_bytes = 1;
    assert!(fixture.connection.send_updates().await.is_err());
    assert!(fixture.frames.try_recv().is_err());
    fixture
        .connection
        .session
        .as_mut()
        .unwrap()
        .config
        .protocol
        .max_server_frame_bytes = limit;
    fixture.connection.send_updates().await.unwrap();
    assert_eq!(
        shown(&event(fixture.frames.recv().await.unwrap())),
        [(true, last)]
    );
    fixture.connection.send_updates().await.unwrap();
    assert!(fixture.frames.try_recv().is_err());
}
