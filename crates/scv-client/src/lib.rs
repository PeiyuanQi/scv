//! What every local client of the SCV daemon needs, without depending on the
//! server: the instance [`Layout`] (every path under `SCV_HOME`), the
//! [`default_socket_path`], the delegation-depth variable a delegated SCV
//! inherits, framed reading and writing ([`Connection`], [`read_frame`]),
//! private instance files ([`fs::replace_private`]), [`Secret`] values
//! that never print, byte-bounded text
//! ([`text::utf8_prefix`]), and [`control`] for daemon management requests.

#![forbid(unsafe_code)]

pub mod connection;
pub mod fs;
pub mod layout;
pub mod secret;
pub mod text;
pub use connection::{Connection, read_frame, write_message};
pub use layout::Layout;
pub use secret::Secret;

use anyhow::{Context, Result, bail};
use scv_protocol::{
    ClientMessage, DaemonCommand, DaemonStatus, Frame, FrameDecoder, Overflow, PROTOCOL_VERSION,
    ServerEvent,
};
use std::{
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

/// A bounded management exchange. Never retries mutations on ambiguous failure.
pub async fn control(path: &Path, command: DaemonCommand) -> Result<DaemonStatus> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let stream = UnixStream::connect(path)
            .await
            .context("SCV daemon unavailable; start it with `scv start` or `scv run`")?;
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
            connection.send(&message).await?;
            let bytes = match connection.read().await? {
                Frame::Line(bytes) => bytes,
                Frame::TooLarge => bail!("SCV status exceeds frame limit"),
                Frame::End | Frame::Truncated(_) => {
                    bail!("SCV daemon closed the management connection")
                }
            };
            match serde_json::from_slice::<ServerEvent>(&bytes)? {
                ServerEvent::Initialized {
                    protocol_version: PROTOCOL_VERSION,
                    ..
                } if matches!(message, ClientMessage::Initialize { .. }) => {}
                ServerEvent::DaemonStatus { status, .. } => return Ok(status),
                ServerEvent::Error { message, .. } => bail!("{message}"),
                _ => bail!("unexpected SCV management response; upgrade/restart the daemon"),
            }
        }
        bail!("SCV daemon omitted status")
    })
    .await
    .context("SCV management request timed out; query status before retrying")?
}

#[cfg(test)]
mod tests;
