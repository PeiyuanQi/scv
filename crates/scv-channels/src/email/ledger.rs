//! An email account's durable state, `state/channels/email/<account>.json`,
//! and the only code that changes it.
//!
//! Every transition is one method here and one atomic write: the new state
//! is built on a copy and replaces the old one only once it is saved, so a
//! failed write changes nothing. Messages are claimed in the same write that
//! moves the cursor past them, and a claim is released in the write that
//! records what became of its message, so a crash repeats work but never
//! loses a message. The file holds mail text only in queued report items and
//! the one message being handed to a mail chat, both bounded; everything
//! else is IDs, hashes, counts, and times.
//!
//! The disk is written off the async threads, and the state's lock is never
//! held while it is: a slow or stuck disk delays the next change, never a
//! reader, the notifier, or shutdown. Each write holds the account's run
//! lock, so a run that replaces this one starts only once the last write
//! has landed.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::Notify;

use super::credentials::Account;
use super::plan::{Class, Logged, Queued, SendClass};
use super::source::{Cursor, SourceRef};
use crate::state::{self, AccountState};

/// The state's format. A release refuses state written by a newer one
/// rather than drop what it does not understand.
pub(crate) const STATE_VERSION: u32 = 1;
/// Messages claimed and not yet decided.
pub(crate) const MAX_CLAIMS: usize = 64;
/// A claim's model reads: after this many attempts it is reported by its
/// headers only, and after [`MAX_ATTEMPTS`] it is given up.
pub(crate) const MODEL_ATTEMPTS: u32 = 2;
pub(crate) const MAX_ATTEMPTS: u32 = 4;
/// The most bytes of one queued item's text.
pub(crate) const MAX_ITEM_BYTES: usize = 1536;
const DAY: u64 = 86_400;
/// Identities of messages already decided, for spotting a second arrival.
const MAX_IDENTITIES: usize = 1024;
const IDENTITY_SECONDS: u64 = 7 * DAY;
/// Keys of items a mail chat stored, so none is queued twice.
const MAX_SENT_KEYS: usize = 1024;
const SENT_KEY_SECONDS: u64 = 7 * DAY;
/// Messages stored by a mail chat whose delivery is still watched.
const MAX_WATCHED: usize = 256;
const WATCH_SECONDS: u64 = 7 * DAY;
/// How long a state write waits out a daemon command's transaction.
const BUSY_RETRY: Duration = Duration::from_secs(5);

/// The account's state file.
pub(crate) type Store = state::Store<Account, MailState>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct MailState {
    /// [`STATE_VERSION`] when written.
    pub(crate) v: u32,
    pub(crate) credential_fingerprint: Option<String>,
    /// 16 random hex digits made with the file. Every notice key includes
    /// it, so no key repeats after a logout deletes the file.
    pub(crate) epoch: String,
    /// Where reading resumes; `None` before the first check.
    pub(crate) cursor: Option<Cursor>,
    /// Messages claimed and not yet decided, oldest first.
    pub(crate) claims: Vec<Claim>,
    /// Identities of messages decided lately.
    pub(crate) identities: Vec<Known>,
    /// Items waiting for a message to the mail chat.
    pub(crate) queue: Vec<Item>,
    pub(crate) next_seq: u64,
    /// The message being handed to a mail chat; at most one at a time.
    pub(crate) batch: Option<Batch>,
    /// Messages sent in the last day, for the rolling limits.
    pub(crate) log: Vec<Logged>,
    /// Keys of items a mail chat stored lately.
    pub(crate) sent_keys: Vec<Stamped>,
    /// Stored messages whose delivery is watched for a refusal.
    pub(crate) watched: Vec<Watched>,
    /// What the next digest's header and footer say.
    pub(crate) counts: Counts,
    /// Today's activity, in the owner's local day.
    pub(crate) day: Day,
    /// When model turns ran in the last hour.
    pub(crate) turns: Vec<u64>,
}

impl Default for MailState {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            credential_fingerprint: None,
            epoch: String::new(),
            cursor: None,
            claims: Vec::new(),
            identities: Vec::new(),
            queue: Vec::new(),
            next_seq: 0,
            batch: None,
            log: Vec::new(),
            sent_keys: Vec::new(),
            watched: Vec::new(),
            counts: Counts::default(),
            day: Day::default(),
            turns: Vec::new(),
        }
    }
}

impl AccountState for MailState {
    fn binding(&self) -> Option<&str> {
        self.credential_fingerprint.as_deref()
    }

