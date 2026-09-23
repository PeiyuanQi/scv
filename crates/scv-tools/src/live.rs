//! Long-running delegated agents: one child process per conversation, spoken
//! to in newline-delimited JSON over its stdin and stdout.
//!
//! [`LiveChild`] is protocol-neutral. It starts the child in its adapter
//! environment and process group, records it as a delegation (so `scv agents
//! ps`, `kill`, reconcile, and orphan reaping apply), frames its stdout into
//! bounded lines, and shuts it down: closing stdin, a grace period, then a
//! group kill. A protocol client such as the SCV one in `scv_agent` (or an
//! ACP JSON-RPC client) runs on top of it.

use std::{
    ffi::OsString,
    os::unix::process::CommandExt as _,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use scv_core::ToolError;
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc},
};

use crate::{
    agent_output::TailBuffer,
    apply_agent_environment,
    delegation::{
        self, DelegationGuard, DelegationRegistry, PendingDelegation, STOP_GRACE, group_exists,
        signal_group,
    },
    drain_output,
};

/// Last bytes of a live child's stderr kept for failure reports.
const STDERR_TAIL_BYTES: usize = 4096;
/// Lines read ahead of the protocol client. The child is idle between turns,
/// so this only buffers a burst of events within one turn.
const LINE_QUEUE: usize = 256;

/// How to start a live child.
pub(crate) struct LiveSpec {
    pub executable: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// The adapter environment plus the delegation tags.
    pub environment: Vec<(OsString, OsString)>,
    /// Longest stdout line accepted; a longer one ends the child.
    pub max_line_bytes: usize,
}

/// One line from a live child's stdout.
#[derive(Debug)]
pub(crate) enum LiveLine {
    Line(Vec<u8>),
    /// A line longer than [`LiveSpec::max_line_bytes`]; nothing more is read.
    TooLong,
}

/// A running live child. Dropping the last reference shuts it down in the
/// background; [`LiveChild::close`] does the same and waits for it.
pub(crate) struct LiveChild {
    pid: u32,
    stdin: Mutex<Option<ChildStdin>>,
    lines: Mutex<mpsc::Receiver<LiveLine>>,
    stderr: Arc<Mutex<TailBuffer>>,
    child: StdMutex<Option<Child>>,
    guard: StdMutex<Option<DelegationGuard>>,
    closed: AtomicBool,
}

impl std::fmt::Debug for LiveChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveChild")
            .field("pid", &self.pid)
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl LiveChild {
    /// Start the child in its own process group with the agent environment,
    /// recording it under `registration` when given. Recording never fails
    /// the start: an unrecorded child is still tagged for later sweeps.
    pub(crate) fn spawn(
        spec: LiveSpec,
        registration: Option<(Arc<DelegationRegistry>, PendingDelegation)>,
    ) -> Result<Arc<Self>, ToolError> {
        let mut command = Command::new(&spec.executable);
        apply_agent_environment(command.as_std_mut(), &spec.environment);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError(format!("launch {:?}: {error}", spec.executable)))?;
        let pid = child
            .id()
            .ok_or_else(|| ToolError("child process has no pid".into()))?;
        delegation::track_spawned(pid);
        let guard =
            registration.and_then(|(registry, pending)| registry.register(pending, pid).ok());
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = Arc::new(Mutex::new(TailBuffer::new(STDERR_TAIL_BYTES)));
        if let Some(reader) = child.stderr.take() {
            tokio::spawn(drain_output(reader, Arc::clone(&stderr)));
        }
        let (sender, receiver) = mpsc::channel(LINE_QUEUE);
        if let Some(stdout) = stdout {
            tokio::spawn(read_lines(stdout, sender, spec.max_line_bytes));
        }
        Ok(Arc::new(Self {
            pid,
            stdin: Mutex::new(stdin),
            lines: Mutex::new(receiver),
            stderr,
            child: StdMutex::new(Some(child)),
            guard: StdMutex::new(guard),
            closed: AtomicBool::new(false),
        }))
    }

    /// Write `message` as one JSON line.
    pub(crate) async fn send(&self, message: &impl Serialize) -> Result<(), ToolError> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|error| ToolError(format!("encode message: {error}")))?;
        bytes.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let writer = stdin
            .as_mut()
            .ok_or_else(|| ToolError("the child's input is closed".into()))?;
        writer
            .write_all(&bytes)
            .await
            .map_err(|error| ToolError(format!("write to child: {error}")))?;
        writer
            .flush()
            .await
            .map_err(|error| ToolError(format!("write to child: {error}")))
    }

    /// The next stdout line, or `None` once the child closed its output.
    pub(crate) async fn recv(&self) -> Option<LiveLine> {
        self.lines.lock().await.recv().await
    }

    /// The end of the child's stderr.
    pub(crate) async fn stderr_tail(&self) -> String {
        self.stderr.lock().await.text()
    }

    /// Whether the child process is still running.
    pub(crate) fn is_running(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// Record the conversation turn this child is serving.
    pub(crate) fn set_turn(&self, turn: u32) {
        if let Some(guard) = self
            .guard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            guard.set_turn(turn);
        }
    }

    /// Shut the child down and wait for it: close its input, give it
    /// [`STOP_GRACE`] to exit, then kill its process group and anything
    /// still tagged with its delegation.
    pub(crate) async fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let stdin = self.stdin.lock().await.take();
        let child = self
            .child
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        let guard = self
            .guard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        shut_down(self.pid, stdin, child, guard).await;
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let stdin = self.stdin.get_mut().take();
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        let guard = self
            .guard
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        let pid = self.pid;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(shut_down(pid, stdin, child, guard));
            }
            Err(_) => {
                // No runtime to wait in: kill at once. The guard's drop
                // sweeps tagged leftovers.
                signal_group(pid, libc::SIGKILL);
                delegation::untrack_spawned(pid);
                drop(guard);
            }
        }
    }
}

async fn shut_down(
    pid: u32,
    stdin: Option<ChildStdin>,
    child: Option<Child>,
    guard: Option<DelegationGuard>,
) {
    // Closing input asks the child to finish; an SCV server exits on EOF.
    drop(stdin);
    if let Some(mut child) = child {
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        let _ = tokio::time::timeout_at(deadline, child.wait()).await;
        // Descendants may outlive the leader, so the group always goes.
        signal_group(pid, libc::SIGKILL);
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    } else if group_exists(pid) {
        signal_group(pid, libc::SIGKILL);
    }
    delegation::untrack_spawned(pid);
    if let Some(guard) = guard {
        guard.finish().await;
    }
}

/// Split `stdout` into lines of at most `max_bytes` for the protocol client.
async fn read_lines(stdout: ChildStdout, sender: mpsc::Sender<LiveLine>, max_bytes: usize) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        loop {
            let buffer = match reader.fill_buf().await {
                Ok(buffer) => buffer,
                Err(_) => return,
            };
            if buffer.is_empty() {
                // End of output; a partial last line is not a message.
                return;
            }
            let take = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |index| index + 1);
            if line.len() + take > max_bytes {
                let _ = sender.send(LiveLine::TooLong).await;
                return;
            }
            line.extend_from_slice(&buffer[..take]);
            reader.consume(take);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        line.pop();
        if sender.send(LiveLine::Line(line)).await.is_err() {
            return;
        }
    }
}
