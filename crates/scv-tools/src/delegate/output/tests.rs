//! Unit tests for `src/delegate/output.rs`.

use super::*;

fn run(format: OutputFormat, stdout: &str, exit: RunExit) -> AgentResult {
    let mut stream = AgentStream::new(format, 64 * 1024);
    // Split mid-line to exercise reassembly across reads.
    let (first, second) = stdout.split_at(stdout.len() / 2);
    stream.push(first.as_bytes());
    stream.push(second.as_bytes());
    stream.finish(exit, None)
}

const OK: RunExit = RunExit::Exited { success: true };

#[test]
fn claude_stream_json_yields_the_result_event() {
    let stdout = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s","tools":["Bash"],"new_field":1}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"big output"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"done: 42","usage":{"input_tokens":120,"output_tokens":7}}"#,
        "\n"
    );
    let result = run(OutputFormat::ClaudeStreamJson, stdout, OK);
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.reply, "done: 42");
    assert_eq!(result.session.as_deref(), Some("s"));
    assert_eq!(
        result.usage,
        Some(AgentUsage {
            input_tokens: 120,
            output_tokens: 7
        })
    );
}

#[test]
fn claude_signed_out_result_is_a_failure_with_its_message() {
    let stdout = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Not logged in · Please run /login"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#,
        "\n"
    );
    let result = run(
        OutputFormat::ClaudeStreamJson,
        stdout,
        RunExit::Exited { success: false },
    );
    assert_eq!(result.status, RunStatus::Failed);
    assert!(result.reply.contains("Not logged in"));
}

#[test]
fn codex_jsonl_yields_the_last_agent_message_and_turn_usage() {
    let stdout = concat!(
        r#"{"type":"thread.started","thread_id":"t"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"ls"}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"ok"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":11257,"cached_input_tokens":0,"output_tokens":5}}"#,
        "\n"
    );
    let result = run(OutputFormat::CodexJsonl, stdout, OK);
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.reply, "ok");
    assert_eq!(result.usage.unwrap().input_tokens, 11257);
    assert_eq!(result.session.as_deref(), Some("t"));
}

#[test]
fn codex_turn_failure_reports_its_error() {
    let stdout = concat!(
        r#"{"type":"thread.started","thread_id":"t"}"#,
        "\n",
        r#"{"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized"}}"#,
        "\n"
    );
    let result = run(
        OutputFormat::CodexJsonl,
        stdout,
        RunExit::Exited { success: false },
    );
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.reply, "unexpected status 401 Unauthorized");
}

#[test]
fn codex_last_message_file_backs_up_a_stream_without_a_message() {
    let mut stream = AgentStream::new(OutputFormat::CodexJsonl, 1024);
    stream.push(b"{\"type\":\"turn.completed\",\"usage\":{}}\n");
    let result = stream.finish(OK, Some("from -o".into()));
    assert_eq!(result.reply, "from -o");
    assert_eq!(result.status, RunStatus::Completed);
}

#[test]
fn pi_json_yields_the_last_assistant_message() {
    let stdout = concat!(
        r#"{"type":"session","id":"p","cwd":"/w"}"#,
        "\n",
        r#"{"type":"message_end","message":{"role":"system","content":"You are pi"}}"#,
        "\n",
        r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":0,"output":0},"stopReason":"error","errorMessage":"overloaded"}}"#,
        "\n",
        r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"ok","textSignature":"x"}],"usage":{"input":900,"output":3}}}"#,
        "\n",
        r#"{"type":"agent_end","messages":[]}"#,
        "\n"
    );
    let result = run(OutputFormat::PiJson, stdout, OK);
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.reply, "ok");
    assert_eq!(result.usage.unwrap().input_tokens, 900);
}

#[test]
fn text_output_is_the_reply_and_structured_garbage_falls_back_to_text() {
    let result = run(OutputFormat::Text, "hello\nworld\n", OK);
    assert_eq!(result.reply, "hello\nworld");
    let result = run(
        OutputFormat::ClaudeStreamJson,
        "Error: unknown option --verbose\n",
        RunExit::Exited { success: false },
    );
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.reply, "Error: unknown option --verbose");
}

#[test]
fn oversized_event_lines_are_skipped_without_losing_later_events() {
    let mut stream = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024);
    stream.push(b"{\"type\":\"user\",\"content\":\"");
    stream.push(&vec![b'x'; MAX_EVENT_LINE_BYTES + 10]);
    stream.push(b"\"}\n{\"type\":\"result\",\"is_error\":false,\"result\":\"fine\"}\n");
    let result = stream.finish(OK, None);
    assert_eq!(result.reply, "fine");
}

