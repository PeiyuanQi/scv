//! Mail actions in the ledger: the only code that moves an action from one
//! state to the next, and the only code that can hand the executor an
//! [`Approved`].
//!
//! An action is proposed with its content already written
//! ([`super::super::content`]), shown to the owner in a preview that carries
//! its code, opened for approval once the mail chat reports the preview
//! delivered, and approved only by the owner's `approve CODE` that passes
//! every check of [`Ledger::approve`]. Execution then asks
//! [`Ledger::begin_execution`], which checks the approval, the time, the
//! content's digest, the credentials, and the settings again, and marks the
//! action `executing` in the same write before it returns the sealed
//! [`Approved`]. Every transition is one write of the state file, so deny,
//! cancel, expiry, and execution can never interleave.
//!
//! A send whose outcome is unclear once its data went out is never tried
//! again: it is checked, and without proof it ends `unknown` and the owner
//! is told to look. A Gmail or Graph mutation answered HTTP 401 is that
//! same doubt for every action, a draft or a mark-read included. A check
//! that finds the change records it done. Any other check ends the action
//! `unknown` and does not carry it out again; the owner has to propose it
//! again.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::{Item, Ledger, MailState, Stamped, push_item_with, random_hex};
use crate::email::content::{ActionContent, ActionKind, Class as Limit, ContentStore, Form};
use crate::email::plan::Class;
use crate::email::render::valid_address;
use crate::email::settings::{ActionMode, ActionSettings};
use crate::email::source::SourceRef;
use crate::hub::KeyedOutcome;
use crate::mail_chat::ChatEvidence;

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;
/// Tombstones kept at most, beyond their age.
pub(crate) const MAX_TOMBSTONES: usize = 1024;
/// Owner messages that approved something, remembered so a message handed
/// over again changes nothing.
const MAX_APPROVALS: usize = 256;
const APPROVAL_SECONDS: u64 = 7 * DAY;
/// Requests waiting for the worker, and how often one is tried.
pub(crate) const MAX_REQUESTS: usize = 8;
pub(crate) const REQUEST_ATTEMPTS: u32 = 2;
const REQUEST_SECONDS: u64 = DAY;
/// Tries of one approved action before it fails.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Times a refused preview is offered again with a new code.
pub(crate) const MAX_REISSUES: u32 = 2;
/// Handles kept at most, beyond their age.
pub(crate) const MAX_HANDLES: usize = 512;
/// How long past its deadline an interrupted action is still checked
/// before it ends `unknown`.
const PROBE_GRACE: u64 = DAY;

/// What the ledger's actions follow: the account's settings when it
/// started, and where their files are.
pub(crate) struct Policy {
    pub(crate) actions: ActionSettings,
    /// The mail chats the account reports to.
    pub(crate) routes: Vec<String>,
    /// The credential fingerprint the account runs with.
    pub(crate) fingerprint: String,
    pub(crate) account: String,
    /// `state/mail/<account>/audit.jsonl`.
    pub(crate) audit: PathBuf,
    pub(crate) content: ContentStore,
    pub(crate) tombstone_seconds: u64,
    pub(crate) unknown_keep_seconds: u64,
    /// The kinds the provider can carry out, as its capabilities and folders
    /// say once the mailbox is open; every kind until then.
    pub(crate) possible: std::sync::Mutex<Vec<ActionKind>>,
}

/// Every kind of action.
pub(crate) const ALL_KINDS: [ActionKind; 6] = [
    ActionKind::Draft,
    ActionKind::Send,
    ActionKind::Archive,
    ActionKind::MarkRead,
    ActionKind::Trash,
    ActionKind::Spam,
];

impl Policy {
    fn mode(&self, kind: ActionKind) -> ActionMode {
        let actions = &self.actions;
        match kind {
            ActionKind::Draft => actions.draft,
            ActionKind::Send => actions.send,
            ActionKind::Archive => actions.archive,
            ActionKind::MarkRead => actions.mark_read,
            ActionKind::Trash => actions.trash,
            ActionKind::Spam => actions.spam,
        }
    }

    /// Whether the settings allow `kind` (of an outgoing message in `form`).
    pub(crate) fn allows(&self, kind: ActionKind, form: Option<Form>) -> bool {
        self.mode(kind) == ActionMode::Approve
            && (form != Some(Form::Forward) || self.actions.forward == ActionMode::Approve)
    }

    /// Whether the settings and the provider allow `kind`.
    pub(crate) fn offers(&self, kind: ActionKind, form: Option<Form>) -> bool {
        self.allows(kind, form)
            && self
                .possible
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&kind)
    }

    /// Record what the provider can do, as found when the mailbox opened.
    pub(crate) fn set_possible(&self, kinds: Vec<ActionKind>) {
        *self
            .possible
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = kinds;
    }

    /// The kinds the account may propose now, for status.
    pub(crate) fn offered(&self) -> Vec<ActionKind> {
        ALL_KINDS
            .into_iter()
            .filter(|kind| self.offers(*kind, None))
            .collect()
    }

    fn daily_limit(&self, limit: Limit) -> u64 {
        match limit {
            Limit::Drafts => self.actions.max_drafts_per_day,
            Limit::Sends => self.actions.max_sends_per_day,
            Limit::Moves => self.actions.max_moves_per_day,
            Limit::Flags => self.actions.max_flags_per_day,
        }
    }

    /// Why the recipients of `content` break the settings, if they do.
    pub(crate) fn recipients_refused(&self, content: &ActionContent) -> Option<&'static str> {
        let recipients = content.recipients();
        if content.message.is_some() && recipients.is_empty() {
            return Some("it has no recipient");
        }
        self.addresses_refused(&recipients)
    }

    /// Why `recipients` break the settings, if they do: too many, one not a
    /// plain address, or one outside `recipient_domains`.
    pub(crate) fn addresses_refused(&self, recipients: &[&str]) -> Option<&'static str> {
        if recipients.len() > self.actions.max_recipients {
            return Some("it has more recipients than mail.actions.max_recipients allows");
        }
        for address in recipients {
            if valid_address(address).as_deref() != Some(*address) {
                return Some("a recipient is not a plain address");
            }
            if !self.actions.recipient_domains.is_empty() {
                let domain = address.rsplit_once('@').map_or("", |(_, domain)| domain);
                let allowed = self.actions.recipient_domains.iter().any(|allowed| {
                    domain == allowed
                        || domain
                            .strip_suffix(allowed.as_str())
                            .is_some_and(|rest| rest.ends_with('.'))
                });
                if !allowed {
                    return Some("a recipient is outside mail.actions.recipient_domains");
                }
            }
        }
        None
    }
}

/// Where an action stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionState {
    /// Written, its preview not yet handed to a mail chat.
    Proposed,
    /// Its preview is being handed over, or stored and not yet delivered.
    Previewing,
    /// Its preview reached the owner: its code approves it.
    Open,
    /// Approved, waiting for the executor.
    Approved,
    /// Being carried out, or interrupted while it was and not yet checked.
    Executing,
    Done,
    Failed,
    /// SCV lost track of it while carrying it out; never retried.
    Unknown,
    Denied,
    Expired,
    /// Another choice for the same mail was approved, or it was revised.
    Superseded,
    Cancelled,
    /// Its content or credentials no longer matched its approval.
    Invalid,
}

impl ActionState {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Previewing => "previewing",
            Self::Open => "open",
            Self::Approved => "approved",
            Self::Executing => "executing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
            Self::Invalid => "invalid",
        }
    }

    /// Over: nothing more happens to it.
    pub(crate) fn terminal(self) -> bool {
        !matches!(
            self,
            Self::Proposed | Self::Previewing | Self::Open | Self::Approved | Self::Executing
        )
    }

    /// Waiting for the owner.
    fn waiting(self) -> bool {
        matches!(self, Self::Proposed | Self::Previewing | Self::Open)
    }
}

/// Where an action's preview went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Preview {
    /// The key of the notice that carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    /// The mail chat that stored it, and the owner it is addressed to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stored_at: Option<u64>,
    /// When the platform took it, in host milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delivered_ms: Option<u64>,
    pub(crate) reissues: u32,
}

/// The evidence an approval was recorded with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Approval {
    pub(crate) route: String,
    pub(crate) peer: String,
    pub(crate) message_id: String,
    pub(crate) sent_ms: u64,
    pub(crate) recorded_at: u64,
    /// The digest the owner approved.
    pub(crate) digest: String,
}

