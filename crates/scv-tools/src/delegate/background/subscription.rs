//! A chat thread's subscription to one background job that already exists.
//!
//! The record is the daemon's, one file under
//! [`Layout::subscriptions`](scv_client::Layout::subscriptions), beside the
//! [job ledger](super::ledger) and not a field of [`JobRecord`](super::ledger::JobRecord).
//! Binding checks that the job is in the ledger and does not start a job.
//! The authorization is who bound it, why, and one fixed scope: deliver
//! follow-ups on the bound thread to that job. It is not an approval mode,
//! and it does not grant merging, publishing, or messaging.
//!
//! Each record keeps the last inbound message id it accepted and the fan-out
//! decision (pending, delivered, handed back, or returned to intake). The
//! full decision list is a sidecar of the record, so an id that was decided
//! stays decided after the short window on the record itself rolls off, and
//! after that thread is no longer a live binding. The sidecar keeps the
//! receiving account with the text and the reply handles. A delivery writes
//! that sidecar before the record, whose window does not keep those fields.
//! A crash between the writes still reloads them. A hand-back or a return
//! to intake writes the record first, its return and its decision together,
//! however old the id. A reload that cannot write the sidecar it recovered
//! from the record leaves those decisions on the record alone, so the next
//! record-first write puts them on the sidecar first, or stores nothing.
//! A reload that finds no
//! sidecar while the window is short recovers them from the message the
//! record still holds and writes the sidecar before that offer can roll off.
//! A delivery is pending until the job has actually taken the message. The
//! job's ledger record names that message before the job starts, and the
//! job does not start without it; the cursor moves only from that record,
//! so a crash in between does not send the text again. A plan alone is not acceptance. Opening the directory
//! does not resume a job.
//! A later owner message on the bound thread does, through the ledger's
//! `resumed_as` / `resumed_from` link and the record's current job, under
//! the same authorization. A job the owner dismissed or cancelled, including
//! a continuation the follow-up queued, receives nothing and is not resumed.
//! Ending a job leaves the subscription in place.
//!
//! An extra thread can deliver to the same job. The worker may hand that
//! thread back; the thread the owner bound stays. The handed-back message
//! belongs to ordinary intake.

use std::{
    collections::{HashMap, HashSet},
    io::Read as _,
    path::{Path, PathBuf},
};

use scv_protocol::AgentState;
use serde::{Deserialize, Serialize};

use super::ledger::{self, JobRecord};

/// What binding grants, and the only scope a subscription may carry.
pub const DELIVER_FOLLOW_UPS: &str = "deliver follow-ups on this thread to this job";

const MAX_WHO_BYTES: usize = 200;
const MAX_WHY_BYTES: usize = 200;
const MAX_THREAD_BYTES: usize = 4096;
const MAX_MESSAGE_BYTES: usize = 512;
const MAX_DECISIONS: usize = 64;
const MAX_HISTORY: usize = 8192;
const MAX_HISTORY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EXTRAS: usize = 32;
const MAX_OFFERS: usize = 64;
const MAX_RETURNS: usize = 64;
const MAX_HELD_TEXT: usize = 16 * 1024;
/// A reply handle longer than this is not a handle this process stored.
const MAX_HANDLE_BYTES: usize = 1024;
/// A reload reads at most this many bytes. A larger file does not parse, and
/// the whole subscription would be discarded. Writes stay within it.
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const VERSION: u32 = 1;

/// Who authorized delivery, and the scope fixed at bind time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authorization {
    /// The session owner who bound the job.
    pub who: String,
    /// Why they bound it.
    pub why: String,
    /// [`DELIVER_FOLLOW_UPS`]. Any other text does not authorize delivery.
    pub scope: String,
    /// Set when delivery is stopped. The record stays.
    #[serde(default)]
    pub stopped: bool,
}

impl Authorization {
    fn live(&self) -> bool {
        !self.stopped && self.scope == DELIVER_FOLLOW_UPS && !self.who.is_empty()
    }
}

/// Whether a message is still waiting to be given to the job, was given, or
/// went back to ordinary intake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fanout {
    /// Recorded before the job takes it. A restart retries it. The cursor
    /// does not move.
    Pending,
    /// The job took it. Not given again. The worker can still hand it back.
    Delivered,
    /// The worker said it is not this job's work.
    HandedBack,
    /// Returned to ordinary intake without a delivery (the job was withheld
    /// after the message had already been held).
    Intake,
}

/// One fan-out decision, in the order the subscription made them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub message_id: String,
    pub thread: String,
    pub fanout: Fanout,
    /// Text kept on the history sidecar while the message can still be
    /// handed back. The record's short window leaves this empty so the
    /// record stays small after the offer list rolls off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The inbound reply handle, kept with the text so a hand-back after
    /// the offer list rolls off is still answered in the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// The handle that posts into the thread. Empty in the direct chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_reply: Option<String>,
    /// `<channel>:<account>` that received the message, kept with the text
    /// so a hand-back after the offer list rolls off still belongs to that
    /// account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// A message the subscription is holding: waiting to be delivered, offered
/// to the job, or waiting for ordinary intake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldMessage {
    pub message_id: String,
    pub thread: String,
    pub text: String,
    /// `<channel>:<account>` that received the message. A return is run
    /// only by that account. Absent on a record written before accounts
    /// were stored; those match the thread's owner instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The transport's handle for answering this message. Empty on a
    /// record written before handles were kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reply_to: String,
    /// The transport's handle for posting into the thread. Empty in the
    /// direct chat and on older records.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thread_reply: String,
}

/// One owner message offered to a subscription.
pub struct Incoming<'a> {
    pub account: Option<&'a str>,
    pub thread: &'a str,
    pub message_id: &'a str,
    pub text: &'a str,
    /// The inbound reply handle.
    pub reply_to: &'a str,
    /// The handle that posts into the thread. Empty in the direct chat.
    pub thread_reply: &'a str,
}

/// The durable control record for one bound thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub v: u32,
    /// File id: the digest of [`Self::thread`], not a path made from it.
    pub id: String,
    /// The conversation key the owner bound (`conversation_of`).
    pub thread: String,
    /// Other conversation keys that also deliver here. The worker may drop
    /// one of these. It cannot drop [`Self::thread`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_threads: Vec<String>,
    /// Ledger session of the job the owner bound.
    pub session: String,
    /// Ledger job the owner bound.
    pub job: String,
    /// `<session>/<job>` a follow-up acts on now. Absent until a continuation
    /// is recorded; the bound job is the start of the chain until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub authorization: Authorization,
    /// Last inbound message id the job accepted. A pending delivery does not
    /// move it, and neither does handing a message back or returning it to
    /// intake: those must not rewind the high-water mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The newest decisions, plus the one the last decision write changed
    /// when that one is older. Older ones stay in the history sidecar and
    /// are still decisive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
    /// Deliveries not yet accepted by the job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<HeldMessage>,
    /// Text of messages the job has taken, newest last, so a hand-back can
    /// still return them. The id itself lives in the history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offers: Vec<HeldMessage>,
    /// Messages waiting for the bridge to run as ordinary intake.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returns: Vec<HeldMessage>,
}

/// What the worker says about a message offered to its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Worker {
    /// The message is this job's work.
    Accept,
    /// The message is not this job's work.
    HandBack,
}

/// What to do with one owner message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// No subscription on this thread. Ordinary intake.
    Intake,
    /// The subscription delivers nothing: authorization is missing or
    /// stopped, or the job was dismissed or cancelled. The job is not resumed.
    Withheld,
    /// This message is at or before the cursor, and it was delivered. Do not
    /// give it to the job again and do not open a turn.
    Settled,
    /// The worker handed the message back. Ordinary intake. `dropped` is set
    /// only when this call removed an extra thread.
    HandedBack(HandBack),
    /// Give `prompt` to the job's existing conversation.
    Deliver(Delivery),
    /// The job has ended. Continue it with the ledger resume link, under
    /// `authorization`, which is the one stored at bind time.
    Resume(Resume),
}

/// A message the worker returned to ordinary intake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandBack {
    pub thread: String,
    pub message_id: String,
    pub text: String,
    /// The extra thread removed by this hand-back. Never the bound thread.
    pub dropped: Option<String>,
}

/// Input for a job that is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub session: String,
    pub job: String,
    pub conversation: Option<String>,
    pub prompt: String,
    pub authorization: Authorization,
    /// The thread the message arrived on.
    pub thread: String,
    /// The inbound id the job records when it takes the text.
    pub message_id: String,
}

