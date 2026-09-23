//! Streaming OpenAI-compatible Responses provider.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use scv_core::{
    AssistantResponse, Message, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    TextDeltaSink, ToolCall, Usage,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ProviderLimits {
    pub max_sse_event_bytes: usize,
    pub max_response_bytes: usize,
    pub max_assistant_bytes: usize,
    pub max_tool_calls: usize,
    pub max_tool_arguments_bytes: usize,
    /// Extra attempts after a transient failure (429, 5xx, overload, a
    /// dropped stream), made only while nothing has streamed yet.
    pub max_retries: usize,
    /// First backoff delay; each retry doubles it, with jitter.
    pub retry_base_delay: Duration,
}

impl Default for ProviderLimits {
    fn default() -> Self {
        Self {
            max_sse_event_bytes: 1024 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_assistant_bytes: 1024 * 1024,
            max_tool_calls: 32,
            max_tool_arguments_bytes: 256 * 1024,
            max_retries: 2,
            retry_base_delay: Duration::from_secs(1),
        }
    }
}

/// The longest `Retry-After` SCV waits for; a provider asking for more is
/// reported instead of retried.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Provider-supplied error text is bounded before it reaches clients.
const MAX_ERROR_CHARS: usize = 300;

/// Why one request attempt failed, and whether another attempt may help.
struct AttemptFailure {
    error: ProviderError,
    transient: bool,
    retry_after: Option<Duration>,
}

impl AttemptFailure {
    fn fatal(error: ProviderError) -> Self {
        Self {
            error,
            transient: false,
            retry_after: None,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: ProviderError::new(ProviderErrorKind::Provider, message),
            transient: true,
            retry_after: None,
        }
    }
}

impl From<ProviderError> for AttemptFailure {
    fn from(error: ProviderError) -> Self {
        Self::fatal(error)
    }
}

pub struct OpenAiProvider {
    client: Client,
    model: String,
    base_url: String,
    api_key: String,
    limits: ProviderLimits,
    headers: std::collections::HashMap<String, String>,
    /// Provider-executed tools, such as hosted web search, sent alongside the
    /// function tools.
    hosted_tools: Vec<Value>,
}

