//! Reading a Responses event stream: SSE framing, and [`StreamState`], which
//! assembles the events into one assistant response.

use std::collections::BTreeMap;

use scv_core::{AssistantResponse, ProviderError, ProviderErrorKind, ToolCall, Usage};
use serde_json::{Value, json};

use crate::{
    ProviderLimits,
    wire::{ErrorDetails, ResponseEvent},
};

/// Why an event ends the attempt.
pub(crate) enum EventError {
    /// A limit or malformed output; repeating the request will not help.
    Fatal(ProviderError),
    /// The provider reported an error, described by `details`.
    Provider {
        context: &'static str,
        details: ErrorDetails,
    },
    /// The provider ended the response incomplete, for this reason (its own
    /// text, still to be sanitized).
    Incomplete(String),
}

impl From<ProviderError> for EventError {
    fn from(error: ProviderError) -> Self {
        Self::Fatal(error)
    }
}

/// What one response stream has produced so far.
#[derive(Debug, Default)]
pub(crate) struct StreamState {
    content: String,
    /// Tool calls by output index, filled in as their parts arrive.
    calls: BTreeMap<usize, PartialToolCall>,
    usage: Usage,
    citations: Vec<Citation>,
    /// The provider said the response is complete.
    pub(crate) done: bool,
    /// Output arrived, so repeating the request could duplicate it.
    pub(crate) emitted: bool,
}

