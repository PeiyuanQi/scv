//! Running a child process for a tool: spawn it in its own process group,
//! drain its output while it runs, and always finish the whole group when
//! it exits, times out, or is cancelled.

use std::{
    ffi::OsString, os::unix::process::CommandExt as _, path::PathBuf, sync::Arc, time::Duration,
};

use scv_core::{ToolError, ToolFailure, ToolOutput};
use serde_json::json;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::Mutex,
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout, timeout_at},
};

use crate::delegate::{adapters, records};

/// Give a native agent command its adapter environment: remove every
/// inherited credential, endpoint, and state-location variable any adapter
/// declares, then set `environment` (such as the relocated config home).
pub fn apply_agent_environment(
    command: &mut std::process::Command,
    environment: &[(OsString, OsString)],
) {
    apply_agent_environment_from(
        command,
        std::env::vars_os().map(|(variable, _)| variable),
        environment,
    );
}

fn apply_agent_environment_from(
    command: &mut std::process::Command,
    inherited: impl IntoIterator<Item = OsString>,
    environment: &[(OsString, OsString)],
) {
    for variable in inherited {
        if adapters::is_removed_agent_variable(&variable) {
            command.env_remove(variable);
        }
    }
    command.envs(environment.iter().map(|(key, value)| (key, value)));
}

/// What to run for a tool, and its bounds.
pub(crate) struct ProcessSpec {
    pub(crate) executable: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: PathBuf,
    pub(crate) environment: Vec<(OsString, OsString)>,
    /// Strip inherited agent credentials and state locations first
    /// ([`apply_agent_environment`]), as for a delegated agent CLI.
    pub(crate) sanitize_scv_environment: bool,
    pub(crate) timeout: Duration,
    pub(crate) output_limit: usize,
}

pub(crate) async fn execute_process(
    spec: ProcessSpec,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let deadline = Instant::now() + spec.timeout;
    let mut child = spawn_process(&spec)?;
    let pid = child_pid(&child)?;
    let output = Arc::new(Mutex::new(BoundedOutput::new(spec.output_limit)));
    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(drain_output(stdout, Arc::clone(&output))));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(drain_output(stderr, Arc::clone(&output))));
    let finished = supervise(
        &mut child,
        pid,
        deadline,
        cancellation,
        stdout_task,
        stderr_task,
    )
    .await;
    records::untrack_spawned(pid as u32);
    let finished = finished?;
    let collected = output.lock().await;
    let text = String::from_utf8_lossy(&collected.bytes).into_owned();
    let content = json!({
        "exit_code": finished.status.code(),
        "timed_out": finished.timed_out,
        "output": text,
        "truncated": collected.truncated
    })
    .to_string();
    let failure = if finished.timed_out {
        Some(ToolFailure::Limit)
    } else {
        (!finished.status.success()).then_some(ToolFailure::Failed)
    };
    Ok(ToolOutput {
        content,
        failure,
        truncated: collected.truncated,
    })
}

pub(crate) fn spawn_process(spec: &ProcessSpec) -> Result<tokio::process::Child, ToolError> {
    let mut command = Command::new(&spec.executable);
    if spec.sanitize_scv_environment {
        apply_agent_environment(command.as_std_mut(), &spec.environment);
    } else {
        command.envs(spec.environment.iter().map(|(key, value)| (key, value)));
    }
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let child = command.spawn().map_err(|error| {
        ToolError::unavailable(format!("launch {:?}: {error}", spec.executable))
    })?;
    if let Some(pid) = child.id() {
        records::track_spawned(pid);
    }
    Ok(child)
}

pub(crate) fn child_pid(child: &tokio::process::Child) -> Result<i32, ToolError> {
    child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| ToolError::failed("child process has no pid"))
}

pub(crate) struct Finished {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) timed_out: bool,
}

