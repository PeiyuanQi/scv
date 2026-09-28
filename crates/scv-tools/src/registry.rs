//! [`builtin_registry`]: the tools one session offers its model.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use scv_core::{ToolContext, ToolError, ToolRegistry};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentAdapterConfig, DelegationContext, SkillMap, ToolsConfig,
    args::Timeouts,
    builtin::{
        chat_attach,
        chat_history::{ChatHistoryTool, ChatKeepTool},
        fs::{ReadTool, WriteTool},
        shell::BashTool,
        skill::ReadSkillTool,
    },
    delegate::{
        acp::AcpAgentTool,
        adapters::{self, Transport},
        agent::{AGENT_TOOL, AgentTool, Backend, Offered},
        background,
        conversation::ConversationStore,
        native::NativeAgentTool,
        records,
        scv::ScvAgentTool,
    },
};

/// The tools a session offers: the built-in ones, then, when an installed
/// agent is offered below the delegation depth limit, the `agent` tool,
/// able to run in the background, and the job tools (`agent_wait`,
/// `agent_status`, `agent_cancel`).
pub fn builtin_registry(
    config: ToolsConfig,
    skills: SkillMap,
    skill_roots: Vec<PathBuf>,
    max_skill_bytes: usize,
    adapters: impl IntoIterator<Item = (String, AgentAdapterConfig)>,
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
    if let Some(chat) = config.chat_attach.clone() {
        registry.register(Arc::new(chat_attach::ChatAttachTool { config: chat }))?;
    }
    if let Some(history) = config.chat_history.clone() {
        registry.register(Arc::new(ChatHistoryTool {
            config: history.clone(),
        }))?;
        registry.register(Arc::new(ChatKeepTool { config: history }))?;
    }
    // A delegated SCV at the depth limit may not delegate further.
    let depth = config
        .delegation
        .as_ref()
        .map_or_else(records::current_depth, DelegationContext::owner_depth);
    if depth >= config.max_delegation_depth {
        return Ok(registry);
    }
    // One store per session, shared by its agents and dropped with it.
    let conversations = Arc::new(ConversationStore::new(
        config.conversations,
        config
            .delegation
            .as_ref()
            .map(|context| context.registry.conversation_dir().to_owned()),
    ));
    let offered: Vec<Offered> = adapters
        .into_iter()
        .filter_map(|(name, adapter)| offer(name, adapter, &config, &conversations))
        .collect();
    if offered.is_empty() {
        return Ok(registry);
    }
    let timeouts = Timeouts {
        default: config.agent_timeout,
        max: config.max_timeout,
    };
    let agent = Arc::new(AgentTool::new(offered, &config.prefer, timeouts));
    if config.max_background == 0 {
        registry.register(agent)?;
        return Ok(registry);
    }
    // One job store per session, dropped with it, which cancels the jobs
    // still running.
    let jobs = config
        .background
        .clone()
        .unwrap_or_else(|| Arc::new(background::BackgroundJobs::new(config.max_background, None)));
    registry.register(Arc::new(background::BackgroundCapable {
        inner: agent,
        jobs: Arc::clone(&jobs),
    }))?;
    registry.register(Arc::new(background::WaitTool {
        jobs: Arc::clone(&jobs),
        timeouts,
    }))?;
    registry.register(Arc::new(background::StatusTool {
        jobs: Arc::clone(&jobs),
    }))?;
    registry.register(Arc::new(background::CancelTool { jobs }))?;
    Ok(registry)
}

/// How long `call_agent` waits for its agent to shut down after the call
/// before stopping what is left of it.
const CHECK_STOP_WAIT: Duration = Duration::from_secs(10);