/// A finished or interrupted job to continue with the owner's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub session: String,
    pub job: String,
    pub workspace: PathBuf,
    pub conversation: Option<String>,
    pub prompt: String,
    pub authorization: Authorization,
    /// The thread the message arrived on.
    pub thread: String,
    /// The inbound id the resumed job records when it takes the text.
    pub message_id: String,
}

/// Why binding or updating a subscription failed.
#[derive(Debug)]
pub enum Error {
    /// The ledger has no such job (missing, or dismissed).
    MissingJob { session: String, job: String },
    /// The thread already has a subscription.
    ThreadTaken,
    /// The thread is not on a subscription.
    UnknownThread,
    /// A name, reason, or message id cannot be stored.
    Invalid(&'static str),
    /// The record could not be written. The in-memory change was reverted.
    Store(std::io::Error),
    /// The history file cannot be trusted. An id it does not already show
    /// is not delivered and is not forgotten.
    Unreadable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingJob { session, job } => write!(formatter, "no job {session}/{job}"),
            Self::ThreadTaken => write!(formatter, "thread already has a subscription"),
            Self::UnknownThread => write!(formatter, "thread has no subscription"),
            Self::Invalid(message) => write!(formatter, "{message}"),
            Self::Store(error) => write!(formatter, "could not store the subscription: {error}"),
            Self::Unreadable => write!(
                formatter,
                "the subscription history cannot be read, so this message stays pending"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// The subscriptions under `root`, checked against the job ledger at `jobs`.
///
/// [`Self::open`] and [`Self::reload`] read the files and do not resume a job.
#[derive(Debug)]
pub struct Subscriptions {
    root: PathBuf,
    jobs: PathBuf,
    records: Vec<Subscription>,
    /// Every decision, including those that rolled off the record's window.
    history: HashMap<String, Vec<Decision>>,
    /// Subscription ids whose history file exists but cannot be trusted.
    /// An id outside the record's window is not delivered.
    unreadable: HashSet<String>,
    /// Subscription ids whose history file the last load could not repair.
    /// A decision recovered from the record's window is only there, so a
    /// write that can drop it from the window writes the history first.
    behind: HashSet<String>,
}

impl Subscriptions {
    /// Load every subscription under `root`. A missing directory is an empty
    /// set. Jobs are not resumed. Opening repairs what a crash left, which
    /// writes, so only the records' one owner opens them. Any other reader
    /// reads the files and writes nothing.
    pub fn open(root: &Path, jobs: &Path) -> Self {
        let mut records = load_dir(root);
        let (history, unreadable) = load_history(root, &records);
        let behind = repair_loaded(root, &mut records, &history, &unreadable);
        Self {
            root: root.to_owned(),
            jobs: jobs.to_owned(),
            records,
            history,
            unreadable,
            behind,
        }
    }

    /// Read the directory again. Jobs are not resumed. A sidecar left behind
    /// the record by a crash is written out, and a pending copy of a message
    /// the sidecar already settled is dropped.
    pub fn reload(&mut self) {
        self.records = load_dir(&self.root);
        let (history, unreadable) = load_history(&self.root, &self.records);
        self.behind = repair_loaded(&self.root, &mut self.records, &history, &unreadable);
        self.history = history;
        self.unreadable = unreadable;
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The subscription that delivers `thread`, bound or extra.
    pub fn subscription(&self, thread: &str) -> Option<&Subscription> {
        self.index_of(thread).map(|index| &self.records[index])
    }

    /// Bind `thread` to the existing job `session`/`job`. Does not start a
    /// job and does not create one. `who` is the session owner and `why` is
    /// the short reason. The scope is [`DELIVER_FOLLOW_UPS`].
    ///
    /// A thread that already has a live subscription is refused. A stopped
    /// subscription, or one whose job will not be resumed (dismissed,
    /// cancelled, or otherwise withheld), is replaced. Decided ids stay
    /// decided. Pending messages go to ordinary intake, not to the new job.
    pub fn bind(
        &mut self,
        thread: &str,
        session: &str,
        job: &str,
        who: &str,
        why: &str,
    ) -> Result<Subscription, Error> {
        check_thread(thread)?;
        let who = check_note(who, MAX_WHO_BYTES, "who bound it is empty or too long")?;
        let why = check_note(why, MAX_WHY_BYTES, "the reason is empty or too long")?;
        if let Some(index) = self.index_of(thread)
            && !self.binding_replaceable(index)
        {
            return Err(Error::ThreadTaken);
        }
        if ledger::load(&self.jobs, session, job).is_none() {
            return Err(Error::MissingJob {
                session: session.to_owned(),
                job: job.to_owned(),
            });
        }
        if let Some(index) = self.index_of(thread) {
            return self.rebind(index, thread, session, job, who, why);
        }
        self.insert_fresh(thread, session, job, who, why)
    }

    /// Deliver `extra` to the job `bound` already subscribes to. The
    /// authorization is unchanged. The worker can drop `extra`; it cannot
    /// drop `bound`.
    pub fn attach(&mut self, bound: &str, extra: &str) -> Result<(), Error> {
        check_thread(extra)?;
        if bound == extra {
            return Err(Error::Invalid("a thread is not an extra of itself"));
        }
        if self.index_of(extra).is_some() {
            return Err(Error::ThreadTaken);
        }
        let Some(index) = self.index_of(bound) else {
            return Err(Error::UnknownThread);
        };
        if self.records[index].thread != bound {
            return Err(Error::Invalid("only the bound thread accepts an extra"));
        }
        if !self.records[index].authorization.live() {
            return Err(Error::Invalid("a stopped subscription accepts no thread"));
        }
        if self.records[index].extra_threads.len() >= MAX_EXTRAS {
            return Err(Error::Invalid("too many extra threads"));
        }
        let previous = self.records[index].clone();
        self.records[index].extra_threads.push(extra.to_owned());
        self.persist(index, previous)
    }

    /// Stop delivery. The record stays, and a later message does not resume
    /// the job or re-evaluate the scope.
    pub fn stop(&mut self, thread: &str) -> Result<(), Error> {
        let Some(index) = self.index_of(thread) else {
            return Err(Error::UnknownThread);
        };
        let previous = self.records[index].clone();
        self.records[index].authorization.stopped = true;
        self.persist(index, previous)
    }

    /// Decide `message_id` on `thread`.
    ///
    /// [`Worker::Accept`] records a pending delivery and does not move the
    /// cursor. [`Self::commit_delivery`] records the delivery once the job
    /// has taken the message. A message already delivered is not given again.
    /// [`Worker::HandBack`] still applies to a message the job has taken: it
    /// is not settled before the hand-back is recorded.
    #[cfg(test)]
    pub(crate) fn decide(
        &mut self,
        thread: &str,
        message_id: &str,
        text: &str,
        worker: Worker,
    ) -> Result<Plan, Error> {
        self.decide_account(
            Incoming {
                account: None,
                thread,
                message_id,
                text,
                reply_to: "",
                thread_reply: "",
            },
            worker,
        )
    }

    /// [`Self::decide`], recording `account` (`<channel>:<account>`) and the
    /// inbound reply handles on a new pending message. A later hand-back
    /// runs only on that account, and the reply stays in the thread.
    pub fn decide_account(
        &mut self,
        incoming: Incoming<'_>,
        worker: Worker,
    ) -> Result<Plan, Error> {
        let Incoming {
            account,
            thread,
            message_id,
            text,
            reply_to,
            thread_reply,
        } = incoming;
        check_message(message_id)?;
        if worker == Worker::HandBack {
            return self.hand_back(thread, message_id, text);
        }
        // Settled ids are remembered on the history sidecar of whichever
        // subscription decided them. Dropping the live binding does not make
        // one deliverable again.
        if let Some(plan) = self.settled_plan(thread, message_id, text) {
            return Ok(plan);
        }
        let Some(index) = self
            .index_of(thread)
            .or_else(|| self.record_holding(thread, message_id))
        else {
            return Ok(Plan::Intake);
        };
        let pending = self.fanout_of(index, message_id) == Some(Fanout::Pending);
        if let Some(plan) = self.already(index, thread, message_id, text) {
            return Ok(plan);
        }
        // A pending id the job's record already names was accepted. The
        // cursor catches up here. This does not send the text, and a plan
        // that has not reached a record is not acceptance.
        if pending && self.settle_if_accepted(thread, message_id)? {
            return Ok(Plan::Settled);
        }
        if !self.records[index].authorization.live() {
            return self.withhold(index, thread, message_id, text, pending);
        }
        let Some(tip) = self.acting(index) else {
            return self.withhold(index, thread, message_id, text, pending);
        };
        if blocked(&tip) {
            return self.withhold(index, thread, message_id, text, pending);
        }
        // A review that is still running is not a place for this message.
        // Ordinary intake takes it. A new job here would be unreviewed work
        // on the builder's conversation.
        if tip.reviewed && !tip.state.is_final() {
            return self.withhold(index, thread, message_id, text, pending);
        }
        let plan = plan_for(&self.records[index], &tip, thread, message_id, text);
        if !pending {
            self.record_pending(
                index,
                Incoming {
                    account,
                    thread,
                    message_id,
                    text,
                    reply_to,
                    thread_reply,
                },
            )?;
        }
        Ok(plan)
    }

    /// The job took `message_id`. The cursor moves to it. Idempotent once
    /// the message is already delivered, handed back, or returned to intake.
    pub fn commit_delivery(&mut self, thread: &str, message_id: &str) -> Result<(), Error> {
        check_message(message_id)?;
        let Some(index) = self.index_of(thread) else {
            return Err(Error::UnknownThread);
        };
        match self.fanout_of(index, message_id) {
            Some(Fanout::Delivered | Fanout::HandedBack | Fanout::Intake) => return Ok(()),
            Some(Fanout::Pending) | None => {}
        }
        let text = self.message_text(index, message_id, "");
        let account = self.account_of(index, message_id);
        let (reply_to, thread_reply) = self.reply_of(index, message_id);
        let previous = self.records[index].clone();
        let previous_history = self.history_of(index);
        self.records[index]
            .pending
            .retain(|held| held.message_id != message_id);
        self.push_offer(
            index,
            Hold {
                thread,
                message_id,
                text: &text,
                account: account.clone(),
                reply_to: &reply_to,
                thread_reply: &thread_reply,
            },
        );
        self.store_decision(
            index,
            StoredDecision {
                thread,
                message_id,
                fanout: Fanout::Delivered,
                text: &text,
                reply_to: &reply_to,
                thread_reply: &thread_reply,
                account,
            },
            &previous,
            previous_history,
        )
    }

    /// Remember that `successor` (`<session>/<job>`) is the job a follow-up
    /// on `thread` acts on now.
    pub fn note_current(&mut self, thread: &str, successor: &str) -> Result<(), Error> {
        if successor
            .split_once('/')
            .is_none_or(|(session, job)| !ledger::valid_name(session) || !ledger::valid_name(job))
        {
            return Err(Error::Invalid("a resume link is <session>/<job>"));
        }
        let Some(index) = self.index_of(thread) else {
            return Err(Error::UnknownThread);
        };
        let previous = self.records[index].clone();
        self.records[index].current = Some(successor.to_owned());
        self.persist(index, previous)
    }

    /// Ledger jobs a live subscription still resolves through, as
    /// `<session>/<job>`. [`ledger::prune_keeping`](super::ledger::prune_keeping)
    /// leaves these in place.
    pub fn pinned_jobs(&self) -> HashSet<String> {
        pinned(&self.records, &self.jobs)
    }

    /// Pending deliveries, oldest first.
    pub fn pending_messages(&self) -> Vec<HeldMessage> {
        self.records
            .iter()
            .flat_map(|record| record.pending.iter().cloned())
            .collect()
    }

    /// The follow-up `session`/`job` itself accepted, still pending or
    /// delivered. `(thread, message id)`. A later job in the same resume
    /// chain does not count: handing back from this job returns its own
    /// message.
    pub fn latest_offer(&self, session: &str, job: &str) -> Option<(String, String)> {
        let record = ledger::load_including_dismissed(&self.jobs, session, job)?;
        let token = record.accepted_follow_up.as_deref()?;
        let (thread, message_id) = acceptance_parts(token)?;
        let index = self.index_of(&thread)?;
        match self.fanout_of(index, &message_id) {
            Some(Fanout::HandedBack | Fanout::Intake) => None,
            Some(Fanout::Pending | Fanout::Delivered) | None => Some((thread, message_id)),
        }
    }

    /// Messages a worker handed back, or that were released to intake, and
    /// not yet claimed by `account` (`<channel>:<account>`). A return that
    /// names an account is offered only to that account. One that names none
    /// is offered when `owner` is the thread's chat. This does not remove
    /// them: [`Self::ack_returns`] does, after the bridge has saved its
    /// claim. A mail chat does not call this.
    pub fn take_returns(&self, account: &str, owner: &str) -> Vec<HeldMessage> {
        self.records
            .iter()
            .flat_map(|record| {
                record
                    .returns
                    .iter()
                    .filter(|held| return_belongs(held, account, owner))
                    .cloned()
            })
            .collect()
    }

    /// Drop returns `claimed` as `(thread, message id)` once `account` has
    /// saved the claim that will run them. Until then they stay, including
    /// across a reload, so a crash before that claim does not lose them.
    /// An id is settled as handed back only in the same record write that
    /// queues its return ([`Self::hand_back`]).
    pub fn ack_returns(
        &mut self,
        account: &str,
        owner: &str,
        claimed: &[(&str, &str)],
    ) -> Result<(), Error> {
        for index in 0..self.records.len() {
            let claimed_here = |held: &HeldMessage| {
                return_belongs(held, account, owner)
                    && claimed.iter().any(|(thread, message_id)| {
                        held.thread == *thread && held.message_id == *message_id
                    })
            };
            if !self.records[index].returns.iter().any(&claimed_here) {
                continue;
            }
            let previous = self.records[index].clone();
            self.records[index]
                .returns
                .retain(|held| !claimed_here(held));
            self.persist(index, previous)?;
        }
        Ok(())
    }

    /// Messages [`Self::decide`] could not record. A restart retries them.
    /// Empty when every decision was stored, or when the list cannot be read.
    pub fn unrecorded_messages(&self) -> Vec<HeldMessage> {
        read_unrecorded(&self.root).unwrap_or_default()
    }

    /// Keep `message_id` until a later decide can store it. Idempotent.
    /// Fails when the list cannot be read or written, in which case the
    /// caller must not treat the message as held.
    pub fn remember_unrecorded(&mut self, incoming: Incoming<'_>) -> Result<(), Error> {
        let Incoming {
            account,
            thread,
            message_id,
            text,
            reply_to,
            thread_reply,
        } = incoming;
        let mut held = read_unrecorded(&self.root).map_err(Error::Store)?;
        if let Some(item) = held
            .iter_mut()
            .find(|item| item.thread == thread && item.message_id == message_id)
        {
            fill_handles(item, reply_to, thread_reply);
            return write_unrecorded(&self.root, &held).map_err(Error::Store);
        }
        if held.len() >= MAX_RETURNS {
            return Err(Error::Invalid(
                "too many messages are waiting for the subscription history",
            ));
        }
        held.push(HeldMessage {
            message_id: message_id.to_owned(),
            thread: thread.to_owned(),
            text: bounded_text(text),
            account: account
                .filter(|account| !account.is_empty())
                .map(str::to_owned),
            reply_to: bounded_handle(reply_to),
            thread_reply: bounded_handle(thread_reply),
        });
        write_unrecorded(&self.root, &held).map_err(Error::Store)
    }

    /// Drop a message from the unrecorded list once it is stored, or once it
    /// no longer needs a retry.
    pub fn forget_unrecorded(&mut self, thread: &str, message_id: &str) -> Result<(), Error> {
        let held = read_unrecorded(&self.root).map_err(Error::Store)?;
        let kept: Vec<HeldMessage> = held
            .iter()
            .filter(|item| item.thread != thread || item.message_id != message_id)
            .cloned()
            .collect();
        if kept.len() == held.len() {
            return Ok(());
        }
        write_unrecorded(&self.root, &kept).map_err(Error::Store)
    }

    /// Whether `message_id` is waiting because it could not be recorded.
    pub fn unrecorded_contains(&self, thread: &str, message_id: &str) -> bool {
        read_unrecorded(&self.root).is_ok_and(|held| {
            held.iter()
                .any(|item| item.thread == thread && item.message_id == message_id)
        })
    }

    /// The worker says `message_id` is not this job's work.
    ///
    /// A message the job already took is still handed back, including after
    /// the subscription was stopped. Only a message already handed back, or
    /// already returned to intake, is left as it was. An id this subscription
    /// never held is refused, so a later message with that id is not settled.
    pub fn hand_back(&mut self, thread: &str, message_id: &str, text: &str) -> Result<Plan, Error> {
        check_message(message_id)?;
        let Some(index) = self
            .index_of(thread)
            .or_else(|| self.record_holding(thread, message_id))
        else {
            if let Some(plan) = self.settled_plan(thread, message_id, text) {
                return Ok(plan);
            }
            return Ok(Plan::Intake);
        };
        match self.fanout_of(index, message_id) {
            Some(Fanout::HandedBack) => {
                return Ok(Plan::HandedBack(hand_back(thread, message_id, text, None)));
            }
            Some(Fanout::Intake) => return Ok(Plan::Intake),
            None => {
                return Err(Error::Invalid(
                    "this subscription did not hold that message",
                ));
            }
            Some(Fanout::Pending | Fanout::Delivered) => {}
        }
        if !self.records[index].authorization.live() {
            return self.withhold(index, thread, message_id, text, true);
        }
        let text = self.message_text(index, message_id, text);
        self.record_hand_back(index, thread, message_id, &text)
    }

    /// Replay `messages` in thread order `(message id, text)`. Ids at or
    /// before the cursor are not delivered again. Jobs are resumed only for
    /// an id that is still pending, and only because this call offers it.
    #[cfg(test)]
    pub(crate) fn accept_replay(
        &mut self,
        thread: &str,
        messages: &[(&str, &str)],
        worker: Worker,
    ) -> Result<Vec<Plan>, Error> {
        let cursor_at = self.cursor_index(thread, messages);
        let mut plans = Vec::with_capacity(messages.len());
        for (index, (id, text)) in messages.iter().enumerate() {
            if cursor_at.is_some_and(|at| index <= at) {
                plans.push(self.replayed(thread, id, text));
                continue;
            }
            plans.push(self.decide(thread, id, text, worker)?);
        }
        Ok(plans)
    }

    /// Record that `successor` (`<session>/<job>`) resumes `session`/`job`,
    /// using the ledger's `resumed_as` and `resumed_from` fields. The
    /// subscription's authorization is left as it was.
    pub fn commit_resume(
        &mut self,
        session: &str,
        job: &str,
        successor: &str,
    ) -> Result<(), Error> {
        let Some((next_session, next_job)) = successor.split_once('/') else {
            return Err(Error::Invalid("a resume link is <session>/<job>"));
        };
        if !ledger::valid_name(session)
            || !ledger::valid_name(job)
            || !ledger::valid_name(next_session)
            || !ledger::valid_name(next_job)
        {
            return Err(Error::Invalid("not a session or job name"));
        }
        let link = format!("{session}/{job}");
        let updated = ledger::update(&self.jobs, session, job, |record| {
            record.resumed_as = Some(successor.to_owned());
        });
        if updated.is_none() {
            return Err(Error::MissingJob {
                session: session.to_owned(),
                job: job.to_owned(),
            });
        }
        ledger::update(&self.jobs, next_session, next_job, |record| {
            if record.resumed_from.is_none() {
                record.resumed_from = Some(link.clone());
            }
        });
        let from = format!("{session}/{job}");
        let indexes: Vec<usize> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record.current.as_deref() == Some(from.as_str())
                    || (record.current.is_none() && record.session == session && record.job == job)
            })
            .map(|(index, _)| index)
            .collect();
        for index in indexes {
            let previous = self.records[index].clone();
            self.records[index].current = Some(successor.to_owned());
            self.persist(index, previous)?;
        }
        Ok(())
    }

    /// The job's record already names this pending message. Move the cursor
    /// to it and point the subscription at that job. Does not send the text.
    fn settle_if_accepted(&mut self, thread: &str, message_id: &str) -> Result<bool, Error> {
        let token = acceptance_token(thread, message_id);
        let Some(found) = ledger::find_accepted(&self.jobs, &token) else {
            return Ok(false);
        };
        let successor = format!("{}/{}", found.session, found.job);
        self.commit_delivery(thread, message_id)?;
        if let Some((session, job)) = found
            .resumed_from
            .as_deref()
            .and_then(|link| link.split_once('/'))
        {
            match self.commit_resume(session, job, &successor) {
                Ok(()) => {}
                Err(Error::MissingJob { .. }) => self.note_current(thread, &successor)?,
                Err(error) => return Err(error),
            }
        } else {
            self.note_current(thread, &successor)?;
        }
        Ok(true)
    }

    fn insert(&mut self, mut record: Subscription) -> Result<(), Error> {
        store_record(&self.root, &mut record).map_err(Error::Store)?;
        self.records.push(record);
        Ok(())
    }

    fn persist(&mut self, index: usize, previous: Subscription) -> Result<(), Error> {
        match store_record(&self.root, &mut self.records[index]) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.records[index] = previous;
                Err(Error::Store(error))
            }
        }
    }

