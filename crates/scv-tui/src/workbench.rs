//! The dashboard's copy of the daemon's workbench: the latest snapshot, the
//! sidebar built from it, the notifications its changes raise, and each
//! open agent view's live log. The daemon stays authoritative; this only
//! displays what it reports and remembers how far each log was read.

use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use scv_protocol::{
    AgentState, AttachOffer, AttachState, DaemonCommand, JobOrigin, RestorableSession, RunEvent,
    RunEventKind, RunLogPage, WorkbenchApproval, WorkbenchJob, WorkbenchRun, WorkbenchSnapshot,
    WorktreeInfo,
};

use crate::{
    attach::{Attached, DETACH_KEY},
    poller::{Answer, Request, Tag},
    transcript::{BoundedLog, Measure, bounded_text},
};

/// How often the snapshot is asked for.
const SNAPSHOT_EVERY: Duration = Duration::from_millis(1000);
/// How often an open agent view's logs are read.
const LOG_EVERY: Duration = Duration::from_millis(400);
/// How often a daemon without the workbench is asked again, since it may be
/// restarted into a release with it.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(30);
/// Notifications kept.
const MAX_NOTICES: usize = 100;
/// How long the newest notification stays in the status line.
pub(crate) const NOTICE_SHOWN: Duration = Duration::from_secs(10);
/// Lines an agent view keeps, and their bytes.
const VIEW_LINES: usize = 5000;
const VIEW_BYTES: usize = 4 * 1024 * 1024;
/// Finished jobs the sidebar lists per workspace.
const FINISHED_SHOWN: usize = 8;

/// Whether the daemon offers the workbench.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    /// Not asked yet.
    Unknown,
    Ready,
    /// The daemon predates it.
    Unsupported,
    /// The dashboard's connection is down; it connects again.
    Disconnected,
}

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) text: String,
    pub(crate) urgent: bool,
    pub(crate) at: Instant,
}

/// What an agent view follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Target {
    /// A background job, and the conversation it runs a turn of.
    Job { session: String, job: String },
    /// A delegated conversation.
    Conversation {
        session: String,
        conversation: String,
    },
    /// One run.
    Run { handle: String },
}

impl Measure for RunEvent {
    fn size(&self) -> usize {
        self.text.len() + 32
    }
}

/// One open agent view: the logs of the runs it follows, and its composer.
pub(crate) struct AgentView {
    pub(crate) target: Target,
    /// Each run's read position, and whether its log ended.
    pub(crate) cursors: BTreeMap<String, (u64, bool)>,
    pub(crate) lines: BoundedLog<RunEvent>,
    pub(crate) input: String,
    /// Cursor position in `input`, in characters.
    pub(crate) cursor: usize,
    pub(crate) scroll: u16,
    pub(crate) follow: bool,
    /// The run whose events the last line came from.
    last_run: Option<String>,
    /// A cancel waiting to be pressed again.
    pub(crate) confirm_cancel: Option<Instant>,
    /// The prompt sent last and not answered yet, which comes back to the
    /// composer if it is refused or lost.
    pub(crate) sent: Option<String>,
    /// The approval request last shown for this view. A later request on
    /// the same job does not take the first Ctrl+Y or Ctrl+N.
    pub(crate) shown_approval: Option<String>,
    /// The agent's terminal while the view is attached to its takeover.
    pub(crate) terminal: Option<Attached>,
    /// An attach was asked for and not answered yet.
    pub(crate) attaching: bool,
    /// The size the view was last drawn at, which a terminal it attaches
    /// to takes.
    pub(crate) drawn: Cell<(u16, u16)>,
    /// An end of the takeover waiting to be pressed again.
    pub(crate) confirm_end: Option<Instant>,
    /// A takeover that stops the named job's running turn, waiting to be
    /// pressed again.
    pub(crate) confirm_interrupt: Option<(Option<String>, Instant)>,
}

impl AgentView {
    fn new(target: Target) -> Self {
        Self {
            target,
            cursors: BTreeMap::new(),
            lines: BoundedLog::new(VIEW_LINES, VIEW_BYTES),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow: true,
            last_run: None,
            confirm_cancel: None,
            sent: None,
            shown_approval: None,
            terminal: None,
            attaching: false,
            drawn: Cell::new((0, 0)),
            confirm_end: None,
            confirm_interrupt: None,
        }
    }

