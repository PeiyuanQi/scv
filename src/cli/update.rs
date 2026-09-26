//! `scv update`: install the latest release with cargo and restart the
//! running daemon into it.

use anyhow::{Context, Result, bail};
use scv_server::config::{Config, ConfigOverrides};
use std::path::Path;
use std::process::Command as ProcessCommand;

pub(crate) fn update_cli(workspace: &Path, index_url: Option<String>) -> Result<()> {
    let configured = Config::load(workspace, ConfigOverrides::default())?
        .update
        .index_url;
    let index_url = index_url
        .or_else(|| std::env::var("SCV_CARGO_INDEX_URL").ok())
        .or(configured);
    let mut cargo = ProcessCommand::new("cargo");
    cargo.args(["install", "--locked", "--force"]);
    if let Some(index_url) = index_url.as_deref() {
        cargo.args(["--index", index_url]);
        println!("Updating SCV from Cargo index {index_url}");
    } else {
        println!("Updating SCV from crates.io");
    }
    cargo.arg("scv-cli");
    let status = cargo.status().context("run cargo install for scv-cli")?;
    if !status.success() {
        bail!("SCV update failed while installing scv-cli");
    }

    let service = scv_server::service_name()?;
    let active = ProcessCommand::new("systemctl")
        .args(["--user", "is-active", "--quiet"])
        .arg(&service)
        .status()
        .is_ok_and(|status| status.success());
    if active {
        let status = ProcessCommand::new("systemctl")
            .args(["--user", "restart"])
            .arg(&service)
            .status()
            .context("restart SCV daemon after update")?;
        if !status.success() {
            bail!("SCV updated, but restarting {service} failed");
        }
        println!("SCV updated and the running daemon was restarted; clients can reconnect.");
    } else {
        println!("SCV updated. No active user daemon was found to restart.");
    }
    Ok(())
}
