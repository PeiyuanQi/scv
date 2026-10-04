//! Preparing actions: the owner's requests, and the moves triage suggests,
//! turned into written content, drawn codes, and previews.
//!
//! The worker, which holds the read-only mailbox connection, prepares each
//! request in turn: it reads the message the request names again (checking
//! that its identity is the one the handle was given for), runs a fresh
//! tool-free drafting turn when words are needed, writes each alternative's
//! content file, draws its code, renders the preview, and records it all in
//! one ledger write. A request that cannot be prepared is dropped with SCV's
//! answer why in the mail chat. Nothing here writes to a mailbox.

use anyhow::Result;
use std::sync::Mutex;

use super::clean;
use super::compose::{self, Base, Drafted};
use super::content::{ActionContent, ActionKind, Folder, FolderRole, Form, Origin, Source};
use super::ledger::actions::{NewAction, Policy, Refusal, Request, RequestWork, preview_key};
use super::ledger::{Ledger, MailState};
use super::model::{self, TurnError};
use super::preview;
use super::settings::{MailSettings, SentCopy};
use super::source::{Caps, Folders, MailSource, Meta, ProviderKind};
use super::{Clock, LowSpace};
use crate::hub::MailRegistration;
use crate::mail_chat::codes;

/// What preparing actions needs beyond the worker's own parts.
pub(crate) struct Actions<'a> {
    pub(crate) registration: Option<&'a MailRegistration>,
    pub(crate) provider: ProviderKind,
    /// The mailbox's own address: the sender of the mail it writes.
    pub(crate) own: Option<String>,
    /// The whole system prompt of this account's drafting sessions.
    pub(crate) frame: String,
    /// Where the special folders are, once the mailbox opened.
    pub(crate) folders: Mutex<Folders>,
    /// Whether this account can send at all.
    pub(crate) can_send: bool,
}

/// The kinds a provider with `caps` and `folders` can carry out.
pub(crate) fn possible(caps: &Caps, folders: &Folders, can_send: bool) -> Vec<ActionKind> {
    let mut kinds = vec![ActionKind::MarkRead];
    if folders.drafts.is_some() {
        kinds.push(ActionKind::Draft);
    }
    if can_send {
        kinds.push(ActionKind::Send);
    }
    for (kind, folder) in [
        (ActionKind::Archive, &folders.archive),
        (ActionKind::Trash, &folders.trash),
        (ActionKind::Spam, &folders.junk),
    ] {
        if caps.can_move && folder.is_some() {
            kinds.push(kind);
        }
    }
    kinds
}

/// Which folder an action on a message goes to.
pub(crate) fn role(kind: ActionKind) -> Option<FolderRole> {
    match kind {
        ActionKind::Archive => Some(FolderRole::Archive),
        ActionKind::Trash => Some(FolderRole::Trash),
        ActionKind::Spam => Some(FolderRole::Junk),
        ActionKind::Draft => Some(FolderRole::Drafts),
        ActionKind::Send => Some(FolderRole::Sent),
        ActionKind::MarkRead => None,
    }
}

impl Actions<'_> {
    pub(crate) fn folder(&self, role: FolderRole) -> Option<Folder> {
        let folders = self
            .folders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        folders.get(role).map(|name| Folder {
            role,
            name: name.to_owned(),
        })
    }

    /// The folder as the owner reads it.
    pub(crate) fn label(&self, folder: &Folder) -> String {
        preview::folder_label(folder.role, &folder.name, self.provider)
    }

    /// A code no action of this account or any other running account has.
    pub(crate) fn draw_code(&self, state: &MailState, drawn: &[String]) -> String {
        loop {
            let code = codes::new_code();
            let taken = drawn.contains(&code)
                || state
                    .actions
                    .iter()
                    .any(|entry| entry.code == code || entry.old_codes.contains(&code))
                || state
                    .tombstones
                    .iter()
                    .any(|tombstone| tombstone.codes.contains(&code));
            if taken {
                continue;
            }
            if self
                .registration
                .is_none_or(|registration| registration.claim_code(&code))
            {
                return code;
            }
        }
    }

    /// A handle no reported mail of this account or any other has.
    pub(crate) fn draw_handle(&self, state: &MailState) -> String {
        loop {
            let handle = codes::new_handle();
            if state.handles.iter().any(|known| known.handle == handle) {
                continue;
            }
            if self
                .registration
                .is_none_or(|registration| registration.claim_handle(&handle))
            {
                return handle;
            }
        }
    }

    /// Give back codes drawn for actions that were not recorded.
    pub(crate) fn release(&self, codes: &[String]) {
        if let Some(registration) = self.registration {
            registration.forget_codes(codes);
        }
    }
}

