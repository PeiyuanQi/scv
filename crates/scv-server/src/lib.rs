//! The SCV server: the authority over sessions, policy, and approvals, served
//! over the daemon's Unix socket ([`run_socket`]) or one stdio connection
//! ([`run_stdio`]), for the instance the caller selected with
//! `scv_client::Layout::from_env`.
//!
//! Each connection gets its own session with an ordered turn queue; turns run
//! `scv_core::AgentRuntime` with the configured provider and the tools
//! `scv_tools` offers. The daemon also supervises long-running components
//! (`components`), such as chat channel accounts, and plans restarts into a
//! newly installed release. [`config`] reads and layers the configuration,
//! which the `scv` command line also inspects.

mod approval;
mod attachments;
mod components;
pub mod config;
mod confirm;
mod connection;
mod control;
mod daemon;
mod events;
mod outbound;
mod prompt;
mod restart;
mod session;
#[cfg(test)]
mod test_support;

pub use daemon::{run_socket, run_stdio};
pub use restart::{BuildInfo, build_info, watchdog as restart_watchdog};

#[cfg(test)]
mod tests;
