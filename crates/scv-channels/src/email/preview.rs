//! What the owner is shown before approving an action: its preview.
//!
//! A preview shows every bound field that decides the action's effect,
//! exactly as it will happen: for outgoing mail the sender, every
//! recipient, the subject, and the whole body; for a move or a mark the
//! message (by its handle, sender, and subject) and the folder. SCV's lines
//! hold only its words, codes, handles, folder names, times, and plain
//! addresses; the subject, the body, and a message's display name and
//! subject go on lines that start with `│ `, one prefix per line, so
//! nothing a sender or a model wrote can pass for SCV's line or a code.
//! A preview is never cut: the content's bounds keep it within one message.

use super::content::{ActionContent, ActionKind, FolderRole, Form};
use super::render::UNTRUSTED;
use super::source::ProviderKind;

/// One alternative a preview offers: its code and what approving it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) code: String,
    pub(crate) kind: ActionKind,
}

/// Every line of `text` on its own untrusted line.
fn untrusted(text: &str, out: &mut Vec<String>) {
    if text.is_empty() {
        out.push(format!("{UNTRUSTED}(empty)"));
        return;
    }
    for line in text.split('\n') {
        out.push(format!("{UNTRUSTED}{line}"));
    }
}

/// The folder as the owner reads it: its role, and for IMAP its own name.
pub(crate) fn folder_label(role: FolderRole, name: &str, provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Imap => {
            let decoded = super::imap::utf7_decode(name);
            if decoded.eq_ignore_ascii_case(role.title()) {
                role.title().to_owned()
            } else {
                format!("{} (the folder \"{}\")", role.title(), one_line(&decoded))
            }
        }
        ProviderKind::Gmail | ProviderKind::Graph => role.title().to_owned(),
    }
}

/// A folder name shown on SCV's line: its control and quote characters
/// dropped, so it cannot end the line or its quotes.
fn one_line(text: &str) -> String {
    super::clean::sanitize(text)
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(100)
        .collect()
}

/// What approving `kind` does, for the choice line.
fn choice_text(kind: ActionKind, folder: Option<&str>) -> String {
    match kind {
        ActionKind::Draft => "save it in Drafts".to_owned(),
        ActionKind::Send => "send it now".to_owned(),
        ActionKind::Archive => "archive it".to_owned(),
        ActionKind::MarkRead => "mark it read".to_owned(),
        ActionKind::Trash | ActionKind::Spam => {
            format!("move it to {}", folder.unwrap_or("that folder"))
        }
    }
}

/// The preview of an outgoing message offered as `choices` (saving it,
/// sending it, or both), with its notes about the original, at most
/// `approval_hours` to answer once it arrives.
pub(crate) fn outgoing(content: &ActionContent, choices: &[Choice], approval_hours: u64) -> String {
    let Some(message) = &content.message else {
        return String::new();
    };
    let handle = content
        .display
        .as_ref()
        .map(|display| display.handle.as_str());
    let title = match (message.form, handle) {
        (Form::Reply, Some(handle)) => format!("Reply to #{handle}"),
        (Form::Forward, Some(handle)) => format!("Forward of #{handle}"),
        (Form::Reply, None) => "Reply".to_owned(),
        (Form::Forward, None) => "Forward".to_owned(),
        (Form::Compose, _) => "New mail".to_owned(),
    };
    let mut lines = vec![format!("{title} · digest {}", content.short_digest())];
    let from = if message.from.name.is_empty() {
        message.from.address.clone()
    } else {
        format!("\"{}\" <{}>", message.from.name, message.from.address)
    };
    lines.push(format!("  From: {from}"));
    lines.push(format!("  To: {}", message.to.join(", ")));
    lines.push(format!(
        "  Cc: {}",
        if message.cc.is_empty() {
            "none".to_owned()
        } else {
            message.cc.join(", ")
        }
    ));
    for note in &message.notes {
        lines.push(format!("  {note}"));
    }
    lines.push("  Subject:".to_owned());
    untrusted(&message.subject, &mut lines);
    lines.push("  Body:".to_owned());
    untrusted(&message.body, &mut lines);
    push_choices(&mut lines, choices, None, approval_hours);
    lines.join("\n")
}

/// The preview of a move or a mark of the message `content` names.
pub(crate) fn change(
    content: &ActionContent,
    choice: &Choice,
    folder: Option<&str>,
    approval_hours: u64,
) -> String {
    let handle = content
        .display
        .as_ref()
        .map_or("?", |display| display.handle.as_str());
    let verb = match content.kind {
        ActionKind::Archive => format!("Archive #{handle}"),
        ActionKind::MarkRead => format!("Mark #{handle} read"),
        _ => format!("Move #{handle} to {}", folder.unwrap_or("another folder")),
    };
    let mut lines = vec![format!("{verb} · digest {}", content.short_digest())];
    if let Some(display) = &content.display {
        if !display.from_address.is_empty() {
            lines.push(format!("  Sender: {} (not verified)", display.from_address));
        }
        let mut untrusted_lines = Vec::new();
        if !display.from_name.is_empty() {
            untrusted_lines.push(format!("From: {}", display.from_name));
        }
        untrusted_lines.push(format!(
            "Subject: {}",
            if display.subject.is_empty() {
                "(none)"
            } else {
                &display.subject
            }
        ));
        for line in untrusted_lines {
            untrusted(&line, &mut lines);
        }
    }
    push_choices(
        &mut lines,
        std::slice::from_ref(choice),
        folder,
        approval_hours,
    );
    lines.join("\n")
}

/// The preview of `contents` (alternatives of one action) offered by
/// `codes`, for `provider`'s folders.
pub(crate) fn render(
    contents: &[ActionContent],
    codes: &[String],
    provider: ProviderKind,
    approval_hours: u64,
) -> String {
    let choices: Vec<Choice> = contents
        .iter()
        .zip(codes)
        .map(|(content, code)| Choice {
            code: code.clone(),
            kind: content.kind,
        })
        .collect();
    let Some(first) = contents.first() else {
        return String::new();
    };
    if first.message.is_some() {
        return outgoing(first, &choices, approval_hours);
    }
    let label = first
        .folder
        .as_ref()
        .map(|folder| folder_label(folder.role, &folder.name, provider));
    change(first, &choices[0], label.as_deref(), approval_hours)
}

/// The line a report gets for a move triage suggests.
pub(crate) fn suggestion(
    handle: &str,
    choice: &Choice,
    folder: Option<&str>,
    approval_hours: u64,
) -> String {
    let what = match choice.kind {
        ActionKind::Archive => format!("archive #{handle}"),
        _ => format!("move #{handle} to {}", folder.unwrap_or("another folder")),
    };
    format!(
        "  Suggested: {what}. approve {} to do it (for {approval_hours} hours once this reaches \
         you), or ignore it.",
        choice.code
    )
}

fn push_choices(lines: &mut Vec<String>, choices: &[Choice], folder: Option<&str>, hours: u64) {
    for choice in choices {
        lines.push(format!(
            "  approve {}  {}",
            choice.code,
            choice_text(choice.kind, folder)
        ));
    }
    let codes: Vec<&str> = choices.iter().map(|choice| choice.code.as_str()).collect();
    lines.push(format!(
        "  The code{} work{} for {hours} hours once this reaches you. deny {} discards it.",
        if codes.len() == 1 { "" } else { "s" },
        if codes.len() == 1 { "s" } else { "" },
        codes.join(" ")
    ));
}

#[cfg(test)]
mod tests;
