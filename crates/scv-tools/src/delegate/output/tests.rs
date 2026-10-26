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
fn claude_init_source_must_match_the_pinned_billing_identity() {
    let mut stream = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeApiKey);
    stream.push(
        br#"{"type":"system","subtype":"init","apiKeySource":"none"}
{"type":"result","is_error":false,"result":"done"}
"#,
    );
    let result = stream.finish(OK, None);
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("claude reported credential source none, not the Claude API key; the run was stopped")
    );
    assert_eq!(result.reply, result.error.clone().unwrap());

    let mut stream = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeSubscription);
    stream.push(
        br#"{"type":"system","subtype":"init","apiKeySource":"apiKeyHelper"}
{"type":"assistant","message":{"content":[{"type":"text","text":"billed"}]}}
{"type":"result","is_error":false,"result":"billed"}
"#,
    );
    let result = stream.finish(OK, None);
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(
            "claude-ai reported credential source apiKeyHelper, not the Claude subscription; the run was stopped"
        )
    );
    assert_eq!(result.reply, result.error.clone().unwrap());
}

#[test]
fn a_claude_run_that_completes_without_an_init_event_fails() {
    let mut stream = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeApiKey);
    stream.push(br#"{"type":"result","is_error":false,"result":"done"}"#);
    let result = stream.finish(OK, None);
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(
            "claude reported credential source missing, not the Claude API key; the run was stopped"
        )
    );
    assert_eq!(result.reply, result.error.clone().unwrap());

    let mut stream = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeApiKey);
    stream
        .push(br#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#);
    let result = stream.finish(OK, None);
    assert_eq!(result.status, RunStatus::Failed);
    assert!(!result.reply.contains("partial"));

    let mut signed_out = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeApiKey);
    signed_out.push(br#"{"type":"result","is_error":true,"result":"Not logged in"}"#);
    let result = signed_out.finish(RunExit::Exited { success: false }, None);
    assert_eq!(result.status, RunStatus::Failed);
    assert!(result.reply.contains("Not logged in"));
    assert!(!result.error.unwrap().contains("credential source"));

    let mut timed_out = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeSubscription);
    timed_out
        .push(br#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#);
    assert_eq!(
        timed_out.finish(RunExit::TimedOut, None).status,
        RunStatus::Timeout
    );

    let mut matched = AgentStream::new(OutputFormat::ClaudeStreamJson, 1024)
        .with_auth(crate::adapters::AgentAuth::ClaudeApiKey);
    matched.push(
        br#"{"type":"system","subtype":"init","apiKeySource":"apiKeyHelper"}
{"type":"result","is_error":false,"result":"ok"}
"#,
    );
    let result = matched.finish(OK, None);
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.reply, "ok");
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

#[test]
fn gemini_stream_json_assembles_deltas_and_reads_its_result() {
    let stdout = concat!(
        r#"{"type":"init","timestamp":"t","session_id":"9c1e0f2a-0000-4000-8000-000000000001","model":"gemini-2.5-pro"}"#,
        "\n",
        r#"{"type":"message","role":"user","content":"fix it"}"#,
        "\n",
        r#"{"type":"message","role":"assistant","content":"Looking","delta":true}"#,
        "\n",
        r#"{"type":"tool_use","tool_name":"run_shell_command","tool_id":"t1","parameters":{"command":"cargo test"}}"#,
        "\n",
        r#"{"type":"tool_result","tool_id":"t1","status":"success","output":"ok"}"#,
        "\n",
        r#"{"type":"message","role":"assistant","content":" done.","delta":true}"#,
        "\n",
        r#"{"type":"result","status":"success","stats":{"total_tokens":30,"input_tokens":20,"output_tokens":10}}"#,
        "\n"
    );
    let result = run(OutputFormat::GeminiStreamJson, stdout, OK);
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.reply, "Looking done.");
    assert_eq!(
        result.session.as_deref(),
        Some("9c1e0f2a-0000-4000-8000-000000000001")
    );
    assert_eq!(
        result.usage,
        Some(AgentUsage {
            input_tokens: 20,
            output_tokens: 10
        })
    );
}

