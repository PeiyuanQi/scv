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

pub use load::{Setting, read_layer, user_home_path};
pub use schema::*;

#[cfg(test)]
mod tests;
