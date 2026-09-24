//! The SCV server: the authority over sessions, policy, and approvals, served
//! over the daemon's Unix socket ([`run_socket`]) or one stdio connection
//! ([`run_stdio`]).
//!
//! Each connection gets its own session with an ordered turn queue; turns run
//! `scv_core::AgentRuntime` with the configured provider and the tools
//! `scv_tools` offers. The daemon also supervises long-running components
//! ([`components`]), such as chat channel accounts, and plans restarts into a
//! newly installed release. It also holds the helpers `scv agents` and
//! `scv config` call.

mod agents;
mod approval;
mod attachments;
pub mod components;
mod config;
mod connection;
mod control;
mod daemon;
mod events;
pub mod imports;
mod outbound;
pub mod overview;
mod prompt;
mod restart;
mod session;
#[cfg(test)]
mod test_support;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use config::Config;
pub use config::{ApprovalPolicy, ConfigOverrides, user_home_path};
use sha2::{Digest, Sha256};
pub fn init_user_config() -> anyhow::Result<std::path::PathBuf> {
    config::Config::init_user_config()
}
pub fn update_index_url(workspace: &std::path::Path) -> anyhow::Result<Option<String>> {
    Ok(config::Config::load(workspace, ConfigOverrides::default())?
        .update
        .index_url)
}

pub use agents::{Endpoint, PI_PROVIDER, StoredStatus, WireApi, read_secret};
pub use daemon::{default_socket_path, run_socket, run_stdio};
pub use restart::{BuildInfo, CONFIG_LAYOUT, build_info, watchdog as restart_watchdog};
pub use scv_tools::{adapters, delegation};

/// Build a command for a native agent's CLI with the same private home and
/// cleaned environment the daemon's `agent_<name>` tool uses, so the agent's
/// own sign-in stores credentials where delegated runs will find them.
/// Project configuration cannot set `[agents]`, so none is read.
pub fn agent_command(agent: &str) -> Result<std::process::Command> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let adapter = config
        .adapters()
        .remove(&format!("agent_{agent}"))
        .ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    let executable =
        scv_tools::adapters::resolve_agent_executable(&adapter.command, &adapter.search_dirs)
            .ok_or_else(|| {
                anyhow!(
                    "{agent} is not installed: {:?} was not found on PATH or in ~/.local/bin",
                    adapter.command
                )
            })?;
    let mut command = std::process::Command::new(executable);
    command.current_dir(config.layout().agent_home(agent));
    scv_tools::apply_agent_environment(&mut command, &adapter.environment);
    Ok(command)
}

/// Where `agent`'s executable resolves, as the daemon would find it.
pub fn agent_executable(agent: &str) -> Result<Option<PathBuf>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let adapter = config
        .adapters()
        .remove(&format!("agent_{agent}"))
        .ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    Ok(scv_tools::adapters::resolve_agent_executable(
        &adapter.command,
        &adapter.search_dirs,
    ))
}

/// The prepared private agent home for `agent`.
pub fn agent_home(agent: &str) -> Result<PathBuf> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let home = config.layout().agent_home(agent);
    if !home.is_dir() {
        return Err(anyhow!("unknown agent {agent}"));
    }
    Ok(home)
}

/// Remove delegated-conversation transcripts older than `older_than` from
/// the agent homes (`agent`, or every agent that keeps them), keeping any a
/// live conversation still uses. Returns each agent's report.
pub fn collect_agent_garbage(
    agent: Option<&str>,
    older_than: std::time::Duration,
    dry_run: bool,
) -> Result<Vec<(&'static str, scv_tools::conversation::GcReport)>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let markers = config.layout().conversations();
    let mut reports = Vec::new();
    for adapter in adapters::ADAPTERS {
        if agent.is_some_and(|agent| agent != adapter.name) {
            continue;
        }
        let Some(files) = adapter.conversation_files else {
            continue;
        };
        let home = config.layout().agent_home(adapter.name);
        if !home.is_dir() {
            continue;
        }
        let report =
            scv_tools::conversation::collect_garbage(&home, files, &markers, older_than, dry_run)
                .with_context(|| format!("clean {} transcripts", adapter.name))?;
        reports.push((adapter.name, report));
    }
    Ok(reports)
}

