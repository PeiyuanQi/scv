//! SCV's bounded, workspace-aware built-in tools.

pub mod adapters;

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
        }
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
    for (name, adapter) in adapters {
        let tool = NativeAgentTool::new(
            name,
            adapter,
            Timeouts {
                default: config.agent_timeout,
                max: config.max_timeout,
            },
            config.output_limit_bytes,
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
struct Timeouts {
    default: Duration,
    max: Duration,
}

impl Timeouts {
    /// The call's timeout: its own request up to the ceiling, else the
    /// default. A request above the ceiling is refused, never clamped, so the
    /// caller learns the limit instead of being cut off early.
    fn resolve(self, requested: Option<u64>) -> Result<Duration, ToolError> {
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

fn timeout_schema(timeouts: Timeouts) -> Value {
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
        // The prompt follows the flags as a positional argument, so it must
        // not be readable as one.
        if args.prompt.starts_with('-') {
            return Err(ToolError("agent prompt must not start with '-'".into()));
        }
        let mut command = self.args.clone();
        command.extend(self.full_permission_args.iter().flatten().cloned());
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
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    prompt: String,
    timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "blank_as_none")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    model: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    effort: Option<String>,
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

fn validate_agent_cwd(cwd: &str) -> Result<(), ToolError> {
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
fn resolve_agent_cwd(workspace: &Path, cwd: Option<&str>) -> Result<PathBuf, ToolError> {
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
fn valid_model_name(value: &str) -> bool {
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
                 follows that project's instructions and skills.",
                self.name
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
        let timeout = self.timeouts.resolve(args.timeout_seconds)?;
        let permissions = if self.full_permission_args.is_some() {
            " FULL PERMISSIONS (permissions = \"full\"): the agent's own approval prompts \
             and sandbox are off, so it edits files, runs commands, and uses the network \
             without asking."
        } else {
            ""
        };
        Ok(format!(
            "Launch {executable} with args {command_args:?} and prompt {:?} in {directory} for up to {} seconds. The nested agent has your user permissions.{permissions}",
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
        let mut command_args: Vec<OsString> =
            command_args.into_iter().map(OsString::from).collect();
        command_args.push(OsString::from(args.prompt));
        let requested = self.timeouts.resolve(args.timeout_seconds)?;
        let mut output = execute_process(
            ProcessSpec {
                executable: executable.as_os_str().to_owned(),
                args: command_args,
                cwd,
                environment: self.environment.clone(),
                sanitize_scv_environment: true,
                timeout: requested,
                output_limit: self.output_limit,
            },
            context.cancellation,
        )
        .await?;
        if output.is_error {
            add_sign_in_hint(&mut output, self.name.trim_start_matches("agent_"));
        }
        Ok(output)
    }
}

/// Point a failed agent run that reads like a missing sign-in at the host
/// command that fixes it, since the agent's own advice (`/login`) cannot be
/// followed from a remote chat.
fn add_sign_in_hint(output: &mut ToolOutput, agent: &str) {
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
    let mut command = Command::new(&spec.executable);
    if spec.sanitize_scv_environment {
        apply_agent_environment(command.as_std_mut(), &spec.environment);
    } else {
        command.envs(spec.environment);
    }
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

fn validate_process_args(value: &str) -> Result<(), ToolError> {
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

fn bounded(value: &str, max_chars: usize) -> String {
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
            max_timeout: Duration::from_millis(50),
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
            timeout: SHELL_STARTUP,
            max_timeout: SHELL_STARTUP,
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
            max_timeout: Duration::from_secs(30),
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
            timeout: SHELL_STARTUP,
            max_timeout: SHELL_STARTUP,
            output_limit: 100,
        };
        let output = tool
            .execute(
                json!({"command":"sleep 60 & echo $! > background.pid; exit 0"}),
                ToolContext {
                    workspace: root.clone(),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
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
                ToolContext {
                    workspace: command_root,
                    cancellation,
                },
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
            },
            Timeouts {
                default: Duration::from_secs(2),
                max: Duration::from_secs(5),
            },
            1024,
        )
    }

    fn context(workspace: &Path) -> ToolContext {
        ToolContext {
            workspace: workspace.canonicalize().unwrap(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
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
        assert_eq!(
            output["output"],
            "-p\n--model\nsonnet\n--effort\nmedium\nhi\n"
        );
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
            },
            Timeouts {
                default: Duration::from_secs(2),
                max: Duration::from_secs(2),
            },
            1024,
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
        assert_eq!(
            output["output"],
            "--model\ngrok-4\n--effort\nhigh\n-p\nhi\n"
        );
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
            output["output"],
            "-p\n--permission-mode\nbypassPermissions\n--model\nopus\nhi\n"
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
            assert_eq!(
                output["output"],
                format!("{}\n", root.display()),
                "{arguments}"
            );
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
                output["output"],
                format!("{}\n", root.join("project").display()),
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
}