/// Wait for a spawned process group until it exits, times out, or is
/// cancelled, always finishing the whole group and draining its output.
pub(crate) async fn supervise(
    child: &mut tokio::process::Child,
    pid: i32,
    deadline: Instant,
    cancellation: tokio_util::sync::CancellationToken,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
) -> Result<Finished, ToolError> {
    enum Completion {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }
    let completion = tokio::select! {
        status = child.wait() => Completion::Exited(status.map_err(|error| ToolError::failed(format!("wait for child: {error}")))?),
        () = cancellation.cancelled() => {
            Completion::Cancelled
        },
        () = sleep_until(deadline) => Completion::TimedOut,
    };

    let (status, timed_out, drain_deadline) = match completion {
        Completion::Exited(status) => {
            let cleanup_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
            let status = terminate_group(pid, child, Some(status), cleanup_deadline, true).await?;
            (
                status,
                false,
                deadline.min(Instant::now() + Duration::from_millis(250)),
            )
        }
        Completion::TimedOut => {
            let status = terminate_group(pid, child, None, Instant::now(), false).await?;
            (status, true, Instant::now() + Duration::from_millis(250))
        }
        Completion::Cancelled => {
            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            let _ = terminate_group(pid, child, None, cleanup_deadline, true).await;
            finish_drain(stdout_task, Instant::now() + Duration::from_millis(250)).await;
            finish_drain(stderr_task, Instant::now() + Duration::from_millis(250)).await;
            return Err(ToolError::cancelled("process cancelled"));
        }
    };
    finish_drain(stdout_task, drain_deadline).await;
    finish_drain(stderr_task, drain_deadline).await;
    Ok(Finished { status, timed_out })
}

async fn terminate_group(
    pid: i32,
    child: &mut tokio::process::Child,
    mut status: Option<std::process::ExitStatus>,
    deadline: Instant,
    graceful: bool,
) -> Result<std::process::ExitStatus, ToolError> {
    // The child was spawned as the leader of its own group.
    let group = u32::try_from(pid).ok().and_then(ProcessGroup::new);
    if let Some(group) = group {
        group.signal(if graceful {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        });
    }
    while Instant::now() < deadline {
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| ToolError::failed(format!("wait for child: {error}")))?;
        }
        if !group.is_some_and(ProcessGroup::is_signalable)
            && let Some(status) = status
        {
            return Ok(status);
        }
        sleep(Duration::from_millis(20)).await;
    }
    // Always finish the process group, even if its original leader already exited.
    if let Some(group) = group {
        group.signal(libc::SIGKILL);
    }
    if let Some(status) = status {
        return Ok(status);
    }
    timeout(Duration::from_secs(1), child.wait())
        .await
        .map_err(|_| ToolError::failed("child did not exit after process-group kill"))?
        .map_err(|error| ToolError::failed(format!("wait after KILL: {error}")))
}

/// A process group SCV may signal. It is never group 0 or 1 (this process's
/// own group, or init's), which an unset or corrupt ID would otherwise
/// address, so every signal SCV sends to a group goes through this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessGroup(i32);

impl ProcessGroup {
    /// The group with ID `pgid`, or `None` for an ID SCV must never signal.
    pub(crate) fn new(pgid: u32) -> Option<Self> {
        i32::try_from(pgid).ok().filter(|&id| id > 1).map(Self)
    }

    /// Send `signal` to every member of the group.
    pub(crate) fn signal(self, signal: i32) {
        // SAFETY: kill(2) takes plain integers and touches no memory of
        // ours; the negative ID addresses the group, never 0 or 1 (`new`).
        unsafe {
            libc::kill(-self.0, signal);
        }
    }

    /// Whether any member can still be signalled, zombies included.
    pub(crate) fn is_signalable(self) -> bool {
        // SAFETY: as in `signal`; signal 0 only checks existence and access.
        let result = unsafe { libc::kill(-self.0, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

async fn finish_drain(task: Option<JoinHandle<()>>, deadline: Instant) {
    let Some(mut task) = task else { return };
    if timeout_at(deadline, &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

/// Where a child's output goes as it is read.
pub(crate) trait OutputSink: Send + 'static {
    fn push(&mut self, bytes: &[u8]);
}

impl OutputSink for BoundedOutput {
    fn push(&mut self, bytes: &[u8]) {
        BoundedOutput::push(self, bytes);
    }
}

pub(crate) async fn drain_output<R, S>(mut reader: R, output: Arc<Mutex<S>>)
where
    R: tokio::io::AsyncRead + Unpin,
    S: OutputSink,
{
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => output.lock().await.push(&chunk[..read]),
        }
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl BoundedOutput {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8192)),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.truncated |= bytes.len() > remaining;
    }
}

#[cfg(test)]
mod tests;
