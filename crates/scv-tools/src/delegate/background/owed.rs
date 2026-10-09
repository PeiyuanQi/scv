//! What a session is owed of reviewed jobs `agent_cancel` stopped: each
//! one's final outcome, once.
//!
//! A cancel fixes a reviewed job's decision at once, but the job's journal
//! goes on until the job really stops, and a journal failure meanwhile is
//! part of its outcome. So the session is owed the outcome as it is once
//! the job has stopped. The cancel's own change delivers it when the job
//! stops before that change is published; otherwise the change says the
//! journal is still open and a `background.updated` follows with the final
//! outcome. An obligation ends only when a client was actually sent the
//! final outcome; a send that failed, or a change that never went out,
//! leaves it to an update. Obligations live apart from the job list, so
//! pruning finished jobs never drops one.

use scv_protocol::{JobChange, JobOutcome};

/// One session's obligations, oldest first.
#[derive(Debug, Default)]
pub(super) struct Owed {
    owed: Vec<Owing>,
}

/// The final outcome of one cancelled reviewed job, and how it is to reach
/// the session.
#[derive(Debug)]
struct Owing {
    job: String,
    /// The outcome while the job is still stopping, its journal open.
    pending: Option<JobOutcome>,
    /// The final outcome, once the job has stopped.
    last: Option<JobOutcome>,
    via: Via,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Via {
    /// `agent_cancel`, the call named, is still waiting for the job.
    Waiting(String),
    /// The change that call made, not yet taken for publication.
    Change(String),
    /// That change is being sent, with the final outcome when `last`.
    Sending { call: String, last: bool },
    /// A `background.updated`, once the job has stopped.
    Update,
    /// That update is being sent.
    Updating,
}

impl Owed {
    fn find(&mut self, job: &str, via: &Via) -> Option<&mut Owing> {
        self.owed
            .iter_mut()
            .find(|owing| owing.job == job && &owing.via == via)
    }

    /// The jobs whose final outcome is owed, until it was sent.
    pub(super) fn jobs(&self) -> impl Iterator<Item = &str> {
        self.owed.iter().map(|owing| owing.job.as_str())
    }

    /// The call `call` is cancelling reviewed job `job`, and waits for it.
    pub(super) fn cancelling(&mut self, job: &str, call: &str) {
        self.owed.push(Owing {
            job: job.to_owned(),
            pending: None,
            last: None,
            via: Via::Waiting(call.to_owned()),
        });
    }

    /// The call `call` finished waiting for `job`, whose outcome is now
    /// `pending` if it is still stopping. What its change should say.
    pub(super) fn waited(
        &mut self,
        job: &str,
        call: &str,
        pending: Option<JobOutcome>,
    ) -> Option<JobOutcome> {
        let owing = self.find(job, &Via::Waiting(call.to_owned()))?;
        owing.pending = pending;
        owing.via = Via::Change(call.to_owned());
        owing.last.clone().or_else(|| owing.pending.clone())
    }

    /// The call `call` stopped waiting before it could say anything, as when
    /// its turn is aborted: its change never goes out, and an update owes
    /// the outcome instead. Whether one is due now.
    pub(super) fn abandoned(&mut self, job: &str, call: &str) -> bool {
        self.find(job, &Via::Waiting(call.to_owned()))
            .is_some_and(|owing| {
                owing.via = Via::Update;
                owing.last.is_some()
            })
    }

    /// Job `job` stopped with `outcome`. Whether an update is due now.
    pub(super) fn stopped(&mut self, job: &str, outcome: &JobOutcome) -> bool {
        let mut due = false;
        for owing in self.owed.iter_mut().filter(|owing| owing.job == job) {
            owing.last = Some(outcome.clone());
            due |= owing.via == Via::Update;
        }
        due
    }

    /// `change`, made by the call `call`, is taken to be sent: it says the
    /// final outcome if the job has stopped, so a superseded snapshot never
    /// goes out, and otherwise that the journal is still open.
    pub(super) fn take(&mut self, call: &str, change: &mut JobChange) {
        if let Some(owing) = self.find(&change.job, &Via::Change(call.to_owned())) {
            let last = owing.last.is_some();
            change.outcome = owing.last.clone().or_else(|| owing.pending.clone());
            owing.via = Via::Sending {
                call: call.to_owned(),
                last,
            };
        }
    }

    /// The changes the call `call` made reached the client, or failed to,
    /// when `delivered` is false. One that delivered the final outcome ends
    /// its obligation; any other leaves it to an update. Whether one is due
    /// now.
    pub(super) fn sent(&mut self, call: &str, delivered: bool) -> bool {
        let mut due = false;
        self.owed.retain_mut(|owing| match &owing.via {
            Via::Sending {
                call: sending,
                last,
            } if sending == call => {
                if delivered && *last {
                    return false;
                }
                owing.via = Via::Update;
                due |= owing.last.is_some();
                true
            }
            _ => true,
        });
        due
    }

    /// The change the call `call` made for `job` was dropped unsent, as the
    /// oldest of too many waiting to be taken: an update owes the outcome.
    pub(super) fn evicted(&mut self, call: &str, job: &str) {
        if let Some(owing) = self.find(job, &Via::Change(call.to_owned())) {
            owing.via = Via::Update;
        }
    }

    /// A turn ended, so no change of its calls can still be sent: any left
    /// unsent, as when its task was aborted, is owed by an update. Whether
    /// an update is due now.
    pub(super) fn turn_ended(&mut self) -> bool {
        let mut due = false;
        for owing in &mut self.owed {
            if matches!(owing.via, Via::Change(_) | Via::Sending { .. }) {
                owing.via = Via::Update;
            }
            due |= owing.via == Via::Update && owing.last.is_some();
        }
        due
    }

    /// The final outcomes due in a `background.updated`, each now being
    /// sent until [`Self::updated`] says how that went.
    pub(super) fn take_updates(&mut self) -> Vec<JobOutcome> {
        let mut due = Vec::new();
        for owing in &mut self.owed {
            if owing.via == Via::Update
                && let Some(last) = &owing.last
            {
                due.push(last.clone());
                owing.via = Via::Updating;
            }
        }
        due
    }

    /// The update naming `jobs` reached the client, or failed to.
    pub(super) fn updated(&mut self, jobs: &[String], delivered: bool) {
        self.owed.retain_mut(|owing| {
            if owing.via != Via::Updating || !jobs.contains(&owing.job) {
                return true;
            }
            if delivered {
                return false;
            }
            owing.via = Via::Update;
            true
        });
    }

    /// For tests: owe job `change.job`'s outcome through the change the
    /// call `call` made, already waited for; `last` once it has stopped.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn owe(&mut self, call: &str, change: &JobChange, last: Option<JobOutcome>) {
        self.owed.push(Owing {
            job: change.job.clone(),
            pending: change.outcome.clone(),
            last,
            via: Via::Change(call.to_owned()),
        });
    }
}

#[cfg(test)]
mod tests;
