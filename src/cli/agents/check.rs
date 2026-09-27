//! `scv agents check`: call each installed agent the way SCV's `agent` tool
//! does, and report its version, how SCV reaches it, the models and efforts
//! it offers, and whether a short call with the user's configured model and
//! effort works.

use std::{
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use scv_server::config::Config;
use scv_tools::{
    DelegationContext, Reach, adapters, agent_options, delegation::DelegationRegistry,
};
use serde_json::json;
use tokio::sync::watch;

use super::setup;

/// What each agent is asked: one short reply, no tools.
const PROMPT: &str = "SCV is checking that it can reach you. Reply with exactly: ok";
/// How long `<cli> --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest excerpt of a reply, error, or version printed.
const EXCERPT_CHARS: usize = 300;

pub(crate) async fn check(
    config: &Config,
    agent: Option<&str>,
    timeout_seconds: u64,
) -> Result<()> {
    config.prepare_adapter_homes()?;
    let mut configured = config.adapters();
    let cwd = std::env::current_dir()?;
    let names: Vec<&str> = match agent {
        Some(name) => vec![name],
        None => adapters::ADAPTERS
            .iter()
            .map(|adapter| adapter.name)
            .collect(),
    };
    // One watcher for the whole check, so a Ctrl-C between calls counts too.
    let (interrupt, interrupted) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = interrupt.send(true);
        }
    });
    let stopped = |mut interrupted: watch::Receiver<bool>| async move {
        if interrupted.wait_for(|stopped| *stopped).await.is_err() {
            // Without a signal handler, only the timeout ends the call.
            std::future::pending::<()>().await;
        }
    };
    // Runs are recorded like a session's, so `scv agents ps` lists them and
    // the daemon stops any this process leaves behind.
    let delegation = DelegationContext {
        registry: Arc::new(DelegationRegistry::new(&config.layout())),
        session: format!("agents-check-{}", std::process::id()),
        depth: 0,
    };
    let mut failed = Vec::new();
    let mut checked = 0;
    for name in names {
        if *interrupted.borrow() {
            bail!("interrupted");
        }
        let Some(adapter) = configured.remove(name) else {
            continue;
        };
        let product = adapters::adapter(name).map_or(name, |descriptor| descriptor.product);
        println!("{name} ({product})");
        let reached = scv_tools::reach(&adapter);
        match &reached {
            Reach::Missing(command) => {
                println!("  not installed: {command:?} is not on PATH or in ~/.local/bin");
                if agent.is_some() {
                    failed.push(name);
                }
                continue;
            }
            Reach::Acp(server) => println!("  reached   over ACP: {}", server.display()),
            Reach::Cli(cli) => println!(
                "  reached   through its CLI, once per turn: {}",
                cli.display()
            ),
            Reach::Scv(scv) => println!("  reached   as a nested SCV: {}", scv.display()),
        }
        let version = tokio::select! {
            version = version(config, name) => version,
            () = stopped(interrupted.clone()) => bail!("interrupted"),
        };
        println!("  version   {version}");
        checked += 1;

        let mut arguments =
            json!({"agent": name, "prompt": PROMPT, "timeout_seconds": timeout_seconds});
        let mut with = Vec::new();
        if let Some(model) = &adapter.model {
            arguments["model"] = model.clone().into();
            with.push(format!("model {model}"));
        }
        if let Some(effort) = &adapter.effort {
            arguments["effort"] = effort.clone().into();
            with.push(format!("effort {effort}"));
        }
        let with = if with.is_empty() {
            " with its own default model".to_owned()
        } else {
            format!(" with your configured {}", with.join(" and "))
        };
        let options_file = adapter.options_file.clone();
        let model_hint = adapter.model_hint.clone();
        let started = Instant::now();
        let result = scv_tools::call_agent(
            name,
            adapter,
            &cwd,
            arguments,
            Duration::from_secs(timeout_seconds),
            delegation.clone(),
            stopped(interrupted.clone()),
        )
        .await;
        let elapsed = started.elapsed().as_secs_f64();
        if *interrupted.borrow() {
            println!("  call      interrupted after {elapsed:.1}s; its agent was stopped");
            bail!("interrupted");
        }

        // The call's ACP session saved what the agent offers now.
        match (&reached, options_file) {
            (Reach::Acp(_), Some(file)) => print_offered(&file, name),
            (Reach::Cli(_), _) => println!(
                "  models    passed to its CLI as given; it lists none ({})",
                model_hint.trim_end_matches('.')
            ),
            _ => {}
        }
        match result {
            Ok(value) if value["status"] == "completed" => {
                let reply = value["reply"].as_str().unwrap_or_default();
                println!(
                    "  call      ok in {elapsed:.1}s{with}: {:?}",
                    excerpt(reply)
                );
            }
            Ok(value) => {
                failed.push(name);
                let status = value["status"].as_str().unwrap_or("failed");
                let detail = value["error"]
                    .as_str()
                    .filter(|error| !error.is_empty())
                    .or_else(|| value["reply"].as_str())
                    .unwrap_or_default();
                println!(
                    "  call      {status} after {elapsed:.1}s{with}: {}",
                    excerpt(detail)
                );
                if let Some(hint) = value["hint"].as_str() {
                    println!("            {}", excerpt(hint));
                }
            }
            Err(message) => {
                failed.push(name);
                println!("  call      refused{with}: {}", excerpt(&message));
            }
        }
    }
    if agent.is_none() && checked == 0 {
        println!("No agent is installed. See `scv agents login --help`.");
    }
    if !failed.is_empty() {
        bail!("check failed for: {}", failed.join(", "));
    }
    Ok(())
}

/// The model and effort values saved for `agent`, as its last ACP session
/// listed them.
fn print_offered(file: &Path, agent: &str) {
    let Some((offered, seen)) = agent_options::read_saved(file, agent) else {
        println!("  models    not known yet: no ACP session has listed them");
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let age = now.saturating_sub(seen);
    let when = if age > 120 {
        format!(" (listed {} ago, by an earlier session)", ago(age))
    } else {
        String::new()
    };
    for (label, choice) in [("models", &offered.model), ("efforts", &offered.effort)] {
        let Some(choice) = choice else {
            println!("  {label:<9} none to choose from");
            continue;
        };
        let default = choice
            .named_default()
            .map(|value| format!("; its default is {value}"))
            .unwrap_or_default();
        println!("  {label:<9} {}{default}{when}", choice.shown().join(", "));
    }
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{} minutes", seconds / 60),
        3600..172_800 => format!("{} hours", seconds / 3600),
        _ => format!("{} days", seconds / 86400),
    }
}

/// The first line of `<agent CLI> --version`, run in the agent's home, or
/// `unknown`.
async fn version(config: &Config, name: &str) -> String {
    let Ok(command) = setup::agent_command(config, name) else {
        return "unknown".into();
    };
    let mut command = tokio::process::Command::from(command);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text.lines().map(str::trim).find(|line| !line.is_empty());
            line.map_or_else(|| "unknown".into(), excerpt)
        }
        _ => "unknown".into(),
    }
}

/// `text` on one line, without control characters, and cut to a readable
/// length: it comes from the agent.
fn excerpt(text: &str) -> String {
    let mut line: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(EXCERPT_CHARS + 1)
        .collect();
    if line.chars().count() > EXCERPT_CHARS {
        line = line.chars().take(EXCERPT_CHARS).collect();
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests;
