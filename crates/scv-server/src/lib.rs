//! The SCV server: the authority over sessions, policy, and approvals, served
//! over the daemon's Unix socket ([`run_socket`]) or one stdio connection
//! ([`run_stdio`]).
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

use anyhow::anyhow;
use sha2::{Digest, Sha256};

pub use daemon::{run_socket, run_stdio};
pub use restart::{BuildInfo, CONFIG_LAYOUT, build_info, watchdog as restart_watchdog};

/// Return the user service name for the selected SCV instance.
pub fn service_name() -> anyhow::Result<String> {
    if std::env::var_os("SCV_HOME").is_none() {
        return Ok("scv.service".into());
    }
    let home =
        config::user_home_path().ok_or_else(|| anyhow!("cannot determine SCV instance home"))?;
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("scv-{suffix}.service"))
}

#[cfg(test)]
mod tests;