    fn index_of(&self, thread: &str) -> Option<usize> {
        self.records.iter().position(|record| {
            record.thread == thread || record.extra_threads.iter().any(|extra| extra == thread)
        })
    }

    fn bound_job(&self, index: usize) -> Option<JobRecord> {
        let record = &self.records[index];
        ledger::load_including_dismissed(&self.jobs, &record.session, &record.job)
    }

    /// The job a follow-up acts on. The owner-bound job being dismissed or
    /// cancelled withholds even when a successor exists. Otherwise the
    /// current job, or the bound job, is followed through every `resumed_as`
    /// link. A cycle stops the walk. A missing original record uses the
    /// current job, so pruning the head of the chain does not strand it.
    fn acting(&self, index: usize) -> Option<JobRecord> {
        if let Some(bound) = self.bound_job(index) {
            if bound.dismissed || bound.state == AgentState::Cancelled {
                return Some(bound);
            }
            if let Some(current) = self.load_current(index) {
                return Some(self.walk(current));
            }
            return Some(self.walk(bound));
        }
        self.load_current(index).map(|record| self.walk(record))
    }

    fn walk(&self, mut record: JobRecord) -> JobRecord {
        let mut seen = HashSet::new();
        loop {
            let key = format!("{}/{}", record.session, record.job);
            if !seen.insert(key) {
                break;
            }
            let Some(link) = record.resumed_as.clone() else {
                break;
            };
            let Some((session, job)) = link.split_once('/') else {
                break;
            };
            let Some(next) = ledger::load_including_dismissed(&self.jobs, session, job) else {
                break;
            };
            record = next;
        }
        record
    }