impl OpenAiProvider {
    pub fn new(
        model: String,
        base_url: String,
        api_key: String,
        timeout: Duration,
        limits: ProviderLimits,
        headers: std::collections::HashMap<String, String>,
    ) -> Result<Self, ProviderError> {
        if api_key.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Provider,
                "provider API key is empty",
            ));
        }
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| ProviderError::new(ProviderErrorKind::Provider, error.to_string()))?;
        Ok(Self {
            client,
            model,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            limits,
            headers,
            hosted_tools: Vec::new(),
        })
    }

    /// Offers the endpoint's hosted Responses `web_search` tool. The provider
    /// runs the searches itself; only the answer and its citations return.
    pub fn with_web_search(mut self) -> Self {
        self.hosted_tools.push(json!({"type":"web_search"}));
        self
    }

    fn request_body(&self, request: &ProviderRequest) -> Value {
        let input = response_input(&request.messages);
        // Tool schemas leave optional fields out of `required`. Strict mode,
        // the Responses default, would make the model fill every field anyway.
        let mut tools: Vec<Value> = request.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect();
        tools.extend(self.hosted_tools.iter().cloned());
        let mut body = json!({"model": self.model, "instructions": request.system_prompt, "input": input, "stream": true});
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        body
    }

    async fn response_error(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> AttemptFailure {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::with_capacity(4096);
        while bytes.len() < 4096 {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return AttemptFailure::fatal(cancelled()),
            };
            let Some(chunk) = chunk else {
                break;
            };
            let Ok(chunk) = chunk else { break };
            let remaining = 4096 - bytes.len();
            bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        let detail = match error_details(&bytes) {
            Some(details) => details.describe(),
            None => String::from_utf8_lossy(&bytes).into_owned(),
        };
        AttemptFailure {
            error: ProviderError::new(
                ProviderErrorKind::Provider,
                format!(
                    "provider returned HTTP {status}: {}",
                    self.sanitize(&detail)
                ),
            ),
            transient: status.as_u16() == 429 || status.is_server_error(),
            retry_after,
        }
    }

    /// Redacts the credential, flattens control characters, and bounds text
    /// the provider chose to send before it reaches clients and logs.
    fn sanitize(&self, text: &str) -> String {
        let text = text.replace(&self.api_key, "[REDACTED]");
        let mut cleaned: String = text
            .trim()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_ERROR_CHARS)
            .collect();
        if text.trim().chars().count() > MAX_ERROR_CHARS {
            cleaned.push('…');
        }
        cleaned
    }

    fn provider_failure(&self, details: &ErrorDetails, context: &str) -> AttemptFailure {
        AttemptFailure {
            error: ProviderError::new(
                ProviderErrorKind::Provider,
                format!("{context}: {}", self.sanitize(&details.describe())),
            ),
            transient: details.is_transient(),
            retry_after: None,
        }
    }

    /// Sends one request and reads its whole stream. `emitted` records
    /// whether any output arrived, after which a retry could duplicate it.
    async fn attempt(
        &self,
        body: &Value,
        deltas: &Arc<dyn TextDeltaSink>,
        cancellation: &CancellationToken,
        emitted: &mut bool,
    ) -> Result<AssistantResponse, AttemptFailure> {
        let response = tokio::select! {
            result = self.client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&self.api_key)
                .headers(self.headers.clone().into_iter().filter_map(|(k,v)| Some((k.parse().ok()?, v.parse().ok()?))).collect())
                .json(body)
                .send() => result.map_err(|error| {
                    // Nothing reached the provider when the connection failed.
                    let transient = error.is_connect();
                    AttemptFailure {
                        error: ProviderError::new(ProviderErrorKind::Provider, self.sanitize(&error.to_string())),
                        transient,
                        retry_after: None,
                    }
                })?,
            _ = cancellation.cancelled() => return Err(cancelled().into()),
        };
        if !response.status().is_success() {
            return Err(self.response_error(response, cancellation).await);
        }

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut response_bytes = 0usize;
        let mut content = String::new();
        let mut calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut citations: Vec<Citation> = Vec::new();
        let mut done = false;

        while !done {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return Err(cancelled().into()),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|error| AttemptFailure {
                error: ProviderError::new(
                    ProviderErrorKind::Provider,
                    format!(
                        "provider stream failed: {}",
                        self.sanitize(&error.to_string())
                    ),
                ),
                // The request timeout bounds a whole response; repeating a
                // response that ran out of time would only multiply the wait.
                transient: !error.is_timeout(),
                retry_after: None,
            })?;
            response_bytes = response_bytes.saturating_add(chunk.len());
            if response_bytes > self.limits.max_response_bytes {
                return Err(limit_error("provider response exceeded byte limit").into());
            }
            buffer.extend_from_slice(&chunk);
            if buffer.len() > self.limits.max_sse_event_bytes
                && find_event_boundary(&buffer).is_none()
            {
                return Err(limit_error("provider SSE event exceeded byte limit").into());
            }
            while let Some((end, separator_len)) = find_event_boundary(&buffer) {
                let event = buffer.drain(..end).collect::<Vec<_>>();
                buffer.drain(..separator_len);
                if event.len() > self.limits.max_sse_event_bytes {
                    return Err(limit_error("provider SSE event exceeded byte limit").into());
                }
                let mut event_name: &[u8] = b"";
                for line in event.split(|byte| *byte == b'\n') {
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    if let Some(name) = line.strip_prefix(b"event:") {
                        event_name = name.strip_prefix(b" ").unwrap_or(name);
                        continue;
                    }
                    let Some(data) = line.strip_prefix(b"data:") else {
                        continue;
                    };
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if data == b"[DONE]" {
                        done = true;
                        break;
                    }
                    if data.is_empty() {
                        continue;
                    }
                    let mut event: ResponseEvent = match serde_json::from_slice(data) {
                        Ok(event) => event,
                        // An `error` event whose data is not JSON still
                        // carries the provider's reason.
                        Err(_) if event_name == b"error" => {
                            let details = ErrorDetails {
                                message: Some(String::from_utf8_lossy(data).into_owned()),
                                ..Default::default()
                            };
                            return Err(self.provider_failure(&details, "provider stream error"));
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
                    match event.event_type.as_str() {
                        "response.output_text.delta" => {
                            if let Some(delta) = event.delta {
                                if content.len().saturating_add(delta.len())
                                    > self.limits.max_assistant_bytes
                                {
                                    return Err(limit_error(
                                        "assistant response exceeded byte limit",
                                    )
                                    .into());
                                }
                                *emitted = true;
                                content.push_str(&delta);
                                deltas.push(&delta).await?;
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            if let Some(delta) = event.delta {
                                *emitted = true;
                                let call =
                                    calls.entry(event.output_index.unwrap_or(0)).or_default();
                                call.arguments.push_str(&delta);
                            }
                        }
                        "response.output_text.annotation.added" => {
                            if let Some(annotation) = &event.annotation {
                                add_citation(&mut citations, annotation);
                            }
                        }
                        "response.output_item.done" => {
                            if let Some(item) = event.item {
                                if item.kind.as_deref() == Some("function_call") {
                                    *emitted = true;
                                    let call =
                                        calls.entry(event.output_index.unwrap_or(0)).or_default();
                                    call.id = item.call_id.unwrap_or_default();
                                    call.name = item.name.unwrap_or_default();
                                } else if item.kind.as_deref() == Some("message") {
                                    // Some endpoints report citations only on
                                    // the finished message.
                                    for annotation in item
                                        .content
                                        .iter()
                                        .flat_map(|content| {
                                            content.as_array().into_iter().flatten()
                                        })
                                        .filter_map(|part| part.get("annotations")?.as_array())
                                        .flatten()
                                    {
                                        add_citation(&mut citations, annotation);
                                    }
                                }
                            }
                        }
                        "response.completed" => {
                            if let Some(reported) =
                                event.response.and_then(|response| response.usage)
                            {
                                usage = Usage {
                                    input_tokens: reported.input_tokens,
                                    output_tokens: reported.output_tokens,
                                };
                            }
                            done = true;
                        }
                        "error" => {
                            let details = match event.error {
                                Some(error) => ErrorDetails::from_value(&error),
                                None => ErrorDetails::from_value(
                                    &json!({"code": event.code, "message": event.message}),
                                ),
                            };
                            return Err(self.provider_failure(&details, "provider stream error"));
                        }
                        "response.failed" => {
                            let details = event
                                .response
                                .and_then(|response| response.error)
                                .map(|error| ErrorDetails::from_value(&error))
                                .unwrap_or_default();
                            return Err(self.provider_failure(&details, "provider response failed"));
                        }
                        "response.incomplete" => {
                            let reason = event
                                .response
                                .and_then(|response| response.incomplete_details)
                                .and_then(|details| details.reason)
                                .unwrap_or_else(|| "unknown reason".into());
                            return Err(ProviderError::new(
                                ProviderErrorKind::Provider,
                                format!("provider response incomplete: {}", self.sanitize(&reason)),
                            )
                            .into());
                        }
                        _ => {}
                    }
                }
            }
        }

        if !buffer.iter().all(u8::is_ascii_whitespace) {
            // Some providers answer an accepted request with a bare JSON
            // error body instead of an event stream.
            if let Some(details) = error_details(&buffer) {
                return Err(self.provider_failure(&details, "provider error"));
            }
            return Err(AttemptFailure::transient(
                "provider stream ended with an incomplete SSE event",
            ));
        }
        if !done {
            return Err(AttemptFailure::transient(
                "provider stream ended before the response completed",
            ));
        }

        if let Some(sources) = sources_appendix(&content, &citations) {
            if content.len().saturating_add(sources.len()) > self.limits.max_assistant_bytes {
                return Err(limit_error("assistant response exceeded byte limit").into());
            }
            content.push_str(&sources);
            deltas.push(&sources).await?;
        }

        let tool_calls = calls
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
            content,
            tool_calls,
            usage,
        })
    }
}