    fn bind(&mut self, fingerprint: String) {
        self.credential_fingerprint = Some(fingerprint);
    }

    fn in_use(&self) -> bool {
        self.cursor.is_some()
            || !self.claims.is_empty()
            || !self.queue.is_empty()
            || self.batch.is_some()
    }
}

/// A message claimed for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Claim {
    pub(crate) source: SourceRef,
    /// Times its decision started.
    pub(crate) attempts: u32,
    /// Unix seconds when it was claimed.
    pub(crate) at: u64,
}

/// A key and when it was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stamped {
    pub(crate) key: String,
    pub(crate) at: u64,
}

/// A message decided lately: its identity, the digest of its `Message-ID`
/// when it had one, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Known {
    pub(crate) key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<String>,
    pub(crate) at: u64,
}

/// How a message stands against the messages decided lately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// Nothing decided lately matches it.
    New,
    /// This very message was decided already, and is listed again, as it is
    /// after its mailbox was renumbered.
    Again,
    /// Another message decided lately had its `Message-ID`: a copy
    /// delivered again, or a sender's forgery. Either way it is decided on
    /// its own.
    ReusedId,
}

/// One item for the mail chat: a report or one of SCV's lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    pub(crate) seq: u64,
    pub(crate) created_at: u64,
    /// Unique among queued and lately sent items: `report:<source>` or
    /// `system:<kind>:<local date>`.
    pub(crate) key: String,
    pub(crate) class: Class,
    pub(crate) urgent: bool,
    /// Rendered, at most [`MAX_ITEM_BYTES`].
    pub(crate) text: String,
}

/// A message being handed to a mail chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Batch {
    /// `mail:<epoch>:batch:<16 random hex digits>`: the mail chat stores a
    /// key once, so handing the batch over again never repeats it.
    pub(crate) key: String,
    pub(crate) class: SendClass,
    /// The items it carries.
    pub(crate) seqs: Vec<u64>,
    pub(crate) text: String,
    /// The counts its header and footer report, taken from `counts` once
    /// it is stored.
    pub(crate) counts: Counts,
    /// Rounds in which no route stored it.
    pub(crate) attempts: u32,
    pub(crate) next_attempt_at: u64,
    pub(crate) created_at: u64,
}

/// A message a mail chat stored, watched until its delivery is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Watched {
    pub(crate) key: String,
    pub(crate) route: String,
    pub(crate) at: u64,
}

/// Mail the owner hears about only as a number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Counts {
    /// Counted without a report: by a rule, by triage, as a duplicate, or
    /// as too old.
    pub(crate) skipped: u64,
    /// Reports no mail chat took or that the platform refused.
    pub(crate) undelivered: u64,
    /// Reports dropped from a full queue, and when the oldest arrived.
    pub(crate) unlisted: u64,
    pub(crate) unlisted_since: Option<u64>,
    /// Mail that could not be read after every attempt.
    pub(crate) unreadable: u64,
}

impl Counts {
    fn minus(self, sent: Self) -> Self {
        Self {
            skipped: self.skipped.saturating_sub(sent.skipped),
            undelivered: self.undelivered.saturating_sub(sent.undelivered),
            unlisted: self.unlisted.saturating_sub(sent.unlisted),
            unlisted_since: if self.unlisted > sent.unlisted {
                self.unlisted_since
            } else {
                None
            },
            unreadable: self.unreadable.saturating_sub(sent.unreadable),
        }
    }
}

/// One local day's activity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Day {
    /// `YYYY-MM-DD` in the owner's local time.
    pub(crate) date: String,
    pub(crate) seen: u64,
    pub(crate) triaged: u64,
    pub(crate) reported: u64,
    pub(crate) tokens: u64,
}

/// What became of a claimed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Report it: a new item for the mail chat.
    Report {
        key: String,
        text: String,
        urgent: bool,
    },
    /// Count it without a report.
    Counted,
    /// It left the mailbox before it was decided.
    Gone,
    /// It could not be read after every attempt.
    Unreadable,
}

/// A claim's decision, recorded in one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decided {
    pub(crate) source: SourceRef,
    /// The message's identity, when its metadata was read.
    pub(crate) identity: Option<String>,
    /// [`super::parse::digest`] of its `Message-ID`, when it had one.
    pub(crate) message_id: Option<String>,
    pub(crate) outcome: Outcome,
    /// A model turn ran for it, and the tokens it cost.
    pub(crate) turned: bool,
    pub(crate) tokens: u64,
}

