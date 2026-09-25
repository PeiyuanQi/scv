//! [`OpenAiProvider`]: sending requests, retrying transient failures, and
//! reporting provider errors safely.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use scv_core::{
    AssistantResponse, Provider, ProviderError, ProviderErrorKind, ProviderRequest, TextDeltaSink,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    encode::response_input,
    stream::{EventError, StreamState, data_lines, find_event_boundary, limit_error, parse_event},
    wire::{ErrorDetails, error_details},
};

/// Bounds on what one response may contain, and the retry policy.
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

/// How long an idle pooled connection is kept for reuse.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Whether images in user messages are sent as image input. Cleared for
    /// the provider's lifetime once the endpoint rejects an image.
    image_input: AtomicBool,
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
            // Proxies commonly drop idle keep-alive connections after 60 s or
            // more; retiring them sooner keeps SCV from sending a request on
            // a socket the server has already closed.
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
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
            image_input: AtomicBool::new(true),
        })
    }

    /// Whether images in user messages go to the model as image input
    /// (the default) or only as a short note naming each one.
    #[must_use]
    pub fn with_image_input(self, enabled: bool) -> Self {
        self.image_input.store(enabled, Ordering::Relaxed);
        self
    }

    /// Offers the endpoint's hosted Responses `web_search` tool. The provider
    /// runs the searches itself; only the answer and its citations return.
    #[must_use]
    pub fn with_web_search(mut self) -> Self {
        self.hosted_tools.push(json!({"type":"web_search"}));
        self
    }

    /// The request, and whether it carries image input.
    fn request_body(&self, request: &ProviderRequest) -> (Value, bool) {
        let images = self.image_input.load(Ordering::Relaxed);
        let (input, shown) = response_input(&request.messages, images);
        // Tool schemas leave optional fields out of `required`. Strict mode,
        // the Responses default, would make the model fill every field anyway.
        let mut tools: Vec<Value> = request.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect();
        tools.extend(self.hosted_tools.iter().cloned());
        let mut body = json!({"model": self.model, "instructions": request.system_prompt, "input": input, "stream": true});
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        (body, shown > 0)
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
                () = cancellation.cancelled() => return AttemptFailure::fatal(cancelled()),
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

    /// What an event error means for this attempt.
    fn event_failure(&self, error: EventError) -> AttemptFailure {
        match error {
            EventError::Fatal(error) => AttemptFailure::fatal(error),
            EventError::Provider { context, details } => self.provider_failure(&details, context),
            EventError::Incomplete(reason) => AttemptFailure::fatal(ProviderError::new(
                ProviderErrorKind::Provider,
                format!("provider response incomplete: {}", self.sanitize(&reason)),
            )),
        }
    }

    /// Send the request; a response that is not a success becomes the failure.
    async fn send(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response, AttemptFailure> {
        let response = tokio::select! {
            result = self.client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&self.api_key)
                .headers(self.headers.clone().into_iter().filter_map(|(k,v)| Some((k.parse().ok()?, v.parse().ok()?))).collect())
                .json(body)
                .send() => result.map_err(|error| {
                    // No response status arrived, so nothing was streamed and
                    // a repeat cannot duplicate output. That covers a refused
                    // connection and a pooled keep-alive connection the server
                    // had already closed. A timeout bounds the whole request
                    // and a malformed request fails again, so neither repeats.
                    let transient =
                        !(error.is_timeout() || error.is_builder() || error.is_redirect());
                    AttemptFailure {
                        error: ProviderError::new(ProviderErrorKind::Provider, self.sanitize(&error.to_string())),
                        transient,
                        retry_after: None,
                    }
                })?,
            () = cancellation.cancelled() => return Err(cancelled().into()),
        };
        if !response.status().is_success() {
            return Err(self.response_error(response, cancellation).await);
        }
        Ok(response)
    }

    /// Sends one request and reads its whole stream into `state`, whose
    /// `emitted` flag records whether any output arrived, after which a retry
    /// could duplicate it.
    async fn attempt(
        &self,
        body: &Value,
        deltas: &Arc<dyn TextDeltaSink>,
        cancellation: &CancellationToken,
        state: &mut StreamState,
    ) -> Result<AssistantResponse, AttemptFailure> {
        let response = self.send(body, cancellation).await?;
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut response_bytes = 0usize;
        while !state.done {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = cancellation.cancelled() => return Err(cancelled().into()),
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
                for (event_name, data) in data_lines(&event) {
                    if data == b"[DONE]" {
                        state.done = true;
                        break;
                    }
                    if data.is_empty() {
                        continue;
                    }
                    let event =
                        parse_event(event_name, data).map_err(|error| self.event_failure(error))?;
                    let delta = state
                        .apply(event, &self.limits)
                        .map_err(|error| self.event_failure(error))?;
                    if let Some(delta) = delta {
                        deltas.push(&delta).await?;
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
        if !state.done {
            return Err(AttemptFailure::transient(
                "provider stream ended before the response completed",
            ));
        }
        if let Some(sources) = state.append_sources(&self.limits)? {
            deltas.push(&sources).await?;
        }
        Ok(std::mem::take(state).finish()?)
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
        let (mut body, mut with_images) = self.request_body(&request);
        let mut retry = 0;
        loop {
            let mut state = StreamState::default();
            let failure = match self
                .attempt(&body, &deltas, &cancellation, &mut state)
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            // An endpoint or model without image input rejects the request;
            // describe images from then on and ask again.
            if with_images && !state.emitted && !failure.transient && rejects_images(&failure.error)
            {
                tracing::warn!(
                    "provider rejected image input ({}); sending image notes instead",
                    failure.error.message
                );
                self.image_input.store(false, Ordering::Relaxed);
                (body, with_images) = self.request_body(&request);
                continue;
            }
            let attempts = retry + 1;
            // A retry after streamed output would repeat it, so only a
            // failure before any output is retried.
            if !failure.transient || state.emitted {
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
                () = cancellation.cancelled() => return Err(cancelled()),
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

/// Whether a provider error is about image input, such as a model without
/// vision or an image it cannot read.
fn rejects_images(error: &ProviderError) -> bool {
    error.message.to_ascii_lowercase().contains("image")
}

fn cancelled() -> ProviderError {
    ProviderError::new(ProviderErrorKind::Cancelled, "provider request cancelled")
}

#[cfg(test)]
mod tests;