/// Why an action ended as it did, or what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeCode {
    /// It happened.
    Applied,
    /// It had already happened (found by a check).
    AlreadyDone,
    /// The message is no longer there.
    Gone,
    /// Too long after the approval to act.
    Stale,
    /// The settings no longer allow it.
    Policy,
    /// The content, credentials, or target no longer match.
    Mismatch,
    /// The provider refused it.
    Refused,
    /// The provider refused the credentials.
    AuthFailed,
    /// The provider could not be reached.
    Unreachable,
    /// The provider cannot do it (no folder, no capability).
    Unsupported,
    /// Its outcome could not be told.
    Ambiguous,
    /// A mutation was answered HTTP 401. It may have happened, and it was
    /// not tried again.
    AuthUncertain,
    /// Something went wrong in SCV.
    Internal,
}

impl OutcomeCode {
    fn reason(self) -> &'static str {
        match self {
            Self::Applied | Self::AlreadyDone => "it happened",
            Self::Gone => "the mail is no longer there",
            Self::Stale => "its approval was too long ago to act on now",
            Self::Policy => "the settings no longer allow it",
            Self::Mismatch => "its target or content no longer matches what you approved",
            Self::Refused => "the mail server refused it",
            Self::AuthFailed => "the mail server refused SCV's sign-in",
            Self::Unreachable => "the mail server could not be reached",
            Self::Unsupported => "the mailbox cannot do it",
            Self::Ambiguous => "SCV could not tell whether it happened",
            Self::AuthUncertain => {
                "the mail server refused the access token after the change was sent, and SCV \
                 could not tell whether it happened"
            }
            Self::Internal => "of an internal error",
        }
    }
}

/// Where a sent message's copy in Sent stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SentCopyState {
    Pending,
    Saved,
    Failed,
}

/// What became of an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionOutcome {
    pub(crate) at: u64,
    pub(crate) code: OutcomeCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sent_copy: Option<SentCopyState>,
}

/// How to take up an IMAP move a check found half done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Resume {
    /// The copy is in place: mark the original deleted and expunge it.
    AfterCopy,
    /// The original is marked deleted: expunge it.
    Expunge,
}

/// One action's record: IDs, codes, and states, never mail text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionEntry {
    pub(crate) id: String,
    pub(crate) kind: ActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) form: Option<Form>,
    /// The handle of the mail it acts on or answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) handle: Option<String>,
    pub(crate) digest: String,
    /// Who asked, so asking again adds nothing.
    pub(crate) origin_key: String,
    pub(crate) state: ActionState,
    /// The code that approves it now.
    pub(crate) code: String,
    pub(crate) generation: u32,
    /// Codes of earlier generations, which answer "replaced".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) old_codes: Vec<String>,
    #[serde(default)]
    pub(crate) preview: Preview,
    pub(crate) created_at: u64,
    pub(crate) hard_expiry: u64,
    /// No approval counts from this second on.
    pub(crate) expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approval: Option<Approval>,
    /// It may start only until this second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) execute_by: Option<u64>,
    #[serde(default)]
    pub(crate) attempts: u32,
    /// Not before this second (a retry's backoff).
    #[serde(default)]
    pub(crate) not_before: u64,
    /// An interrupted run is checked from this second on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) probe_after: Option<u64>,
    /// A mutation was answered HTTP 401. Kept for the rest of the action's
    /// life: a check may record that the change happened, and nothing may
    /// carry the action out again.
    #[serde(default)]
    pub(crate) uncertain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resume: Option<Resume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<ActionOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_at: Option<u64>,
}

impl ActionEntry {
    /// Whether `code` is one of its codes, now or before.
    fn has_code(&self, code: &str) -> bool {
        self.code == code || self.old_codes.iter().any(|old| old == code)
    }

    fn end(&mut self, state: ActionState, code: Option<OutcomeCode>, now: u64) {
        self.state = state;
        self.terminal_at = Some(now);
        if let Some(code) = code {
            self.outcome = Some(ActionOutcome {
                at: now,
                code,
                sent_copy: self.outcome.as_ref().and_then(|outcome| outcome.sent_copy),
            });
        }
    }
}

/// A finished action, kept so its codes stay taken and a late command is
/// answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tombstone {
    pub(crate) id: String,
    pub(crate) codes: Vec<String>,
    pub(crate) kind: ActionKind,
    pub(crate) state: ActionState,
    pub(crate) at: u64,
}

/// A reported message's handle and what it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Handle {
    pub(crate) handle: String,
    pub(crate) source: SourceRef,
    pub(crate) identity: String,
    pub(crate) at: u64,
}

/// What an owner's request asks the worker to prepare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RequestWork {
    Reply {
        handle: String,
        text: String,
    },
    Forward {
        handle: String,
        to: Vec<String>,
        note: String,
    },
    Compose {
        to: Vec<String>,
        text: String,
    },
    /// Draft the mail of action `action` (and its siblings) again.
    Revise {
        action: String,
        text: String,
    },
    Message {
        handle: String,
        kind: ActionKind,
    },
}

/// An owner's request, waiting for the worker. Its text is the owner's own
/// words, at most 2 KiB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    /// `owner:<route>:<platform message ID>`: the same message handed over
    /// again adds nothing.
    pub(crate) key: String,
    pub(crate) work: RequestWork,
    pub(crate) route: String,
    pub(crate) attempts: u32,
    pub(crate) at: u64,
}

/// An approval counted against its daily limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reservation {
    pub(crate) id: String,
    pub(crate) limit: Limit,
    pub(crate) at: u64,
}

/// An action about to be proposed: its content written, its code drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewAction {
    pub(crate) id: String,
    pub(crate) kind: ActionKind,
    pub(crate) group: Option<String>,
    pub(crate) form: Option<Form>,
    pub(crate) handle: Option<String>,
    pub(crate) digest: String,
    pub(crate) origin_key: String,
    pub(crate) code: String,
    pub(crate) created_at: u64,
    pub(crate) hard_expiry: u64,
}

impl NewAction {
    pub(crate) fn new(content: &ActionContent, origin_key: String, code: String) -> Self {
        Self {
            id: content.id.clone(),
            kind: content.kind,
            group: content.group.clone(),
            form: content.message.as_ref().map(|message| message.form),
            handle: content
                .display
                .as_ref()
                .map(|display| display.handle.clone()),
            digest: content.digest.clone(),
            origin_key,
            code,
            created_at: content.created_at,
            hard_expiry: content.hard_expiry,
        }
    }

    fn entry(self) -> ActionEntry {
        ActionEntry {
            id: self.id,
            kind: self.kind,
            group: self.group,
            form: self.form,
            handle: self.handle,
            digest: self.digest,
            origin_key: self.origin_key,
            state: ActionState::Proposed,
            code: self.code,
            generation: 1,
            old_codes: Vec::new(),
            preview: Preview::default(),
            created_at: self.created_at,
            hard_expiry: self.hard_expiry,
            expires_at: self.hard_expiry,
            approval: None,
            execute_by: None,
            attempts: 0,
            not_before: 0,
            probe_after: None,
            uncertain: false,
            resume: None,
            outcome: None,
            terminal_at: None,
        }
    }
}

/// Created only by [`Ledger::begin_execution`], moved into the executor,
/// and consumed by [`Ledger::finish_execution`]: the executor's write
/// paths take it by reference and derive every target from it. It is not
/// `Clone`, and not `Serialize`; its fields are private to this module.
pub(crate) struct Approved {
    id: String,
    content: ActionContent,
    attempt: u32,
    resume: Option<Resume>,
    _seal: seal::Seal,
}

mod seal {
    /// Only [`super`] can make one, so only the ledger makes an
    /// [`super::Approved`].
    pub(crate) struct Seal(());

    impl Seal {
        pub(super) fn new() -> Self {
            Self(())
        }
    }
}

impl Approved {
    pub(crate) fn content(&self) -> &ActionContent {
        &self.content
    }

    /// Which try this is, from 1.
    pub(crate) fn attempt(&self) -> u32 {
        self.attempt
    }

    /// How to take up a half-done IMAP move.
    pub(crate) fn resume(&self) -> Option<Resume> {
        self.resume
    }
}

#[cfg(test)]
impl Approved {
    /// An approval of `content` made without the ledger, for the tests of
    /// what consumes one (the executor's effects).
    pub(crate) fn for_tests(content: ActionContent, attempt: u32, resume: Option<Resume>) -> Self {
        Self {
            id: content.id.clone(),
            content,
            attempt,
            resume,
            _seal: seal::Seal::new(),
        }
    }
}

impl std::fmt::Debug for Approved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Approved")
            .field("id", &self.id)
            .field("kind", &self.content.kind)
            .finish_non_exhaustive()
    }
}

