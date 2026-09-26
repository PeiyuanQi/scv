//! Yes/no questions to the owner in chat, which `scv confirm` asks before an
//! irreversible step, such as a delegated agent publishing SCV to crates.io.
//!
//! A question goes to the chat that started the work the asker runs inside
//! (its `SCV_PARENT` chain names the delegation, whose daemon session a chat
//! bridge answers), or else to the owner chat an unprompted notice would go
//! to. It is sent through the account's durable outbox and held in the
//! channel hub, where the owner's next explicit yes or no in that chat
//! answers it without starting a turn. The asker follows it with
//! `confirm_status`; a question nobody follows for [`LEASE`] is withdrawn.
//! Questions live in memory only: a daemon restart drops them.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use scv_channels::hub::{Hub, Origin};
use scv_protocol::{ConfirmInfo, ConfirmState, DEFAULT_CONFIRM_SECONDS, MAX_CONFIRM_SECONDS};
use scv_tools::delegation::DelegationRegistry;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::restart::Notifier;

/// The longest question, in bytes.
const MAX_QUESTION_BYTES: usize = 4 * 1024;
/// A question nobody asks about for this long is withdrawn: its asker
/// stopped waiting, so an answer would reach no one.
const LEASE: Duration = Duration::from_secs(60);
/// How long a settled question's outcome stays readable.
const KEEP_SETTLED: Duration = Duration::from_secs(10 * 60);
/// How often a waiting question checks its deadline and lease.
const TICK: Duration = Duration::from_secs(1);

/// What the chat is told when no answer came in time.
pub(crate) const NO_ANSWER: &str = "No answer, so stopped.";
/// What the chat is told when the asker stopped waiting.
pub(crate) const WITHDRAWN: &str = "The question was withdrawn, so stopped.";

/// The message that asks `question`, waiting `seconds` for an answer.
pub(crate) fn question_text(question: &str, seconds: u64) -> String {
    let minutes = seconds.div_ceil(60).max(1);
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("{question}\n\nReply yes or no. No answer in {minutes} {unit} counts as no.")
}

struct Entry {
    info: ConfirmInfo,
    /// When the asker last asked about it.
    polled: Instant,
    /// When it was answered, ran out, or failed.
    settled: Option<Instant>,
}

/// Asks the owner and keeps each question's outcome for its asker.
pub(crate) struct Confirmer {
    hub: Arc<Hub>,
    registry: Arc<DelegationRegistry>,
    notifier: Notifier,
    cancel: CancellationToken,
    lease: Duration,
    entries: Mutex<HashMap<String, Entry>>,
}

