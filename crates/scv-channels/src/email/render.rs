//! The text the mail chat shows: one report per message and the digests
//! that carry them.
//!
//! Two kinds of line. SCV's lines hold only SCV's words, validated
//! addresses, times, and counts. Everything a sender or a model wrote
//! (display names, subjects, summaries, attachment names) is sanitized, has
//! its links replaced, and goes on lines that start with `│ `, one prefix per
//! line, so no untrusted text can pass for SCV's own line or fake a command.

use super::clean;
use super::ledger::{Counts, Item, MAX_ITEM_BYTES};
use super::plan::{Class, SendClass};
use super::source::{Address, AttachmentInfo, Meta};

/// The prefix of every line of untrusted text.
pub(crate) const UNTRUSTED: &str = "│ ";
/// The longest untrusted line, in characters.
const MAX_LINE_CHARS: usize = 200;
/// Attachments listed by name in one report.
const MAX_ATTACHMENTS: usize = 5;
/// Summary lines kept from triage.
pub(crate) const MAX_SUMMARY_LINES: usize = 5;

/// What triage said about a message, for its report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) lines: Vec<String>,
}

/// One message's report: its sender and time on SCV's line, then its
/// untrusted fields, the triage summary when there is one, and SCV's `note`
/// about how it was triaged. At most [`MAX_ITEM_BYTES`].
pub(crate) fn report(
    meta: &Meta,
    summary: Option<&Summary>,
    note: Option<&str>,
    urgent: bool,
    offset: i32,
) -> String {
    let mut lines = Vec::new();
    let sender = meta
        .from
        .as_ref()
        .and_then(|from| valid_address(&from.address));
    lines.push(format!(
        "{}{} · {} (sender not verified)",
        if urgent { "! " } else { "" },
        sender.as_deref().unwrap_or("(sender address not shown)"),
        clock(meta.received_at, offset),
    ));
    let mut untrusted = Vec::new();
    if let Some(from) = &meta.from {
        if !from.name.trim().is_empty() {
            untrusted.push(format!("From: {}", one_line(&from.name)));
        }
        if sender.is_none() && !from.address.trim().is_empty() {
            untrusted.push(format!("From address: {}", one_line(&from.address)));
        }
    }
    let subject = one_line(&meta.subject);
    untrusted.push(format!(
        "Subject: {}",
        if subject.is_empty() {
            "(none)"
        } else {
            &subject
        }
    ));
    for line in summary
        .map(|summary| summary.lines.as_slice())
        .unwrap_or_default()
    {
        for piece in clean::replace_links(&clean::sanitize(line)).split('\n') {
            let piece = piece.trim();
            if !piece.is_empty() {
                untrusted.push(cut(piece));
            }
        }
    }
    for attachment in meta.attachments.iter().take(MAX_ATTACHMENTS) {
        untrusted.push(attachment_line(attachment));
    }
    lines.extend(untrusted.iter().map(|line| format!("{UNTRUSTED}{line}")));
    let more = meta.attachments.len().saturating_sub(MAX_ATTACHMENTS);
    if more > 0 {
        lines.push(format!("  and {more} more attachments"));
    }
    if let Some(reply_to) = differing_reply_to(meta) {
        lines.push(format!("  Replies would go to {reply_to}, not the sender."));
    }
    if let Some(note) = note {
        lines.push(format!("  ({note})"));
    }
    fit_lines(&lines)
}

/// The report's lines joined, dropping untrusted lines from the end of the
/// untrusted block (summary and attachments first) until it fits.
fn fit_lines(lines: &[String]) -> String {
    let mut kept: Vec<&str> = lines.iter().map(String::as_str).collect();
    let marker = "│ …";
    let mut cut = false;
    while kept.iter().map(|line| line.len() + 1).sum::<usize>() + marker.len() > MAX_ITEM_BYTES {
        // The last untrusted line goes first; SCV's lines stay.
        let Some(index) = kept.iter().rposition(|line| line.starts_with(UNTRUSTED)) else {
            break;
        };
        kept.remove(index);
        cut = true;
    }
    let mut text = kept.join("\n");
    if cut {
        text.push('\n');
        text.push_str(marker);
    }
    text
}

fn attachment_line(attachment: &AttachmentInfo) -> String {
    let name = one_line(&attachment.name);
    let mime = one_line(&attachment.mime);
    format!(
        "Attachment: {} ({}, {})",
        if name.is_empty() { "unnamed" } else { &name },
        if mime.is_empty() {
            "unknown type"
        } else {
            &mime
        },
        size(attachment.size)
    )
}

