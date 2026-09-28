//! When the mail chat hears about mail: a pure function of the queue, the
//! send log, the settings, and the clock.
//!
//! [`plan`] decides one message per call, never reads the time itself, and
//! never looks at what the items say: a model can affect only an item's
//! `urgent` bit, and that bit counts only within the urgent budget. Every
//! limit is a rolling window over the durable send log, so a restart cannot
//! reset one.

use super::settings::{QuietHours, QuietUrgent};

/// What an item is, for grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Class {
    /// One mail's report.
    Report,
    /// SCV's own line about the account: a reset, a budget, overflow.
    System,
    /// SCV's line the owner should see at once, such as the pause notice.
    Response,
}

/// What kind of message went out, for the rolling limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SendClass {
    Digest,
    Urgent,
    Response,
}

/// A queued item as the plan sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Queued {
    pub(crate) seq: u64,
    pub(crate) created_at: u64,
    pub(crate) class: Class,
    pub(crate) urgent: bool,
    /// Its rendered size in bytes.
    pub(crate) bytes: usize,
}

/// A message sent, as the send log keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Logged {
    pub(crate) at: u64,
    pub(crate) class: SendClass,
}

/// The settings the plan applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) settle_seconds: u64,
    pub(crate) max_delay_seconds: u64,
    pub(crate) max_items: usize,
    /// Room for items in one message, in bytes.
    pub(crate) max_bytes: usize,
    pub(crate) max_urgent_per_hour: usize,
    pub(crate) max_messages_per_hour: usize,
    pub(crate) max_messages_per_day: usize,
    pub(crate) max_responses_per_hour: usize,
    pub(crate) quiet: Option<QuietHours>,
    pub(crate) quiet_urgent: QuietUrgent,
    /// Seconds east of UTC of the owner's local time.
    pub(crate) offset: i32,
}

/// What to do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Nothing is waiting.
    Idle,
    /// Nothing to send before `until`. `paused`: items are due but the
    /// message limits hold them, which the owner should hear about.
    Wait { until: u64, paused: bool },
    /// Send these items, in this order, as one message.
    Send { seqs: Vec<u64>, class: SendClass },
}

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;

/// The next message to send, or how long to wait. `queue` may be in any
/// order; `log` holds at least the last day's messages.
pub(crate) fn plan(now: u64, queue: &[Queued], log: &[Logged], limits: &Limits) -> Plan {
    let mut ready: Vec<&Queued> = queue.iter().collect();
    ready.sort_by_key(|item| (item.created_at, item.seq));
    let counted = |classes: &[SendClass], window: u64| {
        log.iter()
            .filter(|entry| classes.contains(&entry.class) && now.saturating_sub(entry.at) < window)
            .count()
    };
    let hourly_digest = counted(&[SendClass::Digest], HOUR);
    let hourly_urgent = counted(&[SendClass::Urgent], HOUR);
    let hourly_response = counted(&[SendClass::Response], HOUR);
    let daily = counted(&[SendClass::Digest, SendClass::Urgent], DAY);

    let responses: Vec<&Queued> = ready
        .iter()
        .copied()
        .filter(|item| item.class == Class::Response)
        .collect();
    if !responses.is_empty() && hourly_response < limits.max_responses_per_hour {
        return send(&responses, SendClass::Response, limits);
    }
    let response_wait = if responses.is_empty() {
        u64::MAX
    } else {
        room_at(
            now,
            log,
            &[SendClass::Response],
            HOUR,
            limits.max_responses_per_hour,
        )
    };
    let batched: Vec<&Queued> = ready
        .iter()
        .copied()
        .filter(|item| item.class != Class::Response)
        .collect();
    if batched.is_empty() {
        return if responses.is_empty() {
            Plan::Idle
        } else {
            wait(response_wait, false)
        };
    }
    let urgent: Vec<&Queued> = batched.iter().copied().filter(|item| item.urgent).collect();
    let urgent_ok = !urgent.is_empty()
        && hourly_urgent < limits.max_urgent_per_hour
        && daily < limits.max_messages_per_day;
    if let Some(quiet) = limits.quiet
        && quiet_now(now, limits.offset, quiet)
    {
        if urgent_ok && limits.quiet_urgent == QuietUrgent::Deliver {
            return send(&urgent, SendClass::Urgent, limits);
        }
        return wait(
            quiet_end(now, limits.offset, quiet).min(response_wait),
            false,
        );
    }
    if urgent_ok {
        // Urgent items first; the rest ride along as far as they fit.
        let mut ordered = urgent.clone();
        ordered.extend(batched.iter().copied().filter(|item| !item.urgent));
        return send(&ordered, SendClass::Urgent, limits);
    }
    let first = batched
        .iter()
        .map(|item| item.created_at)
        .min()
        .unwrap_or(now);
    let last = batched
        .iter()
        .map(|item| item.created_at)
        .max()
        .unwrap_or(now);
    let due = batched.len() >= limits.max_items
        || now.saturating_sub(last) >= limits.settle_seconds
        || now.saturating_sub(first) >= limits.max_delay_seconds;
    if !due {
        let until = (last + limits.settle_seconds)
            .min(first + limits.max_delay_seconds)
            .min(response_wait);
        return wait(until, false);
    }
    if hourly_digest >= limits.max_messages_per_hour || daily >= limits.max_messages_per_day {
        let hourly = room_at(
            now,
            log,
            &[SendClass::Digest],
            HOUR,
            limits.max_messages_per_hour,
        );
        let day = room_at(
            now,
            log,
            &[SendClass::Digest, SendClass::Urgent],
            DAY,
            limits.max_messages_per_day,
        );
        return wait(hourly.max(day).min(response_wait), true);
    }
    send(&batched, SendClass::Digest, limits)
}

