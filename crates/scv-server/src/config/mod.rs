//! SCV's configuration: the schema of `config.toml` ([`schema`]), how the
//! layers are read and merged ([`load`]), what each value must satisfy and
//! what a project may change ([`validate`]), and the settings the runtime,
//! tools, and provider are built from ([`runtime`]).

mod load;
mod runtime;
mod schema;
mod validate;

pub(crate) use load::read_layer;
pub use load::user_home_path;
pub use schema::*;

#[cfg(test)]
mod tests;
