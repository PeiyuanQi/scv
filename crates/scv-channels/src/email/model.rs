//! One triage turn on the daemon: a fresh, tool-free session with a fixed
//! system prompt, one message, one answer, then the session closes.
//!
//! The server builds no tools for a `no_tools` session, so a model reading
//! mail has nothing to call. As defence in depth, any tool or approval
//! event ends the turn at once and it counts as failed; nothing of the
//! session outlives the turn.

use anyhow::{Context, Result, bail};
use scv_client::Connection;
use scv_protocol::{ClientMessage, Frame, FrameDecoder, Overflow, PROTOCOL_VERSION};
use serde_json::Value;
use std::path::Path;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use super::triage::MAX_ANSWER_BYTES;

/// Largest frame read from the daemon.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// A turn's answer and what it cost, when the provider said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    pub(crate) answer: String,
    pub(crate) tokens: Option<u64>,
}

/// Why a turn failed.
#[derive(Debug)]
pub(crate) enum TurnError {
    /// The session offered a tool or asked for approval, which a mail
    /// session must never do.
    ToolEvent,
    /// Anything else: the daemon, the provider, or the connection.
    Failed(anyhow::Error),
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolEvent => formatter.write_str("the triage session produced a tool event"),
            Self::Failed(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl From<anyhow::Error> for TurnError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

/// Run one turn of `prompt` in a new tool-free session in `cwd`, whose
/// whole system prompt is `frame`, on `model` or the daemon's default.
pub(crate) async fn turn(
    socket: &Path,
    cwd: &Path,
    model: Option<&str>,
    frame: &str,
    prompt: &str,
) -> std::result::Result<Turn, TurnError> {
    let stream = UnixStream::connect(socket)
        .await
        .context("the SCV daemon is not reachable")?;
    let (reader, writer) = stream.into_split();
    let mut connection = Connection::new(
        BufReader::new(reader),
        writer,
        FrameDecoder::new(MAX_FRAME_BYTES, Overflow::Stop),
    );
    connection
        .send(&ClientMessage::initialize("mail-init", "scv-mail"))
        .await
        .context("could not start a triage session")?;
    let hello = next(&mut connection).await?;
    if hello["type"] != "initialized" || hello["protocol_version"] != PROTOCOL_VERSION {
        return Err(TurnError::Failed(anyhow::anyhow!(
            "the daemon did not accept the triage client"
        )));
    }
    connection
        .send(&ClientMessage::SessionStart {
            request_id: "mail-session".into(),
            cwd: cwd.display().to_string(),
            provider: None,
            model: model.map(str::to_owned),
            base_url: None,
            no_tools: Some(true),
            delegation_depth: None,
            channel: None,
            auto_approve: Some(false),
            chat: None,
            system_prompt: Some(frame.to_owned()),
        })
        .await
        .context("could not start a triage session")?;
    let session = loop {
        let event = next(&mut connection).await?;
        refuse_tools(&event)?;
        match event["type"].as_str() {
            Some("session.started") => {
                break event["session_id"].as_str().unwrap_or_default().to_owned();
            }
            Some("error") => {
                return Err(TurnError::Failed(anyhow::anyhow!(
                    "the daemon refused the triage session"
                )));
            }
            _ => {}
        }
    };
    connection
        .send(&ClientMessage::TurnStart {
            request_id: "mail-turn".into(),
            session_id: session,
            prompt: prompt.to_owned(),
            attachments: Vec::new(),
        })
        .await
        .context("could not start the triage turn")?;
    let mut answer = String::new();
    loop {
        let event = next(&mut connection).await?;
        refuse_tools(&event)?;
        match event["type"].as_str() {
            Some("assistant.delta") => {
                append(&mut answer, event["content"].as_str().unwrap_or_default());
            }
            Some("assistant.completed") => {
                answer.clear();
                append(&mut answer, event["content"].as_str().unwrap_or_default());
            }
            Some("turn.completed") => {
                let usage = &event["usage"];
                let tokens = match (
                    usage["input_tokens"].as_u64(),
                    usage["output_tokens"].as_u64(),
                ) {
                    (None, None) => None,
                    (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
                };
                return Ok(Turn { answer, tokens });
            }
            Some("turn.failed" | "turn.cancelled" | "error") => {
                // The daemon's message may quote the prompt; it stays here.
                return Err(TurnError::Failed(anyhow::anyhow!("the triage turn failed")));
            }
            _ => {}
        }
    }
}

/// Q2: a mail session that shows any sign of tools is ended at once.
fn refuse_tools(event: &Value) -> std::result::Result<(), TurnError> {
    let kind = event["type"].as_str().unwrap_or_default();
    let started_by_server = kind == "turn.started" && !event["origin"].is_null();
    if kind.starts_with("tool.") || kind == "approval.requested" || started_by_server {
        return Err(TurnError::ToolEvent);
    }
    Ok(())
}

async fn next(
    connection: &mut Connection<
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
    >,
) -> Result<Value> {
    match connection.read().await? {
        Frame::Line(line) => serde_json::from_slice(&line).context("unreadable daemon event"),
        Frame::TooLarge => bail!("a daemon event exceeded the limit"),
        Frame::End | Frame::Truncated(_) => bail!("the daemon closed the triage session"),
    }
}

fn append(answer: &mut String, content: &str) {
    let room = MAX_ANSWER_BYTES.saturating_sub(answer.len());
    answer.push_str(scv_client::text::utf8_prefix(content, room));
}

#[cfg(test)]
mod tests;
