//! The JSON-RPC 2.0 connection to an ACP server: requests, their
//! responses, and the agent's own requests and notifications in between.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use scv_core::ToolError;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::{
    args::bounded,
    delegate::{
        live::{LiveChild, LiveLine},
        progress::redact,
    },
};

/// JSON-RPC "method not found".
pub(super) const METHOD_NOT_FOUND: i64 = -32601;

/// A JSON-RPC connection to an ACP server.
#[derive(Debug)]
pub(super) struct Rpc {
    pub(super) live: Arc<LiveChild>,
    pub(super) next_id: AtomicU64,
}

/// One message from the agent.
pub(super) enum Incoming {
    Response {
        id: Value,
        outcome: Result<Value, Value>,
    },
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// Why waiting for the agent stopped early.
pub(super) enum Interrupt {
    TimedOut,
    Cancelled,
    /// The agent exited or broke the protocol.
    Lost(String),
}

impl Rpc {
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<u64, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.live
            .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), ToolError> {
        self.live
            .send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    pub(super) async fn respond(&self, id: Value, result: Value) -> Result<(), ToolError> {
        self.live
            .send(&json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }

    pub(super) async fn refuse(&self, id: Value, method: &str) -> Result<(), ToolError> {
        self.live
            .send(&json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{"code":METHOD_NOT_FOUND,"message":format!("SCV does not provide {method}")}
            }))
            .await
    }

    /// The next message, waiting until `deadline` or cancellation.
    pub(super) async fn next(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Incoming, Interrupt> {
        let line = tokio::select! {
            line = timeout_at(deadline, self.live.recv()) => match line {
                Ok(line) => line,
                Err(_) => return Err(Interrupt::TimedOut),
            },
            () = cancellation.cancelled() => return Err(Interrupt::Cancelled),
        };
        let bytes = match line {
            Some(LiveLine::Line(bytes)) => bytes,
            Some(LiveLine::TooLong) => {
                return Err(Interrupt::Lost(
                    "the agent sent a JSON-RPC message larger than SCV accepts".into(),
                ));
            }
            None => return Err(Interrupt::Lost("the agent exited".into())),
        };
        let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(&bytes) else {
            return Err(Interrupt::Lost(
                "the agent sent a line that is not a JSON-RPC message".into(),
            ));
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let params = message.remove("params").unwrap_or(Value::Null);
        Ok(match (message.remove("id"), method) {
            (Some(id), Some(method)) => Incoming::Request { id, method, params },
            (None, Some(method)) => Incoming::Notification { method, params },
            (Some(id), None) => Incoming::Response {
                id,
                outcome: match message.remove("error") {
                    Some(error) => Err(error),
                    None => Ok(message.remove("result").unwrap_or(Value::Null)),
                },
            },
            (None, None) => {
                return Err(Interrupt::Lost(
                    "the agent sent a JSON-RPC message with neither id nor method".into(),
                ));
            }
        })
    }

    /// Call `method` outside a prompt turn and wait for its result. The agent
    /// may not ask for anything meanwhile: permission requests are cancelled
    /// and other requests refused.
    pub(super) async fn call(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Value, CallError> {
        let id = self.request(method, params).await.map_err(CallError::Io)?;
        loop {
            match self.next(deadline, cancellation).await {
                Ok(Incoming::Response { id: reply, outcome }) if reply == json!(id) => {
                    return outcome.map_err(CallError::Rpc);
                }
                Ok(Incoming::Request { id, method, .. }) => {
                    let sent = if method == "session/request_permission" {
                        self.respond(id, json!({"outcome":{"outcome":"cancelled"}}))
                            .await
                    } else {
                        self.refuse(id, &method).await
                    };
                    sent.map_err(CallError::Io)?;
                }
                Ok(_) => {}
                Err(interrupt) => return Err(CallError::Interrupted(interrupt)),
            }
        }
    }
}

pub(super) enum CallError {
    Io(ToolError),
    Rpc(Value),
    Interrupted(Interrupt),
}

/// A JSON-RPC error as one bounded, redacted line.
pub(super) fn describe_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let detail = match error.get("data") {
        Some(Value::String(data)) => Some(data.clone()),
        Some(Value::Object(data)) => data
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    };
    let text = match detail {
        Some(detail) if !message.contains(detail.as_str()) => format!("{message}: {detail}"),
        _ => message.to_owned(),
    };
    bounded(&redact(&text.replace(['\n', '\r'], " ")), 1000)
}
