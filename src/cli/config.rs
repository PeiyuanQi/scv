//! `scv config`: create, locate, and inspect the instance's settings file.

use anyhow::Result;
use scv_server::ConfigOverrides;
use std::path::Path;

use super::args::ConfigCommand;

pub(crate) fn run(command: ConfigCommand, cwd: &Path, overrides: &ConfigOverrides) -> Result<()> {
    match command {
        ConfigCommand::Init => {
            let path = scv_server::init_user_config()?;
            println!("Created configuration at {}", path.display());
            Ok(())
        }
        ConfigCommand::Show { all } => {
            print!("{}", scv_server::overview::render(cwd, overrides, all)?);
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", scv_client::Layout::from_env()?.config().display());
            Ok(())
        }
    }
}
