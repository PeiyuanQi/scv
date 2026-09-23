//! SCV's bounded, workspace-aware built-in tools.

pub mod adapters;
mod agent_output;
mod agent_progress;
pub mod conversation;
pub mod delegation;
mod live;
mod scv_agent;
pub mod web;

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
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolRisk, ToolSpec};

use crate::{
    adapters::{OutputFormat, Resume, Transport},
    agent_output::{AgentStream, RunExit, STDERR_TAIL_BYTES, TailBuffer},
    conversation::{ConversationLimits, ConversationStore},
    delegation::{DelegationGuard, DelegationRegistry},
};
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
    /// Default `bash` timeout when a call does not choose one.
    pub command_timeout: Duration,
    /// Default native-agent timeout when a call does not choose one.
    pub agent_timeout: Duration,
    /// The longest timeout any single call may request.
    pub max_timeout: Duration,
    pub output_limit_bytes: usize,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    /// Agent tools are offered only below this delegation depth.
    pub max_delegation_depth: u32,
    /// How many delegated conversations a session remembers, and for how long.
    pub conversations: ConversationLimits,
    /// Records delegated runs for listing and cleanup; `None` runs them untracked.
    pub delegation: Option<DelegationContext>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(600),
            agent_timeout: Duration::from_secs(3600),
            max_timeout: Duration::from_secs(14400),
            output_limit_bytes: 64 * 1024,
            max_read_bytes: 256 * 1024,
            max_write_bytes: 1024 * 1024,
            max_delegation_depth: 2,
            conversations: ConversationLimits {
                max: 8,
                idle: Duration::from_secs(86400),
            },
            delegation: None,
        }
    }
}

/// The registry and parent session that delegated runs are recorded under.
#[derive(Debug, Clone)]
pub struct DelegationContext {
    pub registry: Arc<DelegationRegistry>,
    pub session: String,
    /// Delegation depth the session's client declared (0 for a direct
    /// client). Runs count from the larger of this and the process's own.
    pub depth: u32,
}

impl DelegationContext {
    /// The depth delegated runs of this session start from.
    pub fn owner_depth(&self) -> u32 {
        self.registry.depth().max(self.depth)
    }
}

#[derive(Debug, Clone)]
pub struct AgentAdapterConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Arguments placed immediately before the prompt, for CLIs that take the
    /// prompt as a flag value.
    pub prompt_args: Vec<String>,
    /// The CLI's own full-autonomy arguments, placed after `args`, when the
    /// user configured `permissions = "full"`; the approval summary says so.
    pub full_permission_args: Option<Vec<String>>,
    /// Arguments appended for a per-call model; `{model}` is substituted.
    /// Empty means the adapter does not offer model selection.
    pub model_args: Vec<String>,
    /// Arguments appended for a per-call effort; `{effort}` is substituted.
    /// Empty means the adapter does not offer effort selection.
    pub effort_args: Vec<String>,
    /// Describes the `model` argument for the calling model.
    pub model_hint: String,
    /// Environment for the nested process. SCV supplies an instance-private home.
    pub environment: Vec<(OsString, OsString)>,
    /// Per-user install directories searched when `command` is not on `PATH`.
    pub search_dirs: Vec<PathBuf>,
    /// What the CLI prints, and so how its reply is read.
    pub output: OutputFormat,
    /// How a conversation with the CLI is continued, if it can be.
    pub resume: Resume,
    /// SCV's private home for this agent, for files SCV hands the CLI.
    pub home: Option<PathBuf>,
    /// How SCV talks to the agent.
    pub transport: Transport,
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
        max_timeout: config.max_timeout,
        output_limit: config.output_limit_bytes,
    }))?;
    // A delegated SCV at the depth limit may not delegate further.
    let depth = config
        .delegation
        .as_ref()
        .map_or_else(delegation::current_depth, DelegationContext::owner_depth);
    let adapters = if depth < config.max_delegation_depth {
        adapters
    } else {
        HashMap::new()
    };
    // One store per session, shared by its agent tools and dropped with it.
    let conversations = Arc::new(ConversationStore::new(
        config.conversations,
        config
            .delegation
            .as_ref()
            .map(|context| context.registry.conversation_dir()),
    ));
    for (name, adapter) in adapters {
        if adapter.transport == Transport::ScvProtocol {
            let resolved =
                adapters::resolve_agent_executable(&adapter.command, &adapter.search_dirs);
            // An agent that is not installed is not offered to the model.
            if resolved.is_some() {
                registry.register(Arc::new(scv_agent::ScvAgentTool {
                    name,
                    command: adapter.command,
                    resolved,
                    args: adapter.args,
                    environment: adapter.environment,
                    timeouts: Timeouts {
                        default: config.agent_timeout,
                        max: config.max_timeout,
                    },
                    output_limit: config.output_limit_bytes,
                    delegation: config.delegation.clone(),
                    conversations: Arc::clone(&conversations),
                }))?;
            }
            continue;
        }
        let tool = NativeAgentTool::new(
            name,
            adapter,
            Timeouts {
                default: config.agent_timeout,
                max: config.max_timeout,
            },
            config.output_limit_bytes,
            config.delegation.clone(),
            Arc::clone(&conversations),
        );
        // An agent that is not installed is not offered to the model.
        if tool.resolved.is_some() {
            registry.register(Arc::new(tool))?;
        }
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
            description: "Load a discovered SCV skill by name".into(),
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
    max_timeout: Duration,
    output_limit: usize,
}

impl BashTool {
    fn timeouts(&self) -> Timeouts {
        Timeouts {
            default: self.timeout,
            max: self.max_timeout,
        }
    }
}

/// A process tool's default timeout and the ceiling a call may raise it to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    pub(crate) default: Duration,
    pub(crate) max: Duration,
}

impl Timeouts {
    /// The call's timeout: its own request up to the ceiling, else the
    /// default. A request above the ceiling is refused, never clamped, so the
    /// caller learns the limit instead of being cut off early.
    pub(crate) fn resolve(self, requested: Option<u64>) -> Result<Duration, ToolError> {
        match requested {
            None => Ok(self.default.min(self.max)),
            Some(0) => Err(ToolError("timeout_seconds must be positive".into())),
            Some(seconds) if seconds > self.max.as_secs() => Err(ToolError(format!(
                "timeout_seconds {seconds} exceeds the configured maximum of {} seconds \
                 (tools.max_timeout_seconds)",
                self.max.as_secs()
            ))),
            Some(seconds) => Ok(Duration::from_secs(seconds)),
        }
    }
}

