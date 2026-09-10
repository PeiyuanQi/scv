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
        let mut messages = vec![json!({
            "role": "system",
            "content": request.system_prompt,
        })];
        messages.extend(request.messages.iter().map(message_to_json));
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = Value::String("auto".into());
        }
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
                .post(format!("{}/chat/completions", self.base_url))
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
        let mut usage = Usage::default();
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
                    let chunk: StreamChunk = serde_json::from_slice(data).map_err(|error| {
                        ProviderError::new(
                            ProviderErrorKind::Provider,
                            format!("invalid provider stream JSON: {error}"),
                        )
                    })?;
                    if let Some(chunk_usage) = chunk.usage {
                        usage.input_tokens = chunk_usage.prompt_tokens;
                        usage.output_tokens = chunk_usage.completion_tokens;
                    }
                    for choice in chunk.choices {
                        if let Some(delta) = choice.delta.content {
                            if content.len().saturating_add(delta.len())
                                > self.limits.max_assistant_bytes
                            {
                                return Err(limit_error("assistant response exceeded byte limit"));
                            }
                            content.push_str(&delta);
                            deltas.push(&delta).await?;
                        }
                        for call in choice.delta.tool_calls {
                            if !calls.contains_key(&call.index)
                                && calls.len() >= self.limits.max_tool_calls
                            {
                                return Err(ProviderError::new(
                                    ProviderErrorKind::ToolLimit,
                                    "provider returned too many tool calls",
                                ));
                            }
                            let partial = calls.entry(call.index).or_default();
                            if let Some(id) = call.id {
                                partial.id.push_str(&id);
                            }
                            if let Some(function) = call.function {
                                if let Some(name) = function.name {
                                    partial.name.push_str(&name);
                                }
                                if let Some(arguments) = function.arguments {
                                    if partial.arguments.len().saturating_add(arguments.len())
                                        > self.limits.max_tool_arguments_bytes
                                    {
                                        return Err(ProviderError::new(
                                            ProviderErrorKind::ToolLimit,
                                            "tool arguments exceeded byte limit",
                                        ));
                                    }
                                    partial.arguments.push_str(&arguments);
                                }
                            }
                        }
                    }
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
                let arguments = serde_json::from_str(&call.arguments).map_err(|error| {
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

fn message_to_json(message: &Message) -> Value {
    match message {
        Message::User { content } => json!({"role":"user", "content":content}),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let calls: Vec<Value> = tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type":"function",
                        "function": {
                            "name":call.name,
                            "arguments":call.arguments.to_string()
                        }
                    })
                })
                .collect();
            let mut message = json!({"role":"assistant", "content":content});
            if !calls.is_empty() {
                message["tool_calls"] = Value::Array(calls);
            }
            message
        }
        Message::Tool {
            call_id, content, ..
        } => json!({"role":"tool", "tool_call_id":call_id, "content":content}),
        Message::HistoryNote { content } => json!({"role":"system", "content":content}),
    }
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
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Debug, Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex,
        thread,
    };

    use super::*;

    struct CollectDeltas(Mutex<String>);

    #[async_trait]
    impl TextDeltaSink for CollectDeltas {
        async fn push(&self, delta: &str) -> Result<(), ProviderError> {
            self.0.lock().unwrap().push_str(delta);
            Ok(())
        }
    }

    #[test]
    fn finds_lf_and_crlf_boundaries() {
        assert_eq!(find_event_boundary(b"data: x\n\nrest"), Some((7, 2)));
        assert_eq!(find_event_boundary(b"data: x\r\n\r\nrest"), Some((7, 4)));
    }

    #[test]
    fn serializes_tool_results_for_chat_completions() {
        let value = message_to_json(&Message::Tool {
            call_id: "call-1".into(),
            name: "read".into(),
            content: "ok".into(),
            is_error: false,
        });
        assert_eq!(value["tool_call_id"], "call-1");
    }

    #[test]
    fn omits_tool_fields_when_no_tools_are_registered() {
        let provider = OpenAiProvider::new(
            "test".into(),
            "http://localhost".into(),
            "secret".into(),
            Duration::from_secs(1),
            ProviderLimits::default(),
            std::collections::HashMap::new(),
        )
        .unwrap();
        let body = provider.request_body(&ProviderRequest {
            system_prompt: "test".into(),
            messages: Vec::new(),
            tools: Vec::new(),
        });
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[tokio::test]
    async fn redacts_credentials_from_provider_error_bodies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let secret = "super-secret-provider-key";
        let body = format!("upstream echoed Authorization: Bearer {secret}");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 16 * 1024];
            let _ = stream.read(&mut request).unwrap();
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let provider = OpenAiProvider::new(
            "test".into(),
            format!("http://{address}"),
            secret.into(),
            Duration::from_secs(5),
            ProviderLimits::default(),
            std::collections::HashMap::new(),
        )
        .unwrap();
        let error = provider
            .complete(
                ProviderRequest {
                    system_prompt: "test".into(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                },
                Arc::new(CollectDeltas(Mutex::new(String::new()))),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_slow_error_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 16 * 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx",
                )
                .unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let provider = OpenAiProvider::new(
            "test".into(),
            format!("http://{address}"),
            "secret".into(),
            Duration::from_secs(5),
            ProviderLimits::default(),
            std::collections::HashMap::new(),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let request = provider.complete(
            ProviderRequest {
                system_prompt: "test".into(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
            Arc::new(CollectDeltas(Mutex::new(String::new()))),
            cancellation,
        );
        let cancel_soon = async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(request, cancel_soon);
        server.join().unwrap();
        assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Cancelled);
    }

    #[tokio::test]
    async fn assembles_streamed_text_tools_and_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]}}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":4}}\n\n",
            "data: [DONE]\n\n"
        );
        let body_owned = body.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 16 * 1024];
            let _ = stream.read(&mut request).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_owned.len(),
                body_owned
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let provider = OpenAiProvider::new(
            "test".into(),
            format!("http://{address}"),
            "secret".into(),
            Duration::from_secs(5),
            ProviderLimits::default(),
            std::collections::HashMap::new(),
        )
        .unwrap();
        let deltas = Arc::new(CollectDeltas(Mutex::new(String::new())));
        let response = provider
            .complete(
                ProviderRequest {
                    system_prompt: "test".into(),
                    messages: vec![Message::User {
                        content: "hello".into(),
                    }],
                    tools: Vec::new(),
                },
                deltas.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.content, "Hello");
        assert_eq!(&*deltas.0.lock().unwrap(), "Hello");
        assert_eq!(response.tool_calls[0].name, "read");
        assert_eq!(response.tool_calls[0].arguments["path"], "README.md");
        assert_eq!(response.usage.input_tokens, Some(12));
    }
}
