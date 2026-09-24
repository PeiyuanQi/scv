//! Unit tests for `src/tool.rs`.

use std::sync::Mutex;

use super::*;

#[test]
fn risks_parse_from_their_wire_names() {
    for risk in [
        ToolRisk::ReadOnly,
        ToolRisk::Filesystem,
        ToolRisk::Process,
        ToolRisk::Delegate,
        ToolRisk::Network,
    ] {
        assert_eq!(ToolRisk::parse(risk.as_str()), Some(risk));
    }
    assert_eq!(ToolRisk::parse("root"), None);
}

struct RecordingGate(Mutex<Vec<ApprovalRequest>>);

#[async_trait]
impl ApprovalGate for RecordingGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        self.0.lock().unwrap().push(request);
        Ok(true)
    }
}

#[tokio::test]
async fn tool_approvals_carry_the_call_and_deny_without_a_gate() {
    let denied = ToolApprovals::default()
        .request(
            "bash",
            ToolRisk::Process,
            PathBuf::from("/w"),
            "Run it",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!denied);
    let gate = Arc::new(RecordingGate(Mutex::new(Vec::new())));
    let approvals = ToolApprovals::new(Arc::clone(&gate) as Arc<dyn ApprovalGate>, "call-7");
    assert!(approvals.is_enabled());
    assert!(
        approvals
            .request(
                "bash",
                ToolRisk::Process,
                PathBuf::from("/w"),
                "[scv-1 depth 1] Run it",
                CancellationToken::new(),
            )
            .await
            .unwrap()
    );
    let requests = gate.0.lock().unwrap();
    assert_eq!(requests[0].call_id, "call-7");
    assert_eq!(requests[0].summary, "[scv-1 depth 1] Run it");
}

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.into(),
            description: "A named test tool".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok(self.0.into())
    }

    async fn execute(
        &self,
        _arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success(self.0))
    }
}

#[test]
fn duplicate_tool_registration_does_not_replace_the_original() {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(Named("echo"))).unwrap();
    assert!(registry.register(Arc::new(Named("echo"))).is_err());
    assert_eq!(registry.tools.len(), 1);
    assert!(registry.get("echo").is_some());
}
