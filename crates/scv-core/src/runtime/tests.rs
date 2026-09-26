//! Unit tests for `src/runtime.rs` and the history limits it enforces.

use std::{collections::VecDeque, sync::Mutex, time::Duration};

use serde_json::Value;
use tokio::sync::Notify;

use super::*;
use crate::{
    AssistantResponse, BudgetContextPolicy, ContextConfig, MAX_PROGRESS_EVENT_BYTES, Tool,
    ToolError, ToolRisk, ToolSpec,
};

struct ScriptedProvider {
    responses: Mutex<VecDeque<AssistantResponse>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn model(&self) -> &'static str {
        "test-model"
    }

    async fn complete(
        &self,
        _request: ProviderRequest,
        deltas: Arc<dyn TextDeltaSink>,
        _cancellation: CancellationToken,
    ) -> Result<AssistantResponse, ProviderError> {
        let response = self.responses.lock().unwrap().pop_front().unwrap();
        deltas.push(&response.content).await?;
        Ok(response)
    }
}

struct CollectSink(Mutex<Vec<CoreEvent>>);

#[async_trait]
impl EventSink for CollectSink {
    async fn emit(&self, event: CoreEvent) -> Result<(), AgentError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

struct Allow;

#[async_trait]
impl ApprovalGate for Allow {
    async fn approve(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        Ok(true)
    }
}

struct Deny;

#[async_trait]
impl ApprovalGate for Deny {
    async fn approve(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        Ok(false)
    }
}

struct WaitForCancellation {
    entered: Arc<Notify>,
}

#[async_trait]
impl ApprovalGate for WaitForCancellation {
    async fn approve(
        &self,
        _request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        self.entered.notify_one();
        cancellation.cancelled().await;
        Err(AgentError::Cancelled)
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Echo a value".into(),
            parameters: serde_json::json!({
                "type":"object",
                "properties":{"value":{"type":"string"}},
                "required":["value"]
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        arguments
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::invalid_arguments("value must be a string"))?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        self.risk(arguments)?;
        Ok("Echo a value".into())
    }

    async fn execute(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success(
            arguments["value"].as_str().unwrap_or_default(),
        ))
    }
}

struct ProgressTool;

#[async_trait]
impl Tool for ProgressTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "work".into(),
            description: "Report progress while working".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn risk(&self, _arguments: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, _arguments: &Value) -> Result<String, ToolError> {
        Ok("Work".into())
    }

    async fn execute(
        &self,
        _arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        for step in 0..22 {
            context.progress.report(&format!("step {step}"));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(ToolOutput::success("worked"))
    }
}

struct TimedSink(Mutex<Vec<(tokio::time::Instant, CoreEvent)>>);

#[async_trait]
impl EventSink for TimedSink {
    async fn emit(&self, event: CoreEvent) -> Result<(), AgentError> {
        self.0
            .lock()
            .unwrap()
            .push((tokio::time::Instant::now(), event));
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn tool_progress_is_paced_and_kept_out_of_history() {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([
            AssistantResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "work".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Usage::default(),
            },
            AssistantResponse {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
            },
        ])),
    });
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(ProgressTool)).unwrap();
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(registry),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 3,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let sink = Arc::new(TimedSink(Mutex::new(Vec::new())));
    let mut history = Vec::new();
    runtime
        .run_turn(
            &mut history,
            "go",
            sink.clone(),
            Arc::new(Allow),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = sink.0.lock().unwrap();
    let started = events
        .iter()
        .position(|(_, event)| matches!(event, CoreEvent::ToolStarted { .. }))
        .unwrap();
    let completed = events
        .iter()
        .position(|(_, event)| matches!(event, CoreEvent::ToolCompleted { .. }))
        .unwrap();
    let progress: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, (at, event))| match event {
            CoreEvent::ToolProgress { call_id, text } => Some((index, *at, call_id, text)),
            _ => None,
        })
        .collect();
    // 2.2 seconds of work at two events a second: four ticks, plus at
    // most one final flush.
    assert!((4..=5).contains(&progress.len()), "{}", progress.len());
    for (index, _, call_id, text) in &progress {
        assert!(*index > started && *index < completed);
        assert_eq!(call_id.as_str(), "call-1");
        assert!(text.len() <= MAX_PROGRESS_EVENT_BYTES);
    }
    for pair in progress.windows(2) {
        assert!(pair[1].1 - pair[0].1 >= PROGRESS_INTERVAL);
    }
    let all: Vec<&str> = progress
        .iter()
        .flat_map(|(_, _, _, text)| text.lines())
        .collect();
    assert_eq!(all.first(), Some(&"step 0"));
    // Lines reported within an interval of the last event are dropped
    // rather than breaking the pace; `ToolCompleted` follows at once.
    assert!(all.contains(&"step 19"));
    let stored = serde_json::to_string(&history).unwrap();
    assert!(!stored.contains("step 1"), "progress leaked into history");
}