/// What carrying an action out did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Execution {
    /// It happened (or had happened); for a send that SCV copies to Sent
    /// itself, the copy is still to come.
    Applied {
        code: OutcomeCode,
        sent_copy: Option<SentCopyState>,
    },
    /// It did not happen; `retry` when trying again may work.
    NotApplied { retry: bool, code: OutcomeCode },
    /// It may or may not have happened: check before anything else. For a
    /// draft or another non-send, a check that finds it not done may try
    /// again.
    Ambiguous,
    /// A mutation was answered HTTP 401. It may have happened. Check it,
    /// and do not carry it out again: any check other than one that finds
    /// it done ends the action unknown. Trying again takes a new proposal
    /// from the owner.
    Uncertain,
}

/// What a read-only check of an interrupted action found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    /// It happened.
    Done,
    /// It did not happen, and trying again is safe (never for a send).
    NotDone,
    /// An IMAP move half done: take it up from here.
    Resume(Resume),
    /// The check cannot tell.
    Unknown,
    /// The provider could not be reached; check again later.
    Unreachable,
    /// The message it acts on is gone: nothing was done.
    Gone,
}

/// Why an action cannot start now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotStarted {
    /// It is not approved, or its retry waits.
    NotReady,
    /// A check failed; it ended with this.
    Ended(ActionState),
}

/// Why a proposal was not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `mail.actions.max_open` actions wait already.
    Full,
    /// The same request made them already.
    Duplicate,
    /// What it revises has been approved or is over.
    Started,
}

/// Whether an owner's request was taken for the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Added,
    Duplicate,
    Full,
}

/// What the ledger learns from the hub while it approves.
pub(crate) trait World: Send + Sync {
    /// The owner of the running chat account `route`; `None` while it is
    /// not running or has no owner.
    fn owner(&self, route: &str) -> Option<String>;
    /// How the notice `key` went in mail chat `route`.
    fn delivered(&self, route: &str, key: &str) -> Option<KeyedOutcome>;
}

/// What recovery found when the account started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Recovered {
    /// Every code the account answers for, live or remembered.
    pub(crate) codes: Vec<String>,
    /// Every handle still valid.
    pub(crate) handles: Vec<String>,
    /// Proposed actions no queued notice previews.
    pub(crate) unpreviewed: Vec<String>,
}

/// What finished actions left to clean up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Swept {
    /// Actions tombstoned: their content files can go.
    pub(crate) tombstoned: Vec<String>,
    /// Codes and handles no longer answered for.
    pub(crate) codes: Vec<String>,
    pub(crate) handles: Vec<String>,
}

/// The mark of a notice that previews actions; its key is kept for it.
pub(crate) fn preview_key(id: &str, generation: u32) -> String {
    format!("preview:{id}:{generation}")
}

/// Record proposed actions whose content is written, and give their IDs.
/// Each one is bound to the chat that may deny it before a preview is stored.
pub(crate) fn add_proposed(
    state: &mut MailState,
    proposed: Vec<NewAction>,
    _now: u64,
    routes: &[String],
) -> Vec<String> {
    let mut ids = Vec::new();
    for action in proposed {
        if state.actions.iter().any(|entry| entry.id == action.id) {
            continue;
        }
        ids.push(action.id.clone());
        state.actions.push(action.entry());
    }
    bind_unassigned(state, routes);
    ids
}

/// The chat an action is for before any preview is stored: the chat its
/// owner asked in, when this account still reports there, otherwise the
/// first chat it reports to. `origin_key` for an owner's request is
/// `owner:<route>:<message id>:<index>`.
fn intended_route(origin_key: &str, routes: &[String]) -> Option<String> {
    if let Some(route) = owner_route(origin_key)
        && routes.iter().any(|known| known == route)
    {
        return Some(route.to_owned());
    }
    routes.first().cloned()
}

/// `<route>` from `owner:<route>:<message id>:<index>`. A route is
/// `<channel>:<account>`, neither of which holds a colon, while a message ID
/// may (a Slack message's is `<conversation>:<ts>`).
fn owner_route(origin_key: &str) -> Option<&str> {
    let rest = origin_key.strip_prefix("owner:")?;
    let mut parts = rest.splitn(3, ':');
    let (channel, account) = (parts.next()?, parts.next()?);
    parts.next()?;
    (!channel.is_empty() && !account.is_empty()).then(|| &rest[..channel.len() + 1 + account.len()])
}

/// Give every proposed or previewing action that names no chat the chat
/// [`intended_route`] picks. A stored preview already names its chat and is
/// left alone. Deny uses this binding and does not invent one.
pub(crate) fn bind_unassigned(state: &mut MailState, routes: &[String]) {
    for entry in &mut state.actions {
        if entry.preview.route.is_some()
            || !matches!(entry.state, ActionState::Proposed | ActionState::Previewing)
        {
            continue;
        }
        entry.preview.route = intended_route(&entry.origin_key, routes);
    }
}

/// A batch starts: the actions its items preview are being handed over.
/// A chat chosen when the action was proposed stays chosen. Clearing it
/// would let another chat's owner deny the action before this preview is
/// stored.
pub(crate) fn batch_previews(state: &mut MailState, batch: &super::Batch, routes: &[String]) {
    let ids: Vec<String> = state
        .queue
        .iter()
        .filter(|item| batch.seqs.contains(&item.seq))
        .flat_map(|item| item.actions.iter().cloned())
        .collect();
    for entry in &mut state.actions {
        if ids.contains(&entry.id)
            && matches!(entry.state, ActionState::Proposed | ActionState::Previewing)
        {
            entry.state = ActionState::Previewing;
            entry.preview.key = Some(batch.key.clone());
            entry.preview.stored_at = None;
            entry.preview.delivered_ms = None;
        }
    }
    bind_unassigned(state, routes);
}

/// Mail chat `route` stored the notice `key` for `peer`.
pub(crate) fn previews_stored(state: &mut MailState, key: &str, route: &str, peer: &str, now: u64) {
    for entry in &mut state.actions {
        if entry.state == ActionState::Previewing && entry.preview.key.as_deref() == Some(key) {
            entry.preview.route = Some(route.to_owned());
            entry.preview.peer = Some(peer.to_owned());
            entry.preview.stored_at = Some(now);
        }
    }
}

/// No mail chat took the notice `key`: its actions expire.
pub(crate) fn previews_lost(state: &mut MailState, key: &str, now: u64) {
    let mut expired = 0;
    for entry in &mut state.actions {
        if entry.state == ActionState::Previewing && entry.preview.key.as_deref() == Some(key) {
            entry.end(ActionState::Expired, None, now);
            expired += 1;
        }
    }
    state.counts.expired += expired;
}

/// The platform took notice `key` at `at_ms` (host clock): its actions
/// open for approval until their window closes.
pub(crate) fn previews_delivered(state: &mut MailState, key: &str, at_ms: u64, policy: &Policy) {
    for entry in &mut state.actions {
        if entry.state == ActionState::Previewing
            && entry.preview.key.as_deref() == Some(key)
            && entry.preview.stored_at.is_some()
        {
            open(entry, at_ms, policy);
        }
    }
}

fn open(entry: &mut ActionEntry, at_ms: u64, policy: &Policy) {
    entry.state = ActionState::Open;
    entry.preview.delivered_ms = Some(at_ms);
    entry.expires_at = entry
        .hard_expiry
        .min(at_ms / 1000 + policy.actions.approval_hours * HOUR);
}

/// The platform refused notice `key`: its actions are offered again with a
/// new code from `reissue`, at most [`MAX_REISSUES`] times, and otherwise
/// expire. Returns the actions to preview again.
pub(crate) fn previews_refused(
    state: &mut MailState,
    key: &str,
    reissue: &mut (dyn FnMut() -> Option<String> + Send),
    now: u64,
    routes: &[String],
) -> Vec<String> {
    let mut again = Vec::new();
    let mut expired = 0;
    for entry in &mut state.actions {
        if entry.state != ActionState::Previewing || entry.preview.key.as_deref() != Some(key) {
            continue;
        }
        let code = (entry.preview.reissues < MAX_REISSUES)
            .then(&mut *reissue)
            .flatten();
        if let Some(code) = code {
            let old = std::mem::replace(&mut entry.code, code);
            entry.old_codes.push(old);
            entry.generation += 1;
            entry.state = ActionState::Proposed;
            entry.preview = Preview {
                reissues: entry.preview.reissues + 1,
                ..Preview::default()
            };
            again.push(entry.id.clone());
        } else {
            entry.end(ActionState::Expired, None, now);
            expired += 1;
        }
    }
    state.counts.expired += expired;
    bind_unassigned(state, routes);
    again
}