impl StreamState {
    /// Apply one event. Returns answer text to stream to the client now.
    pub(crate) fn apply(
        &mut self,
        event: ResponseEvent,
        limits: &ProviderLimits,
    ) -> Result<Option<String>, EventError> {
        match event.event_type.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = event.delta {
                    if self.content.len().saturating_add(delta.len()) > limits.max_assistant_bytes {
                        return Err(limit_error("assistant response exceeded byte limit").into());
                    }
                    self.emitted = true;
                    self.content.push_str(&delta);
                    return Ok(Some(delta));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.delta {
                    self.emitted = true;
                    let call = self
                        .calls
                        .entry(event.output_index.unwrap_or(0))
                        .or_default();
                    call.arguments.push_str(&delta);
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(annotation) = &event.annotation {
                    add_citation(&mut self.citations, annotation);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.item {
                    if item.kind.as_deref() == Some("function_call") {
                        self.emitted = true;
                        let call = self
                            .calls
                            .entry(event.output_index.unwrap_or(0))
                            .or_default();
                        call.id = item.call_id.unwrap_or_default();
                        call.name = item.name.unwrap_or_default();
                    } else if item.kind.as_deref() == Some("message") {
                        // Some endpoints report citations only on the
                        // finished message.
                        for annotation in item
                            .content
                            .iter()
                            .flat_map(|content| content.as_array().into_iter().flatten())
                            .filter_map(|part| part.get("annotations")?.as_array())
                            .flatten()
                        {
                            add_citation(&mut self.citations, annotation);
                        }
                    }
                }
            }
            "response.completed" => {
                if let Some(reported) = event.response.and_then(|response| response.usage) {
                    self.usage = Usage {
                        input_tokens: reported.input_tokens,
                        output_tokens: reported.output_tokens,
                    };
                }
                self.done = true;
            }
            "error" => {
                let details = match event.error {
                    Some(error) => ErrorDetails::from_value(&error),
                    None => ErrorDetails::from_value(
                        &json!({"code": event.code, "message": event.message}),
                    ),
                };
                return Err(EventError::Provider {
                    context: "provider stream error",
                    details,
                });
            }
            "response.failed" => {
                let details = event
                    .response
                    .and_then(|response| response.error)
                    .map(|error| ErrorDetails::from_value(&error))
                    .unwrap_or_default();
                return Err(EventError::Provider {
                    context: "provider response failed",
                    details,
                });
            }
            "response.incomplete" => {
                let reason = event
                    .response
                    .and_then(|response| response.incomplete_details)
                    .and_then(|details| details.reason)
                    .unwrap_or_else(|| "unknown reason".into());
                return Err(EventError::Incomplete(reason));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Append the cited sources the answer text does not link yet. Returns the
    /// appended text, to stream to the client.
    pub(crate) fn append_sources(
        &mut self,
        limits: &ProviderLimits,
    ) -> Result<Option<String>, ProviderError> {
        let Some(sources) = sources_appendix(&self.content, &self.citations) else {
            return Ok(None);
        };
        if self.content.len().saturating_add(sources.len()) > limits.max_assistant_bytes {
            return Err(limit_error("assistant response exceeded byte limit"));
        }
        self.content.push_str(&sources);
        Ok(Some(sources))
    }

    /// The finished response; every tool call must be complete.
    pub(crate) fn finish(self) -> Result<AssistantResponse, ProviderError> {
        let tool_calls = self
            .calls
            .into_values()
            .map(|call| {
                if call.id.is_empty() || call.name.is_empty() {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Provider,
                        "provider returned an incomplete tool call",
                    ));
                }
                let arguments = serde_json::from_str(if call.arguments.is_empty() {
                    "{}"
                } else {
                    &call.arguments
                })
                .map_err(|error| {
                    ProviderError::new(
                        ProviderErrorKind::Provider,
                        format!("provider returned invalid tool arguments: {error}"),
                    )
                })?;
                Ok(ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AssistantResponse {
            content: self.content,
            tool_calls,
            usage: self.usage,
        })
    }
}

/// The `data:` lines of one SSE event, each with the name from the `event:`
/// line before it (empty when there is none).
pub(crate) fn data_lines(event: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut event_name: &[u8] = b"";
    event.split(|byte| *byte == b'\n').filter_map(move |line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(name) = line.strip_prefix(b"event:") {
            event_name = name.strip_prefix(b" ").unwrap_or(name);
            return None;
        }
        let data = line.strip_prefix(b"data:")?;
        Some((event_name, data.strip_prefix(b" ").unwrap_or(data)))
    })
}

/// Parse one non-empty `data:` payload named `event_name`.
pub(crate) fn parse_event(event_name: &[u8], data: &[u8]) -> Result<ResponseEvent, EventError> {
    let mut event: ResponseEvent = match serde_json::from_slice(data) {
        Ok(event) => event,
        // An `error` event whose data is not JSON still carries the
        // provider's reason.
        Err(_) if event_name == b"error" => {
            return Err(EventError::Provider {
                context: "provider stream error",
                details: ErrorDetails {
                    message: Some(String::from_utf8_lossy(data).into_owned()),
                    ..Default::default()
                },
            });
        }
        Err(error) => {
            return Err(ProviderError::new(
                ProviderErrorKind::Provider,
                format!("invalid provider stream JSON: {error}"),
            )
            .into());
        }
    };
    if event.event_type.is_empty() {
        event.event_type = String::from_utf8_lossy(event_name).into_owned();
    }
    Ok(event)
}

/// Where the first complete event in `buffer` ends, and the separator length.
pub(crate) fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| (position, 4))
        })
}

pub(crate) fn limit_error(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::ResponseLimit, message)
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// A web source the provider cited for the answer.
#[derive(Debug, Clone, PartialEq)]
struct Citation {
    title: String,
    url: String,
}

fn add_citation(citations: &mut Vec<Citation>, annotation: &Value) {
    if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
        return;
    }
    let Some(url) = annotation.get("url").and_then(Value::as_str) else {
        return;
    };
    if url.is_empty() || citations.iter().any(|citation| citation.url == url) {
        return;
    }
    citations.push(Citation {
        title: annotation
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        url: url.to_owned(),
    });
}

/// Lists cited sources that the answer text does not already link, so the
/// reader can always see where a searched answer came from.
fn sources_appendix(content: &str, citations: &[Citation]) -> Option<String> {
    let missing: Vec<&Citation> = citations
        .iter()
        .filter(|citation| !content.contains(&citation.url))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut appendix = String::from("\n\nSources:");
    for citation in missing {
        let title = citation.title.replace(['[', ']', '\n'], " ");
        let title = title.trim();
        if title.is_empty() {
            appendix.push_str(&format!("\n- <{}>", citation.url));
        } else {
            appendix.push_str(&format!("\n- [{title}]({})", citation.url));
        }
    }
    Some(appendix)
}

#[cfg(test)]
mod tests;
