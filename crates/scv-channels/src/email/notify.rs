//! Handing reports to the mail chat, one message at a time.
//!
//! The notifier asks [`plan`] what to send, renders it, records it as the
//! batch in flight, and hands it to the first route that is a running mail
//! chat. A key is stored once, so handing the same batch over again after a
//! lost acknowledgement never repeats it, and a route the hub reports as an
//! ordinary chat is never used: the hub and the bridge refuse it too.

use anyhow::Result;
use std::time::Duration;

use super::Clock;
use super::ledger::{Batch, Counts, Ledger};
use super::plan::{self, Class, Limits, Plan, SendClass};
use super::render::{self, DigestOptions};
use super::settings::{MailSettings, Skipped};
use crate::hub::{Hub, KeyedOutcome, NotifyError};
use crate::state::Purpose;

/// The longest the notifier sleeps without looking again.
const TICK: Duration = Duration::from_secs(60);
/// Room kept in a message for its header and footer.
const FRAME_BYTES: usize = 512;

/// The plan's view of the account's notification settings, at `offset`.
pub(crate) fn limits(settings: &MailSettings, offset: i32) -> Limits {
    let notify = &settings.notify;
    Limits {
        settle_seconds: notify.settle_seconds,
        max_delay_seconds: notify.max_delay_seconds,
        max_items: notify.max_items,
        max_bytes: (notify.max_message_kib * 1024).saturating_sub(FRAME_BYTES),
        max_urgent_per_hour: notify.max_urgent_per_hour,
        max_messages_per_hour: notify.max_messages_per_hour,
        max_messages_per_day: notify.max_messages_per_day,
        max_responses_per_hour: notify.max_responses_per_hour,
        // Checked when the settings were read.
        quiet: notify.quiet().ok().flatten(),
        quiet_urgent: notify.quiet_urgent,
        offset,
    }
}

pub(crate) struct Notifier<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) settings: &'a MailSettings,
    pub(crate) account: &'a str,
    pub(crate) clock: &'a dyn Clock,
    /// The daemon's hub; `None` when the account runs without a daemon.
    pub(crate) hub: Option<&'a Hub>,
}

impl Notifier<'_> {
    /// Send reports for the life of the run. Only a failed state write
    /// ends it.
    pub(crate) async fn run(&self) -> Result<()> {
        loop {
            let wait = self.step().await?;
            tokio::select! {
                () = self.ledger.changed.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Do what is due now, and say how long to wait before looking again.
    pub(crate) async fn step(&self) -> Result<Duration> {
        let now = self.clock.now();
        self.settle_watched().await?;
        let snapshot = self.ledger.snapshot();
        if let Some(batch) = snapshot.batch {
            if batch.next_attempt_at > now {
                return Ok(Duration::from_secs(batch.next_attempt_at - now).min(TICK));
            }
            self.hand_over(&batch).await?;
            return Ok(Duration::ZERO);
        }
        let offset = self.clock.offset(now);
        let limits = limits(self.settings, offset);
        match plan::plan(now, &snapshot.queued(), &snapshot.log, &limits) {
            Plan::Idle => Ok(TICK),
            Plan::Wait { until, paused } => {
                if paused {
                    self.pause_line(until, snapshot.queue.len(), offset, now)
                        .await?;
                }
                Ok(Duration::from_secs(until.saturating_sub(now))
                    .clamp(Duration::from_secs(1), TICK))
            }
            Plan::Send { seqs, class } => {
                let items: Vec<_> = seqs
                    .iter()
                    .filter_map(|seq| snapshot.queue.iter().find(|item| item.seq == *seq))
                    .collect();
                // Responses carry no counts; digests take the counts so far.
                let counts = if class == SendClass::Response {
                    Counts::default()
                } else {
                    snapshot.counts
                };
                let text = render::digest(
                    self.account,
                    class,
                    &items,
                    &counts,
                    DigestOptions {
                        offset,
                        show_skipped: self.settings.notify.skipped == Skipped::Count,
                    },
                );
                let batch = self
                    .ledger
                    .begin_batch(seqs, class, text, counts, now)
                    .await?;
                self.hand_over(&batch).await?;
                Ok(Duration::ZERO)
            }
        }
    }

    /// Tell the owner once per local day that the message limits hold mail
    /// back, and until when.
    async fn pause_line(&self, until: u64, waiting: usize, offset: i32, now: u64) -> Result<()> {
        let today = self.clock.today(now);
        let text = format!(
            "Mail notifications are paused until {}: the message limit is reached, and {waiting} \
             are waiting.",
            render::clock(until, offset)
        );
        self.ledger
            .note(&format!("system:pause:{today}"), Class::Response, text, now)
            .await
    }

    /// Hand `batch` to the first route that stores it; with none, try again
    /// later, and give it up once it is older than `give_up_hours`.
    async fn hand_over(&self, batch: &Batch) -> Result<()> {
        let now = self.clock.now();
        if now.saturating_sub(batch.created_at) > self.settings.notify.give_up_hours * 3600 {
            tracing::warn!("gave up a mail message no mail chat took in time");
            return self.ledger.batch_dissolve().await;
        }
        let Some(hub) = self.hub else {
            return self.ledger.batch_retry(now).await;
        };
        for route in &self.settings.notify.route {
            match hub.purpose(route) {
                Some(Purpose::Mail) => {}
                Some(Purpose::Chat) => {
                    // Never an ordinary chat: a model could be shown it there.
                    tracing::error!(route = %route, "a mail route is not a mail chat; skipped it");
                    continue;
                }
                None => continue,
            }
            let Some(Some(owner)) = hub.owner(route) else {
                continue;
            };
            let stored = match hub
                .notify_keyed(route, &owner, &batch.text, &batch.key)
                .await
            {
                Ok(()) => true,
                // It may have been stored after the hub stopped waiting.
                Err(NotifyError::NotStored) => hub.keyed_outcome(route, &batch.key).is_some(),
                Err(_) => false,
            };
            if stored {
                tracing::info!(
                    items = batch.seqs.len(),
                    "a mail chat stored a mail message"
                );
                return self.ledger.batch_stored(route, self.clock.now()).await;
            }
        }
        self.ledger.batch_retry(self.clock.now()).await
    }

    /// Stop watching stored messages whose delivery is known, counting
    /// refusals for the next digest.
    async fn settle_watched(&self) -> Result<()> {
        let Some(hub) = self.hub else {
            return Ok(());
        };
        let watched = self.ledger.snapshot().watched;
        let mut delivered = Vec::new();
        let mut refused = Vec::new();
        for entry in watched {
            match hub.keyed_outcome(&entry.route, &entry.key) {
                Some(KeyedOutcome::Delivered { .. }) => delivered.push(entry.key),
                Some(KeyedOutcome::Refused) => refused.push(entry.key),
                Some(KeyedOutcome::Pending) | None => {}
            }
        }
        self.ledger.settle_watched(&delivered, &refused).await
    }
}

#[cfg(test)]
mod tests;
