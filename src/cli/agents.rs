//! `scv agents`: sign the delegated agent CLIs in and out of SCV's private
//! agent homes, import setups into them, and list, collect, or stop their
//! runs.

mod imports;
pub(crate) mod setup;

use anyhow::{Context, Result, bail};
use scv_client::Layout;
use scv_protocol::DaemonCommand;
use scv_server::config::{Config, ConfigOverrides};
use scv_tools::adapters::{self, AdapterDescriptor};
use scv_tools::stores::{Endpoint, StoredStatus, WireApi};
use std::path::PathBuf;

use super::args::AgentsCommand;
use super::control;
use super::prompt::{prompt_line, read_secret};

pub(crate) async fn agents(
    layout: &Layout,
    overrides: &ConfigOverrides,
    command: AgentsCommand,
) -> Result<()> {
    use adapters::{Login, Logout, Status};
    let user_config = || user_config(layout, overrides);
    match command {
        AgentsCommand::Login {
            agent,
            openai_compatible,
            base_url,
            wire_api,
            model,
            extra,
        } => {
            let adapter = agent_descriptor(&agent)?;
            let name = adapter.name;
            if openai_compatible {
                if name != "pi" {
                    bail!(
                        "--openai-compatible configures pi; {name} signs in with `scv agents login {name}`"
                    );
                }
                let endpoint = Endpoint {
                    base_url: match base_url {
                        Some(url) => url,
                        None => prompt_line("Base URL (e.g. https://host/v1)")?,
                    },
                    api: match wire_api {
                        Some(api) => api.into(),
                        None => match prompt_line("Wire API [responses/chat] (default responses)")?
                            .as_str()
                        {
                            "" | "responses" => WireApi::Responses,
                            "chat" => WireApi::ChatCompletions,
                            other => bail!("unknown wire API {other:?}; use responses or chat"),
                        },
                    },
                    model: match model {
                        Some(model) => model,
                        None => prompt_line("Default model id")?,
                    },
                };
                let key = read_secret("API key (input hidden)")?;
                for line in setup::configure_pi_endpoint(&user_config()?, &endpoint, &key)? {
                    println!("{line}");
                }
                println!("Check with `scv agents status pi`.");
                return Ok(());
            }
            println!(
                "Signing {name} in for SCV's agent_{name} tool (separate from your own {} login).",
                adapter.product
            );
            match adapter.login {
                Login::Command(args) => run_agent(user_config, name, args, &extra, "sign-in")?,
                Login::Interactive { args, hint } => {
                    println!("Opening {} in SCV's agent home: {hint}.", adapter.product);
                    run_agent(user_config, name, args, &extra, "sign-in")?;
                }
                Login::Import => {
                    if !extra.is_empty() {
                        bail!("{name} copies SCV's own configuration and takes no arguments");
                    }
                    import_scv_child(user_config)?;
                }
                Login::ApiKey(store) => {
                    if !extra.is_empty() {
                        bail!("{name} takes its API key from a prompt or stdin, not arguments");
                    }
                    let key = read_secret(&format!("{} API key (input hidden)", adapter.product))?;
                    for line in setup::store_agent_key(&user_config()?, name, store, &key)? {
                        println!("{line}");
                    }
                }
            }
            println!(
                "Done. `scv agents status` shows the result; the daemon picks it up on the next call."
            );
            Ok(())
        }
        AgentsCommand::Status { agent } => {
            let selected: Vec<_> = match agent {
                Some(name) => vec![agent_descriptor(&name)?],
                None => adapters::ADAPTERS.iter().collect(),
            };
            for adapter in selected {
                let name = adapter.name;
                println!("{name}:");
                let config = user_config()?;
                let installed = setup::agent_executable(&config, name)?;
                if installed.is_none() {
                    println!(
                        "  not installed ({:?} is not on PATH or in ~/.local/bin)",
                        adapter.command
                    );
                }
                match adapter.status {
                    // The agent's own status names the account (an email) or
                    // part of a key, so only a summary is printed; its own
                    // advice would also sign in the wrong home.
                    Status::Command(args) if installed.is_some() => {
                        match setup::agent_command(&config, name)?
                            .args(args)
                            .stdin(std::process::Stdio::null())
                            .output()
                        {
                            Ok(output) => {
                                // Codex reports on stderr, Claude Code on stdout.
                                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                                text.push('\n');
                                text.push_str(&String::from_utf8_lossy(&output.stderr));
                                let summary = adapters::summarize_status(
                                    adapter.status_summary,
                                    output.status.success(),
                                    &text,
                                );
                                println!("  {summary}");
                                if summary == "not signed in" {
                                    println!("  Sign in for SCV with: scv agents login {name}");
                                }
                            }
                            Err(error) => println!("  unavailable: {error}"),
                        }
                    }
                    Status::Command(_) => {}
                    Status::Stored(store) => {
                        let StoredStatus { ready, lines } =
                            setup::agent_stored_status(&config, name, store)?;
                        for line in lines {
                            println!("  {line}");
                        }
                        if !ready {
                            println!("  Sign in for SCV with: scv agents login {name}");
                        }
                    }
                }
                if let Some(line) = setup::agent_import_status(&config, name)? {
                    println!("  {line}");
                }
            }
            Ok(())
        }
        AgentsCommand::Ps { all } => {
            let status = control(layout, DaemonCommand::Delegations { all }).await?;
            print_delegations(&status.delegations.entries);
            Ok(())
        }
        AgentsCommand::Gc {
            agent,
            older_than,
            dry_run,
        } => {
            let reports = setup::collect_agent_garbage(
                &user_config()?,
                agent.as_deref(),
                older_than,
                dry_run,
            )?;
            if reports.is_empty() {
                println!("No agent keeps conversation transcripts yet.");
            }
            for (agent, report) in reports {
                let verb = if dry_run { "would remove" } else { "removed" };
                println!(
                    "{agent}: {verb} {} transcript(s), {:.1} MiB; kept {} in use",
                    report.removed.len(),
                    report.bytes as f64 / (1024.0 * 1024.0),
                    report.kept_live
                );
                if dry_run {
                    for path in &report.removed {
                        println!("  {}", path.display());
                    }
                }
            }
            Ok(())
        }
        AgentsCommand::Kill { handle, orphans } => {
            let status = control(layout, DaemonCommand::DelegationKill { handle, orphans }).await?;
            if status.delegations.killed.is_empty() {
                println!("Nothing to stop.");
            } else {
                println!("Stopped: {}", status.delegations.killed.join(", "));
            }
            Ok(())
        }
        AgentsCommand::Import {
            agent,
            from,
            from_scv_provider,
        } => match agent.as_str() {
            "codex" => {
                if from_scv_provider {
                    bail!("--from-scv-provider applies to pi; codex imports your own Codex home");
                }
                let source = from
                    .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
                    })
                    .context("cannot determine your Codex home; pass --from")?;
                println!("Importing Codex setup from {}", source.display());
                for line in setup::import_codex(&user_config()?, &source)? {
                    println!("  {line}");
                }
                println!(
                    "This is a copy: re-run after changing your own Codex config. Check with `scv agents status codex`."
                );
                Ok(())
            }
            "grok" => {
                if from_scv_provider {
                    bail!("--from-scv-provider applies to pi; grok imports your own Grok home");
                }
                let source = from
                    .or_else(|| std::env::var_os("GROK_HOME").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok"))
                    })
                    .context("cannot determine your Grok home; pass --from")?;
                println!("Importing Grok setup from {}", source.display());
                for line in setup::import_grok(&user_config()?, &source)? {
                    println!("  {line}");
                }
                println!(
                    "This is a copy: re-run after changing your own Grok config. Check with `scv agents status grok`."
                );
                Ok(())
            }
            "scv" => {
                if from.is_some() {
                    bail!("scv imports SCV's own provider; it takes no --from");
                }
                import_scv_child(user_config)
            }
            "pi" => {
                if !from_scv_provider || from.is_some() {
                    bail!(
                        "pi imports SCV's own provider: `scv agents import pi --from-scv-provider`"
                    );
                }
                println!("Pointing SCV's pi at SCV's own provider");
                for line in setup::import_pi_from_scv_provider(&user_config()?)? {
                    println!("  {line}");
                }
                println!(
                    "This is a copy: re-run after changing SCV's provider. Check with `scv agents status pi`."
                );
                Ok(())
            }
            other => {
                bail!("{other} has nothing to import; sign it in with `scv agents login {other}`")
            }
        },
        AgentsCommand::Logout { agent } => {
            let adapter = agent_descriptor(&agent)?;
            match adapter.logout {
                Logout::Command(args) => {
                    run_agent(user_config, adapter.name, args, &[], "sign-out")
                }
                Logout::Stored(store) => {
                    for line in
                        setup::remove_agent_credentials(&user_config()?, adapter.name, store)?
                    {
                        println!("{line}");
                    }
                    Ok(())
                }
            }
        }
    }
}

