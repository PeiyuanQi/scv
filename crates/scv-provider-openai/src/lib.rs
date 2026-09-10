//! Streaming OpenAI-compatible chat-completions provider.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use scv_core::{
    AssistantResponse, Message, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    TextDeltaSink, ToolCall, Usage,
};
use reqwest::Client;
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
}

impl Default for ProviderLimits {
    fn default() -> Self {
        Self {
            max_sse_event_bytes: 1024 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_assistant_bytes: 1024 * 1024,
            max_tool_calls: 32,
            max_tool_arguments_bytes: 256 * 1024,
        }
    }
}

pub struct OpenAiProvider {
    client: Client,
    model: String,
    base_url: String,
    api_key: String,
    limits: ProviderLimits,
    headers: std::collections::HashMap<String, String>,
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
        })
    }

    fn request_body(&self, request: &ProviderRequest) -> Value {
        let input: Vec<Value> = request.messages.iter().map(message_to_response_json).collect();
        let tools: Vec<Value> = request.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect();
        let mut body = json!({"model": self.model, "instructions": request.system_prompt, "input": input, "stream": true});
        if !tools.is_empty() { body["tools"] = Value::Array(tools); }
        body
    }

    async fn response_error(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> ProviderError {
        let status = response.status();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::with_capacity(4096);
        while bytes.len() < 4096 {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return cancelled(),
            };
            let Some(chunk) = chunk else {
                break;
            };
            let Ok(chunk) = chunk else { break };
            let remaining = 4096 - bytes.len();
            bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        let body = String::from_utf8_lossy(&bytes).replace(&self.api_key, "[REDACTED]");
        ProviderError::new(
            ProviderErrorKind::Provider,
            format!("provider returned HTTP {status}: {body}"),
        )
    }
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
        let response = tokio::select! {
            result = self.client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&self.api_key)
                .headers(self.headers.clone().into_iter().filter_map(|(k,v)| Some((k.parse().ok()?, v.parse().ok()?))).collect())
                .json(&self.request_body(&request))
                .send() => result.map_err(|error| ProviderError::new(ProviderErrorKind::Provider, error.to_string()))?,
            _ = cancellation.cancelled() => return Err(cancelled()),
        };
        if !response.status().is_success() {
            return Err(self.response_error(response, &cancellation).await);
        }

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut response_bytes = 0usize;
        let mut content = String::new();
        let mut calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let usage = Usage::default();
        let mut done = false;

        while !done {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancellation.cancelled() => return Err(cancelled()),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|error| {
                ProviderError::new(ProviderErrorKind::Provider, error.to_string())
            })?;
            response_bytes = response_bytes.saturating_add(chunk.len());
            if response_bytes > self.limits.max_response_bytes {
                return Err(limit_error("provider response exceeded byte limit"));
            }
            buffer.extend_from_slice(&chunk);
            if buffer.len() > self.limits.max_sse_event_bytes
                && find_event_boundary(&buffer).is_none()
            {
                return Err(limit_error("provider SSE event exceeded byte limit"));
            }
            while let Some((end, separator_len)) = find_event_boundary(&buffer) {
                let event = buffer.drain(..end).collect::<Vec<_>>();
                buffer.drain(..separator_len);
                if event.len() > self.limits.max_sse_event_bytes {
                    return Err(limit_error("provider SSE event exceeded byte limit"));
                }
                for line in event.split(|byte| *byte == b'\n') {
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
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
                    let event: ResponseEvent = serde_json::from_slice(data).map_err(|error| ProviderError::new(ProviderErrorKind::Provider, format!("invalid provider stream JSON: {error}")))?;
                    if event.event_type == "response.output_text.delta" {
                        if let Some(delta) = event.delta {
                            if content.len().saturating_add(delta.len())
                                > self.limits.max_assistant_bytes
                            {
                                return Err(limit_error("assistant response exceeded byte limit"));
                            }
                            content.push_str(&delta);
                            deltas.push(&delta).await?;
                        }
                    } else if event.event_type == "response.function_call_arguments.delta" {
                        if let Some(delta) = event.delta { let call = calls.entry(event.output_index.unwrap_or(0)).or_default(); call.arguments.push_str(&delta); }
                    } else if event.event_type == "response.output_item.done" {
                        if let Some(item) = event.item { if item.kind.as_deref() == Some("function_call") { let call = calls.entry(event.output_index.unwrap_or(0)).or_default(); call.id = item.call_id.unwrap_or_default(); call.name = item.name.unwrap_or_default(); } }
                    } else if event.event_type == "response.completed" { if let Some(summary) = event.response.and_then(|r| r.usage) { /* usage is reported by the server event */ let _ = summary; } done = true; }
                }
            }
        }

        if !buffer.iter().all(u8::is_ascii_whitespace) {
            return Err(ProviderError::new(
                ProviderErrorKind::Provider,
                "provider stream ended with an incomplete SSE event",
            ));
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
                let arguments = serde_json::from_str(if call.arguments.is_empty() { "{}" } else { &call.arguments }).map_err(|error| {
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

fn message_to_response_json(message: &Message) -> Value { match message { Message::User{content} => json!({"role":"user","content":content}), Message::Assistant{content,..} => json!({"role":"assistant","content":content}), Message::Tool{call_id,content,..} => json!({"type":"function_call_output","call_id":call_id,"output":content}), Message::HistoryNote{content} => json!({"role":"user","content":content}) } }

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
struct ResponseEvent { #[serde(rename="type")] event_type: String, delta: Option<String>, output_index: Option<usize>, item: Option<ResponseItem>, response: Option<ResponseSummary> }
#[derive(Debug, Deserialize)]
struct ResponseSummary { usage: Option<ResponseUsage> }
#[derive(Debug, Deserialize)]
struct ResponseUsage { _input_tokens: Option<u64>, _output_tokens: Option<u64> }
#[derive(Debug, Deserialize)]
struct ResponseItem { #[serde(rename="type")] kind: Option<String>, _id: Option<String>, call_id: Option<String>, name: Option<String> }

