//! `scv mail`: list or withdraw the mail actions email accounts are waiting
//! to carry out. Nothing here approves anything: only the owner does, with
//! an action's code in the mail chat. Output names actions by ID, kind, and
//! state only, never a code, an address, or mail text.

use anyhow::Result;
use scv_client::Layout;
use scv_protocol::{ComponentHealth, DaemonCommand, MailAction};

use super::args::MailCommand;
use super::control;

pub(crate) async fn mail(layout: &Layout, command: MailCommand) -> Result<()> {
    match command {
        MailCommand::Status { account } => {
            let status = control(
                layout,
                DaemonCommand::MailStatus {
                    account: account.clone(),
                },
            )
            .await?;
            let accounts: Vec<&ComponentHealth> = status
                .components
                .iter()
                .filter(|health| health.channel == scv_channels::email::CHANNEL)
                .filter(|health| account.as_deref().is_none_or(|name| health.account == name))
                .collect();
            if accounts.is_empty() {
                println!("No email account is signed in.");
                return Ok(());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            for health in accounts {
                println!("{}", account_line(health));
                let actions: Vec<&MailAction> = status
                    .mail_actions
                    .iter()
                    .filter(|action| action.account == health.id)
                    .collect();
                for action in actions {
                    println!("  {}", action_line(action, now));
                }
            }
            Ok(())
        }
        MailCommand::Cancel { account, id, all } => {
            let status = control(layout, DaemonCommand::MailCancel { account, id, all }).await?;
            println!(
                "{}",
                status
                    .mail_note
                    .as_deref()
                    .unwrap_or("The daemon did not say what it withdrew.")
            );
            Ok(())
        }
    }
}

/// One account: what it may do, and its actions as counts.
pub(crate) fn account_line(health: &ComponentHealth) -> String {
    let Some(counts) = &health.mail else {
        return format!("{}: not running", health.id);
    };
    let provider = counts.provider.as_deref().unwrap_or("imap");
    if counts.actions.is_empty() {
        return format!("{} ({provider}): reads mail only", health.id);
    }
    format!(
        "{} ({provider}): may {} once you approve each; {} waiting, {} being carried out, {} of \
         unknown outcome, {} done in the last day",
        health.id,
        counts.actions.join(", "),
        counts.actions_open,
        counts.actions_executing,
        counts.actions_unknown,
        counts.actions_done_24h
    )
}

/// One action: its ID, kind, state, and how long it has left.
pub(crate) fn action_line(action: &MailAction, now: u64) -> String {
    let left = action
        .expires_unix_seconds
        .map(|at| {
            let seconds = at.saturating_sub(now);
            if seconds == 0 {
                ", expiring".to_owned()
            } else {
                format!(
                    ", {} h {} min left to approve",
                    seconds / 3600,
                    seconds / 60 % 60
                )
            }
        })
        .unwrap_or_default();
    format!("{} {} {}{left}", action.id, action.kind, action.state)
}

#[cfg(test)]
mod tests;