/// Whether `item` previews an action that is not over.
pub(crate) fn offers_live(state: &MailState, item: &Item) -> bool {
    item.actions.iter().any(|id| {
        state
            .actions
            .iter()
            .any(|entry| entry.id == *id && !entry.state.terminal())
    })
}

/// Drop what is past its age regardless of settings: the approval ring,
/// reservations older than a day, and stale requests.
pub(crate) fn prune(state: &mut MailState, now: u64) {
    state
        .approvals
        .retain(|approval| now.saturating_sub(approval.at) < APPROVAL_SECONDS);
    let excess = state.approvals.len().saturating_sub(MAX_APPROVALS);
    state.approvals.drain(..excess);
    state
        .reservations
        .retain(|reservation| now.saturating_sub(reservation.at) < DAY);
    state
        .requests
        .retain(|request| now.saturating_sub(request.at) < REQUEST_SECONDS);
}

/// What the owner is told an action does, in SCV's words: its kind and the
/// handle of the mail, never an address or a subject.
pub(crate) fn describe(kind: ActionKind, form: Option<Form>, handle: Option<&str>) -> String {
    let mail = |form: Option<Form>| match (form, handle) {
        (Some(Form::Reply), Some(handle)) => format!("the reply to #{handle}"),
        (Some(Form::Forward), Some(handle)) => format!("the forward of #{handle}"),
        (Some(Form::Reply), None) => "the reply".to_owned(),
        (Some(Form::Forward), None) => "the forward".to_owned(),
        (Some(Form::Compose) | None, _) => "the new mail".to_owned(),
    };
    let message = handle.map_or_else(|| "the mail".to_owned(), |handle| format!("#{handle}"));
    match kind {
        ActionKind::Draft => format!("saving {} in Drafts", mail(form)),
        ActionKind::Send => format!("sending {}", mail(form)),
        ActionKind::Archive => format!("archiving {message}"),
        ActionKind::MarkRead => format!("marking {message} read"),
        ActionKind::Trash => format!("moving {message} to Trash"),
        ActionKind::Spam => format!("moving {message} to Spam"),
    }
}

/// The same, done.
fn describe_done(entry: &ActionEntry) -> String {
    let handle = entry.handle.as_deref();
    let mail = match (entry.form, handle) {
        (Some(Form::Reply), Some(handle)) => format!("the reply to #{handle}"),
        (Some(Form::Forward), Some(handle)) => format!("the forward of #{handle}"),
        _ => "the new mail".to_owned(),
    };
    let message = handle.map_or_else(|| "the mail".to_owned(), |handle| format!("#{handle}"));
    match entry.kind {
        ActionKind::Draft => format!("saved {mail} in Drafts"),
        ActionKind::Send => format!("sent {mail}"),
        ActionKind::Archive => format!("archived {message}"),
        ActionKind::MarkRead => format!("marked {message} read"),
        ActionKind::Trash => format!("moved {message} to Trash"),
        ActionKind::Spam => format!("moved {message} to Spam"),
    }
}

fn describe_entry(entry: &ActionEntry) -> String {
    describe(entry.kind, entry.form, entry.handle.as_deref())
}

/// SCV's answer about code `code` of an action in `state`, when that state
/// means the code no longer approves anything. `outcome` is that action's
/// outcome code, when it has one.
fn state_answer(
    code: &str,
    state: ActionState,
    replaced: bool,
    outcome: Option<OutcomeCode>,
) -> String {
    if replaced {
        return format!("{code} was replaced by a newer request.");
    }
    match state {
        ActionState::Proposed | ActionState::Previewing => {
            format!("{code} has not reached you yet; approve it once it has.")
        }
        ActionState::Open => format!("{code} is waiting for your answer."),
        ActionState::Approved => format!("{code} was already approved."),
        ActionState::Executing => format!("{code} is being carried out."),
        ActionState::Done => format!("{code} is done."),
        ActionState::Failed => format!("{code} was tried and did not happen."),
        ActionState::Unknown if outcome == Some(OutcomeCode::AuthUncertain) => format!(
            "{code} was tried; the mail server refused the access token and SCV does not know \
             whether it happened. Check the mailbox. SCV will not retry it. Propose it again only \
             if it did not happen."
        ),
        ActionState::Unknown => {
            format!("{code} was tried; SCV does not know whether it happened. Check the mailbox.")
        }
        ActionState::Denied => format!("{code} was denied."),
        ActionState::Expired => format!("{code} expired."),
        ActionState::Superseded => {
            format!("{code} was not needed: another choice for the same mail was approved.")
        }
        ActionState::Cancelled => format!("{code} was cancelled."),
        ActionState::Invalid => format!("{code} was refused: it no longer matched what you saw."),
    }
}

/// Queue SCV's line about an action's end for the owner, once.
fn tell(state: &mut MailState, id: &str, text: String, now: u64) {
    push_item_with(
        state,
        &format!("outcome:{id}"),
        Class::Response,
        false,
        text,
        Vec::new(),
        now,
    );
}

/// The outcome line for `entry`, which just ended.
fn outcome_text(entry: &ActionEntry) -> Option<String> {
    let code = &entry.code;
    match entry.state {
        ActionState::Done => {
            let copy = match entry.outcome.as_ref().and_then(|outcome| outcome.sent_copy) {
                Some(SentCopyState::Failed) => "; its copy in Sent could not be saved",
                _ => "",
            };
            Some(format!("Done: {code} {}{copy}.", describe_done(entry)))
        }
        ActionState::Failed | ActionState::Invalid => Some(format!(
            "Not done: {code} ({}) did not happen because {}.",
            describe_entry(entry),
            entry
                .outcome
                .as_ref()
                .map_or(OutcomeCode::Internal, |outcome| outcome.code)
                .reason()
        )),
        ActionState::Unknown
            if entry
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.code == OutcomeCode::AuthUncertain) =>
        {
            Some(format!(
                "SCV could not tell whether {code} ({}) happened: the mail server refused the \
                 access token after the change was sent. Check the mailbox. SCV will not retry \
                 it. Propose it again only if it did not happen.",
                describe_entry(entry)
            ))
        }
        ActionState::Unknown => Some(format!(
            "SCV lost track of {code} ({}) while carrying it out; it may or may not have \
             happened. Check the mailbox. SCV will not retry it.",
            describe_entry(entry)
        )),
        _ => None,
    }
}

/// Release `id`'s daily reservation: it will not happen.
fn release(state: &mut MailState, id: &str) {
    state
        .reservations
        .retain(|reservation| reservation.id != id);
}

/// End entry `index` in `ended`, releasing its reservation unless it may
/// have happened, and tell the owner when it was approved.
fn finish(
    state: &mut MailState,
    index: usize,
    ended: ActionState,
    code: Option<OutcomeCode>,
    now: u64,
) {
    let entry = &mut state.actions[index];
    let approved = entry.approval.is_some();
    entry.end(ended, code, now);
    let id = entry.id.clone();
    let text = approved.then(|| outcome_text(entry)).flatten();
    if !matches!(ended, ActionState::Done | ActionState::Unknown) {
        release(state, &id);
    }
    if let Some(text) = text {
        tell(state, &id, text, now);
    }
}

impl Ledger {
    /// The policy actions follow; `None` when the account only reads.
    pub(crate) fn actions_policy(&self) -> Option<&Policy> {
        self.policy.as_deref()
    }

    /// Add the owner's request for the worker, unless the same message
    /// asked already or [`MAX_REQUESTS`] wait.
    pub(crate) async fn add_request(&self, request: Request) -> anyhow::Result<Admission> {
        self.commit(|state| {
            if state.requests.iter().any(|known| known.key == request.key)
                || state
                    .actions
                    .iter()
                    .any(|entry| entry.origin_key.starts_with(&format!("{}:", request.key)))
            {
                return Admission::Duplicate;
            }
            if state.requests.len() >= MAX_REQUESTS {
                return Admission::Full;
            }
            state.requests.push(request);
            Admission::Added
        })
        .await
    }

    /// Count another try at request `key` before it starts; `None` once it
    /// is gone.
    pub(crate) async fn request_attempt(&self, key: &str) -> anyhow::Result<Option<u32>> {
        self.commit(|state| {
            state
                .requests
                .iter_mut()
                .find(|request| request.key == key)
                .map(|request| {
                    request.attempts += 1;
                    request.attempts
                })
        })
        .await
    }

