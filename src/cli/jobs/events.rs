//! `scv jobs events`: what changes on the workbench, one line each. The
//! daemon pushes nothing, so this polls the workbench snapshot, reports
//! how it differs from the one before, and tails the activity logs of the
//! runs it lists. Within one poll, log lines come before the state changes,
//! so a job's last activity is printed before it is reported ended.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use scv_client::Layout;
use scv_protocol::{
    AgentState, RunEvent, WorkbenchApproval, WorkbenchJob, WorkbenchRun, WorkbenchSnapshot,
};
use serde::Serialize;

use super::{describe, emit, inert, json_line, short, snapshot};

/// How often `--follow` polls the workbench.
const EVERY: Duration = Duration::from_secs(1);

/// One change on the workbench, as a line of `scv jobs events --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(super) enum Event {
    /// A job not listed before.
    #[serde(rename = "job.added")]
    JobAdded { job: WorkbenchJob },
    /// A listed job's state, run, conversation, worktree, or resumability
    /// changed; `from` is the state it had.
    #[serde(rename = "job.changed")]
    JobChanged { from: AgentState, job: WorkbenchJob },
    /// A job no longer listed: dismissed, pruned, or past the snapshot's
    /// newest finished jobs.
    #[serde(rename = "job.removed")]
    JobRemoved { session: String, job: String },
    /// An approval request began waiting for the owner.
    #[serde(rename = "approval.requested")]
    ApprovalRequested { approval: WorkbenchApproval },
    /// An approval request stopped waiting: answered, or expired.
    #[serde(rename = "approval.resolved")]
    ApprovalResolved {
        id: String,
        session: String,
        job: String,
    },
    /// A new line of a run's activity log.
    #[serde(rename = "run.event")]
    Run {
        run: String,
        session: String,
        event: RunEvent,
    },
    /// The jobs and approvals listed when the command started have all
    /// been printed; everything after is a change.
    #[serde(rename = "ready")]
    Ready { generated_unix_seconds: u64 },
}