#[tokio::test]
async fn completes_a_simple_turn() {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([AssistantResponse {
            content: "done".into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: Some(3),
                output_tokens: Some(1),
            },
        }])),
    });
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(ToolRegistry::default()),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 2,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
    let mut history = Vec::new();
    let outcome = runtime
        .run_turn(
            &mut history,
            "hello",
            sink.clone(),
            Arc::new(Allow),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.steps, 1);
    assert_eq!(history.len(), 2);
    assert!(matches!(
        sink.0.lock().unwrap().last(),
        Some(CoreEvent::AssistantCompleted { .. })
    ));
}

#[tokio::test]
async fn repeated_history_trimming_rebuilds_the_note_and_makes_progress() {
    let runtime = AgentRuntime::new(
        Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::new()),
        }),
        Arc::new(ToolRegistry::default()),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 1,
            history_limits: HistoryLimits {
                max_bytes: 4096,
                max_messages: 3,
                note_max_chars: 80,
            },
        },
        PathBuf::from("/tmp"),
    );
    let sink = CollectSink(Mutex::new(Vec::new()));
    let mut history = vec![
        Message::HistoryNote {
            content: "previous trim".into(),
        },
        Message::user("old request"),
        Message::Assistant {
            content: "old answer".into(),
            tool_calls: Vec::new(),
        },
        Message::user("active request"),
    ];
    runtime
        .enforce_history_limits(&mut history, &sink)
        .await
        .unwrap();
    assert!(history.len() <= 3);
    assert!(matches!(history.first(), Some(Message::HistoryNote { .. })));
    assert!(matches!(history.last(), Some(Message::User { .. })));
}

#[tokio::test]
async fn active_turn_over_history_limit_rolls_back() {
    let runtime = AgentRuntime::new(
        Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::new()),
        }),
        Arc::new(ToolRegistry::default()),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 1,
            history_limits: HistoryLimits {
                max_bytes: 16,
                max_messages: 10,
                note_max_chars: 8,
            },
        },
        PathBuf::from("/tmp"),
    );
    let sink = CollectSink(Mutex::new(Vec::new()));
    let mut history = Vec::new();
    let result = runtime
        .run_turn(
            &mut history,
            "too large for the configured history",
            Arc::new(sink),
            Arc::new(Allow),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(AgentError::HistoryLimit(_))));
    assert!(history.is_empty());
}

#[tokio::test]
async fn executes_a_multi_step_tool_loop_and_aggregates_usage() {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([
            AssistantResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"value":"hello"}),
                }],
                usage: Usage {
                    input_tokens: Some(2),
                    output_tokens: Some(1),
                },
            },
            AssistantResponse {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Usage {
                    input_tokens: Some(4),
                    output_tokens: Some(2),
                },
            },
        ])),
    });
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(EchoTool)).unwrap();
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(registry),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 3,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
    let mut history = Vec::new();
    let outcome = runtime
        .run_turn(
            &mut history,
            "start",
            sink,
            Arc::new(Allow),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.usage.input_tokens, Some(6));
    assert_eq!(outcome.usage.output_tokens, Some(3));
    assert!(matches!(
        history.get(2),
        Some(Message::Tool {
            content,
            is_error: false,
            ..
        }) if content == "hello"
    ));
}

