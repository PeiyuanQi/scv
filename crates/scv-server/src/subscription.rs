//! The daemon's task subscriptions: owner messages on a bound chat thread
//! go to that job, and a restart reloads the records without resuming it.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use std::collections::HashSet;

use scv_channels::{FollowUp, ReturnedFollowUp, TaskFollowUps};
use scv_client::Layout;
use scv_tools::background::subscription::{
    Error as SubscriptionError, HeldMessage, Incoming, Plan, Subscription, Subscriptions, Worker,
    acceptance_token,
};
use scv_tools::delegation::DelegationRegistry;

use crate::workbench::Hosts;

/// Subscriptions for this process, and the workbench that can deliver to a
/// live job or resume a finished one.
pub(crate) struct FollowUps {
    book: Mutex<Subscriptions>,
    hosts: Mutex<Option<Arc<Hosts>>>,
    /// Messages whose delivery is in progress, so a retry does not send twice.
    sending: Mutex<HashSet<String>>,
    /// Where account settings live, so a retry can read the current grant.
    layout: Layout,
}

impl FollowUps {
    /// Load the records. An interrupted job stays interrupted.
    pub(crate) fn open(layout: &Layout) -> Self {
        let book = Subscriptions::open(&layout.subscriptions(), &layout.jobs());
        tracing::info!(
            count = book.len(),
            "loaded task subscriptions; an interrupted job stays interrupted until its owner messages the bound thread"
        );
        Self {
            book: Mutex::new(book),
            hosts: Mutex::new(None),
            sending: Mutex::new(HashSet::new()),
            layout: layout.clone(),
        }
    }

    /// Bind `thread` to an existing job. `who` is this daemon's session
    /// owner, not a name the caller supplies. Does not start a job.
    pub(crate) fn bind_owner(
        &self,
        thread: &str,
        session: &str,
        job: &str,
        why: &str,
    ) -> Result<Subscription, String> {
        lock(&self.book)
            .bind(thread, session, job, &session_owner(), why)
            .map_err(|error| error.to_string())
    }

    /// Stop delivery on `thread`. The record stays.
    pub(crate) fn stop(&self, thread: &str) -> Result<(), String> {
        lock(&self.book)
            .stop(thread)
            .map_err(|error| error.to_string())
    }

    /// Deliver `extra` to the job `thread` already subscribes to. The
    /// authorization is unchanged. The worker can drop `extra`.
    pub(crate) fn attach_thread(&self, thread: &str, extra: &str) -> Result<(), String> {
        lock(&self.book)
            .attach(thread, extra)
            .map_err(|error| error.to_string())
    }

    /// The worker says the message is not the job's work. `parent` is the
    /// caller's `SCV_PARENT` chain, used when the worker does not name the
    /// message: the follow-up that job's own ledger record accepted.
    pub(crate) fn hand_back_message(
        &self,
        registry: &DelegationRegistry,
        thread: Option<&str>,
        message_id: Option<&str>,
        parent: Option<&str>,
    ) -> Result<String, String> {
        let (thread, message_id) = if let (Some(thread), Some(message_id)) = (thread, message_id) {
            (thread.to_owned(), message_id.to_owned())
        } else {
            let run = parent
                .and_then(|chain| registry.own_run(chain))
                .ok_or_else(|| {
                    "name the thread and message, or call this from the subscribed job".to_owned()
                })?;
            let job = self.job_of_run(&run.session, &run.handle).ok_or_else(|| {
                "the running job has not recorded which follow-up it was offered".to_owned()
            })?;
            lock(&self.book)
                .latest_offer(&run.session, &job)
                .ok_or_else(|| "this job has no follow-up waiting to be handed back".to_owned())?
        };
        let text = {
            let book = lock(&self.book);
            book.subscription(&thread)
                .and_then(|record| {
                    record
                        .offers
                        .iter()
                        .chain(record.pending.iter())
                        .find(|held| held.message_id == message_id)
                        .map(|held| held.text.clone())
                })
                .unwrap_or_default()
        };
        let plan = lock(&self.book)
            .hand_back(&thread, &message_id, &text)
            .map_err(|error| error.to_string())?;
        match plan {
            Plan::HandedBack(handed) => Ok(format!(
                "Handed back {}.",
                handed.dropped.as_deref().unwrap_or("the message")
            )),
            Plan::Intake => Ok("The message goes to ordinary intake.".into()),
            Plan::Withheld => Ok("The subscription is stopped; nothing was delivered.".into()),
            Plan::Settled => Ok("That message was already settled.".into()),
            Plan::Deliver(_) | Plan::Resume(_) => {
                Err("the message is still this job's work".into())
            }
        }
    }

