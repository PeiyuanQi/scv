//! The event long connection: one WebSocket to the host Feishu names, with
//! pings at the server's interval, reassembly of split events, and
//! acknowledgements the caller sends once an event is safely handled.

use crate::feishu::{
    api::Api,
    frame::{self, Fragments, Frame},
};
use anyhow::{Result, anyhow, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use prost::Message as _;
use std::time::Duration;
use tokio::{net::TcpStream, time::Instant};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{self, protocol::WebSocketConfig},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The server's default when it names none.
const DEFAULT_PING: Duration = Duration::from_secs(120);
/// Silence beyond two ping intervals and this grace means the peer is gone.
const SILENCE_GRACE: Duration = Duration::from_secs(30);

/// One connected socket.
pub(crate) struct Link {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    service: i32,
    ping_every: Duration,
    next_ping: Instant,
    last_heard: Instant,
    fragments: Fragments,
}

/// An event received on the socket, whole, with the frame to acknowledge.
pub(crate) struct Delivery {
    pub(crate) frame: Frame,
    pub(crate) payload: Vec<u8>,
    pub(crate) received: Instant,
}

impl Link {
    /// Ask Feishu for a socket address, check it, and connect.
    pub(crate) async fn connect(api: &Api) -> Result<Self> {
        let (url, ping) = api.socket_endpoint().await?;
        let service = url
            .query_pairs()
            .find(|(key, _)| key == "service_id")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(frame::MAX_EVENT_BYTES + 64 * 1024);
        config.max_frame_size = Some(frame::MAX_EVENT_BYTES + 64 * 1024);
        let (socket, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(url.as_str(), Some(config), false),
        )
        .await
        .map_err(|_| anyhow!("Feishu long connection timed out"))?
        .map_err(|error| anyhow!("Feishu long connection failed: {}", describe(&error)))?;
        let now = Instant::now();
        let ping_every = ping.unwrap_or(DEFAULT_PING);
        let mut link = Self {
            socket,
            service,
            ping_every,
            next_ping: now + ping_every,
            last_heard: now,
            fragments: Fragments::default(),
        };
        // The SDKs ping as soon as they connect.
        link.write(Frame::ping(service)).await?;
        Ok(link)
    }

    /// The next whole event, or `None` once `until` passes. Pings go out
    /// when due; pongs and other control frames are consumed here.
    pub(crate) async fn next(&mut self, until: Instant) -> Result<Option<Delivery>> {
        loop {
            let silent_since = self.last_heard + self.ping_every * 2 + SILENCE_GRACE;
            if Instant::now() >= silent_since {
                bail!("Feishu long connection went silent")
            }
            if Instant::now() >= self.next_ping {
                self.write(Frame::ping(self.service)).await?;
                self.next_ping = Instant::now() + self.ping_every;
            }
            let wake = until.min(self.next_ping).min(silent_since);
            let message = tokio::select! {
                message = self.socket.next() => message,
                () = tokio::time::sleep_until(wake) => {
                    if Instant::now() >= until {
                        return Ok(None);
                    }
                    continue;
                }
            };
            let message = match message {
                Some(Ok(message)) => message,
                Some(Err(error)) => bail!("Feishu long connection failed: {}", describe(&error)),
                None => bail!("Feishu closed the long connection"),
            };
            self.last_heard = Instant::now();
            let bytes = match message {
                tungstenite::Message::Binary(bytes) => bytes,
                tungstenite::Message::Close(_) => bail!("Feishu closed the long connection"),
                // Protocol pings are answered by the library; text is unused.
                _ => continue,
            };
            let Ok(frame) = Frame::decode(bytes.as_ref()) else {
                tracing::warn!("Feishu sent an undecodable frame; ignoring it");
                continue;
            };
            match frame.method {
                frame::METHOD_CONTROL => self.control(&frame),
                frame::METHOD_DATA => {
                    // Card callbacks and other types need no answer here.
                    if frame.header("type") != Some("event") {
                        continue;
                    }
                    if let Some(payload) = self.fragments.accept(&frame) {
                        return Ok(Some(Delivery {
                            frame,
                            payload,
                            received: Instant::now(),
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    /// Tell Feishu an event was handled.
    pub(crate) async fn acknowledge(&mut self, delivery: Delivery) -> Result<()> {
        let handled_in = delivery.received.elapsed();
        self.write(delivery.frame.acknowledgement(handled_in)).await
    }

    /// A pong may carry new client settings, such as the ping interval.
    fn control(&mut self, frame: &Frame) {
        if frame.header("type") != Some("pong") {
            return;
        }
        let ping = frame
            .payload
            .as_deref()
            .and_then(|payload| serde_json::from_slice::<serde_json::Value>(payload).ok())
            .and_then(|config| {
                config
                    .get("PingInterval")
                    .and_then(serde_json::Value::as_u64)
            })
            .filter(|seconds| (5..=3600).contains(seconds));
        if let Some(seconds) = ping {
            self.ping_every = Duration::from_secs(seconds);
        }
    }

    async fn write(&mut self, frame: Frame) -> Result<()> {
        let bytes = frame.encode_to_vec();
        tokio::time::timeout(
            WRITE_TIMEOUT,
            self.socket.send(tungstenite::Message::Binary(bytes.into())),
        )
        .await
        .map_err(|_| anyhow!("Feishu long connection write timed out"))?
        .map_err(|error| anyhow!("Feishu long connection failed: {}", describe(&error)))
    }
}

/// A connection error without URLs, which carry one-time access keys.
fn describe(error: &tungstenite::Error) -> String {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            let reason = response
                .headers()
                .get("Handshake-Msg")
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 200 && value.chars().all(|c| !c.is_control()))
                .unwrap_or_default();
            format!("handshake refused with status {status} {reason}")
                .trim_end()
                .to_owned()
        }
        tungstenite::Error::Url(_) => "invalid socket address".into(),
        tungstenite::Error::Io(error) => format!("I/O error: {}", error.kind()),
        tungstenite::Error::Tls(_) => "TLS error".into(),
        tungstenite::Error::Capacity(_) => "message too large".into(),
        tungstenite::Error::Protocol(error) => format!("protocol error: {error}"),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            "connection closed".into()
        }
        _ => "connection error".into(),
    }
}
