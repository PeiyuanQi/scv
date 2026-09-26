//! Running the daemon in the foreground, the restart hand-over it offers to
//! a delegated release, and the rule that delegated runs never manage it.

use anyhow::{Context, Result, bail};
use scv_protocol::DaemonCommand;
use scv_server::config::ConfigOverrides;
use std::io::{self, IsTerminal};
use std::path::Path;

use super::args::Command;
use super::control;

/// A delegated agent (or an SCV it started) must not start, stop, replace, or
/// reconfigure daemons: that is the host's decision, and an SCV run from
/// inside a delegation would otherwise manage its parent.
pub(crate) fn refuse_nested_daemon_control(command: &Command) -> Result<()> {
    let depth = scv_tools::delegation::current_depth();
    let lifecycle = match command {
        Command::Run { .. } => Some("run"),
        Command::Start { .. } => Some("start"),
        Command::Stop => Some("stop"),
        // Asking the daemon to restart itself when idle is how a delegated
        // release (the feature-flow deploy) hands over; the daemon decides.
        Command::Restart {
            when_idle: true, ..
        } => None,
        Command::Restart { .. } => Some("restart"),
        Command::RestartWatchdog { .. } => Some("restart-watchdog"),
        Command::Update { .. } => Some("update"),
        Command::Channels { .. } => Some("channels"),
        _ => None,
    };
    if depth > 0
        && let Some(action) = lifecycle
    {
        bail!(
            "`scv {action}` is refused inside a delegated agent run (delegation depth {depth}); \
             the host owner manages the daemon"
        );
    }
    Ok(())
}

/// Exit status of `scv restart --when-idle` when the daemon is not running
/// or predates it; the caller then restarts the unit itself.
const RESTART_UNSUPPORTED: i32 = 3;

pub(crate) async fn restart_when_idle(
    version: Option<String>,
    commit: Option<String>,
    max_wait: Option<u64>,
) -> Result<()> {
    let parent = std::env::var(scv_tools::delegation::PARENT_VARIABLE)
        .ok()
        .filter(|chain| !chain.trim().is_empty());
    let status = match control(DaemonCommand::RestartWhenIdle {
        version,
        commit,
        parent,
        max_wait_seconds: max_wait,
    })
    .await
    {
        Ok(status) => status,
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("unknown variant") || message.contains("SCV daemon unavailable") {
                eprintln!("{message}");
                eprintln!(
                    "The daemon is not running or cannot schedule its own restart; restart its unit instead."
                );
                std::process::exit(RESTART_UNSUPPORTED);
            }
            return Err(error);
        }
    };
    let info = status
        .restart
        .context("the daemon did not report the scheduled restart")?;
    println!("Restart into v{} scheduled.", info.to_version);
    println!("{}", describe_restart(&info));
    Ok(())
}

/// One line on a scheduled restart: what it waits for and until when.
pub(crate) fn describe_restart(info: &scv_protocol::RestartInfo) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let left = info.deadline_unix_seconds.saturating_sub(now);
    let waiting = match &info.waiting_for {
        Some(what) => format!(
            "waiting for {what}, restarting anyway in {}m{:02}s",
            left / 60,
            left % 60
        ),
        None => "restarting now".into(),
    };
    let target = match &info.origin {
        Some(origin) => format!("the chat on {origin} that asked"),
        None => "the [notify] accounts".into(),
    };
    format!(
        "Restart into v{}: {waiting}; the outcome goes to {target}.",
        info.to_version
    )
}

pub(crate) async fn run_daemon(workspace: &Path, overrides: ConfigOverrides) -> Result<()> {
    let socket = scv_client::default_socket_path()?;
    std::env::set_current_dir(workspace)
        .with_context(|| format!("change to daemon workspace {}", workspace.display()))?;
    scv_server::run_socket(&socket, overrides).await
}

pub(crate) fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // Colour only for a terminal: the user service sends stderr to the
    // journal, where escape codes would hide `WARN`/`ERROR` from searches.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .try_init();
}
