//! Unit tests for `src/context.rs`.

use super::*;
use crate::ToolCall;

#[test]
fn context_keeps_tool_groups_together() {
    let policy = BudgetContextPolicy::new(ContextConfig {
        max_tokens: 120,
        reserve_output_tokens: 10,
        safety_margin_tokens: 10,
        bytes_per_token: 3,
        summary_max_chars: 120,
    })
    .unwrap();
    let history = vec![
        Message::user("old request ".repeat(20)),
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"a"}),
            }],
        },
        Message::Tool {
            call_id: "1".into(),
            name: "read".into(),
            content: "result".into(),
            is_error: false,
        },
        Message::user("new"),
    ];
    let selection = policy.select(&history, "system", &[]).unwrap();
    assert!(selection.removed_messages > 0);
    assert_eq!(
        selection.removed_messages,
        history.len() - (selection.messages.len() - 1)
    );
    assert!(matches!(
        selection.messages.last(),
        Some(Message::User { .. })
    ));
    assert!(
        !selection
            .messages
            .iter()
            .any(|message| matches!(message, Message::Tool { call_id, .. } if call_id == "1"))
    );
}