/// The Reply-To address when it differs from the sender's, and is valid.
fn differing_reply_to(meta: &Meta) -> Option<String> {
    let reply_to = valid_address(&meta.reply_to.as_ref()?.address)?;
    let from = meta
        .from
        .as_ref()
        .and_then(|from: &Address| valid_address(&from.address));
    (from.as_deref() != Some(reply_to.as_str())).then_some(reply_to)
}

/// One untrusted field as one line: sanitized, links replaced, line breaks
/// turned into spaces, and cut to [`MAX_LINE_CHARS`].
fn one_line(text: &str) -> String {
    let clean = clean::replace_links(&clean::sanitize(text));
    let joined = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&joined)
}

fn cut(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(MAX_LINE_CHARS - 1).collect();
    kept.push('…');
    kept
}

/// `address` when it is safe to show on SCV's own line: a dot-atom local
/// part and an ASCII domain of letters, digits, and hyphens, lowercased.
/// Anything else stays on an untrusted line.
pub(crate) fn valid_address(address: &str) -> Option<String> {
    let address = address.trim();
    if address.len() > 254 {
        return None;
    }
    let (local, domain) = address.rsplit_once('@')?;
    let atext = |byte: u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte);
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|byte| atext(byte) || byte == b'.');
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    (local_ok && domain_ok).then(|| format!("{local}@{}", domain.to_ascii_lowercase()))
}

/// `HH:MM` in the owner's local time.
pub(crate) fn clock(unix: u64, offset: i32) -> String {
    let local = i64::try_from(unix).unwrap_or(i64::MAX) + i64::from(offset);
    let minutes = local.rem_euclid(86_400) / 60;
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// `+08:00`.
pub(crate) fn offset_label(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KiB", bytes / 1024),
        _ => format!("{} MiB", bytes / 1_048_576),
    }
}

/// How the digest shows counts it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DigestOptions {
    pub(crate) offset: i32,
    /// Whether mail counted without a report is mentioned.
    pub(crate) show_skipped: bool,
}

/// One message to the mail chat: a header with counts and the time span,
/// the items, and SCV's footer lines about mail not listed. The header and
/// footer are SCV's lines; every item keeps its own prefixes.
pub(crate) fn digest(
    account: &str,
    class: SendClass,
    items: &[&Item],
    counts: &Counts,
    options: DigestOptions,
) -> String {
    let reports = items
        .iter()
        .filter(|item| item.class == Class::Report)
        .count();
    let urgent = items
        .iter()
        .filter(|item| item.class == Class::Report && item.urgent)
        .count();
    let mut header = format!("Mail · {account}");
    if reports > 0 {
        header.push_str(&format!(" · {reports} new"));
        if urgent > 0 {
            header.push_str(&format!(" ({urgent} urgent)"));
        }
    }
    if options.show_skipped && counts.skipped > 0 && class != SendClass::Response {
        header.push_str(&format!(
            "{}{} skipped",
            if reports > 0 { ", " } else { " · " },
            counts.skipped
        ));
    }
    if let (Some(first), Some(last)) = (
        items.iter().map(|item| item.created_at).min(),
        items.iter().map(|item| item.created_at).max(),
    ) {
        let first = clock(first, options.offset);
        let last = clock(last, options.offset);
        let span = if first == last {
            first
        } else {
            format!("{first}–{last}")
        };
        header.push_str(&format!(" · {span} ({})", offset_label(options.offset)));
    }
    let mut parts = vec![header];
    parts.extend(items.iter().map(|item| item.text.clone()));
    let mut footer = Vec::new();
    if counts.undelivered > 0 {
        footer.push(format!(
            "{} earlier report{} could not be delivered.",
            counts.undelivered,
            plural(counts.undelivered)
        ));
    }
    if counts.unlisted > 0 {
        let since = counts
            .unlisted_since
            .map(|at| format!(" (oldest {})", clock(at, options.offset)))
            .unwrap_or_default();
        footer.push(format!(
            "{} more mail{} not listed{since}: the queue was full.",
            counts.unlisted,
            plural(counts.unlisted)
        ));
    }
    if counts.unreadable > 0 {
        footer.push(format!(
            "{} mail{} could not be read.",
            counts.unreadable,
            plural(counts.unreadable)
        ));
    }
    if !footer.is_empty() {
        parts.push(footer.join("\n"));
    }
    parts.join("\n\n")
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests;
