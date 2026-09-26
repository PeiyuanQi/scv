//! What `scv agents` does inside SCV's private agent homes: running an
//! agent's own CLI there, storing keys and endpoints in its native files,
//! collecting old transcripts, and importing setups. Each function takes the
//! user configuration (`Config::load_user`), since project configuration
//! cannot set `[agents]`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use scv_client::Layout;
use scv_server::config::{Config, ProviderConfig};
use scv_tools::adapters::{self, KeyStore};
use scv_tools::conversation::GcReport;
use scv_tools::stores::{self, Endpoint, StoredStatus, WireApi};

use super::imports;

/// Build a command for a native agent's CLI with the same private home and
/// cleaned environment the daemon's delegated runs of that agent use, so
/// the agent's own sign-in stores credentials where they will find them.
pub(crate) fn agent_command(config: &Config, agent: &str) -> Result<std::process::Command> {
    config.prepare_adapter_homes()?;
    let adapter = config
        .adapters()
        .remove(agent)
        .ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    let executable = adapters::resolve_agent_executable(&adapter.command, &adapter.search_dirs)
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
pub(crate) fn agent_executable(config: &Config, agent: &str) -> Result<Option<PathBuf>> {
    let adapter = config
        .adapters()
        .remove(agent)
        .ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    Ok(adapters::resolve_agent_executable(
        &adapter.command,
        &adapter.search_dirs,
    ))
}

/// The prepared private agent home for `agent`.
fn agent_home(config: &Config, agent: &str) -> Result<PathBuf> {
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
pub(crate) fn collect_agent_garbage(
    config: &Config,
    agent: Option<&str>,
    older_than: Duration,
    dry_run: bool,
) -> Result<Vec<(&'static str, GcReport)>> {
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

fn key_store_home(config: &Config, agent: &str) -> Result<PathBuf> {
    adapters::adapter(agent).ok_or_else(|| anyhow!("unknown agent {agent}"))?;
    agent_home(config, agent)
}

/// Store `key` in the agent's native credential file inside its agent home.
pub(crate) fn store_agent_key(
    config: &Config,
    agent: &str,
    store: KeyStore,
    key: &str,
) -> Result<Vec<String>> {
    stores::store_key(store, &key_store_home(config, agent)?, key)
}

/// Whether the agent's stored credentials exist, with display lines that
/// never contain secrets.
pub(crate) fn agent_stored_status(
    config: &Config,
    agent: &str,
    store: KeyStore,
) -> Result<StoredStatus> {
    stores::stored_status(store, &key_store_home(config, agent)?)
}

/// Remove the agent's stored credentials from its agent home.
pub(crate) fn remove_agent_credentials(
    config: &Config,
    agent: &str,
    store: KeyStore,
) -> Result<Vec<String>> {
    stores::remove_stored(store, &key_store_home(config, agent)?)
}

/// Point SCV's pi at an OpenAI-compatible endpoint.
pub(crate) fn configure_pi_endpoint(
    config: &Config,
    endpoint: &Endpoint,
    key: &str,
) -> Result<Vec<String>> {
    stores::configure_pi_endpoint(&pi_agent_dir(config)?, endpoint, key)
}

/// Point SCV's pi at SCV's own active provider: its base URL, model, and key
/// (read from `api_key`, or from the `api_key_env` variable now, since
/// delegated agents never inherit key variables).
pub(crate) fn import_pi_from_scv_provider(config: &Config) -> Result<Vec<String>> {
    let provider = &config.provider;
    let key = scv_provider_key(provider)?;
    let endpoint = Endpoint {
        base_url: provider.base_url.clone(),
        api: WireApi::Responses,
        model: provider.model.clone(),
    };
    let mut notes = stores::configure_pi_endpoint(&pi_agent_dir(config)?, &endpoint, &key)?;
    imports::record(
        &config.layout(),
        "pi",
        imports::Source::ScvProvider,
        provider_digest("pi", config, &key)?,
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
fn scv_provider_key(provider: &ProviderConfig) -> Result<String> {
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
pub(crate) fn agent_import_status(config: &Config, agent: &str) -> Result<Option<String>> {
    let status = imports::check(&config.layout(), agent, || {
        let key = scv_provider_key(&config.provider).ok()?;
        provider_digest(agent, config, &key).ok()
    })?;
    Ok(status.map(|status| status.describe(agent, imports::now())))
}

/// Give the nested SCV (the `scv` agent) its own copy of SCV's active provider,
/// in `$SCV_HOME/agents/scv/config.toml` (mode 0600).
pub(crate) fn import_scv_from_scv_provider(config: &Config) -> Result<Vec<String>> {
    config.prepare_adapter_homes()?;
    let provider = &config.provider;
    let key = scv_provider_key(provider)?;
    let digest = provider_digest("scv", config, &key)?;
    let notes = stores::configure_scv_child(
        &agent_home(config, "scv")?,
        &stores::ScvChildProvider {
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

fn pi_agent_dir(config: &Config) -> Result<PathBuf> {
    let descriptor = adapters::adapter("pi").ok_or_else(|| anyhow!("unknown agent pi"))?;
    let adapters::Status::Stored(KeyStore::Pi { dir }) = descriptor.status else {
        return Err(anyhow!("pi has no SCV-managed store"));
    };
    Ok(agent_home(config, "pi")?.join(dir))
}

/// Copy the user's own Codex setup from `source` into SCV's private Codex
/// agent home: `config.toml`, and `auth.json` only when it holds an API key.
/// Returns display lines that never contain secret values.
pub(crate) fn import_codex(config: &Config, source: &Path) -> Result<Vec<String>> {
    config.prepare_adapter_homes()?;
    let layout = config.layout();
    let notes = stores::import_codex(source, &layout.agent_home("codex"))?;
    record_file_import(&layout, "codex", source, stores::codex_copied_files(source))?;
    Ok(notes)
}

/// Remember which files an import copied from `source`, so a later change
/// there shows up as a stale copy.
fn record_file_import(
    layout: &Layout,
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
pub(crate) fn import_grok(config: &Config, source: &Path) -> Result<Vec<String>> {
    config.prepare_adapter_homes()?;
    let descriptor = adapters::adapter("grok").ok_or_else(|| anyhow!("unknown agent grok"))?;
    let grok_home = descriptor
        .home_environment
        .iter()
        .find(|(variable, _)| *variable == "GROK_HOME")
        .map(|(_, relative)| *relative)
        .ok_or_else(|| anyhow!("grok has no GROK_HOME in its agent home"))?;
    let layout = config.layout();
    let notes = stores::import_grok(source, &layout.agent_home("grok").join(grok_home))?;
    record_file_import(&layout, "grok", source, vec!["config.toml".into()])?;
    Ok(notes)
}