pub(crate) fn timeout_schema(timeouts: Timeouts) -> Value {
    json!({
        "type":"integer",
        "minimum":1,
        "maximum":timeouts.max.as_secs(),
        "description":format!(
            "Seconds before the process is killed. Defaults to {}; at most {}. \
             Raise it for long work such as builds, releases, or landing a change.",
            timeouts.default.min(timeouts.max).as_secs(),
            timeouts.max.as_secs()
        )
    })
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
                    "timeout_seconds":timeout_schema(self.timeouts())
                },
                "required":["command"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: BashArgs = parse_args(arguments)?;
        validate_process_args(&args.command)?;
        self.timeouts().resolve(args.timeout_seconds)?;
        Ok(ToolRisk::Process)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: BashArgs = parse_args(arguments)?;
        validate_process_args(&args.command)?;
        self.timeouts().resolve(args.timeout_seconds)?;
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
        validate_process_args(&args.command)?;
        let requested = self.timeouts().resolve(args.timeout_seconds)?;
        execute_process(
            ProcessSpec {
                executable: OsString::from("/bin/bash"),
                args: vec![OsString::from("-lc"), OsString::from(args.command)],
                cwd: context.workspace,
                environment: Vec::new(),
                sanitize_scv_environment: false,
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
    prompt_args: Vec<String>,
    full_permission_args: Option<Vec<String>>,
    model_args: Vec<String>,
    effort_args: Vec<String>,
    model_hint: String,
    environment: Vec<(OsString, OsString)>,
    timeouts: Timeouts,
    output_limit: usize,
    output: OutputFormat,
    resume: Resume,
    home: Option<PathBuf>,
    delegation: Option<DelegationContext>,
    conversations: Arc<ConversationStore>,
}

/// Effort levels accepted by the built-in adapters' CLIs.
const AGENT_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

impl NativeAgentTool {
    /// The fixed arguments, validated model and effort selections, and the
    /// prompt arguments; the prompt is appended separately as the final argument.
    fn command_args(&self, args: &AgentArgs) -> Result<Vec<String>, ToolError> {
        validate_process_args(&args.prompt)?;
        self.timeouts.resolve(args.timeout_seconds)?;
        if let Some(cwd) = &args.cwd {
            validate_agent_cwd(cwd)?;
        }
        if let Some(session) = &args.session {
            if !self.resume.is_supported() {
                return Err(ToolError(format!(
                    "{} cannot continue a conversation; omit session to start a new one",
                    self.name
                )));
            }
            if !conversation::is_handle(session) {
                return Err(ToolError(format!(
                    "session {:?} is not a conversation handle; pass the `session` value an \
                     earlier {} call returned, or omit it to start a new conversation",
                    bounded(session, 80),
                    self.name
                )));
            }
        }
        // The prompt follows the flags as a positional argument, so it must
        // not be readable as one.
        if args.prompt.starts_with('-') {
            return Err(ToolError("agent prompt must not start with '-'".into()));
        }
        let mut command = self.args.clone();
        command.extend(self.full_permission_args.iter().flatten().cloned());
        command.extend(self.output.args().iter().map(|arg| (*arg).to_owned()));
        for (field, value, template, placeholder) in [
            ("model", &args.model, &self.model_args, "{model}"),
            ("effort", &args.effort, &self.effort_args, "{effort}"),
        ] {
            let Some(value) = value else {
                continue;
            };
            if template.is_empty() {
                return Err(ToolError(format!(
                    "{} does not support selecting a {field}",
                    self.name
                )));
            }
            let valid = if field == "model" {
                valid_model_name(value)
            } else {
                AGENT_EFFORTS.contains(&value.as_str())
            };
            if !valid {
                return Err(ToolError(format!("invalid {field} {value:?}")));
            }
            command.extend(template.iter().map(|part| part.replace(placeholder, value)));
        }
        command.extend(self.prompt_args.iter().cloned());
        Ok(command)
    }
    fn new(
        name: String,
        config: AgentAdapterConfig,
        timeouts: Timeouts,
        output_limit: usize,
        delegation: Option<DelegationContext>,
        conversations: Arc<ConversationStore>,
    ) -> Self {
        let resolved = adapters::resolve_agent_executable(&config.command, &config.search_dirs);
        Self {
            name,
            command: config.command,
            resolved,
            args: config.args,
            prompt_args: config.prompt_args,
            full_permission_args: config.full_permission_args,
            model_args: config.model_args,
            effort_args: config.effort_args,
            model_hint: config.model_hint,
            environment: config.environment,
            timeouts,
            output_limit,
            output: config.output,
            resume: config.resume,
            home: config.home,
            delegation,
            conversations,
        }
    }

    /// Arguments that name per-run files or IDs, placed after the fixed and
    /// format arguments. Returns the Codex last-message file to read and
    /// remove afterwards.
    fn run_args(&self, id: &str) -> (Vec<OsString>, Option<PathBuf>) {
        match self.output {
            OutputFormat::CodexJsonl => {
                let Some(dir) = self.home.as_ref().map(|home| home.join("tmp")) else {
                    return (Vec::new(), None);
                };
                if private_dir(&dir).is_err() {
                    return (Vec::new(), None);
                }
                let file = dir.join(format!("scv-{id}.last-message"));
                (vec!["-o".into(), file.clone().into()], Some(file))
            }
            OutputFormat::Text | OutputFormat::ClaudeStreamJson | OutputFormat::PiJson => {
                (Vec::new(), None)
            }
        }
    }
}

/// `template` with `{session}` replaced by `session`.
fn session_args(template: &[&str], session: &str) -> Vec<OsString> {
    template
        .iter()
        .map(|part| OsString::from(part.replace("{session}", session)))
        .collect()
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Read and delete a file the CLI wrote for SCV, bounded to `limit` bytes.
fn take_file(path: &Path, limit: usize) -> Option<String> {
    let file = std::fs::File::open(path).ok();
    let _ = std::fs::remove_file(path);
    let mut bytes = Vec::new();
    std::io::Read::take(file?, u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentArgs {
    pub(crate) prompt: String,
    pub(crate) timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) session: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) cwd: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) model: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) effort: Option<String>,
}

/// Models often send an optional string they mean to leave unset as `""`, so
/// a blank value selects the default rather than failing the call.
fn blank_as_none<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|value| !value.trim().is_empty()))
}

/// Longest `cwd` argument accepted, in bytes.
const MAX_AGENT_CWD_BYTES: usize = 4096;