#[tokio::test]
async fn denial_is_recorded_as_a_model_visible_tool_failure() {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([
            AssistantResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"value":"blocked"}),
                }],
                usage: Usage::default(),
            },
            AssistantResponse {
                content: "handled".into(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
            },
        ])),
    });
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(EchoTool)).unwrap();
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(registry),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 3,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let mut history = Vec::new();
    let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
    runtime
        .run_turn(
            &mut history,
            "start",
            Arc::clone(&sink) as Arc<dyn EventSink>,
            Arc::new(Deny),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        history.get(2),
        Some(Message::Tool {
            content,
            is_error: true,
            ..
        }) if content == "tool call denied by policy or user"
    ));
    assert_eq!(
        completed_outputs(&sink),
        [ToolOutput::failed(
            ToolFailure::Denied,
            "tool call denied by policy or user"
        )]
    );
}

/// The outputs of every `ToolCompleted` event `sink` collected.
fn completed_outputs(sink: &CollectSink) -> Vec<ToolOutput> {
    sink.0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            CoreEvent::ToolCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn calls_that_cannot_run_say_why() {
    let call = |id: &str, name: &str, arguments: Value| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    };
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([
            AssistantResponse {
                content: String::new(),
                tool_calls: vec![
                    call("call-1", "missing", serde_json::json!({})),
                    call("call-2", "echo", serde_json::json!({"value": 7})),
                ],
                usage: Usage::default(),
            },
            AssistantResponse {
                content: "handled".into(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
            },
        ])),
    });
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(EchoTool)).unwrap();
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(registry),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 3,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
    runtime
        .run_turn(
            &mut Vec::new(),
            "start",
            Arc::clone(&sink) as Arc<dyn EventSink>,
            Arc::new(Allow),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        completed_outputs(&sink),
        [
            ToolOutput::failed(ToolFailure::UnknownTool, "unknown tool: missing"),
            ToolOutput::failed(ToolFailure::InvalidArguments, "value must be a string"),
        ]
    );
}

#[tokio::test]
async fn cancellation_during_approval_rolls_back_the_active_tool_group() {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([AssistantResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-cancel".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"value":"hello"}),
            }],
            usage: Usage::default(),
        }])),
    });
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(EchoTool)).unwrap();
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(registry),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 2,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let before = vec![
        Message::user("previous"),
        Message::Assistant {
            content: "answer".into(),
            tool_calls: Vec::new(),
        },
    ];
    let mut history = before.clone();
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    let entered = Arc::new(Notify::new());
    let wait = Arc::clone(&entered);
    let run = runtime.run_turn(
        &mut history,
        "new turn",
        Arc::new(CollectSink(Mutex::new(Vec::new()))),
        Arc::new(WaitForCancellation { entered }),
        cancellation,
    );
    let cancel_when_waiting = async move {
        wait.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(run, cancel_when_waiting);
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert_eq!(history, before);
}

#[tokio::test]
async fn stops_after_the_configured_maximum_step() {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(VecDeque::from([AssistantResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"value":"one"}),
            }],
            usage: Usage::default(),
        }])),
    });
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(EchoTool)).unwrap();
    let runtime = AgentRuntime::new(
        provider,
        Arc::new(registry),
        Arc::new(BudgetContextPolicy::new(ContextConfig::default()).unwrap()),
        AgentConfig {
            system_prompt: "test".into(),
            max_steps: 1,
            history_limits: HistoryLimits::default(),
        },
        PathBuf::from("/tmp"),
    );
    let result = runtime
        .run_turn(
            &mut Vec::new(),
            "start",
            Arc::new(CollectSink(Mutex::new(Vec::new()))),
            Arc::new(Allow),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(AgentError::StepLimit)));
}
