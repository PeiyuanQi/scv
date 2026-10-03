//! Socket Mode framing, hello validation, bounded I/O and liveness checks.
use super::Api;
use anyhow::{Result, anyhow, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use std::time::Duration;
use tokio::{net::TcpStream, time::Instant};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const IO_TIMEOUT: Duration = Duration::from_secs(20);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PING: Duration = Duration::from_secs(15);
const SILENCE: Duration = Duration::from_secs(45);
const MAX_FRAME: usize = 1024 * 1024;
pub(super) struct Link {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    last_heard: Instant,
    next_ping: Instant,
}
pub(super) struct Envelope {
    pub(super) envelope_id: String,
    pub(super) payload: Value,
}

impl Link {
    #[cfg(test)]
    pub(super) fn for_test(socket: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
        Self {
            socket,
            last_heard: Instant::now(),
            next_ping: Instant::now() + PING,
        }
    }
    /// Pretend nothing was heard for `silent`, as after a long pause.
    #[cfg(test)]
    pub(super) fn silent_for(&mut self, silent: Duration) {
        self.last_heard = Instant::now() - silent;
    }
    /// Start the liveness clock again after a pause in reading, such as a
    /// catch-up: silence counts only while SCV listens, and a ping goes out
    /// at once to prove the socket still works.
    pub(super) fn resume(&mut self) {
        let now = Instant::now();
        self.last_heard = now;
        self.next_ping = now;
    }
    pub(super) async fn connect(api: &Api, app_id: &str) -> Result<Self> {
        let url = api.socket_url().await?;
        Self::connect_url(url.as_str(), app_id).await
    }
    async fn connect_url(url: &str, app_id: &str) -> Result<Self> {
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(MAX_FRAME);
        config.max_frame_size = Some(MAX_FRAME);
        let (socket, _) = tokio::time::timeout(
            IO_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(url, Some(config), false),
        )
        .await
        .map_err(|_| anyhow!("Slack Socket Mode connect timed out"))?
        .map_err(|_| anyhow!("Slack Socket Mode WebSocket connection failed"))?;
        let now = Instant::now();
        let mut link = Self {
            socket,
            last_heard: now,
            next_ping: now + PING,
        };
        let hello = tokio::time::timeout(IO_TIMEOUT, link.socket.next())
            .await
            .map_err(|_| anyhow!("Slack Socket Mode hello timed out"))?
            .ok_or_else(|| anyhow!("Slack closed before Socket Mode hello"))?
            .map_err(|_| anyhow!("Slack Socket Mode hello failed"))?;
        let Message::Text(text) = hello else {
            bail!("Slack did not send Socket Mode hello")
        };
        let hello: Value =
            serde_json::from_str(&text).map_err(|_| anyhow!("invalid Slack Socket Mode hello"))?;
        if hello["type"] == "disconnect" {
            bail!("{}", disconnect_note(&hello));
        }
        if hello["type"] != "hello" || hello["connection_info"]["app_id"] != app_id {
            bail!(
                "Slack Socket Mode app does not match the bot token; supply tokens for the same app"
            )
        }
        Ok(link)
    }
    pub(super) async fn next(&mut self, until: Instant) -> Result<Option<Envelope>> {
        loop {
            let now = Instant::now();
            if now >= self.last_heard + SILENCE {
                bail!("Slack Socket Mode heartbeat timed out; reconnecting")
            }
            if now >= until {
                return Ok(None);
            }
            if now >= self.next_ping {
                self.write(Message::Ping(Vec::new().into())).await?;
                self.next_ping = Instant::now() + PING;
            }
            let wake = until.min(self.next_ping).min(self.last_heard + SILENCE);
            let Ok(message) = tokio::time::timeout_at(wake, self.socket.next()).await else {
                continue;
            };
            let message = message
                .ok_or_else(|| anyhow!("Slack closed Socket Mode; reconnecting"))?
                .map_err(|_| anyhow!("Slack Socket Mode read failed; reconnecting"))?;
            self.last_heard = Instant::now();
            match message {
                Message::Close(_) => bail!("Slack closed Socket Mode; reconnecting"),
                Message::Ping(bytes) => {
                    self.write(Message::Pong(bytes)).await?;
                }
                Message::Pong(_) => {}
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|_| anyhow!("invalid Slack Socket Mode JSON"))?;
                    if value["type"] == "disconnect" {
                        bail!("{}", disconnect_note(&value));
                    }
                    if let Some(id) = value.get("envelope_id") {
                        let id = id
                            .as_str()
                            .filter(|id| {
                                !id.is_empty()
                                    && id.len() <= 256
                                    && !id.chars().any(char::is_control)
                            })
                            .ok_or_else(|| anyhow!("invalid Slack envelope_id"))?;
                        // Other envelope types still need acknowledgement, but
                        // cannot smuggle event-shaped payloads into a session.
                        let payload = if value["type"] == "events_api" {
                            value.get("payload").cloned().unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        };
                        return Ok(Some(Envelope {
                            envelope_id: id.into(),
                            payload,
                        }));
                    }
                    if matches!(
                        value["type"].as_str(),
                        Some("events_api" | "interactive" | "slash_commands")
                    ) {
                        bail!("Slack event omitted envelope_id")
                    }
                }
                _ => {}
            }
        }
    }
    async fn write(&mut self, message: Message) -> Result<()> {
        tokio::time::timeout(WRITE_TIMEOUT, self.socket.send(message))
            .await
            .map_err(|_| anyhow!("Slack Socket Mode write timed out; reconnecting"))?
            .map_err(|_| anyhow!("Slack Socket Mode write failed; reconnecting"))
    }
    pub(super) async fn ack(&mut self, id: &str) -> Result<()> {
        self.write(Message::Text(
            serde_json::json!({"envelope_id":id}).to_string().into(),
        ))
        .await
    }
    pub(super) async fn close(&mut self) {
        let _ = tokio::time::timeout(WRITE_TIMEOUT, self.socket.close(None)).await;
    }
}
fn disconnect_note(value: &Value) -> &'static str {
    match value["reason"].as_str() {
        Some("link_disabled") => {
            "Slack Socket Mode disabled (link_disabled). Enable Socket Mode in Slack app settings; SCV will retry."
        }
        Some("warning" | "refresh_requested") => {
            "Slack requested a Socket Mode refresh; reconnecting with a fresh URL"
        }
        _ => "Slack Socket Mode disconnected; reconnecting",
    }
}
#[cfg(test)]
mod tests;