/// Make one `agent` call to `name` from `cwd`, as a session's `agent` tool
/// makes it, for `scv agents check`. The call runs in the foreground and
/// may take `timeout`; when `interrupted` completes first, it is cancelled.
/// The agent's model list decides, not a saved one. The run is recorded
/// under `delegation`, like a session's, and this returns only once the
/// agent's process group is gone. The result is the tool's JSON result, or
/// SCV's own error when the call was refused or nothing could run.
pub async fn call_agent(
    name: &str,
    adapter: AgentAdapterConfig,
    cwd: &Path,
    arguments: Value,
    timeout: Duration,
    delegation: DelegationContext,
    interrupted: impl Future<Output = ()>,
) -> Result<Value, String> {
    let records = Arc::clone(&delegation.registry);
    let session = delegation.session.clone();
    let result = call_once(
        name,
        adapter,
        cwd,
        arguments,
        timeout,
        delegation,
        interrupted,
    )
    .await;
    // Dropping the tool asked its agent to stop; wait for its whole group.
    let deadline = tokio::time::Instant::now() + CHECK_STOP_WAIT;
    loop {
        let left: Vec<String> = records
            .list(true)
            .into_iter()
            .filter(|entry| entry.record.session == session)
            .map(|entry| entry.record.handle)
            .collect();
        if left.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            for handle in left {
                let _ = records.kill(&handle).await;
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    result
}

async fn call_once(
    name: &str,
    adapter: AgentAdapterConfig,
    cwd: &Path,
    arguments: Value,
    timeout: Duration,
    delegation: DelegationContext,
    interrupted: impl Future<Output = ()>,
) -> Result<Value, String> {
    let defaults = ToolsConfig::default();
    let tools = ToolsConfig {
        agent_timeout: timeout,
        max_timeout: timeout.max(defaults.max_timeout),
        max_background: 0,
        prefer: vec![name.to_owned()],
        delegation: Some(delegation),
        precheck_agent_models: false,
        ..defaults
    };
    let registry = builtin_registry(
        tools,
        SkillMap::new(),
        Vec::new(),
        0,
        [(name.to_owned(), adapter)],
    )
    .map_err(|error| error.message)?;
    let agent = registry.get(AGENT_TOOL).ok_or_else(|| {
        format!(
            "{name} is not offered here: it is not installed, or this SCV is at its delegation \
             depth limit"
        )
    })?;
    let cancellation = CancellationToken::new();
    let call = agent.execute(
        arguments,
        ToolContext::new(cwd.to_owned(), cancellation.clone()),
    );
    tokio::pin!(call);
    let result = tokio::select! {
        result = &mut call => result,
        () = interrupted => {
            // The call stops its agent before it returns.
            cancellation.cancel();
            call.await
        }
    };
    let output = result.map_err(|error| error.message)?;
    serde_json::from_str(&output.content).map_err(|_| output.content)
}

/// How SCV reaches an agent, decided from its adapter settings and what is
/// installed; the `agent` tool and `scv agents check` decide the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Its Agent Client Protocol server.
    Acp(PathBuf),
    /// Its CLI, once per turn.
    Cli(PathBuf),
    /// A nested `scv server --stdio`.
    Scv(PathBuf),
    /// Not installed: the named command resolves nowhere, so the agent is
    /// not offered.
    Missing(String),
}

/// How SCV would reach the agent `adapter` configures.
pub fn reach(adapter: &AgentAdapterConfig) -> Reach {
    let resolve = |command: &str| adapters::resolve_agent_executable(command, &adapter.search_dirs);
    if adapter.transport == Transport::ScvProtocol {
        return resolve(&adapter.command)
            .map_or_else(|| Reach::Missing(adapter.command.clone()), Reach::Scv);
    }
    if let Some(launch) = &adapter.acp {
        if let Some(server) = resolve(&launch.command) {
            return Reach::Acp(server);
        }
        // `transport = "acp"` never falls back to the CLI.
        if launch.required {
            return Reach::Missing(launch.command.clone());
        }
    }
    resolve(&adapter.command).map_or_else(|| Reach::Missing(adapter.command.clone()), Reach::Cli)
}

/// The `model` hint for an agent reached over ACP before SCV has seen what
/// its server offers; once it has, the description lists the values instead.
const ACP_MODEL_HINT: &str = "a value its ACP server lists, which SCV has not seen yet; omit model \
     unless the user names one, and a value the server does not list fails naming those it does";

/// The `model` hint for an agent whose ACP server listed its options but no
/// model to choose; a listed model replaces it.
const ACP_NO_MODEL_HINT: &str = "its ACP server lists no model to choose, so omit model";

/// The agent `name` as the `agent` tool offers it, on the backend its
/// adapter and transport call for; `None` when it is not installed.
fn offer(
    name: String,
    adapter: AgentAdapterConfig,
    config: &ToolsConfig,
    conversations: &Arc<ConversationStore>,
) -> Option<Offered> {
    let timeouts = Timeouts {
        default: config.agent_timeout,
        max: config.max_timeout,
    };
    let mut model_hint = adapter.model_hint.clone();
    let mut offered = None;
    let reached = reach(&adapter);
    // An ACP server or nested SCV keeps a conversation's model and effort;
    // a CLI started once per turn is given them again on every turn.
    let holds_settings = !matches!(reached, Reach::Cli(_));
    let (backend, accepts): (Arc<dyn Backend>, _) = match reached {
        Reach::Missing(_) => return None,
        Reach::Scv(resolved) => (
            Arc::new(ScvAgentTool {
                name: name.clone(),
                command: adapter.command.clone(),
                resolved: Some(resolved),
                args: adapter.args.clone(),
                environment: adapter.environment.clone(),
                timeouts,
                output_limit: config.output_limit_bytes,
                delegation: config.delegation.clone(),
                conversations: Arc::clone(conversations),
            }),
            ScvAgentTool::ACCEPTS,
        ),
        Reach::Acp(resolved) => {
            let launch = adapter.acp.clone()?;
            let tool = AcpAgentTool::new(
                name.clone(),
                &adapter,
                launch,
                Some(resolved),
                timeouts,
                config.output_limit_bytes,
                config.delegation.clone(),
                Arc::clone(conversations),
            )
            .with_precheck(config.precheck_agent_models);
            let accepts = tool.accepts();
            offered = tool.offered();
            // Its CLI's own model names may not be what the server accepts.
            model_hint = if offered.is_some() {
                ACP_NO_MODEL_HINT
            } else {
                ACP_MODEL_HINT
            }
            .to_owned();
            (Arc::new(tool), accepts)
        }
        Reach::Cli(_) => {
            let tool = NativeAgentTool::new(
                name.clone(),
                adapter.clone(),
                timeouts,
                config.output_limit_bytes,
                config.delegation.clone(),
                Arc::clone(conversations),
            );
            tool.resolved.as_ref()?;
            let accepts = tool.accepts();
            (Arc::new(tool), accepts)
        }
    };
    Some(Offered {
        name,
        backend,
        accepts,
        model_hint,
        offered,
        use_for: adapter.use_for,
        defaults: adapter.defaults,
        holds_settings,
    })
}

#[cfg(test)]
mod tests;
