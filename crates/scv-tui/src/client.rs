//! The connection to an SCV server: the daemon socket for the terminal UI,
//! or a child `scv server --stdio` for `scv exec`.

use std::{io, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use scv_client::{read_frame, write_message};
use scv_protocol::{ClientMessage, Frame, FrameDecoder, Overflow, ServerEvent, trim_line};
use tokio::{
    io::{AsyncBufRead, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    process::{Child, ChildStdin, ChildStdout, Command},
};
use uuid::Uuid;

pub(crate) const DEFAULT_SERVER_FRAME_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const RECONNECT_DELAY: Duration = Duration::from_secs(1);
// The server may spend three seconds stopping an active turn and three more
// draining its writer after stdin EOF. Keep the client grace strictly longer.
const SERVER_EXIT_GRACE: Duration = Duration::from_secs(7);

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub approval_policy: Option<String>,
}

pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) cwd: String,
    pub(crate) model: String,
    pub(crate) context_max_tokens: usize,
    pub(crate) max_server_frame_bytes: usize,
    pub(crate) max_transcript_bytes: usize,
    pub(crate) max_transcript_items: usize,
    pub(crate) max_prompt_history_bytes: usize,
    pub(crate) max_prompt_history_items: usize,
}

pub(crate) struct Client {
    child: Option<Child>,
    stdin: ClientOutput,
    stdout: ClientInput,
    max_server_frame: usize,
    /// Keeps a partly read frame: select! cancels reads on every UI tick.
    decoder: FrameDecoder,
}

pub(crate) enum ClientOutput {
    Child(ChildStdin),
    Socket(OwnedWriteHalf),
}

pub(crate) enum ClientInput {
    Child(BufReader<ChildStdout>),
    Socket(BufReader<OwnedReadHalf>),
}

impl Client {
    /// Connect to the daemon listening on `path` and start a session.
    pub(crate) async fn connect(
        path: &Path,
        cwd: &Path,
        options: &LaunchOptions,
    ) -> Result<(Self, SessionInfo)> {
        tokio::time::timeout(SOCKET_TIMEOUT, async {
            let stream = UnixStream::connect(path).await.with_context(|| {
                format!(
                    "SCV server not started or not found at {}. Start it with `scv start --workspace {}` or run `scv run` in another terminal",
                    path.display(),
                    cwd.display()
                )
            })?;
            let (reader, writer) = stream.into_split();
            let client = Self {
                child: None,
                stdin: ClientOutput::Socket(writer),
                stdout: ClientInput::Socket(BufReader::new(reader)),
                max_server_frame: DEFAULT_SERVER_FRAME_LIMIT,
                decoder: frame_decoder(DEFAULT_SERVER_FRAME_LIMIT),
            };
            client.initialize(cwd, options).await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "server connection timed out"))?
    }