    fn load_current(&self, index: usize) -> Option<JobRecord> {
        let link = self.records[index].current.as_deref()?;
        let (session, job) = link.split_once('/')?;
        ledger::load_including_dismissed(&self.jobs, session, job)
    }

    fn fanout_of(&self, index: usize, message_id: &str) -> Option<Fanout> {
        let record = &self.records[index];
        self.history
            .get(&record.id)
            .and_then(|decisions| {
                decisions
                    .iter()
                    .find(|decision| decision.message_id == message_id)
                    .map(|decision| decision.fanout)
            })
            .or_else(|| {
                record
                    .decisions
                    .iter()
                    .find(|decision| decision.message_id == message_id)
                    .map(|decision| decision.fanout)
            })
            .or_else(|| (record.cursor.as_deref() == Some(message_id)).then_some(Fanout::Delivered))
    }

    fn already(&self, index: usize, thread: &str, message_id: &str, text: &str) -> Option<Plan> {
        match self.fanout_of(index, message_id) {
            Some(Fanout::Delivered) => Some(Plan::Settled),
            Some(Fanout::HandedBack) => {
                Some(Plan::HandedBack(hand_back(thread, message_id, text, None)))
            }
            Some(Fanout::Intake) => Some(Plan::Intake),
            Some(Fanout::Pending) | None => None,
        }
    }

    fn withhold(
        &mut self,
        index: usize,
        thread: &str,
        message_id: &str,
        text: &str,
        pending: bool,
    ) -> Result<Plan, Error> {
        if !pending {
            // Remember the id so a later presentation, after the channel's
            // seen list has dropped it, does not open another turn or reach
            // the job. The bridge opens the one ordinary turn for this call.
            let previous = self.records[index].clone();
            let previous_history = self.history_of(index);
            self.store_decision(
                index,
                StoredDecision {
                    thread,
                    message_id,
                    fanout: Fanout::Intake,
                    text: "",
                    reply_to: "",
                    thread_reply: "",
                    account: None,
                },
                &previous,
                previous_history,
            )?;
            return Ok(Plan::Withheld);
        }
        let text = self.message_text(index, message_id, text);
        let account = self.account_of(index, message_id);
        let (reply_to, thread_reply) = self.reply_of(index, message_id);
        let previous = self.records[index].clone();
        let previous_history = self.history_of(index);
        self.records[index]
            .pending
            .retain(|held| held.message_id != message_id);
        if let Err(error) = self.push_return(
            index,
            Hold {
                thread,
                message_id,
                text: &text,
                account: account.clone(),
                reply_to: &reply_to,
                thread_reply: &thread_reply,
            },
        ) {
            self.records[index] = previous;
            return Err(error);
        }
        self.store_decision(
            index,
            StoredDecision {
                thread,
                message_id,
                fanout: Fanout::Intake,
                text: "",
                reply_to: "",
                thread_reply: "",
                account,
            },
            &previous,
            previous_history,
        )?;
        Ok(Plan::Intake)
    }