/// What preparing a request came to.
pub(crate) enum Prepared {
    /// Actions to record: their content (not yet written), the preview
    /// offering them, and the actions a revision replaces.
    Proposal {
        contents: Vec<ActionContent>,
        codes: Vec<String>,
        preview: String,
        revised: Vec<String>,
    },
    /// Nothing to record; SCV's answer why, for the mail chat.
    Refused(String),
}

/// Everything a preparation reads.
pub(crate) struct Preparer<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) policy: &'a Policy,
    pub(crate) actions: &'a Actions<'a>,
    pub(crate) settings: &'a MailSettings,
    pub(crate) socket: &'a std::path::Path,
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) low_space: &'a LowSpace,
}

/// A failure of the mailbox connection, which the worker reconnects after.
pub(crate) struct SourceLost(pub(crate) anyhow::Error);

type Step<T> = std::result::Result<T, SourceLost>;

impl Preparer<'_> {
    fn base(
        &self,
        origin: Origin,
        source: Option<Source>,
        display: Option<super::content::Display>,
    ) -> Base<'_> {
        let now = self.clock.now();
        Base {
            account: &self.policy.account,
            fingerprint: &self.policy.fingerprint,
            origin,
            source,
            display,
            now,
            hard_expiry: now + self.policy.actions.max_pending_hours * 3600,
        }
    }

    /// The message `handle` names, read again and checked to be the same
    /// message; the refusal to give otherwise.
    async fn message(
        &self,
        mailbox: &mut impl MailSource,
        handle: &str,
    ) -> Step<std::result::Result<Meta, String>> {
        let Some(known) = self.ledger.handle(handle) else {
            return Ok(Err(format!(
                "No reported mail has handle #{handle} any more; nothing was done."
            )));
        };
        let metas = mailbox
            .metadata(std::slice::from_ref(&known.source))
            .await
            .map_err(SourceLost)?;
        let Some(meta) = metas.into_iter().next().map(Meta::bounded) else {
            return Ok(Err(format!(
                "#{handle} is no longer in the mailbox; nothing was done."
            )));
        };
        if meta.identity != known.identity {
            return Ok(Err(format!(
                "#{handle} is not the mail it was any more; nothing was done."
            )));
        }
        Ok(Ok(meta))
    }

    fn source(meta: &Meta) -> Source {
        Source {
            reference: meta.source.clone(),
            identity: meta.identity.clone(),
            locator: meta.locator.clone(),
            message_id: meta.message_id.clone(),
        }
    }

    /// Why no more actions can be recorded now, if something stops them.
    pub(crate) fn full(&self) -> Option<String> {
        if self.low_space.is_low() {
            return Some("Not prepared: the local mail store is short of disk space.".to_owned());
        }
        let snapshot = self.ledger.snapshot();
        let live = snapshot
            .actions
            .iter()
            .filter(|entry| !entry.state.terminal())
            .count();
        (live >= self.policy.actions.max_open).then(|| {
            "Not prepared: as many actions wait as mail.actions.max_open allows.".to_owned()
        })
    }

    /// Prepare one of the owner's requests.
    pub(crate) async fn request(
        &self,
        mailbox: &mut impl MailSource,
        request: &Request,
    ) -> Step<Prepared> {
        if let Some(full) = self.full() {
            return Ok(Prepared::Refused(full));
        }
        // `owner:<route>:<message ID>`; a message ID may hold a colon.
        let message_id = request
            .key
            .strip_prefix("owner:")
            .and_then(|rest| rest.strip_prefix(request.route.as_str()))
            .and_then(|rest| rest.strip_prefix(':'))
            .unwrap_or(&request.key)
            .to_owned();
        let origin = Origin::Owner {
            route: request.route.clone(),
            message_id,
        };
        match &request.work {
            RequestWork::Reply { handle, text } => self.reply(mailbox, origin, handle, text).await,
            RequestWork::Forward { handle, to, note } => {
                self.forward(mailbox, origin, handle, to, note).await
            }
            RequestWork::Compose { to, text } => Ok(self.compose(origin, to, text).await),
            RequestWork::Revise { action, text } => Ok(self.revise(origin, action, text).await),
            RequestWork::Message { handle, kind } => {
                let meta = match self.message(mailbox, handle).await? {
                    Ok(meta) => meta,
                    Err(refusal) => return Ok(Prepared::Refused(refusal)),
                };
                Ok(self.change(origin, &meta, handle, *kind, None))
            }
        }
    }

    /// The owner's own address, or the refusal to give without one.
    fn own(&self) -> std::result::Result<&str, Prepared> {
        self.actions.own.as_deref().ok_or_else(|| {
            Prepared::Refused(
                "Not prepared: this account's own address is not known; sign it in again with \
                 --address."
                    .to_owned(),
            )
        })
    }

    /// The kinds an outgoing message in `form` may be offered as.
    fn outgoing_kinds(&self, form: Form) -> Vec<ActionKind> {
        [ActionKind::Draft, ActionKind::Send]
            .into_iter()
            .filter(|kind| self.policy.offers(*kind, Some(form)))
            .collect()
    }

    fn sent_copy(&self) -> SentCopy {
        match self.actions.provider {
            ProviderKind::Imap => self.policy.actions.sent_copy,
            // The APIs file sent mail themselves.
            ProviderKind::Gmail | ProviderKind::Graph => SentCopy::Provider,
        }
    }

    /// A drafting turn, within the day's limit; the words, or the refusal.
    async fn draft(
        &self,
        prompt: &str,
        new_mail: bool,
        what: &str,
    ) -> std::result::Result<(Option<String>, String), String> {
        let now = self.clock.now();
        let today = self.clock.today(now);
        let snapshot = self.ledger.snapshot();
        let composed = if snapshot.day.date == today {
            snapshot.day.composed
        } else {
            0
        };
        if composed >= self.policy.actions.max_compose_per_day {
            return Err(format!(
                "Could not prepare {what}: today's limit of drafting turns is reached."
            ));
        }
        if self.actions.frame.len() + prompt.len() > super::triage::MAX_PROMPT_BYTES {
            return Err(format!(
                "Could not prepare {what}: the request is too large."
            ));
        }
        let model =
            Some(self.policy.actions.compose_model.as_str()).filter(|model| !model.is_empty());
        let result = tokio::time::timeout(
            super::worker::TURN_TIMEOUT,
            model::turn(self.socket, self.cwd, model, &self.actions.frame, prompt),
        )
        .await;
        let estimate =
            super::triage::estimate(&self.actions.frame, prompt) + compose::ANSWER_TOKENS;
        let (answer, tokens) = match result {
            Ok(Ok(turn)) => {
                let tokens = turn.tokens.unwrap_or(estimate);
                (Some(turn.answer), tokens)
            }
            Ok(Err(TurnError::ToolEvent)) => {
                tracing::error!("a drafting session produced a tool event and was closed");
                (None, estimate)
            }
            Ok(Err(TurnError::Failed(error))) => {
                tracing::warn!(error = %error, "a drafting turn failed");
                (None, estimate)
            }
            Err(_) => {
                tracing::warn!("a drafting turn timed out");
                (None, estimate)
            }
        };
        if let Err(error) = self.ledger.composed(&today, tokens).await {
            tracing::error!(error = %error, "could not count a drafting turn");
        }
        let Some(answer) = answer else {
            return Err(format!(
                "Could not prepare {what}: the model did not answer."
            ));
        };
        match compose::parse(&answer, new_mail) {
            Some(Drafted::Written { subject, body }) => Ok((subject, body)),
            Some(Drafted::Declined(reason)) => Err(format!(
                "Could not prepare {what}; the model declined:\n{}{}",
                super::render::UNTRUSTED,
                clean::replace_links(&reason)
            )),
            None => Err(format!(
                "Could not prepare {what}: the model's answer was unreadable."
            )),
        }
    }

    async fn reply(
        &self,
        mailbox: &mut impl MailSource,
        origin: Origin,
        handle: &str,
        text: &str,
    ) -> Step<Prepared> {
        let own = match self.own() {
            Ok(own) => own.to_owned(),
            Err(refused) => return Ok(refused),
        };
        let meta = match self.message(mailbox, handle).await? {
            Ok(meta) => meta,
            Err(refusal) => return Ok(Prepared::Refused(refusal)),
        };
        if let Some(why) = compose::reply_refusal(&meta, &own) {
            return Ok(Prepared::Refused(format!(
                "Not prepared: #{handle}: {why}."
            )));
        }
        let compose::Recipients { to, cc, notes } =
            match compose::reply_recipients(&meta, &own, &self.policy.actions) {
                Ok(recipients) => recipients,
                Err(why) => {
                    return Ok(Prepared::Refused(format!(
                        "Not prepared: #{handle}: {why}."
                    )));
                }
            };
        let everyone: Vec<&str> = to.iter().chain(&cc).map(String::as_str).collect();
        if let Some(why) = self.policy.addresses_refused(&everyone) {
            return Ok(Prepared::Refused(format!(
                "Not prepared: #{handle}: {why}."
            )));
        }
        let body = match (&meta.text, self.settings.send_body) {
            (Some(part), true) => {
                match mailbox
                    .text(&meta.source, part, self.settings.max_fetch_bytes())
                    .await
                    .map_err(SourceLost)?
                {
                    Some(text) => {
                        let max = self.settings.max_body_bytes();
                        tokio::task::spawn_blocking(move || {
                            clean::clean_body(&text.text, text.html, max)
                        })
                        .await
                        .ok()
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let nonce = super::ledger::random_hex();
        let prompt = compose::reply_prompt(&meta, body.as_ref(), text, &nonce);
        let what = format!("the reply to #{handle}");
        let (_, words) = match self.draft(&prompt, false, &what).await {
            Ok(words) => words,
            Err(refusal) => return Ok(Prepared::Refused(refusal)),
        };
        let message = compose::outgoing(
            Form::Reply,
            &own,
            &self.policy.actions.from_name,
            to,
            cc,
            compose::reply_subject(&meta.subject),
            words,
            compose::threading(&meta),
            self.sent_copy(),
        );
        let display = compose::display(&meta, handle);
        Ok(self.offer_outgoing(
            origin,
            Some(Self::source(&meta)),
            Some(display),
            message,
            &notes,
            Vec::new(),
        ))
    }

    async fn forward(
        &self,
        mailbox: &mut impl MailSource,
        origin: Origin,
        handle: &str,
        to: &[String],
        note: &str,
    ) -> Step<Prepared> {
        let own = match self.own() {
            Ok(own) => own.to_owned(),
            Err(refused) => return Ok(refused),
        };
        let Some(to) = compose::typed_recipients(to, self.policy.actions.max_recipients) else {
            return Ok(Prepared::Refused(
                "Not prepared: the addresses are not plain addresses.".into(),
            ));
        };
        let meta = match self.message(mailbox, handle).await? {
            Ok(meta) => meta,
            Err(refusal) => return Ok(Prepared::Refused(refusal)),
        };
        let text = match &meta.text {
            Some(part) => mailbox
                .text(&meta.source, part, self.settings.max_fetch_bytes())
                .await
                .map_err(SourceLost)?,
            None => None,
        };
        let body = compose::forward_body(note, &meta, text.as_ref());
        let message = compose::outgoing(
            Form::Forward,
            &own,
            &self.policy.actions.from_name,
            to,
            Vec::new(),
            compose::forward_subject(&meta.subject),
            body,
            (None, Vec::new()),
            self.sent_copy(),
        );
        let display = compose::display(&meta, handle);
        let mut notes = Vec::new();
        if !meta.attachments.is_empty() {
            notes.push(format!(
                "The original's {} attachment(s) are not forwarded.",
                meta.attachments.len()
            ));
        }
        Ok(self.offer_outgoing(
            origin,
            Some(Self::source(&meta)),
            Some(display),
            message,
            &notes,
            Vec::new(),
        ))
    }

    async fn compose(&self, origin: Origin, to: &[String], text: &str) -> Prepared {
        let own = match self.own() {
            Ok(own) => own.to_owned(),
            Err(refused) => return refused,
        };
        let Some(to) = compose::typed_recipients(to, self.policy.actions.max_recipients) else {
            return Prepared::Refused(
                "Not prepared: the addresses are not plain addresses.".into(),
            );
        };
        let prompt = compose::compose_prompt(&to, text);
        let (subject, words) = match self.draft(&prompt, true, "the new mail").await {
            Ok(words) => words,
            Err(refusal) => return Prepared::Refused(refusal),
        };
        let message = compose::outgoing(
            Form::Compose,
            &own,
            &self.policy.actions.from_name,
            to,
            Vec::new(),
            subject.unwrap_or_default(),
            words,
            (None, Vec::new()),
            self.sent_copy(),
        );
        self.offer_outgoing(origin, None, None, message, &[], Vec::new())
    }

    async fn revise(&self, origin: Origin, action: &str, text: &str) -> Prepared {
        let snapshot = self.ledger.snapshot();
        let Some(entry) = snapshot
            .actions
            .iter()
            .find(|entry| entry.id == action && !entry.state.terminal())
        else {
            return Prepared::Refused("That draft is no longer waiting; nothing was done.".into());
        };
        let revised: Vec<String> = match &entry.group {
            Some(group) => snapshot
                .actions
                .iter()
                .filter(|other| other.group.as_ref() == Some(group))
                .map(|other| other.id.clone())
                .collect(),
            None => vec![entry.id.clone()],
        };
        let content = {
            let store = self.policy.content.clone();
            let id = entry.id.clone();
            tokio::task::spawn_blocking(move || store.read(&id)).await
        };
        let Ok(Ok(Some(previous))) = content else {
            return Prepared::Refused("Could not read that draft; nothing was done.".into());
        };
        let Some(message) = &previous.message else {
            return Prepared::Refused("That action has no mail to revise.".into());
        };
        let new_mail = message.form == Form::Compose;
        let prompt = compose::revise_prompt(message, text, &super::ledger::random_hex());
        let (subject, words) = match self.draft(&prompt, new_mail, "the revised draft").await {
            Ok(words) => words,
            Err(refusal) => return Prepared::Refused(refusal),
        };
        let mut revised_message = message.clone();
        revised_message.body = words;
        if let Some(subject) = subject {
            revised_message.subject = subject;
        }
        revised_message.message_id = compose::message_id(&message.from.address);
        self.offer_outgoing(
            origin,
            previous.source.clone(),
            previous.display.clone(),
            revised_message,
            &[],
            revised,
        )
    }

    /// The draft and send alternatives of `message`, previewed together.
    fn offer_outgoing(
        &self,
        origin: Origin,
        source: Option<Source>,
        display: Option<super::content::Display>,
        mut message: super::content::Outgoing,
        notes: &[String],
        revised: Vec<String>,
    ) -> Prepared {
        if message.notes.is_empty() {
            message.notes = notes.to_vec();
        }
        let kinds = self.outgoing_kinds(message.form);
        let drafts = self.actions.folder(FolderRole::Drafts);
        let sent = self.actions.folder(FolderRole::Sent);
        let base = self.base(origin, source, display);
        let contents = compose::outgoing_actions(
            &base,
            &message,
            &kinds,
            drafts.as_ref().map(|folder| folder.name.as_str()),
            sent.as_ref().map(|folder| folder.name.as_str()),
        );
        if contents.is_empty() {
            return Prepared::Refused(
                "Not prepared: this account can neither save drafts nor send now.".into(),
            );
        }
        let snapshot = self.ledger.snapshot();
        let mut codes = Vec::new();
        for _ in &contents {
            let code = self.actions.draw_code(&snapshot, &codes);
            codes.push(code);
        }
        let preview = preview::render(
            &contents,
            &codes,
            self.actions.provider,
            self.policy.actions.approval_hours,
        );
        Prepared::Proposal {
            contents,
            codes,
            preview,
            revised,
        }
    }

    /// A move or a mark of `meta`, previewed on its own, or with
    /// `suggested` as a line of its report.
    pub(crate) fn change(
        &self,
        origin: Origin,
        meta: &Meta,
        handle: &str,
        kind: ActionKind,
        state: Option<&MailState>,
    ) -> Prepared {
        let folder = role(kind).and_then(|role| self.actions.folder(role));
        if kind.moves() && folder.is_none() {
            return Prepared::Refused(format!(
                "Not prepared: this mailbox has no folder for that; set mail.actions.{}_folder.",
                match kind {
                    ActionKind::Archive => "archive",
                    ActionKind::Trash => "trash",
                    _ => "spam",
                }
            ));
        }
        let base = self.base(
            origin,
            Some(Self::source(meta)),
            Some(compose::display(meta, handle)),
        );
        let content = compose::change_action(&base, kind, folder);
        let snapshot;
        let state = if let Some(state) = state {
            state
        } else {
            snapshot = self.ledger.snapshot();
            &snapshot
        };
        let code = self.actions.draw_code(state, &[]);
        let preview = preview::render(
            std::slice::from_ref(&content),
            std::slice::from_ref(&code),
            self.actions.provider,
            self.policy.actions.approval_hours,
        );
        Prepared::Proposal {
            contents: vec![content],
            codes: vec![code],
            preview,
            revised: Vec::new(),
        }
    }

    /// Write `contents` and record them for `request` with `preview`, as
    /// `origin_key` numbered; on refusal, remove what was written.
    pub(crate) async fn record(
        &self,
        request: Option<&str>,
        origin_key: &str,
        contents: Vec<ActionContent>,
        codes: Vec<String>,
        preview: String,
        revised: Vec<String>,
    ) -> Result<std::result::Result<(), Refusal>> {
        let written = self.write(&contents).await;
        if let Err(error) = written {
            tracing::error!(error = %error, "could not write a mail action's content");
            self.actions.release(&codes);
            return Ok(Err(Refusal::Full));
        }
        let proposed: Vec<NewAction> = contents
            .iter()
            .zip(&codes)
            .enumerate()
            .map(|(index, (content, code))| {
                NewAction::new(content, format!("{origin_key}:{index}"), code.clone())
            })
            .collect();
        let key = preview_key(&contents[0].id, 1);
        let now = self.clock.now();
        let result = self
            .ledger
            .propose(request, proposed, (key, preview), revised, now)
            .await?;
        if result.is_err() {
            self.actions.release(&codes);
            let ids: Vec<String> = contents.iter().map(|content| content.id.clone()).collect();
            let store = self.policy.content.clone();
            let _ = tokio::task::spawn_blocking(move || {
                for id in ids {
                    let _ = store.remove(&id);
                }
            })
            .await;
        }
        Ok(result)
    }

    /// Write each content file, within `retention.max_actions_mib`.
    pub(crate) async fn write(&self, contents: &[ActionContent]) -> Result<()> {
        let store = self.policy.content.clone();
        let contents = contents.to_vec();
        let max = self.settings.retention.max_actions_mib * 1024 * 1024;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let used: u64 = store.list().iter().map(|listed| listed.bytes).sum();
            if used > max {
                anyhow::bail!("the mail actions' content is at its size limit");
            }
            for content in &contents {
                store.write_new(content)?;
            }
            Ok(())
        })
        .await?
    }
}

/// The request key's refusal line, for the mail chat.
pub(crate) fn refusal_text(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Full => {
            "Not prepared: the local mail store or the open actions are at their limit."
        }
        Refusal::Duplicate => "That request was prepared already.",
        Refusal::Started => "Not prepared: that draft was approved or is over already.",
    }
}

#[cfg(test)]
mod tests;
