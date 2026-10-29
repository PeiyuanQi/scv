//! Each delegated conversation's lane: the `agent` calls that continue it,
//! in the order they arrived. A call takes its place when it arrives,
//! before anything runs, and keeps it until its turn ends. Only the first
//! call in a lane runs a turn, so queued prompts reach the agent in order
//! and a later call never overtakes one already waiting. The one exception
//! is the owner's interrupting takeover, which is reserved right behind the
//! running turn ([`Lanes::reserve_takeover`]) so that it, and nothing
//! queued, comes next when that turn is stopped.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use scv_core::ToolError;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::sync::lock;

/// One session's lanes, by conversation handle.
#[derive(Debug, Default)]
pub(in crate::delegate) struct Lanes {
    state: Mutex<State>,
    /// Woken whenever a call leaves a lane.
    changed: Notify,
}

#[derive(Debug, Default)]
struct State {
    next: u64,
    /// Each lane's places, first one first. An empty lane is removed.
    lanes: HashMap<String, VecDeque<u64>>,
    /// The places the owner's takeovers of conversations hold.
    takeovers: HashSet<u64>,
    /// The background job each place's call runs as, when it is one.
    jobs: HashMap<u64, String>,
}

/// One call's place in its conversation's lane, given up when dropped.
#[derive(Debug)]
pub(in crate::delegate) struct Place {
    lanes: Arc<Lanes>,
    handle: String,
    id: u64,
    /// Calls that were in the lane when this one joined.
    pub(in crate::delegate) ahead: usize,
}

/// The first call in a conversation's lane: the one running its turn, or
/// about to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::delegate) enum Front {
    /// The owner's takeover holds the lane.
    Takeover,
    /// A background job's call, whose turn the owner can stop while it
    /// runs.
    Job(String),
    /// A call that is no background job: a model's call in the foreground,
    /// or a reviewed job's own turn. Only its session stops it.
    Call,
}

/// Why [`Lanes::reserve_takeover`] reserved nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::delegate) enum Unreserved {
    /// A takeover holds the lane, or waits in it already.
    TakenOver,
    /// The call ahead is no background job, so it cannot be stopped here.
    NotAJob,
}

impl Lanes {
    /// Join the end of `handle`'s lane.
    pub(in crate::delegate) fn join(self: &Arc<Self>, handle: &str) -> Place {
        self.join_as(handle, false)
    }

    /// Join the end of `handle`'s lane for the owner's takeover of it.
    pub(in crate::delegate) fn join_takeover(self: &Arc<Self>, handle: &str) -> Place {
        self.join_as(handle, true)
    }

    /// Whether the first place in `handle`'s lane is a takeover's.
    pub(in crate::delegate) fn held_by_takeover(&self, handle: &str) -> bool {
        self.front(handle) == Some(Front::Takeover)
    }

    /// Whether a takeover holds `handle`'s lane or is reserved in it, so a
    /// prompt sent now waits for the takeover rather than steering a turn
    /// that is about to be stopped.
    pub(in crate::delegate) fn has_takeover(&self, handle: &str) -> bool {
        let state = lock(&self.state);
        state
            .lanes
            .get(handle)
            .is_some_and(|lane| lane.iter().any(|id| state.takeovers.contains(id)))
    }

    /// What the first call in `handle`'s lane is; `None` for an empty lane.
    pub(in crate::delegate) fn front(&self, handle: &str) -> Option<Front> {
        let state = lock(&self.state);
        let id = state.lanes.get(handle).and_then(VecDeque::front)?;
        Some(state.front(*id))
    }

    /// Reserve the place right behind `handle`'s first call for the owner's
    /// takeover, ahead of every call queued after it, or the first place
    /// when the lane is empty: when that call's turn ends, the takeover is
    /// first, so nothing else starts a turn in between. Returns the place
    /// and the background job whose call is ahead of it, which the caller
    /// stops once it has seen that job's turn run. Nothing is reserved when
    /// a takeover holds or waits in the lane already, or the call ahead is
    /// no background job.
    pub(in crate::delegate) fn reserve_takeover(
        self: &Arc<Self>,
        handle: &str,
    ) -> Result<(Place, Option<String>), Unreserved> {
        let mut state = lock(&self.state);
        let lane = state.lanes.get(handle);
        if lane.is_some_and(|lane| lane.iter().any(|id| state.takeovers.contains(id))) {
            return Err(Unreserved::TakenOver);
        }
        let ahead = match lane.and_then(VecDeque::front) {
            None => None,
            Some(id) => match state.front(*id) {
                Front::Job(job) => Some(job),
                Front::Takeover => return Err(Unreserved::TakenOver),
                Front::Call => return Err(Unreserved::NotAJob),
            },
        };
        state.next += 1;
        let id = state.next;
        let lane = state.lanes.entry(handle.to_owned()).or_default();
        let at = lane.len().min(1);
        lane.insert(at, id);
        state.takeovers.insert(id);
        Ok((
            Place {
                lanes: Arc::clone(self),
                handle: handle.to_owned(),
                id,
                ahead: at,
            },
            ahead,
        ))
    }

    fn join_as(self: &Arc<Self>, handle: &str, takeover: bool) -> Place {
        let mut state = lock(&self.state);
        state.next += 1;
        let id = state.next;
        let lane = state.lanes.entry(handle.to_owned()).or_default();
        let ahead = lane.len();
        lane.push_back(id);
        if takeover {
            state.takeovers.insert(id);
        }
        Place {
            lanes: Arc::clone(self),
            handle: handle.to_owned(),
            id,
            ahead,
        }
    }
}

impl State {
    fn front(&self, id: u64) -> Front {
        if self.takeovers.contains(&id) {
            Front::Takeover
        } else if let Some(job) = self.jobs.get(&id) {
            Front::Job(job.clone())
        } else {
            Front::Call
        }
    }
}

impl Place {
    /// The conversation this place is in.
    pub(in crate::delegate) fn handle(&self) -> &str {
        &self.handle
    }

    /// The call holding this place runs as background job `job`, which the
    /// owner can stop to take the conversation over.
    pub(in crate::delegate) fn run_as(&self, job: &str) {
        lock(&self.lanes.state).jobs.insert(self.id, job.to_owned());
    }

    fn is_first(&self) -> bool {
        lock(&self.lanes.state)
            .lanes
            .get(&self.handle)
            .and_then(VecDeque::front)
            == Some(&self.id)
    }

    /// Wait until every call ahead of this one has left the lane.
    pub(in crate::delegate) async fn wait_first(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        loop {
            // Registered before the check, so a call leaving in between
            // still wakes this one.
            let changed = self.lanes.changed.notified();
            if self.is_first() {
                return Ok(());
            }
            tokio::select! {
                () = changed => {}
                () = cancellation.cancelled() => {
                    return Err(ToolError::cancelled(format!(
                        "cancelled while waiting for conversation {}",
                        self.handle
                    )));
                }
            }
        }
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut state = lock(&self.lanes.state);
        if let Some(lane) = state.lanes.get_mut(&self.handle) {
            lane.retain(|id| *id != self.id);
            if lane.is_empty() {
                state.lanes.remove(&self.handle);
            }
        }
        state.takeovers.remove(&self.id);
        state.jobs.remove(&self.id);
        drop(state);
        self.lanes.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
