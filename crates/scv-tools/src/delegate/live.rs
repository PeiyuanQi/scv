//! Long-running delegated agents: one child process per conversation, spoken
//! to in newline-delimited JSON over its stdin and stdout.
//!
//! [`LiveChild`] is protocol-neutral. It starts the child in its adapter
//! environment and process group, records it as a delegation (so `scv agents
//! ps`, `kill`, reconcile, and orphan reaping apply), frames its stdout into
//! bounded lines, and shuts it down: closing stdin, a grace period, then a
//! group kill. A protocol client such as the SCV one in `scv_agent` (or an
//! ACP JSON-RPC client) runs on top of it.
//!
//! A conversation keeps its child between turns, when no turn is reading
//! from it. A reaper task therefore owns the process and waits for it from
//! the start: whenever the child exits, whether it ends by itself or `scv
//! agents kill` stops it, the reaper collects it at once, stops what is left
//! of its group, and removes its delegation record.

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
use scv_protocol::{Frame, FrameDecoder, Overflow};
use serde::Serialize;
use tokio::{
    io::{AsyncWriteExt as _, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc, watch},
};

use crate::{
    delegate::{
        output::TailBuffer,
        records::{
            self, DelegationGuard, DelegationRegistry, PendingDelegation, STOP_GRACE, group_exists,
        },
    },
    process::{ProcessGroup, apply_agent_environment, drain_output},
    sync::lock,
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

/// Where a live child's process is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    Running,
    /// The leader exited and was collected; cleanup is under way.
    Exited,
    /// Its group is stopped and its delegation record removed.
    Finished,
}

/// A running live child. Dropping the last reference shuts it down in the
/// background; [`LiveChild::close`] does the same and waits for it.
pub(crate) struct LiveChild {
    pid: u32,
    stdin: Mutex<Option<ChildStdin>>,
    lines: Mutex<mpsc::Receiver<LiveLine>>,
    stderr: Arc<Mutex<TailBuffer>>,
    /// Shared with the reaper, which finishes it when the child exits.
    guard: Arc<StdMutex<Option<DelegationGuard>>>,
    life: watch::Receiver<Life>,
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
        let mut child = command.spawn().map_err(|error| {
            ToolError::unavailable(format!("launch {:?}: {error}", spec.executable))
        })?;
        let pid = child
            .id()
            .ok_or_else(|| ToolError::failed("child process has no pid"))?;
        records::track_spawned(pid);
        let guard = Arc::new(StdMutex::new(
            registration.and_then(|(registry, pending)| registry.register(pending, pid).ok()),
        ));
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
        let (life_tx, life) = watch::channel(Life::Running);
        tokio::spawn(reap(pid, child, Arc::clone(&guard), life_tx));
        Ok(Arc::new(Self {
            pid,
            stdin: Mutex::new(stdin),
            lines: Mutex::new(receiver),
            stderr,
            guard,
            life,
            closed: AtomicBool::new(false),
        }))
    }

    /// Write `message` as one JSON line.
    pub(crate) async fn send(&self, message: &impl Serialize) -> Result<(), ToolError> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|error| ToolError::failed(format!("encode message: {error}")))?;
        bytes.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let writer = stdin
            .as_mut()
            .ok_or_else(|| ToolError::failed("the child's input is closed"))?;
        writer
            .write_all(&bytes)
            .await
            .map_err(|error| ToolError::failed(format!("write to child: {error}")))?;
        writer
            .flush()
            .await
            .map_err(|error| ToolError::failed(format!("write to child: {error}")))
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
        !self.closed.load(Ordering::Acquire) && *self.life.borrow() == Life::Running
    }

    /// Record the conversation turn this child is serving.
    pub(crate) fn set_turn(&self, turn: u32) {
        if let Some(guard) = lock(&self.guard).as_ref() {
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
        shut_down(self.pid, stdin, self.life.clone()).await;
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let stdin = self.stdin.get_mut().take();
        let pid = self.pid;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(shut_down(pid, stdin, self.life.clone()));
        } else {
            // No runtime to wait in, so the reaper is gone too: kill at
            // once. The guard's drop sweeps tagged leftovers.
            kill_group(pid);
            records::untrack_spawned(pid);
            drop(lock(&self.guard).take());
        }
    }
}

/// Close the child's input, give it [`STOP_GRACE`] to exit, then kill its
/// group, and wait for the reaper to finish cleaning up.
async fn shut_down(pid: u32, stdin: Option<ChildStdin>, mut life: watch::Receiver<Life>) {
    // Closing input asks the child to finish; an SCV server exits on EOF.
    drop(stdin);
    if tokio::time::timeout(STOP_GRACE, life.wait_for(|state| *state != Life::Running))
        .await
        .is_err()
    {
        kill_group(pid);
    }
    // The reaper then stops leftovers, giving tagged ones their own grace.
    let _ = tokio::time::timeout(
        STOP_GRACE * 2 + Duration::from_millis(500),
        life.wait_for(|state| *state == Life::Finished),
    )
    .await;
}

/// Own the child for its whole life: collect it the moment it exits, then
/// stop the rest of its group and forget its delegation, so an exit between
/// turns leaves neither a zombie nor a stale `scv agents ps` entry.
async fn reap(
    pid: u32,
    mut child: Child,
    guard: Arc<StdMutex<Option<DelegationGuard>>>,
    life: watch::Sender<Life>,
) {
    let _ = child.wait().await;
    let _ = life.send(Life::Exited);
    // Descendants may outlive the leader, so the group always goes.
    if group_exists(pid) {
        kill_group(pid);
    }
    records::untrack_spawned(pid);
    let guard = lock(&guard).take();
    if let Some(guard) = guard {
        guard.finish().await;
    }
    let _ = life.send(Life::Finished);
}

/// Split `stdout` into lines of at most `max_bytes`, counting the line
/// ending, for the protocol client.
async fn read_lines(stdout: ChildStdout, sender: mpsc::Sender<LiveLine>, max_bytes: usize) {
    let mut reader = BufReader::new(stdout);
    let mut decoder = FrameDecoder::new(max_bytes, Overflow::Stop);
    loop {
        let line = match scv_client::read_frame(&mut reader, &mut decoder).await {
            Ok(Frame::Line(line)) => LiveLine::Line(line),
            Ok(Frame::TooLarge) => {
                let _ = sender.send(LiveLine::TooLong).await;
                return;
            }
            // End of output; a partial last line is not a message.
            Ok(Frame::End | Frame::Truncated(_)) | Err(_) => return,
        };
        if sender.send(line).await.is_err() {
            return;
        }
    }
}

/// Kill every process in the child's group.
fn kill_group(pid: u32) {
    if let Some(group) = ProcessGroup::new(pid) {
        group.signal(libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests;
