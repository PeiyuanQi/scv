//! `scv config`: create, locate, and inspect the instance's settings file.

mod overview;

use anyhow::Result;
use scv_client::Layout;
use scv_server::config::{Config, ConfigOverrides};
use std::path::Path;

use super::args::ConfigCommand;

pub(crate) fn run(
    command: ConfigCommand,
    layout: &Layout,
    cwd: &Path,
    overrides: &ConfigOverrides,
) -> Result<()> {
    match command {
        ConfigCommand::Init => {
            let path = Config::init_user_config(layout)?;
            println!("Created configuration at {}", path.display());
            Ok(())
        }
        ConfigCommand::Show { all } => {
            print!("{}", overview::render(layout, cwd, overrides, all)?);
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", layout.config().display());
            Ok(())
        }
    }
}
