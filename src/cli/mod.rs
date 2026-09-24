//! The `scv` subcommands, one module per command group. `args` holds the
//! clap definitions; `run` dispatches a parsed command line to its handler.

pub(crate) mod agents;
pub(crate) mod args;
pub(crate) mod channels;
pub(crate) mod common;
pub(crate) mod config;
pub(crate) mod daemon;
pub(crate) mod service;
pub(crate) mod status;
pub(crate) mod update;

use anyhow::Result;
use clap::ValueEnum as _;
use scv_protocol::{DaemonCommand, DaemonStatus};
use scv_server::ConfigOverrides;
use scv_tui::LaunchOptions;
use std::path::PathBuf;

use args::{Cli, Command};

/// Run one parsed `scv` command line. `main` has already selected the
/// instance (`SCV_HOME`/`SCV_CONFIG`) before starting the runtime.
pub(crate) async fn run(cli: Cli, cwd: PathBuf) -> Result<()> {
    let launch = LaunchOptions {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli
            .approval_policy
            .map(|value| value.to_possible_value().unwrap().get_name().to_owned()),
    };
    let overrides = ConfigOverrides {
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli.approval_policy.map(Into::into),
        no_tools: false,
    };
    let command = cli.command.unwrap_or(Command::Tui);
    daemon::refuse_nested_daemon_control(&command)?;
    match command {
        Command::Config { command } => config::run(command, &cwd, &overrides),
        Command::Tui => scv_tui::run_tui(&cwd, launch).await,
        Command::Exec { prompt, yes } => scv_tui::run_exec(&cwd, prompt, yes, launch).await,
        Command::Server { stdio: _ } => {
            daemon::init_tracing();
            scv_server::run_stdio(overrides).await
        }
        Command::Run { workspace } => {
            // Daemon diagnostics (channel poll and delivery failures) go to
            // stderr, which the user service sends to the journal.
            daemon::init_tracing();
            daemon::run_daemon(&workspace, overrides).await
        }
        Command::Start {
            workspace,
            allow_sudo,
        } => service::daemon_control(
            "start",
            Some(&workspace),
            cli.approval_policy,
            cli.provider.as_deref(),
            cli.model.as_deref(),
            cli.base_url.as_deref(),
            allow_sudo,
        ),
        Command::Stop => service::daemon_control("stop", None, None, None, None, None, false),
        Command::Restart {
            when_idle: true,
            version,
            commit,
            max_wait,
            ..
        } => daemon::restart_when_idle(version, commit, max_wait).await,
        Command::Restart {
            workspace,
            allow_sudo,
            ..
        } => service::daemon_control(
            "restart",
            Some(&workspace),
            cli.approval_policy,
            cli.provider.as_deref(),
            cli.model.as_deref(),
            cli.base_url.as_deref(),
            allow_sudo,
        ),
        Command::Status => status::show_status(None, None).await,
        Command::Reload => {
            control(DaemonCommand::Reload).await?;
            println!("Component configuration reloaded.");
            Ok(())
        }
        Command::Update { index_url } => update::update_cli(&cwd, index_url),
        Command::Channels { command } => channels::channels(command).await,
        Command::Agents { command } => agents::agents(command).await,
        Command::BuildInfo => {
            println!("{}", serde_json::to_string(&scv_server::build_info())?);
            Ok(())
        }
        Command::RestartWatchdog { plan } => {
            daemon::init_tracing();
            scv_server::restart_watchdog(&plan).await
        }
    }
}

/// Send one control command to this instance's running daemon.
pub(crate) async fn control(command: DaemonCommand) -> Result<DaemonStatus> {
    scv_client::control(&scv_client::default_socket_path()?, command).await
}
