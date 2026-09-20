//! Shared local transport interfaces, without server policy or bridge dependencies.

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

pub fn default_socket_path() -> Result<PathBuf> {
    let root = std::env::var_os("SCV_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".scv")))
        .context("cannot determine SCV_HOME")?;
    Ok(root.join("server.sock"))
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
