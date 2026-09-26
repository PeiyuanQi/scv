//! `scv config`: create, locate, and inspect the instance's settings file.

mod overview;

use anyhow::Result;
use scv_server::config::{Config, ConfigOverrides};
use std::path::Path;

use super::args::ConfigCommand;

pub(crate) fn run(command: ConfigCommand, cwd: &Path, overrides: &ConfigOverrides) -> Result<()> {
    match command {
        ConfigCommand::Init => {
            let path = Config::init_user_config()?;
            println!("Created configuration at {}", path.display());
            Ok(())
        }
        ConfigCommand::Show { all } => {
            print!("{}", overview::render(cwd, overrides, all)?);
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", scv_client::Layout::from_env()?.config().display());
            Ok(())
        }
    }
}
