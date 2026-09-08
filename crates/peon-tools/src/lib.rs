//! Peon's bounded, workspace-aware built-in tools.

use std::{
    collections::HashMap,
    ffi::OsString,
    io::{Read as _, Write as _},
    os::unix::process::CommandExt as _,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use peon_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::Mutex,
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout, timeout_at},
};

#[derive(Debug, Clone)]
pub struct ToolsConfig {
    pub command_timeout: Duration,
    pub output_limit_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(120),
            output_limit_bytes: 64 * 1024,
            max_read_bytes: 256 * 1024,
            max_write_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentAdapterConfig {
    pub command: String,
    pub args: Vec<String>,
}

pub type SkillMap = HashMap<String, PathBuf>;

pub fn builtin_registry(
    config: ToolsConfig,
    skills: SkillMap,
    skill_roots: Vec<PathBuf>,
    max_skill_bytes: usize,
    adapters: HashMap<String, AgentAdapterConfig>,
) -> Result<ToolRegistry, ToolError> {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(ReadTool {
        max_bytes: config.max_read_bytes,
    }))?;
    registry.register(Arc::new(ReadSkillTool {
        skills,
        roots: skill_roots,
        max_bytes: max_skill_bytes,
    }))?;
    registry.register(Arc::new(WriteTool {
        max_bytes: config.max_write_bytes,
    }))?;
    registry.register(Arc::new(BashTool {
        timeout: config.command_timeout,
        output_limit: config.output_limit_bytes,
    }))?;
    for (name, adapter) in adapters {
        registry.register(Arc::new(NativeAgentTool::new(
            name,
            adapter,
            config.command_timeout,
            config.output_limit_bytes,
        )))?;
    }
    Ok(registry)
}

struct ReadTool {
    max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a bounded UTF-8 file inside the workspace".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "offset":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1}
                },
                "required":["path"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: ReadArgs = parse_args(arguments)?;
        validate_read_args(&args)?;
        Ok(if is_secret_like(Path::new(&args.path)) {
            ToolRisk::Filesystem
        } else {
            ToolRisk::ReadOnly
        })
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: ReadArgs = parse_args(arguments)?;
        validate_read_args(&args)?;
        Ok(format!("Read {}", args.path))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse_args(&arguments)?;
        validate_read_args(&args)?;
        let requested = args.limit.unwrap_or(self.max_bytes).min(self.max_bytes);
        let offset = u64::try_from(args.offset).unwrap_or(u64::MAX);
        let workspace = context.workspace.clone();
        let display_path = args.path.clone();
        let relative = PathBuf::from(&args.path);
        validate_relative(&relative)?;
        let read = tokio::task::spawn_blocking(move || {
            let root = open_workspace(&workspace)?;
            let mut file = root
                .open(&relative)
                .map_err(|error| map_cap_error("read", &display_path, error))?;
            let total_bytes = file
                .metadata()
                .map_err(|error| ToolError(format!("stat {display_path}: {error}")))?
                .len();
            let start = offset.min(total_bytes);
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start))
                .map_err(|error| ToolError(format!("seek {display_path}: {error}")))?;
            let mut bytes = Vec::with_capacity(requested.min(8192));
            std::io::Read::take(&mut file, u64::try_from(requested).unwrap_or(u64::MAX))
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError(format!("read {display_path}: {error}")))?;
            Ok::<_, ToolError>((bytes, total_bytes, start))
        });
        let (bytes, total_bytes, start) = tokio::select! {
            result = read => result.map_err(|error| ToolError(format!("read task failed: {error}")))??,
            _ = context.cancellation.cancelled() => return Err(ToolError("read cancelled".into())),
        };
        let content = std::str::from_utf8(&bytes)
            .map_err(|_| ToolError(format!("selected range of {} is not UTF-8", args.path)))?;
        let end = start.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let truncated = start > 0 || end < total_bytes;
        Ok(ToolOutput {
            content: json!({
                "path": args.path,
                "content": content,
                "total_bytes": total_bytes,
                "offset": start,
                "truncated": truncated
            })
            .to_string(),
            is_error: false,
            truncated,
        })
    }
}

