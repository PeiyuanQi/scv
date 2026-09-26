//! SCV's configuration: the schema of `config.toml` ([`schema`]), how the
//! layers are read and merged ([`load`]), what each value must satisfy and
//! what a project may change ([`validate`]), and the settings the runtime,
//! tools, and provider are built from ([`runtime`]).
//!
//! The `scv` command line reads it too: `scv config show` lists every
//! [`Setting`] with its origin, and `scv agents` finds the agent homes.

mod load;
mod runtime;
mod schema;
mod validate;

pub use load::{Setting, read_layer};
pub use schema::*;

use anyhow::Result;
use scv_client::Layout;

/// The instance a server process serves and the overrides its command line
/// chose. Every configuration the process loads starts from these.
#[derive(Debug, Clone)]
pub(crate) struct Instance {
    pub(crate) layout: Layout,
    pub(crate) overrides: ConfigOverrides,
}

impl Instance {
    /// The configuration files alone: the explicit layer, without the
    /// command line's provider, model, or policy flags.
    fn files(&self) -> ConfigOverrides {
        ConfigOverrides {
            config_file: self.overrides.config_file.clone(),
            ..ConfigOverrides::default()
        }
    }

    /// The configuration files as a session in `workspace` reads them.
    pub(crate) fn load(&self, workspace: &std::path::Path) -> Result<Config> {
        Config::load(&self.layout, workspace, self.files())
    }

    /// The user's configuration files, without a project layer.
    pub(crate) fn load_user(&self) -> Result<Config> {
        Config::load_user(&self.layout, self.files())
    }
}

#[cfg(test)]
mod tests;
