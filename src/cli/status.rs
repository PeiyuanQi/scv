//! `scv status` and `scv channels status`: the daemon's version, running and
//! idle delegations, a scheduled restart, and each channel account's health.

use anyhow::Result;
use scv_client::Layout;
use scv_protocol::{DaemonCommand, DelegationSummary};

use super::channels::{answers_nobody, nobody_note};
use super::control;
use super::daemon::describe_restart;

pub(crate) async fn show_status(
    layout: &Layout,
    channel: Option<&str>,
    account: Option<&str>,
) -> Result<()> {
    let status = match control(layout, DaemonCommand::Status).await {
        Ok(status) => status,
        Err(error) => {
            println!("Daemon: unavailable; component connectivity is unknown.");
            return Err(error);
        }
    };
    println!(
        "Daemon: running, version {}, pid {}",
        status.version, status.pid
    );
    println!("{}", describe_delegations(&status.delegations));
    if let Some(restart) = &status.restart {
        println!("{}", describe_restart(restart));
    }
    let matching: Vec<_> = status
        .components
        .iter()
        .filter(|h| channel.is_none_or(|name| h.channel == name))
        .filter(|h| account.is_none_or(|name| h.account == name))
        .collect();
    let enabled = matching.iter().filter(|health| health.enabled).count();
    let connected = matching
        .iter()
        .filter(|health| health.enabled && health.state == scv_protocol::ComponentState::Connected)
        .count();
    println!("Channels: {connected} of {enabled} enabled accounts connected");
    for health in &matching {
        // JSON escaping makes account identity and other untrusted strings terminal-safe.
        println!("{}", serde_json::to_string_pretty(health)?);
        if answers_nobody(health) {
            println!("Note: {}", nobody_note(health));
        }
    }
    if matching.is_empty() {
        println!("No matching supervised components.");
    }
    Ok(())
}

/// The `Delegations:` line. Runs at work count as running and live agents
/// between turns as idle, as `scv agents ps` lists them; a daemon that does
/// not report idle agents gets no idle part.
fn describe_delegations(delegations: &DelegationSummary) -> String {
    let orphans = format!(
        "{} orphaned runs stopped since the daemon started",
        delegations.reaped
    );
    match delegations.idle {
        Some(idle) => format!(
            "Delegations: {} running, {idle} idle, {orphans}",
            delegations.active.saturating_sub(idle)
        ),
        None => format!("Delegations: {} running, {orphans}", delegations.active),
    }
}

#[cfg(test)]
mod tests;
