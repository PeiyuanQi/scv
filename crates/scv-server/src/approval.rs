//! Approving tool calls: the policy's own decisions, requests the client
//! answers, and the unattended answers background jobs get.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use scv_core::{AgentError, ApprovalGate, ApprovalRequest, ToolRisk};
use scv_protocol::ServerEvent;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    config::ApprovalPolicy,
    outbound::{OutboundSender, send_turn_event},
    session::{TurnMeta, next_seq},
};

#[derive(Default)]
pub(crate) struct ApprovalBroker {
    pub(crate) pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl ApprovalBroker {
    pub(crate) async fn insert(&self, id: String, sender: oneshot::Sender<bool>) {
        self.pending.lock().await.insert(id, sender);
    }

    pub(crate) async fn remove(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }

    pub(crate) async fn resolve(&self, id: &str, approved: bool) -> bool {
        let sender = self.pending.lock().await.remove(id);
        sender.is_some_and(|sender| sender.send(approved).is_ok())
    }
}

/// The decision `policy` makes for `risk` on its own, or `None` when it
/// asks the client.
pub(crate) fn policy_decision(policy: ApprovalPolicy, risk: ToolRisk) -> Option<bool> {
    match policy {
        ApprovalPolicy::OnRisk if risk == ToolRisk::ReadOnly => Some(true),
        ApprovalPolicy::Never => Some(risk == ToolRisk::ReadOnly),
        ApprovalPolicy::Always | ApprovalPolicy::OnRisk => None,
    }
}

pub(crate) struct ProtocolApprovalGate {
    pub(crate) policy: ApprovalPolicy,
    pub(crate) broker: Arc<ApprovalBroker>,
    pub(crate) meta: TurnMeta,
    pub(crate) output: OutboundSender,
}

/// Decides a background job's nested approval requests, which outlive the
/// turn that could carry them to the client. Each gets the answer the
/// session would give without asking a person: the policy's own decision,
/// else the client's declared blanket answer (`auto_approve`), else a denial.
/// It never grants more than the same request would get in the foreground.
pub(crate) struct UnattendedGate {
    pub(crate) policy: ApprovalPolicy,
    pub(crate) client_approves_all: bool,
}

#[async_trait]
impl ApprovalGate for UnattendedGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        Ok(policy_decision(self.policy, request.risk).unwrap_or(self.client_approves_all))
    }
}

#[async_trait]
impl ApprovalGate for ProtocolApprovalGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        if let Some(decision) = policy_decision(self.policy, request.risk) {
            return Ok(decision);
        }
        let approval_id = Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        self.broker.insert(approval_id.clone(), sender).await;
        let event = ServerEvent::ApprovalRequested {
            request_id: self.meta.request_id.clone(),
            session_id: self.meta.session_id.clone(),
            turn_id: self.meta.turn_id.clone(),
            seq: next_seq(&self.meta.seq),
            approval_id: approval_id.clone(),
            call_id: request.call_id,
            name: request.name,
            risk: request.risk.as_str().into(),
            cwd: request.cwd.display().to_string(),
            summary: request.summary,
        };
        if let Err(error) = send_turn_event(
            &self.output,
            event,
            self.meta.max_server_frame,
            &cancellation,
        )
        .await
        {
            self.broker.remove(&approval_id).await;
            return Err(error);
        }
        tokio::select! {
            result = receiver => result.map_err(|_| AgentError::Cancelled),
            () = cancellation.cancelled() => {
                self.broker.remove(&approval_id).await;
                Err(AgentError::Cancelled)
            }
        }
    }
}

#[cfg(test)]
mod tests;