/// Give the nested SCV behind `agent_scv` a copy of SCV's own provider.
fn import_scv_child(user_config: impl Fn() -> Result<Config>) -> Result<()> {
    println!("Giving SCV's nested SCV (agent_scv) a copy of SCV's own provider");
    for line in setup::import_scv_from_scv_provider(&user_config()?)? {
        println!("  {line}");
    }
    println!(
        "This is a copy: re-run after changing SCV's provider. Check with `scv agents status scv`."
    );
    Ok(())
}

fn agent_descriptor(name: &str) -> Result<&'static AdapterDescriptor> {
    adapters::adapter(name).with_context(|| format!("unknown agent {name}"))
}

/// The user configuration, which is all `scv agents` reads: project
/// configuration cannot set `[agents]`, and the command line's provider
/// flags do not apply.
fn user_config(layout: &Layout, overrides: &ConfigOverrides) -> Result<Config> {
    Config::load_user(
        layout,
        ConfigOverrides {
            config_file: overrides.config_file.clone(),
            ..ConfigOverrides::default()
        },
    )
}

/// Run an agent's own command inside its SCV agent home.
fn run_agent(
    user_config: impl Fn() -> Result<Config>,
    name: &str,
    args: &[&str],
    extra: &[String],
    action: &str,
) -> Result<()> {
    let status = setup::agent_command(&user_config()?, name)?
        .args(args)
        .args(extra)
        .status()
        .with_context(|| format!("run {name} {action}"))?;
    if !status.success() {
        bail!("{name} {action} did not complete");
    }
    Ok(())
}