#[test]
fn timeouts_and_kills_override_the_stream() {
    let stdout = "{\"type\":\"result\",\"is_error\":false,\"result\":\"partial\"}\n";
    assert_eq!(
        run(OutputFormat::ClaudeStreamJson, stdout, RunExit::TimedOut).status,
        RunStatus::Timeout
    );
    assert_eq!(
        run(OutputFormat::ClaudeStreamJson, stdout, RunExit::Killed).status,
        RunStatus::Cancelled
    );
}

#[test]
fn session_ids_are_captured_only_when_safe_to_pass_back() {
    let pi = run(
        OutputFormat::PiJson,
        r#"{"type":"session","id":"01a0cd6c-a3a9-77b3","cwd":"/w"}"#,
        RunExit::Exited { success: true },
    );
    assert_eq!(pi.session.as_deref(), Some("01a0cd6c-a3a9-77b3"));
    for unsafe_id in ["--help", "../x", "a b", "", "x;rm"] {
        let line = serde_json::json!({"type":"thread.started","thread_id":unsafe_id}).to_string();
        let codex = run(
            OutputFormat::CodexJsonl,
            &line,
            RunExit::Exited { success: true },
        );
        assert_eq!(codex.session, None, "{unsafe_id:?}");
    }
    // Text output never reports a session.
    let text = run(
        OutputFormat::Text,
        r#"{"type":"thread.started","thread_id":"t"}"#,
        RunExit::Exited { success: true },
    );
    assert_eq!(text.session, None);
}

#[test]
fn replies_are_bounded_on_character_boundaries() {
    let result = AgentResult {
        status: RunStatus::Completed,
        reply: "ééé".into(),
        error: None,
        usage: None,
        truncated: false,
        session: None,
    };
    let (json, truncated) = result.to_json("claude", None, Some(0), "", 3);
    assert!(truncated);
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["reply"], "é");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["usage"], Value::Null);
}

#[test]
fn a_claude_refusal_is_declined_whatever_its_reply_says() {
    let stdout = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I can't help bypass authentication or a 403."}],"stop_reason":"refusal"}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"I can't help bypass authentication or a 403."}"#,
        "\n"
    );
    let result = run(OutputFormat::ClaudeStreamJson, stdout, OK);
    assert_eq!(result.status, RunStatus::Declined);
    assert_eq!(result.error, None);
    let (json, _) = result.to_json("claude", None, Some(0), "", 4096);
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["status"], "declined");
    assert_eq!(value["note"], DECLINED_NOTE);
    assert!(value.get("error").is_none(), "{value}");
    // The same text from a run that ended normally is just a reply.
    let plain = run(
        OutputFormat::ClaudeStreamJson,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Fixed the 403 in authentication."}"#,
        OK,
    );
    assert_eq!(plain.status, RunStatus::Completed);
}

#[test]
fn failures_carry_the_reported_error_apart_from_the_reply() {
    // Claude Code's own error in a failed result.
    let claude = run(
        OutputFormat::ClaudeStreamJson,
        r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 429 rate limited"}"#,
        RunExit::Exited { success: false },
    );
    assert_eq!(claude.error.as_deref(), Some("API Error: 429 rate limited"));
    // Codex: the reply stays the agent's message, the error is its own.
    let codex = run(
        OutputFormat::CodexJsonl,
        concat!(
            r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"see the authentication docs"}}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"stream disconnected"}}"#,
            "\n"
        ),
        RunExit::Exited { success: false },
    );
    assert_eq!(codex.error.as_deref(), Some("stream disconnected"));
    // A failed plain-text CLI's closing output is its error message.
    let text = run(
        OutputFormat::Text,
        "Error: model not found\n",
        RunExit::Exited { success: false },
    );
    assert_eq!(text.error.as_deref(), Some("Error: model not found"));
    // Successful runs carry none, whatever their reply mentions.
    let ok = run(OutputFormat::Text, "fixed the 403\n", OK);
    assert_eq!(ok.error, None);
}

#[test]
fn stderr_tail_keeps_the_end() {
    let mut tail = TailBuffer::new(4);
    tail.push(b"abcdef");
    tail.push(b"gh");
    assert_eq!(tail.text(), "efgh");
}
