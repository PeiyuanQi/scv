//! The dashboard's own connection to the daemon, apart from every chat tab's
//! session: it sends the workbench's `daemon.control` requests one at a
//! time and hands their answers back, reconnecting when the daemon
//! restarts. It holds no session, so closing it cancels nothing.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use scv_client::{read_frame, write_message};
use scv_protocol::{
    ClientMessage, DaemonCommand, DaemonStatus, ErrorCode, Frame, FrameDecoder, Overflow,
    ServerEvent, trim_line,
};
use tokio::{
    io::BufReader,
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
};

/// The largest answer read: a snapshot or a page of a run's log.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// How long one request may take, connecting included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait before connecting again.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// A request the dashboard sends.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    /// Says what the answer is for.
    pub(crate) tag: Tag,
    pub(crate) command: DaemonCommand,
}

/// What a request is for, so its answer finds its place.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Tag {
    /// The periodic snapshot.
    Snapshot,
    /// A page of run `handle`'s log, read from byte position `after`.
    Log(String, u64),
    /// An owner's action, whose note or refusal is shown.
    Action,
    /// A prompt an agent view sent, which it gets back if refused.
    Send(crate::workbench::Target),
    /// The worktree list.
    Worktrees,
    /// A view asked to take its conversation over in its agent's terminal.
    Attach(crate::workbench::Target),
    /// An attached view's read of its screen, with what was typed.
    AttachIo(crate::workbench::Target),
    /// A view detached, or ended a takeover; the note is shown.
    AttachDone,
}

/// What the dashboard learns.
#[derive(Debug)]
pub(crate) enum Answer {
    Status(Tag, Box<DaemonStatus>),
    /// The daemon refused the request; the message is SCV's.
    Refused(Tag, String),
    /// The daemon predates the workbench.
    Unsupported,
    /// The request got no answer: the connection broke or could not be
    /// made. The poller connects again for the next request.
    Disconnected(Tag),
    /// The request was not answered before its limit. The connection is
    /// dropped, because a late reply would be read as the next request's;
    /// the daemon may still be up, and the action may have taken effect.
    TimedOut(Tag),
}

/// Start the poller for the daemon at `socket`: requests go in, answers
/// come out, until the dashboard drops its sender.
pub(crate) fn spawn(
    socket: PathBuf,
) -> (
    mpsc::UnboundedSender<Request>,
    mpsc::UnboundedReceiver<Answer>,
) {
    let (requests, mut incoming) = mpsc::unbounded_channel::<Request>();
    let (answers, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut connection: Option<Connection> = None;
        // After a failed connection, requests until then are answered at
        // once, so none wait in line behind the retries.
        let mut down_until: Option<Instant> = None;
        while let Some(request) = incoming.recv().await {
            if connection.is_none() {
                if down_until.is_some_and(|until| Instant::now() < until) {
                    if answers.send(Answer::Disconnected(request.tag)).is_err() {
                        return;
                    }
                    continue;
                }
                connection = Connection::open(&socket).await;
                if connection.is_none() {
                    down_until = Some(Instant::now() + RETRY_DELAY);
                    if answers.send(Answer::Disconnected(request.tag)).is_err() {
                        return;
                    }
                    continue;
                }
                down_until = None;
            }
            let Some(open) = connection.as_mut() else {
                continue;
            };
            let answer = match tokio::time::timeout(
                scv_client::control_timeout(&request.command),
                open.ask(&request.command),
            )
            .await
            {
                Ok(Ok(Reply::Status(status))) => Answer::Status(request.tag, status),
                Ok(Ok(Reply::Error(ErrorCode::InvalidJson, _))) => Answer::Unsupported,
                Ok(Ok(Reply::Error(_, message))) => Answer::Refused(request.tag, message),
                Ok(Err(())) => {
                    connection = None;
                    Answer::Disconnected(request.tag)
                }
                Err(_elapsed) => {
                    // A late reply would be read as the next request's.
                    connection = None;
                    Answer::TimedOut(request.tag)
                }
            };
            if answers.send(answer).is_err() {
                return;
            }
        }
    });
    (requests, received)
}

enum Reply {
    Status(Box<DaemonStatus>),
    Error(ErrorCode, String),
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    decoder: FrameDecoder,
    next: u64,
}

impl Connection {
    async fn open(socket: &std::path::Path) -> Option<Self> {
        let stream = tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(socket))
            .await
            .ok()?
            .ok()?;
        let (reader, writer) = stream.into_split();
        let mut connection = Self {
            reader: BufReader::new(reader),
            writer,
            decoder: FrameDecoder::new(MAX_FRAME_BYTES + 2, Overflow::Stop),
            next: 0,
        };
        write_message(
            &mut connection.writer,
            &ClientMessage::initialize("workbench-init", "scv-tui"),
        )
        .await
        .ok()?;
        match tokio::time::timeout(REQUEST_TIMEOUT, connection.event()).await {
            Ok(Ok(ServerEvent::Initialized { .. })) => Some(connection),
            _ => None,
        }
    }

    async fn event(&mut self) -> Result<ServerEvent, ()> {
        match read_frame(&mut self.reader, &mut self.decoder).await {
            Ok(Frame::Line(line)) => {
                let line = trim_line(line, MAX_FRAME_BYTES).ok_or(())?;
                serde_json::from_slice(&line).map_err(|_| ())
            }
            _ => Err(()),
        }
    }

    async fn ask(&mut self, command: &DaemonCommand) -> Result<Reply, ()> {
        self.next += 1;
        let request_id = format!("workbench-{}", self.next);
        write_message(
            &mut self.writer,
            &ClientMessage::DaemonControl {
                request_id: request_id.clone(),
                command: command.clone(),
            },
        )
        .await
        .map_err(|_| ())?;
        loop {
            match self.event().await? {
                ServerEvent::DaemonStatus {
                    request_id: answered,
                    status,
                } if answered == request_id => return Ok(Reply::Status(Box::new(status))),
                ServerEvent::Error {
                    request_id: Some(answered),
                    code,
                    message,
                    ..
                } if answered == request_id => return Ok(Reply::Error(code, message)),
                // A connection-level error answers the request too.
                ServerEvent::Error {
                    request_id: None,
                    code,
                    message,
                    ..
                } => return Ok(Reply::Error(code, message)),
                _ => {}
            }
        }
    }
}