    /// The sent prompt did not arrive. It comes back into the composer.
    /// Text the owner typed while it was in flight stays after it, so
    /// neither line is dropped.
    fn unsend(&mut self) {
        let Some(prompt) = self.sent.take() else {
            return;
        };
        if self.input.is_empty() {
            self.cursor = prompt.chars().count();
            self.input = prompt;
            return;
        }
        let typed = std::mem::take(&mut self.input);
        self.input = prompt;
        self.input.push('\n');
        self.input.push_str(&typed);
        self.cursor = self.input.chars().count();
    }
}

/// A sidebar entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Item {
    /// A group's title; not selectable.
    Header(String),
    /// This TUI's chat tab, by tab ID, with its line.
    Tab(u64, String),
    Approval(WorkbenchApproval),
    Job(WorkbenchJob),
    /// A delegated run serving no background job (a foreground call).
    Run(WorkbenchRun),
    Restorable(RestorableSession),
}

impl Item {
    pub(crate) fn selectable(&self) -> bool {
        !matches!(self, Self::Header(_))
    }

    /// What the entry is, whatever row it is on: the sidebar's selection
    /// and its confirmations follow it as the list changes.
    pub(crate) fn key(&self) -> Option<String> {
        match self {
            Self::Header(_) => None,
            Self::Tab(id, _) => Some(format!("tab:{id}")),
            Self::Approval(approval) => Some(format!("approval:{}", approval.id)),
            Self::Job(job) => Some(format!("job:{}/{}", job.session, job.job)),
            Self::Run(run) => Some(format!("run:{}", run.handle)),
            Self::Restorable(session) => Some(format!("queue:{}", session.id)),
        }
    }
}

/// The dashboard's workbench state.
pub(crate) struct Workbench {
    pub(crate) availability: Availability,
    pub(crate) snapshot: Option<WorkbenchSnapshot>,
    pub(crate) notices: VecDeque<Notice>,
    /// Ring the terminal bell once after the next draw.
    pub(crate) bell: bool,
    pub(crate) views: HashMap<Target, AgentView>,
    pub(crate) worktrees: Option<Vec<WorktreeInfo>>,
    /// The sidebar's selected entry ([`Item::key`]), and the row it was on.
    selected: Option<(String, usize)>,
    last_snapshot: Option<Instant>,
    snapshot_pending: bool,
    /// Log pages asked for, by run, with the position each was read from.
    logs_pending: HashMap<String, u64>,
    last_logs: Option<Instant>,
}

impl Default for Workbench {
    fn default() -> Self {
        Self {
            availability: Availability::Unknown,
            snapshot: None,
            notices: VecDeque::new(),
            bell: false,
            views: HashMap::new(),
            worktrees: None,
            selected: None,
            last_snapshot: None,
            snapshot_pending: false,
            logs_pending: HashMap::new(),
            last_logs: None,
        }
    }
}

impl Workbench {
    /// The requests due now: the snapshot every second, and each open
    /// view's unread logs, never two of one kind at once.
    pub(crate) fn due(&mut self, now: Instant) -> Vec<Request> {
        let mut requests = Vec::new();
        let every = if self.availability == Availability::Unsupported {
            UNSUPPORTED_RETRY
        } else {
            SNAPSHOT_EVERY
        };
        if !self.snapshot_pending
            && self
                .last_snapshot
                .is_none_or(|last| now.duration_since(last) >= every)
        {
            self.snapshot_pending = true;
            self.last_snapshot = Some(now);
            requests.push(Request {
                tag: Tag::Snapshot,
                command: DaemonCommand::Workbench { watch: true },
            });
        }
        // Logs wait until the snapshot says the workbench answers again.
        if self.availability != Availability::Ready && self.availability != Availability::Unknown {
            return requests;
        }
        if self
            .last_logs
            .is_none_or(|last| now.duration_since(last) >= LOG_EVERY)
        {
            self.last_logs = Some(now);
            let runs = self.runs_of_views();
            for (handle, after) in runs {
                if !self.logs_pending.contains_key(&handle) {
                    self.logs_pending.insert(handle.clone(), after);
                    requests.push(Request {
                        tag: Tag::Log(handle.clone(), after),
                        command: DaemonCommand::RunLog {
                            handle,
                            after,
                            limit: None,
                        },
                    });
                }
            }
        }
        requests
    }