struct ReadSkillTool {
    skills: SkillMap,
    roots: Vec<PathBuf>,
    max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSkillArgs {
    name: String,
}

#[async_trait]
impl Tool for ReadSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_skill".into(),
            description: "Load a discovered Peon skill by name".into(),
            parameters: json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: ReadSkillArgs = parse_args(arguments)?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: ReadSkillArgs = parse_args(arguments)?;
        Ok(format!("Load skill {}", args.name))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadSkillArgs = parse_args(&arguments)?;
        let configured = self
            .skills
            .get(&args.name)
            .ok_or_else(|| ToolError(format!("unknown skill: {}", args.name)))?;
        let path = std::fs::canonicalize(configured)
            .map_err(|error| ToolError(format!("load skill {}: {error}", args.name)))?;
        if !self.roots.iter().any(|root| path.starts_with(root)) {
            return Err(ToolError("skill path escaped its configured root".into()));
        }
        let max_bytes = self.max_bytes;
        let skill_name = args.name.clone();
        let bytes = tokio::select! {
            result = tokio::task::spawn_blocking(move || {
                let mut file = std::fs::File::open(&path)
                    .map_err(|error| ToolError(format!("load skill {skill_name}: {error}")))?;
                let mut bytes = Vec::with_capacity(max_bytes.min(8192));
                std::io::Read::take(
                    &mut file,
                    u64::try_from(max_bytes).unwrap_or(u64::MAX).saturating_add(1),
                )
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError(format!("load skill {skill_name}: {error}")))?;
                Ok::<_, ToolError>(bytes)
            }) => result.map_err(|error| ToolError(format!("skill read task failed: {error}")))??,
            _ = context.cancellation.cancelled() => return Err(ToolError("skill read cancelled".into())),
        };
        let end = bytes.len().min(self.max_bytes);
        let content = std::str::from_utf8(&bytes[..end])
            .map_err(|_| ToolError("skill is not UTF-8".into()))?;
        Ok(ToolOutput {
            content: content.to_owned(),
            is_error: false,
            truncated: end < bytes.len(),
        })
    }
}

struct WriteTool {
    max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    mode: WriteMode,
    expected_sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    Create,
    Replace,
}

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Atomically create or replace a UTF-8 file inside the workspace".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "content":{"type":"string"},
                    "mode":{"type":"string","enum":["create","replace"]},
                    "expected_sha256":{"type":"string"}
                },
                "required":["path","content","mode"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: WriteArgs = parse_args(arguments)?;
        Ok(ToolRisk::Filesystem)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: WriteArgs = parse_args(arguments)?;
        let mode = match args.mode {
            WriteMode::Create => "Create",
            WriteMode::Replace => "Replace",
        };
        Ok(format!(
            "{mode} {} ({} bytes)",
            args.path,
            args.content.len()
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs = parse_args(&arguments)?;
        if args.content.len() > self.max_bytes {
            return Err(ToolError(format!(
                "write exceeds {} byte limit",
                self.max_bytes
            )));
        }
        let workspace = context.workspace.clone();
        let cancellation = context.cancellation.clone();
        tokio::task::spawn_blocking(move || {
            if cancellation.is_cancelled() {
                return Err(ToolError("write cancelled".into()));
            }
            let path = PathBuf::from(&args.path);
            validate_relative(&path)?;
            let root = open_workspace(&workspace)?;
            let exists = match root.symlink_metadata(&path) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(map_cap_error("inspect", &args.path, error)),
            };
            match args.mode {
                WriteMode::Create if exists => {
                    return Err(ToolError(format!("{} already exists", args.path)));
                }
                WriteMode::Replace if !exists => {
                    return Err(ToolError(format!("{} does not exist", args.path)));
                }
                _ => {}
            }
            if let Some(expected) = args.expected_sha256 {
                let mut current_file = root
                    .open(&path)
                    .map_err(|error| map_cap_error("hash", &args.path, error))?;
                let mut current = Vec::new();
                current_file
                    .read_to_end(&mut current)
                    .map_err(|error| ToolError(format!("hash {}: {error}", args.path)))?;
                let actual = format!("{:x}", Sha256::digest(current));
                if actual != expected.to_ascii_lowercase() {
                    return Err(ToolError(format!(
                        "{} changed: expected sha256 {}, found {}",
                        args.path, expected, actual
                    )));
                }
            }
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            root.create_dir_all(parent)
                .map_err(|error| map_cap_error("create directory for", &args.path, error))?;
            let temporary_path = unique_temporary_path(parent);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut temporary = root
                .open_with(&temporary_path, &options)
                .map_err(|error| map_cap_error("create temporary file for", &args.path, error))?;
            let write_result = (|| {
                temporary
                    .write_all(args.content.as_bytes())
                    .and_then(|_| temporary.sync_all())
                    .map_err(|error| ToolError(format!("write {}: {error}", args.path)))?;
                if cancellation.is_cancelled() {
                    return Err(ToolError("write cancelled".into()));
                }
                match args.mode {
                    WriteMode::Create => root
                        .hard_link(&temporary_path, &root, &path)
                        .map_err(|error| map_cap_error("create", &args.path, error)),
                    WriteMode::Replace => root
                        .rename(&temporary_path, &root, &path)
                        .map_err(|error| map_cap_error("replace", &args.path, error)),
                }
            })();
            if matches!(args.mode, WriteMode::Create) || write_result.is_err() {
                let _ = root.remove_file(&temporary_path);
            }
            write_result?;
            Ok(ToolOutput::success(
                json!({
                    "path":args.path,
                    "bytes":args.content.len(),
                    "sha256":format!("{:x}", Sha256::digest(args.content.as_bytes()))
                })
                .to_string(),
            ))
        })
        .await
        .map_err(|error| ToolError(format!("write task failed: {error}")))?
    }
}

