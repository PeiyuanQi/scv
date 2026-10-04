//! A mail chat's commands: a chat account with `purpose = "mail"` runs no
//! model turn, so every message its owner sends is read here as one of
//! SCV's commands or answered with the help reply.
//!
//! Parsing has no side effects. `mail status` and `mail help` are answered
//! by the bridge; every other command goes, through the daemon's hub, to the
//! email accounts that report to this chat, and only their ledgers can do
//! anything with it. Only the account owner's direct message counts, and a
//! message that quotes or forwards another, or carries files, is never read
//! as a command: the text a transport inlines from a quoted report could
//! otherwise pass for one.

use scv_protocol::MailCounts;

use crate::Message;

pub(crate) mod codes;

/// The reply to anything that is not a command, and to `mail help`.
pub(crate) const HELP_REPLY: &str = "This chat carries SCV's mail reports; no model reads or \
     answers it. Commands: mail status, mail help. For accounts with mail actions on: approve \
     CODE…, deny CODE…|all, mail reply #H [what to say], mail forward #H ADDRESS[,ADDRESS] \
     [note], mail compose [ACCOUNT] ADDRESS[,ADDRESS] what to write, mail revise CODE what to \
     change, mail archive|read|trash|spam #H.";
/// The reply to `mail status` when no email account reports here.
pub(crate) const NO_ACCOUNTS_REPLY: &str = "No mail account is reporting to this chat right now.";
/// The reply when no running account here takes actions.
pub(crate) const NOT_RUNNING_REPLY: &str =
    "Mail actions are not running for any account that reports here; nothing was done.";

/// The most codes one `approve` or `deny` names.
pub(crate) const MAX_CODES: usize = 20;
/// The most addresses one command names.
pub(crate) const MAX_ADDRESSES: usize = 10;
/// The most bytes of a command's free text.
pub(crate) const MAX_TEXT_BYTES: usize = 2 * 1024;

/// What a message moves or marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageAction {
    Archive,
    Read,
    Trash,
    Spam,
}

/// A command the email accounts carry out. Codes and handles are
/// uppercased, handles without their `#`; addresses are as typed, checked
/// again by the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailCommand {
    /// `approve CODE…`: approve each action by its code.
    Approve(Vec<String>),
    /// `deny CODE…`: withdraw each action.
    Deny(Vec<String>),
    /// `deny all`: withdraw every action waiting for this chat's owner.
    DenyAll,
    /// `mail reply #H [what to say]`: draft a reply.
    Reply { handle: String, text: String },
    /// `mail forward #H ADDRESS[,ADDRESS] [note]`: forward a message.
    Forward {
        handle: String,
        to: Vec<String>,
        note: String,
    },
    /// `mail compose [ACCOUNT] ADDRESS[,ADDRESS] what to write`.
    Compose {
        account: Option<String>,
        to: Vec<String>,
        text: String,
    },
    /// `mail revise CODE what to change`: draft that reply or mail again.
    Revise { code: String, text: String },
    /// `mail archive|read|trash|spam #H`.
    Message {
        handle: String,
        action: MessageAction,
    },
    /// `mail status`: counts, and the actions waiting.
    Status,
}

/// What the owner's message asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Not a command, or `mail help`.
    Help,
    /// A command, and whether it needs an account that takes actions.
    Mail(MailCommand),
    /// A command written wrong: SCV's answer why.
    Invalid(&'static str),
}

/// What proves a command came from the owner of a mail chat: the platform
/// authenticated the sender, and the bridge checked it is the account's
/// owner, in a direct message, without files, quotes, or forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatEvidence {
    /// The mail chat, `<channel>:<account>`.
    pub(crate) route: String,
    /// The sender, who was the chat's owner when the bridge read it.
    pub(crate) peer: String,
    /// The platform's ID of the message.
    pub(crate) message_id: String,
    /// When the owner sent it, in Unix milliseconds by the platform's
    /// clock; a message without one never approves.
    pub(crate) sent_ms: Option<u64>,
}