    pub(crate) fn set_hosts(&self, hosts: Arc<Hosts>) {
        *lock(&self.hosts) = Some(hosts);
    }

    /// Messages whose decision could not be stored, as `(thread, id, text)`.
    #[cfg(test)]
    pub(crate) fn unrecorded(&self) -> Vec<(String, String, String)> {
        lock(&self.book)
            .unrecorded_messages()
            .into_iter()
            .map(|held| (held.thread, held.message_id, held.text))
            .collect()
    }

    /// Read the directory again. Does not resume a job.
    pub(crate) fn reload(&self) {
        lock(&self.book).reload();
    }

    /// Deliver messages that were recorded and not yet accepted, and messages
    /// whose decision could not be stored. A restart calls this; it does not
    /// resume a job that has no pending message. A message is given to the
    /// job only while the account that received it still grants tools to
    /// that thread's chat. Otherwise it stays pending.
    pub(crate) async fn retry_pending(&self) {
        let pending = lock(&self.book).pending_messages();
        let unrecorded = lock(&self.book).unrecorded_messages();
        let mut seen = HashSet::new();
        let mut batch = Vec::new();
        for held in pending.into_iter().chain(unrecorded) {
            if seen.insert((held.thread.clone(), held.message_id.clone())) {
                batch.push(held);
            }
        }
        for held in batch {
            if !self.grant_allows(&held) {
                tracing::debug!(
                    "subscribed follow-up stays pending; the account does not grant tools"
                );
                continue;
            }
            let outcome = self.follow_up_in(&held).await;
            let recorded = lock(&self.book).pending_messages().iter().any(|pending| {
                pending.thread == held.thread && pending.message_id == held.message_id
            });
            if recorded
                || !matches!(
                    outcome,
                    FollowUp::Held | FollowUp::Unstored | FollowUp::Unresumable
                )
            {
                let _ = lock(&self.book).forget_unrecorded(&held.thread, &held.message_id);
            }
        }
    }

    fn hosts(&self) -> Option<Arc<Hosts>> {
        lock(&self.hosts).clone()
    }

    /// The account that received `held` still grants tools to its chat.
    /// The same rule as the bridge's first look: `remote_tools = "owner"`
    /// and the thread's chat is that owner. A message with no account cannot
    /// be checked, so it is not given to the job.
    fn grant_allows(&self, held: &HeldMessage) -> bool {
        let Some(account) = held
            .account
            .as_deref()
            .filter(|account| !account.is_empty())
        else {
            return false;
        };
        scv_channels::follow_up_grant_holds(&self.layout, account, &held.thread)
    }

    /// `Ok(Some(successor))` when delivery or resume started `<session>/<job>`.
    /// `Ok(None)` when the running turn took the prompt and no new job started.
    async fn carry_out(&self, plan: Plan) -> Result<Option<String>, String> {
        match plan {
            Plan::Deliver(delivery) => self.deliver(delivery).await,
            Plan::Resume(resume) => self.resume(resume).await,
            Plan::Intake | Plan::Withheld | Plan::Settled | Plan::HandedBack(_) => Ok(None),
        }
    }

