//! `scv`: the terminal client, the daemon, and the commands that manage
//! them. Each command group lives in its own module under `cli/`.

mod cli;

use anyhow::Result;
use clap::Parser as _;

fn main() -> Result<()> {
    let cli = cli::args::Cli::parse();
    let cwd = std::env::current_dir()?;
    // SAFETY: nothing but this thread exists yet. The tokio runtime is built
    // below, after the instance is selected, and no child process has been
    // started, so no one can read the environment while it is being set.
    unsafe {
        cli::common::apply_process_config(
            cli.scv_home.as_deref(),
            cli.config_path.as_deref(),
            &cwd,
        )?;
    }
    tokio::runtime::Runtime::new()?.block_on(cli::run(cli, cwd))
}