/// Why a change was refused.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The state file would outgrow `mail.retention.max_state_kib`.
    Full,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the account's mail state is at its size limit")
    }
}

impl std::error::Error for Refused {}

/// The account's state and its only writer.
pub(crate) struct Ledger {
    store: Arc<Store>,
    /// The account's run lock, held until the ledger and its last write are
    /// gone.
    run_lock: Arc<std::fs::File>,
    account: String,
    /// Held by one change from when it copies the state until the copy is
    /// saved and published, so changes apply one at a time and in order.
    writer: tokio::sync::Mutex<()>,
    /// The state as last saved: locked only to copy or replace it.
    state: Mutex<MailState>,
    /// Woken whenever the queue or the batch changes.
    pub(crate) changed: Notify,
    max_state_bytes: usize,
    max_queue: usize,
    /// When the mailbox was last checked, kept only in memory.
    last_check: AtomicU64,
    /// Runs on the blocking thread before each write, standing in for a
    /// slow disk.
    #[cfg(test)]
    pub(crate) before_write: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Ledger {
    /// Take over `state`, bound and loaded by the caller under `run_lock`
    /// (the account's [`state::Store::lock`]), for `account`. A state file
    /// from a newer release is refused; a new one gets its epoch.
    pub(crate) fn open(
        store: Store,
        run_lock: std::fs::File,
        account: &str,
        mut state: MailState,
        max_state_bytes: usize,
        max_queue: usize,
    ) -> Result<Self> {
        if state.v > STATE_VERSION {
            bail!(
                "this mail account's state was written by a newer SCV; upgrade, or log the \
                 account out and in again"
            );
        }
        if state.epoch.is_empty() {
            state.epoch = random_hex();
            state.v = STATE_VERSION;
            store.save_state(account, &state)?;
        }
        Ok(Self {
            store: Arc::new(store),
            run_lock: Arc::new(run_lock),
            account: account.to_owned(),
            writer: tokio::sync::Mutex::new(()),
            state: Mutex::new(state),
            changed: Notify::new(),
            max_state_bytes,
            max_queue,
            last_check: AtomicU64::new(0),
            #[cfg(test)]
            before_write: None,
        })
    }

    fn state(&self) -> MutexGuard<'_, MailState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A copy of the current state.
    pub(crate) fn snapshot(&self) -> MailState {
        self.state().clone()
    }

    /// When the mailbox was last checked successfully.
    pub(crate) fn last_check(&self) -> Option<u64> {
        Some(self.last_check.load(Ordering::Relaxed)).filter(|at| *at > 0)
    }

    pub(crate) fn checked(&self, now: u64) {
        self.last_check.store(now, Ordering::Relaxed);
    }

    /// Apply `change` to a copy of the state, trimming it to fit, and save
    /// it; only then does it become the state. A change that cannot fit
    /// even after trimming is refused with [`Refused::Full`].
    async fn commit<T>(&self, change: impl FnOnce(&mut MailState) -> T) -> Result<T> {
        let _writer = self.writer.lock().await;
        let mut next = self.snapshot();
        let value = change(&mut next);
        let text = fit(&mut next, self.max_state_bytes)?;
        let next = self.save(next, text).await?;
        *self.state() = next;
        self.changed.notify_one();
        Ok(value)
    }

    /// Write the serialized state on a blocking thread, waiting out a
    /// daemon command's short transaction on the account, and hand the
    /// state back once it is saved. Dropped midway, the write still
    /// finishes or fails whole, and the run lock is held until it has.
    async fn save(&self, state: MailState, text: String) -> Result<MailState> {
        let deadline = tokio::time::Instant::now() + BUSY_RETRY;
        let mut pending = (state, text);
        loop {
            let store = Arc::clone(&self.store);
            let run_lock = Arc::clone(&self.run_lock);
            let account = self.account.clone();
            #[cfg(test)]
            let before_write = self.before_write.clone();
            let (state, text) = pending;
            let (state, text, result) = tokio::task::spawn_blocking(move || {
                let _run_lock = run_lock;
                #[cfg(test)]
                if let Some(before_write) = before_write {
                    before_write();
                }
                let result = store.save_serialized(&account, &state, &text);
                (state, text, result)
            })
            .await
            .context("writing the mail state stopped")?;
            match result {
                Err(error) if state::is_busy(&error) && tokio::time::Instant::now() < deadline => {
                    pending = (state, text);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                result => return result.map(|()| state),
            }
        }
    }

    /// Claim `refs` and move the cursor past them, in one write. After a
    /// mailbox reset, `beyond` counts mail in the resync window that was
    /// not listed, and SCV says once today that it resynced.
    pub(crate) async fn claim(
        &self,
        refs: Vec<SourceRef>,
        next: Cursor,
        reset: Option<ResetNote>,
        now: u64,
    ) -> Result<()> {
        self.commit(|state| {
            for source in refs {
                if state.claims.len() < MAX_CLAIMS
                    && !state.claims.iter().any(|claim| claim.source == source)
                {
                    state.claims.push(Claim {
                        source,
                        attempts: 0,
                        at: now,
                    });
                }
            }
            state.cursor = Some(next);
            if let Some(reset) = reset {
                if reset.beyond > 0 {
                    state.counts.unlisted += reset.beyond as u64;
                    state.counts.unlisted_since.get_or_insert(now);
                }
                push_item(state, &reset.key, Class::System, false, reset.text, now);
            }
        })
        .await?;
        self.checked(now);
        Ok(())
    }

    /// Record another attempt at `source`'s decision before it starts, so a
    /// message that crashes the account every time is given up. `None` when
    /// it is no longer claimed.
    pub(crate) async fn attempt(&self, source: &SourceRef) -> Result<Option<u32>> {
        self.commit(|state| {
            state
                .claims
                .iter_mut()
                .find(|claim| &claim.source == source)
                .map(|claim| {
                    claim.attempts += 1;
                    claim.attempts
                })
        })
        .await
    }

    /// Record `decided` and release its claim, in one write. A report that
    /// no longer fits the state file is counted instead.
    pub(crate) async fn finish(&self, decided: Decided, today: &str, now: u64) -> Result<()> {
        let full = decided.clone();
        let result = self
            .commit(|state| record(state, full, today, now, self.max_queue))
            .await;
        match result {
            Err(error) if error.downcast_ref::<Refused>().is_some() => {
                tracing::warn!("mail state is full; counted a message without a report");
                let counted = Decided {
                    outcome: Outcome::Counted,
                    ..decided
                };
                self.commit(|state| record(state, counted, today, now, self.max_queue))
                    .await
            }
            result => result,
        }
    }

    /// Queue one of SCV's own lines under `key`, unless that key is queued
    /// or was sent lately.
    pub(crate) async fn note(&self, key: &str, class: Class, text: String, now: u64) -> Result<()> {
        self.commit(|state| push_item(state, key, class, false, text, now))
            .await
    }

    /// Start handing `seqs` over as one message, `text`, whose header and
    /// footer report `counts`.
    pub(crate) async fn begin_batch(
        &self,
        seqs: Vec<u64>,
        class: SendClass,
        text: String,
        counts: Counts,
        now: u64,
    ) -> Result<Batch> {
        self.commit(|state| {
            let batch = Batch {
                key: format!("mail:{}:batch:{}", state.epoch, random_hex()),
                class,
                seqs,
                text,
                counts,
                attempts: 0,
                next_attempt_at: now,
                created_at: now,
            };
            state.batch = Some(batch.clone());
            batch
        })
        .await
    }

    /// The batch was stored by `route`: its items and counts are done, its
    /// key is recorded, and its delivery is watched.
    pub(crate) async fn batch_stored(&self, route: &str, now: u64) -> Result<()> {
        self.commit(|state| {
            let Some(batch) = state.batch.take() else {
                return;
            };
            // Its items' keys are remembered, so neither a report nor a
            // once-a-day line is queued again.
            let (sent, kept): (Vec<Item>, Vec<Item>) = std::mem::take(&mut state.queue)
                .into_iter()
                .partition(|item| batch.seqs.contains(&item.seq));
            state.queue = kept;
            state.sent_keys.extend(sent.into_iter().map(|item| Stamped {
                key: item.key,
                at: now,
            }));
            state.counts = state.counts.minus(batch.counts);
            state.log.push(Logged {
                at: now,
                class: batch.class,
            });
            state.watched.push(Watched {
                key: batch.key,
                route: route.to_owned(),
                at: now,
            });
            prune(state, now);
        })
        .await
    }

    /// No route stored the batch this round: try every route again later,
    /// backing off from 30 seconds to 30 minutes.
    pub(crate) async fn batch_retry(&self, now: u64) -> Result<()> {
        self.commit(|state| {
            if let Some(batch) = &mut state.batch {
                batch.attempts += 1;
                let delay = 30u64
                    .saturating_mul(1u64 << batch.attempts.min(10))
                    .min(30 * 60);
                batch.next_attempt_at = now + delay;
            }
        })
        .await
    }

    /// Give up a batch no route took in time: its reports are counted as
    /// undelivered, and its counts go to the next digest.
    pub(crate) async fn batch_dissolve(&self) -> Result<()> {
        self.commit(|state| {
            let Some(batch) = state.batch.take() else {
                return;
            };
            let reports = state
                .queue
                .iter()
                .filter(|item| batch.seqs.contains(&item.seq) && item.class == Class::Report)
                .count();
            state.queue.retain(|item| !batch.seqs.contains(&item.seq));
            state.counts.undelivered += reports as u64;
        })
        .await
    }

    /// Stop watching stored messages whose delivery is now known; each of
    /// `refused` is counted as undelivered in the next digest.
    pub(crate) async fn settle_watched(
        &self,
        delivered: &[String],
        refused: &[String],
    ) -> Result<()> {
        if delivered.is_empty() && refused.is_empty() {
            return Ok(());
        }
        self.commit(|state| {
            state.watched.retain(|watched| {
                !delivered.contains(&watched.key) && !refused.contains(&watched.key)
            });
            state.counts.undelivered += refused.len() as u64;
        })
        .await
    }

    /// Drop what is past its age or count, for the janitor.
    pub(crate) async fn prune(&self, now: u64) -> Result<()> {
        self.commit(|state| prune(state, now)).await
    }
}

/// The text for a mailbox reset, queued once per local day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResetNote {
    pub(crate) key: String,
    pub(crate) text: String,
    pub(crate) beyond: usize,
}

/// Apply one decision to `state`.
fn record(state: &mut MailState, decided: Decided, today: &str, now: u64, max_queue: usize) {
    state.claims.retain(|claim| claim.source != decided.source);
    if let Some(identity) = decided.identity {
        state.identities.retain(|seen| seen.key != identity);
        state.identities.push(Known {
            key: identity,
            message_id: decided.message_id,
            at: now,
        });
    }
    roll_day(state, today);
    if decided.turned {
        state.turns.push(now);
        state.day.triaged += 1;
    }
    state.day.tokens = state.day.tokens.saturating_add(decided.tokens);
    if decided.outcome != Outcome::Gone {
        state.day.seen += 1;
    }
    match decided.outcome {
        Outcome::Report { key, text, urgent } => {
            state.day.reported += 1;
            push_item(state, &key, Class::Report, urgent, text, now);
        }
        Outcome::Counted => state.counts.skipped += 1,
        Outcome::Unreadable => state.counts.unreadable += 1,
        Outcome::Gone => {}
    }
    collapse(state, max_queue, now);
    prune(state, now);
}

/// Start a new local day's counts when `today` differs.
fn roll_day(state: &mut MailState, today: &str) {
    if state.day.date != today {
        state.day = Day {
            date: today.to_owned(),
            ..Day::default()
        };
    }
}

/// Queue an item unless its key is queued or sent lately.
fn push_item(state: &mut MailState, key: &str, class: Class, urgent: bool, text: String, now: u64) {
    if state.queue.iter().any(|item| item.key == key)
        || state.sent_keys.iter().any(|sent| sent.key == key)
    {
        return;
    }
    const CUT: &str = "\n[…]";
    let text = if text.len() > MAX_ITEM_BYTES {
        let cut = scv_client::text::utf8_prefix(&text, MAX_ITEM_BYTES - CUT.len()).to_owned();
        format!("{cut}{CUT}")
    } else {
        text
    };
    state.queue.push(Item {
        seq: state.next_seq,
        created_at: now,
        key: key.to_owned(),
        class,
        urgent,
        text,
    });
    state.next_seq += 1;
}

/// Beyond `max_queue`, the oldest reports not being handed over are dropped
/// and counted, so the queue's text stays bounded. SCV's own lines stay.
fn collapse(state: &mut MailState, max_queue: usize, now: u64) {
    let excess = state.queue.len().saturating_sub(max_queue);
    if excess == 0 {
        return;
    }
    let in_batch = state
        .batch
        .as_ref()
        .map(|batch| batch.seqs.clone())
        .unwrap_or_default();
    let mut dropped = Vec::new();
    let mut candidates: Vec<&Item> = state
        .queue
        .iter()
        .filter(|item| item.class == Class::Report && !in_batch.contains(&item.seq))
        .collect();
    candidates.sort_by_key(|item| (item.created_at, item.seq));
    for item in candidates.into_iter().take(excess) {
        dropped.push((item.seq, item.created_at));
    }
    if dropped.is_empty() {
        return;
    }
    let oldest = dropped.iter().map(|(_, at)| *at).min().unwrap_or(now);
    state
        .queue
        .retain(|item| !dropped.iter().any(|(seq, _)| *seq == item.seq));
    state.counts.unlisted += dropped.len() as u64;
    let since = state.counts.unlisted_since.get_or_insert(oldest);
    *since = (*since).min(oldest);
}

/// Drop ring entries past their age, then the oldest beyond their count.
fn prune(state: &mut MailState, now: u64) {
    fn ring<T>(entries: &mut Vec<T>, at: fn(&T) -> u64, now: u64, max_age: u64, max: usize) {
        entries.retain(|entry| now.saturating_sub(at(entry)) < max_age);
        let excess = entries.len().saturating_sub(max);
        entries.drain(..excess);
    }
    ring(
        &mut state.identities,
        |known| known.at,
        now,
        IDENTITY_SECONDS,
        MAX_IDENTITIES,
    );
    ring(
        &mut state.sent_keys,
        |sent| sent.at,
        now,
        SENT_KEY_SECONDS,
        MAX_SENT_KEYS,
    );
    state.log.retain(|entry| now.saturating_sub(entry.at) < DAY);
    state
        .watched
        .retain(|watched| now.saturating_sub(watched.at) < WATCH_SECONDS);
    let excess = state.watched.len().saturating_sub(MAX_WATCHED);
    state.watched.drain(..excess);
    state.turns.retain(|at| now.saturating_sub(*at) < 3600);
}

/// `state` serialized within `max_bytes`: first the identity ring gives way,
/// then the oldest queued reports collapse into a count. What still does
/// not fit is refused.
fn fit(state: &mut MailState, max_bytes: usize) -> Result<String> {
    let mut text = serde_json::to_string(state)?;
    if text.len() <= max_bytes {
        return Ok(text);
    }
    tracing::warn!("mail state reached its size limit; trimming");
    state.identities.drain(..state.identities.len() / 2);
    text = serde_json::to_string(state)?;
    while text.len() > max_bytes {
        let before = state.queue.len();
        let now = state
            .queue
            .iter()
            .map(|item| item.created_at)
            .max()
            .unwrap_or(0);
        collapse(state, before.saturating_sub(1), now);
        if state.queue.len() == before {
            return Err(anyhow!(Refused::Full));
        }
        text = serde_json::to_string(state)?;
    }
    Ok(text)
}

impl MailState {
    /// How a message with `identity`, and `message_id` (the digest of its
    /// `Message-ID`), stands against those decided within the last week.
    pub(crate) fn arrival(&self, identity: &str, message_id: Option<&str>) -> Arrival {
        if self.identities.iter().any(|known| known.key == identity) {
            Arrival::Again
        } else if message_id.is_some_and(|id| {
            self.identities
                .iter()
                .any(|known| known.message_id.as_deref() == Some(id))
        }) {
            Arrival::ReusedId
        } else {
            Arrival::New
        }
    }

    /// Model turns in the hour before `now`, and tokens spent `today`.
    pub(crate) fn spent(&self, today: &str, now: u64) -> (usize, u64) {
        let turns = self
            .turns
            .iter()
            .filter(|at| now.saturating_sub(**at) < 3600)
            .count();
        let tokens = if self.day.date == today {
            self.day.tokens
        } else {
            0
        };
        (turns, tokens)
    }

    /// The queue as the plan sees it, less what is being handed over.
    pub(crate) fn queued(&self) -> Vec<Queued> {
        self.queue
            .iter()
            .filter(|item| {
                self.batch
                    .as_ref()
                    .is_none_or(|batch| !batch.seqs.contains(&item.seq))
            })
            .map(|item| Queued {
                seq: item.seq,
                created_at: item.created_at,
                class: item.class,
                urgent: item.urgent,
                bytes: item.text.len(),
            })
            .collect()
    }
}

/// 16 random lowercase hex digits.
pub(crate) fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_owned()
}

#[cfg(test)]
mod tests;