    async fn deliver(
        &self,
        mut delivery: scv_tools::background::subscription::Delivery,
    ) -> Result<Option<String>, String> {
        let hosts = self
            .hosts()
            .ok_or_else(|| "task subscription has no workbench yet".to_owned())?;
        if delivery.conversation.is_none() {
            let (session, job) = (delivery.session.clone(), delivery.job.clone());
            let workbench = Arc::clone(hosts.workbench());
            delivery.conversation =
                tokio::task::spawn_blocking(move || workbench.job_conversation(&session, &job))
                    .await
                    .map_err(|error| error.to_string())?;
        }
        let conversation = delivery
            .conversation
            .ok_or_else(|| format!("subscribed job {} has no conversation yet", delivery.job))?;
        let from = format!("{}/{}", delivery.session, delivery.job);
        let token = acceptance_token(&delivery.thread, &delivery.message_id);
        match hosts
            .workbench()
            .deliver_to_job(
                &delivery.session,
                &conversation,
                &delivery.prompt,
                Some(from),
                Some(token),
            )
            .await
        {
            Ok(scv_tools::workbench::Sent::Job(successor)) => Ok(Some(successor)),
            Ok(scv_tools::workbench::Sent::Steered) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn resume(
        &self,
        resume: scv_tools::background::subscription::Resume,
    ) -> Result<Option<String>, String> {
        let hosts = self
            .hosts()
            .ok_or_else(|| "task subscription has no workbench yet".to_owned())?;
        let from = format!("{}/{}", resume.session, resume.job);
        let token = acceptance_token(&resume.thread, &resume.message_id);
        if let Some(conversation) = &resume.conversation {
            match hosts
                .workbench()
                .deliver_to_job(
                    &resume.session,
                    conversation,
                    &resume.prompt,
                    Some(from.clone()),
                    Some(token.clone()),
                )
                .await
            {
                Ok(scv_tools::workbench::Sent::Job(successor)) => return Ok(Some(successor)),
                Ok(scv_tools::workbench::Sent::Steered) => return Ok(None),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "subscribed job's session did not take the follow-up; resuming it"
                    );
                }
            }
        }
        let host = hosts
            .session_for(&resume.workspace.display().to_string())
            .await
            .map_err(|error| format!("{error:#}"))?;
        let resumed = hosts
            .workbench()
            .resume_with(
                &host,
                &resume.session,
                &resume.job,
                &resume.prompt,
                Some(token),
            )
            .await?;
        Ok(Some(resumed))
    }

    fn job_of_run(&self, session: &str, handle: &str) -> Option<String> {
        let hosts = self.hosts()?;
        let (found_session, job) = hosts.workbench().job_for_run(handle)?;
        (found_session == session).then_some(job)
    }
}

#[async_trait]
impl TaskFollowUps for FollowUps {
    async fn follow_up(
        &self,
        account: &str,
        thread: &str,
        message_id: &str,
        text: &str,
        reply_to: &str,
        thread_reply: &str,
    ) -> FollowUp {
        self.follow_up_in(&HeldMessage {
            message_id: message_id.to_owned(),
            thread: thread.to_owned(),
            text: text.to_owned(),
            account: Some(account.to_owned()),
            reply_to: reply_to.to_owned(),
            thread_reply: thread_reply.to_owned(),
        })
        .await
    }

    async fn take_returns(&self, account: &str, owner: &str) -> Vec<ReturnedFollowUp> {
        lock(&self.book)
            .take_returns(account, owner)
            .into_iter()
            .map(|held| ReturnedFollowUp {
                thread: held.thread,
                message_id: held.message_id,
                text: held.text,
                reply_to: held.reply_to,
                thread_reply: held.thread_reply,
            })
            .collect()
    }

