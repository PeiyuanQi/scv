//! Reading a delegated CLI's stdout into one bounded result.
//!
//! Structured formats are parsed line by line as the CLI writes them, keeping
//! only the reply, usage, and error, so a long run's full event log never
//! reaches the parent model. Unknown events and fields are ignored, and a
//! stream with no parsable event falls back to its text.

use scv_core::ProgressSink;
use serde_json::{Value, json};

use crate::{adapters::OutputFormat, agent_progress::progress_lines};

/// Longest stdout line parsed as an event. Longer lines (such as a tool
/// result echoed back in full) are skipped; the events SCV needs are short.
const MAX_EVENT_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Plain-text stdout kept for a structured stream that produced no event.
const MAX_FALLBACK_BYTES: usize = 16 * 1024;
/// Bytes of stderr kept, from the end.
pub(crate) const STDERR_TAIL_BYTES: usize = 2048;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The final state of one delegated run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Completed,
    Failed,
    Timeout,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Incremental reader for a delegated CLI's stdout.
#[derive(Debug)]
pub(crate) struct AgentStream {
    format: OutputFormat,
    line: Vec<u8>,
    line_overflowed: bool,
    /// Text-format stdout, or a structured stream's non-JSON lines.
    text: Vec<u8>,
    text_limit: usize,
    text_truncated: bool,
    parsed_events: usize,
    reply: Option<String>,
    error: Option<String>,
    failed: bool,
    completed: bool,
    usage: Option<AgentUsage>,
    /// The CLI's own session ID, for continuing the conversation.
    session: Option<String>,
    /// Where status lines from structured events go while the run lasts.
    progress: ProgressSink,
}

impl AgentStream {
    pub(crate) fn new(format: OutputFormat, output_limit: usize) -> Self {
        Self {
            format,
            line: Vec::new(),
            line_overflowed: false,
            text: Vec::new(),
            text_limit: if format == OutputFormat::Text {
                output_limit
            } else {
                output_limit.min(MAX_FALLBACK_BYTES)
            },
            text_truncated: false,
            parsed_events: 0,
            reply: None,
            error: None,
            failed: false,
            completed: false,
            usage: None,
            session: None,
            progress: ProgressSink::default(),
        }
    }

    /// Report what the agent does (commands, files, searches, tool calls)
    /// to `progress` as its events arrive.
    pub(crate) fn with_progress(mut self, progress: ProgressSink) -> Self {
        self.progress = progress;
        self
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.format == OutputFormat::Text {
            self.push_text(bytes);
            return;
        }
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            let (content, ends_line) = match chunk.strip_suffix(b"\n") {
                Some(content) => (content, true),
                None => (chunk, false),
            };
            if !self.line_overflowed {
                if self.line.len() + content.len() > MAX_EVENT_LINE_BYTES {
                    self.line.clear();
                    self.line_overflowed = true;
                } else {
                    self.line.extend_from_slice(content);
                }
            }
            if ends_line {
                self.end_line();
            }
        }
    }

    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        if std::mem::take(&mut self.line_overflowed) {
            return;
        }
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() {
            return;
        }
        match serde_json::from_slice::<Value>(trimmed) {
            Ok(Value::Object(event)) => {
                self.parsed_events += 1;
                self.event(&Value::Object(event));
            }
            _ => {
                self.push_text(trimmed);
                self.push_text(b"\n");
            }
        }
    }

    fn push_text(&mut self, bytes: &[u8]) {
        let remaining = self.text_limit.saturating_sub(self.text.len());
        self.text
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.text_truncated |= bytes.len() > remaining;
    }

    fn event(&mut self, event: &Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if self.progress.is_enabled() {
            for line in progress_lines(self.format, kind, event) {
                self.progress.report(&line);
            }
        }
        let session = match (self.format, kind) {
            (OutputFormat::ClaudeStreamJson, "system" | "result") => event.get("session_id"),
            (OutputFormat::CodexJsonl, "thread.started") => event.get("thread_id"),
            (OutputFormat::PiJson, "session") => event.get("id"),
            _ => None,
        };
        if let Some(session) = session
            .and_then(Value::as_str)
            .filter(|id| valid_session_id(id))
        {
            self.session = Some(session.to_owned());
        }
        match self.format {
            OutputFormat::Text => {}
            OutputFormat::ClaudeStreamJson => match kind {
                "result" => {
                    if let Some(result) = event.get("result").and_then(Value::as_str) {
                        self.reply = Some(result.to_owned());
                    }
                    let error_subtype = event
                        .get("subtype")
                        .and_then(Value::as_str)
                        .is_some_and(|subtype| subtype.starts_with("error"));
                    if event.get("is_error").and_then(Value::as_bool) == Some(true) || error_subtype
                    {
                        self.failed = true;
                    } else {
                        self.completed = true;
                    }
                    self.usage = usage(event.get("usage"), "input_tokens", "output_tokens");
                }
                "assistant" => {
                    let text = text_parts(event.pointer("/message/content"));
                    if !text.is_empty() && !self.completed {
                        self.reply = Some(text);
                    }
                }
                _ => {}
            },
            OutputFormat::CodexJsonl => match kind {
                "item.completed" => {
                    if event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
                        && let Some(text) = event.pointer("/item/text").and_then(Value::as_str)
                    {
                        self.reply = Some(text.to_owned());
                    }
                }
                "turn.completed" => {
                    self.completed = true;
                    let turn = usage(event.get("usage"), "input_tokens", "output_tokens");
                    self.usage = add_usage(self.usage, turn);
                }
                "turn.failed" => {
                    self.failed = true;
                    if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
                        self.error = Some(message.to_owned());
                    }
                }
                // Codex also reports recoverable problems (reconnects) this
                // way; only a run without `turn.completed` treats it as final.
                "error" => {
                    if let Some(message) = event.get("message").and_then(Value::as_str) {
                        self.error = Some(message.to_owned());
                    }
                }
                _ => {}
            },
            OutputFormat::PiJson => {
                if kind == "message_end"
                    && event.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
                {
                    let message = &event["message"];
                    let text = text_parts(message.get("content"));
                    if !text.is_empty() {
                        self.reply = Some(text);
                        self.completed = true;
                    }
                    let turn = usage(message.get("usage"), "input", "output");
                    self.usage = add_usage(self.usage, turn);
                    if message.get("stopReason").and_then(Value::as_str) == Some("error")
                        && let Some(error) = message.get("errorMessage").and_then(Value::as_str)
                    {
                        self.error = Some(error.to_owned());
                    }
                }
            }
        }
    }

    /// Settle the run. `fallback_reply` is a final message the CLI wrote
    /// elsewhere (Codex `-o`), used when the stream carried none.
    pub(crate) fn finish(mut self, exit: RunExit, fallback_reply: Option<String>) -> AgentResult {
        if !self.line.is_empty() || self.line_overflowed {
            self.end_line();
        }
        let text = String::from_utf8_lossy(&self.text).trim_end().to_owned();
        let structured = self.format != OutputFormat::Text && self.parsed_events > 0;
        let mut reply = if structured {
            self.reply.or(fallback_reply).or_else(|| self.error.clone())
        } else {
            fallback_reply.or_else(|| (!text.is_empty()).then(|| text.clone()))
        }
        .unwrap_or_default();
        let incomplete = structured && !self.completed;
        let status = match exit {
            RunExit::TimedOut => RunStatus::Timeout,
            RunExit::Killed => RunStatus::Cancelled,
            RunExit::Exited { success } => {
                if !success || self.failed || (incomplete && self.error.is_some()) {
                    RunStatus::Failed
                } else {
                    RunStatus::Completed
                }
            }
        };
        if status != RunStatus::Completed
            && let Some(error) = &self.error
            && !reply.contains(error.as_str())
        {
            if reply.is_empty() {
                reply = error.clone();
            } else {
                reply = format!("{reply}\n{error}");
            }
        }
        AgentResult {
            status,
            reply,
            usage: self.usage,
            truncated: self.text_truncated && !structured,
            session: self.session,
        }
    }
}

