//! Yes/no questions to the owner in chat, which `scv confirm` asks before an
//! irreversible step, `scv policy check publish` asks past the publish
//! limit, and the approval gates ask for a request only the owner may answer
//! ([`Confirmer::ask_owner`]).
//!
//! A question goes to the chat that started the work the asker runs inside
//! (its `SCV_PARENT` chain names the delegation, whose daemon session a chat
//! bridge answers, or a gate names its session), or else to the owner chat
//! an unprompted notice would go to. It is sent through the account's
//! durable outbox and held in the channel hub. Once the bridge has
//! delivered its text, the owner's next explicit yes or no in that chat,
//! sent after that, answers it without starting a turn; text the platform
//! refuses, or that is still undelivered at the deadline, fails the
//! question, since the owner was never asked.
//! The asker follows it with `confirm_status`; a question nobody follows for
//! [`LEASE`] is withdrawn. Questions live in memory only: a daemon restart
//! drops them.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use scv_channels::hub::{Hub, Origin, Withdrawal};
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
/// How often a waiting question checks its deadline and lease, and a gate
/// waiting on one follows it.
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
        let session = parent
            .and_then(|chain| self.registry.own_run(chain))
            .map(|run| run.session);
        self.ask_in(question, session.as_deref(), timeout_seconds)
            .await
    }

    /// Ask the owner `question` for the work of daemon session `session`
    /// and wait for the answer, following it as `scv confirm` does: yes only
    /// on an explicit yes before the deadline ([`DEFAULT_CONFIRM_SECONDS`]).
    /// A question that cannot be asked, or a cancelled wait, is a no.
    pub(crate) async fn ask_owner(
        self: &Arc<Self>,
        question: &str,
        session: &str,
        cancellation: &CancellationToken,
    ) -> bool {
        let info = match self.ask_in(question, Some(session), None).await {
            Ok(info) => info,
            Err(error) => {
                tracing::warn!("Could not ask the owner for session {session}: {error}");
                return false;
            }
        };
        loop {
            match self.status(&info.id).map(|info| info.state) {
                Ok(ConfirmState::Pending) => {}
                Ok(state) => return state == ConfirmState::Yes,
                Err(_) => return false,
            }
            tokio::select! {
                // Unfollowed, the question is withdrawn after its lease.
                () = cancellation.cancelled() => return false,
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }

    /// Ask `question` in the chat of daemon session `session`, or else the
    /// owner chat.
    async fn ask_in(
        self: &Arc<Self>,
        question: &str,
        session: Option<&str>,
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
        let chat = self.chat(session).await?;
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

    /// The owner's direct chat to ask in: the chat that started daemon
    /// session `session`, or else the notify target.
    async fn chat(&self, session: Option<&str>) -> Result<Origin, String> {
        let started = session.and_then(|session| self.hub.origin(session));
        let chat = match started {
            Some(chat) => chat,
            None => self.notifier.owner_chat().await.ok_or(
                "no owner chat to ask in: this work did not start in a chat, and no [notify] \
                 owner account (or the owner's last chat) is connected",
            )?,
        };
        if self.hub.is_mail_chat(&chat.component) {
            return Err(format!(
                "{} is a mail chat, which carries only mail and takes no questions",
                chat.component
            ));
        }
        match self.hub.owner(&chat.component) {
            Some(Some(owner)) if owner == chat.peer => Ok(chat),
            Some(_) => Err(format!(
                "only the account owner's direct chat can answer, and the chat on {} is not it",
                chat.component
            )),
            None => Err(format!("{} is not running", chat.component)),
        }
    }

    /// Queue the question, then wait for its answer, deadline, lease, or the
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
            stored = self.hub.send_question(&id, &chat.component, &chat.peer, &text) => stored,
        };
        // In the outbox, the question opens once the bridge delivers it.
        if let Err(error) = stored {
            tracing::warn!("Question {id} for {} not sent: {error}", chat.component);
            self.hub.withdraw(&id);
            self.settle(&id, ConfirmState::Failed);
            return;
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
                    tracing::warn!(
                        "Question {id} ended unanswered: the platform refused it, or the answer was lost"
                    );
                    self.settle(&id, ConfirmState::Failed);
                    return;
                }
                None => {}
            }
            let Some((state, reply)) = self.overdue(&id) else {
                continue;
            };
            match self.hub.withdraw(&id) {
                // An answer or a failure is already on its way, which the
                // next pass receives.
                Withdrawal::Settled => {}
                // The owner saw it: tell the chat it is over.
                Withdrawal::Unanswered => {
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
                // The owner never saw it, and the bridge drops it unsent:
                // nothing was asked.
                Withdrawal::Unsent => {
                    let state = if state == ConfirmState::Expired {
                        ConfirmState::Failed
                    } else {
                        state
                    };
                    tracing::warn!(
                        "Question {id} never reached {} before it ended: {state:?}",
                        chat.component
                    );
                    self.settle(&id, state);
                    return;
                }
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