    async fn ack_returns(&self, account: &str, owner: &str, claimed: &[(&str, &str)]) {
        if let Err(error) = lock(&self.book).ack_returns(account, owner, claimed) {
            tracing::warn!(%error, "could not drop a claimed subscription return");
        }
    }
}

impl FollowUps {
    async fn follow_up_in(&self, held: &HeldMessage) -> FollowUp {
        let thread = held.thread.as_str();
        let message_id = held.message_id.as_str();
        let incoming = Incoming {
            account: held.account.as_deref(),
            thread,
            message_id,
            text: &held.text,
            reply_to: &held.reply_to,
            thread_reply: &held.thread_reply,
        };
        let plan = lock(&self.book).decide_account(incoming, Worker::Accept);
        match plan {
            // The history cannot be trusted, or the decision could not be
            // stored. Remember the message and retry it. An ordinary turn
            // must not run an id we might already have accepted. If even
            // that list cannot be written, the bridge must not mark it seen.
            Err(SubscriptionError::Unreadable | SubscriptionError::Store(_)) => {
                let already = lock(&self.book).unrecorded_contains(thread, message_id);
                match lock(&self.book).remember_unrecorded(Incoming {
                    account: held.account.as_deref(),
                    thread,
                    message_id,
                    text: &held.text,
                    reply_to: &held.reply_to,
                    thread_reply: &held.thread_reply,
                }) {
                    Ok(()) => {
                        if !already {
                            tracing::warn!(
                                "task subscription could not record the message; it stays pending"
                            );
                        }
                        FollowUp::Held
                    }
                    Err(error) => {
                        tracing::warn!(%error, "task subscription could not keep the message");
                        FollowUp::Unstored
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "task subscription did not take the message");
                FollowUp::Turn
            }
            Ok(Plan::Settled) => FollowUp::Settled,
            // Already handed back, or already released to the return queue.
            // That queue runs ordinary intake once. A thread with no
            // subscription is ordinary intake now: a turn.
            Ok(Plan::HandedBack(_)) => FollowUp::Settled,
            Ok(Plan::Intake) => {
                if lock(&self.book).subscription(thread).is_none() {
                    FollowUp::Turn
                } else {
                    FollowUp::Settled
                }
            }
            Ok(Plan::Withheld) => FollowUp::Turn,
            Ok(plan @ (Plan::Deliver(_) | Plan::Resume(_))) => {
                let key = format!("{thread}\0{message_id}");
                if !lock(&self.sending).insert(key.clone()) {
                    return FollowUp::Held;
                }
                let outcome = self.carry_out(plan).await;
                lock(&self.sending).remove(&key);
                match outcome {
                    Ok(successor) => {
                        let mut book = lock(&self.book);
                        if let Err(error) = book.commit_delivery(thread, message_id) {
                            tracing::warn!(%error, "could not record a delivered follow-up");
                            return FollowUp::Held;
                        }
                        if let Some(successor) = successor
                            && let Err(error) = book.note_current(thread, &successor)
                        {
                            tracing::warn!(%error, "could not record the subscribed continuation");
                        }
                        FollowUp::Delivered
                    }
                    Err(error) => {
                        if error == scv_tools::workbench::UNRESUMABLE_FOLLOW_UP {
                            tracing::debug!(
                                %error,
                                "subscribed follow-up stays pending; the job's conversation cannot be resumed"
                            );
                            FollowUp::Unresumable
                        } else {
                            tracing::warn!(%error, "subscribed message stays pending");
                            FollowUp::Held
                        }
                    }
                }
            }
        }
    }
}

/// The person who owns this daemon process. The subscription records that
/// name; a caller does not get to choose it.
fn session_owner() -> String {
    for variable in ["USER", "LOGNAME"] {
        if let Ok(name) = std::env::var(variable) {
            let name = name.trim();
            if !name.is_empty() && name.len() <= 200 && !name.bytes().any(|byte| byte < b' ') {
                return name.to_owned();
            }
        }
    }
    "owner".to_owned()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
