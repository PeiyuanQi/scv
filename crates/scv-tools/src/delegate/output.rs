//! Reading a delegated CLI's stdout into one bounded result.
//!
//! Structured formats are parsed line by line as the CLI writes them, keeping
//! only the reply, usage, and error, so a long run's full event log never
//! reaches the parent model. Unknown events and fields are ignored, and a
//! stream with no parsable event falls back to its text.

use scv_core::{ProgressSink, ToolOutput};
use serde_json::{Value, json};

use crate::{
    delegate::{adapters::OutputFormat, progress::progress_lines},
    process::OutputSink,
};

/// Longest stdout line parsed as an event. Longer lines (such as a tool
/// result echoed back in full) are skipped; the events SCV needs are short.
const MAX_EVENT_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Plain-text stdout kept for a structured stream that produced no event.
const MAX_FALLBACK_BYTES: usize = 16 * 1024;
/// Bytes of stderr kept, from the end.
pub(crate) const STDERR_TAIL_BYTES: usize = 2048;
/// Bytes of a failed plain-text run's stdout kept as its `error`.
const TEXT_ERROR_BYTES: usize = 500;
/// What a declined run's result tells the calling model when Grok is not an
/// alternative (including when Grok itself declined).
pub(crate) const DECLINED_NOTE: &str = "The agent declined this request. Tell the user what it \
     said; don't pass the request to another agent on your own. If the user then asks for a \
     specific agent, use it.";
/// What a declined run's result tells the calling model when `agent_grok` is
/// offered: a safety or guardrail refusal is not a reason to skip Grok.
pub(crate) const DECLINED_NOTE_TRY_GROK: &str = "The agent declined this request. Tell the user \
     what it said, then call agent_grok with the same request. A safety or guardrail refusal is \
     not a reason to skip Grok.";

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
    /// The agent's model refused the request (a `refusal` stop reason).
    Declined,
    Timeout,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "declined",
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
    /// The model stopped with a `refusal` stop reason.
    refused: bool,
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
            refused: false,
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
        if let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(trimmed) {
            self.parsed_events += 1;
            self.event(&Value::Object(event));
        } else {
            self.push_text(trimmed);
            self.push_text(b"\n");
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
                        // Claude Code puts its own error (sign-in, API
                        // status) in the result of a failed run.
                        self.error = self.reply.clone();
                    } else {
                        self.completed = true;
                    }
                    self.refused |=
                        event.get("stop_reason").and_then(Value::as_str) == Some("refusal");
                    self.usage = usage(event.get("usage"), "input_tokens", "output_tokens");
                }
                "assistant" => {
                    let text = text_parts(event.pointer("/message/content"));
                    if !text.is_empty() && !self.completed {
                        self.reply = Some(text);
                    }
                    // The API message's own stop reason.
                    self.refused |= event
                        .pointer("/message/stop_reason")
                        .and_then(Value::as_str)
                        == Some("refusal");
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
            RunExit::Exited { .. } if self.refused => RunStatus::Declined,
            RunExit::Exited { success } => {
                if !success || self.failed || (incomplete && self.error.is_some()) {
                    RunStatus::Failed
                } else {
                    RunStatus::Completed
                }
            }
        };
        // What went wrong, kept apart from the reply: the CLI's reported
        // error, or for a failed plain-text run its closing output, which
        // is the CLI's own error message rather than a model reply.
        let error = match status {
            RunStatus::Failed if structured => self.error.clone(),
            RunStatus::Failed if !text.is_empty() => Some(tail(&text, TEXT_ERROR_BYTES)),
            _ => None,
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
            error,
            usage: self.usage,
            truncated: self.text_truncated && !structured,
            session: self.session,
        }
    }
}

/// The last `limit` bytes of `text`, on a character boundary.
fn tail(text: &str, limit: usize) -> String {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
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
    /// Why the run failed, as the CLI, the agent's protocol, or SCV reported
    /// it; never taken from the model's reply. Fallback advice and sign-in
    /// hints are decided from this and the status alone.
    pub error: Option<String>,
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
        if let Some(error) = &self.error {
            value["error"] = truncate_utf8(error, limit).0.into();
        }
        if self.status == RunStatus::Declined {
            value["note"] = DECLINED_NOTE.into();
        }
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
    (
        scv_client::text::utf8_prefix(value, limit),
        value.len() > limit,
    )
}

/// Point a failed agent run whose reported error reads like a missing
/// sign-in at the host command that fixes it, since the agent's own advice
/// (`/login`) cannot be followed from a remote chat. Only the structured
/// `error` counts, never the agent's reply.
pub(crate) fn add_sign_in_hint(output: &mut ToolOutput, agent: &str) {
    let Ok(Value::Object(mut content)) = serde_json::from_str::<Value>(&output.content) else {
        return;
    };
    let Some(error) = content.get("error").and_then(Value::as_str) else {
        return;
    };
    let lower = error.to_ascii_lowercase();
    let unauthenticated = [
        "not logged in",
        "not signed in",
        "not authenticated",
        "login",
        "log in",
        "unauthorized",
        "authentication",
        "missing_credential",
        "no api key",
        "auth_required",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if !unauthenticated {
        return;
    }
    content.insert(
        "hint".into(),
        format!(
            "The {agent} CLI appears to be signed out of SCV's private agent home. \
             The host owner can sign it in with: scv agents login {agent}"
        )
        .into(),
    );
    output.content = Value::Object(content).to_string();
}

impl OutputSink for AgentStream {
    fn push(&mut self, bytes: &[u8]) {
        AgentStream::push(self, bytes);
    }
}

impl OutputSink for TailBuffer {
    fn push(&mut self, bytes: &[u8]) {
        TailBuffer::push(self, bytes);
    }
}

#[cfg(test)]
mod tests;