struct BashTool {
    timeout: Duration,
    output_limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    timeout_seconds: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a Bash command in the workspace (not sandboxed)".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string"},
                    "timeout_seconds":{"type":"integer","minimum":1}
                },
                "required":["command"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: BashArgs = parse_args(arguments)?;
        validate_process_args(&args.command, args.timeout_seconds)?;
        Ok(ToolRisk::Process)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: BashArgs = parse_args(arguments)?;
        validate_process_args(&args.command, args.timeout_seconds)?;
        Ok(format!(
            "Run with /bin/bash -lc: {}",
            bounded(&args.command, 2000)
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = parse_args(&arguments)?;
        validate_process_args(&args.command, args.timeout_seconds)?;
        let requested = args
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(self.timeout)
            .min(self.timeout);
        execute_process(
            ProcessSpec {
                executable: OsString::from("/bin/bash"),
                args: vec![OsString::from("-lc"), OsString::from(args.command)],
                cwd: context.workspace,
                timeout: requested,
                output_limit: self.output_limit,
            },
            context.cancellation,
        )
        .await
    }
}

struct NativeAgentTool {
    name: String,
    command: String,
    resolved: Option<PathBuf>,
    args: Vec<String>,
    timeout: Duration,
    output_limit: usize,
}

impl NativeAgentTool {
    fn new(
        name: String,
        config: AgentAdapterConfig,
        timeout: Duration,
        output_limit: usize,
    ) -> Self {
        let resolved = which::which(&config.command).ok();
        Self {
            name,
            command: config.command,
            resolved,
            args: config.args,
            timeout,
            output_limit,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    prompt: String,
    timeout_seconds: Option<u64>,
}

#[async_trait]
impl Tool for NativeAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "Launch the configured {} CLI as a nested agent (not sandboxed)",
                self.name
            ),
            parameters: json!({
                "type":"object",
                "properties":{
                    "prompt":{"type":"string"},
                    "timeout_seconds":{"type":"integer","minimum":1}
                },
                "required":["prompt"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        validate_process_args(&args.prompt, args.timeout_seconds)?;
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        validate_process_args(&args.prompt, args.timeout_seconds)?;
        let executable = self.resolved.as_ref().map_or_else(
            || self.command.as_str().into(),
            |path| path.display().to_string(),
        );
        Ok(format!(
            "Launch {executable} with fixed args {:?} and prompt {:?}. The nested agent has your user permissions.",
            self.args,
            bounded(&args.prompt, 2000)
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: AgentArgs = parse_args(&arguments)?;
        validate_process_args(&args.prompt, args.timeout_seconds)?;
        let executable = self.resolved.as_ref().ok_or_else(|| {
            ToolError(format!(
                "{} executable {:?} was not found in PATH",
                self.name, self.command
            ))
        })?;
        let mut command_args: Vec<OsString> = self.args.iter().map(OsString::from).collect();
        command_args.push(OsString::from(args.prompt));
        let requested = args
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(self.timeout)
            .min(self.timeout);
        execute_process(
            ProcessSpec {
                executable: executable.as_os_str().to_owned(),
                args: command_args,
                cwd: context.workspace,
                timeout: requested,
                output_limit: self.output_limit,
            },
            context.cancellation,
        )
        .await
    }
}

struct ProcessSpec {
    executable: OsString,
    args: Vec<OsString>,
    cwd: PathBuf,
    timeout: Duration,
    output_limit: usize,
}

async fn execute_process(
    spec: ProcessSpec,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let deadline = Instant::now() + spec.timeout;
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| ToolError(format!("launch {:?}: {error}", spec.executable)))?;
    let pid = child
        .id()
        .ok_or_else(|| ToolError("child process has no pid".into()))? as i32;
    let output = Arc::new(Mutex::new(BoundedOutput::new(spec.output_limit)));
    let stdout_task = child.stdout.take().map(|stdout| {
        let output = Arc::clone(&output);
        tokio::spawn(drain_output(stdout, output))
    });
    let stderr_task = child.stderr.take().map(|stderr| {
        let output = Arc::clone(&output);
        tokio::spawn(drain_output(stderr, output))
    });

    enum Completion {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }
    let completion = tokio::select! {
        status = child.wait() => Completion::Exited(status.map_err(|error| ToolError(format!("wait for child: {error}")))?),
        _ = cancellation.cancelled() => {
            Completion::Cancelled
        },
        _ = sleep_until(deadline) => Completion::TimedOut,
    };

    let (status, timed_out, drain_deadline) = match completion {
        Completion::Exited(status) => {
            let cleanup_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
            let status =
                terminate_group(pid, &mut child, Some(status), cleanup_deadline, true).await?;
            (
                status,
                false,
                deadline.min(Instant::now() + Duration::from_millis(250)),
            )
        }
        Completion::TimedOut => {
            let status = terminate_group(pid, &mut child, None, Instant::now(), false).await?;
            (status, true, Instant::now() + Duration::from_millis(250))
        }
        Completion::Cancelled => {
            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            let _ = terminate_group(pid, &mut child, None, cleanup_deadline, true).await;
            finish_drain(stdout_task, Instant::now() + Duration::from_millis(250)).await;
            finish_drain(stderr_task, Instant::now() + Duration::from_millis(250)).await;
            return Err(ToolError("process cancelled".into()));
        }
    };
    finish_drain(stdout_task, drain_deadline).await;
    finish_drain(stderr_task, drain_deadline).await;
    let collected = output.lock().await;
    let text = String::from_utf8_lossy(&collected.bytes).into_owned();
    let content = json!({
        "exit_code": status.code(),
        "timed_out": timed_out,
        "output": text,
        "truncated": collected.truncated
    })
    .to_string();
    Ok(ToolOutput {
        content,
        is_error: timed_out || !status.success(),
        truncated: collected.truncated,
    })
}

async fn terminate_group(
    pid: i32,
    child: &mut tokio::process::Child,
    mut status: Option<std::process::ExitStatus>,
    deadline: Instant,
    graceful: bool,
) -> Result<std::process::ExitStatus, ToolError> {
    signal_group(
        pid,
        if graceful {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        },
    );
    while Instant::now() < deadline {
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| ToolError(format!("wait for child: {error}")))?;
        }
        if !process_group_exists(pid)
            && let Some(status) = status
        {
            return Ok(status);
        }
        sleep(Duration::from_millis(20)).await;
    }
    // Always finish the process group, even if its original leader already exited.
    signal_group(pid, libc::SIGKILL);
    if let Some(status) = status {
        return Ok(status);
    }
    timeout(Duration::from_secs(1), child.wait())
        .await
        .map_err(|_| ToolError("child did not exit after process-group kill".into()))?
        .map_err(|error| ToolError(format!("wait after KILL: {error}")))
}

fn signal_group(pid: i32, signal: i32) {
    // Negative PID addresses the process group created at spawn.
    unsafe {
        libc::kill(-pid, signal);
    }
}

fn process_group_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(-pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

async fn finish_drain(task: Option<JoinHandle<()>>, deadline: Instant) {
    let Some(mut task) = task else { return };
    if timeout_at(deadline, &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

async fn drain_output<R>(mut reader: R, output: Arc<Mutex<BoundedOutput>>)
where
    R: tokio::io::AsyncRead + Unpin,
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

fn parse_args<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, ToolError> {
    serde_json::from_value(value.clone())
        .map_err(|error| ToolError(format!("invalid arguments: {error}")))
}

fn validate_read_args(args: &ReadArgs) -> Result<(), ToolError> {
    if args.limit == Some(0) {
        return Err(ToolError("read limit must be positive".into()));
    }
    Ok(())
}

fn validate_process_args(value: &str, timeout_seconds: Option<u64>) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError("command or prompt must be non-empty".into()));
    }
    if timeout_seconds == Some(0) {
        return Err(ToolError("timeout_seconds must be positive".into()));
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(ToolError("path must be non-empty and relative".into()));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(ToolError(
                "parent traversal and absolute paths are not allowed".into(),
            ));
        }
    }
    Ok(())
}

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

fn open_workspace(workspace: &Path) -> Result<Dir, ToolError> {
    Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| ToolError(format!("open workspace capability: {error}")))
}