    fn record_pending(&mut self, index: usize, incoming: Incoming<'_>) -> Result<(), Error> {
        let Incoming {
            account,
            thread,
            message_id,
            text,
            reply_to,
            thread_reply,
        } = incoming;
        let previous = self.records[index].clone();
        let previous_history = self.history_of(index);
        let text = bounded_text(text);
        let reply_to = bounded_handle(reply_to);
        let thread_reply = bounded_handle(thread_reply);
        let stored_account = account
            .filter(|account| !account.is_empty())
            .map(str::to_owned);
        if let Some(held) = self.records[index]
            .pending
            .iter_mut()
            .find(|held| held.message_id == message_id)
        {
            if held.account.is_none()
                && let Some(account) = stored_account.clone()
            {
                held.account = Some(account);
            }
            fill_handles(held, &reply_to, &thread_reply);
        } else {
            self.records[index].pending.push(HeldMessage {
                message_id: message_id.to_owned(),
                thread: thread.to_owned(),
                text: text.clone(),
                account: stored_account.clone(),
                reply_to: reply_to.clone(),
                thread_reply: thread_reply.clone(),
            });
        }
        self.store_decision(
            index,
            StoredDecision {
                thread,
                message_id,
                fanout: Fanout::Pending,
                text: &text,
                reply_to: &reply_to,
                thread_reply: &thread_reply,
                account: stored_account,
            },
            &previous,
            previous_history,
        )
    }

    fn record_hand_back(
        &mut self,
        index: usize,
        thread: &str,
        message_id: &str,
        text: &str,
    ) -> Result<Plan, Error> {
        let previous = self.records[index].clone();
        let previous_history = self.history_of(index);
        let bound = self.records[index].thread.clone();
        let dropped = (thread != bound).then(|| thread.to_owned());
        if dropped.is_some() {
            self.records[index]
                .extra_threads
                .retain(|extra| extra != thread);
        }
        let text = bounded_text(text);
        let account = self.account_of(index, message_id);
        let (reply_to, thread_reply) = self.reply_of(index, message_id);
        self.records[index]
            .pending
            .retain(|held| held.message_id != message_id);
        self.records[index]
            .offers
            .retain(|held| held.message_id != message_id);
        // The return is on this record before the id is handed back. The
        // write below saves both, so a crash cannot settle the id without
        // the return that intake will run. A full queue leaves the id as it
        // was: an unclaimed return is not discarded to make room.
        if let Err(error) = self.push_return(
            index,
            Hold {
                thread,
                message_id,
                text: &text,
                account: account.clone(),
                reply_to: &reply_to,
                thread_reply: &thread_reply,
            },
        ) {
            self.records[index] = previous;
            return Err(error);
        }
        self.store_decision(
            index,
            StoredDecision {
                thread,
                message_id,
                fanout: Fanout::HandedBack,
                text: "",
                reply_to: "",
                thread_reply: "",
                account,
            },
            &previous,
            previous_history,
        )?;
        Ok(Plan::HandedBack(hand_back(
            thread, message_id, &text, dropped,
        )))
    }

    fn store_decision(
        &mut self,
        index: usize,
        stored: StoredDecision<'_>,
        previous: &Subscription,
        previous_history: Vec<Decision>,
    ) -> Result<(), Error> {
        let StoredDecision {
            thread,
            message_id,
            fanout,
            text,
            reply_to,
            thread_reply,
            account,
        } = stored;
        let id = self.records[index].id.clone();
        if self.unreadable.contains(&id) {
            self.records[index] = previous.clone();
            return Err(Error::Unreadable);
        }
        let mut history = previous_history.clone();
        let kept = matches!(fanout, Fanout::Pending | Fanout::Delivered);
        let stored_text = kept
            .then(|| bounded_text(text))
            .filter(|text| !text.is_empty());
        let stored_reply = kept
            .then(|| bounded_handle(reply_to))
            .filter(|handle| !handle.is_empty());
        let stored_thread = kept
            .then(|| bounded_handle(thread_reply))
            .filter(|handle| !handle.is_empty());
        let stored_account = kept
            .then_some(account)
            .flatten()
            .filter(|account| !account.is_empty());
        if let Some(decision) = history
            .iter_mut()
            .find(|decision| decision.message_id == message_id)
        {
            decision.fanout = fanout;
            decision.thread = thread.to_owned();
            if kept {
                if let Some(text) = stored_text {
                    decision.text = Some(text);
                }
                if let Some(reply_to) = stored_reply {
                    decision.reply_to = Some(reply_to);
                }
                if let Some(thread_reply) = stored_thread {
                    decision.thread_reply = Some(thread_reply);
                }
                if let Some(account) = stored_account {
                    decision.account = Some(account);
                }
            } else {
                decision.text = None;
                decision.reply_to = None;
                decision.thread_reply = None;
                decision.account = None;
            }
        } else if history.len() >= MAX_HISTORY {
            self.records[index] = previous.clone();
            return Err(Error::Invalid(
                "the subscription cannot forget a decided message",
            ));
        } else {
            history.push(Decision {
                message_id: message_id.to_owned(),
                thread: thread.to_owned(),
                fanout,
                text: stored_text,
                reply_to: stored_reply,
                thread_reply: stored_thread,
                account: stored_account,
            });
        }
        if !history_fits(&history) {
            self.records[index] = previous.clone();
            return Err(Error::Invalid(
                "the subscription cannot forget a decided message",
            ));
        }
        // The record's window is the newest decisions, and always this one.
        // A hand-back or a return to intake reaches the record first, with
        // its return. An older id changed in place keeps its place in the
        // history, so the newest alone would leave that write without it,
        // and a crash before the sidecar would leave the id deliverable
        // while its return still runs. The window carries no message text.
        // The sidecar does.
        let mut start = history.len().saturating_sub(MAX_DECISIONS);
        let older = history
            .iter()
            .position(|decision| decision.message_id == message_id)
            .filter(|at| *at < start);
        if older.is_some() {
            start += 1;
        }
        self.records[index].decisions = older
            .map(|at| &history[at])
            .into_iter()
            .chain(&history[start..])
            .cloned()
            .map(|mut decision| {
                decision.text = None;
                decision.reply_to = None;
                decision.thread_reply = None;
                decision.account = None;
                decision
            })
            .collect();
        // Only an acceptance moves the cursor, and only forward to this id.
        // Handing an older message back must not rewind it.
        if fanout == Fanout::Delivered {
            self.records[index].cursor = Some(message_id.to_owned());
        }
        // A delivery lands on the sidecar before the record. The sidecar
        // keeps the account, the text, and the reply handles, so a crash
        // before the record still reloads them and does not deliver again.
        // A hand-back or a return to intake lands on the record first: the
        // return is in that write, so the id is not settled without it.
        if fanout == Fanout::Delivered {
            if let Err(error) = write_history(&self.root, &id, &history) {
                self.records[index] = previous.clone();
                return Err(Error::Store(error));
            }
            // The sidecar now holds every decision this process does. A
            // rollback below writes back the history held before, which
            // also does.
            self.behind.remove(&id);
            if let Err(error) = store_record(&self.root, &mut self.records[index]) {
                self.records[index] = previous.clone();
                if rollback_history(&self.root, &id, &previous_history) {
                    if previous_history.is_empty() {
                        self.history.remove(&id);
                    } else {
                        self.history.insert(id, previous_history);
                    }
                } else {
                    // The sidecar has the delivery and the rollback did not
                    // land. Remember that, so a retry does not send it again.
                    self.history.insert(id, history);
                }
                return Err(Error::Store(error));
            }
            self.history.insert(id, history);
            return Ok(());
        }
        // A load that could not repair the sidecar left some decisions only
        // in the record's window, and the window written next can drop one.
        // The sidecar takes them first, or nothing is stored.
        if self.behind.contains(&id) {
            if !history_fits(&previous_history) {
                self.records[index] = previous.clone();
                return Err(Error::Invalid(
                    "the subscription cannot forget a decided message",
                ));
            }
            if let Err(error) = write_history(&self.root, &id, &previous_history) {
                self.records[index] = previous.clone();
                return Err(Error::Store(error));
            }
            self.behind.remove(&id);
        }
        if let Err(error) = store_record(&self.root, &mut self.records[index]) {
            self.records[index] = previous.clone();
            return Err(Error::Store(error));
        }
        if let Err(error) = write_history(&self.root, &id, &history) {
            self.records[index] = previous.clone();
            let _ = write_record(&self.root, previous);
            if rollback_history(&self.root, &id, &previous_history) {
                if previous_history.is_empty() {
                    self.history.remove(&id);
                } else {
                    self.history.insert(id, previous_history);
                }
            }
            return Err(Error::Store(error));
        }
        self.history.insert(id, history);
        Ok(())
    }

