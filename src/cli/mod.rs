//! The `scv` subcommands, one module per command group. `args` holds the
//! clap definitions; `run` dispatches a parsed command line to its handler.

pub(crate) mod agents;
pub(crate) mod args;
pub(crate) mod channels;
pub(crate) mod common;
pub(crate) mod config;
pub(crate) mod confirm;
pub(crate) mod daemon;
pub(crate) mod prompt;
pub(crate) mod service;
pub(crate) mod status;
pub(crate) mod update;

use anyhow::Result;
use clap::ValueEnum as _;
use scv_client::Layout;
use scv_protocol::{DaemonCommand, DaemonStatus};
use scv_server::config::ConfigOverrides;
use scv_tui::LaunchOptions;
use std::path::PathBuf;

use args::{Cli, Command};

/// Run one parsed `scv` command line for the instance at `layout`, which
/// `main` selected (and exported as `SCV_HOME`/`SCV_CONFIG` for child
/// processes) before starting the runtime.
pub(crate) async fn run(cli: Cli, cwd: PathBuf, layout: Layout) -> Result<()> {
    let launch = LaunchOptions {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli.approval_policy.map(|value| {
            value
                .to_possible_value()
                .expect("every ApprovalArg has a value name")
                .get_name()
                .to_owned()
        }),
    };
    let overrides = ConfigOverrides {
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli.approval_policy.map(Into::into),
        no_tools: false,
        config_file: cli
            .config_path
            .as_deref()
            .map(|path| common::absolute_path(path, &cwd)),
    };
    let flags = service::Flags {
        approval_policy: cli.approval_policy,
        provider: cli.provider.as_deref(),
        model: cli.model.as_deref(),
        base_url: cli.base_url.as_deref(),
    };
    let command = cli.command.unwrap_or(Command::Tui);
    daemon::refuse_nested_daemon_control(&command)?;
    match command {
        Command::Config { command } => config::run(command, &layout, &cwd, &overrides),
        Command::Tui => scv_tui::run_tui(&layout.socket(), &cwd, launch).await,
        Command::Exec { prompt, yes } => scv_tui::run_exec(&cwd, prompt, yes, launch).await,
        Command::Server { stdio: _ } => {
            daemon::init_tracing();
            scv_server::run_stdio(&layout, overrides).await
        }
        Command::Run { workspace } => {
            // Daemon diagnostics (channel poll and delivery failures) go to
            // stderr, which the user service sends to the journal.
            daemon::init_tracing();
            daemon::run_daemon(&layout, &workspace, overrides).await
        }
        Command::Start {
            workspace,
            allow_sudo,
        } => service::daemon_control(
            &layout,
            &overrides,
            "start",
            Some(&workspace),
            &flags,
            allow_sudo,
        ),
        Command::Stop => service::daemon_control(&layout, &overrides, "stop", None, &flags, false),
        Command::Restart {
            when_idle: true,
            version,
            commit,
            max_wait,
            ..
        } => daemon::restart_when_idle(&layout, version, commit, max_wait).await,
        Command::Restart {
            workspace,
            allow_sudo,
            ..
        } => service::daemon_control(
            &layout,
            &overrides,
            "restart",
            Some(&workspace),
            &flags,
            allow_sudo,
        ),
        Command::Status => status::show_status(&layout, None, None).await,
        Command::Reload => {
            control(&layout, DaemonCommand::Reload).await?;
            println!("Component configuration reloaded.");
            Ok(())
        }
        Command::Confirm { timeout, question } => {
            confirm::confirm(&layout, question, timeout).await
        }
        Command::Update { index_url } => update::update_cli(&layout, &overrides, &cwd, index_url),
        Command::Channels { command } => channels::channels(&layout, command).await,
        Command::Agents { command } => agents::agents(&layout, &overrides, command).await,
        Command::BuildInfo => {
            println!("{}", serde_json::to_string(&scv_server::build_info())?);
            Ok(())
        }
        Command::RestartWatchdog { plan } => {
            daemon::init_tracing();
            scv_server::restart_watchdog(&layout, &plan).await
        }
    }
}

/// Send one control command to the running daemon of the instance at
/// `layout`. A failed request carries a [`scv_client::ControlError`].
pub(crate) async fn control(layout: &Layout, command: DaemonCommand) -> Result<DaemonStatus> {
    Ok(scv_client::control(&layout.socket(), command).await?)
}