fn unique_temporary_path(parent: &Path) -> PathBuf {
    let id = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".peon-write-{}-{id}.tmp", std::process::id()))
}

fn map_cap_error(action: &str, path: &str, error: std::io::Error) -> ToolError {
    ToolError(format!(
        "{action} {path}: {error}; path must remain within workspace"
    ))
}

fn is_secret_like(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        value == ".env"
            || value.starts_with(".env.")
            || value.contains("credential")
            || value.contains("private_key")
            || value.ends_with(".pem")
            || value.ends_with(".key")
    })
}

fn bounded(value: &str, max_chars: usize) -> String {
    let mut output: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        assert!(validate_relative(Path::new("../secret")).is_err());
        assert!(validate_relative(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn detects_secret_like_paths() {
        assert!(is_secret_like(Path::new(".env")));
        assert!(is_secret_like(Path::new("keys/id.pem")));
        assert!(!is_secret_like(Path::new("src/main.rs")));
    }

    #[tokio::test]
    async fn read_is_contained_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello.txt"), "abcdef").unwrap();
        let tool = ReadTool { max_bytes: 3 };
        let output = tool
            .execute(
                json!({"path":"hello.txt"}),
                ToolContext {
                    workspace: directory.path().canonicalize().unwrap(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert!(output.truncated);
        assert!(output.content.contains("abc"));
    }

    #[tokio::test]
    async fn read_rejects_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "nope").unwrap();
        symlink(outside.path(), workspace.path().join("escape")).unwrap();
        let tool = ReadTool { max_bytes: 100 };
        let result = tool
            .execute(
                json!({"path":"escape/secret"}),
                ToolContext {
                    workspace: workspace.path().canonicalize().unwrap(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("workspace"));
    }

    #[tokio::test]
    async fn write_is_atomic_and_checks_hash() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let tool = WriteTool { max_bytes: 100 };
        tool.execute(
            json!({"path":"file.txt","content":"first","mode":"create"}),
            ToolContext {
                workspace: root.clone(),
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();
        let hash = format!("{:x}", Sha256::digest(b"first"));
        tool.execute(
            json!({"path":"file.txt","content":"second","mode":"replace","expected_sha256":hash}),
            ToolContext {
                workspace: root.clone(),
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "second"
        );
        let result = tool
            .execute(
                json!({"path":"file.txt","content":"third","mode":"replace","expected_sha256":"deadbeef"}),
                ToolContext {
                    workspace: root,
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("changed"));
    }

    #[tokio::test]
    async fn write_rejects_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("escape")).unwrap();
        let tool = WriteTool { max_bytes: 100 };
        let result = tool
            .execute(
                json!({"path":"escape/file.txt","content":"nope","mode":"create"}),
                ToolContext {
                    workspace: workspace.path().canonicalize().unwrap(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("workspace"));
        assert!(!outside.path().join("file.txt").exists());
    }

    #[tokio::test]
    async fn bash_timeout_terminates_the_process() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = BashTool {
            timeout: Duration::from_millis(50),
            output_limit: 100,
        };
        let started = std::time::Instant::now();
        let output = tool
            .execute(
                json!({"command":"sleep 5"}),
                ToolContext {
                    workspace: workspace.path().canonicalize().unwrap(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn bash_output_is_bounded_and_reports_truncation() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(2),
            output_limit: 8,
        };
        let output = tool
            .execute(
                json!({"command":"printf 12345678901234567890"}),
                ToolContext {
                    workspace: workspace.path().canonicalize().unwrap(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert!(output.truncated);
        assert!(output.content.contains("12345678"));
        assert!(!output.content.contains("123456789"));
    }

    #[tokio::test]
    async fn bash_cancellation_terminates_the_process_group() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(30),
            output_limit: 100,
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancel = cancellation.clone();
        let started = std::time::Instant::now();
        let execution = tokio::spawn(async move {
            tool.execute(
                json!({"command":"sleep 30"}),
                ToolContext {
                    workspace: workspace.path().canonicalize().unwrap(),
                    cancellation,
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let error = execution.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn background_descendant_cannot_hold_output_pipes_open() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(5),
            output_limit: 100,
        };
        let started = std::time::Instant::now();
        let output = tool
            .execute(
                json!({"command":"sleep 30 & echo $! > background.pid; exit 0"}),
                ToolContext {
                    workspace: root.clone(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid: i32 = std::fs::read_to_string(root.join("background.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..20 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background descendant {pid} survived tool completion");
    }

    #[tokio::test]
    async fn cancellation_kills_a_term_ignoring_descendant() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(30),
            output_limit: 100,
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancel = cancellation.clone();
        let command_root = root.clone();
        let execution = tokio::spawn(async move {
            tool.execute(
                json!({"command":"trap '' TERM; (trap '' TERM; sleep 30) & echo $! > stubborn.pid; wait"}),
                ToolContext {
                    workspace: command_root,
                    cancellation,
                },
            )
            .await
        });
        let pid_path = root.join("stubborn.pid");
        let mut descendant_pid = None;
        for _ in 0..100 {
            descendant_pid = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok());
            if descendant_pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = descendant_pid.expect("command did not report its descendant pid");
        let started = std::time::Instant::now();
        cancel.cancel();
        let error = execution.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
        for _ in 0..20 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("TERM-ignoring descendant {pid} survived cancellation");
    }

    #[tokio::test]
    async fn native_agent_preserves_argument_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        let executable = workspace.path().join("fake-agent");
        std::fs::write(&executable, "#!/bin/bash\npwd\nprintf '%s\\n' \"$@\"\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let tool = NativeAgentTool::new(
            "agent_fake".into(),
            AgentAdapterConfig {
                command: executable.display().to_string(),
                args: vec!["--fixed".into()],
            },
            Duration::from_secs(2),
            1024,
        );
        let output = tool
            .execute(
                json!({"prompt":"hello; echo unsafe"}),
                ToolContext {
                    workspace: workspace.path().canonicalize().unwrap(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert!(output.content.contains("--fixed"));
        assert!(output.content.contains("hello; echo unsafe"));
        assert!(
            output
                .content
                .contains(&workspace.path().display().to_string())
        );
    }
}
