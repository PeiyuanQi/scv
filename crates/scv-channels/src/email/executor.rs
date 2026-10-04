//! The executor: carries out approved actions, one at a time, oldest
//! approval first, and checks interrupted ones before anything else.
//!
//! For each action it asks the ledger to begin ([`Ledger::begin_execution`],
//! which checks the approval, the deadline, the content's digest, the
//! credentials, and the settings, and marks it `executing`), builds the
//! outgoing message's bytes when there is one, and hands the sealed
//! [`Approved`] to a fresh [`MailEffects`] connection. Whatever that did is
//! recorded before the next action starts. An action under way counts in
//! the hub, so a planned restart waits for it; while the hub drains, or the
//! account stops, no new one starts, and the one under way runs to its end
//! within its time budget, so stopping never cuts a send in half.
//!
//! [`Approved`]: super::ledger::Approved

use anyhow::Result;
use std::time::Duration;

use super::Clock;
use super::content::{ActionKind, ContentStore};
use super::effects::MailEffects;
use super::ledger::Ledger;
use super::ledger::actions::{Execution, OutcomeCode, SentCopyState};
use super::message;
use super::settings::SentCopy;
use crate::hub::MailRegistration;

/// How often the executor looks for due work without being woken.
const TICK: Duration = Duration::from_secs(30);
/// The most each stage of an action may take: carrying it out, and then,
/// for a send SCV files itself, its copy in Sent. Carrying it out always
/// ends within the account's stop grace ([`super::STOP_GRACE`]); a copy a
/// stop cuts short is reported as not saved when the account starts again.
pub(crate) const ACTION_BUDGET: Duration = Duration::from_secs(55);

/// Makes a fresh connection's effects for one action.
pub(crate) type Effects<'a> = &'a (dyn Fn() -> Box<dyn MailEffects> + Send + Sync);

pub(crate) struct Executor<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) content: &'a ContentStore,
    pub(crate) effects: Effects<'a>,
    /// Counts actions under way in the hub, and tells when it drains.
    pub(crate) registration: Option<&'a MailRegistration>,
    pub(crate) clock: &'a dyn Clock,
}

/// Counts one action as under way for as long as it lives.
struct Counted<'a>(Option<&'a MailRegistration>);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        if let Some(registration) = self.0 {
            registration.end_execution();
        }
    }
}

impl Executor<'_> {
    /// Work until `stop` resolves; an action under way when it does runs to
    /// its end first. Only a failed state write ends it early.
    pub(crate) async fn run(&self, stop: impl std::future::Future<Output = ()>) -> Result<()> {
        tokio::pin!(stop);
        loop {
            tokio::select! {
                biased;
                () = &mut stop => return Ok(()),
                () = std::future::ready(()) => {}
            }
            let mut worked = false;
            let now = self.clock.now();
            for id in self.ledger.due_probes(now) {
                self.probe(&id).await?;
                worked = true;
            }
            self.ledger.expire(now).await?;
            tokio::select! {
                biased;
                () = &mut stop => return Ok(()),
                () = std::future::ready(()) => {}
            }
            if let Some(id) = self.ledger.next_approved(now)
                && self.draining().is_none()
            {
                worked |= self.execute(&id).await?;
            }
            if worked {
                // Look again at once, unless the account is stopping.
                tokio::select! {
                    biased;
                    () = &mut stop => return Ok(()),
                    () = tokio::task::yield_now() => continue,
                }
            }
            tokio::select! {
                biased;
                () = &mut stop => return Ok(()),
                () = self.ledger.changed.notified() => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }

    /// Why nothing new starts now, if something stops it.
    fn draining(&self) -> Option<&'static str> {
        self.registration
            .is_some_and(|registration| registration.hub().mail_draining())
            .then_some("a planned restart drains mail actions")
    }

    fn read(&self, id: &str) -> Option<super::content::ActionContent> {
        match self.content.read(id) {
            Ok(content) => content,
            Err(error) => {
                tracing::error!(action = id, error = %error, "could not read a mail action's content");
                None
            }
        }
    }

    /// Carry out approved action `id`; whether it started.
    async fn execute(&self, id: &str) -> Result<bool> {
        let counted = match self.registration {
            Some(registration) if !registration.begin_execution() => return Ok(false),
            registration => Counted(registration),
        };
        let content = self.read(id);
        let now = self.clock.now();
        let approved = match self.ledger.begin_execution(id, content, now).await? {
            Ok(approved) => approved,
            Err(not_started) => {
                tracing::info!(action = id, ?not_started, "a mail action did not start");
                return Ok(true);
            }
        };
        let kind = approved.content().kind;
        tracing::info!(
            action = id,
            kind = kind.name(),
            attempt = approved.attempt(),
            "carrying out a mail action"
        );
        let bytes = approved
            .content()
            .message
            .as_ref()
            .map(|outgoing| message::build(outgoing, now, self.clock.offset(now)));
        let mut effects = (self.effects)();
        let perform = async {
            match (kind, &bytes) {
                (ActionKind::Draft, Some(bytes)) => effects.save_draft(&approved, bytes).await,
                (ActionKind::Send, Some(bytes)) => effects.send(&approved, bytes).await,
                (ActionKind::Draft | ActionKind::Send, None) => Execution::NotApplied {
                    retry: false,
                    code: OutcomeCode::Internal,
                },
                _ => effects.change(&approved).await,
            }
        };
        // Past the budget the outcome is unknown: a check decides, and for
        // anything but a send that check may try again. A mutation the
        // provider answered HTTP 401 arrives as `Execution::Uncertain` and
        // is recorded as that. The ledger may check it, and does not carry
        // it out again.
        let execution = tokio::time::timeout(ACTION_BUDGET, perform)
            .await
            .unwrap_or(Execution::Ambiguous);
        tracing::info!(action = id, outcome = ?execution, "a mail action ran");
        let append = approved
            .content()
            .message
            .as_ref()
            .is_some_and(|outgoing| outgoing.sent_copy == SentCopy::Append);
        if kind == ActionKind::Send
            && append
            && let (Execution::Applied { .. }, Some(bytes)) = (execution, &bytes)
        {
            // Sent: nothing can send it again from here on.
            self.ledger.mark_sent(&approved, self.clock.now()).await?;
            let saved = tokio::time::timeout(ACTION_BUDGET, effects.copy_sent(&approved, bytes))
                .await
                .unwrap_or(false);
            let copy = if saved {
                SentCopyState::Saved
            } else {
                tracing::warn!(action = id, "could not save a sent message's copy in Sent");
                SentCopyState::Failed
            };
            self.ledger
                .finish_execution(
                    approved,
                    Execution::Applied {
                        code: OutcomeCode::Applied,
                        sent_copy: Some(copy),
                    },
                    self.clock.now(),
                )
                .await?;
        } else {
            self.ledger
                .finish_execution(approved, execution, self.clock.now())
                .await?;
        }
        drop(counted);
        Ok(true)
    }

    /// Check what the interrupted action `id` did, read-only.
    async fn probe(&self, id: &str) -> Result<()> {
        let Some(content) = self.read(id) else {
            self.ledger
                .probed(id, super::ledger::actions::Probe::Unknown, self.clock.now())
                .await?;
            return Ok(());
        };
        let mut effects = (self.effects)();
        let probe = tokio::time::timeout(ACTION_BUDGET, effects.probe(&content))
            .await
            .unwrap_or(super::ledger::actions::Probe::Unreachable);
        tracing::info!(action = id, result = ?probe, "checked an interrupted mail action");
        self.ledger.probed(id, probe, self.clock.now()).await
    }
}

#[cfg(test)]
mod tests;
