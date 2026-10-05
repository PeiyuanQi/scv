//! Each delegated conversation's lane: the `agent` calls that continue it,
//! in the order they arrived. A call takes its place when it arrives,
//! before anything runs, and keeps it until its turn ends. Only the first
//! call in a lane runs a turn, so queued prompts reach the agent in order
//! and a later call never overtakes one already waiting.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use scv_core::ToolError;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::sync::lock;

/// One session's lanes, by conversation handle.
#[derive(Debug, Default)]
pub(super) struct Lanes {
    state: Mutex<State>,
    /// Woken whenever a call leaves a lane.
    changed: Notify,
}

#[derive(Debug, Default)]
struct State {
    next: u64,
    /// Each lane's places, first one first. An empty lane is removed.
    lanes: HashMap<String, VecDeque<u64>>,
}

/// One call's place in its conversation's lane, given up when dropped.
#[derive(Debug)]
pub(super) struct Place {
    lanes: Arc<Lanes>,
    handle: String,
    id: u64,
    /// Calls that were in the lane when this one joined.
    pub(super) ahead: usize,
}

impl Lanes {
    /// Join the end of `handle`'s lane.
    pub(super) fn join(self: &Arc<Self>, handle: &str) -> Place {
        let mut state = lock(&self.state);
        state.next += 1;
        let id = state.next;
        let lane = state.lanes.entry(handle.to_owned()).or_default();
        let ahead = lane.len();
        lane.push_back(id);
        Place {
            lanes: Arc::clone(self),
            handle: handle.to_owned(),
            id,
            ahead,
        }
    }
}

impl Place {
    /// The conversation this place is in.
    pub(super) fn handle(&self) -> &str {
        &self.handle
    }

    fn is_first(&self) -> bool {
        lock(&self.lanes.state)
            .lanes
            .get(&self.handle)
            .and_then(VecDeque::front)
            == Some(&self.id)
    }

    /// Wait until every call ahead of this one has left the lane.
    pub(super) async fn wait_first(
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
        drop(state);
        self.lanes.changed.notify_waiters();
    }
}