    /// Drop request `key` without a proposal, telling the owner `text`.
    pub(crate) async fn drop_request(
        &self,
        key: &str,
        text: String,
        now: u64,
    ) -> anyhow::Result<()> {
        self.commit(|state| {
            state.requests.retain(|request| request.key != key);
            push_item_with(
                state,
                &format!("response:{key}"),
                Class::Response,
                false,
                text,
                Vec::new(),
                now,
            );
        })
        .await
    }

    /// Count one drafting turn for the owner's local day `today`.
    pub(crate) async fn composed(&self, today: &str, tokens: u64) -> anyhow::Result<()> {
        self.commit(|state| {
            super::roll_day(state, today);
            state.day.composed += 1;
            state.day.tokens = state.day.tokens.saturating_add(tokens);
        })
        .await
    }

    /// Record `proposed` (their content files written, their codes drawn)
    /// for request `request`, with the notice `preview` offering them, in
    /// one write; superseding the actions `revised`, which a revision
    /// replaces, unless one of them was approved or is over.
    pub(crate) async fn propose(
        &self,
        request: Option<&str>,
        proposed: Vec<NewAction>,
        preview: (String, String),
        revised: Vec<String>,
        now: u64,
    ) -> anyhow::Result<Result<(), Refusal>> {
        let max_open = self
            .policy
            .as_ref()
            .map_or(0, |policy| policy.actions.max_open);
        let routes = self
            .policy
            .as_ref()
            .map(|policy| policy.routes.clone())
            .unwrap_or_default();
        self.commit(|state| {
            let live = state
                .actions
                .iter()
                .filter(|entry| !entry.state.terminal())
                .count();
            if proposed.iter().any(|action| {
                state
                    .actions
                    .iter()
                    .any(|entry| entry.origin_key == action.origin_key)
            }) {
                return Err(Refusal::Duplicate);
            }
            let replaced: Vec<usize> = state
                .actions
                .iter()
                .enumerate()
                .filter(|(_, entry)| revised.contains(&entry.id))
                .map(|(index, _)| index)
                .collect();
            if replaced.iter().any(|&index| {
                let state = state.actions[index].state;
                !state.waiting() && state != ActionState::Superseded
            }) {
                return Err(Refusal::Started);
            }
            let waiting = replaced
                .iter()
                .filter(|&&index| state.actions[index].state.waiting())
                .count();
            if live - waiting + proposed.len() > max_open {
                return Err(Refusal::Full);
            }
            for index in replaced {
                if state.actions[index].state.waiting() {
                    state.actions[index].end(ActionState::Superseded, None, now);
                }
            }
            if let Some(request) = request {
                state.requests.retain(|known| known.key != request);
            }
            let ids = add_proposed(state, proposed, now, &routes);
            let (key, text) = preview;
            push_item_with(state, &key, Class::Response, false, text, ids, now);
            Ok(())
        })
        .await
    }

    /// Queue the notice `key`, `text`, previewing actions `ids` again.
    pub(crate) async fn queue_preview(
        &self,
        ids: Vec<String>,
        key: String,
        text: String,
        now: u64,
    ) -> anyhow::Result<()> {
        let routes = self
            .policy
            .as_ref()
            .map(|policy| policy.routes.clone())
            .unwrap_or_default();
        self.commit(|state| {
            push_item_with(state, &key, Class::Response, false, text, ids, now);
            bind_unassigned(state, &routes);
        })
        .await
    }

    /// The owner's `approve` with `codes`, checked as the mail design says,
    /// code by code; every code that passes is recorded in one write, with
    /// its siblings superseded and its daily limit reserved. The answer has
    /// one line per code, SCV's words only.
    pub(crate) async fn approve(
        &self,
        codes: &[String],
        evidence: &ChatEvidence,
        world: &dyn World,
        now: u64,
    ) -> anyhow::Result<Vec<String>> {
        let Some(policy) = self.policy.clone() else {
            return Ok(vec![crate::mail_chat::NOT_RUNNING_REPLY.to_owned()]);
        };
        self.commit(|state| approve(state, codes, evidence, world, &policy, now))
            .await
    }

    /// The owner's `deny` with `codes`, or of everything waiting for this
    /// chat when `codes` is `None`. An action answers only in the chat its
    /// preview is bound to: the chat that stored it, or, before that, the
    /// chat it was proposed for. An action that names no chat is not
    /// denied, by code or by `deny all`. Only the current code denies. An
    /// earlier code answers that it was replaced. An action being carried
    /// out cannot be denied.
    pub(crate) async fn deny(
        &self,
        codes: Option<&[String]>,
        evidence: &ChatEvidence,
        world: &dyn World,
        now: u64,
    ) -> anyhow::Result<Vec<String>> {
        let Some(policy) = self.policy.clone() else {
            return Ok(vec![crate::mail_chat::NOT_RUNNING_REPLY.to_owned()]);
        };
        self.commit(|state| deny(state, codes, evidence, world, &policy, now))
            .await
    }

    /// `scv mail cancel`: withdraw action `id`, or every action that has not
    /// started. Answers in SCV's words, naming actions by ID only.
    pub(crate) async fn cancel(&self, id: Option<&str>, now: u64) -> anyhow::Result<String> {
        self.commit(|state| {
            let mut cancelled = 0;
            let mut running = 0;
            let mut found = id.is_none();
            for index in 0..state.actions.len() {
                let entry = &state.actions[index];
                if id.is_some_and(|id| entry.id != id) || entry.state.terminal() {
                    if id.is_some_and(|id| entry.id == id) {
                        found = true;
                    }
                    continue;
                }
                found = true;
                if entry.state == ActionState::Executing {
                    running += 1;
                    continue;
                }
                finish(state, index, ActionState::Cancelled, None, now);
                cancelled += 1;
            }
            match (found, cancelled, running) {
                (false, ..) => "No such mail action is waiting.".to_owned(),
                (true, 0, 0) => "No mail action was waiting.".to_owned(),
                (true, cancelled, 0) => format!("Cancelled {cancelled} mail action(s)."),
                (true, cancelled, running) => format!(
                    "Cancelled {cancelled} mail action(s); {running} being carried out cannot be \
                     cancelled now."
                ),
            }
        })
        .await
    }

    /// Expire actions whose approval can no longer come; count them for the
    /// next digest.
    pub(crate) async fn expire(&self, now: u64) -> anyhow::Result<usize> {
        let due = self.snapshot().actions.iter().any(|entry| {
            (entry.state.waiting() && now >= entry.expires_at)
                || (entry.state == ActionState::Approved
                    && entry.execute_by.is_some_and(|by| now > by))
        });
        if !due {
            return Ok(0);
        }
        self.commit(|state| {
            let mut expired = 0;
            for index in 0..state.actions.len() {
                let entry = &state.actions[index];
                if entry.state.waiting() && now >= entry.expires_at {
                    state.actions[index].end(ActionState::Expired, None, now);
                    expired += 1;
                } else if entry.state == ActionState::Approved
                    && entry.execute_by.is_some_and(|by| now > by)
                {
                    finish(
                        state,
                        index,
                        ActionState::Failed,
                        Some(OutcomeCode::Stale),
                        now,
                    );
                }
            }
            state.counts.expired += expired as u64;
            expired
        })
        .await
    }

    /// The approved action to carry out next, oldest first, if one is due.
    pub(crate) fn next_approved(&self, now: u64) -> Option<String> {
        self.snapshot()
            .actions
            .iter()
            .filter(|entry| entry.state == ActionState::Approved && entry.not_before <= now)
            .min_by_key(|entry| {
                entry
                    .approval
                    .as_ref()
                    .map_or(entry.created_at, |approval| approval.recorded_at)
            })
            .map(|entry| entry.id.clone())
    }