/// The delay before retry number `retry` (0-based): the base doubled per
/// retry, scaled by a random factor in [0.5, 1.5) so clients spread out.
fn backoff(base: Duration, retry: usize) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let exponential = base.saturating_mul(1u32 << retry.min(16));
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let factor = 0.5 + (random % 1000) as f64 / 1000.0;
    exponential.mul_f64(factor)
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        deltas: Arc<dyn TextDeltaSink>,
        cancellation: CancellationToken,
    ) -> Result<AssistantResponse, ProviderError> {
        let body = self.request_body(&request);
        let mut retry = 0;
        loop {
            let mut emitted = false;
            let failure = match self
                .attempt(&body, &deltas, &cancellation, &mut emitted)
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            let attempts = retry + 1;
            // A retry after streamed output would repeat it, so only a
            // failure before any output is retried.
            if !failure.transient || emitted {
                return Err(failure.error);
            }
            if retry >= self.limits.max_retries {
                return Err(gave_up(failure.error, attempts));
            }
            let delay = failure
                .retry_after
                .unwrap_or_else(|| backoff(self.limits.retry_base_delay, retry));
            if delay > MAX_RETRY_DELAY {
                return Err(gave_up(failure.error, attempts));
            }
            tracing::warn!(
                "provider request failed ({}); retrying in {} ms (retry {} of {})",
                failure.error.message,
                delay.as_millis(),
                retry + 1,
                self.limits.max_retries
            );
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = cancellation.cancelled() => return Err(cancelled()),
            }
            retry += 1;
        }
    }
}

fn gave_up(error: ProviderError, attempts: usize) -> ProviderError {
    if attempts == 1 {
        return error;
    }
    ProviderError::new(
        error.kind,
        format!("{} (gave up after {attempts} attempts)", error.message),
    )
}

/// Replays the conversation as Responses input items.
///
/// The Responses API answers a `function_call_output` only when the input also
/// carries its `function_call`, and rejects a `function_call` that has no
/// output, so every call and output are emitted as a pair. A turn cancelled
/// between a call and its result leaves an unanswered call in history, which
/// is closed with a synthetic failure before the next message.
fn response_input(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::with_capacity(messages.len());
    let mut unanswered: Vec<&str> = Vec::new();
    for message in messages {
        match message {
            Message::Tool {
                call_id,
                name,
                content,
                ..
            } => {
                if let Some(position) = unanswered.iter().position(|id| *id == call_id) {
                    unanswered.remove(position);
                } else {
                    input.push(function_call(call_id, name, "{}"));
                }
                input.push(function_call_output(call_id, content));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                close_unanswered(&mut input, &mut unanswered);
                if !content.is_empty() || tool_calls.is_empty() {
                    input.push(json!({"role":"assistant","content":content}));
                }
                for call in tool_calls {
                    input.push(function_call(
                        &call.id,
                        &call.name,
                        &call.arguments.to_string(),
                    ));
                    unanswered.push(&call.id);
                }
            }
            Message::User { content } | Message::HistoryNote { content } => {
                close_unanswered(&mut input, &mut unanswered);
                input.push(json!({"role":"user","content":content}));
            }
        }
    }
    close_unanswered(&mut input, &mut unanswered);
    input
}

