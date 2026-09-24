//! Unit tests for `src/request.rs`, including whole requests against a local
//! HTTP server.

use async_trait::async_trait;
use scv_core::{ImageInput, Message, ProviderRequest, ToolCall};

use super::*;

mod stream_errors;

pub(super) fn image_message(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Message {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    Message::User {
        content: format!("look at {name}"),
        images: vec![ImageInput {
            path,
            mime: "image/png".into(),
        }],
    }
}

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
    Message::user(content)
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
    let (body, _) = provider.request_body(&ProviderRequest {
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
    let (body, _) = provider.request_body(&ProviderRequest {
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
    let (body, _) = provider.request_body(&request(vec![scv_core::ToolSpec {
        name: "read".into(),
        description: "Read".into(),
        parameters: json!({"type":"object"}),
    }]));
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][1], json!({"type":"web_search"}));
    let (body, _) = provider.request_body(&request(Vec::new()));
    assert_eq!(body["tools"], json!([{"type":"web_search"}]));
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
