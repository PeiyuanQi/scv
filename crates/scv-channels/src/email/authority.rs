//! The account's authority: the commands the owner sends in a mail chat,
//! and the daemon's orders, carried out on the ledger.
//!
//! The hub hands each command here with its evidence. `approve` and `deny`
//! go straight to the ledger, which checks and records them in one write.
//! Requests that need the mailbox or a model (a reply, a forward, new mail,
//! a revision, a move or a mark) are checked against the settings here and
//! queued for the worker, durably and once per message; their previews
//! follow as mail chat messages. Every answer is SCV's own words, codes,
//! handles, and counts, never an address, a subject, or a model's text.
//! An answer the hub stopped waiting for goes to the mail chat instead.

use anyhow::Result;
use tokio::sync::mpsc;

use super::Clock;
use super::content::{ActionKind, Form};
use super::ledger::Ledger;
use super::ledger::actions::{Admission, Request, RequestWork, World};
use crate::hub::{KeyedOutcome, MailOrder, MailRegistration, MailReply, MailRequest, MailWork};
use crate::mail_chat::{ChatEvidence, MailCommand, MessageAction};

/// What the ledger learns from the hub while it approves.
pub(crate) struct HubWorld<'a>(pub(crate) &'a MailRegistration);

impl World for HubWorld<'_> {
    fn owner(&self, route: &str) -> Option<String> {
        self.0.chat_owner(route)
    }

    fn delivered(&self, route: &str, key: &str) -> Option<KeyedOutcome> {
        self.0.hub().keyed_outcome(route, key)
    }
}

pub(crate) struct Authority<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) world: &'a dyn World,
    /// `email:<account>`.
    pub(crate) component: &'a str,
    pub(crate) clock: &'a dyn Clock,
}

/// The answer when a write of the state failed: nothing changed.
const NOT_RECORDED: &str = "Could not record that; nothing was done. Try again.";