    /// Actions that were interrupted and are due for a check.
    pub(crate) fn due_probes(&self, now: u64) -> Vec<String> {
        self.snapshot()
            .actions
            .iter()
            .filter(|entry| {
                entry.state == ActionState::Executing
                    && entry.probe_after.is_some_and(|after| after <= now)
            })
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Start carrying out action `id`, whose content file holds `content`:
    /// it must be approved and due, not past its deadline, its content must
    /// still hash to what was approved, the credentials and the settings
    /// must still allow it. Only then is it marked `executing`, with one
    /// more try, and the sealed [`Approved`] returned. A failed check ends
    /// the action in the same write.
    pub(crate) async fn begin_execution(
        &self,
        id: &str,
        content: Option<ActionContent>,
        now: u64,
    ) -> anyhow::Result<Result<Approved, NotStarted>> {
        let Some(policy) = self.policy.clone() else {
            return Ok(Err(NotStarted::NotReady));
        };
        self.commit(|state| {
            let Some(index) = state.actions.iter().position(|entry| entry.id == id) else {
                return Err(NotStarted::NotReady);
            };
            let entry = &state.actions[index];
            if entry.state != ActionState::Approved || entry.not_before > now {
                return Err(NotStarted::NotReady);
            }
            // A mutation 401 may already have landed. Do not carry it out
            // again even if a check put it back to approved.
            if entry.uncertain {
                finish(
                    state,
                    index,
                    ActionState::Unknown,
                    Some(OutcomeCode::AuthUncertain),
                    now,
                );
                return Err(NotStarted::Ended(ActionState::Unknown));
            }
            if entry.execute_by.is_none_or(|by| now > by) {
                finish(
                    state,
                    index,
                    ActionState::Failed,
                    Some(OutcomeCode::Stale),
                    now,
                );
                return Err(NotStarted::Ended(ActionState::Failed));
            }
            let approved_digest = entry
                .approval
                .as_ref()
                .map(|approval| approval.digest.as_str());
            let matches = content.as_ref().is_some_and(|content| {
                let digest = content.compute_digest();
                content.id == entry.id
                    && digest == content.digest
                    && digest == entry.digest
                    && Some(digest.as_str()) == approved_digest
                    && content.kind == entry.kind
                    && content.fingerprint == policy.fingerprint
                    && content.account == policy.account
            });
            let Some(content) = content.filter(|_| matches) else {
                finish(
                    state,
                    index,
                    ActionState::Invalid,
                    Some(OutcomeCode::Mismatch),
                    now,
                );
                return Err(NotStarted::Ended(ActionState::Invalid));
            };
            let form = content.message.as_ref().map(|message| message.form);
            if !policy.offers(content.kind, form) || policy.recipients_refused(&content).is_some() {
                finish(
                    state,
                    index,
                    ActionState::Failed,
                    Some(OutcomeCode::Policy),
                    now,
                );
                return Err(NotStarted::Ended(ActionState::Failed));
            }
            let entry = &mut state.actions[index];
            entry.state = ActionState::Executing;
            entry.attempts += 1;
            entry.probe_after = None;
            let resume = entry.resume.take();
            Ok(Approved {
                id: entry.id.clone(),
                attempt: entry.attempts,
                content,
                resume,
                _seal: seal::Seal::new(),
            })
        })
        .await
    }

    /// Record that a send went out, before SCV copies it to Sent itself:
    /// from here on nothing can send it again.
    pub(crate) async fn mark_sent(&self, approved: &Approved, now: u64) -> anyhow::Result<()> {
        let id = approved.id.clone();
        self.commit(|state| {
            if let Some(entry) = state.actions.iter_mut().find(|entry| entry.id == id) {
                entry.end(ActionState::Done, Some(OutcomeCode::Applied), now);
                if let Some(outcome) = &mut entry.outcome {
                    outcome.sent_copy = Some(SentCopyState::Pending);
                }
            }
        })
        .await
    }

    /// Record what carrying `approved` out did, consuming it.
    pub(crate) async fn finish_execution(
        &self,
        approved: Approved,
        execution: Execution,
        now: u64,
    ) -> anyhow::Result<()> {
        let Approved { id, resume, .. } = approved;
        self.commit(|state| {
            let Some(index) = state.actions.iter().position(|entry| entry.id == id) else {
                return;
            };
            let entry = &mut state.actions[index];
            match execution {
                Execution::Applied { code, sent_copy } => {
                    if entry.state == ActionState::Done {
                        // A send already recorded; only its copy changes.
                        if let Some(outcome) = &mut entry.outcome {
                            outcome.sent_copy = sent_copy;
                        }
                        let text = outcome_text(entry);
                        if let Some(text) = text {
                            tell(state, &id, text, now);
                        }
                        return;
                    }
                    finish(state, index, ActionState::Done, Some(code), now);
                    if let Some(outcome) = &mut state.actions[index].outcome {
                        outcome.sent_copy = sent_copy;
                    }
                }
                Execution::NotApplied { retry, code } => {
                    let in_time = entry.execute_by.is_some_and(|by| now <= by);
                    if retry && entry.attempts < MAX_ATTEMPTS && in_time {
                        entry.state = ActionState::Approved;
                        entry.not_before = now + if entry.attempts <= 1 { 30 } else { 120 };
                        // Nothing changed this time, so a half-done move is
                        // still where the check found it.
                        entry.resume = resume;
                    } else {
                        finish(state, index, ActionState::Failed, Some(code), now);
                    }
                }
                Execution::Ambiguous => {
                    entry.probe_after = Some(now);
                }
                Execution::Uncertain => {
                    entry.uncertain = true;
                    entry.probe_after = Some(now);
                }
            }
        })
        .await
    }

    /// Record what a check of the interrupted action `id` found.
    pub(crate) async fn probed(&self, id: &str, probe: Probe, now: u64) -> anyhow::Result<()> {
        self.commit(|state| {
            let Some(index) = state
                .actions
                .iter()
                .position(|entry| entry.id == id && entry.state == ActionState::Executing)
            else {
                return;
            };
            let entry = &mut state.actions[index];
            let deadline = entry.execute_by.unwrap_or(entry.created_at);
            let in_time = now <= deadline && entry.attempts < MAX_ATTEMPTS;
            // HTTP 401 on a mutation: only a check that finds the change
            // resolves it. Anything else ends unknown, including a check
            // that says the change is not there.
            let uncertain = entry.uncertain;
            match probe {
                Probe::Done => finish(
                    state,
                    index,
                    ActionState::Done,
                    Some(OutcomeCode::AlreadyDone),
                    now,
                ),
                Probe::Gone | Probe::NotDone | Probe::Resume(_) if uncertain => finish(
                    state,
                    index,
                    ActionState::Unknown,
                    Some(OutcomeCode::AuthUncertain),
                    now,
                ),
                Probe::Gone => finish(
                    state,
                    index,
                    ActionState::Failed,
                    Some(OutcomeCode::Gone),
                    now,
                ),
                Probe::NotDone if entry.kind != ActionKind::Send && in_time => {
                    entry.state = ActionState::Approved;
                    entry.not_before = now;
                    entry.probe_after = None;
                }
                Probe::Resume(resume) if in_time => {
                    entry.state = ActionState::Approved;
                    entry.resume = Some(resume);
                    entry.not_before = now;
                    entry.probe_after = None;
                }
                Probe::NotDone if entry.kind != ActionKind::Send => {
                    finish(
                        state,
                        index,
                        ActionState::Failed,
                        Some(OutcomeCode::Stale),
                        now,
                    );
                }
                Probe::Unreachable if now <= deadline + PROBE_GRACE => {
                    let waited = entry
                        .probe_after
                        .map_or(0, |after| now.saturating_sub(after));
                    entry.probe_after = Some(now + (waited * 2).clamp(30, 600));
                }
                Probe::Unreachable | Probe::Unknown | Probe::NotDone | Probe::Resume(_) => {
                    let code = if uncertain {
                        OutcomeCode::AuthUncertain
                    } else {
                        OutcomeCode::Ambiguous
                    };
                    finish(state, index, ActionState::Unknown, Some(code), now);
                }
            }
        })
        .await
    }

    /// Recover when the account starts: apply the settings it starts with
    /// (a kind turned off, or a route removed, withdraws its waiting and
    /// approved actions; a shorter approval window shortens the waiting
    /// ones), have interrupted actions checked, tell the owner of a sent
    /// message whose copy in Sent a stop cut short, and say what the hub
    /// must know.
    pub(crate) async fn recover(&self, now: u64) -> anyhow::Result<Recovered> {
        let Some(policy) = self.policy.clone() else {
            return Ok(Recovered::default());
        };
        self.commit(|state| {
            bind_unassigned(state, &policy.routes);
            for index in 0..state.actions.len() {
                let entry = &state.actions[index];
                let off = !policy.allows(entry.kind, entry.form);
                let unrouted = entry
                    .preview
                    .route
                    .as_ref()
                    .is_some_and(|route| !policy.routes.contains(route));
                let copy_pending = entry
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.sent_copy == Some(SentCopyState::Pending));
                match entry.state {
                    ActionState::Proposed
                    | ActionState::Previewing
                    | ActionState::Open
                    | ActionState::Approved
                        if off || unrouted =>
                    {
                        finish(state, index, ActionState::Cancelled, None, now);
                    }
                    ActionState::Open => {
                        let entry = &mut state.actions[index];
                        if let Some(at_ms) = entry.preview.delivered_ms {
                            entry.expires_at = entry
                                .expires_at
                                .min(at_ms / 1000 + policy.actions.approval_hours * HOUR);
                        }
                    }
                    ActionState::Executing => {
                        state.actions[index].probe_after = Some(now);
                    }
                    // Sent, and stopped before its copy in Sent was saved.
                    ActionState::Done if copy_pending => {
                        let entry = &mut state.actions[index];
                        if let Some(outcome) = &mut entry.outcome {
                            outcome.sent_copy = Some(SentCopyState::Failed);
                        }
                        let id = entry.id.clone();
                        if let Some(text) = outcome_text(entry) {
                            tell(state, &id, text, now);
                        }
                    }
                    _ => {}
                }
            }
            let previewed: Vec<&String> = state
                .queue
                .iter()
                .flat_map(|item| item.actions.iter())
                .collect();
            let unpreviewed = state
                .actions
                .iter()
                .filter(|entry| {
                    entry.state == ActionState::Proposed && !previewed.contains(&&entry.id)
                })
                .map(|entry| entry.id.clone())
                .collect();
            let mut codes: Vec<String> = state
                .actions
                .iter()
                .flat_map(|entry| {
                    std::iter::once(entry.code.clone()).chain(entry.old_codes.clone())
                })
                .collect();
            codes.extend(
                state
                    .tombstones
                    .iter()
                    .flat_map(|tombstone| tombstone.codes.clone()),
            );
            Recovered {
                codes,
                handles: state
                    .handles
                    .iter()
                    .map(|handle| handle.handle.clone())
                    .collect(),
                unpreviewed,
            }
        })
        .await
    }