    /// Every run an open view follows that has more to read, with where to
    /// read from.
    fn runs_of_views(&mut self) -> Vec<(String, u64)> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for view in self.views.values_mut() {
            for run in runs_of(snapshot, &view.target) {
                view.cursors.entry(run.handle.clone()).or_insert((0, false));
            }
            for (handle, (next, ended)) in &view.cursors {
                if !ended {
                    found.push((handle.clone(), *next));
                }
            }
        }
        found.sort();
        found.dedup_by(|a, b| a.0 == b.0);
        found
    }

    /// Take in an answer from the dashboard's connection.
    pub(crate) fn answer(&mut self, answer: Answer) -> Option<String> {
        if let Answer::Status(tag, _)
        | Answer::Refused(tag, _)
        | Answer::Disconnected(tag)
        | Answer::TimedOut(tag) = &answer
            && matches!(tag, Tag::Attach(_) | Tag::AttachIo(_) | Tag::AttachDone)
        {
            return self.terminal_answer(answer);
        }
        match answer {
            Answer::Status(tag, status) => {
                self.availability = Availability::Ready;
                match tag {
                    Tag::Snapshot => {
                        self.snapshot_pending = false;
                        if let Some(snapshot) = status.workbench {
                            self.take_snapshot(snapshot);
                        }
                        None
                    }
                    Tag::Log(handle, after) => {
                        self.log_answered(&handle, after);
                        if let Some(page) = status.run_log {
                            self.take_page(after, page);
                        }
                        None
                    }
                    Tag::Worktrees => {
                        self.worktrees = Some(status.worktrees);
                        status.workbench_note
                    }
                    Tag::Action | Tag::Send(_) => {
                        if let Tag::Send(target) = &tag
                            && let Some(view) = self.views.get_mut(target)
                        {
                            view.sent = None;
                        }
                        if let Some(snapshot) = status.workbench {
                            self.take_snapshot(snapshot);
                        }
                        status.workbench_note
                    }
                    // Taken in by `terminal_answer`.
                    Tag::Attach(_) | Tag::AttachIo(_) | Tag::AttachDone => None,
                }
            }
            Answer::Refused(tag, message) => {
                match &tag {
                    Tag::Snapshot => self.snapshot_pending = false,
                    Tag::Log(handle, after) => {
                        self.log_answered(handle, *after);
                        // A log that is gone is read no more.
                        for view in self.views.values_mut() {
                            if let Some(cursor) = view.cursors.get_mut(handle) {
                                cursor.1 = true;
                            }
                        }
                    }
                    Tag::Send(target) => {
                        if let Some(view) = self.views.get_mut(target) {
                            view.unsend();
                        }
                    }
                    Tag::Action | Tag::Worktrees => {}
                    Tag::Attach(_) | Tag::AttachIo(_) | Tag::AttachDone => {}
                }
                matches!(tag, Tag::Action | Tag::Worktrees | Tag::Send(_)).then_some(message)
            }
            Answer::Unsupported => {
                let first = self.availability != Availability::Unsupported;
                self.availability = Availability::Unsupported;
                self.snapshot_pending = false;
                self.logs_pending.clear();
                first.then(|| {
                    "This daemon has no workbench; restart it into this release to use the \
                     dashboard (the TUI checks again every 30 seconds)."
                        .into()
                })
            }
            Answer::Disconnected(tag) => {
                self.availability = Availability::Disconnected;
                self.snapshot_pending = false;
                self.logs_pending.clear();
                match tag {
                    Tag::Send(target) => {
                        if let Some(view) = self.views.get_mut(&target) {
                            view.unsend();
                        }
                        Some(
                            "The prompt may not have reached the daemon (its connection broke); \
                             it is back in the composer."
                                .into(),
                        )
                    }
                    Tag::Action | Tag::Worktrees => Some(
                        "The daemon did not answer (its connection broke); check whether that \
                         took effect."
                            .into(),
                    ),
                    Tag::Snapshot | Tag::Log(..) => None,
                    Tag::Attach(_) | Tag::AttachIo(_) | Tag::AttachDone => None,
                }
            }
            Answer::TimedOut(tag) => {
                match &tag {
                    Tag::Snapshot => {
                        self.snapshot_pending = false;
                        // A snapshot that does not return is the daemon not
                        // answering. A slow action is not.
                        self.availability = Availability::Disconnected;
                    }
                    Tag::Log(handle, after) => self.log_answered(handle, *after),
                    Tag::Send(target) => {
                        if let Some(view) = self.views.get_mut(target) {
                            view.unsend();
                        }
                    }
                    Tag::Action | Tag::Worktrees => {}
                    Tag::Attach(_) | Tag::AttachIo(_) | Tag::AttachDone => {}
                }
                match tag {
                    Tag::Send(_) => Some(
                        "Sending the prompt timed out. It may have reached the daemon; it is \
                         back in the composer, so check the conversation before sending it again."
                            .into(),
                    ),
                    Tag::Action | Tag::Worktrees => Some(
                        "The daemon did not answer in time; check whether that took effect.".into(),
                    ),
                    Tag::Snapshot | Tag::Log(..) => None,
                    Tag::Attach(_) | Tag::AttachIo(_) | Tag::AttachDone => None,
                }
            }
        }
    }

    /// Take in an answer to an attach request, from the dashboard's
    /// terminal connection. A failure of the terminal falls back to the
    /// activity view, and says why.
    pub(crate) fn terminal_answer(&mut self, answer: Answer) -> Option<String> {
        match answer {
            Answer::Status(Tag::Attach(target), status) => {
                // A view closed meanwhile lets its hold lapse.
                let view = self.views.get_mut(&target)?;
                view.attaching = false;
                let page = status.attach?;
                let conversation = page.conversation.clone();
                view.terminal = Some(Attached::new(page));
                // Which job's turn an interrupting attach stopped.
                let stopped = status
                    .workbench_note
                    .map(|note| format!("{note} "))
                    .unwrap_or_default();
                Some(format!(
                    "{stopped}Attached to {conversation}'s terminal: keys go to the agent; \
                     {DETACH_KEY} detaches and leaves it running."
                ))
            }
            Answer::Status(Tag::AttachIo(target), status) => {
                let view = self.views.get_mut(&target)?;
                let attached = view.terminal.as_mut()?;
                let Some(page) = status.attach.filter(|page| page.id == attached.id) else {
                    attached.unanswered();
                    return None;
                };
                let note = page.note.clone();
                let ended = page
                    .ended
                    .then(|| (page.conversation.clone(), page.exit.clone()));
                attached.apply(page);
                if let Some((conversation, exit)) = ended {
                    view.terminal = None;
                    return Some(format!(
                        "{conversation}'s terminal closed ({}); its activity view is back.",
                        exit.as_deref().unwrap_or("closed")
                    ));
                }
                note
            }
            Answer::Status(_, status) => status.workbench_note,
            Answer::Refused(tag, message) => {
                match tag {
                    Tag::Attach(target) => {
                        if let Some(view) = self.views.get_mut(&target) {
                            view.attaching = false;
                        }
                    }
                    Tag::AttachIo(target) => {
                        if let Some(view) = self.views.get_mut(&target) {
                            view.terminal = None;
                        }
                    }
                    _ => {}
                }
                Some(message)
            }
            Answer::Unsupported => {
                for view in self.views.values_mut() {
                    view.attaching = false;
                    view.terminal = None;
                }
                Some(
                    "This daemon cannot attach to an agent's terminal (it predates attach); \
                     restart it into this release. The activity view goes on as before."
                        .into(),
                )
            }
            Answer::Disconnected(tag) | Answer::TimedOut(tag) => match tag {
                Tag::Attach(target) => {
                    if let Some(view) = self.views.get_mut(&target) {
                        view.attaching = false;
                    }
                    Some(
                        "The daemon did not answer the attach; a takeover may have started, so \
                         check the sidebar before attaching again."
                            .into(),
                    )
                }
                Tag::AttachIo(target) => self
                    .views
                    .get_mut(&target)
                    .and_then(|view| view.terminal.take())
                    .map(|attached| {
                        format!(
                            "The connection to the daemon broke; detached from {}'s terminal \
                             (the agent may still run there).",
                            attached.conversation
                        )
                    }),
                _ => None,
            },
        }
    }

    /// The reads attached views need now, each with what was typed.
    pub(crate) fn terminal_due(&mut self, now: Instant) -> Vec<Request> {
        self.views
            .iter_mut()
            .filter_map(|(target, view)| {
                let command = view.terminal.as_mut()?.due(now)?;
                Some(Request {
                    tag: Tag::AttachIo(target.clone()),
                    command,
                })
            })
            .collect()
    }

    /// Whether any view is attached to a terminal, which then reads often.
    pub(crate) fn attached(&self) -> bool {
        self.views.values().any(|view| view.terminal.is_some())
    }

    /// The live takeover a view's conversation is in, as its session and
    /// conversation: the newest run the view follows, when it is a
    /// terminal still running.
    pub(crate) fn takeover(&self, target: &Target) -> Option<(String, String)> {
        let snapshot = self.snapshot.as_ref()?;
        let run = runs_of(snapshot, target)
            .into_iter()
            .max_by_key(|run| run.started_unix_seconds)?;
        (run.transport == "terminal" && run.live)
            .then(|| Some((run.session.clone(), run.conversation.clone()?)))
            .flatten()
    }

    /// Whether the conversation a view follows can be taken over now, and
    /// why not, as the daemon last said: from the view's job, else from
    /// any job or run of that conversation. `None` from a daemon that
    /// predates it, or without a conversation.
    pub(crate) fn attach_offer(&self, target: &Target) -> Option<&AttachOffer> {
        if let Some(offer) = self.job(target).and_then(|job| job.attach.as_ref()) {
            return Some(offer);
        }
        let (session, conversation) = self.conversation(target)?;
        let snapshot = self.snapshot.as_ref()?;
        let ours = |of: &str, named: Option<&String>| {
            of == session && named.is_some_and(|named| *named == conversation)
        };
        snapshot
            .jobs
            .iter()
            .filter(|job| ours(&job.session, job.conversation.as_ref()))
            .find_map(|job| job.attach.as_ref())
            .or_else(|| {
                snapshot
                    .runs
                    .iter()
                    .filter(|run| ours(&run.session, run.conversation.as_ref()))
                    .find_map(|run| run.attach.as_ref())
            })
    }

    /// The daemon's last offer to stop the job running `target`'s
    /// conversation's turn and take the conversation over, if it made one.
    pub(crate) fn stoppable(&self, target: &Target) -> Option<&AttachOffer> {
        self.attach_offer(target)
            .filter(|offer| offer.state == AttachState::Busy && offer.interrupt)
    }

    /// The page of `handle`'s log read from `after` came back: that request
    /// is no longer pending, unless a newer one is.
    fn log_answered(&mut self, handle: &str, after: u64) {
        if self.logs_pending.get(handle) == Some(&after) {
            self.logs_pending.remove(handle);
        }
    }

    /// The sidebar row selected among `items`: the selected entry wherever
    /// it is now, else the selectable row nearest where it was.
    pub(crate) fn selection(&self, items: &[Item]) -> Option<usize> {
        if let Some((key, _)) = &self.selected
            && let Some(index) = items
                .iter()
                .position(|item| item.key().as_ref() == Some(key))
        {
            return Some(index);
        }
        let row = self.selected.as_ref().map_or(0, |(_, row)| *row);
        let selectable = || {
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.selectable())
        };
        selectable()
            .find(|(index, _)| *index >= row)
            .or_else(|| selectable().next_back())
            .map(|(index, _)| index)
    }

    /// Remember the entry the sidebar is showing when the owner has not
    /// moved the selection. The default highlight is otherwise untracked, so
    /// a later key acts on whatever moved into that row.
    pub(crate) fn note_highlight(&mut self, items: &[Item]) {
        if self.selected.is_some() {
            return;
        }
        if let Some(index) = self.selection(items) {
            self.select(items, index);
        }
    }

    /// Whether the selected entry is no longer among `items`, so
    /// [`Self::selection`] fell back to another entry.
    pub(crate) fn selection_gone(&self, items: &[Item]) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|(key, _)| !items.iter().any(|item| item.key().as_ref() == Some(key)))
    }

    /// Select row `index` of `items`.
    pub(crate) fn select(&mut self, items: &[Item], index: usize) {
        self.selected = items.get(index).and_then(Item::key).map(|key| (key, index));
    }

    fn take_snapshot(&mut self, snapshot: WorkbenchSnapshot) {
        if let Some(previous) = &self.snapshot {
            let notices = changes(previous, &snapshot);
            for (text, urgent) in notices {
                self.notify(text, urgent);
            }
        }
        self.snapshot = Some(snapshot);
        self.note_shown_approvals();
        self.drop_stale_interrupts();
    }

    /// Forget a first Ctrl+O whose job the new snapshot no longer offers to
    /// stop, even for a moment, so a turn stoppable again takes two presses.
    fn drop_stale_interrupts(&mut self) {
        let stale: Vec<Target> = self
            .views
            .iter()
            .filter(|(target, view)| {
                view.confirm_interrupt.as_ref().is_some_and(|(asked, _)| {
                    self.stoppable(target)
                        .is_none_or(|offer| offer.job != *asked)
                })
            })
            .map(|(target, _)| target.clone())
            .collect();
        for target in stale {
            if let Some(view) = self.views.get_mut(&target) {
                view.confirm_interrupt = None;
            }
        }
    }

    /// Remember the approval each open view is showing, without replacing
    /// one the owner has not answered yet. The next Ctrl+Y or Ctrl+N then
    /// names the new request instead of approving it.
    fn note_shown_approvals(&mut self) {
        let noted: Vec<(Target, Option<String>)> = self
            .views
            .keys()
            .cloned()
            .map(|target| {
                let id = self.approval(&target).map(|approval| approval.id.clone());
                (target, id)
            })
            .collect();
        for (target, id) in noted {
            let Some(view) = self.views.get_mut(&target) else {
                continue;
            };
            match (&view.shown_approval, id) {
                (None, Some(id)) => view.shown_approval = Some(id),
                (Some(_), None) => view.shown_approval = None,
                // A different request stays unnoted until the owner presses
                // Ctrl+Y or Ctrl+N, which then names it.
                _ => {}
            }
        }
    }

    /// Take in a page of a run's log read from position `after`: each view
    /// that had read the run up to there gets its events.
    fn take_page(&mut self, after: u64, page: RunLogPage) {
        for view in self.views.values_mut() {
            let Some(cursor) = view.cursors.get_mut(&page.handle) else {
                continue;
            };
            if cursor.0 != after {
                continue;
            }
            cursor.0 = page.next;
            cursor.1 = page.ended && !page.more;
            if !page.events.is_empty() && view.last_run.as_deref() != Some(page.handle.as_str()) {
                if view.last_run.is_some() {
                    view.lines.push(RunEvent {
                        seq: 0,
                        ts_ms: 0,
                        kind: RunEventKind::Note,
                        text: format!("— run {} —", page.handle),
                        turn: None,
                        status: None,
                        job: None,
                        risk: None,
                        by: None,
                    });
                }
                view.last_run = Some(page.handle.clone());
            }
            for event in &page.events {
                view.lines.push(event.clone());
            }
        }
    }

    /// Add a notification; an urgent one rings the bell.
    pub(crate) fn notify(&mut self, text: String, urgent: bool) {
        self.bell |= urgent;
        self.notices.push_back(Notice {
            text,
            urgent,
            at: Instant::now(),
        });
        while self.notices.len() > MAX_NOTICES {
            self.notices.pop_front();
        }
    }

    /// The newest notification, while it is fresh.
    pub(crate) fn latest_notice(&self, now: Instant) -> Option<&Notice> {
        self.notices
            .back()
            .filter(|notice| now.duration_since(notice.at) < NOTICE_SHOWN)
    }

    /// Open, or return, the view following `target`.
    pub(crate) fn open(&mut self, target: Target) -> &mut AgentView {
        self.views
            .entry(target.clone())
            .or_insert_with(|| AgentView::new(target))
    }

    /// The job a view's target names.
    pub(crate) fn job(&self, target: &Target) -> Option<&WorkbenchJob> {
        let snapshot = self.snapshot.as_ref()?;
        match target {
            Target::Job { session, job } => snapshot
                .jobs
                .iter()
                .find(|candidate| &candidate.session == session && &candidate.job == job),
            _ => None,
        }
    }

    /// The conversation a view's input goes to: its session and handle.
    pub(crate) fn conversation(&self, target: &Target) -> Option<(String, String)> {
        let snapshot = self.snapshot.as_ref();
        match target {
            Target::Conversation {
                session,
                conversation,
            } => Some((session.clone(), conversation.clone())),
            Target::Job { session, .. } => {
                let job = self.job(target)?;
                job.conversation
                    .clone()
                    .map(|conversation| (session.clone(), conversation))
            }
            Target::Run { handle } => snapshot?
                .runs
                .iter()
                .find(|run| &run.handle == handle)
                .and_then(|run| {
                    run.conversation
                        .clone()
                        .map(|conversation| (run.session.clone(), conversation))
                }),
        }
    }

    /// What a view's target is doing now, and SCV's words for it.
    pub(crate) fn state(&self, target: &Target) -> (AgentState, Option<String>) {
        let Some(snapshot) = &self.snapshot else {
            return (AgentState::Unknown, None);
        };
        if let Some(job) = self.job(target) {
            return (job.state, job.detail.clone());
        }
        runs_of(snapshot, target)
            .into_iter()
            .max_by_key(|run| run.started_unix_seconds)
            .map_or((AgentState::Unknown, None), |run| {
                (run.state, run.detail.clone())
            })
    }

    /// The approval request a view's job waits on.
    pub(crate) fn approval(&self, target: &Target) -> Option<&WorkbenchApproval> {
        let snapshot = self.snapshot.as_ref()?;
        let (session, job) = match target {
            Target::Job { session, job } => (session.clone(), job.clone()),
            other => {
                let run = runs_of(snapshot, other)
                    .into_iter()
                    .max_by_key(|run| run.started_unix_seconds)?;
                (run.session.clone(), run.job.clone()?)
            }
        };
        snapshot
            .approvals
            .iter()
            .find(|approval| approval.session == session && approval.job == job)
    }

    /// The sidebar: approvals waiting, this TUI's tabs, each workspace's
    /// jobs and other runs, and sessions whose queue can be restored.
    pub(crate) fn items(&self, tabs: &[(u64, String)]) -> Vec<Item> {
        let mut items = Vec::new();
        let Some(snapshot) = &self.snapshot else {
            items.push(Item::Header("This TUI".into()));
            items.extend(tabs.iter().map(|(id, line)| Item::Tab(*id, line.clone())));
            return items;
        };
        if !snapshot.approvals.is_empty() {
            items.push(Item::Header(format!(
                "Waiting for you ({})",
                snapshot.approvals.len()
            )));
            items.extend(snapshot.approvals.iter().cloned().map(Item::Approval));
        }
        items.push(Item::Header("This TUI".into()));
        items.extend(tabs.iter().map(|(id, line)| Item::Tab(*id, line.clone())));
        let mut workspaces: BTreeMap<String, Vec<Item>> = BTreeMap::new();
        let mut finished: HashMap<String, usize> = HashMap::new();
        for job in &snapshot.jobs {
            if job.state == AgentState::Interrupted && job.resumable {
                continue;
            }
            if job.state.is_final() {
                let count = finished.entry(job.workspace.clone()).or_default();
                *count += 1;
                if *count > FINISHED_SHOWN {
                    continue;
                }
            }
            workspaces
                .entry(job.workspace.clone())
                .or_default()
                .push(Item::Job(job.clone()));
        }
        // A live run serving no job: a foreground call, or a conversation
        // waiting between turns.
        for run in snapshot
            .runs
            .iter()
            .filter(|run| run.job.is_none() && run.live)
        {
            // Its session's workspace, else that of a job of its session.
            let workspace = snapshot
                .sessions
                .iter()
                .find(|session| session.id == run.session)
                .map(|session| session.workspace.clone())
                .or_else(|| {
                    snapshot
                        .jobs
                        .iter()
                        .find(|job| job.session == run.session)
                        .map(|job| job.workspace.clone())
                })
                .unwrap_or_else(|| run.cwd.clone());
            workspaces
                .entry(workspace)
                .or_default()
                .push(Item::Run(run.clone()));
        }
        for (workspace, entries) in workspaces {
            items.push(Item::Header(short_path(&workspace, 28)));
            items.extend(entries);
        }
        let resumable: Vec<&WorkbenchJob> =
            snapshot.jobs.iter().filter(|job| job.resumable).collect();
        if !resumable.is_empty() {
            items.push(Item::Header(format!("Interrupted ({})", resumable.len())));
            items.extend(resumable.into_iter().cloned().map(Item::Job));
        }
        if !snapshot.restorable.is_empty() {
            items.push(Item::Header("Restorable queues".into()));
            items.extend(snapshot.restorable.iter().cloned().map(Item::Restorable));
        }
        items
    }

    /// Counts for the header: jobs working, waiting for the owner, and
    /// interrupted.
    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        let Some(snapshot) = &self.snapshot else {
            return (0, 0, 0);
        };
        let working = snapshot
            .jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.state,
                    AgentState::Working | AgentState::Starting | AgentState::Queued
                )
            })
            .count();
        let waiting = snapshot
            .jobs
            .iter()
            .filter(|job| job.state == AgentState::AwaitingInput)
            .count();
        let interrupted = snapshot.jobs.iter().filter(|job| job.resumable).count();
        (working, waiting, interrupted)
    }
}

