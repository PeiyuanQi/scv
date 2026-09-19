//! Protocol client for the single SCV Unix-socket daemon.

use anyhow::{Context, Result, anyhow, bail};
use scv_protocol::{ClientMessage, PeerInfo, ServerEvent, PROTOCOL_VERSION};
use std::path::Path;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::{OwnedReadHalf, OwnedWriteHalf}};
use uuid::Uuid;

pub struct Session {
    stdin: OwnedWriteHalf,
    stdout: BufReader<OwnedReadHalf>,
    pub session_id: String,
    pub last_used: Instant,
}

impl Session {
    pub async fn spawn(workspace: &Path) -> Result<Self> {
        let socket = scv_server::default_socket_path()?;
        let stream = UnixStream::connect(&socket).await.with_context(|| format!(
            "SCV server not started or not found at {}. Start it with `scv start` or `scv run`",
            socket.display()
        ))?;
        let (reader, writer) = stream.into_split();
        let mut session = Self {
            stdin: writer,
            stdout: BufReader::new(reader),
            session_id: String::new(),
            last_used: Instant::now(),
        };
        write(
            &mut session.stdin,
            &ClientMessage::Initialize {
                request_id: "clawbot-init".into(),
                protocol_version: PROTOCOL_VERSION,
                client: PeerInfo {
                    name: "scv-clawbot".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            },
        )
        .await?;
        match session.event().await? {
            ServerEvent::Initialized { protocol_version, .. } if protocol_version == PROTOCOL_VERSION => {}
            ServerEvent::Error { message, .. } => bail!("ClawBot protocol initialization failed: {message}"),
            _ => bail!("ClawBot protocol initialization failed"),
        }
        write(
            &mut session.stdin,
            &ClientMessage::SessionStart {
                request_id: "clawbot-session".into(),
                cwd: workspace.display().to_string(),
                provider: None,
                model: None,
                base_url: None,
                no_tools: Some(true),
            },
        )
        .await?;
        loop {
            match session.event().await? {
                ServerEvent::SessionStarted { session_id, .. } => {
                    session.session_id = session_id;
                    break;
                }
                ServerEvent::Error { message, .. } => bail!("ClawBot protocol session failed: {message}"),
                _ => {}
            }
        }
        Ok(session)
    }

    async fn event(&mut self) -> Result<ServerEvent> {
        let mut line = Vec::new();
        loop {
            let buf = self.stdout.fill_buf().await?;
            if buf.is_empty() {
                bail!("SCV server closed the ClawBot session")
            }
            let take = buf
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buf.len(), |index| index + 1);
            if line.len() + take > 8 * 1024 * 1024 {
                bail!("ClawBot protocol frame exceeds limit")
            }
            line.extend_from_slice(&buf[..take]);
            self.stdout.consume(take);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        serde_json::from_slice(&line).context("decode SCV protocol event")
    }

    pub async fn turn(&mut self, prompt: &str, max_bytes: usize) -> Result<String> {
        self.last_used = Instant::now();
        write(
            &mut self.stdin,
            &ClientMessage::TurnStart {
                request_id: Uuid::new_v4().to_string(),
                session_id: self.session_id.clone(),
                prompt: prompt.to_owned(),
            },
        )
        .await?;
        let mut answer = String::new();
        loop {
            match self.event().await? {
                ServerEvent::AssistantCompleted { content, .. } => answer = content,
                ServerEvent::AssistantDelta { content, .. } => answer.push_str(&content),
                ServerEvent::ApprovalRequested { approval_id, .. } => {
                    write(
                        &mut self.stdin,
                        &ClientMessage::ApprovalResolve {
                            request_id: Uuid::new_v4().to_string(),
                            session_id: self.session_id.clone(),
                            approval_id,
                            approved: false,
                        },
                    )
                    .await?;
                }
                ServerEvent::TurnCompleted { .. } => {
                    return Ok(crate::split_utf8(&answer, max_bytes).join(""));
                }
                ServerEvent::TurnFailed { message, .. } | ServerEvent::Error { message, .. } => bail!("{message}"),
                ServerEvent::TurnCancelled { .. } => bail!("turn cancelled"),
                _ => {}
            }
        }
    }
}

async fn write(writer: &mut OwnedWriteHalf, message: &ClientMessage) -> Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(|error| anyhow!(error))?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