fn close_unanswered(input: &mut Vec<Value>, unanswered: &mut Vec<&str>) {
    for call_id in unanswered.drain(..) {
        input.push(function_call_output(
            call_id,
            "Tool call did not complete: the turn ended before it returned a result.",
        ));
    }
}

fn function_call(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments})
}

fn function_call_output(call_id: &str, output: &str) -> Value {
    json!({"type":"function_call_output","call_id":call_id,"output":output})
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

fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
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

fn limit_error(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::ResponseLimit, message)
}

fn cancelled() -> ProviderError {
    ProviderError::new(ProviderErrorKind::Cancelled, "provider request cancelled")
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ResponseEvent {
    /// Empty when the provider names the event only on its `event:` line.
    #[serde(rename = "type", default)]
    event_type: String,
    delta: Option<String>,
    output_index: Option<usize>,
    item: Option<ResponseItem>,
    response: Option<ResponseSummary>,
    /// An `error` event carries its details either nested or at top level.
    error: Option<Value>,
    code: Option<Value>,
    message: Option<Value>,
    annotation: Option<Value>,
}
#[derive(Debug, Deserialize)]
struct ResponseSummary {
    usage: Option<ResponseUsage>,
    error: Option<Value>,
    incomplete_details: Option<IncompleteDetails>,
}
#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    reason: Option<String>,
}

/// A provider error as OpenAI-compatible endpoints report it.
#[derive(Debug, Default)]
struct ErrorDetails {
    kind: Option<Value>,
    code: Option<Value>,
    message: Option<String>,
}

impl ErrorDetails {
    /// Accepts an error object or a bare string; other fields are ignored.
    fn from_value(value: &Value) -> Self {
        match value {
            Value::String(message) => Self {
                message: Some(message.clone()),
                ..Default::default()
            },
            Value::Object(object) => Self {
                kind: object.get("type").cloned(),
                code: object.get("code").cloned(),
                message: object.get("message").map(|message| match message {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                }),
            },
            _ => Self::default(),
        }
    }

    fn labels(&self) -> impl Iterator<Item = String> + '_ {
        [&self.code, &self.kind]
            .into_iter()
            .flatten()
            .filter_map(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .filter(|label| !label.is_empty())
    }

    fn describe(&self) -> String {
        let mut labels: Vec<String> = self.labels().collect();
        labels.dedup();
        let message = self
            .message
            .as_deref()
            .filter(|message| !message.trim().is_empty())
            .unwrap_or("no details");
        if labels.is_empty() {
            message.to_owned()
        } else {
            format!("{message} ({})", labels.join(", "))
        }
    }

    /// Overload, rate-limit, and server-side errors may clear on retry;
    /// request and policy errors will not.
    fn is_transient(&self) -> bool {
        const TRANSIENT: [&str; 5] = [
            "overload",
            "unavailable",
            "rate_limit",
            "server_error",
            "timeout",
        ];
        self.labels().any(|label| {
            let label = label.to_ascii_lowercase();
            TRANSIENT.iter().any(|needle| label.contains(needle))
        }) || self
            .message
            .as_deref()
            .is_some_and(|message| message.to_ascii_lowercase().contains("overloaded"))
    }
}

/// Reads a JSON error body such as `{"error":{"message":…}}`, optionally
/// behind a `data:` prefix, from bytes that are not a complete event stream.
fn error_details(bytes: &[u8]) -> Option<ErrorDetails> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let text = text
        .strip_prefix("data:")
        .map(str::trim_start)
        .unwrap_or(text);
    let value: Value = serde_json::from_str(text).ok()?;
    let details = ErrorDetails::from_value(value.get("error").unwrap_or(&value));
    (details.message.is_some() || details.code.is_some()).then_some(details)
}
#[derive(Debug, Deserialize)]
struct ResponseUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}
#[derive(Debug, Deserialize)]
struct ResponseItem {
    #[serde(rename = "type")]
    kind: Option<String>,
    _id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    content: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    fn tool(id: &str, name: &str, content: &str) -> Message {
        Message::Tool {
            call_id: id.into(),
            name: name.into(),
            content: content.into(),
            is_error: false,
        }
    }

    fn user(content: &str) -> Message {
        Message::User {
            content: content.into(),
        }
    }