/// The runs a target follows, oldest first.
pub(crate) fn runs_of<'a>(
    snapshot: &'a WorkbenchSnapshot,
    target: &Target,
) -> Vec<&'a WorkbenchRun> {
    let job = match target {
        Target::Job { session, job } => snapshot
            .jobs
            .iter()
            .find(|candidate| &candidate.session == session && &candidate.job == job),
        _ => None,
    };
    let mut runs: Vec<&WorkbenchRun> = snapshot
        .runs
        .iter()
        .filter(|run| match target {
            Target::Run { handle } => &run.handle == handle,
            Target::Conversation {
                session,
                conversation,
            } => &run.session == session && run.conversation.as_ref() == Some(conversation),
            Target::Job { session, job: name } => {
                &run.session == session
                    && (run.job.as_ref() == Some(name)
                        || run.jobs.contains(name)
                        || job.and_then(|job| job.conversation.as_ref()).is_some_and(
                            |conversation| {
                                run.conversation.as_ref() == Some(conversation)
                                    && run.started_unix_seconds
                                        >= job.map_or(0, |job| job.created_unix_seconds)
                            },
                        ))
            }
        })
        .collect();
    runs.sort_by_key(|run| (run.started_unix_seconds, run.handle.clone()));
    runs
}

/// What changed between two snapshots that the owner should hear about.
fn changes(before: &WorkbenchSnapshot, after: &WorkbenchSnapshot) -> Vec<(String, bool)> {
    let mut notices = Vec::new();
    let known: HashSet<&str> = before
        .approvals
        .iter()
        .map(|approval| approval.id.as_str())
        .collect();
    for approval in after
        .approvals
        .iter()
        .filter(|approval| !known.contains(approval.id.as_str()))
    {
        notices.push((
            format!(
                "{} asks for approval ({}): {}",
                approval.job,
                approval.risk,
                bounded_text(&approval.summary, 120)
            ),
            true,
        ));
    }
    let previous: HashMap<(&str, &str), AgentState> = before
        .jobs
        .iter()
        .map(|job| ((job.session.as_str(), job.job.as_str()), job.state))
        .collect();
    for job in &after.jobs {
        // A job first seen already past starting (it began and ended, or
        // asked for input, between two snapshots) is news as well.
        let was = previous
            .get(&(job.session.as_str(), job.job.as_str()))
            .copied()
            .unwrap_or(AgentState::Starting);
        if was == job.state {
            continue;
        }
        let who = format!(
            "{} ({}{})",
            job.job,
            job.agent,
            if job.started_by == JobOrigin::Owner {
                ", yours"
            } else {
                ""
            }
        );
        let detail = job
            .detail
            .as_deref()
            .map(|detail| format!(": {}", bounded_text(detail, 100)))
            .unwrap_or_default();
        match job.state {
            AgentState::AwaitingInput => {
                notices.push((format!("{who} is waiting for input{detail}"), true));
            }
            AgentState::Completed => notices.push((format!("{who} completed"), false)),
            AgentState::Failed | AgentState::Timeout | AgentState::Declined => {
                notices.push((format!("{who} {}{detail}", job.state), true));
            }
            AgentState::Interrupted => notices.push((
                format!(
                    "{who} was interrupted{detail}{}",
                    if job.resumable {
                        "; it can be resumed"
                    } else {
                        ""
                    }
                ),
                true,
            )),
            AgentState::Unknown if was == AgentState::Working => {
                notices.push((format!("{who} has gone quiet{detail}"), false));
            }
            _ => {}
        }
    }
    notices
}

/// A sidebar line's state mark, readable without color.
pub(crate) fn mark(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "◌",
        AgentState::Working => "●",
        AgentState::AwaitingInput => "?",
        AgentState::Idle => "○",
        AgentState::Queued => "…",
        AgentState::Completed => "✓",
        AgentState::Failed | AgentState::Timeout => "✗",
        AgentState::Declined => "⊘",
        AgentState::Cancelled => "■",
        AgentState::Interrupted => "⟳",
        AgentState::Unknown => "~",
    }
}

/// `value` shortened from the left to `max` characters, with control
/// characters replaced ([`crate::render::clean`]): a directory's name may
/// hold anything.
pub(crate) fn short_path(value: &str, max: usize) -> String {
    let value = crate::render::clean(value);
    if value.chars().count() <= max {
        return value;
    }
    let tail: Vec<char> = value.chars().rev().take(max.saturating_sub(1)).collect();
    let mut short = String::from("…");
    short.extend(tail.into_iter().rev());
    short
}

#[cfg(test)]
mod tests;
