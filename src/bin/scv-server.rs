//! `scv-server`: the stdio agent server on its own, for clients that spawn
//! it directly (`scv server` is the same endpoint inside the main binary).

#[path = "../cli/common.rs"]
mod common;

use anyhow::Result;
use clap::Parser;
use scv_server::config::ConfigOverrides;
use std::path::PathBuf;

use common::ApprovalArg;

#[derive(Parser)]
#[command(name = "scv-server", version, about = "SCV stdio agent server")]
struct Cli {
    /// Newline-delimited JSON over stdin/stdout is the only mode; the flag
    /// is still accepted for the callers that pass it.
    #[arg(long, hide = true)]
    stdio: bool,
    #[arg(long, value_name = "PATH", env = "SCV_HOME")]
    scv_home: Option<PathBuf>,
    #[arg(long, value_name = "PATH", env = "SCV_CONFIG")]
    config_path: Option<PathBuf>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long, value_enum)]
    approval_policy: Option<ApprovalArg>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    // SAFETY: nothing but this thread exists yet. The tokio runtime is built
    // below, after the instance is selected, and no child process has been
    // started, so no one can read the environment while it is being set.
    unsafe {
        common::apply_process_config(cli.scv_home.as_deref(), cli.config_path.as_deref(), &cwd)?;
    }
    let layout = scv_client::Layout::from_env()?;
    let config_file = cli
        .config_path
        .as_deref()
        .map(|path| common::absolute_path(path, &cwd));
    tokio::runtime::Runtime::new()?.block_on(serve(cli, layout, config_file))
}

async fn serve(cli: Cli, layout: scv_client::Layout, config_file: Option<PathBuf>) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    scv_server::run_stdio(
        &layout,
        ConfigOverrides {
            provider: cli.provider,
            model: cli.model,
            base_url: cli.base_url,
            approval_policy: cli.approval_policy.map(Into::into),
            no_tools: false,
            config_file,
        },
    )
    .await
}
