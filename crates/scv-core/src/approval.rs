//! The [`ApprovalGate`] that decides whether a tool call may run.

use std::path::PathBuf;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{AgentError, ToolRisk};

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub name: String,
    pub risk: ToolRisk,
    pub cwd: PathBuf,
    /// What the call will do, for the person deciding: display text only.
    pub summary: String,
    /// The exact command the call runs, such as a `bash` call's script or a
    /// relayed agent's shell command, which the policy's command gates match.
    pub command: Option<String>,
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError>;
}
