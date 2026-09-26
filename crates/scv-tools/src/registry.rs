//! [`builtin_registry`]: the tools one session offers its model.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use scv_core::{Tool, ToolError, ToolRegistry};

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
        background, choice,
        conversation::ConversationStore,
        native::NativeAgentTool,
        records,
        scv::ScvAgentTool,
    },
};

/// The tools a session offers: the built-in ones, then an `agent_*` tool for
/// each installed agent below the delegation depth limit, wrapped so it can
/// run in the background and name the other agents, and the job tools
/// (`agent_wait`, `agent_status`, `agent_cancel`) when there are agents.
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
    if let Some(chat) = config.chat_attach.clone() {
        registry.register(Arc::new(chat_attach::ChatAttachTool { config: chat }))?;
    }
    // A delegated SCV at the depth limit may not delegate further.
    let depth = config
        .delegation
        .as_ref()
        .map_or_else(records::current_depth, DelegationContext::owner_depth);
    let adapters = if depth < config.max_delegation_depth {
        adapters
    } else {
        HashMap::new()
    };
    // One job store per session, shared by its agent tools and dropped with
    // it, which cancels the jobs still running.
    let jobs = (config.max_background > 0).then(|| {
        config.background.clone().unwrap_or_else(|| {
            Arc::new(background::BackgroundJobs::new(config.max_background, None))
        })
    });
    // Agents are registered together once all are known, so each can name
    // the others as fallbacks.
    let mut found: Vec<FoundAgent> = Vec::new();
    // One store per session, shared by its agent tools and dropped with it.
    let conversations = Arc::new(ConversationStore::new(
        config.conversations,
        config
            .delegation
            .as_ref()
            .map(|context| context.registry.conversation_dir().to_owned()),
    ));
    for (name, adapter) in adapters {
        let (tool_name, use_for, model, effort) = (
            name.clone(),
            adapter.use_for.clone(),
            adapter.model.clone(),
            adapter.effort.clone(),
        );
        let mut register_agent = |_: &mut ToolRegistry, tool: Arc<dyn Tool>| {
            found.push(FoundAgent {
                name: tool_name.clone(),
                tool,
                use_for: use_for.clone(),
                model: model.clone(),
                effort: effort.clone(),
            });
            Ok::<(), ToolError>(())
        };
        if adapter.transport == Transport::ScvProtocol {
            let resolved =
                adapters::resolve_agent_executable(&adapter.command, &adapter.search_dirs);
            // An agent that is not installed is not offered to the model.
            if resolved.is_some() {
                register_agent(
                    &mut registry,
                    Arc::new(ScvAgentTool {
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
                    }),
                )?;
            }
            continue;
        }
        if let Some(launch) = adapter.acp.clone() {
            let resolved =
                adapters::resolve_agent_executable(&launch.command, &adapter.search_dirs);
            if resolved.is_some() {
                register_agent(
                    &mut registry,
                    Arc::new(AcpAgentTool::new(
                        name,
                        &adapter,
                        launch,
                        resolved,
                        Timeouts {
                            default: config.agent_timeout,
                            max: config.max_timeout,
                        },
                        config.output_limit_bytes,
                        config.delegation.clone(),
                        Arc::clone(&conversations),
                    )),
                )?;
                continue;
            }
            if launch.required {
                // `transport = "acp"` without its server: not offered.
                continue;
            }
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
            register_agent(&mut registry, Arc::new(tool))?;
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<String> = found.iter().map(|agent| agent.name.clone()).collect();
    let agents = found.len();
    for agent in found {
        let tool: Arc<dyn Tool> = Arc::new(choice::ChosenAgent {
            inner: agent.tool,
            use_for: agent.use_for,
            model: agent.model,
            effort: agent.effort,
            alternatives: names
                .iter()
                .filter(|other| **other != agent.name)
                .cloned()
                .collect(),
        });
        match &jobs {
            Some(jobs) => registry.register(Arc::new(background::BackgroundCapable {
                inner: tool,
                jobs: Arc::clone(jobs),
            }))?,
            None => registry.register(tool)?,
        }
    }
    if let Some(jobs) = jobs.filter(|_| agents > 0) {
        registry.register(Arc::new(background::WaitTool {
            jobs: Arc::clone(&jobs),
            timeouts: Timeouts {
                default: config.agent_timeout,
                max: config.max_timeout,
            },
        }))?;
        registry.register(Arc::new(background::StatusTool {
            jobs: Arc::clone(&jobs),
        }))?;
        registry.register(Arc::new(background::CancelTool { jobs }))?;
    }
    Ok(registry)
}

struct FoundAgent {
    name: String,
    tool: Arc<dyn Tool>,
    use_for: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

#[cfg(test)]
mod tests;
