//! What the executor may do to a mailbox: the only code that writes to one
//! or sends mail, and only with an [`Approved`] in hand.
//!
//! Each provider implements [`MailEffects`] on a connection (or grant) made
//! for one approved action, and derives every target (the message, the
//! folder, every recipient) from the `Approved`, never from an argument.
//! Before it changes anything it checks again that the message the action
//! was approved for is still there and still the same one. What each call
//! did is classified strictly: a send whose data may have reached the
//! server is `Ambiguous`, never retried, only checked. A Gmail or Graph
//! mutation answered HTTP 401 is `Uncertain`: it may have happened, a check
//! may record that it did, and it is never carried out again.
//!
//! [`MailEffects::probe`] is read-only: it looks for the effect of an
//! interrupted action, so a crash never makes one happen twice.

use async_trait::async_trait;

use super::content::ActionContent;
use super::ledger::Approved;
use super::ledger::actions::{Execution, Probe};

#[async_trait]
pub(crate) trait MailEffects: Send {
    /// Save `message`, the approved outgoing message's bytes, in Drafts,
    /// unless a check finds it there already.
    async fn save_draft(&mut self, approved: &Approved, message: &[u8]) -> Execution;

    /// Move the approved message, or mark it read.
    async fn change(&mut self, approved: &Approved) -> Execution;

    /// Send `message`, the approved outgoing message's bytes.
    async fn send(&mut self, approved: &Approved, message: &[u8]) -> Execution;

    /// Put a copy of the sent `message` in Sent, when SCV files it itself
    /// (`sent_copy = "append"`); whether the copy is there.
    async fn copy_sent(&mut self, approved: &Approved, message: &[u8]) -> bool;

    /// Read-only: whether the action `content` describes happened.
    async fn probe(&mut self, content: &ActionContent) -> Probe;
}

/// A failure to reach the provider before anything was written: trying
/// again is safe.
pub(crate) fn unreachable() -> Execution {
    Execution::NotApplied {
        retry: true,
        code: super::ledger::actions::OutcomeCode::Unreachable,
    }
}