    /// Tombstone finished actions (an `unknown` one only after
    /// `unknown_keep_days`), and drop tombstones and handles past their
    /// age. The caller then deletes the tombstoned actions' content files.
    pub(crate) async fn sweep_actions(
        &self,
        handle_seconds: u64,
        now: u64,
    ) -> anyhow::Result<Swept> {
        let Some(policy) = self.policy.clone() else {
            return Ok(Swept::default());
        };
        let due = {
            let snapshot = self.snapshot();
            snapshot
                .actions
                .iter()
                .any(|entry| tombstone_due(entry, &policy, now))
                || snapshot
                    .tombstones
                    .iter()
                    .any(|tombstone| now.saturating_sub(tombstone.at) >= policy.tombstone_seconds)
                || snapshot.tombstones.len() > MAX_TOMBSTONES
                || snapshot
                    .handles
                    .iter()
                    .any(|handle| now.saturating_sub(handle.at) >= handle_seconds)
                || snapshot.handles.len() > MAX_HANDLES
        };
        if !due {
            return Ok(Swept::default());
        }
        self.commit(|state| {
            let mut swept = Swept::default();
            let (over, kept): (Vec<ActionEntry>, Vec<ActionEntry>) =
                std::mem::take(&mut state.actions)
                    .into_iter()
                    .partition(|entry| tombstone_due(entry, &policy, now));
            state.actions = kept;
            for entry in over {
                swept.tombstoned.push(entry.id.clone());
                let mut codes = entry.old_codes;
                codes.push(entry.code);
                state.tombstones.push(Tombstone {
                    id: entry.id,
                    codes,
                    kind: entry.kind,
                    state: entry.state,
                    at: entry.terminal_at.unwrap_or(now),
                });
            }
            let old: Vec<Tombstone>;
            (old, state.tombstones) =
                std::mem::take(&mut state.tombstones)
                    .into_iter()
                    .partition(|tombstone| {
                        now.saturating_sub(tombstone.at) >= policy.tombstone_seconds
                    });
            swept
                .codes
                .extend(old.into_iter().flat_map(|tombstone| tombstone.codes));
            let excess = state.tombstones.len().saturating_sub(MAX_TOMBSTONES);
            swept.codes.extend(
                state
                    .tombstones
                    .drain(..excess)
                    .flat_map(|tombstone| tombstone.codes),
            );
            // A handle a live action names stays.
            let named: Vec<String> = state
                .actions
                .iter()
                .filter_map(|entry| entry.handle.clone())
                .collect();
            let (gone, kept): (Vec<Handle>, Vec<Handle>) = std::mem::take(&mut state.handles)
                .into_iter()
                .partition(|handle| {
                    now.saturating_sub(handle.at) >= handle_seconds
                        && !named.contains(&handle.handle)
                });
            state.handles = kept;
            swept
                .handles
                .extend(gone.into_iter().map(|handle| handle.handle));
            let excess = state.handles.len().saturating_sub(MAX_HANDLES);
            swept
                .handles
                .extend(state.handles.drain(..excess).map(|handle| handle.handle));
            swept
        })
        .await
    }

    /// The handle `handle` names, while it is valid.
    pub(crate) fn handle(&self, handle: &str) -> Option<Handle> {
        self.snapshot()
            .handles
            .into_iter()
            .find(|known| known.handle == handle)
    }

    /// The actions for `scv mail status`: those not over, by ID, kind, and
    /// state.
    pub(crate) fn listed(&self, component: &str) -> Vec<scv_protocol::MailAction> {
        self.snapshot()
            .actions
            .iter()
            .filter(|entry| !entry.state.terminal() || entry.state == ActionState::Unknown)
            .map(|entry| scv_protocol::MailAction {
                account: component.to_owned(),
                id: entry.id.clone(),
                kind: entry.kind.name().to_owned(),
                state: entry.state.name().to_owned(),
                created_unix_seconds: entry.created_at,
                expires_unix_seconds: entry.state.waiting().then_some(entry.expires_at),
            })
            .collect()
    }

    /// The `mail status` lines about actions, for the mail chat: codes,
    /// kinds, handles, and states, never an address or a subject.
    pub(crate) fn status_lines(&self, component: &str) -> String {
        let snapshot = self.snapshot();
        let waiting: Vec<String> = snapshot
            .actions
            .iter()
            .filter(|entry| entry.state == ActionState::Open)
            .map(|entry| format!("{} {}", entry.code, describe_entry(entry)))
            .collect();
        let count = |state: ActionState| {
            snapshot
                .actions
                .iter()
                .filter(|entry| entry.state == state)
                .count()
        };
        let mut line = format!(
            "{component}: {} waiting for your answer{}",
            waiting.len(),
            if waiting.is_empty() {
                String::new()
            } else {
                format!(" ({})", waiting.join("; "))
            }
        );
        let upcoming = count(ActionState::Proposed) + count(ActionState::Previewing);
        if upcoming > 0 {
            line.push_str(&format!(", {upcoming} on the way to you"));
        }
        for (state, label) in [
            (ActionState::Approved, "approved"),
            (ActionState::Executing, "being carried out"),
            (ActionState::Unknown, "of unknown outcome"),
        ] {
            let n = count(state);
            if n > 0 {
                line.push_str(&format!(", {n} {label}"));
            }
        }
        if !snapshot.requests.is_empty() {
            line.push_str(&format!(
                ", {} request(s) being prepared",
                snapshot.requests.len()
            ));
        }
        line.push('.');
        line
    }
}

fn tombstone_due(entry: &ActionEntry, policy: &Policy, now: u64) -> bool {
    match entry.state {
        ActionState::Unknown => entry
            .terminal_at
            .is_some_and(|at| now.saturating_sub(at) >= policy.unknown_keep_seconds),
        // A send whose copy to Sent is being saved still has its outcome to
        // tell; a restart settles the copy first.
        ActionState::Done
            if entry
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.sent_copy == Some(SentCopyState::Pending)) =>
        {
            false
        }
        state => state.terminal(),
    }
}