fn print_delegations(entries: &[scv_protocol::DelegationInfo]) {
    if entries.is_empty() {
        println!("No delegated agent runs.");
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    println!(
        "{:<16} {:<7} {:<13} {:<10} {:>8} {:>5} {:>7} {:>5}  CWD",
        "HANDLE", "AGENT", "CONVERSATION", "STATE", "PID", "PROCS", "AGE", "DEPTH"
    );
    for entry in entries {
        let age = now.saturating_sub(entry.started_unix_seconds);
        let conversation = match (&entry.conversation, entry.turn) {
            (Some(handle), Some(turn)) => format!("{handle} #{turn}"),
            (Some(handle), None) => handle.clone(),
            _ => "-".into(),
        };
        // Debug formatting escapes control characters in the untrusted path.
        println!(
            "{:<16} {:<7} {:<13} {:<10} {:>8} {:>5} {:>7} {:>5}  {:?}",
            entry.handle,
            entry.agent,
            conversation,
            delegation_state(entry),
            entry.pid,
            entry.processes,
            format!("{}m{:02}s", age / 60, age % 60),
            entry.depth,
            entry.cwd,
        );
    }
}

/// A run's STATE in `scv agents ps`: a live agent between turns is idle,
/// unless background jobs of its own still run or wait to be reported.
fn delegation_state(entry: &scv_protocol::DelegationInfo) -> &'static str {
    if entry.orphaned {
        "orphaned"
    } else if entry.idle_since_unix_seconds.is_none() {
        "running"
    } else if entry.background_jobs.is_some_and(|jobs| jobs > 0) {
        "background"
    } else {
        "idle"
    }
}

#[cfg(test)]
mod tests;