    fn assistant(content: &str, tool_calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            content: content.into(),
            tool_calls,
        }
    }

    fn fc(id: &str, name: &str, arguments: &str) -> Value {
        json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments})
    }

    fn out(id: &str, output: &str) -> Value {
        json!({"type":"function_call_output","call_id":id,"output":output})
    }

    #[test]
    fn multi_step_turns_replay_each_call_before_its_output() {
        let messages = vec![
            user("summarize README"),
            assistant("", vec![call("c1", "read", json!({"path":"README.md"}))]),
            tool("c1", "read", "# SCV"),
            assistant("", vec![call("c2", "bash", json!({"command":"ls"}))]),
            tool("c2", "bash", "src"),
            assistant("A Rust agent.", Vec::new()),
            user("thanks"),
        ];
        assert_eq!(
            response_input(&messages),
            vec![
                json!({"role":"user","content":"summarize README"}),
                fc("c1", "read", r#"{"path":"README.md"}"#),
                out("c1", "# SCV"),
                fc("c2", "bash", r#"{"command":"ls"}"#),
                out("c2", "src"),
                json!({"role":"assistant","content":"A Rust agent."}),
                json!({"role":"user","content":"thanks"}),
            ]
        );
    }

    #[test]
    fn parallel_calls_keep_the_assistant_text_and_call_order() {
        let messages = vec![
            user("compare"),
            assistant(
                "Reading both.",
                vec![
                    call("a", "read", json!({"path":"a"})),
                    call("b", "read", json!({"path":"b"})),
                ],
            ),
            tool("a", "read", "A"),
            tool("b", "read", "B"),
        ];
        assert_eq!(
            response_input(&messages),
            vec![
                json!({"role":"user","content":"compare"}),
                json!({"role":"assistant","content":"Reading both."}),
                fc("a", "read", r#"{"path":"a"}"#),
                fc("b", "read", r#"{"path":"b"}"#),
                out("a", "A"),
                out("b", "B"),
            ]
        );
    }

    #[test]
    fn an_interrupted_call_is_closed_before_the_next_message() {
        let messages = vec![
            user("run both"),
            assistant(
                "",
                vec![
                    call("a", "bash", json!({"command":"true"})),
                    call("b", "bash", json!({"command":"sleep 99"})),
                ],
            ),
            tool("a", "bash", "ok"),
            user("never mind"),
        ];
        let input = response_input(&messages);
        assert_eq!(input[3], out("a", "ok"));
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "b");
        assert!(
            input[4]["output"]
                .as_str()
                .unwrap()
                .contains("did not complete")
        );
        assert_eq!(input[5], json!({"role":"user","content":"never mind"}));
        assert_eq!(input.len(), 6);
    }

    #[test]
    fn an_output_without_its_call_gets_one_and_plain_messages_are_unchanged() {
        let messages = vec![
            Message::HistoryNote {
                content: "[earlier]".into(),
            },
            tool("x", "read", "orphaned"),
            assistant("", Vec::new()),
        ];
        assert_eq!(
            response_input(&messages),
            vec![
                json!({"role":"user","content":"[earlier]"}),
                fc("x", "read", "{}"),
                out("x", "orphaned"),
                json!({"role":"assistant","content":""}),
            ]
        );
    }

    #[test]
    fn request_body_carries_the_paired_input() {
        let provider = OpenAiProvider::new(
            "model".into(),
            "http://127.0.0.1:9/v1".into(),
            "key".into(),
            Duration::from_secs(1),
            ProviderLimits::default(),
            Default::default(),
        )
        .unwrap();
        let body = provider.request_body(&ProviderRequest {
            system_prompt: "system".into(),
            messages: vec![
                user("hi"),
                assistant("", vec![call("c", "read", json!({"path":"x"}))]),
                tool("c", "read", "y"),
            ],
            tools: Vec::new(),
        });
        assert_eq!(body["input"][1], fc("c", "read", r#"{"path":"x"}"#));
        assert_eq!(body["input"][2], out("c", "y"));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tools_are_sent_non_strict_so_optional_fields_stay_optional() {
        let provider = OpenAiProvider::new(
            "model".into(),
            "http://127.0.0.1:9/v1".into(),
            "key".into(),
            Duration::from_secs(1),
            ProviderLimits::default(),
            Default::default(),
        )
        .unwrap();
        let parameters = json!({
            "type":"object",
            "properties":{"path":{"type":"string"},"limit":{"type":"integer"}},
            "required":["path"],
            "additionalProperties":false
        });
        let body = provider.request_body(&ProviderRequest {
            system_prompt: String::new(),
            messages: vec![user("hi")],
            tools: vec![scv_core::ToolSpec {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: parameters.clone(),
            }],
        });
        assert_eq!(
            body["tools"],
            json!([{
                "type":"function",
                "name":"read",
                "description":"Read a file",
                "parameters":parameters,
                "strict":false
            }])
        );
    }

    mod stream_errors {
        use std::sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        use super::*;

        const KEY: &str = "sk-test-secret";
        const OVERLOADED: &str = r#"{"type":"error","error":{"type":"service_unavailable_error","code":"service_unavailable_error","message":"Our servers are currently overloaded"}}"#;

        fn sse(body: &str) -> String {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }

        fn status(line: &str, headers: &str, body: &str) -> String {
            format!(
                "HTTP/1.1 {line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                body.len()
            )
        }

        fn event(data: &str) -> String {
            format!("data: {data}\n\n")
        }

        fn hello() -> String {
            sse(&[
                event(r#"{"type":"response.output_text.delta","delta":"hello"}"#),
                event(r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":1}}}"#),
            ]
            .concat())
        }

        /// Answers each request with the next canned response and counts
        /// the requests it received.
        async fn serve(responses: Vec<String>) -> (String, Arc<AtomicUsize>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let count = Arc::new(AtomicUsize::new(0));
            let served = Arc::clone(&count);
            tokio::spawn(async move {
                for response in responses {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    served.fetch_add(1, Ordering::SeqCst);
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 8192];
                    loop {
                        let read = stream.read(&mut buffer).await.unwrap();
                        request.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&request);
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length: usize = text[..end]
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                        if read == 0 {
                            break;
                        }
                    }
                    stream.write_all(response.as_bytes()).await.unwrap();
                    let _ = stream.shutdown().await;
                }
            });
            (base, count)
        }

        fn provider(
            base: String,
            max_retries: usize,
            retry_base_delay: Duration,
        ) -> OpenAiProvider {
            OpenAiProvider::new(
                "model".into(),
                base,
                KEY.into(),
                Duration::from_secs(10),
                ProviderLimits {
                    max_retries,
                    retry_base_delay,
                    ..ProviderLimits::default()
                },
                Default::default(),
            )
            .unwrap()
        }

        #[derive(Default)]
        struct Recorder(Mutex<String>);

        #[async_trait]
        impl TextDeltaSink for Recorder {
            async fn push(&self, delta: &str) -> Result<(), ProviderError> {
                self.0.lock().unwrap().push_str(delta);
                Ok(())
            }
        }

        async fn run(
            provider: &OpenAiProvider,
            cancellation: CancellationToken,
        ) -> (Result<AssistantResponse, ProviderError>, String) {
            let recorder = Arc::new(Recorder::default());
            let result = provider
                .complete(
                    ProviderRequest {
                        system_prompt: String::new(),
                        messages: vec![user("hi")],
                        tools: Vec::new(),
                    },
                    Arc::clone(&recorder) as Arc<dyn TextDeltaSink>,
                    cancellation,
                )
                .await;
            let text = recorder.0.lock().unwrap().clone();
            (result, text)
        }

        const FAST: Duration = Duration::from_millis(10);

        #[tokio::test]
        async fn an_overload_is_retried_and_the_second_attempt_succeeds() {
            let (base, count) = serve(vec![sse(&event(OVERLOADED)), hello()]).await;
            let (result, streamed) = run(&provider(base, 2, FAST), CancellationToken::new()).await;
            let response = result.unwrap();
            assert_eq!(response.content, "hello");
            assert_eq!(streamed, "hello");
            assert_eq!(response.usage.input_tokens, Some(5));
            assert_eq!(response.usage.output_tokens, Some(1));
            assert_eq!(count.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn an_error_event_after_output_fails_without_a_retry() {
            let body = [
                event(r#"{"type":"response.output_text.delta","delta":"partial"}"#),
                event(OVERLOADED),
            ]
            .concat();
            let (base, count) = serve(vec![sse(&body), hello()]).await;
            let (result, streamed) = run(&provider(base, 2, FAST), CancellationToken::new()).await;
            let error = result.unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Provider);
            assert!(
                error
                    .message
                    .contains("Our servers are currently overloaded"),
                "{}",
                error.message
            );
            assert_eq!(streamed, "partial");
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn a_request_error_event_is_reported_without_a_retry() {
            let body = format!(
                "event: error\n{}",
                event(r#"{"code":"invalid_prompt","message":"prompt rejected"}"#)
            );
            let (base, count) = serve(vec![sse(&body), hello()]).await;
            let (result, _) = run(&provider(base, 2, FAST), CancellationToken::new()).await;
            let message = result.unwrap_err().message;
            assert_eq!(
                message,
                "provider stream error: prompt rejected (invalid_prompt)"
            );
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn response_failed_and_incomplete_are_errors() {
            let failed = event(
                r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"invalid_image","message":"cannot read image"}}}"#,
            );
            let incomplete = event(
                r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
            );
            let transient = event(
                r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"try later"}}}"#,
            );
            let (base, count) = serve(vec![
                sse(&failed),
                sse(&incomplete),
                sse(&transient),
                hello(),
            ])
            .await;
            let provider = provider(base, 2, FAST);
            let (result, _) = run(&provider, CancellationToken::new()).await;
            assert_eq!(
                result.unwrap_err().message,
                "provider response failed: cannot read image (invalid_image)"
            );
            let (result, _) = run(&provider, CancellationToken::new()).await;
            assert_eq!(
                result.unwrap_err().message,
                "provider response incomplete: max_output_tokens"
            );
            // A server-side failure is transient and the retry succeeds.
            let (result, _) = run(&provider, CancellationToken::new()).await;
            assert_eq!(result.unwrap().content, "hello");
            assert_eq!(count.load(Ordering::SeqCst), 4);
        }

        #[tokio::test]
        async fn a_stream_that_ends_early_is_retried_and_then_reported() {
            let early = sse(": keep-alive\n\n");
            let (base, count) = serve(vec![early.clone(), early.clone(), early]).await;
            let (result, streamed) = run(&provider(base, 2, FAST), CancellationToken::new()).await;
            assert_eq!(
                result.unwrap_err().message,
                "provider stream ended before the response completed (gave up after 3 attempts)"
            );
            assert!(streamed.is_empty());
            assert_eq!(count.load(Ordering::SeqCst), 3);
        }

        #[tokio::test]
        async fn a_plain_json_error_body_is_reported() {
            let body = r#":

{"error":{"message":"Our servers are currently overloaded","type":"service_unavailable_error"}}"#;
            let (base, count) = serve(vec![sse(body), sse(body)]).await;
            let (result, _) = run(&provider(base, 1, FAST), CancellationToken::new()).await;
            assert_eq!(
                result.unwrap_err().message,
                "provider error: Our servers are currently overloaded (service_unavailable_error) (gave up after 2 attempts)"
            );
            assert_eq!(count.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn http_statuses_retry_only_when_transient() {
            let bad_request = status(
                "400 Bad Request",
                "",
                r#"{"error":{"message":"unknown model","type":"invalid_request_error"}}"#,
            );
            let unavailable = status("503 Service Unavailable", "", "upstream down");
            let (base, count) = serve(vec![bad_request, unavailable, hello()]).await;
            let provider = provider(base, 2, FAST);
            let (result, _) = run(&provider, CancellationToken::new()).await;
            assert_eq!(
                result.unwrap_err().message,
                "provider returned HTTP 400 Bad Request: unknown model (invalid_request_error)"
            );
            let (result, _) = run(&provider, CancellationToken::new()).await;
            assert_eq!(result.unwrap().content, "hello");
            assert_eq!(count.load(Ordering::SeqCst), 3);
        }

        #[tokio::test]
        async fn retry_after_is_honoured_up_to_a_minute() {
            let limited = status("429 Too Many Requests", "Retry-After: 1\r\n", "{}");
            let (base, count) = serve(vec![limited, hello()]).await;
            let started = std::time::Instant::now();
            let (result, _) = run(&provider(base, 2, FAST), CancellationToken::new()).await;
            assert_eq!(result.unwrap().content, "hello");
            assert!(started.elapsed() >= Duration::from_secs(1));
            assert_eq!(count.load(Ordering::SeqCst), 2);

            let too_long = status("429 Too Many Requests", "Retry-After: 3600\r\n", "{}");
            let (base, count) = serve(vec![too_long, hello()]).await;
            let (result, _) = run(&provider(base, 2, FAST), CancellationToken::new()).await;
            assert!(
                result
                    .unwrap_err()
                    .message
                    .starts_with("provider returned HTTP 429")
            );
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn cancellation_interrupts_the_backoff() {
            let (base, count) =
                serve(vec![status("503 Service Unavailable", "", "{}"), hello()]).await;
            let provider = provider(base, 2, Duration::from_secs(30));
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                cancel.cancel();
            });
            let started = std::time::Instant::now();
            let (result, _) = run(&provider, cancellation).await;
            assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Cancelled);
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn provider_error_text_is_redacted_flattened_and_bounded() {
            let message = format!("key {KEY} leaked\nline two {}", "x".repeat(400));
            let body =
                event(&json!({"type":"error","message":message,"code":"invalid"}).to_string());
            let (base, _) = serve(vec![sse(&body)]).await;
            let (result, _) = run(&provider(base, 0, FAST), CancellationToken::new()).await;
            let message = result.unwrap_err().message;
            assert!(!message.contains(KEY));
            assert!(message.contains("[REDACTED] leaked line two"));
            assert!(!message.contains('\n'));
            assert!(message.ends_with('…'));
            assert!(
                message.chars().count() <= "provider stream error: ".len() + MAX_ERROR_CHARS + 1
            );
        }

        #[test]
        fn error_bodies_are_read_in_their_common_shapes() {
            let nested =
                error_details(br#"{"error":{"message":"m","code":"c","type":"t"}}"#).unwrap();
            assert_eq!(nested.describe(), "m (c, t)");
            let flat = error_details(br#"data: {"message":"m","code":429}"#).unwrap();
            assert_eq!(flat.describe(), "m (429)");
            let text = error_details(br#"{"error":"overloaded, retry"}"#).unwrap();
            assert_eq!(text.describe(), "overloaded, retry");
            assert!(error_details(b"not json").is_none());
            assert!(error_details(br#"{"ok":true}"#).is_none());
            assert!(
                ErrorDetails::from_value(&json!({"type":"rate_limit_exceeded"})).is_transient()
            );
            assert!(ErrorDetails::from_value(&json!("The engine is overloaded")).is_transient());
            assert!(
                !ErrorDetails::from_value(&json!({"code":"invalid_request_error","message":"no"}))
                    .is_transient()
            );
        }

        #[test]
        fn backoff_doubles_with_jitter() {
            for retry in 0..4 {
                let delay = backoff(Duration::from_secs(1), retry);
                let nominal = Duration::from_secs(1 << retry);
                assert!(delay >= nominal / 2 && delay < nominal * 3 / 2, "{delay:?}");
            }
        }
    }

    #[test]
    fn hosted_web_search_is_offered_beside_function_tools() {
        let provider = OpenAiProvider::new(
            "model".into(),
            "http://127.0.0.1:9/v1".into(),
            "key".into(),
            Duration::from_secs(1),
            ProviderLimits::default(),
            Default::default(),
        )
        .unwrap()
        .with_web_search();
        let request = |tools| ProviderRequest {
            system_prompt: String::new(),
            messages: vec![user("latest serde?")],
            tools,
        };
        let body = provider.request_body(&request(vec![scv_core::ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type":"object"}),
        }]));
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][1], json!({"type":"web_search"}));
        let body = provider.request_body(&request(Vec::new()));
        assert_eq!(body["tools"], json!([{"type":"web_search"}]));
    }

    #[test]
    fn sources_are_appended_only_when_the_text_does_not_link_them() {
        let citations = vec![
            Citation {
                title: "tokio 1.53.1 - Docs.rs".into(),
                url: "https://docs.rs/tokio".into(),
            },
            Citation {
                title: String::new(),
                url: "https://crates.io/crates/tokio".into(),
            },
        ];
        assert_eq!(
            sources_appendix("See [docs.rs](https://docs.rs/tokio).", &citations).unwrap(),
            "\n\nSources:\n- <https://crates.io/crates/tokio>"
        );
        assert_eq!(
            sources_appendix("Latest is 1.53.1.", &citations[..1]).unwrap(),
            "\n\nSources:\n- [tokio 1.53.1 - Docs.rs](https://docs.rs/tokio)"
        );
        assert!(
            sources_appendix(
                "https://docs.rs/tokio https://crates.io/crates/tokio",
                &citations
            )
            .is_none()
        );
        let mut collected = Vec::new();
        for annotation in [
            json!({"type":"url_citation","url":"https://a.test","title":"A"}),
            json!({"type":"url_citation","url":"https://a.test","title":"again"}),
            json!({"type":"file_citation","file_id":"f"}),
        ] {
            add_citation(&mut collected, &annotation);
        }
        assert_eq!(collected.len(), 1);
    }

    struct Collect(std::sync::Mutex<String>);

    #[async_trait]
    impl TextDeltaSink for Collect {
        async fn push(&self, delta: &str) -> Result<(), ProviderError> {
            self.0.lock().unwrap().push_str(delta);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_searched_answer_streams_with_its_cited_sources() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = String::from_utf8(request).unwrap();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body).unwrap();
            let events = concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\n",
                "data: {\"type\":\"response.web_search_call.completed\",\"output_index\":0,\"item_id\":\"ws_1\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"tokio\"}}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"Tokio is at 1.53.1.\"}\n\n",
                "data: {\"type\":\"response.output_text.annotation.added\",\"output_index\":1,\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://docs.rs/tokio\",\"title\":\"tokio - Docs.rs\",\"start_index\":0,\"end_index\":5}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Tokio is at 1.53.1.\",\"annotations\":[{\"type\":\"url_citation\",\"url\":\"https://crates.io/crates/tokio\",\"title\":\"crates.io\"}]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                events.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            serde_json::from_slice::<Value>(&body).unwrap()
        });
        let provider = OpenAiProvider::new(
            "model".into(),
            format!("http://{address}/v1"),
            "key".into(),
            Duration::from_secs(5),
            ProviderLimits::default(),
            Default::default(),
        )
        .unwrap()
        .with_web_search();
        let deltas = Arc::new(Collect(std::sync::Mutex::new(String::new())));
        let response = provider
            .complete(
                ProviderRequest {
                    system_prompt: String::new(),
                    messages: vec![user("latest tokio?")],
                    tools: Vec::new(),
                },
                deltas.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let expected = "Tokio is at 1.53.1.\n\nSources:\n- [tokio - Docs.rs](https://docs.rs/tokio)\n- [crates.io](https://crates.io/crates/tokio)";
        assert_eq!(response.content, expected);
        assert_eq!(*deltas.0.lock().unwrap(), expected);
        assert!(response.tool_calls.is_empty());
        let body = server.join().unwrap();
        assert_eq!(body["tools"], json!([{"type":"web_search"}]));
    }
}