    fn history_of(&self, index: usize) -> Vec<Decision> {
        self.history
            .get(&self.records[index].id)
            .cloned()
            .unwrap_or_default()
    }

    fn message_text(&self, index: usize, message_id: &str, fallback: &str) -> String {
        let record = &self.records[index];
        if let Some(text) = record
            .pending
            .iter()
            .chain(record.offers.iter())
            .find(|held| held.message_id == message_id)
            .map(|held| held.text.clone())
            .filter(|text| !text.is_empty())
        {
            return text;
        }
        if let Some(text) = self.history.get(&record.id).and_then(|decisions| {
            decisions
                .iter()
                .find(|decision| decision.message_id == message_id)
                .and_then(|decision| decision.text.clone())
                .filter(|text| !text.is_empty())
        }) {
            return text;
        }
        bounded_text(fallback)
    }

    fn account_of(&self, index: usize, message_id: &str) -> Option<String> {
        let record = &self.records[index];
        if let Some(account) = record
            .pending
            .iter()
            .chain(record.offers.iter())
            .chain(record.returns.iter())
            .find(|held| held.message_id == message_id)
            .and_then(|held| held.account.clone())
        {
            return Some(account);
        }
        self.history.get(&record.id).and_then(|decisions| {
            decisions
                .iter()
                .find(|decision| decision.message_id == message_id)
                .and_then(|decision| decision.account.clone())
                .filter(|account| !account.is_empty())
        })
    }

    fn reply_of(&self, index: usize, message_id: &str) -> (String, String) {
        let record = &self.records[index];
        for held in record
            .pending
            .iter()
            .chain(record.offers.iter())
            .chain(record.returns.iter())
        {
            if held.message_id == message_id
                && (!held.reply_to.is_empty() || !held.thread_reply.is_empty())
            {
                return (held.reply_to.clone(), held.thread_reply.clone());
            }
        }
        let Some(decision) = self.history.get(&record.id).and_then(|decisions| {
            decisions
                .iter()
                .find(|decision| decision.message_id == message_id)
        }) else {
            return (String::new(), String::new());
        };
        (
            decision.reply_to.clone().unwrap_or_default(),
            decision.thread_reply.clone().unwrap_or_default(),
        )
    }

    fn push_offer(&mut self, index: usize, hold: Hold<'_>) {
        let offers = &mut self.records[index].offers;
        offers.retain(|held| held.message_id != hold.message_id);
        offers.push(held_from(hold));
        if offers.len() > MAX_OFFERS {
            let extra = offers.len() - MAX_OFFERS;
            offers.drain(..extra);
        }
    }

    /// Queue a return. A message already queued is left as it is. A full
    /// queue refuses the new one: the oldest unclaimed return stays, and the
    /// caller keeps the message in its previous state.
    fn push_return(&mut self, index: usize, hold: Hold<'_>) -> Result<(), Error> {
        let returns = &mut self.records[index].returns;
        if returns
            .iter()
            .any(|held| held.message_id == hold.message_id && held.thread == hold.thread)
        {
            return Ok(());
        }
        if returns.len() >= MAX_RETURNS {
            return Err(Error::Invalid(
                "too many messages are waiting to return to intake",
            ));
        }
        returns.push(held_from(hold));
        Ok(())
    }

    /// A non-pending decision for `thread` and `message_id`, from any
    /// subscription's history. Live bindings are not required.
    fn settled_plan(&self, thread: &str, message_id: &str, text: &str) -> Option<Plan> {
        let fanout = self.history.values().find_map(|decisions| {
            decisions.iter().find_map(|decision| {
                (decision.thread == thread
                    && decision.message_id == message_id
                    && decision.fanout != Fanout::Pending)
                    .then_some(decision.fanout)
            })
        })?;
        Some(match fanout {
            Fanout::Delivered => Plan::Settled,
            Fanout::HandedBack => Plan::HandedBack(hand_back(thread, message_id, text, None)),
            Fanout::Intake => {
                if self.index_of(thread).is_some() {
                    Plan::Intake
                } else {
                    Plan::Settled
                }
            }
            Fanout::Pending => Plan::Settled,
        })
    }

    /// The subscription that still holds `message_id` on `thread` after that
    /// thread was removed from the live bindings.
    fn record_holding(&self, thread: &str, message_id: &str) -> Option<usize> {
        self.records.iter().position(|record| {
            record
                .pending
                .iter()
                .chain(record.offers.iter())
                .chain(record.returns.iter())
                .any(|held| held.thread == thread && held.message_id == message_id)
                || self.history.get(&record.id).is_some_and(|decisions| {
                    decisions.iter().any(|decision| {
                        decision.thread == thread && decision.message_id == message_id
                    })
                })
        })
    }

    #[cfg(test)]
    fn cursor_index(&self, thread: &str, messages: &[(&str, &str)]) -> Option<usize> {
        let cursor = self.subscription(thread)?.cursor.as_deref()?;
        messages.iter().position(|(id, _)| *id == cursor)
    }

    #[cfg(test)]
    fn replayed(&self, thread: &str, message_id: &str, text: &str) -> Plan {
        let Some(index) = self.index_of(thread) else {
            return Plan::Settled;
        };
        self.already(index, thread, message_id, text)
            .unwrap_or(Plan::Settled)
    }

    fn insert_fresh(
        &mut self,
        thread: &str,
        session: &str,
        job: &str,
        who: String,
        why: String,
    ) -> Result<Subscription, Error> {
        let record = Subscription {
            v: VERSION,
            id: file_id(thread),
            thread: thread.to_owned(),
            extra_threads: Vec::new(),
            session: session.to_owned(),
            job: job.to_owned(),
            current: None,
            authorization: Authorization {
                who,
                why,
                scope: DELIVER_FOLLOW_UPS.to_owned(),
                stopped: false,
            },
            cursor: None,
            decisions: Vec::new(),
            pending: Vec::new(),
            offers: Vec::new(),
            returns: Vec::new(),
        };
        self.insert(record.clone())?;
        Ok(record)
    }

    /// A live subscription whose job can still receive or resume is not
    /// replaced. Stopped, dismissed, cancelled, and any other withhold
    /// forever are.
    fn binding_replaceable(&self, index: usize) -> bool {
        if !self.records[index].authorization.live() {
            return true;
        }
        match self.acting(index) {
            None => true,
            Some(job) => blocked(&job),
        }
    }

    fn rebind(
        &mut self,
        index: usize,
        thread: &str,
        session: &str,
        job: &str,
        who: String,
        why: String,
    ) -> Result<Subscription, Error> {
        if self.records[index].thread != thread {
            let previous = self.records[index].clone();
            self.records[index]
                .extra_threads
                .retain(|extra| extra != thread);
            self.persist(index, previous)?;
            return self.insert_fresh(thread, session, job, who, why);
        }
        self.release_pending(index)?;
        let previous = self.records[index].clone();
        let record = &mut self.records[index];
        record.extra_threads.clear();
        record.session = session.to_owned();
        record.job = job.to_owned();
        record.current = None;
        record.offers.clear();
        record.authorization = Authorization {
            who,
            why,
            scope: DELIVER_FOLLOW_UPS.to_owned(),
            stopped: false,
        };
        self.persist(index, previous)?;
        Ok(self.records[index].clone())
    }

    /// Pending deliveries belonged to the subscription being replaced. They
    /// go to ordinary intake and are not given to the new job.
    fn release_pending(&mut self, index: usize) -> Result<(), Error> {
        let pending = self.records[index].pending.clone();
        for held in pending {
            let previous = self.records[index].clone();
            let previous_history = self.history_of(index);
            let account = held.account.clone();
            let text = held.text.clone();
            let reply_to = held.reply_to.clone();
            let thread_reply = held.thread_reply.clone();
            self.records[index]
                .pending
                .retain(|item| item.message_id != held.message_id);
            if let Err(error) = self.push_return(
                index,
                Hold {
                    thread: &held.thread,
                    message_id: &held.message_id,
                    text: &text,
                    account: account.clone(),
                    reply_to: &reply_to,
                    thread_reply: &thread_reply,
                },
            ) {
                self.records[index] = previous;
                return Err(error);
            }
            self.store_decision(
                index,
                StoredDecision {
                    thread: &held.thread,
                    message_id: &held.message_id,
                    fanout: Fanout::Intake,
                    text: "",
                    reply_to: "",
                    thread_reply: "",
                    account,
                },
                &previous,
                previous_history,
            )?;
        }
        Ok(())
    }
}