/// The approval itself; see [`Ledger::approve`].
fn approve(
    state: &mut MailState,
    codes: &[String],
    evidence: &ChatEvidence,
    world: &dyn World,
    policy: &Policy,
    now: u64,
) -> Vec<String> {
    let replayed = state
        .approvals
        .iter()
        .any(|approval| approval.key == evidence.message_id);
    let mut answers = Vec::new();
    let mut approved_any = false;
    for code in codes {
        let Some(index) = state.actions.iter().position(|entry| entry.has_code(code)) else {
            answers.push(
                match state.tombstones.iter().find(|t| t.codes.contains(code)) {
                    Some(tombstone) => state_answer(code, tombstone.state, false, None),
                    None => format!("No mail action has code {code}."),
                },
            );
            continue;
        };
        // A preview delivered since the ledger last looked opens now.
        {
            let entry = &mut state.actions[index];
            if entry.state == ActionState::Previewing
                && entry.code == *code
                && let (Some(route), Some(key), Some(_)) = (
                    entry.preview.route.clone(),
                    entry.preview.key.clone(),
                    entry.preview.stored_at,
                )
                && let Some(KeyedOutcome::Delivered { at_ms }) = world.delivered(&route, &key)
            {
                open(entry, at_ms, policy);
            }
        }
        let entry = &state.actions[index];
        if entry.code != *code {
            answers.push(state_answer(
                code,
                entry.state,
                true,
                entry.outcome.as_ref().map(|outcome| outcome.code),
            ));
            continue;
        }
        if replayed || entry.state != ActionState::Open {
            answers.push(state_answer(
                code,
                entry.state,
                false,
                entry.outcome.as_ref().map(|outcome| outcome.code),
            ));
            continue;
        }
        let same_chat = entry.preview.route.as_deref() == Some(evidence.route.as_str())
            && entry.preview.peer.as_deref() == Some(evidence.peer.as_str())
            && world.owner(&evidence.route).as_deref() == Some(evidence.peer.as_str());
        if !same_chat {
            answers.push(format!("{code} was sent to another chat; answer it there."));
            continue;
        }
        let Some(sent_ms) = evidence.sent_ms else {
            answers.push(format!(
                "Not approved: this chat did not say when your message was sent, so it cannot \
                 approve {code}."
            ));
            continue;
        };
        if entry
            .preview
            .delivered_ms
            .is_none_or(|delivered| sent_ms < delivered)
        {
            answers.push(format!(
                "Not approved: this message was sent before {code} reached you."
            ));
            continue;
        }
        if now >= entry.expires_at {
            state.actions[index].end(ActionState::Expired, None, now);
            state.counts.expired += 1;
            answers.push(format!("{code} expired."));
            continue;
        }
        if !policy.offers(entry.kind, entry.form) {
            answers.push(format!(
                "Not approved: {} is off in this account's settings now.",
                entry.kind.name()
            ));
            continue;
        }
        if !policy.routes.contains(&evidence.route) {
            answers.push(format!(
                "Not approved: this chat is no longer one {} reports to.",
                policy.account
            ));
            continue;
        }
        let limit = entry.kind.class();
        let used = state
            .reservations
            .iter()
            .filter(|reservation| {
                reservation.limit == limit && now.saturating_sub(reservation.at) < DAY
            })
            .count() as u64;
        if used >= policy.daily_limit(limit) {
            answers.push(format!(
                "Not approved: today's limit for {} is reached; {code} stays open until it \
                 expires.",
                match limit {
                    Limit::Drafts => "drafts",
                    Limit::Sends => "sending",
                    Limit::Moves => "moving mail",
                    Limit::Flags => "marking mail",
                }
            ));
            continue;
        }
        let group = entry.group.clone();
        if let Some(group) = &group
            && state.actions.iter().any(|other| {
                other.group.as_ref() == Some(group)
                    && other.id != entry.id
                    && matches!(
                        other.state,
                        ActionState::Approved | ActionState::Executing | ActionState::Done
                    )
            })
        {
            answers.push(state_answer(code, ActionState::Superseded, false, None));
            continue;
        }
        let id = entry.id.clone();
        let described = describe_entry(entry);
        let entry = &mut state.actions[index];
        entry.state = ActionState::Approved;
        entry.approval = Some(Approval {
            route: evidence.route.clone(),
            peer: evidence.peer.clone(),
            message_id: evidence.message_id.clone(),
            sent_ms,
            recorded_at: now,
            digest: entry.digest.clone(),
        });
        entry.execute_by = Some(now + policy.actions.execute_minutes * 60);
        entry.not_before = now;
        if let Some(group) = &group {
            for other in &mut state.actions {
                if other.group.as_ref() == Some(group) && other.id != id && !other.state.terminal()
                {
                    other.end(ActionState::Superseded, None, now);
                }
            }
        }
        state.reservations.push(Reservation { id, limit, at: now });
        approved_any = true;
        answers.push(format!("Approved {code}: {described}."));
    }
    if approved_any {
        state.approvals.push(Stamped {
            key: evidence.message_id.clone(),
            at: now,
        });
    }
    answers
}

/// Whether `evidence` may deny `entry`: the current owner of the chat the
/// action is bound to, while that chat is still one this account reports
/// to. Before a preview is stored the binding is the chat it was proposed
/// for, and the current owner of that chat may deny it. Once a preview is
/// stored, only the owner it was addressed to may. An action that names no
/// chat is not denied from any chat, including another configured one. A
/// revision supersedes the action it revises, so the same rule decides who
/// may ask for one.
pub(crate) fn may_deny(
    entry: &ActionEntry,
    evidence: &ChatEvidence,
    world: &dyn World,
    policy: &Policy,
) -> bool {
    let Some(route) = entry.preview.route.as_deref() else {
        return false;
    };
    let owner_here = world.owner(&evidence.route).as_deref() == Some(evidence.peer.as_str());
    let configured = policy.routes.iter().any(|known| known == &evidence.route);
    if route != evidence.route || !owner_here || !configured {
        return false;
    }
    entry
        .preview
        .peer
        .as_deref()
        .is_none_or(|peer| peer == evidence.peer)
}

/// Why `evidence` may not deny `code`, in SCV's words. `None` when it may.
fn deny_refusal(
    entry: &ActionEntry,
    evidence: &ChatEvidence,
    world: &dyn World,
    policy: &Policy,
    code: &str,
) -> Option<String> {
    if may_deny(entry, evidence, world, policy) {
        return None;
    }
    let same_chat = entry.preview.route.as_deref() == Some(evidence.route.as_str())
        && entry
            .preview
            .peer
            .as_deref()
            .is_none_or(|peer| peer == evidence.peer)
        && world.owner(&evidence.route).as_deref() == Some(evidence.peer.as_str());
    if same_chat {
        Some(format!(
            "Not denied: this chat is no longer one {} reports to.",
            policy.account
        ))
    } else if entry.preview.route.is_none() {
        Some(format!(
            "{code} has not reached a chat yet; deny it once it has."
        ))
    } else {
        Some(format!("{code} was sent to another chat; answer it there."))
    }
}

/// The deny itself; see [`Ledger::deny`].
fn deny(
    state: &mut MailState,
    codes: Option<&[String]>,
    evidence: &ChatEvidence,
    world: &dyn World,
    policy: &Policy,
    now: u64,
) -> Vec<String> {
    let Some(codes) = codes else {
        let mut denied = 0;
        let mut running = 0;
        for index in 0..state.actions.len() {
            if !may_deny(&state.actions[index], evidence, world, policy) {
                continue;
            }
            match state.actions[index].state {
                ActionState::Executing => running += 1,
                state_now if !state_now.terminal() => {
                    finish(state, index, ActionState::Denied, None, now);
                    denied += 1;
                }
                _ => {}
            }
        }
        let mut answer = match denied {
            0 => "Nothing was waiting.".to_owned(),
            1 => "Denied the one action that was waiting.".to_owned(),
            n => format!("Denied the {n} actions that were waiting."),
        };
        if running > 0 {
            answer.push_str(&format!(
                " {running} being carried out cannot be stopped now."
            ));
        }
        return vec![answer];
    };
    let mut answers = Vec::new();
    for code in codes {
        let Some(index) = state.actions.iter().position(|entry| entry.has_code(code)) else {
            answers.push(
                match state.tombstones.iter().find(|t| t.codes.contains(code)) {
                    Some(tombstone) => state_answer(code, tombstone.state, false, None),
                    None => format!("No mail action has code {code}."),
                },
            );
            continue;
        };
        let entry = &state.actions[index];
        // An earlier generation's code names this action but does not deny it.
        if entry.code != *code {
            answers.push(state_answer(
                code,
                entry.state,
                true,
                entry.outcome.as_ref().map(|outcome| outcome.code),
            ));
            continue;
        }
        if let Some(refusal) = deny_refusal(entry, evidence, world, policy, code) {
            answers.push(refusal);
            continue;
        }
        if entry.state == ActionState::Executing {
            answers.push(format!("{code} is being carried out."));
        } else if entry.state.terminal() {
            answers.push(state_answer(
                code,
                entry.state,
                false,
                entry.outcome.as_ref().map(|outcome| outcome.code),
            ));
        } else {
            let described = describe_entry(entry);
            finish(state, index, ActionState::Denied, None, now);
            answers.push(format!("Denied {code} ({described})."));
        }
    }
    answers
}

/// A new group ID for alternatives of one message.
pub(crate) fn new_group() -> String {
    format!("g{}", random_hex())
}

#[cfg(test)]
mod tests;