    pub(crate) async fn spawn(cwd: &Path, options: &LaunchOptions) -> Result<(Self, SessionInfo)> {
        let executable = std::env::current_exe().context("locate scv executable")?;
        let mut command = Command::new(executable);
        if let Some(model) = &options.model {
            command.args(["--model", model]);
        }
        if let Some(provider) = &options.provider {
            command.args(["--provider", provider]);
        }
        if let Some(base_url) = &options.base_url {
            command.args(["--base-url", base_url]);
        }
        if let Some(policy) = &options.approval_policy {
            command.args(["--approval-policy", policy]);
        }
        command
            .args(["server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().context("launch scv server")?;
        let stdin = child.stdin.take().context("server stdin unavailable")?;
        let stdout = child.stdout.take().context("server stdout unavailable")?;
        let client = Self {
            child: Some(child),
            stdin: ClientOutput::Child(stdin),
            stdout: ClientInput::Child(BufReader::new(stdout)),
            max_server_frame: DEFAULT_SERVER_FRAME_LIMIT,
            decoder: frame_decoder(DEFAULT_SERVER_FRAME_LIMIT),
        };
        client.initialize(cwd, options).await
    }

    async fn initialize(
        mut self,
        cwd: &Path,
        options: &LaunchOptions,
    ) -> Result<(Self, SessionInfo)> {
        self.send(&ClientMessage::initialize("initialize", "scv-tui"))
            .await?;
        match self.read_event().await? {
            Some(ServerEvent::Initialized { .. }) => {}
            Some(ServerEvent::Error { code, message, .. }) => {
                bail!("server initialization failed ({code}): {message}")
            }
            other => bail!("unexpected server initialization response: {other:?}"),
        }
        self.send(&ClientMessage::SessionStart {
            request_id: "session-start".into(),
            cwd: cwd.display().to_string(),
            provider: options.provider.clone(),
            model: options.model.clone(),
            base_url: options.base_url.clone(),
            no_tools: None,
            delegation_depth: scv_client::inherited_delegation_depth(),
            channel: None,
            auto_approve: None,
        })
        .await?;
        let session = match self.read_event().await? {
            Some(ServerEvent::SessionStarted {
                session_id,
                cwd,
                model,
                context_max_tokens,
                max_server_frame_bytes,
                max_transcript_bytes,
                max_transcript_items,
                max_prompt_history_bytes,
                max_prompt_history_items,
                ..
            }) => SessionInfo {
                id: session_id,
                cwd,
                model,
                context_max_tokens,
                max_server_frame_bytes,
                max_transcript_bytes,
                max_transcript_items,
                max_prompt_history_bytes,
                max_prompt_history_items,
            },
            Some(ServerEvent::Error { code, message, .. }) => {
                bail!("session startup failed ({code}): {message}")
            }
            other => bail!("unexpected session startup response: {other:?}"),
        };
        self.max_server_frame = session.max_server_frame_bytes;
        self.decoder
            .set_limit(self.max_server_frame.saturating_add(2));
        Ok((self, session))
    }

    pub(crate) async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        match &mut self.stdin {
            ClientOutput::Child(writer) => write_message(writer, message).await?,
            ClientOutput::Socket(writer) => {
                // A partial write has an unknown outcome. Drop the connection on
                // timeout; retrying the message could execute a prompt twice.
                tokio::time::timeout(SOCKET_TIMEOUT, write_message(writer, message))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "server write timed out")
                    })??;
            }
        }
        Ok(())
    }

    pub(crate) async fn read_event(&mut self) -> Result<Option<ServerEvent>> {
        let frame = match &mut self.stdout {
            ClientInput::Child(reader) => {
                read_bounded_frame(reader, &mut self.decoder, self.max_server_frame).await?
            }
            ClientInput::Socket(reader) => {
                read_bounded_frame(reader, &mut self.decoder, self.max_server_frame).await?
            }
        };
        let Some(frame) = frame else {
            return Ok(None);
        };
        Ok(Some(
            serde_json::from_slice(&frame).context("decode server event")?,
        ))
    }

    pub(crate) async fn shutdown(&mut self) {
        match &mut self.stdin {
            ClientOutput::Child(writer) => {
                let _ = writer.shutdown().await;
            }
            ClientOutput::Socket(writer) => {
                let _ = writer.shutdown().await;
            }
        }
        if let Some(child) = &mut self.child
            && tokio::time::timeout(SERVER_EXIT_GRACE, child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

pub(crate) async fn reconnect_client(
    path: &Path,
    cwd: &Path,
    options: &LaunchOptions,
) -> (Client, SessionInfo) {
    loop {
        if let Ok(connection) = Client::connect(path, cwd, options).await {
            return connection;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

pub(crate) fn is_transport_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::NotConnected
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::TimedOut
        )
    })
}

/// A decoder for server frames of at most `max_bytes` before the line
/// ending.
pub(crate) fn frame_decoder(max_bytes: usize) -> FrameDecoder {
    FrameDecoder::new(max_bytes.saturating_add(2), Overflow::Stop)
}

pub(crate) async fn read_bounded_frame<R>(
    reader: &mut R,
    decoder: &mut FrameDecoder,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    match read_frame(reader, decoder).await? {
        Frame::End => Ok(None),
        Frame::Truncated(_) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server disconnected during a frame",
        )
        .into()),
        Frame::TooLarge => bail!("server frame exceeded configured client limit"),
        Frame::Line(line) => match trim_line(line, max_bytes) {
            Some(line) => Ok(Some(line)),
            None => bail!("server frame exceeded configured client limit"),
        },
    }
}

pub(crate) fn new_id() -> String {
    Uuid::new_v4().to_string()
}