/// Parse a `scv agents gc --older-than` age such as `30d`.
pub fn conversation_age(value: &str) -> std::result::Result<std::time::Duration, String> {
    scv_tools::conversation::parse_age(value)
}

fn key_store_home(agent: &str) -> Result<PathBuf> {
    scv_tools::adapters::adapter(agent).ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    agent_home(agent)
}

/// Store `key` in the agent's native credential file inside its agent home.
pub fn store_agent_key(agent: &str, store: adapters::KeyStore, key: &str) -> Result<Vec<String>> {
    agents::store_key(store, &key_store_home(agent)?, key)
}

/// Whether the agent's stored credentials exist, with display lines that
/// never contain secrets.
pub fn agent_stored_status(agent: &str, store: adapters::KeyStore) -> Result<StoredStatus> {
    agents::stored_status(store, &key_store_home(agent)?)
}

/// Remove the agent's stored credentials from its agent home.
pub fn remove_agent_credentials(agent: &str, store: adapters::KeyStore) -> Result<Vec<String>> {
    agents::remove_stored(store, &key_store_home(agent)?)
}

/// Point SCV's pi at an OpenAI-compatible endpoint.
pub fn configure_pi_endpoint(endpoint: &Endpoint, key: &str) -> Result<Vec<String>> {
    agents::configure_pi_endpoint(&pi_agent_dir()?, endpoint, key)
}

/// Point SCV's pi at SCV's own active provider: its base URL, model, and key
/// (read from `api_key`, or from the `api_key_env` variable now, since
/// delegated agents never inherit key variables).
pub fn import_pi_from_scv_provider() -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let provider = &config.provider;
    let key = scv_provider_key(provider)?;
    let endpoint = Endpoint {
        base_url: provider.base_url.clone(),
        api: WireApi::Responses,
        model: provider.model.clone(),
    };
    let mut notes = agents::configure_pi_endpoint(&pi_agent_dir()?, &endpoint, &key)?;
    imports::record(
        &config.layout(),
        "pi",
        imports::Source::ScvProvider,
        provider_digest("pi", &config, &key)?,
    )?;
    if !provider.headers.is_empty() {
        notes.push(
            "Note: SCV's provider sends extra headers, which were not copied; add them to \
             pi's models.json if the endpoint needs them"
                .into(),
        );
    }
    Ok(notes)
}

/// The key of SCV's own provider: `api_key`, or the `api_key_env` variable
/// read now, since delegated agents never inherit key variables.
fn scv_provider_key(provider: &config::ProviderConfig) -> Result<String> {
    if provider.kind != "openai-compatible" {
        return Err(anyhow!("SCV's provider is not openai-compatible"));
    }
    match (&provider.api_key, &provider.api_key_env) {
        (Some(key), _) if !key.trim().is_empty() => Ok(key.trim().to_owned()),
        (_, Some(variable)) => std::env::var(variable)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("SCV's provider reads its key from ${variable}, which is not set here")
            }),
        _ => Err(anyhow!("SCV's provider has no API key configured")),
    }
}

/// Digest of what a provider import copies to `agent`: SCV's provider
/// settings and key, as each agent receives them.
fn provider_digest(agent: &str, config: &Config, key: &str) -> Result<String> {
    let provider = &config.provider;
    let mut headers: Vec<_> = provider.headers.iter().collect();
    headers.sort();
    match agent {
        "pi" => imports::digest_value(&(&provider.base_url, &provider.model, key)),
        _ => imports::digest_value(&(
            &provider.base_url,
            &provider.model,
            key,
            &provider.wire_api,
            provider.timeout_seconds,
            headers,
            config.hosted_web_search(),
        )),
    }
}

/// How `agent`'s imported copy compares with its source now, as one display
/// line without secrets; `None` when nothing was imported.
pub fn agent_import_status(agent: &str) -> Result<Option<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    let status = imports::check(&config.layout(), agent, || {
        let key = scv_provider_key(&config.provider).ok()?;
        provider_digest(agent, &config, &key).ok()
    })?;
    Ok(status.map(|status| status.describe(agent, imports::now())))
}

