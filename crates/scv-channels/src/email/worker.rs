//! Reading the mailbox and deciding each new message, one at a time.
//!
//! The worker owns the read-only mailbox connection. Each check claims what
//! arrived (moving the cursor in the same write), prepares the owner's
//! waiting requests (they come first: the owner is waiting), then decides
//! every claim up the ladder, spending tokens only once cheaper rungs pass:
//! identity and age, the rules, the budget, and only then one fresh
//! tool-free model turn for that one message. What it decides is recorded
//! in one write that also releases the claim. With mail actions on, each
//! report names its message by a handle, and a move triage suggests
//! becomes an action the owner may approve, on that message only.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

use super::clean;
use super::content::{ActionKind, Form, Origin};
use super::ledger::actions::{NewAction, REQUEST_ATTEMPTS};
use super::ledger::{
    Arrival, Decided, Handle, Ledger, MAX_ATTEMPTS, MAX_CLAIMS, MODEL_ATTEMPTS, Outcome, ResetNote,
};
use super::model::{self, TurnError};
use super::parse;
use super::plan::Class;
use super::prepare::{self, Actions, Prepared, Preparer, SourceLost};
use super::render::{self, Summary};
use super::rules;
use super::settings::{MailSettings, RuleAction, Suggestion};
use super::source::{Changes, FolderNames, MailSource, Meta, SourceRef};
use super::triage;
use super::{Clock, LowSpace};

/// How long one triage turn may take.
pub(crate) const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a check stopped.
#[derive(Debug)]
pub(crate) enum CheckError {
    /// The mailbox connection failed; the worker reconnects.
    Source(anyhow::Error),
    /// The state could not be written; the run fails.
    State(anyhow::Error),
}

type Checked<T> = std::result::Result<T, CheckError>;

fn state<T>(result: Result<T>) -> Checked<T> {
    result.map_err(CheckError::State)
}

fn source<T>(result: Result<T>) -> Checked<T> {
    result.map_err(CheckError::Source)
}

/// What the worker needs besides its connection.
pub(crate) struct Worker<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) settings: &'a MailSettings,
    /// The daemon socket triage sessions connect to.
    pub(crate) socket: &'a Path,
    /// The empty private directory triage sessions start in.
    pub(crate) cwd: &'a Path,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) low_space: &'a LowSpace,
    /// The whole system prompt of this account's triage sessions.
    pub(crate) frame: String,
    /// What a triage answer may carry beyond its report.
    pub(crate) options: triage::Options,
    /// With mail actions on: what preparing them needs.
    pub(crate) actions: Option<&'a Actions<'a>>,
}