struct StoredDecision<'a> {
    thread: &'a str,
    message_id: &'a str,
    fanout: Fanout,
    text: &'a str,
    reply_to: &'a str,
    thread_reply: &'a str,
    account: Option<String>,
}

struct Hold<'a> {
    thread: &'a str,
    message_id: &'a str,
    text: &'a str,
    account: Option<String>,
    reply_to: &'a str,
    thread_reply: &'a str,
}

fn held_from(hold: Hold<'_>) -> HeldMessage {
    HeldMessage {
        message_id: hold.message_id.to_owned(),
        thread: hold.thread.to_owned(),
        text: hold.text.to_owned(),
        account: hold.account,
        reply_to: bounded_handle(hold.reply_to),
        thread_reply: bounded_handle(hold.thread_reply),
    }
}

fn fill_handles(held: &mut HeldMessage, reply_to: &str, thread_reply: &str) {
    if held.reply_to.is_empty() && !reply_to.is_empty() {
        held.reply_to = bounded_handle(reply_to);
    }
    if held.thread_reply.is_empty() && !thread_reply.is_empty() {
        held.thread_reply = bounded_handle(thread_reply);
    }
}

fn plan_for(
    subscription: &Subscription,
    tip: &JobRecord,
    thread: &str,
    message_id: &str,
    text: &str,
) -> Plan {
    let authorization = subscription.authorization.clone();
    let conversation = tip.conversation.clone().or_else(|| {
        tip.arguments
            .get("session")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    });
    let thread = thread.to_owned();
    let message_id = message_id.to_owned();
    if tip.state.is_final() {
        Plan::Resume(Resume {
            session: tip.session.clone(),
            job: tip.job.clone(),
            workspace: tip.workspace.clone(),
            conversation,
            prompt: text.to_owned(),
            authorization,
            thread,
            message_id,
        })
    } else {
        Plan::Deliver(Delivery {
            session: tip.session.clone(),
            job: tip.job.clone(),
            conversation,
            prompt: text.to_owned(),
            authorization,
            thread,
            message_id,
        })
    }
}

/// What the worker reads for a follow-up delivered into a live conversation.
/// The owner's words follow the note. A resumed job's prompt says the same
/// about handing the message back, and also repeats the original task.
pub(crate) fn worker_prompt(text: &str) -> String {
    format!(
        "[SCV] The owner sent this follow-up on the thread subscribed to this job. \
         Continue the same task. If it is not this job's work, hand it back with \
         `scv jobs hand-back` and do not act on it.\n\n{text}"
    )
}

/// What the job ledger stores so a follow-up can be recognized after a crash.
/// A subscription plan does not carry this; only a job record does.
pub fn acceptance_token(thread: &str, message_id: &str) -> String {
    serde_json::json!({
        "thread": thread,
        "message_id": message_id,
    })
    .to_string()
}

/// [`Subscriptions::pinned_jobs`] for the records under `root`, read and not
/// opened: nothing is repaired or written. The workbench's maintenance runs
/// outside the daemon's lock on its [`Subscriptions`], so a write here could
/// replace a record the daemon changed after this read.
pub(crate) fn pinned_jobs_in(root: &Path, jobs: &Path) -> HashSet<String> {
    pinned(&load_dir(root), jobs)
}

fn pinned(records: &[Subscription], jobs: &Path) -> HashSet<String> {
    let mut pinned = HashSet::new();
    for record in records {
        if !record.authorization.live() {
            continue;
        }
        pinned.insert(format!("{}/{}", record.session, record.job));
        if let Some(current) = &record.current {
            pinned.insert(current.clone());
        }
        if let Some(bound) = ledger::load_including_dismissed(jobs, &record.session, &record.job) {
            pinned.extend(chain_keys(jobs, bound));
        }
    }
    for record in ledger::list_all(jobs) {
        if record
            .resumed_from
            .as_ref()
            .is_some_and(|from| pinned.contains(from))
        {
            pinned.insert(format!("{}/{}", record.session, record.job));
        }
    }
    pinned
}

/// `record` and every job its `resumed_as` links reach, as `<session>/<job>`.
/// A cycle stops the walk.
fn chain_keys(jobs: &Path, mut record: JobRecord) -> Vec<String> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let key = format!("{}/{}", record.session, record.job);
        if !seen.insert(key.clone()) {
            break;
        }
        keys.push(key);
        let Some(link) = record.resumed_as.clone() else {
            break;
        };
        let Some((session, job)) = link.split_once('/') else {
            break;
        };
        let Some(next) = ledger::load_including_dismissed(jobs, session, job) else {
            break;
        };
        record = next;
    }
    keys
}

/// The job receives nothing and must not be resumed.
fn blocked(record: &JobRecord) -> bool {
    if record.dismissed || record.state == AgentState::Cancelled {
        return true;
    }
    // A reviewed job's journal is its record. Finishing it does not resume it.
    if record.reviewed && record.state.is_final() {
        return true;
    }
    // The resume link points at a job this process could not load. Do not
    // start another one.
    record.resumed_as.is_some() && record.state.is_final()
}

fn hand_back(thread: &str, message_id: &str, text: &str, dropped: Option<String>) -> HandBack {
    HandBack {
        thread: thread.to_owned(),
        message_id: message_id.to_owned(),
        text: text.to_owned(),
        dropped,
    }
}

fn check_thread(thread: &str) -> Result<(), Error> {
    if thread.is_empty() || thread.len() > MAX_THREAD_BYTES {
        return Err(Error::Invalid("the thread key is empty or too long"));
    }
    Ok(())
}

fn check_message(message_id: &str) -> Result<(), Error> {
    if message_id.is_empty() || message_id.len() > MAX_MESSAGE_BYTES {
        return Err(Error::Invalid("the message id is empty or too long"));
    }
    Ok(())
}

fn check_note(text: &str, max: usize, invalid: &'static str) -> Result<String, Error> {
    let text = text.trim();
    if text.is_empty() || text.len() > max || text.bytes().any(|byte| byte < b' ') {
        return Err(Error::Invalid(invalid));
    }
    Ok(text.to_owned())
}

fn file_id(thread: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(thread.as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn load_dir(root: &Path) -> Vec<Subscription> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".json")
            || name.ends_with(".history.json")
            || name.ends_with(".held.json")
        {
            continue;
        }
        if let Some(record) = read_record(&path) {
            records.push(record);
        }
    }
    records
}

fn read_record(path: &Path) -> Option<Subscription> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES).read_to_end(&mut bytes).ok()?;
    let record: Subscription = serde_json::from_slice(&bytes).ok()?;
    let stem = path.file_stem()?.to_str()?;
    (record.v == VERSION && stem == record.id && stem == file_id(&record.thread)).then_some(record)
}

fn record_fits(record: &Subscription) -> bool {
    serde_json::to_vec_pretty(record).is_ok_and(|bytes| bytes.len() as u64 <= MAX_RECORD_BYTES)
}

/// Drop oldest delivered-message text until the record fits in
/// [`MAX_RECORD_BYTES`]. That text, its reply handles, and its account stay
/// on the history sidecar. Pending messages, returns, and decisions stay on
/// the record.
fn fit_record(record: &mut Subscription) -> std::io::Result<()> {
    while !record_fits(record) {
        if record.offers.is_empty() {
            return Err(std::io::Error::other(
                "the subscription record would exceed its readable size",
            ));
        }
        record.offers.remove(0);
    }
    Ok(())
}

fn store_record(root: &Path, record: &mut Subscription) -> std::io::Result<()> {
    fit_record(record)?;
    write_record(root, record)
}

fn write_record(root: &Path, record: &Subscription) -> std::io::Result<()> {
    #[cfg(test)]
    spend_write()?;
    if !record_fits(record) {
        return Err(std::io::Error::other(
            "the subscription record would exceed its readable size",
        ));
    }
    crate::delegate::records::write_private_json(root, &format!("{}.json", record.id), record)
}

fn history_name(id: &str) -> String {
    format!("{id}.history.json")
}

