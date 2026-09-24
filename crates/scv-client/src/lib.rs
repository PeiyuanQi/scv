//! Shared local transport interfaces and the instance layout, without server
//! policy or bridge dependencies.

pub mod layout;
pub use layout::Layout;

use anyhow::{Context, Result, bail};
use scv_protocol::{
    ClientMessage, DaemonCommand, DaemonStatus, PROTOCOL_VERSION, PeerInfo, ServerEvent,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

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
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        for message in [
            ClientMessage::Initialize {
                request_id: "init".into(),
                protocol_version: PROTOCOL_VERSION,
                client: PeerInfo {
                    name: "scv-control".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            },
            ClientMessage::DaemonControl {
                request_id: "control".into(),
                command,
            },
        ] {
            let mut frame = serde_json::to_vec(&message)?;
            frame.push(b'\n');
            writer.write_all(&frame).await?;
            let mut bytes = Vec::new();
            loop {
                let buf = reader.fill_buf().await?;
                if buf.is_empty() {
                    bail!("SCV daemon closed the management connection");
                }
                let take = buf
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(buf.len(), |n| n + 1);
                if bytes.len() + take > 1024 * 1024 {
                    bail!("SCV status exceeds frame limit");
                }
                bytes.extend_from_slice(&buf[..take]);
                reader.consume(take);
                if bytes.last() == Some(&b'\n') {
                    break;
                }
            }
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
mod tests {
    use super::*;

    #[test]
    fn only_a_positive_inherited_depth_is_declared() {
        assert_eq!(parse_delegation_depth(Some("2")), Some(2));
        assert_eq!(parse_delegation_depth(Some(" 1\n")), Some(1));
        assert_eq!(parse_delegation_depth(Some("0")), None);
        assert_eq!(parse_delegation_depth(Some("deep")), None);
        assert_eq!(parse_delegation_depth(None), None);
    }
}
