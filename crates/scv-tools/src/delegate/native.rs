//! [`NativeAgentTool`]: one agent CLI process per turn, its stdout parsed
//! as it arrives, continuing a conversation through the CLI's own resume
//! flags when the adapter supports it.

use std::{
    ffi::OsString,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde_json::{Value, json};
use tokio::{sync::Mutex, time::Instant};

use crate::{
    AgentAdapterConfig, DelegationContext,
    args::{Timeouts, bounded, parse_args, timeout_schema, validate_process_args},
    delegate::{
        adapters::{self, OutputFormat, Resume},
        conversation::{self, ConversationStore},
        output::{self, AgentStream, RunExit, STDERR_TAIL_BYTES, TailBuffer, add_sign_in_hint},
        records::{self, DelegationGuard, DelegationRegistry},
        request::{
            AGENT_EFFORTS, AgentArgs, resolve_agent_cwd, valid_effort, valid_model_name,
            validate_agent_cwd,
        },
    },
    process::{ProcessSpec, child_pid, drain_output, spawn_process, supervise},
};

pub(crate) struct NativeAgentTool {
    name: String,
    command: String,
    /// The executable found on `PATH` or in the install directories; an
    /// agent that is not installed is not offered.
    pub(crate) resolved: Option<PathBuf>,
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
                return Err(ToolError::invalid_arguments(format!(
                    "{} cannot continue a conversation; omit session to start a new one",
                    self.name
                )));
            }
            if !conversation::is_handle(session) {
                return Err(ToolError::invalid_arguments(format!(
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
            return Err(ToolError::invalid_arguments(
                "agent prompt must not start with '-'",
            ));
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
                return Err(ToolError::invalid_arguments(format!(
                    "{} does not support selecting a {field}",
                    self.name
                )));
            }
            let valid = if field == "model" {
                valid_model_name(value)
            } else {
                valid_effort(value)
            };
            if !valid {
                return Err(ToolError::invalid_arguments(format!(
                    "invalid {field} {value:?}"
                )));
            }
            command.extend(template.iter().map(|part| part.replace(placeholder, value)));
        }
        command.extend(self.prompt_args.iter().cloned());
        Ok(command)
    }
    pub(crate) fn new(
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
                    "{} Set when the user asks, or when the work matches a configured \
                     use_for default; omit to use the agent's configured default.",
                    self.model_hint
                )
            });
        }
        if !self.effort_args.is_empty() {
            properties["effort"] = json!({
                "type":"string",
                "enum":AGENT_EFFORTS,
                "description":"Reasoning effort. Set when the user asks, or when the work \
                    matches a configured use_for default; omit to use the agent's configured default."
            });
        }
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "Runs its CLI as a nested coding agent (not sandboxed). Delegate substantial \
                 work here rather than doing it step by step with bash: research and web \
                 lookups, multi-file coding, and running tools, builds, and tests. Give it a \
                 self-contained brief, since it does not see this conversation, and set cwd \
                 to the project the work is in so it follows that project's instructions \
                 and skills.{}",
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
            ToolError::unavailable(format!(
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
                records::DEPTH_VARIABLE.into(),
                (records::current_depth() + 1).to_string().into(),
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
        let mut result = run.stream.finish(run.exit, fallback);
        // A failed CLI's stderr is its own diagnostics, never the reply.
        let stderr = run.stderr_tail.trim();
        if result.status == output::RunStatus::Failed
            && !stderr.is_empty()
            && !result
                .error
                .as_deref()
                .is_some_and(|error| error.contains(stderr))
        {
            result.error = Some(match result.error.take() {
                Some(error) => format!("{error}\n{stderr}"),
                None => stderr.to_owned(),
            });
        }
        let conversation = turn.and_then(|turn| {
            let number = turn.turn;
            turn.finish(
                result.session.clone(),
                result.status == output::RunStatus::Completed,
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
            failure: result.status.failure(),
            truncated,
        };
        if output.is_error() {
            add_sign_in_hint(&mut output, agent);
        }
        Ok(output)
    }
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
    registration: Option<(Arc<DelegationRegistry>, records::PendingDelegation)>,
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
    records::untrack_spawned(pid as u32);
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
        .map_err(|_| ToolError::failed("agent output reader is still running"))?
        .into_inner();
    Ok(AgentRun {
        stream,
        exit,
        exit_code: finished.status.code(),
        stderr_tail,
    })
}

#[cfg(test)]
mod tests;
