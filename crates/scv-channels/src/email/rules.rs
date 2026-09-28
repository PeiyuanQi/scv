//! Deciding, before any model, what a message is worth: the owner's rules
//! in order, then the built-in ones. This is the cheapest rung of the
//! ladder: most bulk mail never costs a token.

use super::settings::{Rule, RuleAction};
use super::source::Meta;

/// What to do with one message, and whether a rule made it urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) action: RuleAction,
    pub(crate) urgent: bool,
}

/// The first of the owner's `rules` that matches `meta`, else the built-in
/// rules: bulk and automated mail is counted, mail from a no-reply sender
/// is reported by its headers, and everything else is triaged.
pub(crate) fn decide(rules: &[Rule], meta: &Meta) -> Decision {
    if let Some(rule) = rules.iter().find(|rule| matches(rule, meta)) {
        return Decision {
            action: rule.action,
            urgent: rule.urgent,
        };
    }
    let action = if meta.signals.bulk() {
        RuleAction::Count
    } else if noreply(meta) {
        RuleAction::Header
    } else {
        RuleAction::Triage
    };
    Decision {
        action,
        urgent: false,
    }
}

fn matches(rule: &Rule, meta: &Meta) -> bool {
    let sender = meta
        .from
        .as_ref()
        .map(|from| from.address.trim().to_lowercase())
        .unwrap_or_default();
    let from = rule.from.is_empty() || rule.from.iter().any(|entry| sender_matches(entry, &sender));
    let list_id = rule.list_id.is_empty()
        || meta.signals.list_id.as_deref().is_some_and(|list| {
            let list = list.to_lowercase();
            rule.list_id
                .iter()
                .any(|entry| list.contains(entry.as_str()))
        });
    let bulk = rule.bulk.is_none_or(|bulk| bulk == meta.signals.bulk());
    let noreply = rule.noreply.is_none_or(|wanted| wanted == noreply(meta));
    from && list_id && bulk && noreply
}

/// Whether the lowercased `sender` is the address `entry` names, or lies in
/// the domain an `@domain` entry names or one of its subdomains.
fn sender_matches(entry: &str, sender: &str) -> bool {
    if let Some(domain) = entry.strip_prefix('@') {
        let Some((_, sender_domain)) = sender.rsplit_once('@') else {
            return false;
        };
        sender_domain == domain
            || sender_domain
                .strip_suffix(domain)
                .is_some_and(|rest| rest.ends_with('.'))
    } else {
        entry == sender
    }
}

/// Whether the sender looks like an address nobody reads: a no-reply or
/// system mailbox, or a message with an empty return path (bounces and
/// auto-replies).
pub(crate) fn noreply(meta: &Meta) -> bool {
    const LOCAL: &[&str] = &[
        "noreply",
        "no-reply",
        "no_reply",
        "donotreply",
        "do-not-reply",
        "do_not_reply",
        "mailer-daemon",
        "postmaster",
    ];
    let local = meta
        .from
        .as_ref()
        .and_then(|from| from.address.rsplit_once('@'))
        .map(|(local, _)| local.to_lowercase())
        .unwrap_or_default();
    meta.signals.null_return_path
        || LOCAL.iter().any(|name| {
            local == *name
                || local
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(['+', '-', '.', '_']))
        })
}

#[cfg(test)]
mod tests;