fn wait(until: u64, paused: bool) -> Plan {
    Plan::Wait { until, paused }
}

/// The longest prefix of `items` within the message's item and byte
/// limits, and never empty: an item larger than a message goes alone.
fn send(items: &[&Queued], class: SendClass, limits: &Limits) -> Plan {
    let mut seqs = Vec::new();
    let mut bytes = 0;
    for item in items {
        let added = bytes + item.bytes + 2;
        if !seqs.is_empty() && (seqs.len() >= limits.max_items || added > limits.max_bytes) {
            break;
        }
        seqs.push(item.seq);
        bytes = added;
    }
    Plan::Send { seqs, class }
}

/// When a rolling `window` over `classes` next has room below `max`: now if
/// it has room, else when enough of its entries have aged out.
fn room_at(now: u64, log: &[Logged], classes: &[SendClass], window: u64, max: usize) -> u64 {
    let mut times: Vec<u64> = log
        .iter()
        .filter(|entry| classes.contains(&entry.class) && now.saturating_sub(entry.at) < window)
        .map(|entry| entry.at)
        .collect();
    if times.len() < max {
        return now;
    }
    times.sort_unstable();
    // Room returns once all but `max - 1` of them have aged out.
    times[times.len() - max] + window
}

/// Minutes after local midnight at `now`.
fn local_minute(now: u64, offset: i32) -> u32 {
    let local = i64::try_from(now).unwrap_or(i64::MAX) + i64::from(offset);
    u32::try_from(local.rem_euclid(DAY as i64) / 60).unwrap_or(0)
}

/// Whether `now` is within quiet hours, `[start, end)`, possibly across
/// midnight.
pub(crate) fn quiet_now(now: u64, offset: i32, quiet: QuietHours) -> bool {
    let minute = local_minute(now, offset);
    if quiet.start < quiet.end {
        (quiet.start..quiet.end).contains(&minute)
    } else {
        minute >= quiet.start || minute < quiet.end
    }
}

/// The first moment after `now` at which quiet hours end.
fn quiet_end(now: u64, offset: i32, quiet: QuietHours) -> u64 {
    let local = i64::try_from(now).unwrap_or(i64::MAX) + i64::from(offset);
    let into_day = local.rem_euclid(DAY as i64);
    let mut ahead = i64::from(quiet.end) * 60 - into_day;
    if ahead <= 0 {
        ahead += DAY as i64;
    }
    now + u64::try_from(ahead).unwrap_or(0)
}

#[cfg(test)]
mod tests;