impl Confirmer {
    pub(crate) fn new(
        hub: Arc<Hub>,
        registry: Arc<DelegationRegistry>,
        notifier: Notifier,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            hub,
            registry,
            notifier,
            cancel,
            lease: LEASE,
            entries: Mutex::default(),
        })
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Handle `confirm_ask`: find the chat, hold the question there, and
    /// send it in the background. The error is shown to the caller.
    pub(crate) async fn ask(
        self: &Arc<Self>,
        question: &str,
        parent: Option<&str>,
        timeout_seconds: Option<u64>,
    ) -> Result<ConfirmInfo, String> {
        let question = question.trim();
        if question.is_empty() {
            return Err("the question is empty".into());
        }
        if question.len() > MAX_QUESTION_BYTES {
            return Err(format!(
                "the question is longer than {MAX_QUESTION_BYTES} bytes"
            ));
        }
        let seconds = timeout_seconds
            .unwrap_or(DEFAULT_CONFIRM_SECONDS)
            .clamp(1, MAX_CONFIRM_SECONDS);
        let chat = self.chat(parent).await?;
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let answer = self
            .hub
            .ask(&id, &chat.component, &chat.peer)
            .ok_or_else(|| {
                format!(
                    "a question is already waiting for the owner's answer on {}",
                    chat.component
                )
            })?;
        let info = ConfirmInfo {
            id: id.clone(),
            state: ConfirmState::Pending,
            chat: chat.component.clone(),
            deadline_unix_seconds: unix_now() + seconds,
        };
        {
            let mut entries = self.entries();
            prune(&mut entries);
            entries.insert(
                id.clone(),
                Entry {
                    info: info.clone(),
                    polled: Instant::now(),
                    settled: None,
                },
            );
        }
        tracing::info!("Asking the owner on {} (question {id})", chat.component);
        let confirmer = Arc::clone(self);
        let text = question_text(question, seconds);
        tokio::spawn(async move { confirmer.follow(id, chat, text, answer).await });
        Ok(info)
    }

    /// Handle `confirm_status`: where question `id` stands. Asking keeps
    /// the question alive.
    pub(crate) fn status(&self, id: &str) -> Result<ConfirmInfo, String> {
        let mut entries = self.entries();
        prune(&mut entries);
        let entry = entries.get_mut(id).ok_or_else(|| {
            format!(
                "no question {id} is known here; the daemon may have restarted since it was asked"
            )
        })?;
        entry.polled = Instant::now();
        Ok(entry.info.clone())
    }

    /// The owner's direct chat to ask in: the chat that started the
    /// delegation `parent` names, or else the notify target.
    async fn chat(&self, parent: Option<&str>) -> Result<Origin, String> {
        let started = parent
            .and_then(|chain| self.registry.own_run(chain))
            .and_then(|run| self.hub.origin(&run.session));
        let chat = match started {
            Some(chat) => chat,
            None => self.notifier.owner_chat().await.ok_or(
                "no owner chat to ask in: this work did not start in a chat, and no [notify] \
                 owner account (or the owner's last chat) is connected",
            )?,
        };
        match self.hub.owner(&chat.component) {
            Some(Some(owner)) if owner == chat.peer => Ok(chat),
            Some(_) => Err(format!(
                "only the account owner's direct chat can answer, and the chat on {} is not it",
                chat.component
            )),
            None => Err(format!("{} is not running", chat.component)),
        }
    }

    /// Send the question, then wait for its answer, deadline, lease, or the
    /// daemon's shutdown, and record the outcome.
    async fn follow(
        self: Arc<Self>,
        id: String,
        chat: Origin,
        text: String,
        mut answer: oneshot::Receiver<bool>,
    ) {
        let stored = tokio::select! {
            () = self.cancel.cancelled() => {
                self.hub.withdraw(&id);
                return;
            }
            stored = self.hub.notify(&chat.component, &chat.peer, &text) => stored,
        };
        match stored {
            // In the outbox: the owner's answer counts from now on.
            Ok(()) => self.hub.open(&id),
            Err(error) => {
                tracing::warn!("Question {id} for {} not sent: {error}", chat.component);
                self.hub.withdraw(&id);
                self.settle(&id, ConfirmState::Failed);
                return;
            }
        }
        loop {
            let answered = tokio::select! {
                // The daemon stops: the question goes with it.
                () = self.cancel.cancelled() => {
                    self.hub.withdraw(&id);
                    return;
                }
                answered = &mut answer => Some(answered),
                () = tokio::time::sleep(TICK) => None,
            };
            match answered {
                Some(Ok(yes)) => {
                    let state = if yes {
                        ConfirmState::Yes
                    } else {
                        ConfirmState::No
                    };
                    tracing::info!("The owner answered question {id}: {state:?}");
                    self.settle(&id, state);
                    return;
                }
                Some(Err(_)) => {
                    tracing::warn!("The answer to question {id} was lost");
                    self.settle(&id, ConfirmState::Failed);
                    return;
                }
                None => {}
            }
            let Some((state, reply)) = self.overdue(&id) else {
                continue;
            };
            // Unless an answer is already on its way, which the next pass
            // receives.
            if self.hub.withdraw(&id) {
                tracing::info!("Question {id} ended without an answer: {state:?}");
                self.settle(&id, state);
                tokio::select! {
                    () = self.cancel.cancelled() => {}
                    told = self.hub.notify(&chat.component, &chat.peer, reply) => {
                        if let Err(error) = told {
                            tracing::warn!("Could not tell {} how question {id} ended: {error}", chat.component);
                        }
                    }
                }
                return;
            }
        }
    }

    /// Whether question `id` ran out of time or lost its asker, with what
    /// the chat is told.
    fn overdue(&self, id: &str) -> Option<(ConfirmState, &'static str)> {
        let entries = self.entries();
        let entry = entries.get(id)?;
        if unix_now() >= entry.info.deadline_unix_seconds {
            Some((ConfirmState::Expired, NO_ANSWER))
        } else if entry.polled.elapsed() >= self.lease {
            Some((ConfirmState::Withdrawn, WITHDRAWN))
        } else {
            None
        }
    }

    fn settle(&self, id: &str, state: ConfirmState) {
        if let Some(entry) = self.entries().get_mut(id) {
            entry.info.state = state;
            entry.settled = Some(Instant::now());
        }
    }
}

/// Forget outcomes nobody read in time.
fn prune(entries: &mut HashMap<String, Entry>) {
    entries.retain(|_, entry| {
        entry
            .settled
            .is_none_or(|settled| settled.elapsed() < KEEP_SETTLED)
    });
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