/// Read the owner's direct message as a command.
pub(crate) fn command(message: &Message) -> Command {
    if message.quoted || message.reference.is_some() || !message.media.is_empty() {
        return Command::Help;
    }
    parse(&message.text)
}

/// Read `text` as a command.
pub(crate) fn parse(text: &str) -> Command {
    let text = text.trim();
    let words = Words::new(text);
    let Some(verb) = words.clone().next() else {
        return Command::Help;
    };
    match verb.to_lowercase().as_str() {
        "approve" | "批准" => codes_command(words, MailCommand::Approve),
        "deny" | "拒绝" => {
            let mut rest = words.clone();
            rest.next();
            let what = strip_end(rest.rest()).trim_start();
            if what.eq_ignore_ascii_case("all") || what == "全部" {
                return Command::Mail(MailCommand::DenyAll);
            }
            codes_command(words, MailCommand::Deny)
        }
        "mail" => mail(words),
        _ => Command::Help,
    }
}

/// Drop one trailing `.`, `。`, `!`, or `！` and the spaces before it.
fn strip_end(text: &str) -> &str {
    text.trim_end()
        .trim_end_matches(['.', '。', '!', '！'])
        .trim_end()
}

fn codes_command(mut words: Words<'_>, make: fn(Vec<String>) -> MailCommand) -> Command {
    words.text = strip_end(words.text);
    words.next();
    let mut found = Vec::new();
    for word in words {
        let Some(code) = codes::code(word) else {
            return Command::Invalid(
                "Not done: a code is six letters and digits, such as approve Q7M2KD.",
            );
        };
        if !found.contains(&code) {
            found.push(code);
        }
    }
    if found.is_empty() {
        return Command::Invalid("Not done: name the code, such as approve Q7M2KD.");
    }
    if found.len() > MAX_CODES {
        return Command::Invalid("Not done: one message may name at most 20 codes.");
    }
    Command::Mail(make(found))
}

fn mail(mut words: Words<'_>) -> Command {
    words.next();
    let Some(sub) = words.next() else {
        return Command::Help;
    };
    let sub = sub.to_lowercase();
    let sub = strip_end(&sub);
    match sub {
        "help" if words.rest().is_empty() => Command::Help,
        "status" if strip_end(words.rest()).is_empty() => Command::Mail(MailCommand::Status),
        "reply" => {
            let Some(handle) = words.next().and_then(codes::handle) else {
                return Command::Invalid("Not done: say which mail, such as mail reply #4K7P.");
            };
            let text = words.rest().trim();
            if text.len() > MAX_TEXT_BYTES {
                return Command::Invalid("Not done: say it in at most 2 KiB.");
            }
            Command::Mail(MailCommand::Reply {
                handle,
                text: text.to_owned(),
            })
        }
        "forward" | "fwd" => {
            let Some(handle) = words.next().and_then(codes::handle) else {
                return Command::Invalid(
                    "Not done: say which mail and to whom, such as mail forward #4K7P \
                     name@example.com.",
                );
            };
            let Some(to) = addresses(&mut words) else {
                return Command::Invalid(
                    "Not done: name up to 10 addresses, separated by commas, such as mail \
                     forward #4K7P name@example.com.",
                );
            };
            let note = words.rest().trim();
            if note.len() > MAX_TEXT_BYTES {
                return Command::Invalid("Not done: keep the note to at most 2 KiB.");
            }
            Command::Mail(MailCommand::Forward {
                handle,
                to,
                note: note.to_owned(),
            })
        }
        "compose" | "new" => {
            let account = words
                .clone()
                .next()
                .filter(|word| !word.contains('@') && crate::state::validate_name(word).is_ok())
                .map(str::to_owned);
            if account.is_some() {
                words.next();
            }
            let Some(to) = addresses(&mut words) else {
                return Command::Invalid(
                    "Not done: name up to 10 addresses, separated by commas, then what to \
                     write, such as mail compose name@example.com ask about Friday.",
                );
            };
            let text = words.rest().trim();
            if text.is_empty() {
                return Command::Invalid("Not done: say what to write after the addresses.");
            }
            if text.len() > MAX_TEXT_BYTES {
                return Command::Invalid("Not done: say it in at most 2 KiB.");
            }
            Command::Mail(MailCommand::Compose {
                account,
                to,
                text: text.to_owned(),
            })
        }
        "revise" => {
            let Some(code) = words.next().and_then(codes::code) else {
                return Command::Invalid(
                    "Not done: name the draft's code, such as mail revise Q7M2KD shorter.",
                );
            };
            let text = words.rest().trim();
            if text.is_empty() {
                return Command::Invalid("Not done: say what to change after the code.");
            }
            if text.len() > MAX_TEXT_BYTES {
                return Command::Invalid("Not done: say it in at most 2 KiB.");
            }
            Command::Mail(MailCommand::Revise {
                code,
                text: text.to_owned(),
            })
        }
        "archive" | "read" | "trash" | "spam" | "junk" => {
            let action = match sub {
                "archive" => MessageAction::Archive,
                "read" => MessageAction::Read,
                "trash" => MessageAction::Trash,
                _ => MessageAction::Spam,
            };
            let handle = words.next().map(strip_end).and_then(codes::handle);
            match handle {
                Some(handle) if strip_end(words.rest()).is_empty() => {
                    Command::Mail(MailCommand::Message { handle, action })
                }
                _ => Command::Invalid("Not done: say which mail, such as mail trash #4K7P."),
            }
        }
        _ => Command::Help,
    }
}

