//! Running one turn: announcing it, wiring its events and approvals to the
//! connection, and reporting finished background jobs.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use scv_core::{AgentError, ApprovalGate, EventSink, TurnInput};
use scv_protocol::{OriginKind, ServerEvent, TurnOrigin};
use scv_tools::background;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use super::{Session, TurnMeta, next_seq};
use crate::{
    approval::{ApprovalBroker, ProtocolApprovalGate},
    events::ProtocolSink,
    outbound::{OutboundSender, send_event},
};

pub(crate) struct ActiveTurn {
    pub(crate) turn_id: String,
    pub(crate) cancellation: CancellationToken,
    pub(crate) task: JoinHandle<()>,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

pub(crate) async fn shutdown_active_turn(mut active: ActiveTurn, grace: Duration) -> bool {
    active.cancellation.cancel();
    if tokio::time::timeout(grace, &mut active.task).await.is_ok() {
        true
    } else {
        active.task.abort();
        let _ = (&mut active.task).await;
        false
    }
}

pub(crate) struct TurnDone {
    pub(crate) request_id: String,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) origin: Option<TurnOrigin>,
    pub(crate) result: Result<scv_core::TurnOutcome, AgentError>,
}

/// What a connection needs to start a turn in its session.
pub(crate) struct TurnStarter {
    pub(crate) output: OutboundSender,
    pub(crate) approvals: Arc<ApprovalBroker>,
    pub(crate) done: mpsc::Sender<TurnDone>,
    pub(crate) tasks: TaskTracker,
    pub(crate) cancellation: CancellationToken,
}

impl TurnStarter {
    /// Announce and run one turn of `current` for `prompt`.
    pub(crate) async fn start(
        &self,
        current: &Session,
        turn_id: String,
        request_id: String,
        prompt: TurnInput,
        origin: Option<TurnOrigin>,
    ) -> Result<ActiveTurn> {
        let cancellation = self.cancellation.child_token();
        send_event(
            &self.output,
            ServerEvent::TurnStarted {
                request_id: request_id.clone(),
                session_id: current.id.clone(),
                turn_id: turn_id.clone(),
                seq: next_seq(&current.seq),
                origin: origin.clone(),
            },
            current.config.protocol.max_server_frame_bytes,
        )
        .await?;
        let meta = TurnMeta {
            request_id: request_id.clone(),
            session_id: current.id.clone(),
            turn_id: turn_id.clone(),
            seq: Arc::clone(&current.seq),
            max_server_frame: current.config.protocol.max_server_frame_bytes,
        };
        let sink: Arc<dyn EventSink> = Arc::new(ProtocolSink {
            meta: meta.clone(),
            output: self.output.clone(),
            cancellation: cancellation.clone(),
            background: current.background.clone(),
        });
        let gate: Arc<dyn ApprovalGate> = Arc::new(ProtocolApprovalGate {
            policy: current.config.tools.approval_policy,
            broker: Arc::clone(&self.approvals),
            meta,
            output: self.output.clone(),
        });
        let runtime = Arc::clone(&current.runtime);
        let history = Arc::clone(&current.history);
        let done = self.done.clone();
        let session_id = current.id.clone();
        let task_turn = turn_id.clone();
        let task_cancel = cancellation.clone();
        let task = self.tasks.spawn(async move {
            let mut history = history.lock().await;
            let result = runtime
                .run_turn(&mut history, prompt, sink, gate, task_cancel)
                .await;
            let _ = done
                .send(TurnDone {
                    request_id,
                    session_id,
                    turn_id: task_turn,
                    origin,
                    result,
                })
                .await;
        });
        Ok(ActiveTurn {
            turn_id,
            cancellation,
            task,
        })
    }

    /// Start a turn reporting background jobs the model has not seen yet,
    /// or `None` when every finished job was already seen.
    pub(crate) async fn report_background(&self, current: &Session) -> Result<Option<ActiveTurn>> {
        let Some(jobs) = &current.background else {
            return Ok(None);
        };
        let reports = jobs.take_unreported();
        if reports.is_empty() {
            return Ok(None);
        }
        let origin = TurnOrigin {
            kind: OriginKind::Background,
            jobs: reports.iter().map(|report| report.job.clone()).collect(),
        };
        let prompt = background::report_prompt(&reports);
        let request_id = format!("background:{}", Uuid::new_v4());
        self.start(
            current,
            Uuid::new_v4().to_string(),
            request_id,
            prompt.into(),
            Some(origin),
        )
        .await
        .map(Some)
    }
}

#[cfg(test)]
mod tests;