impl Authority<'_> {
    /// Answer every request for the life of the run. Only a failed state
    /// write ends it, after the request is answered.
    pub(crate) async fn serve(&self, mut requests: mpsc::Receiver<MailRequest>) -> Result<()> {
        while let Some(request) = requests.recv().await {
            let (reply, failed) = match self.handle(request.work).await {
                Ok(reply) => (reply, None),
                Err(error) => (MailReply::Text(NOT_RECORDED.to_owned()), Some(error)),
            };
            if let Err(MailReply::Text(text)) = request.reply.send(reply) {
                // The chat stopped waiting: tell it once the answer is known.
                let now = self.clock.now();
                self.ledger
                    .note(
                        &format!("response:late:{}", super::ledger::random_hex()),
                        super::plan::Class::Response,
                        text,
                        now,
                    )
                    .await?;
            }
            if let Some(error) = failed {
                return Err(error);
            }
        }
        std::future::pending().await
    }

    pub(crate) async fn handle(&self, work: MailWork) -> Result<MailReply> {
        let now = self.clock.now();
        Ok(match work {
            MailWork::Order(MailOrder::List) => {
                MailReply::Actions(self.ledger.listed(self.component))
            }
            MailWork::Order(MailOrder::Cancel { action }) => {
                MailReply::Text(self.ledger.cancel(action.as_deref(), now).await?)
            }
            MailWork::Chat { command, evidence } => {
                MailReply::Text(self.command(command, &evidence, now).await?)
            }
        })
    }

    async fn command(
        &self,
        command: MailCommand,
        evidence: &ChatEvidence,
        now: u64,
    ) -> Result<String> {
        let Some(policy) = self.ledger.actions_policy() else {
            return Ok(crate::mail_chat::NOT_RUNNING_REPLY.to_owned());
        };
        // Every other command prepares an action, which only the owner of a
        // chat this account reports to may ask for.
        let prepares = !matches!(
            command,
            MailCommand::Approve(_)
                | MailCommand::Deny(_)
                | MailCommand::DenyAll
                | MailCommand::Status
        );
        if prepares
            && !(policy.routes.contains(&evidence.route)
                && self.world.owner(&evidence.route).as_deref() == Some(evidence.peer.as_str()))
        {
            return Ok(format!(
                "{} does not report to this chat; ask in its own mail chat. Nothing was done.",
                self.component
            ));
        }
        let key = format!("owner:{}:{}", evidence.route, evidence.message_id);
        let work = match command {
            MailCommand::Approve(codes) => {
                let lines = self
                    .ledger
                    .approve(&codes, evidence, self.world, now)
                    .await?;
                return Ok(lines.join("\n"));
            }
            MailCommand::Deny(codes) => {
                return Ok(self
                    .ledger
                    .deny(Some(&codes), evidence, self.world, now)
                    .await?
                    .join("\n"));
            }
            MailCommand::DenyAll => {
                return Ok(self
                    .ledger
                    .deny(None, evidence, self.world, now)
                    .await?
                    .join("\n"));
            }
            MailCommand::Status => return Ok(self.ledger.status_lines(self.component)),
            MailCommand::Reply { handle, text } => {
                if !(policy.offers(ActionKind::Draft, Some(Form::Reply))
                    || policy.offers(ActionKind::Send, Some(Form::Reply)))
                {
                    return Ok(off(self.component, "replies"));
                }
                if self.ledger.handle(&handle).is_none() {
                    return Ok(unknown_handle(&handle));
                }
                RequestWork::Reply { handle, text }
            }
            MailCommand::Forward { handle, to, note } => {
                if !(policy.offers(ActionKind::Draft, Some(Form::Forward))
                    || policy.offers(ActionKind::Send, Some(Form::Forward)))
                {
                    return Ok(off(self.component, "forwards"));
                }
                if self.ledger.handle(&handle).is_none() {
                    return Ok(unknown_handle(&handle));
                }
                if let Some(refusal) = recipients_refusal(&to, policy) {
                    return Ok(refusal);
                }
                RequestWork::Forward { handle, to, note }
            }
            MailCommand::Compose { to, text, .. } => {
                if !(policy.offers(ActionKind::Draft, Some(Form::Compose))
                    || policy.offers(ActionKind::Send, Some(Form::Compose)))
                {
                    return Ok(off(self.component, "new mail"));
                }
                if let Some(refusal) = recipients_refusal(&to, policy) {
                    return Ok(refusal);
                }
                RequestWork::Compose { to, text }
            }
            MailCommand::Revise { code, text } => {
                let snapshot = self.ledger.snapshot();
                let Some(entry) = snapshot.actions.iter().find(|entry| entry.code == code) else {
                    return Ok(format!("No draft waiting for your answer has code {code}."));
                };
                if entry.form.is_none() || entry.state.terminal() {
                    return Ok(format!(
                        "{code} is not a reply, forward, or new mail waiting for your answer."
                    ));
                }
                if !super::ledger::actions::may_deny(entry, evidence, self.world, policy) {
                    return Ok(format!(
                        "{code} was sent to another chat; revise it there. Nothing was done."
                    ));
                }
                RequestWork::Revise {
                    action: entry.id.clone(),
                    text,
                }
            }
            MailCommand::Message { handle, action } => {
                let (kind, what) = match action {
                    MessageAction::Archive => (ActionKind::Archive, "archiving"),
                    MessageAction::Read => (ActionKind::MarkRead, "marking mail read"),
                    MessageAction::Trash => (ActionKind::Trash, "moving mail to Trash"),
                    MessageAction::Spam => (ActionKind::Spam, "moving mail to Spam"),
                };
                if !policy.offers(kind, None) {
                    return Ok(off(self.component, what));
                }
                if self.ledger.handle(&handle).is_none() {
                    return Ok(unknown_handle(&handle));
                }
                RequestWork::Message { handle, kind }
            }
        };
        let preparing = match &work {
            RequestWork::Reply { handle, .. } => format!("Preparing a reply to #{handle}"),
            RequestWork::Forward { handle, .. } => format!("Preparing the forward of #{handle}"),
            RequestWork::Compose { .. } => "Preparing the new mail".to_owned(),
            RequestWork::Revise { .. } => "Preparing the revised draft".to_owned(),
            RequestWork::Message { handle, kind } => format!(
                "Preparing {}",
                super::ledger::actions::describe(*kind, None, Some(handle))
            ),
        };
        let added = self
            .ledger
            .add_request(Request {
                key,
                work,
                route: evidence.route.clone(),
                attempts: 0,
                at: now,
            })
            .await?;
        Ok(match added {
            Admission::Added => format!("{preparing}; its preview with codes to approve follows here."),
            Admission::Duplicate => "That request is already being prepared.".to_owned(),
            Admission::Full => {
                "Too many requests are being prepared; nothing was done. Try again in a few minutes."
                    .to_owned()
            }
        })
    }
}

fn off(component: &str, what: &str) -> String {
    format!(
        "{component} does not take {what}: it is off in its settings, or the mailbox cannot; nothing was done."
    )
}

fn unknown_handle(handle: &str) -> String {
    format!("No reported mail has handle #{handle} any more; nothing was done.")
}

/// Why the owner's typed recipients cannot be used, in SCV's words
/// without the addresses themselves.
fn recipients_refusal(to: &[String], policy: &super::ledger::actions::Policy) -> Option<String> {
    let Some(valid) = super::compose::typed_recipients(to, policy.actions.max_recipients) else {
        return Some(format!(
            "Not done: name 1 to {} plain addresses, such as name@example.com.",
            policy.actions.max_recipients
        ));
    };
    let valid: Vec<&str> = valid.iter().map(String::as_str).collect();
    policy
        .addresses_refused(&valid)
        .map(|why| format!("Not done: {why}."))
}

#[cfg(test)]
mod tests;