fn load_history(
    root: &Path,
    records: &[Subscription],
) -> (HashMap<String, Vec<Decision>>, HashSet<String>) {
    let mut history = HashMap::new();
    let mut unreadable = HashSet::new();
    for record in records {
        let path = root.join(history_name(&record.id));
        let mut decisions = match read_history(&path) {
            HistoryFile::Loaded(decisions) => decisions,
            // A full window with no readable sidecar has ids older than the
            // window. Those stay undecided until the file can be read, rather
            // than being delivered again.
            HistoryFile::Missing if record.decisions.len() >= MAX_DECISIONS => {
                unreadable.insert(record.id.clone());
                record.decisions.clone()
            }
            HistoryFile::Missing => record.decisions.clone(),
            HistoryFile::Unreadable => {
                unreadable.insert(record.id.clone());
                record.decisions.clone()
            }
        };
        if decisions.is_empty() && record.decisions.len() >= MAX_DECISIONS {
            unreadable.insert(record.id.clone());
            decisions = record.decisions.clone();
        }
        // Whichever file is ahead keeps its fan-out. A delivery writes the
        // sidecar first, so its fan-out outranks the record after a crash
        // there. A hand-back writes the record first, so that fan-out
        // outranks a sidecar that is one write behind. An id only the record
        // has is kept. Provenance already on the sidecar stays; gaps are
        // filled from the record and from the message it still holds.
        for decision in &record.decisions {
            if let Some(existing) = decisions
                .iter_mut()
                .find(|existing| existing.message_id == decision.message_id)
            {
                if fanout_rank(decision.fanout) > fanout_rank(existing.fanout) {
                    existing.fanout = decision.fanout;
                    existing.thread = decision.thread.clone();
                }
                fill_provenance(existing, decision);
            } else {
                decisions.push(decision.clone());
            }
        }
        for decision in &mut decisions {
            // Handed-back and returned decisions drop this text on purpose.
            // A delivery that has not reached the sidecar still has it on
            // the offer or the pending message.
            if !matches!(decision.fanout, Fanout::Pending | Fanout::Delivered) {
                continue;
            }
            if let Some(held) = record
                .pending
                .iter()
                .chain(record.offers.iter())
                .chain(record.returns.iter())
                .find(|held| {
                    held.thread == decision.thread && held.message_id == decision.message_id
                })
            {
                fill_from_held(decision, held);
            }
        }
        history.insert(record.id.clone(), decisions);
    }
    (history, unreadable)
}

/// Write a sidecar a crash left behind the record, and drop pending copies of
/// messages the sidecar already settled. An unreadable sidecar is left alone.
/// Returns the ids whose sidecar is still behind: a decision recovered from
/// the record's window is not on it yet.
fn repair_loaded(
    root: &Path,
    records: &mut [Subscription],
    history: &HashMap<String, Vec<Decision>>,
    unreadable: &HashSet<String>,
) -> HashSet<String> {
    let mut behind = HashSet::new();
    for record in records.iter() {
        if unreadable.contains(&record.id) {
            continue;
        }
        let Some(memory) = history.get(&record.id) else {
            continue;
        };
        let path = root.join(history_name(&record.id));
        let disk = match read_history(&path) {
            HistoryFile::Loaded(decisions) => decisions,
            HistoryFile::Missing => Vec::new(),
            HistoryFile::Unreadable => {
                behind.insert(record.id.clone());
                continue;
            }
        };
        if disk == *memory {
            continue;
        }
        if !history_fits(memory) || write_history(root, &record.id, memory).is_err() {
            behind.insert(record.id.clone());
        }
    }
    for record in records.iter_mut() {
        let Some(decisions) = history.get(&record.id) else {
            continue;
        };
        let settled = |held: &HeldMessage| {
            decisions.iter().any(|decision| {
                decision.thread == held.thread
                    && decision.message_id == held.message_id
                    && decision.fanout != Fanout::Pending
            })
        };
        if !record.pending.iter().any(&settled) {
            continue;
        }
        record.pending.retain(|held| !settled(held));
        let _ = store_record(root, record);
    }
    behind
}

fn fanout_rank(fanout: Fanout) -> u8 {
    match fanout {
        Fanout::Pending => 0,
        Fanout::Delivered => 1,
        Fanout::HandedBack | Fanout::Intake => 2,
    }
}

fn fill_gap(slot: &mut Option<String>, value: Option<String>) {
    if slot.as_ref().is_some_and(|value| !value.is_empty()) {
        return;
    }
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        *slot = Some(value);
    }
}

fn fill_provenance(decision: &mut Decision, from: &Decision) {
    fill_gap(&mut decision.text, from.text.clone());
    fill_gap(&mut decision.reply_to, from.reply_to.clone());
    fill_gap(&mut decision.thread_reply, from.thread_reply.clone());
    fill_gap(&mut decision.account, from.account.clone());
}

fn fill_from_held(decision: &mut Decision, held: &HeldMessage) {
    fill_gap(
        &mut decision.text,
        (!held.text.is_empty()).then(|| held.text.clone()),
    );
    fill_gap(
        &mut decision.reply_to,
        (!held.reply_to.is_empty()).then(|| held.reply_to.clone()),
    );
    fill_gap(
        &mut decision.thread_reply,
        (!held.thread_reply.is_empty()).then(|| held.thread_reply.clone()),
    );
    fill_gap(&mut decision.account, held.account.clone());
}

enum HistoryFile {
    Missing,
    Unreadable,
    Loaded(Vec<Decision>),
}

fn read_history(path: &Path) -> HistoryFile {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HistoryFile::Missing,
        Err(_) => return HistoryFile::Unreadable,
    };
    let mut bytes = Vec::new();
    // One byte past the cap means the file cannot be trusted whole.
    if file
        .take(MAX_HISTORY_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_HISTORY_BYTES
    {
        return HistoryFile::Unreadable;
    }
    serde_json::from_slice(&bytes).map_or(HistoryFile::Unreadable, HistoryFile::Loaded)
}

fn history_fits(decisions: &[Decision]) -> bool {
    serde_json::to_vec_pretty(decisions).is_ok_and(|bytes| bytes.len() as u64 <= MAX_HISTORY_BYTES)
}

fn write_history(root: &Path, id: &str, decisions: &[Decision]) -> std::io::Result<()> {
    #[cfg(test)]
    spend_write()?;
    crate::delegate::records::write_private_json(root, &history_name(id), &decisions)
}

#[cfg(test)]
thread_local! {
    /// How many more record and history writes this test thread lets land.
    /// Each later one fails, as if the process had stopped there. `None` is
    /// no limit.
    static WRITES_LEFT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn spend_write() -> std::io::Result<()> {
    match WRITES_LEFT.get() {
        Some(0) => Err(std::io::Error::other("the test stopped writes here")),
        Some(left) => {
            WRITES_LEFT.set(Some(left - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

/// Restore the sidecar to `previous`. False means the newer sidecar is still
/// on disk.
fn rollback_history(root: &Path, id: &str, previous: &[Decision]) -> bool {
    if previous.is_empty() {
        #[cfg(test)]
        if spend_write().is_err() {
            return false;
        }
        let path = root.join(history_name(id));
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    } else {
        write_history(root, id, previous).is_ok()
    }
}

fn bounded_text(text: &str) -> String {
    scv_client::text::utf8_prefix(text, MAX_HELD_TEXT).to_owned()
}

fn bounded_handle(handle: &str) -> String {
    if handle.len() > MAX_HANDLE_BYTES {
        String::new()
    } else {
        handle.to_owned()
    }
}

/// The conversation key as a person can type it. NUL becomes the two
/// characters `\0`.
pub fn visible_thread(key: &str) -> String {
    key.replace('\0', "\\0")
}

/// `<channel>:<account>` owns a return that names it. A return from before
/// accounts were stored belongs to the account whose owner is the thread's
/// chat. A mail chat never asks.
fn return_belongs(held: &HeldMessage, account: &str, owner: &str) -> bool {
    match held.account.as_deref() {
        Some(recorded) => recorded == account,
        None => !owner.is_empty() && chat_of(&held.thread) == owner,
    }
}

/// The chat a conversation key is in. A thread's key is `<chat>\0thread\0<id>`.
fn chat_of(key: &str) -> &str {
    key.split_once("\0thread\0").map_or(key, |(chat, _)| chat)
}

fn acceptance_parts(token: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(token).ok()?;
    let thread = value.get("thread")?.as_str()?.to_owned();
    let message_id = value.get("message_id")?.as_str()?.to_owned();
    (!thread.is_empty() && !message_id.is_empty()).then_some((thread, message_id))
}

const UNRECORDED_NAME: &str = "unrecorded.held.json";

fn read_unrecorded(root: &Path) -> std::io::Result<Vec<HeldMessage>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = root.join(UNRECORDED_NAME);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    // One byte past the cap means the list cannot be trusted whole, so it is
    // not replaced with a shorter parse.
    if file
        .take(MAX_HISTORY_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_HISTORY_BYTES
    {
        return Err(std::io::Error::other(
            "the unrecorded subscription list cannot be read",
        ));
    }
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

fn write_unrecorded(root: &Path, held: &[HeldMessage]) -> std::io::Result<()> {
    if held.is_empty() {
        let path = root.join(UNRECORDED_NAME);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    }
    crate::delegate::records::write_private_json(root, UNRECORDED_NAME, &held)
}

#[cfg(test)]
mod tests;