pub(crate) fn validate_agent_cwd(cwd: &str) -> Result<(), ToolError> {
    if cwd.trim().is_empty() || cwd.len() > MAX_AGENT_CWD_BYTES || cwd.contains('\0') {
        return Err(ToolError(format!(
            "cwd must be a non-empty directory path of at most {MAX_AGENT_CWD_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Resolve a requested agent directory against the workspace. Resolution
/// follows symlinks, so a link pointing outside the workspace is refused
/// rather than trusted by name.
pub(crate) fn resolve_agent_cwd(workspace: &Path, cwd: Option<&str>) -> Result<PathBuf, ToolError> {
    let root = std::fs::canonicalize(workspace)
        .map_err(|error| ToolError(format!("resolve workspace: {error}")))?;
    let Some(cwd) = cwd else {
        return Ok(root);
    };
    validate_agent_cwd(cwd)?;
    let resolved = std::fs::canonicalize(root.join(cwd))
        .map_err(|error| ToolError(format!("cwd {cwd:?}: {error}")))?;
    if !resolved.starts_with(&root) {
        return Err(ToolError(format!("cwd {cwd:?} is outside the workspace")));
    }
    if !resolved.is_dir() {
        return Err(ToolError(format!("cwd {cwd:?} is not a directory")));
    }
    Ok(resolved)
}

/// Model names are passed as one argument, so only reject values that could
/// read as a flag, name an `@file` argument, or carry unexpected characters.
pub(crate) fn valid_model_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with(['-', '@'])
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@[]-".contains(c))
}

#[async_trait]
impl Tool for NativeAgentTool {
    fn spec(&self) -> ToolSpec {
        let mut properties = json!({
            "prompt":{"type":"string"},
            "cwd":{
                "type":"string",
                "description":"Directory inside the workspace to run in, such as a project directory (\"scv\"). \
                    The agent loads that directory's AGENTS.md or CLAUDE.md and its project skills. \
                    Defaults to the workspace root."
            },
            "timeout_seconds":timeout_schema(self.timeouts)
        });
        if self.resume.is_supported() {
            properties["session"] = json!({
                "type":"string",
                "description":"The `session` handle an earlier call to this tool returned, such as \"codex-1\". \
                    Pass it to continue that conversation: the agent keeps its context, in the same cwd. \
                    Omit it to start a new conversation for unrelated work."
            });
        }
        if !self.model_args.is_empty() {
            properties["model"] = json!({
                "type":"string",
                "description":format!(
                    "{} Set only when the user asks for a specific model; \
                     omit to use the agent's configured default.",
                    self.model_hint
                )
            });
        }
        if !self.effort_args.is_empty() {
            properties["effort"] = json!({
                "type":"string",
                "enum":AGENT_EFFORTS,
                "description":"Reasoning effort. Set only when the user asks for one; \
                    omit to use the agent's configured default."
            });
        }
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "Launch the configured {} CLI as a nested coding agent (not sandboxed). \
                 Delegate substantial work here rather than doing it step by step with \
                 bash: research and web lookups, multi-file coding, and running tools, \
                 builds, and tests. Set cwd to the project the work is in so the agent \
                 follows that project's instructions and skills.{}",
                self.name,
                if self.resume.is_supported() {
                    " Each result carries a `session` handle: pass it back to follow up on the \
                     same work (answers, fixes, next steps) instead of repeating the context."
                } else {
                    " Each call starts a fresh conversation."
                }
            ),
            parameters: json!({
                "type":"object",
                "properties":properties,
                "required":["prompt"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        self.command_args(&args)?;
        Ok(ToolRisk::Delegate)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: AgentArgs = parse_args(arguments)?;
        let command_args = self.command_args(&args)?;
        let executable = self.resolved.as_ref().map_or_else(
            || self.command.as_str().into(),
            |path| path.display().to_string(),
        );
        let directory = args.cwd.as_deref().map_or_else(
            || "the workspace root".to_owned(),
            |cwd| format!("{:?} (inside the workspace)", bounded(cwd, 200)),
        );
        let conversation = args.session.as_deref().map_or_else(
            || " in a new conversation".to_owned(),
            |session| format!(", continuing conversation {session},"),
        );
        let timeout = self.timeouts.resolve(args.timeout_seconds)?;
        let permissions = if self.full_permission_args.is_some() {
            " FULL PERMISSIONS (permissions = \"full\"): the agent's own approval prompts \
             and sandbox are off, so it edits files, runs commands, and uses the network \
             without asking."
        } else {
            ""
        };
        Ok(format!(
            "Launch {executable} with args {command_args:?} and prompt {:?}{conversation} in {directory} for up to {} seconds. The nested agent has your user permissions.{permissions}",
            bounded(&args.prompt, 2000),
            timeout.as_secs()
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: AgentArgs = parse_args(&arguments)?;
        let command_args = self.command_args(&args)?;
        let cwd = resolve_agent_cwd(&context.workspace, args.cwd.as_deref())?;
        let executable = self.resolved.as_ref().ok_or_else(|| {
            ToolError(format!(
                "{} executable {:?} was not found on PATH or in the user's install directories",
                self.name, self.command
            ))
        })?;
        let agent = self.name.trim_start_matches("agent_");
        let turn = if self.resume.is_supported() {
            Some(self.conversations.begin(
                agent,
                args.session.as_deref(),
                &cwd,
                self.resume.assigns_id(),
            )?)
        } else {
            None
        };
        let pending = self.delegation.as_ref().map(|delegation| {
            delegation.registry.begin_at(
                delegation.owner_depth(),
                agent,
                &delegation.session,
                &cwd,
                turn.as_ref().map(|turn| (turn.handle.as_str(), turn.turn)),
            )
        });
        let run_id = pending.as_ref().map_or_else(
            || uuid::Uuid::new_v4().simple().to_string(),
            |pending| pending.handle.clone(),
        );
        let fixed_len = self.args.len()
            + self.full_permission_args.as_ref().map_or(0, Vec::len)
            + self.output.args().len();
        let (fixed, rest) = command_args.split_at(fixed_len);
        let (selections, prompt_args) = rest.split_at(rest.len() - self.prompt_args.len());
        let (base, modes) = fixed.split_at(self.args.len());
        let continuing = args.session.is_some();
        let (run_args, last_message) = self.run_args(&run_id);
        let resume = match (self.resume, &turn) {
            (
                Resume::Supported {
                    start,
                    subcommand,
                    options,
                    positional,
                },
                Some(turn),
            ) => {
                let vendor = turn.vendor.as_deref().unwrap_or_default();
                if continuing {
                    (
                        subcommand.iter().map(OsString::from).collect(),
                        session_args(options, vendor),
                        session_args(positional, vendor),
                    )
                } else if turn.vendor.is_some() {
                    (Vec::new(), session_args(start, vendor), Vec::new())
                } else {
                    Default::default()
                }
            }
            _ => Default::default(),
        };
        let (subcommand, session_options, positional): (
            Vec<OsString>,
            Vec<OsString>,
            Vec<OsString>,
        ) = resume;
        let mut process_args: Vec<OsString> = base.iter().map(OsString::from).collect();
        process_args.extend(subcommand);
        process_args.extend(modes.iter().map(OsString::from));
        process_args.extend(run_args);
        process_args.extend(session_options);
        process_args.extend(selections.iter().map(OsString::from));
        process_args.extend(positional);
        process_args.extend(prompt_args.iter().map(OsString::from));
        process_args.push(OsString::from(args.prompt));
        let mut environment = self.environment.clone();
        match &pending {
            Some(pending) => environment.extend(pending.environment.iter().cloned()),
            None => environment.push((
                delegation::DEPTH_VARIABLE.into(),
                (delegation::current_depth() + 1).to_string().into(),
            )),
        }
        let requested = self.timeouts.resolve(args.timeout_seconds)?;
        let registration = self
            .delegation
            .as_ref()
            .map(|delegation| Arc::clone(&delegation.registry))
            .zip(pending);
        let run = execute_agent_process(
            ProcessSpec {
                executable: executable.as_os_str().to_owned(),
                args: process_args,
                cwd,
                environment,
                sanitize_scv_environment: true,
                timeout: requested,
                output_limit: self.output_limit,
            },
            AgentStream::new(self.output, self.output_limit).with_progress(context.progress),
            registration,
            context.cancellation,
        )
        .await;
        let fallback = last_message
            .as_deref()
            .and_then(|path| take_file(path, self.output_limit));
        let run = run?;
        let result = run.stream.finish(run.exit, fallback);
        let conversation = turn.and_then(|turn| {
            let number = turn.turn;
            turn.finish(
                result.session.clone(),
                result.status == agent_output::RunStatus::Completed,
            )
            .map(|handle| (handle, number))
        });
        let (content, truncated) = result.to_json(
            agent,
            conversation
                .as_ref()
                .map(|(handle, turn)| (handle.as_str(), *turn)),
            run.exit_code,
            &run.stderr_tail,
            self.output_limit,
        );
        let mut output = ToolOutput {
            content,
            is_error: result.status != agent_output::RunStatus::Completed,
            truncated,
        };
        if output.is_error {
            add_sign_in_hint(&mut output, agent);
        }
        Ok(output)
    }
}

/// Point a failed agent run that reads like a missing sign-in at the host
/// command that fixes it, since the agent's own advice (`/login`) cannot be
/// followed from a remote chat.
pub(crate) fn add_sign_in_hint(output: &mut ToolOutput, agent: &str) {
    let lower = output.content.to_ascii_lowercase();
    let unauthenticated = [
        "not logged in",
        "not signed in",
        "not authenticated",
        "login",
        "log in",
        "unauthorized",
        "authentication",
        "missing_credential",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if !unauthenticated {
        return;
    }
    if let Ok(Value::Object(mut content)) = serde_json::from_str::<Value>(&output.content) {
        content.insert(
            "hint".into(),
            format!(
                "The {agent} CLI appears to be signed out of SCV's private agent home. \
                 The host owner can sign it in with: scv agents login {agent}"
            )
            .into(),
        );
        output.content = Value::Object(content).to_string();
    }
}

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

struct ProcessSpec {
    executable: OsString,
    args: Vec<OsString>,
    cwd: PathBuf,
    environment: Vec<(OsString, OsString)>,
    sanitize_scv_environment: bool,
    timeout: Duration,
    output_limit: usize,
}

async fn execute_process(
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
    delegation::untrack_spawned(pid as u32);
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
    Ok(ToolOutput {
        content,
        is_error: finished.timed_out || !finished.status.success(),
        truncated: collected.truncated,
    })
}

/// A delegated run's stdout reader, stderr tail, and how it ended.
struct AgentRun {
    stream: AgentStream,
    exit: RunExit,
    exit_code: Option<i32>,
    stderr_tail: String,
}

/// Run a native agent: stdout is parsed as it arrives rather than buffered,
/// stderr keeps only its tail, and the run is recorded in the delegation
/// registry while it lasts.
async fn execute_agent_process(
    spec: ProcessSpec,
    stream: AgentStream,
    registration: Option<(Arc<DelegationRegistry>, delegation::PendingDelegation)>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<AgentRun, ToolError> {
    let deadline = Instant::now() + spec.timeout;
    let mut child = spawn_process(&spec)?;
    let pid = child_pid(&child)?;
    // Bookkeeping must not fail the delegation: an unrecorded run is still
    // tagged, so a later sweep can find what it leaves behind.
    let guard: Option<DelegationGuard> =
        registration.and_then(|(registry, pending)| registry.register(pending, pid as u32).ok());
    let stdout = Arc::new(Mutex::new(stream));
    let stderr = Arc::new(Mutex::new(TailBuffer::new(STDERR_TAIL_BYTES)));
    let stdout_task = child
        .stdout
        .take()
        .map(|reader| tokio::spawn(drain_output(reader, Arc::clone(&stdout))));
    let stderr_task = child
        .stderr
        .take()
        .map(|reader| tokio::spawn(drain_output(reader, Arc::clone(&stderr))));
    let finished = supervise(
        &mut child,
        pid,
        deadline,
        cancellation,
        stdout_task,
        stderr_task,
    )
    .await;
    delegation::untrack_spawned(pid as u32);
    let killed = guard.as_ref().is_some_and(DelegationGuard::was_killed);
    if let Some(guard) = guard {
        guard.finish().await;
    }
    let finished = finished?;
    let exit = if finished.timed_out {
        RunExit::TimedOut
    } else if killed {
        RunExit::Killed
    } else {
        RunExit::Exited {
            success: finished.status.success(),
        }
    };
    let stderr_tail = stderr.lock().await.text();
    let stream = Arc::try_unwrap(stdout)
        .map_err(|_| ToolError("agent output reader is still running".into()))?
        .into_inner();
    Ok(AgentRun {
        stream,
        exit,
        exit_code: finished.status.code(),
        stderr_tail,
    })
}

fn spawn_process(spec: &ProcessSpec) -> Result<tokio::process::Child, ToolError> {
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
    let child = command
        .spawn()
        .map_err(|error| ToolError(format!("launch {:?}: {error}", spec.executable)))?;
    if let Some(pid) = child.id() {
        delegation::track_spawned(pid);
    }
    Ok(child)
}

fn child_pid(child: &tokio::process::Child) -> Result<i32, ToolError> {
    child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| ToolError("child process has no pid".into()))
}

struct Finished {
    status: std::process::ExitStatus,
    timed_out: bool,
}

/// Wait for a spawned process group until it exits, times out, or is
/// cancelled, always finishing the whole group and draining its output.
async fn supervise(
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
        status = child.wait() => Completion::Exited(status.map_err(|error| ToolError(format!("wait for child: {error}")))?),
        _ = cancellation.cancelled() => {
            Completion::Cancelled
        },
        _ = sleep_until(deadline) => Completion::TimedOut,
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
            return Err(ToolError("process cancelled".into()));
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

/// Where a child's output goes as it is read.
pub(crate) trait OutputSink: Send + 'static {
    fn push(&mut self, bytes: &[u8]);
}

impl OutputSink for BoundedOutput {
    fn push(&mut self, bytes: &[u8]) {
        BoundedOutput::push(self, bytes);
    }
}

impl OutputSink for AgentStream {
    fn push(&mut self, bytes: &[u8]) {
        AgentStream::push(self, bytes);
    }
}

impl OutputSink for TailBuffer {
    fn push(&mut self, bytes: &[u8]) {
        TailBuffer::push(self, bytes);
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

pub(crate) fn parse_args<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, ToolError> {
    serde_json::from_value(value.clone())
        .map_err(|error| ToolError(format!("invalid arguments: {error}")))
}

fn validate_read_args(args: &ReadArgs) -> Result<(), ToolError> {
    if args.limit == Some(0) {
        return Err(ToolError("read limit must be positive".into()));
    }
    Ok(())
}

pub(crate) fn validate_process_args(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError("command or prompt must be non-empty".into()));
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
    parent.join(format!(".scv-write-{}-{id}.tmp", std::process::id()))
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

pub(crate) fn bounded(value: &str, max_chars: usize) -> String {
    let mut output: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn test_conversations() -> Arc<ConversationStore> {
        Arc::new(ConversationStore::new(
            ToolsConfig::default().conversations,
            None,
        ))
    }

    /// `bash -l` sources the host's login profile before it runs a command,
    /// and CI images can spend seconds there under parallel test load. Waits
    /// that include shell startup use this ceiling; they end as soon as their
    /// condition holds.
    const SHELL_STARTUP: Duration = Duration::from_secs(30);

    /// Polls `probe` every 10 ms until it yields a value or `limit` passes.
    async fn wait_for<T>(limit: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if let Some(value) = probe() {
                return Some(value);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn is_gone(pid: i32) -> Option<()> {
        (unsafe { libc::kill(pid, 0) } != 0).then_some(())
    }

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
                ToolContext::new(
                    directory.path().canonicalize().unwrap(),
                    tokio_util::sync::CancellationToken::new(),
                ),
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
                ToolContext::new(
                    workspace.path().canonicalize().unwrap(),
                    tokio_util::sync::CancellationToken::new(),
                ),
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
            ToolContext::new(root.clone(), tokio_util::sync::CancellationToken::new()),
        )
        .await
        .unwrap();
        let hash = format!("{:x}", Sha256::digest(b"first"));
        tool.execute(
            json!({"path":"file.txt","content":"second","mode":"replace","expected_sha256":hash}),
            ToolContext::new(root.clone(), tokio_util::sync::CancellationToken::new()),
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
                ToolContext::new(root, tokio_util::sync::CancellationToken::new()),
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
                ToolContext::new(
                    workspace.path().canonicalize().unwrap(),
                    tokio_util::sync::CancellationToken::new(),
                ),
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
            max_timeout: Duration::from_millis(50),
            output_limit: 100,
        };
        let started = std::time::Instant::now();
        let output = tool
            .execute(
                json!({"command":"sleep 5"}),
                ToolContext::new(
                    workspace.path().canonicalize().unwrap(),
                    tokio_util::sync::CancellationToken::new(),
                ),
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
            timeout: SHELL_STARTUP,
            max_timeout: SHELL_STARTUP,
            output_limit: 8,
        };
        let output = tool
            .execute(
                json!({"command":"printf 12345678901234567890"}),
                ToolContext::new(
                    workspace.path().canonicalize().unwrap(),
                    tokio_util::sync::CancellationToken::new(),
                ),
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
            max_timeout: Duration::from_secs(30),
            output_limit: 100,
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancel = cancellation.clone();
        let started = std::time::Instant::now();
        let execution = tokio::spawn(async move {
            tool.execute(
                json!({"command":"sleep 30"}),
                ToolContext::new(workspace.path().canonicalize().unwrap(), cancellation),
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
            timeout: SHELL_STARTUP,
            max_timeout: SHELL_STARTUP,
            output_limit: 100,
        };
        let output = tool
            .execute(
                json!({"command":"sleep 60 & echo $! > background.pid; exit 0"}),
                ToolContext::new(root.clone(), tokio_util::sync::CancellationToken::new()),
            )
            .await
            .unwrap();
        let returned = std::time::SystemTime::now();
        assert!(!output.is_error);
        // Time from the shell's last write, which excludes its startup.
        let exited = std::fs::metadata(root.join("background.pid"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(returned.duration_since(exited).unwrap_or_default() < Duration::from_secs(3));
        let pid: i32 = std::fs::read_to_string(root.join("background.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_for(Duration::from_secs(5), || is_gone(pid))
                .await
                .is_some(),
            "background descendant {pid} survived tool completion"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_a_term_ignoring_descendant() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let tool = BashTool {
            timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(30),
            output_limit: 100,
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancel = cancellation.clone();
        let command_root = root.clone();
        let execution = tokio::spawn(async move {
            tool.execute(
                json!({"command":"trap '' TERM; (trap '' TERM; sleep 30) & echo $! > stubborn.pid; wait"}),
                ToolContext::new(command_root, cancellation),
            )
            .await
        });
        let pid_path = root.join("stubborn.pid");
        let pid = wait_for(SHELL_STARTUP, || {
            std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
        })
        .await
        .expect("command did not report its descendant pid");
        let started = std::time::Instant::now();
        cancel.cancel();
        let error = execution.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            wait_for(Duration::from_secs(5), || is_gone(pid))
                .await
                .is_some(),
            "TERM-ignoring descendant {pid} survived cancellation"
        );
    }

    /// Fake agents run through `bash` so no test ever executes a file that a
    /// concurrently forked test process may still hold open for writing
    /// (which fails spawning with ETXTBSY).
    fn fake_agent(
        workspace: &Path,
        name: &str,
        script: &str,
        args: &[&str],
        environment: Vec<(OsString, OsString)>,
    ) -> NativeAgentTool {
        fake_agent_with_prompt_args(workspace, name, script, args, &[], environment)
    }

    fn fake_agent_with_prompt_args(
        workspace: &Path,
        name: &str,
        script: &str,
        args: &[&str],
        prompt_args: &[&str],
        environment: Vec<(OsString, OsString)>,
    ) -> NativeAgentTool {
        let script_path = workspace.join("fake-agent.sh");
        std::fs::write(&script_path, script).unwrap();
        let mut fixed = vec![script_path.display().to_string()];
        fixed.extend(args.iter().map(|arg| arg.to_string()));
        NativeAgentTool::new(
            name.into(),
            AgentAdapterConfig {
                command: "bash".into(),
                args: fixed,
                prompt_args: prompt_args.iter().map(|arg| arg.to_string()).collect(),
                full_permission_args: None,
                model_args: vec!["--model".into(), "{model}".into()],
                effort_args: vec!["--effort".into(), "{effort}".into()],
                model_hint: adapters::adapter(name.trim_start_matches("agent_"))
                    .map_or(
                        "Model ID in the form this agent's CLI accepts.",
                        |adapter| adapter.model_hint,
                    )
                    .into(),
                environment,
                search_dirs: Vec::new(),
                output: OutputFormat::Text,
                resume: Resume::Unsupported,
                home: None,
                transport: Transport::Process,
            },
            Timeouts {
                default: Duration::from_secs(2),
                max: Duration::from_secs(5),
            },
            1024,
            None,
            test_conversations(),
        )
    }

    fn context(workspace: &Path) -> ToolContext {
        ToolContext::new(
            workspace.canonicalize().unwrap(),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn native_agent_preserves_argument_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = fake_agent(
            workspace.path(),
            "agent_fake",
            "pwd\nprintf '%s\\n' \"$@\"\n",
            &["--fixed"],
            Vec::new(),
        );
        let output = tool
            .execute(
                json!({"prompt":"hello; echo unsafe"}),
                context(workspace.path()),
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

    #[tokio::test]
    async fn native_agent_maps_model_and_effort_to_adapter_flags() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = fake_agent(
            workspace.path(),
            "agent_claude",
            "printf '%s\\n' \"$@\"\n",
            &["-p"],
            Vec::new(),
        );
        let properties = &tool.spec().parameters["properties"];
        assert_eq!(properties["effort"]["enum"], json!(AGENT_EFFORTS));
        assert_eq!(properties["model"]["type"], "string");
        let arguments = json!({"prompt":"hi","model":"sonnet","effort":"medium"});
        assert!(
            tool.approval_summary(&arguments)
                .unwrap()
                .contains(r#""--model", "sonnet", "--effort", "medium""#)
        );
        let output = tool
            .execute(arguments, context(workspace.path()))
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(output["reply"], "-p\n--model\nsonnet\n--effort\nmedium\nhi");
        for invalid in [
            json!({"prompt":"hi","model":"--dangerously-skip-permissions"}),
            json!({"prompt":"hi","model":"sonnet medium"}),
            json!({"prompt":"hi","effort":"extreme"}),
            json!({"prompt":"hi","model":"@/etc/passwd"}),
            json!({"prompt":"--resume"}),
        ] {
            assert!(tool.risk(&invalid).is_err());
        }
        let fixed_only = NativeAgentTool::new(
            "agent_pi".into(),
            AgentAdapterConfig {
                command: "pi".into(),
                args: vec!["-p".into()],
                prompt_args: Vec::new(),
                full_permission_args: None,
                model_args: Vec::new(),
                effort_args: Vec::new(),
                model_hint: String::new(),
                environment: Vec::new(),
                search_dirs: Vec::new(),
                output: OutputFormat::Text,
                resume: Resume::Unsupported,
                home: None,
                transport: Transport::Process,
            },
            Timeouts {
                default: Duration::from_secs(2),
                max: Duration::from_secs(2),
            },
            1024,
            None,
            test_conversations(),
        );
        assert!(
            fixed_only.spec().parameters["properties"]
                .get("model")
                .is_none()
        );
        let error = fixed_only
            .risk(&json!({"prompt":"hi","model":"sonnet"}))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not support selecting a model")
        );
    }

    #[test]
    fn native_agent_model_hints_name_the_adapter_family_and_default() {
        let workspace = tempfile::tempdir().unwrap();
        let description = |name: &str, field: &str| {
            fake_agent(workspace.path(), name, "", &[], Vec::new())
                .spec()
                .parameters["properties"][field]["description"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let claude = description("agent_claude", "model");
        let codex = description("agent_codex", "model");
        let other = description("agent_other", "model");
        assert!(claude.contains("sonnet or opus"));
        for text in [&codex, &other] {
            assert!(!text.contains("sonnet"), "{text}");
        }
        assert!(codex.contains("not a Claude alias"));
        for text in [claude, codex, other, description("agent_codex", "effort")] {
            assert!(
                text.contains("omit to use the agent's configured default"),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn signed_out_dsh_failure_names_the_host_login_command() {
        let workspace = tempfile::tempdir().unwrap();
        // DeepSeek Harness 0.1.7-rc.1's startup error without a key.
        let tool = fake_agent(
            workspace.path(),
            "agent_dsh",
            "echo 'dsh: MISSING_CREDENTIAL: llm-deepseek: no API key for provider route \"deepseek-official\"' >&2\nexit 1\n",
            &[],
            Vec::new(),
        );
        let output = tool
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        let content: Value = serde_json::from_str(&output.content).unwrap();
        assert!(
            content["hint"]
                .as_str()
                .unwrap()
                .ends_with("scv agents login dsh"),
            "{content}"
        );
    }

    #[tokio::test]
    async fn signed_out_agent_failure_names_the_host_login_command() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = fake_agent(
            workspace.path(),
            "agent_claude",
            "echo 'Not logged in · Please run /login'\nexit 1\n",
            &[],
            Vec::new(),
        );
        let output = tool
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        let content: Value = serde_json::from_str(&output.content).unwrap();
        assert!(
            content["hint"]
                .as_str()
                .unwrap()
                .ends_with("scv agents login claude")
        );
        let other = fake_agent(
            workspace.path(),
            "agent_claude",
            "echo 'disk full'\nexit 1\n",
            &[],
            Vec::new(),
        );
        let output = other
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(!output.content.contains("hint"));
    }

    #[tokio::test]
    async fn native_agent_uses_instance_private_environment() {
        let workspace = tempfile::tempdir().unwrap();
        let home = workspace.path().join("private-home");
        let tool = fake_agent(
            workspace.path(),
            "agent_codex",
            "printf 'HOME=%s\\nSCV_HOME=%s\\nCODEX_HOME=%s\\nSCV_CONFIG=%s\\nOPENAI_API_KEY=%s\\nCODEX_API_KEY=%s\\n' \"$HOME\" \"$SCV_HOME\" \"$CODEX_HOME\" \"${SCV_CONFIG-unset}\" \"${OPENAI_API_KEY-unset}\" \"${CODEX_API_KEY-unset}\"\n",
            &[],
            vec![
                ("HOME".into(), home.clone().into()),
                ("SCV_HOME".into(), home.clone().into()),
                ("CODEX_HOME".into(), home.join("codex").into()),
            ],
        );
        let output = tool
            .execute(
                json!({"prompt":"print environment"}),
                context(workspace.path()),
            )
            .await
            .unwrap();
        assert!(output.content.contains(&format!("HOME={}", home.display())));
        assert!(
            output
                .content
                .contains(&format!("CODEX_HOME={}/codex", home.display()))
        );
        assert!(output.content.contains("SCV_CONFIG=unset"));
        assert!(output.content.contains("OPENAI_API_KEY=unset"));
        assert!(output.content.contains("CODEX_API_KEY=unset"));
    }

    #[tokio::test]
    async fn native_agent_places_prompt_flags_just_before_the_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = fake_agent_with_prompt_args(
            workspace.path(),
            "agent_grok",
            "printf '%s\\n' \"$@\"\n",
            &[],
            &["-p"],
            Vec::new(),
        );
        let arguments = json!({"prompt":"hi","model":"grok-4","effort":"high"});
        assert!(
            tool.approval_summary(&arguments)
                .unwrap()
                .contains(r#""--model", "grok-4", "--effort", "high", "-p""#)
        );
        let output = tool
            .execute(arguments, context(workspace.path()))
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(output["reply"], "--model\ngrok-4\n--effort\nhigh\n-p\nhi");
    }

    #[tokio::test]
    async fn full_permissions_follow_the_fixed_arguments_and_are_announced() {
        let workspace = tempfile::tempdir().unwrap();
        let mut tool = fake_agent(
            workspace.path(),
            "agent_claude",
            "printf '%s\\n' \"$@\"\n",
            &["-p"],
            Vec::new(),
        );
        let arguments = json!({"prompt":"hi","model":"opus"});
        assert!(!tool.approval_summary(&arguments).unwrap().contains("FULL"));
        tool.full_permission_args =
            Some(vec!["--permission-mode".into(), "bypassPermissions".into()]);
        let summary = tool.approval_summary(&arguments).unwrap();
        assert!(summary.contains("FULL PERMISSIONS"), "{summary}");
        assert!(
            summary
                .contains(r#""-p", "--permission-mode", "bypassPermissions", "--model", "opus""#)
        );
        let output = tool
            .execute(arguments, context(workspace.path()))
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(
            output["reply"],
            "-p\n--permission-mode\nbypassPermissions\n--model\nopus\nhi"
        );
    }

    #[test]
    fn agent_environment_drops_inherited_credentials_but_keeps_its_own_home() {
        let mut command = std::process::Command::new("true");
        apply_agent_environment_from(
            &mut command,
            [
                "GROK_HOME",
                "XAI_API_KEY",
                "PI_CODING_AGENT_DIR",
                "DEEPSEEK_API_KEY",
                "ANTHROPIC_API_KEY",
                "OPENROUTER_API_KEY",
                "PATH",
            ]
            .map(OsString::from),
            &[("GROK_HOME".into(), "/private/.grok".into())],
        );
        let envs: HashMap<_, _> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
            .collect();
        assert_eq!(
            envs[&OsString::from("GROK_HOME")],
            Some(OsString::from("/private/.grok"))
        );
        for removed in [
            "XAI_API_KEY",
            "PI_CODING_AGENT_DIR",
            "DEEPSEEK_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            assert_eq!(envs[&OsString::from(removed)], None, "{removed}");
        }
        assert!(!envs.contains_key(&OsString::from("PATH")));
    }

    #[test]
    fn uninstalled_agents_are_not_offered() {
        let adapter = |command: &str| AgentAdapterConfig {
            command: command.into(),
            args: Vec::new(),
            prompt_args: Vec::new(),
            full_permission_args: None,
            model_args: Vec::new(),
            effort_args: Vec::new(),
            model_hint: String::new(),
            environment: Vec::new(),
            search_dirs: Vec::new(),
            output: OutputFormat::Text,
            resume: Resume::Unsupported,
            home: None,
            transport: Transport::Process,
        };
        let registry = builtin_registry(
            ToolsConfig::default(),
            SkillMap::new(),
            Vec::new(),
            1024,
            HashMap::from([
                ("agent_present".to_owned(), adapter("bash")),
                (
                    "agent_missing".to_owned(),
                    adapter("scv-test-agent-that-is-not-installed"),
                ),
            ]),
        )
        .unwrap();
        assert!(registry.get("agent_present").is_some());
        assert!(registry.get("agent_missing").is_none());
    }

    #[tokio::test]
    async fn native_agent_runs_in_a_contained_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        std::fs::write(root.join("notes.txt"), "not a directory").unwrap();
        symlink(outside.path(), root.join("escape")).unwrap();
        symlink(root.join("project"), root.join("inner-link")).unwrap();
        let tool = fake_agent(&root, "agent_codex", "pwd\n", &[], Vec::new());
        let run = |arguments: Value| tool.execute(arguments, context(&root));

        for arguments in [
            json!({"prompt":"hi"}),
            json!({"prompt":"hi","cwd":""}),
            json!({"prompt":"hi","cwd":"  ","model":"","effort":" "}),
        ] {
            let output = run(arguments.clone()).await.unwrap();
            let output: Value = serde_json::from_str(&output.content).unwrap();
            assert_eq!(output["reply"], root.display().to_string(), "{arguments}");
        }
        for cwd in [
            "project".to_owned(),
            "project/".to_owned(),
            "inner-link".to_owned(),
            root.join("project").display().to_string(),
        ] {
            let output = run(json!({"prompt":"hi","cwd":cwd})).await.unwrap();
            let output: Value = serde_json::from_str(&output.content).unwrap();
            assert_eq!(
                output["reply"],
                root.join("project").display().to_string(),
                "{cwd}"
            );
        }
        for (cwd, error) in [
            ("..", "outside the workspace"),
            ("escape", "outside the workspace"),
            ("/", "outside the workspace"),
            ("notes.txt", "not a directory"),
            ("missing", "No such file"),
        ] {
            let result = run(json!({"prompt":"hi","cwd":cwd})).await;
            assert!(
                result.as_ref().unwrap_err().to_string().contains(error),
                "{cwd}: {result:?}"
            );
        }
        assert!(tool.risk(&json!({"prompt":"hi","cwd":"a\0b"})).is_err());
        assert!(
            tool.risk(&json!({"prompt":"hi","cwd":"x".repeat(MAX_AGENT_CWD_BYTES + 1)}))
                .is_err()
        );
        let summary = tool
            .approval_summary(&json!({"prompt":"hi","cwd":"project","timeout_seconds":4}))
            .unwrap();
        assert!(summary.contains(r#"in "project" (inside the workspace) for up to 4 seconds"#));
        assert!(
            tool.approval_summary(&json!({"prompt":"hi"}))
                .unwrap()
                .contains("in the workspace root for up to 2 seconds")
        );
        let description = tool.spec().parameters["properties"]["cwd"]["description"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(description.contains("AGENTS.md"));
    }

    #[tokio::test]
    async fn per_call_timeouts_may_rise_to_the_ceiling_but_not_past_it() {
        let timeouts = Timeouts {
            default: Duration::from_secs(120),
            max: Duration::from_secs(1800),
        };
        assert_eq!(timeouts.resolve(None).unwrap(), Duration::from_secs(120));
        assert_eq!(timeouts.resolve(Some(30)).unwrap(), Duration::from_secs(30));
        assert_eq!(
            timeouts.resolve(Some(1800)).unwrap(),
            Duration::from_secs(1800)
        );
        assert!(timeouts.resolve(Some(0)).is_err());
        assert!(
            timeouts
                .resolve(Some(1801))
                .unwrap_err()
                .to_string()
                .contains("maximum of 1800 seconds (tools.max_timeout_seconds)")
        );

        let workspace = tempfile::tempdir().unwrap();
        let agent = fake_agent(
            workspace.path(),
            "agent_codex",
            "echo ran\n",
            &[],
            Vec::new(),
        );
        let schema = &agent.spec().parameters["properties"]["timeout_seconds"];
        assert_eq!(schema["maximum"], 5);
        assert!(
            schema["description"]
                .as_str()
                .unwrap()
                .contains("Defaults to 2; at most 5")
        );
        assert!(
            agent
                .risk(&json!({"prompt":"hi","timeout_seconds":5}))
                .is_ok()
        );
        assert!(
            agent
                .risk(&json!({"prompt":"hi","timeout_seconds":6}))
                .is_err()
        );
        assert!(
            agent
                .execute(
                    json!({"prompt":"hi","timeout_seconds":6}),
                    context(workspace.path())
                )
                .await
                .is_err()
        );

        let bash = BashTool {
            timeout: Duration::from_secs(1),
            max_timeout: Duration::from_secs(3),
            output_limit: 100,
        };
        assert_eq!(
            bash.spec().parameters["properties"]["timeout_seconds"]["maximum"],
            3
        );
        assert!(
            bash.risk(&json!({"command":"true","timeout_seconds":3}))
                .is_ok()
        );
        assert!(
            bash.risk(&json!({"command":"true","timeout_seconds":4}))
                .unwrap_err()
                .to_string()
                .contains("tools.max_timeout_seconds")
        );
    }

    /// A fake agent CLI in `format`, run through `bash script`, optionally
    /// recorded in `delegation`.
    fn structured_agent(
        workspace: &Path,
        name: &str,
        format: OutputFormat,
        script: &str,
        home: Option<PathBuf>,
        delegation: Option<DelegationContext>,
        timeout: Duration,
    ) -> NativeAgentTool {
        conversing_agent(
            workspace,
            name,
            format,
            Resume::Unsupported,
            script,
            home,
            delegation,
            timeout,
            test_conversations(),
        )
    }

    /// Like [`structured_agent`], continuing conversations as `resume` says,
    /// in `conversations` (shared by one session's tools).
    #[allow(clippy::too_many_arguments)]
    fn conversing_agent(
        workspace: &Path,
        name: &str,
        format: OutputFormat,
        resume: Resume,
        script: &str,
        home: Option<PathBuf>,
        delegation: Option<DelegationContext>,
        timeout: Duration,
        conversations: Arc<ConversationStore>,
    ) -> NativeAgentTool {
        let script_path = workspace.join(format!("fake-{name}.sh"));
        std::fs::write(&script_path, script).unwrap();
        NativeAgentTool::new(
            name.into(),
            AgentAdapterConfig {
                command: "bash".into(),
                args: vec![script_path.display().to_string()],
                prompt_args: Vec::new(),
                full_permission_args: None,
                model_args: Vec::new(),
                effort_args: Vec::new(),
                model_hint: String::new(),
                environment: Vec::new(),
                search_dirs: Vec::new(),
                output: format,
                resume,
                home,
                transport: Transport::Process,
            },
            Timeouts {
                default: timeout,
                max: Duration::from_secs(30),
            },
            64 * 1024,
            delegation,
            conversations,
        )
    }

    fn delegation_context(home: &Path) -> DelegationContext {
        DelegationContext {
            registry: Arc::new(DelegationRegistry::new(home)),
            session: "session-1".into(),
            depth: 0,
        }
    }

    #[tokio::test]
    async fn claude_stream_json_becomes_a_structured_result() {
        let workspace = tempfile::tempdir().unwrap();
        let args_file = workspace.path().join("args.txt");
        let script = format!(
            r#"printf '%s\n' "$@" > {args}
printf '%s\n' "$SCV_PARENT" "$SCV_DELEGATION_DEPTH" >> {args}
echo '{{"type":"system","subtype":"init","session_id":"x","unknown":[1,2]}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"thinking"}}]}}}}'
echo 'stray diagnostic' >&2
echo '{{"type":"result","subtype":"success","is_error":false,"result":"all done","usage":{{"input_tokens":12,"output_tokens":3}}}}'
"#,
            args = args_file.display()
        );
        let home = tempfile::tempdir().unwrap();
        let context_home = delegation_context(home.path());
        let tool = conversing_agent(
            workspace.path(),
            "agent_claude",
            OutputFormat::ClaudeStreamJson,
            adapters::adapter("claude").unwrap().resume,
            &script,
            None,
            Some(context_home.clone()),
            Duration::from_secs(10),
            test_conversations(),
        );
        let output = tool
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["agent"], "claude");
        assert_eq!(
            (value["session"].as_str(), value["turn"].as_u64()),
            (Some("claude-1"), Some(1))
        );
        assert_eq!(value["status"], "completed");
        assert_eq!(value["reply"], "all done");
        assert_eq!(value["usage"]["input_tokens"], 12);
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["stderr_tail"], "stray diagnostic");
        assert_eq!(value["truncated"], false);
        // No event log reaches the parent.
        assert!(!output.content.contains("thinking"));
        let recorded = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(
            &lines[..4],
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--session-id"
            ]
        );
        assert!(uuid::Uuid::parse_str(lines[4]).is_ok());
        assert_eq!(lines[5], "hi");
        let chain = lines[6];
        assert!(chain.contains("/session-1/claude-"), "{chain}");
        assert_eq!(lines[7], "1");
        // The run's record is gone once it ends.
        assert!(context_home.registry.list(true).is_empty());
    }

    /// A fake Codex that records each call's arguments, reports thread
    /// `th-1`, and answers with the prompt it was given. With `slow_start`,
    /// a first (non-resume) turn hangs after reporting its thread.
    fn fake_codex(workspace: &Path, slow_start: bool) -> String {
        let log = workspace.join("calls.txt");
        format!(
            r#"printf '%s\n' "$@" '--' >> {log}
case " $* " in *" resume "*) ;; *) echo '{{"type":"thread.started","thread_id":"th-1"}}'; {hang} ;; esac
for last; do :; done
echo "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"echo: $last\"}}}}"
echo '{{"type":"turn.completed","usage":{{"input_tokens":1,"output_tokens":1}}}}'
"#,
            log = log.display(),
            hang = if slow_start { "sleep 30" } else { ":" }
        )
    }

    fn calls(workspace: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(workspace.join("calls.txt"))
            .unwrap()
            .split("--\n")
            .filter(|call| !call.is_empty())
            .map(|call| call.lines().map(str::to_owned).collect())
            .collect()
    }

    #[tokio::test]
    async fn conversations_continue_the_cli_session_in_the_same_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("sub")).unwrap();
        let codex_resume = adapters::adapter("codex").unwrap().resume;
        let store = test_conversations();
        let tool = conversing_agent(
            workspace.path(),
            "agent_codex",
            OutputFormat::CodexJsonl,
            codex_resume,
            &fake_codex(workspace.path(), false),
            None,
            None,
            Duration::from_secs(10),
            Arc::clone(&store),
        );
        let first = tool
            .execute(
                json!({"prompt":"remember heron"}),
                context(workspace.path()),
            )
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&first.content).unwrap();
        assert_eq!(value["status"], "completed", "{value}");
        assert_eq!(
            (value["session"].as_str(), value["turn"].as_u64()),
            (Some("codex-1"), Some(1))
        );
        let second = tool
            .execute(
                json!({"prompt":"what word?","session":"codex-1"}),
                context(workspace.path()),
            )
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&second.content).unwrap();
        assert_eq!(value["reply"], "echo: what word?");
        assert_eq!(
            (value["session"].as_str(), value["turn"].as_u64()),
            (Some("codex-1"), Some(2))
        );
        // The script path is the only fixed argument, so `$@` starts after it:
        // `resume` comes right after the fixed arguments, and the CLI's thread
        // ID sits just before the prompt.
        let recorded = calls(workspace.path());
        assert_eq!(recorded[0], ["--json", "remember heron"]);
        assert_eq!(recorded[1], ["resume", "--json", "th-1", "what word?"]);

        // A conversation stays in its cwd.
        let moved = tool
            .execute(
                json!({"prompt":"x","session":"codex-1","cwd":"sub"}),
                context(workspace.path()),
            )
            .await
            .unwrap_err();
        assert!(moved.0.contains("runs in"), "{}", moved.0);
        // Another session's tools do not know this session's handles.
        let other_session = conversing_agent(
            workspace.path(),
            "agent_codex",
            OutputFormat::CodexJsonl,
            codex_resume,
            &fake_codex(workspace.path(), false),
            None,
            None,
            Duration::from_secs(10),
            test_conversations(),
        );
        let unknown = other_session
            .execute(
                json!({"prompt":"x","session":"codex-1"}),
                context(workspace.path()),
            )
            .await
            .unwrap_err();
        assert!(
            unknown.0.contains("unknown in this session"),
            "{}",
            unknown.0
        );
        assert_eq!(
            calls(workspace.path()).len(),
            2,
            "rejected turns never launch the CLI"
        );
        // The CLI's own ID is never accepted in place of a handle.
        let vendor = json!({"prompt":"x","session":"01a0cd5a-7195-7b31-a503-e235d5da7b45"});
        assert!(
            tool.risk(&vendor)
                .unwrap_err()
                .0
                .contains("not a conversation handle")
        );
        assert!(
            tool.spec().parameters["properties"]
                .get("session")
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_timed_out_turn_stays_resumable_and_unsupported_agents_refuse_sessions() {
        let workspace = tempfile::tempdir().unwrap();
        let tool = conversing_agent(
            workspace.path(),
            "agent_codex",
            OutputFormat::CodexJsonl,
            adapters::adapter("codex").unwrap().resume,
            &fake_codex(workspace.path(), true),
            None,
            None,
            Duration::from_secs(1),
            test_conversations(),
        );
        let first = tool
            .execute(json!({"prompt":"start"}), context(workspace.path()))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&first.content).unwrap();
        assert_eq!(value["status"], "timeout", "{value}");
        assert_eq!(value["session"], "codex-1");
        let resumed = tool
            .execute(
                json!({"prompt":"continue where you left off","session":"codex-1"}),
                context(workspace.path()),
            )
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&resumed.content).unwrap();
        assert_eq!(value["status"], "completed", "{value}");
        assert_eq!(value["turn"], 2);

        let plain = structured_agent(
            workspace.path(),
            "agent_grok",
            OutputFormat::Text,
            "echo hi\n",
            None,
            None,
            Duration::from_secs(5),
        );
        let refused = plain
            .risk(&json!({"prompt":"x","session":"grok-1"}))
            .unwrap_err();
        assert!(
            refused.0.contains("cannot continue a conversation"),
            "{}",
            refused.0
        );
        assert!(
            plain.spec().parameters["properties"]
                .get("session")
                .is_none()
        );
        let output = plain
            .execute(json!({"prompt":"x"}), context(workspace.path()))
            .await
            .unwrap();
        assert!(
            !output.content.contains("\"session\""),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn codex_json_reads_the_last_message_file_and_removes_it() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let script = r#"while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then printf 'final from file\n' > "$2"; echo "$2" > last-path.txt; fi
  shift
done
echo '{"type":"thread.started","thread_id":"t"}'
echo '{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":1}}'
"#;
        let tool = structured_agent(
            workspace.path(),
            "agent_codex",
            OutputFormat::CodexJsonl,
            script,
            Some(home.path().to_owned()),
            None,
            Duration::from_secs(10),
        );
        let output = tool
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["status"], "completed", "{value}");
        assert_eq!(value["reply"], "final from file");
        let path = std::fs::read_to_string(workspace.path().join("last-path.txt")).unwrap();
        let path = PathBuf::from(path.trim());
        assert!(path.starts_with(home.path().join("tmp")));
        assert!(!path.exists(), "the last-message file is removed");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(home.path().join("tmp"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn pi_json_and_signed_out_claude_results() {
        let workspace = tempfile::tempdir().unwrap();
        let pi = structured_agent(
            workspace.path(),
            "agent_pi",
            OutputFormat::PiJson,
            r#"echo '{"type":"session","id":"p"}'
echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"pi ok"}],"usage":{"input":7,"output":2}}}'
"#,
            None,
            None,
            Duration::from_secs(10),
        );
        let output = pi
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["reply"], "pi ok");
        assert_eq!(value["usage"]["output_tokens"], 2);

        let claude = structured_agent(
            workspace.path(),
            "agent_claude",
            OutputFormat::ClaudeStreamJson,
            r#"echo '{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}'
exit 1
"#,
            None,
            None,
            Duration::from_secs(10),
        );
        let output = claude
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["exit_code"], 1);
        assert!(
            value["hint"]
                .as_str()
                .unwrap()
                .contains("scv agents login claude")
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timed_out_run_and_its_detached_descendants_are_stopped() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let delegation = delegation_context(home.path());
        let tool = structured_agent(
            workspace.path(),
            "agent_codex",
            OutputFormat::CodexJsonl,
            // The detached sleep leaves the agent's process group and session.
            "setsid sleep 60 &\necho \"$SCV_PARENT\" > chain.txt\nexec sleep 60\n",
            None,
            Some(delegation.clone()),
            Duration::from_secs(1),
        );
        let output = tool
            .execute(json!({"prompt":"hi"}), context(workspace.path()))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["status"], "timeout");
        let chain = std::fs::read_to_string(workspace.path().join("chain.txt")).unwrap();
        let handle = chain.trim().rsplit('/').next().unwrap().to_owned();
        let tagged = || {
            std::fs::read_dir("/proc")
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    std::fs::read(entry.path().join("environ")).is_ok_and(|environ| {
                        environ
                            .split(|byte| *byte == 0)
                            .any(|entry| entry == format!("SCV_PARENT={}", chain.trim()).as_bytes())
                    })
                })
                .count()
        };
        let mut remaining = tagged();
        for _ in 0..100 {
            if remaining == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            remaining = tagged();
        }
        assert_eq!(remaining, 0, "tagged processes of {handle} survived");
        assert!(delegation.registry.list(true).is_empty());
    }

    #[test]
    fn agents_are_not_offered_at_the_delegation_depth_limit() {
        let adapter = AgentAdapterConfig {
            command: "bash".into(),
            args: Vec::new(),
            prompt_args: Vec::new(),
            full_permission_args: None,
            model_args: Vec::new(),
            effort_args: Vec::new(),
            model_hint: String::new(),
            environment: Vec::new(),
            search_dirs: Vec::new(),
            output: OutputFormat::Text,
            resume: Resume::Unsupported,
            home: None,
            transport: Transport::Process,
        };
        let home = tempfile::tempdir().unwrap();
        for (max_depth, offered) in [(0, false), (1, true)] {
            let registry = builtin_registry(
                ToolsConfig {
                    max_delegation_depth: max_depth,
                    delegation: Some(delegation_context(home.path())),
                    ..ToolsConfig::default()
                },
                SkillMap::new(),
                Vec::new(),
                1024,
                HashMap::from([("agent_claude".to_owned(), adapter.clone())]),
            )
            .unwrap();
            assert_eq!(registry.get("agent_claude").is_some(), offered);
            assert!(registry.get("bash").is_some());
        }
        // A client that is itself delegated (`session.start.delegation_depth`)
        // counts too, even though this process is not delegated.
        for (declared, max_depth, offered) in [(1, 1, false), (1, 2, true), (5, 2, false)] {
            let registry = builtin_registry(
                ToolsConfig {
                    max_delegation_depth: max_depth,
                    delegation: Some(DelegationContext {
                        depth: declared,
                        ..delegation_context(home.path())
                    }),
                    ..ToolsConfig::default()
                },
                SkillMap::new(),
                Vec::new(),
                1024,
                HashMap::from([("agent_claude".to_owned(), adapter.clone())]),
            )
            .unwrap();
            assert_eq!(
                registry.get("agent_claude").is_some(),
                offered,
                "declared {declared}, limit {max_depth}"
            );
        }
    }
}
