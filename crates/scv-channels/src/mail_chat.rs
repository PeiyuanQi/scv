//! A mail chat's replies to its owner: a chat account with
//! `purpose = "mail"` runs no model turn, so every message its owner sends
//! gets one of SCV's fixed replies, decided here without side effects.
//!
//! This release reports mail and takes no action on it, so the only commands
//! are `mail status` and `mail help`. The action commands of the mail design
//! (`approve`, `deny`, `mail reply`, …) are recognised only to say they are
//! not available; nothing here can change a mailbox or send mail.

use scv_protocol::MailCounts;

use crate::Message;

/// The reply to anything that is not a command, and to `mail help`.
pub(crate) const HELP_REPLY: &str = "This chat carries SCV's mail reports; no model reads or \
     answers it. Commands: mail status, mail help. Replying to, saving, sending, and moving mail \
     are not available in this release.";
/// The reply to a command for a mail action.
pub(crate) const LATER_REPLY: &str =
    "Mail actions are not available in this release; nothing was done.";
/// The reply to `mail status` when no email account reports here.
pub(crate) const NO_ACCOUNTS_REPLY: &str = "No mail account is reporting to this chat right now.";

/// What the owner's message asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Status,
    Help,
    /// A mail action this release does not offer.
    Action,
}

/// Read the owner's direct message as a command. A message that quotes or
/// forwards another, or carries files, is never a command: the text a
/// transport inlines from a quoted report could otherwise be read as one.
pub(crate) fn command(message: &Message) -> Command {
    if message.quoted || message.reference.is_some() || !message.media.is_empty() {
        return Command::Help;
    }
    let normalized = message
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let words: Vec<&str> = normalized
        .trim_end_matches(['.', '。', '!', '！'])
        .split(' ')
        .collect();
    match words.as_slice() {
        ["mail", "status"] => Command::Status,
        ["mail", "help"] => Command::Help,
        ["approve" | "批准" | "deny" | "拒绝", _, ..]
        | [
            "mail",
            "reply" | "trash" | "spam" | "compose" | "revise",
            ..,
        ] => Command::Action,
        _ => Command::Help,
    }
}

/// The fixed reply to `command`, given the running email accounts that
/// report here and the time now in Unix seconds. It holds SCV's words and
/// counts only: never an address, a subject, or a model's text.
pub(crate) fn reply(command: Command, accounts: &[(String, MailCounts)], now: u64) -> String {
    match command {
        Command::Help => HELP_REPLY.to_owned(),
        Command::Action => LATER_REPLY.to_owned(),
        Command::Status if accounts.is_empty() => NO_ACCOUNTS_REPLY.to_owned(),
        Command::Status => accounts
            .iter()
            .map(|(component, counts)| status_line(component, counts, now))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn status_line(component: &str, counts: &MailCounts, now: u64) -> String {
    let checked = counts.last_check_unix_seconds.map_or_else(
        || "not checked yet".to_owned(),
        |at| format!("last check {}", ago(now, at)),
    );
    let budget = if counts.token_budget == 0 {
        "the model is off".to_owned()
    } else {
        format!(
            "{} of {} model tokens used",
            counts.tokens_today, counts.token_budget
        )
    };
    format!(
        "{component}: {} new today, {} triaged, {} reported; {} waiting to be sent; {budget}; \
         {checked}.",
        counts.seen_today, counts.triaged_today, counts.reported_today, counts.queued
    )
}

/// How long ago `at` was, roughly.
fn ago(now: u64, at: u64) -> String {
    let seconds = now.saturating_sub(at);
    match seconds {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", seconds / 60),
        _ => format!("{} h ago", seconds / 3600),
    }
}

#[cfg(test)]
mod tests;
