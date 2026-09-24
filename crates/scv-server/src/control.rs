//! `daemon.control`: status, component and delegation management, and
//! scheduling a restart into a new release.

use std::sync::Arc;

use scv_protocol::{DaemonCommand, DaemonStatus, DelegationInfo, DelegationSummary};
use scv_tools::delegation::DelegationRegistry;
use tokio::sync::Mutex;

use crate::components;

/// Why a daemon control request failed.
pub(crate) enum ControlFailure {
    /// A delegation request the client can correct; the message is safe to show.
    Delegation(String),
    /// A restart the daemon refused; the message is safe to show.
    Restart(String),
    Component,
}

/// Apply a daemon control command, adding the instance's delegations.
pub(crate) async fn daemon_control(
    components: &Arc<Mutex<components::Components>>,
    registry: &DelegationRegistry,
    command: DaemonCommand,
) -> std::result::Result<DaemonStatus, ControlFailure> {
    let mut killed = Vec::new();
    let restarter = components.lock().await.restarter();
    if let DaemonCommand::RestartWhenIdle { .. } = &command {
        let restarter = restarter.as_ref().ok_or_else(|| {
            ControlFailure::Restart("only the SCV daemon can schedule its restart".into())
        })?;
        restarter
            .request(command.clone())
            .await
            .map_err(ControlFailure::Restart)?;
    }
    let listing = match &command {
        DaemonCommand::Delegations { all } => Some(*all),
        DaemonCommand::DelegationKill { handle, orphans } => {
            if handle.is_none() && !orphans {
                return Err(ControlFailure::Delegation(
                    "name a delegation handle or ask for orphans".into(),
                ));
            }
            if *orphans {
                let report = registry.reconcile().await;
                killed.extend(report.reaped);
            }
            if let Some(handle) = handle {
                registry
                    .kill(handle)
                    .await
                    .map_err(ControlFailure::Delegation)?;
                killed.push(handle.clone());
            }
            Some(true)
        }
        _ => None,
    };
    let mut status = components
        .lock()
        .await
        .control(command)
        .await
        .map_err(|_| ControlFailure::Component)?;
    let running = registry.list(false);
    status.delegations = DelegationSummary {
        active: running.len() as u64,
        reaped: registry.reaped_total(),
        entries: match listing {
            Some(true) => registry.list(true),
            Some(false) => running,
            None => Vec::new(),
        }
        .into_iter()
        .map(|entry| DelegationInfo {
            handle: entry.record.handle,
            agent: entry.record.agent,
            session: entry.record.session,
            depth: entry.record.depth,
            pid: entry.record.process.pid,
            owner_pid: entry.record.owner.pid,
            processes: u32::try_from(entry.processes).unwrap_or(u32::MAX),
            cwd: entry.record.cwd.display().to_string(),
            started_unix_seconds: entry.record.started_unix,
            orphaned: entry.orphaned,
            conversation: entry.record.conversation,
            turn: entry.record.turn,
        })
        .collect(),
        killed,
    };
    status.restart = restarter.and_then(|restarter| restarter.info());
    Ok(status)
}

#[cfg(test)]
mod tests;
