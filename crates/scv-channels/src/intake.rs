//! What the bridge does with one received message, decided without side
//! effects: [`classify`] reads the message, the delivery state, and the live
//! conversations, and the bridge then carries out its [`Verdict`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::state::Senders;
use crate::{
    Inbound, Job, MAX_CLAIMS, MAX_QUEUED_PER_CONVERSATION, MAX_SESSIONS, MediaKind, Message,
    TURN_TIMEOUT, ToolOwner, conversation_key, state,
};

/// A live conversation's job queue as seen by the poller.
pub(crate) struct Conversation {
    pub(crate) jobs: mpsc::UnboundedSender<Job>,
    pub(crate) last_used: Instant,
    /// Set by the conversation while its session has background jobs
    /// running or reports to send: closing it then would cancel them.
    pub(crate) watching: Arc<AtomicBool>,
}

/// What the bridge knows when a message arrives.
pub(crate) struct Intake<'a> {
    /// Delivery state: IDs seen, claims, and replies waiting to be sent.
    pub(crate) state: &'a state::BridgeState,
    /// The conversations running now, by key.
    pub(crate) conversations: &'a HashMap<String, Conversation>,
    /// The account owner, whether or not it holds remote tools.
    pub(crate) owner: Option<&'a str>,
    /// The owner, when the account grants it remote tools.
    pub(crate) tool_owner: Option<&'a ToolOwner>,
    /// Whose messages the account answers.
    pub(crate) senders: Senders,
}

/// A message to answer, who it comes from, and where it belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sender<'m> {
    pub(crate) message: &'m Message,
    /// Its conversation: the sender's direct chat, or the sender within a
    /// group.
    pub(crate) key: String,
    /// The account owner wrote in a direct chat, with or without remote
    /// tools.
    pub(crate) owner_chat: bool,
}

/// What to do with one received message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict<'m> {
    /// Seen before, or nothing to answer: only record its ID as seen.
    Ignore,
    /// Already claimed, or answered and waiting for delivery: nothing to do,
    /// so it never runs a second turn.
    Claimed,
    /// From someone the account does not answer (`senders = "owner"`, and
    /// not the owner): record its ID as seen and nothing more.
    Stranger,
    /// A message to answer while the bridge is at its work limits: answer
    /// with the busy notice instead of a turn.
    Busy(Sender<'m>),
    /// A voice message with no transcript and nothing else, which the model
    /// cannot hear: answer with the voice notice instead of a turn, without
    /// downloading it.
    Unheard(Sender<'m>),
    /// Claim the message and run a turn on its conversation.
    Turn {
        sender: Sender<'m>,
        /// The owner holds remote tools and wrote in a direct chat: the turn
        /// runs with tools and the owner's limits.
        owner: bool,
        /// How long the turn may run.
        limit: Duration,
        /// A conversation to close first to make room for this one.
        evict: Option<String>,
    },
}

/// Decide what to do with `inbound`, given what the bridge knows.
pub(crate) fn classify<'m>(inbound: &'m Inbound, intake: &Intake<'_>) -> Verdict<'m> {
    let Intake {
        state,
        conversations,
        owner,
        tool_owner,
        senders,
    } = *intake;
    let id = inbound.id();
    if state.seen.iter().any(|seen| seen == id) {
        // Keep all IDs encountered in this bounded batch until its cursor
        // commits, including IDs recovered from the preceding run.
        return Verdict::Ignore;
    }
    if state.in_flight.iter().any(|claim| claim.message_id == id)
        || state.pending.iter().any(|pending| pending.message_id == id)
    {
        return Verdict::Claimed;
    }
    let Inbound::Text(message) = inbound else {
        return Verdict::Ignore;
    };
    let sender = message.sender.as_str();
    // An owner-only account without a known owner answers nobody.
    if senders == Senders::Owner && owner != Some(sender) {
        return Verdict::Stranger;
    }
    let direct = message.group.is_none();
    let key = message
        .group
        .as_ref()
        .map_or_else(|| sender.to_owned(), |group| format!("{group}\0{sender}"));
    let tools = direct && tool_owner.is_some_and(|tool_owner| tool_owner.user_id == sender);
    let limit = match tool_owner {
        Some(tool_owner) if tools => tool_owner.turn_timeout,
        _ => TURN_TIMEOUT,
    };
    let from = Sender {
        message,
        key,
        owner_chat: direct && owner == Some(sender),
    };
    // Nothing a turn could use, and nothing that waits for a turn slot.
    if unheard(message) {
        return Verdict::Unheard(from);
    }
    let waiting = |key: &str| {
        state
            .in_flight
            .iter()
            .filter(|claim| conversation_key(&claim.key, &claim.to_user_id) == key)
            .count()
    };
    let known = conversations.contains_key(&from.key);
    // A full session table makes room by closing the least recently used
    // conversation that has nothing waiting and no background work.
    let evict = if known || conversations.len() < MAX_SESSIONS {
        None
    } else {
        evictable(conversations, waiting)
    };
    let room = state.in_flight.len() < MAX_CLAIMS
        && waiting(&from.key) < MAX_QUEUED_PER_CONVERSATION
        && (known || conversations.len() < MAX_SESSIONS || evict.is_some());
    if !room {
        return Verdict::Busy(from);
    }
    Verdict::Turn {
        sender: from,
        owner: tools,
        limit,
        evict,
    }
}

/// Whether `message` is only voice that no transcript turned into text: no
/// text of its own, and every file audio with an empty or missing transcript.
fn unheard(message: &Message) -> bool {
    message.text.trim().is_empty()
        && !message.media.is_empty()
        && message.media.iter().all(|media| {
            media.kind == MediaKind::Audio
                && media
                    .transcript
                    .as_deref()
                    .is_none_or(|transcript| transcript.trim().is_empty())
        })
}

/// The conversation a full session table closes to make room: the least
/// recently used one with no claimed messages waiting and no background
/// work in flight, or none.
fn evictable(
    conversations: &HashMap<String, Conversation>,
    waiting: impl Fn(&str) -> usize,
) -> Option<String> {
    conversations
        .iter()
        .filter(|(key, conversation)| {
            waiting(key) == 0 && !conversation.watching.load(Ordering::Acquire)
        })
        .min_by_key(|(_, conversation)| conversation.last_used)
        .map(|(key, _)| key.clone())
}

#[cfg(test)]
mod tests;
