//! `daemon.control`: status, component and delegation management,
//! scheduling a restart into a new release, asking the owner a yes/no
//! question in chat, and listing or withdrawing mail actions (never
//! approving one).

use std::sync::Arc;

use scv_channels::hub::{Hub, MailOrder, MailReply};
use scv_protocol::{DaemonCommand, DaemonStatus, DelegationInfo, DelegationSummary, MailAction};
use scv_tools::delegation::DelegationRegistry;
use tokio::sync::Mutex;

use crate::components;

/// Why a daemon control request failed.
pub(crate) enum ControlFailure {
    /// A delegation request the client can correct; the message is safe to show.
    Delegation(String),
    /// A restart the daemon refused; the message is safe to show.
    Restart(String),
    /// A question the daemon could not ask, or does not know; the message is
    /// safe to show.
    Confirm(String),
    /// A project ledger request the operator can correct.
    Project(String),
    /// A mail action request the daemon could not carry out; the message is
    /// safe to show.
    Mail(String),
    Component,
}

/// Apply a daemon control command, adding the instance's delegations.
pub(crate) async fn daemon_control(
    components: &Arc<Mutex<components::Components>>,
    registry: &DelegationRegistry,
    command: DaemonCommand,
) -> std::result::Result<DaemonStatus, ControlFailure> {
    let mut killed = Vec::new();
    let (restarter, confirmer) = {
        let components = components.lock().await;
        (components.restarter(), components.confirmer())
    };
    if let DaemonCommand::RestartWhenIdle { .. } = &command {
        let restarter = restarter.as_ref().ok_or_else(|| {
            ControlFailure::Restart("only the SCV daemon can schedule its restart".into())
        })?;
        restarter
            .request(command.clone())
            .await
            .map_err(ControlFailure::Restart)?;
    }
    let confirm = match &command {
        DaemonCommand::ConfirmAsk {
            question,
            parent,
            timeout_seconds,
        } => Some(
            confirmer
                .as_ref()
                .ok_or_else(|| ControlFailure::Confirm(ONLY_THE_DAEMON_ASKS.into()))?
                .ask(question, parent.as_deref(), *timeout_seconds)
                .await
                .map_err(ControlFailure::Confirm)?,
        ),
        DaemonCommand::ConfirmStatus { id } => Some(
            confirmer
                .as_ref()
                .ok_or_else(|| ControlFailure::Confirm(ONLY_THE_DAEMON_ASKS.into()))?
                .status(id)
                .map_err(ControlFailure::Confirm)?,
        ),
        _ => None,
    };
    let mail = match &command {
        DaemonCommand::MailStatus { account } => {
            let hub = components.lock().await.hub();
            Some(mail_status(&hub, account.as_deref()).await?)
        }
        DaemonCommand::MailCancel { account, id, all } => {
            let hub = components.lock().await.hub();
            Some(mail_cancel(&hub, account, id.as_deref(), *all).await?)
        }
        _ => None,
    };
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
    let project_command = is_project_command(&command);
    let mut status = components
        .lock()
        .await
        .control(command)
        .await
        .map_err(|error| {
            if project_command {
                ControlFailure::Project(format!("{error:#}"))
            } else {
                ControlFailure::Component
            }
        })?;
    let running = registry.list(false);
    // Idle as `scv agents ps` shows it: a live agent between turns, unless
    // a nested SCV's own background jobs keep it at work.
    let idle = running
        .iter()
        .filter(|entry| {
            entry.record.idle_since_unix.is_some()
                && entry.record.background_jobs.is_none_or(|jobs| jobs == 0)
        })
        .count();
    status.delegations = DelegationSummary {
        active: running.len() as u64,
        idle: Some(idle as u64),
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
            idle_since_unix_seconds: entry.record.idle_since_unix,
            background_jobs: entry.record.background_jobs,
        })
        .collect(),
        killed,
    };
    status.restart = restarter.and_then(|restarter| restarter.info());
    status.confirm = confirm;
    if let Some((actions, note)) = mail {
        status.mail_actions = actions;
        status.mail_note = note;
    }
    Ok(status)
}

fn is_project_command(command: &DaemonCommand) -> bool {
    matches!(
        command,
        DaemonCommand::ProjectCreate { .. }
            | DaemonCommand::ProjectStatus { .. }
            | DaemonCommand::ProjectEvents { .. }
            | DaemonCommand::ProjectTasks { .. }
            | DaemonCommand::ProjectReport { .. }
            | DaemonCommand::ProjectTaskAdd { .. }
            | DaemonCommand::ProjectTaskUpdate { .. }
            | DaemonCommand::ProjectRunStart { .. }
            | DaemonCommand::ProjectRunProgress { .. }
            | DaemonCommand::ProjectRunFinish { .. }
            | DaemonCommand::ProjectHeartbeat { .. }
    )
}

/// The actions of email account `account`, or of every running one that
/// takes actions: IDs, kinds, states, and times only.
async fn mail_status(
    hub: &Hub,
    account: Option<&str>,
) -> std::result::Result<(Vec<MailAction>, Option<String>), ControlFailure> {
    let components = match account {
        Some(account) => vec![mail_component(account)?],
        None => hub.mail_authorities(),
    };
    let mut actions = Vec::new();
    for component in components {
        match hub.mail_order(&component, MailOrder::List).await {
            Ok(MailReply::Actions(listed)) => actions.extend(listed),
            Ok(MailReply::Text(_)) => {}
            Err(error) if account.is_some() => return Err(ControlFailure::Mail(error.to_string())),
            Err(_) => {}
        }
    }
    Ok((actions, None))
}

/// Withdraw email account `account`'s action `id`, or all its actions.
async fn mail_cancel(
    hub: &Hub,
    account: &str,
    id: Option<&str>,
    all: bool,
) -> std::result::Result<(Vec<MailAction>, Option<String>), ControlFailure> {
    if id.is_some() == all {
        return Err(ControlFailure::Mail(
            "name one mail action's ID, or ask for all of them".into(),
        ));
    }
    if let Some(id) = id
        && !(id.len() == 33
            && id.starts_with('a')
            && id[1..].bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(ControlFailure::Mail(
            "that is not a mail action's ID".into(),
        ));
    }
    let component = mail_component(account)?;
    let order = MailOrder::Cancel {
        action: id.map(str::to_owned),
    };
    match hub.mail_order(&component, order).await {
        Ok(MailReply::Text(note)) => Ok((Vec::new(), Some(note))),
        Ok(MailReply::Actions(_)) => Ok((Vec::new(), None)),
        Err(error) => Err(ControlFailure::Mail(error.to_string())),
    }
}

/// The component of email account `account`.
fn mail_component(account: &str) -> std::result::Result<String, ControlFailure> {
    scv_channels::state::validate_name(account)
        .map_err(|_| ControlFailure::Mail("not an email account name".into()))?;
    Ok(format!("{}:{account}", scv_channels::email::CHANNEL))
}

const ONLY_THE_DAEMON_ASKS: &str = "only the SCV daemon can ask the owner";

#[cfg(test)]
mod tests;