pub(super) async fn run(layout: &Layout, follow: bool, json: bool) -> Result<()> {
    let mut seen = Seen::default();
    let mut tails = Tails::new(layout.runs());
    let mut first = true;
    // With `--follow`, one Ctrl-C listener throughout, so a Ctrl-C while a
    // poll runs is not lost.
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    loop {
        let polled = tokio::select! {
            polled = snapshot(layout) => polled,
            _ = &mut stop, if follow => return Ok(()),
        };
        let snapshot = match polled {
            Ok(snapshot) => Some(snapshot),
            Err(error) if !first => {
                eprintln!("scv jobs events: {error:#}; trying again");
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(snapshot) = snapshot {
            let mut events = tails.poll(&snapshot.runs, first);
            events.extend(seen.update(&snapshot));
            if first {
                events.push(Event::Ready {
                    generated_unix_seconds: snapshot.generated_unix_seconds,
                });
                first = false;
            }
            for event in &events {
                let line = if json {
                    json_line(event)?
                } else if let Some(line) = describe_event(event) {
                    line
                } else {
                    continue;
                };
                if !emit(&line)? {
                    return Ok(());
                }
            }
        }
        if !follow {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(EVERY) => {}
            _ = &mut stop => return Ok(()),
        }
    }
}

/// The jobs and approvals the last snapshot listed.
#[derive(Default)]
pub(super) struct Seen {
    jobs: BTreeMap<(String, String), WorkbenchJob>,
    approvals: BTreeMap<String, WorkbenchApproval>,
}

impl Seen {
    /// How `snapshot` differs from the snapshot before, which it replaces:
    /// added jobs oldest first, changed ones, removed ones, then approvals.
    pub(super) fn update(&mut self, snapshot: &WorkbenchSnapshot) -> Vec<Event> {
        let mut events = Vec::new();
        let mut jobs = BTreeMap::new();
        for job in snapshot.jobs.iter().rev() {
            let key = (job.session.clone(), job.job.clone());
            match self.jobs.remove(&key) {
                None => events.push(Event::JobAdded { job: job.clone() }),
                Some(before) if changed(&before, job) => events.push(Event::JobChanged {
                    from: before.state,
                    job: job.clone(),
                }),
                Some(_) => {}
            }
            jobs.insert(key, job.clone());
        }
        for (session, job) in std::mem::replace(&mut self.jobs, jobs).into_keys() {
            events.push(Event::JobRemoved { session, job });
        }
        let mut approvals = BTreeMap::new();
        for approval in &snapshot.approvals {
            if self.approvals.remove(&approval.id).is_none() {
                events.push(Event::ApprovalRequested {
                    approval: approval.clone(),
                });
            }
            approvals.insert(approval.id.clone(), approval.clone());
        }
        for (id, approval) in std::mem::replace(&mut self.approvals, approvals) {
            events.push(Event::ApprovalResolved {
                id,
                session: approval.session,
                job: approval.job,
            });
        }
        events
    }
}

/// Whether a job changed in a way worth a line. Its detail alone does not
/// count: a running job's says how long it has run.
fn changed(before: &WorkbenchJob, now: &WorkbenchJob) -> bool {
    before.state != now.state
        || before.run != now.run
        || before.conversation != now.conversation
        || before.worktree != now.worktree
        || before.resumable != now.resumable
        || before.resumed_as != now.resumed_as
}

/// Where each run's log was read up to.
pub(super) struct Tails {
    dir: PathBuf,
    runs: HashMap<String, Tail>,
}

struct Tail {
    session: String,
    after: u64,
    ended: bool,
}

impl Tails {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            runs: HashMap::new(),
        }
    }

    /// The new lines of the logs of `runs`. On the `first` poll a live run's
    /// log is read from its end, so only new activity is printed, and an
    /// ended run's not at all; a run that appears later is read from its
    /// start.
    pub(super) fn poll(&mut self, runs: &[WorkbenchRun], first: bool) -> Vec<Event> {
        let listed: HashSet<&str> = runs.iter().map(|run| run.handle.as_str()).collect();
        self.runs
            .retain(|handle, _| listed.contains(handle.as_str()));
        let mut events = Vec::new();
        // Oldest first, as the snapshot lists them newest first.
        for run in runs.iter().rev() {
            let dir = &self.dir;
            let tail = self.runs.entry(run.handle.clone()).or_insert_with(|| Tail {
                session: run.session.clone(),
                after: if first {
                    scv_tools::activity::read_page(dir, &run.handle, u64::MAX, 1)
                        .map_or(0, |page| page.next)
                } else {
                    0
                },
                ended: first && !run.live,
            });
            while !tail.ended {
                let Ok(page) = scv_tools::activity::read_page(dir, &run.handle, tail.after, 500)
                else {
                    // A log not written yet is read on the next poll; one a
                    // run that is gone never wrote, never.
                    tail.ended = !run.live;
                    break;
                };
                tail.after = page.next;
                tail.ended = page.ended;
                events.extend(page.events.into_iter().map(|event| Event::Run {
                    run: run.handle.clone(),
                    session: tail.session.clone(),
                    event,
                }));
                if !page.more {
                    break;
                }
            }
        }
        events
    }
}

/// `event` as a line of text, control characters removed; `None` for what
/// only a script needs.
fn describe_event(event: &Event) -> Option<String> {
    let line = match event {
        Event::JobAdded { job } => format!(
            "+ {}/{} {} {}: {}",
            short(&job.session),
            job.job,
            job.agent,
            job.state,
            job.task
        ),
        Event::JobChanged { from, job } => format!(
            "~ {}/{} {from} → {}{}",
            short(&job.session),
            job.job,
            job.state,
            job.detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        ),
        Event::JobRemoved { session, job } => format!("- {}/{job}", short(session)),
        Event::ApprovalRequested { approval } => format!(
            "? {} {}/{} ({}): {}",
            approval.id,
            short(&approval.session),
            approval.job,
            approval.risk,
            approval.summary
        ),
        Event::ApprovalResolved { id, .. } => format!("  {id} answered or expired"),
        Event::Run { run, event, .. } => format!("{run}  {}", describe(event)),
        Event::Ready { .. } => return None,
    };
    Some(inert(&line))
}

#[cfg(test)]
mod tests;
