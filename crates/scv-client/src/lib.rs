//! What every local client of the SCV daemon needs, without depending on the
//! server: the instance [`Layout`] (every path under `SCV_HOME`), the
//! [`default_socket_path`], the delegation-depth variable a delegated SCV
//! inherits, framed reading and writing ([`Connection`], [`read_frame`]),
//! private instance files ([`fs::replace_private`]), [`Secret`] values
//! that never print, byte-bounded text
//! ([`text::utf8_prefix`]), and [`control`] for daemon management requests,
//! which fail with a typed [`ControlError`].

#![forbid(unsafe_code)]

pub mod connection;
pub mod fs;
pub mod layout;
pub mod secret;
pub mod text;
pub use connection::{Connection, read_frame, write_message};
pub use layout::Layout;
pub use secret::Secret;

use anyhow::Result;
use scv_protocol::{
    ClientMessage, DaemonCommand, DaemonStatus, ErrorCode, Frame, FrameDecoder, Overflow,
    PROTOCOL_VERSION, ServerEvent,
};
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::BufReader, net::UnixStream};

/// Largest management reply, counting its line ending.
const MAX_CONTROL_FRAME_BYTES: usize = 1024 * 1024;

/// Environment variable carrying a delegated process's depth; SCV sets it on
/// every agent it starts.
pub const DELEGATION_DEPTH_VARIABLE: &str = "SCV_DELEGATION_DEPTH";

/// The delegation depth to declare in `session.start`: this process's own,
/// when an SCV started it, so a delegated client cannot reset the count by
/// connecting to a daemon.
pub fn inherited_delegation_depth() -> Option<u32> {
    parse_delegation_depth(std::env::var(DELEGATION_DEPTH_VARIABLE).ok().as_deref())
}

fn parse_delegation_depth(value: Option<&str>) -> Option<u32> {
    value
        .and_then(|value| value.trim().parse().ok())
        .filter(|depth| *depth > 0)
}

/// The daemon socket of the instance selected by `SCV_HOME`.
pub fn default_socket_path() -> Result<PathBuf> {
    Ok(Layout::from_env()?.socket())
}

/// Why a [`control`] request failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ControlError {
    /// No daemon accepted the connection: it is not running, or the socket
    /// is stale. Nothing was sent.
    Unavailable(std::io::Error),
    /// The daemon refused the request. A daemon that predates a command
    /// refuses it with [`ErrorCode::InvalidJson`], as a frame it cannot parse.
    Server {
        /// Why, as the daemon's stable code.
        code: ErrorCode,
        /// What went wrong, for people.
        message: String,
    },
    /// No answer within the helper's time limit. The daemon may still carry
    /// out a mutation; query status before retrying.
    TimedOut,
    /// The exchange broke off, or the daemon answered something this client
    /// does not understand. A mutation's outcome is unknown.
    Protocol(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(_) => formatter
                .write_str("SCV daemon unavailable; start it with `scv start` or `scv run`"),
            Self::Server { message, .. } => formatter.write_str(message),
            Self::TimedOut => formatter
                .write_str("SCV management request timed out; query status before retrying"),
            Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable(error) => Some(error),
            _ => None,
        }
    }
}

/// A bounded management exchange. Never retries mutations on ambiguous failure.
pub async fn control(path: &Path, command: DaemonCommand) -> Result<DaemonStatus, ControlError> {
    let broken = |error: std::io::Error| ControlError::Protocol(format!("{error}"));
    tokio::time::timeout(Duration::from_secs(20), async {
        let stream = UnixStream::connect(path)
            .await
            .map_err(ControlError::Unavailable)?;
        let (reader, writer) = stream.into_split();
        let mut connection = Connection::new(
            BufReader::new(reader),
            writer,
            FrameDecoder::new(MAX_CONTROL_FRAME_BYTES, Overflow::Stop),
        );
        for message in [
            ClientMessage::initialize("init", "scv-control"),
            ClientMessage::DaemonControl {
                request_id: "control".into(),
                command,
            },
        ] {
            connection.send(&message).await.map_err(broken)?;
            let bytes = match connection.read().await.map_err(broken)? {
                Frame::Line(bytes) => bytes,
                Frame::TooLarge => {
                    return Err(ControlError::Protocol(
                        "SCV status exceeds frame limit".into(),
                    ));
                }
                Frame::End | Frame::Truncated(_) => {
                    return Err(ControlError::Protocol(
                        "SCV daemon closed the management connection".into(),
                    ));
                }
            };
            let event = serde_json::from_slice::<ServerEvent>(&bytes)
                .map_err(|error| ControlError::Protocol(format!("{error}")))?;
            match event {
                ServerEvent::Initialized {
                    protocol_version: PROTOCOL_VERSION,
                    ..
                } if matches!(message, ClientMessage::Initialize { .. }) => {}
                ServerEvent::DaemonStatus { status, .. } => return Ok(status),
                ServerEvent::Error { code, message, .. } => {
                    return Err(ControlError::Server { code, message });
                }
                _ => {
                    return Err(ControlError::Protocol(
                        "unexpected SCV management response; upgrade/restart the daemon".into(),
                    ));
                }
            }
        }
        Err(ControlError::Protocol("SCV daemon omitted status".into()))
    })
    .await
    .unwrap_or(Err(ControlError::TimedOut))
}

#[cfg(test)]
mod tests;