#[test]
fn a_gemini_error_result_fails_with_its_message_and_its_json_output_reads_too() {
    let failed = concat!(
        r#"{"type":"message","role":"assistant","content":"Trying","delta":true}"#,
        "\n",
        r#"{"type":"result","status":"error","error":{"type":"auth","message":"Please set an Auth method"}}"#,
        "\n"
    );
    let result = run(
        OutputFormat::GeminiStreamJson,
        failed,
        RunExit::Exited { success: false },
    );
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("Please set an Auth method"));
    let whole = "{\"response\":\"All set.\",\"stats\":{}}\n";
    let result = run(OutputFormat::GeminiStreamJson, whole, OK);
    assert_eq!(
        (result.status, result.reply.as_str()),
        (RunStatus::Completed, "All set.")
    );
}

#[test]
fn cursor_and_amp_read_like_claude_code_without_its_billing_pin() {
    let stdout = concat!(
        r#"{"type":"system","subtype":"init","session_id":"c0ffee00-0000-4000-8000-000000000002","model":"sonnet-4.5"}"#,
        "\n",
        r#"{"type":"tool_call","subtype":"started","call_id":"1","tool_call":{"shellToolCall":{"args":{"command":"ls"}}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Listed."}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Listed.","session_id":"c0ffee00-0000-4000-8000-000000000002"}"#,
        "\n"
    );
    for format in [OutputFormat::CursorStreamJson, OutputFormat::AmpStreamJson] {
        let result = run(format, stdout, OK);
        assert_eq!(result.status, RunStatus::Completed, "{format:?}");
        assert_eq!(result.reply, "Listed.");
        assert_eq!(
            result.session.as_deref(),
            Some("c0ffee00-0000-4000-8000-000000000002")
        );
    }
}

#[test]
fn a_streams_message_text_and_tool_lines_reach_its_activity_log() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let activity = crate::delegate::activity::RunActivity::open(
        &runs,
        crate::delegate::records::ProcessIdentity::current().unwrap(),
        crate::delegate::activity::RunHeader {
            handle: "codex-00c0de".into(),
            agent: "codex".into(),
            session: "s".into(),
            transport: crate::delegate::activity::RunTransport::Cli,
            cwd: "/w".into(),
            conversation: None,
            streams: true,
        },
    );
    let mut stream =
        AgentStream::new(OutputFormat::ClaudeStreamJson, 64 * 1024).with_activity(activity);
    stream.push(
        concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I'll look.\nFirst the tests."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            "\n"
        )
        .as_bytes(),
    );
    let page = crate::delegate::activity::read_page(&runs, "codex-00c0de", 0, 100).unwrap();
    let lines: Vec<(scv_protocol::RunEventKind, &str)> = page
        .events
        .iter()
        .map(|event| (event.kind, event.text.as_str()))
        .collect();
    use scv_protocol::RunEventKind::{RunStarted, Text, Tool};
    assert_eq!(
        lines,
        [
            (RunStarted, "codex over cli in /w"),
            (Tool, "$ cargo test"),
            (Text, "I'll look."),
            (Text, "First the tests."),
        ]
    );
    // Plain text is logged as it prints, whole characters only.
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let activity = crate::delegate::activity::RunActivity::open(
        &runs,
        crate::delegate::records::ProcessIdentity::current().unwrap(),
        crate::delegate::activity::RunHeader {
            handle: "grok-00c0de".into(),
            agent: "grok".into(),
            session: "s".into(),
            transport: crate::delegate::activity::RunTransport::Cli,
            cwd: "/w".into(),
            conversation: None,
            streams: false,
        },
    );
    let mut stream = AgentStream::new(OutputFormat::Text, 1024).with_activity(activity);
    let text = "héllo\nwörld\n".as_bytes();
    for byte in text {
        stream.push(std::slice::from_ref(byte));
    }
    let page = crate::delegate::activity::read_page(&runs, "grok-00c0de", 0, 100).unwrap();
    let texts: Vec<&str> = page
        .events
        .iter()
        .skip(1)
        .map(|event| event.text.as_str())
        .collect();
    assert_eq!(texts, ["héllo", "wörld"]);
}