/// How the process ended, as far as the parser needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunExit {
    Exited {
        success: bool,
    },
    TimedOut,
    /// Stopped by `scv agents kill`.
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentResult {
    pub status: RunStatus,
    pub reply: String,
    pub usage: Option<AgentUsage>,
    pub truncated: bool,
    /// The session ID the CLI reported, if any.
    pub session: Option<String>,
}

impl AgentResult {
    /// The tool result: the reply bounded to `limit` bytes, never the log.
    pub(crate) fn to_json(
        &self,
        agent: &str,
        conversation: Option<(&str, u32)>,
        exit_code: Option<i32>,
        stderr_tail: &str,
        limit: usize,
    ) -> (String, bool) {
        let (reply, cut) = truncate_utf8(&self.reply, limit);
        let truncated = self.truncated || cut;
        let mut value = json!({
            "agent": agent,
            "status": self.status.as_str(),
            "reply": reply,
            "usage": self.usage.map(|usage| json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            })),
            "exit_code": exit_code,
            "stderr_tail": stderr_tail,
            "truncated": truncated,
        });
        if let Some((handle, turn)) = conversation {
            value["session"] = handle.into();
            value["turn"] = turn.into();
        }
        (value.to_string(), truncated)
    }
}

/// Whether a CLI-reported session ID is safe to pass back as one argument:
/// it must not read as a flag or carry path or shell syntax.
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The last bytes written to a stream.
#[derive(Debug)]
pub(crate) struct TailBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl TailBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > self.limit * 2 {
            let excess = self.bytes.len() - self.limit;
            self.bytes.drain(..excess);
        }
    }

    pub(crate) fn text(&self) -> String {
        let start = self.bytes.len().saturating_sub(self.limit);
        String::from_utf8_lossy(&self.bytes[start..])
            .trim()
            .to_owned()
    }
}

fn text_parts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn usage(value: Option<&Value>, input: &str, output: &str) -> Option<AgentUsage> {
    let value = value?;
    let input_tokens = value.get(input).and_then(Value::as_u64);
    let output_tokens = value.get(output).and_then(Value::as_u64);
    (input_tokens.is_some() || output_tokens.is_some()).then(|| AgentUsage {
        input_tokens: input_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
    })
}

fn add_usage(total: Option<AgentUsage>, turn: Option<AgentUsage>) -> Option<AgentUsage> {
    match (total, turn) {
        (Some(total), Some(turn)) => Some(AgentUsage {
            input_tokens: total.input_tokens.saturating_add(turn.input_tokens),
            output_tokens: total.output_tokens.saturating_add(turn.output_tokens),
        }),
        (total, turn) => total.or(turn),
    }
}

pub(crate) fn truncate_utf8(value: &str, limit: usize) -> (&str, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

#[cfg(test)]
mod tests {
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
            let line =
                serde_json::json!({"type":"thread.started","thread_id":unsafe_id}).to_string();
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
    fn stderr_tail_keeps_the_end() {
        let mut tail = TailBuffer::new(4);
        tail.push(b"abcdef");
        tail.push(b"gh");
        assert_eq!(tail.text(), "efgh");
    }
}