impl Worker<'_> {
    /// Keep checking the mailbox every `poll_seconds` for the life of the
    /// run, connecting through `connect` and reconnecting with backoff when
    /// the connection fails. Only a failed state write ends it.
    pub(crate) async fn watch<S, F, Fut>(
        &self,
        connect: F,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<()>
    where
        S: MailSource,
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<S>>,
    {
        let mut backoff = crate::retry::Backoff::new();
        let poll = Duration::from_secs(self.settings.poll_seconds);
        loop {
            let mut source = match connect().await {
                Ok(mut source) => {
                    let caps = source.caps();
                    tracing::info!(
                        idle = caps.push,
                        move_ = caps.move_,
                        uidplus = caps.uidplus,
                        special_use = caps.special_use,
                        "opened the mailbox read-only"
                    );
                    if let Err(error) = self.discover(&mut source).await {
                        tracing::warn!(error = %error, "could not list the mailbox's folders");
                        report(false);
                        backoff.wait().await;
                        continue;
                    }
                    source
                }
                Err(error) => {
                    tracing::warn!(error = %error, "could not open the mailbox");
                    report(false);
                    backoff.wait().await;
                    continue;
                }
            };
            loop {
                match self.check(&mut source).await {
                    Ok(()) => {
                        report(true);
                        backoff.reset();
                    }
                    Err(CheckError::Source(error)) => {
                        tracing::warn!(error = %error, "lost the mailbox connection");
                        report(false);
                        break;
                    }
                    Err(CheckError::State(error)) => return Err(error),
                }
                tokio::time::sleep(poll).await;
            }
            backoff.wait().await;
        }
    }

    /// With mail actions on, learn where the special folders are and so
    /// what the provider can do.
    async fn discover(&self, mailbox: &mut impl MailSource) -> Result<()> {
        let (Some(actions), Some(policy)) = (self.actions, self.ledger.actions_policy()) else {
            return Ok(());
        };
        let settings = &policy.actions;
        let names = FolderNames {
            drafts: settings.drafts_folder.clone(),
            sent: settings.sent_folder.clone(),
            trash: settings.trash_folder.clone(),
            junk: settings.spam_folder.clone(),
            archive: settings.archive_folder.clone(),
        };
        let folders = mailbox.folders(&names).await?;
        let possible = prepare::possible(&mailbox.caps(), &folders, actions.can_send);
        tracing::info!(
            kinds = ?possible.iter().map(|kind| kind.name()).collect::<Vec<_>>(),
            "the mailbox can carry out these kinds of action"
        );
        policy.set_possible(possible);
        *actions
            .folders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = folders;
        Ok(())
    }

    /// The preparer of actions, when they are on.
    fn preparer(&self) -> Option<Preparer<'_>> {
        Some(Preparer {
            ledger: self.ledger,
            policy: self.ledger.actions_policy()?,
            actions: self.actions?,
            settings: self.settings,
            socket: self.socket,
            cwd: self.cwd,
            clock: self.clock,
            low_space: self.low_space,
        })
    }

    /// Prepare the owner's waiting requests, oldest first.
    async fn serve_requests(&self, mailbox: &mut impl MailSource) -> Checked<()> {
        let Some(preparer) = self.preparer() else {
            return Ok(());
        };
        for request in self.ledger.snapshot().requests {
            let Some(attempt) = state(self.ledger.request_attempt(&request.key).await)? else {
                continue;
            };
            let now = self.clock.now();
            if attempt > REQUEST_ATTEMPTS {
                tracing::warn!("gave up an owner's mail request after every attempt");
                state(
                    self.ledger
                        .drop_request(
                            &request.key,
                            "Could not prepare that request after trying twice; nothing was done."
                                .to_owned(),
                            now,
                        )
                        .await,
                )?;
                continue;
            }
            let prepared = preparer
                .request(mailbox, &request)
                .await
                .map_err(|SourceLost(error)| CheckError::Source(error))?;
            match prepared {
                Prepared::Refused(text) => {
                    state(
                        self.ledger
                            .drop_request(&request.key, text, self.clock.now())
                            .await,
                    )?;
                }
                Prepared::Proposal {
                    contents,
                    codes,
                    preview,
                    revised,
                } => {
                    let recorded = state(
                        preparer
                            .record(
                                Some(&request.key),
                                &request.key,
                                contents,
                                codes,
                                preview,
                                revised,
                            )
                            .await,
                    )?;
                    if let Err(refusal) = recorded {
                        state(
                            self.ledger
                                .drop_request(
                                    &request.key,
                                    prepare::refusal_text(refusal).to_owned(),
                                    self.clock.now(),
                                )
                                .await,
                        )?;
                    } else {
                        tracing::info!("prepared an owner's mail request for approval");
                    }
                }
            }
        }
        Ok(())
    }

    /// One check: claim what arrived, prepare the owner's requests, then
    /// decide every claim.
    pub(crate) async fn check(&self, mailbox: &mut impl MailSource) -> Checked<()> {
        let now = self.clock.now();
        let snapshot = self.ledger.snapshot();
        let room = MAX_CLAIMS.saturating_sub(snapshot.claims.len());
        if room > 0 {
            let window = self.settings.catchup_hours * 3600;
            let changes = source(
                mailbox
                    .changes(snapshot.cursor.as_ref(), room, window)
                    .await,
            )?;
            match changes {
                Changes::New { refs, next } => {
                    if !refs.is_empty() || snapshot.cursor.as_ref() != Some(&next) {
                        tracing::info!(new = refs.len(), "claimed new mail");
                        state(self.ledger.claim(refs, next, None, now).await)?;
                    }
                }
                Changes::Reset {
                    recent,
                    beyond,
                    next,
                } => {
                    tracing::warn!(
                        recent = recent.len(),
                        beyond,
                        "the mailbox was reset; resyncing"
                    );
                    let today = self.clock.today(now);
                    let note = ResetNote {
                        key: format!("system:reset:{today}"),
                        text: format!(
                            "The mail server reset this mailbox's message numbers, so SCV \
                             checked the mail of the last {} hours again; mail it had \
                             already reported is not reported twice.",
                            self.settings.catchup_hours
                        ),
                        beyond,
                    };
                    state(self.ledger.claim(recent, next, Some(note), now).await)?;
                }
            }
        }
        self.ledger.checked(now);
        self.serve_requests(mailbox).await?;
        self.decide_claims(mailbox).await
    }

    async fn decide_claims(&self, mailbox: &mut impl MailSource) -> Checked<()> {
        let claims = self.ledger.snapshot().claims;
        if claims.is_empty() {
            return Ok(());
        }
        let refs: Vec<SourceRef> = claims.iter().map(|claim| claim.source.clone()).collect();
        // Adapters return bounded metadata; bounding it here holds for any.
        let metas: Vec<Meta> = source(mailbox.metadata(&refs).await)?
            .into_iter()
            .map(Meta::bounded)
            .collect();
        for reference in refs {
            let Some(attempts) = state(self.ledger.attempt(&reference).await)? else {
                continue;
            };
            let meta = metas.iter().find(|meta| meta.source == reference);
            let decided = match meta {
                _ if attempts > MAX_ATTEMPTS => {
                    tracing::warn!(message = %reference.label(), "gave up a message after every attempt");
                    Decided {
                        source: reference,
                        identity: meta.map(|meta| meta.identity.clone()),
                        message_id: meta.and_then(message_digest),
                        outcome: Outcome::Unreadable,
                        turned: false,
                        tokens: 0,
                    }
                }
                None => Decided {
                    source: reference,
                    identity: None,
                    message_id: None,
                    outcome: Outcome::Gone,
                    turned: false,
                    tokens: 0,
                },
                Some(meta) => self.decide(mailbox, meta, attempts).await?,
            };
            let now = self.clock.now();
            state(
                self.ledger
                    .finish(decided, &self.clock.today(now), now)
                    .await,
            )?;
        }
        Ok(())
    }

    /// Decide one message, cheapest rung first.
    async fn decide(
        &self,
        mailbox: &mut impl MailSource,
        meta: &Meta,
        attempts: u32,
    ) -> Checked<Decided> {
        let now = self.clock.now();
        let offset = self.clock.offset(now);
        let snapshot = self.ledger.snapshot();
        let message_id = message_digest(meta);
        let counted = |turned, tokens| Decided {
            source: meta.source.clone(),
            identity: Some(meta.identity.clone()),
            message_id: message_id.clone(),
            outcome: Outcome::Counted,
            turned,
            tokens,
        };
        let reported = |summary: Option<&Summary>,
                        note: Option<&str>,
                        urgent: bool,
                        turned,
                        tokens,
                        extra: Extra| {
            if self.low_space.is_low() {
                return counted(turned, tokens);
            }
            let (handle, lines, actions) = self.offer(meta, extra);
            let mut text = render::report_with(
                meta,
                summary,
                note,
                urgent,
                offset,
                handle.as_ref().map(|h| h.handle.as_str()),
            );
            for line in lines {
                text.push('\n');
                text.push_str(&line);
            }
            Decided {
                source: meta.source.clone(),
                identity: Some(meta.identity.clone()),
                message_id: message_id.clone(),
                outcome: Outcome::Report {
                    key: format!("report:{}", meta.source.label()),
                    text,
                    urgent,
                    handle,
                    actions,
                },
                turned,
                tokens,
            }
        };
        match snapshot.arrival(&meta.identity, message_id.as_deref()) {
            Arrival::Again if self.settings.dedupe_message_id => return Ok(counted(false, 0)),
            // A Message-ID is the sender's to choose, so one seen before
            // hides nothing.
            Arrival::ReusedId => tracing::warn!(
                message = %meta.source.label(),
                "a message reuses the Message-ID of mail decided lately; deciding it on its own"
            ),
            Arrival::Again | Arrival::New => {}
        }
        let catchup = self.settings.catchup_hours * 3600;
        if meta.received_at > 0 && now.saturating_sub(meta.received_at) > catchup {
            return Ok(counted(false, 0));
        }
        let decision = rules::decide(&self.settings.rules, meta);
        let urgent = decision.urgent;
        let with_body = match decision.action {
            RuleAction::Count => return Ok(counted(false, 0)),
            RuleAction::Header => {
                return Ok(reported(None, None, urgent, false, 0, Extra::default()));
            }
            RuleAction::TriageMeta => false,
            RuleAction::Triage => self.settings.send_body,
        };
        if self.settings.max_tokens_per_day == 0 {
            return Ok(reported(None, None, urgent, false, 0, Extra::default()));
        }
        if attempts > MODEL_ATTEMPTS {
            return Ok(reported(
                None,
                Some("not triaged: reading it failed before"),
                urgent,
                false,
                0,
                Extra::default(),
            ));
        }
        let today = self.clock.today(now);
        let (turns, spent) = snapshot.spent(&today, now);
        if turns >= self.settings.max_triage_per_hour as usize {
            self.note_once(
                "hourly",
                &today,
                "More mail arrived this hour than triage may read; the rest is reported by its \
                 headers only until the hour has room.",
            )
            .await?;
            return Ok(reported(
                None,
                Some("not triaged: this hour's triage limit is reached"),
                urgent,
                false,
                0,
                Extra::default(),
            ));
        }
        let body = match (&meta.text, with_body) {
            (Some(part), true) => {
                let text = source(
                    mailbox
                        .text(&meta.source, part, self.settings.max_fetch_bytes())
                        .await,
                )?;
                match text {
                    // Turning a large HTML part into text takes a while, so
                    // it runs off the async threads.
                    Some(text) => {
                        let max = self.settings.max_body_bytes();
                        let cleaned = tokio::task::spawn_blocking(move || {
                            clean::clean_body(&text.text, text.html, max)
                        })
                        .await
                        .map_err(|error| CheckError::State(anyhow::anyhow!(error)))?;
                        Some(cleaned)
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let nonce = super::ledger::random_hex();
        let prompt = triage::prompt(meta, body.as_ref(), &nonce);
        if self.frame.len() + prompt.len() > triage::MAX_PROMPT_BYTES {
            tracing::warn!(message = %meta.source.label(), "a triage prompt was over its bound");
            return Ok(reported(
                None,
                Some("not triaged: too large to read"),
                urgent,
                false,
                0,
                Extra::default(),
            ));
        }
        let estimate = triage::estimate(&self.frame, &prompt);
        if spent.saturating_add(estimate) > self.settings.max_tokens_per_day {
            self.note_once(
                "budget",
                &today,
                "Today's model budget for mail is used up; until tomorrow SCV reports new mail \
                 by its headers only.",
            )
            .await?;
            return Ok(reported(
                None,
                Some("not triaged: today's model budget is used up"),
                urgent,
                false,
                0,
                Extra::default(),
            ));
        }
        let model = Some(self.settings.triage_model.as_str()).filter(|model| !model.is_empty());
        let result = tokio::time::timeout(
            TURN_TIMEOUT,
            model::turn(self.socket, self.cwd, model, &self.frame, &prompt),
        )
        .await;
        let answer = match result {
            Ok(Ok(turn)) => {
                let tokens = turn.tokens.unwrap_or(estimate);
                return Ok(match triage::parse_with(&turn.answer, &self.options) {
                    // A message the model would move is reported, so the
                    // owner can approve the move.
                    Some(answer) if !answer.notify && answer.suggestion.is_none() => {
                        counted(true, tokens)
                    }
                    Some(answer) => {
                        let urgent =
                            urgent || (self.settings.notify.urgent_by_model && answer.urgent);
                        let extra = Extra {
                            reply: answer.reply_suggested,
                            suggestion: answer.suggestion,
                        };
                        reported(Some(&answer.summary), None, urgent, true, tokens, extra)
                    }
                    None => reported(
                        None,
                        Some("triage answer unreadable"),
                        urgent,
                        true,
                        tokens,
                        Extra::default(),
                    ),
                });
            }
            Ok(Err(TurnError::ToolEvent)) => {
                tracing::error!(message = %meta.source.label(), "a triage session produced a tool event and was closed");
                "not triaged: internal error"
            }
            Ok(Err(TurnError::Failed(error))) => {
                tracing::warn!(error = %error, message = %meta.source.label(), "triage failed");
                "not triaged: the model did not answer"
            }
            Err(_) => {
                tracing::warn!(message = %meta.source.label(), "triage timed out");
                "not triaged: the model did not answer in time"
            }
        };
        // A turn that may have reached the provider is charged its estimate.
        Ok(reported(
            None,
            Some(answer),
            urgent,
            true,
            estimate,
            Extra::default(),
        ))
    }

    /// With mail actions on: the report's handle, its extra lines, and the
    /// action a triage suggestion becomes (its content written, its code
    /// drawn). A suggestion that cannot be offered is dropped silently.
    fn offer(&self, meta: &Meta, extra: Extra) -> (Option<Handle>, Vec<String>, Vec<NewAction>) {
        let (Some(actions), Some(preparer)) = (self.actions, self.preparer()) else {
            return (None, Vec::new(), Vec::new());
        };
        let snapshot = self.ledger.snapshot();
        let handle = Handle {
            handle: actions.draw_handle(&snapshot),
            source: meta.source.clone(),
            identity: meta.identity.clone(),
            at: self.clock.now(),
        };
        let mut lines = Vec::new();
        let policy = preparer.policy;
        if extra.reply
            && (policy.offers(ActionKind::Draft, Some(Form::Reply))
                || policy.offers(ActionKind::Send, Some(Form::Reply)))
        {
            lines.push(format!(
                "  A reply may be wanted: mail reply #{} [what to say].",
                handle.handle
            ));
        }
        let mut proposed = Vec::new();
        let kind = extra.suggestion.map(|suggestion| match suggestion {
            Suggestion::Archive => ActionKind::Archive,
            Suggestion::Trash => ActionKind::Trash,
            Suggestion::Spam => ActionKind::Spam,
        });
        if let Some(kind) = kind.filter(|kind| policy.offers(*kind, None))
            && preparer.full().is_none()
        {
            let origin = Origin::Triage {
                source: meta.source.label(),
            };
            if let Prepared::Proposal {
                contents, codes, ..
            } = preparer.change(origin, meta, &handle.handle, kind, Some(&snapshot))
                && let (Some(content), Some(code)) = (contents.first(), codes.first())
            {
                // Written now: the report that offers it is recorded next.
                let store = policy.content.clone();
                if store.write_new(content).is_ok() {
                    let folder = content.folder.as_ref().map(|folder| actions.label(folder));
                    lines.push(super::preview::suggestion(
                        &handle.handle,
                        &super::preview::Choice {
                            code: code.clone(),
                            kind,
                        },
                        folder.as_deref(),
                        policy.actions.approval_hours,
                    ));
                    proposed.push(NewAction::new(
                        content,
                        format!("triage:{}:0", meta.source.label()),
                        code.clone(),
                    ));
                } else {
                    actions.release(&codes);
                }
            }
        }
        (Some(handle), lines, proposed)
    }

    /// Queue one of SCV's lines of `kind` once per local day.
    async fn note_once(&self, kind: &str, today: &str, text: &str) -> Checked<()> {
        let key = format!("system:{kind}:{today}");
        state(
            self.ledger
                .note(&key, Class::System, text.to_owned(), self.clock.now())
                .await,
        )
    }
}

/// What a triage answer adds to a report.
#[derive(Debug, Clone, Copy, Default)]
struct Extra {
    reply: bool,
    suggestion: Option<Suggestion>,
}

/// The digest of a message's `Message-ID`, when it has one.
fn message_digest(meta: &Meta) -> Option<String> {
    meta.message_id
        .as_deref()
        .map(|id| parse::digest(id.as_bytes()))
}

#[cfg(test)]
mod tests;
