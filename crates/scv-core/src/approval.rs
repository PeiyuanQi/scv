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
    pub summary: String,
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError>;
}
