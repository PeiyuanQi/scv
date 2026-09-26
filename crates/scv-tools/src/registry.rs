//! [`builtin_registry`]: the tools one session offers its model.

use std::{path::PathBuf, sync::Arc};

use scv_core::{ToolError, ToolRegistry};

use crate::{
    AgentAdapterConfig, DelegationContext, SkillMap, ToolsConfig,
    args::Timeouts,
    builtin::{
        chat_attach,
        fs::{ReadTool, WriteTool},
        shell::BashTool,
        skill::ReadSkillTool,
    },
    delegate::{
        acp::AcpAgentTool,
        adapters::{self, Transport},
        agent::{AgentTool, Backend, Offered},
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
    let (backend, accepts): (Arc<dyn Backend>, _) = if adapter.transport == Transport::ScvProtocol {
        let resolved = adapters::resolve_agent_executable(&adapter.command, &adapter.search_dirs)?;
        (
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
        )
    } else if let Some(launch) = adapter.acp.clone()
        && let Some(resolved) =
            adapters::resolve_agent_executable(&launch.command, &adapter.search_dirs)
    {
        let tool = AcpAgentTool::new(
            name.clone(),
            &adapter,
            launch,
            Some(resolved),
            timeouts,
            config.output_limit_bytes,
            config.delegation.clone(),
            Arc::clone(conversations),
        );
        let accepts = tool.accepts();
        (Arc::new(tool), accepts)
    } else if adapter.acp.as_ref().is_some_and(|launch| launch.required) {
        // `transport = "acp"` without its server: not offered.
        return None;
    } else {
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
    };
    Some(Offered {
        name,
        backend,
        accepts,
        model_hint: adapter.model_hint,
        use_for: adapter.use_for,
        model: adapter.model,
        effort: adapter.effort,
    })
}

#[cfg(test)]
mod tests;