/// The next word as a list of addresses separated by commas (ASCII or
/// full-width) or semicolons, each `local@domain`; `None` when it is not.
fn addresses(words: &mut Words<'_>) -> Option<Vec<String>> {
    let word = words.next()?;
    let list: Vec<String> = word
        .split([',', '，', ';', '；'])
        .filter(|part| !part.is_empty())
        .map(|part| unlinked(part).to_owned())
        .collect();
    let plausible = |address: &String| {
        address.len() <= 254
            && address
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    };
    (!list.is_empty() && list.len() <= MAX_ADDRESSES && list.iter().all(plausible)).then_some(list)
}

/// `part` without the link a chat wraps a typed address in: Slack sends
/// `a@example.com` as `<mailto:a@example.com|a@example.com>`. A link whose
/// label is not its address stays as it is, and is refused as one.
fn unlinked(part: &str) -> &str {
    let Some(link) = part
        .strip_prefix("<mailto:")
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return part;
    };
    match link.split_once('|') {
        None => link,
        Some((address, label)) if address.eq_ignore_ascii_case(label) => address,
        Some(_) => part,
    }
}

/// The words of a command, and what follows the last one taken, as
/// written.
#[derive(Clone)]
struct Words<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Words<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    /// Everything after the words taken so far.
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }
}

impl<'a> Iterator for Words<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let rest = &self.text[self.at..];
        let start = rest.len() - rest.trim_start().len();
        let rest = &rest[start..];
        if rest.is_empty() {
            self.at = self.text.len();
            return None;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        self.at += start + end;
        Some(&rest[..end])
    }
}

/// `mail status` for accounts that report here without taking actions, as
/// counts only: never an address, a subject, or a model's text.
pub(crate) fn status(accounts: &[(String, MailCounts)], now: u64) -> String {
    if accounts.is_empty() {
        return NO_ACCOUNTS_REPLY.to_owned();
    }
    accounts
        .iter()
        .map(|(component, counts)| status_line(component, counts, now))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One account's counts on one line.
pub(crate) fn status_line(component: &str, counts: &MailCounts, now: u64) -> String {
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