/// Give the nested SCV (`agent_scv`) its own copy of SCV's active provider,
/// in `$SCV_HOME/agents/scv/config.toml` (mode 0600).
pub fn import_scv_from_scv_provider() -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let provider = &config.provider;
    let key = scv_provider_key(provider)?;
    let digest = provider_digest("scv", &config, &key)?;
    let notes = agents::configure_scv_child(
        &agent_home("scv")?,
        &agents::ScvChildProvider {
            wire_api: &provider.wire_api,
            model: &provider.model,
            base_url: &provider.base_url,
            timeout_seconds: provider.timeout_seconds,
            headers: &provider.headers,
            hosted_web_search: config.hosted_web_search(),
        },
        &key,
    )?;
    imports::record(
        &config.layout(),
        "scv",
        imports::Source::ScvProvider,
        digest,
    )?;
    Ok(notes)
}

fn pi_agent_dir() -> Result<PathBuf> {
    let descriptor =
        scv_tools::adapters::adapter("pi").ok_or_else(|| anyhow!("unknown agent pi"))?;
    let scv_tools::adapters::Status::Stored(scv_tools::adapters::KeyStore::Pi { dir }) =
        descriptor.status
    else {
        return Err(anyhow!("pi has no SCV-managed store"));
    };
    Ok(agent_home("pi")?.join(dir))
}

/// Copy the user's own Codex setup from `source` into SCV's private Codex
/// agent home: `config.toml`, and `auth.json` only when it holds an API key.
/// Returns display lines that never contain secret values.
pub fn import_codex(source: &Path) -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let layout = config.layout();
    let notes = agents::import_codex(source, &layout.agent_home("codex"))?;
    record_file_import(&layout, "codex", source, agents::codex_copied_files(source))?;
    Ok(notes)
}

/// Remember which files an import copied from `source`, so a later change
/// there shows up as a stale copy.
fn record_file_import(
    layout: &scv_client::Layout,
    agent: &str,
    source: &Path,
    files: Vec<String>,
) -> Result<()> {
    let dir = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_owned());
    let digest = imports::digest_files(&dir, &files)?;
    imports::record(layout, agent, imports::Source::Files { dir, files }, digest)
}

/// Copy the user's own Grok `config.toml` from `source` (a Grok home) into
/// SCV's private Grok home, keeping settings only SCV's copy has. Returns
/// display lines that never contain secret values.
pub fn import_grok(source: &Path) -> Result<Vec<String>> {
    let config = Config::load_user(ConfigOverrides::default())?;
    config.prepare_adapter_homes()?;
    let descriptor =
        scv_tools::adapters::adapter("grok").ok_or_else(|| anyhow!("unknown agent grok"))?;
    let grok_home = descriptor
        .home_environment
        .iter()
        .find(|(variable, _)| *variable == "GROK_HOME")
        .map(|(_, relative)| *relative)
        .ok_or_else(|| anyhow!("grok has no GROK_HOME in its agent home"))?;
    let layout = config.layout();
    let notes = agents::import_grok(source, &layout.agent_home("grok").join(grok_home))?;
    record_file_import(&layout, "grok", source, vec!["config.toml".into()])?;
    Ok(notes)
}

/// Return the user service name for the selected SCV instance.
pub fn service_name() -> anyhow::Result<String> {
    if std::env::var_os("SCV_HOME").is_none() {
        return Ok("scv.service".into());
    }
    let home = user_home_path().ok_or_else(|| anyhow!("cannot determine SCV instance home"))?;
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("scv-{suffix}.service"))
}

pub fn service_unit_path() -> anyhow::Result<std::path::PathBuf> {
    let config =
        dirs::config_dir().ok_or_else(|| anyhow!("cannot determine XDG config directory"))?;
    Ok(config.join("systemd/user").join(service_name()?))
}

#[cfg(test)]
mod tests;
