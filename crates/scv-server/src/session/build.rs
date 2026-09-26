//! Building a session for `session.start`: configuration, provider, tools,
//! background jobs, and the system prompt.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use scv_core::{AgentRuntime, ApprovalGate, BudgetContextPolicy, ToolRegistry};
use scv_provider_openai::OpenAiProvider;
use scv_tools::{
    DelegationContext, background::BackgroundJobs, builtin_registry,
    delegation::DelegationRegistry, stores,
};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use super::{Session, SessionClient};
use crate::{
    approval::UnattendedGate,
    config::{self, Config, ConfigOverrides},
    prompt::{PromptContext, SkillListings, build_system_prompt, skills::discover_skills},
};

/// The configured agents to offer: signed-out ones are left out when their
/// sign-in state is a local file SCV can check cheaply; the rest are offered
/// and fail with a sign-in hint if they turn out to be signed out.
pub(crate) fn offered_adapters(config: &Config) -> HashMap<String, scv_tools::AgentAdapterConfig> {
    let mut adapters = config.adapters();
    adapters.retain(|tool, adapter| {
        let descriptor = tool
            .strip_prefix("agent_")
            .and_then(scv_tools::adapters::adapter);
        match (
            descriptor.map(|descriptor| descriptor.status),
            &adapter.home,
        ) {
            (Some(scv_tools::adapters::Status::Stored(store)), Some(home)) => !matches!(
                stores::stored_status(store, home),
                Ok(stores::StoredStatus { ready: false, .. })
            ),
            _ => true,
        }
    });
    adapters
}

/// Agent tools that delegate work, as opposed to observing or stopping jobs.
pub(crate) fn agent_tool_names(tools: &ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = tools
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .filter(|name| {
            name.starts_with("agent_")
                && !["agent_wait", "agent_status", "agent_cancel"].contains(&name.as_str())
        })
        .collect();
    names.sort();
    names
}

/// `delegation_depth` is the depth the client declared in `session.start`
/// (0 for a direct client); the session's delegated runs count from it.
///
/// A chat client (one naming its `channel`) sends files the model attaches,
/// so a session with tools then offers `chat_attach`.
pub(crate) async fn build_session(
    cwd: &str,
    overrides: ConfigOverrides,
    delegation_depth: u32,
    registry: &Arc<DelegationRegistry>,
    client: SessionClient,
) -> Result<(Session, Option<mpsc::UnboundedReceiver<()>>)> {
    let id = Uuid::new_v4().to_string();
    let workspace = std::fs::canonicalize(cwd).with_context(|| format!("resolve cwd {cwd}"))?;
    if !workspace.is_dir() {
        return Err(anyhow!("cwd is not a directory"));
    }
    let no_tools = overrides.no_tools;
    let config = Config::load(&workspace, overrides)?;
    if !no_tools {
        config.prepare_adapter_homes()?;
    }
    let provider_config = config.provider.clone();
    let api_key = provider_config.api_key.clone().or_else(|| {
        provider_config.api_key_env.as_deref().and_then(|name| std::env::var(name).ok()).map(scv_client::Secret::from)
    }).filter(|key| !key.trim().is_empty()).ok_or_else(|| anyhow!("provider credential is not configured; set provider.api_key or provider.api_key_env"))?;
    let skills = discover_skills(&workspace, &config, !no_tools)?;
    let listings = SkillListings {
        listing: skills.listing,
        project_listing: skills.project_listing,
    };
    let mut provider = OpenAiProvider::new(
        provider_config.model.clone(),
        provider_config.base_url.clone(),
        api_key.into_inner(),
        Duration::from_secs(provider_config.timeout_seconds),
        config.provider_limits(),
        provider_config
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.expose().to_owned()))
            .collect(),
    )?
    .with_image_input(provider_config.image_input);
    if !no_tools && config.hosted_web_search() {
        provider = provider.with_web_search();
    }
    let provider = Arc::new(provider);
    let (background, finished) = if !no_tools && config.agent.max_background > 0 {
        let (finished_tx, finished_rx) = mpsc::unbounded_channel();
        let unattended: Arc<dyn ApprovalGate> = Arc::new(UnattendedGate {
            policy: config.tools.approval_policy,
            client_approves_all: client.auto_approve,
        });
        (
            Some(Arc::new(
                BackgroundJobs::new(config.agent.max_background, Some(finished_tx))
                    .with_approvals(unattended),
            )),
            Some(finished_rx),
        )
    } else {
        (None, None)
    };
    let tools = if no_tools {
        Arc::new(ToolRegistry::default())
    } else {
        let mut tools = config.tools();
        tools.delegation = Some(DelegationContext {
            registry: Arc::clone(registry),
            session: id.clone(),
            depth: delegation_depth,
        });
        tools.background = background.clone();
        if client.channel.is_some() {
            tools.chat_attach = chat_attach_config();
        }
        let mut registry = builtin_registry(
            tools,
            skills.map,
            skills.roots,
            config.skills.max_skill_bytes,
            offered_adapters(&config),
        )?;
        if let Some(web) = config.web_tools() {
            scv_tools::web::register(&mut registry, web)?;
        }
        Arc::new(registry)
    };
    let agents = agent_tool_names(&tools);
    let system_prompt = build_system_prompt(
        &workspace,
        &config,
        &listings,
        &PromptContext {
            agents: &agents,
            background: tools.get("agent_status").is_some(),
            channel: client.channel.as_deref(),
        },
    )?;
    let context = Arc::new(BudgetContextPolicy::new((&config.context).into())?);
    let runtime = Arc::new(AgentRuntime::new(
        provider,
        tools,
        context,
        config.core_agent(system_prompt),
        workspace.clone(),
    ));
    Ok((
        Session {
            id,
            workspace,
            config,
            runtime,
            history: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicU64::new(0)),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            background,
            tools: !no_tools,
        },
        finished,
    ))
}

/// `chat_attach` for a chat session: it copies checked files into the
/// channels' media outbox, and refuses the SCV instance itself except its
/// media, secret locations in the user's home, and host secrets.
pub(crate) fn chat_attach_config() -> Option<scv_tools::chat_attach::ChatAttachConfig> {
    let scv_home = config::user_home_path()?;
    let media = scv_client::Layout::new(&scv_home).media();
    Some(scv_tools::chat_attach::ChatAttachConfig::standard(
        dirs::home_dir().as_deref(),
        &scv_home,
        scv_channels::media::outbox(&media),
        vec![media],
        scv_channels::media::MAX_REPLY_FILE_BYTES,
    ))
}

#[cfg(test)]
mod tests;
