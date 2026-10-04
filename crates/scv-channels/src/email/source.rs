//! What the mail pipeline needs from a mailbox, independent of the provider.
//!
//! The pipeline sees a mailbox only through [`MailSource`]: the changes
//! since a [`Cursor`], each message's metadata, and one bounded text part.
//! Every record it keeps names messages by [`SourceRef`], so another adapter
//! (the Gmail API, Microsoft Graph) adds a variant instead of a migration.
//! This release has one adapter, IMAP with a password or authorization code,
//! and it is read-only: no source here can change a mailbox.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub(crate) use super::parse::TransferEncoding;

/// Which adapter a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    /// IMAP over implicit TLS.
    Imap,
    /// The Gmail API.
    Gmail,
    /// Microsoft Graph.
    Graph,
}

/// A message's identity at its provider, as records bind it. Never shown to
/// a model and never written to logs beyond its numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SourceRef {
    /// A UID is stable within one `UIDVALIDITY` of one mailbox.
    Imap {
        mailbox: String,
        uidvalidity: u32,
        uid: u32,
    },
    /// A Gmail message ID, stable for the message's life.
    Gmail { id: String },
    /// A Graph immutable ID, stable while the message stays in the
    /// mailbox, moves included.
    Graph { id: String },
}

impl SourceRef {
    /// A short, text-free label for logs and notice keys, such as
    /// `imap:1700000000:42`. An API's ID is shown as a digest of it.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Imap {
                uidvalidity, uid, ..
            } => format!("imap:{uidvalidity}:{uid}"),
            Self::Gmail { id } => format!("gmail:{}", &super::parse::digest(id.as_bytes())[..16]),
            Self::Graph { id } => format!("graph:{}", &super::parse::digest(id.as_bytes())[..16]),
        }
    }

    /// Whether an ID an API returned is safe to use in a request path.
    /// `.` and `..` are rejected, as is any ID that contains `..`.
    pub(crate) fn valid_api_id(id: &str) -> bool {
        !id.is_empty()
            && id != "."
            && !id.contains("..")
            && id.len() <= 512
            && id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'=' | b'.')
            })
    }
}

/// Where the next [`MailSource::changes`] resumes. Opaque to everything but
/// the adapter that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cursor {
    pub(crate) provider: ProviderKind,
    pub(crate) value: String,
}

/// What [`MailSource::changes`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Changes {
    /// Messages that arrived after the cursor, oldest first, at most the
    /// limit asked for; `next` resumes after the last of them.
    New { refs: Vec<SourceRef>, next: Cursor },
    /// The cursor no longer applies (for IMAP, `UIDVALIDITY` changed). The
    /// source listed what arrived within the resync window instead:
    /// `recent`, oldest first and at most the limit; `beyond` counts the
    /// rest of the window. `next` resumes after the newest message now.
    Reset {
        recent: Vec<SourceRef>,
        beyond: usize,
        next: Cursor,
    },
}

/// A mailbox address from a message's headers, decoded but not yet
/// validated. `name` is the display name, possibly empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Address {
    pub(crate) name: String,
    /// `local@domain`, as the header gave it; empty when it had none.
    pub(crate) address: String,
}

/// Headers that mark a message as bulk or automated, as sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Signals {
    /// `List-Id`, decoded.
    pub(crate) list_id: Option<String>,
    /// A `List-Unsubscribe` header is present.
    pub(crate) list_unsubscribe: bool,
    /// `Precedence`, lowercased.
    pub(crate) precedence: Option<String>,
    /// `Auto-Submitted`, lowercased.
    pub(crate) auto_submitted: Option<String>,
    /// `Return-Path: <>`, which bounces and auto-replies carry.
    pub(crate) null_return_path: bool,
}

impl Signals {
    /// Whether the message is bulk or automated: a mailing list, a
    /// `Precedence` of `bulk`, `list`, or `junk`, or `Auto-Submitted` other
    /// than `no`.
    pub(crate) fn bulk(&self) -> bool {
        self.list_id.is_some()
            || self.list_unsubscribe
            || self
                .precedence
                .as_deref()
                .is_some_and(|value| matches!(value, "bulk" | "list" | "junk"))
            || self
                .auto_submitted
                .as_deref()
                .is_some_and(|value| value != "no")
    }
}

/// The text part the pipeline may read, as the source describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartRef {
    /// The adapter's name for the part: for IMAP, the body section such
    /// as `1` or `1.2`.
    pub(crate) id: String,
    /// `text/plain` or `text/html`, lowercased.
    pub(crate) mime: String,
    /// The declared charset, lowercased; `None` when absent.
    pub(crate) charset: Option<String>,
    pub(crate) encoding: TransferEncoding,
    /// The encoded size the source announced.
    pub(crate) size: u64,
}

impl PartRef {
    pub(crate) fn is_html(&self) -> bool {
        self.mime == "text/html"
    }
}

/// An attachment as the source lists it; its content is never fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentInfo {
    /// The decoded file name; empty when it has none.
    pub(crate) name: String,
    /// The declared type, lowercased, such as `application/pdf`.
    pub(crate) mime: String,
    /// The encoded size the source announced.
    pub(crate) size: u64,
}

/// The most bytes a subject keeps.
pub(crate) const MAX_SUBJECT_BYTES: usize = 1024;
/// The most bytes a display name, an attachment's name, or a list ID keeps.
pub(crate) const MAX_NAME_BYTES: usize = 256;
/// The most bytes an address keeps: RFC 5321's longest path.
pub(crate) const MAX_ADDRESS_BYTES: usize = 320;
/// The most bytes a MIME type, a charset, or another short label keeps.
pub(crate) const MAX_LABEL_BYTES: usize = 128;
/// The most addresses kept of each of To and Cc.
pub(crate) const MAX_RECIPIENTS: usize = 64;
/// The most attachments described; the rest are not.
pub(crate) const MAX_ATTACHMENTS: usize = 16;
/// The most message IDs of `References` kept.
pub(crate) const MAX_REFERENCES: usize = 10;
/// What ends a field that was cut to its bound.
pub(crate) const CUT: &str = "…";

/// Cut `text` to at most `max` bytes, at a character boundary, ending it in
/// [`CUT`] when anything was dropped.
pub(crate) fn cut(text: &mut String, max: usize) {
    if text.len() > max {
        let keep = scv_client::text::utf8_prefix(text, max.saturating_sub(CUT.len())).len();
        text.truncate(keep);
        text.push_str(CUT);
    }
}

/// One message's metadata: everything the rules and a header-only report
/// need, fetched without reading its body. Every text field is decoded but
/// not sanitized: it is mail-derived and untrusted. An adapter returns it
/// [`Meta::bounded`], so no field outgrows its bound however large the
/// message's headers are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) source: SourceRef,
    /// [`super::parse::identity`]: the same for one stored message however
    /// the provider renumbers its mailbox, and different for another
    /// message even when it reuses a `Message-ID`.
    pub(crate) identity: String,
    /// When the mailbox received it, in Unix seconds.
    pub(crate) received_at: u64,
    /// The whole message's size in bytes.
    pub(crate) size: u64,
    pub(crate) from: Option<Address>,
    pub(crate) reply_to: Option<Address>,
    pub(crate) to: Vec<Address>,
    pub(crate) cc: Vec<Address>,
    pub(crate) subject: String,
    /// The valid `Message-ID`, angle brackets kept.
    pub(crate) message_id: Option<String>,
    /// [`super::parse::locator`]: the same for this message in any folder.
    pub(crate) locator: String,
    /// The valid message IDs of its `References` header, oldest first, at
    /// most [`MAX_REFERENCES`].
    pub(crate) references: Vec<String>,
    /// Its `Date` header as sent, for a forward; bounded.
    pub(crate) date: Option<String>,
    pub(crate) signals: Signals,
    /// The provider's own category, such as Gmail's `CATEGORY_PROMOTIONS`;
    /// IMAP has none.
    pub(crate) category: Option<String>,
    /// The body part to read: the first `text/plain` that is not an
    /// attachment, or else the first such `text/html`.
    pub(crate) text: Option<PartRef>,
    pub(crate) attachments: Vec<AttachmentInfo>,
}

impl Meta {
    /// This metadata with every text field cut to its bound, at most
    /// [`MAX_RECIPIENTS`] of each recipient list, and at most
    /// [`MAX_ATTACHMENTS`] attachments.
    pub(crate) fn bounded(mut self) -> Self {
        fn address(address: &mut Address) {
            cut(&mut address.name, MAX_NAME_BYTES);
            cut(&mut address.address, MAX_ADDRESS_BYTES);
        }
        for each in self.from.iter_mut().chain(self.reply_to.iter_mut()) {
            address(each);
        }
        for list in [&mut self.to, &mut self.cc] {
            list.truncate(MAX_RECIPIENTS);
            list.iter_mut().for_each(address);
        }
        cut(&mut self.subject, MAX_SUBJECT_BYTES);
        let excess = self.references.len().saturating_sub(MAX_REFERENCES);
        self.references.drain(..excess);
        if let Some(date) = &mut self.date {
            cut(date, MAX_LABEL_BYTES);
        }
        let signals = &mut self.signals;
        for (field, max) in [
            (&mut signals.list_id, MAX_NAME_BYTES),
            (&mut signals.precedence, MAX_LABEL_BYTES),
            (&mut signals.auto_submitted, MAX_LABEL_BYTES),
            (&mut self.category, MAX_LABEL_BYTES),
        ] {
            if let Some(field) = field {
                cut(field, max);
            }
        }
        if let Some(text) = &mut self.text {
            cut(&mut text.mime, MAX_LABEL_BYTES);
            if let Some(charset) = &mut text.charset {
                cut(charset, MAX_LABEL_BYTES);
            }
        }
        self.attachments.truncate(MAX_ATTACHMENTS);
        for attachment in &mut self.attachments {
            cut(&mut attachment.name, MAX_NAME_BYTES);
            cut(&mut attachment.mime, MAX_LABEL_BYTES);
        }
        self
    }
}

/// A text part's content, decoded to Unicode but not cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartText {
    pub(crate) text: String,
    pub(crate) html: bool,
    /// The part was longer than the bytes fetched.
    pub(crate) truncated: bool,
}

/// What the source can do, probed at connection: the provider's own
/// extensions, and the actions it can carry out at all. An action the
/// provider cannot do is never proposed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    /// IMAP `IDLE`.
    pub(crate) push: bool,
    /// IMAP `MOVE`.
    pub(crate) move_: bool,
    /// IMAP `UIDPLUS`.
    pub(crate) uidplus: bool,
    /// IMAP `SPECIAL-USE`.
    pub(crate) special_use: bool,
    /// IMAP `ID`.
    pub(crate) id: bool,
    /// A message can be found by its `Message-ID`, so an interrupted
    /// action can be checked.
    pub(crate) find_by_message_id: bool,
    /// The provider files sent mail in Sent by itself.
    pub(crate) sent_autofile: bool,
    /// Moving a message: with IMAP, `MOVE`, or `UIDPLUS` for copy and
    /// expunge; always with an API.
    pub(crate) can_move: bool,
}

/// Where the provider keeps its special folders, by their provider names
/// (for IMAP the modified UTF-7 names); `None` when none was found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Folders {
    pub(crate) drafts: Option<String>,
    pub(crate) sent: Option<String>,
    pub(crate) trash: Option<String>,
    pub(crate) junk: Option<String>,
    pub(crate) archive: Option<String>,
}

impl Folders {
    pub(crate) fn get(&self, role: super::content::FolderRole) -> Option<&str> {
        use super::content::FolderRole;
        match role {
            FolderRole::Drafts => self.drafts.as_deref(),
            FolderRole::Sent => self.sent.as_deref(),
            FolderRole::Trash => self.trash.as_deref(),
            FolderRole::Junk => self.junk.as_deref(),
            FolderRole::Archive => self.archive.as_deref(),
        }
    }
}

/// The owner's folder names for providers that do not mark their folders,
/// as UTF-8; empty means none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FolderNames {
    pub(crate) drafts: String,
    pub(crate) sent: String,
    pub(crate) trash: String,
    pub(crate) junk: String,
    pub(crate) archive: String,
}

/// A mailbox the pipeline reads, through a read-only connection or
/// credential. Every method is bounded in time and in bytes; mail content
/// stays in memory.
#[async_trait]
pub(crate) trait MailSource: Send {
    /// Messages that arrived after `cursor`, at most `limit`. With no
    /// cursor (the first run), nothing is listed and `next` is the current
    /// position: there is no backfill. After a reset, messages that arrived
    /// within `window_seconds` of now are listed.
    async fn changes(
        &mut self,
        cursor: Option<&Cursor>,
        limit: usize,
        window_seconds: u64,
    ) -> Result<Changes>;

    /// Metadata for each reference, in order; a message no longer there is
    /// left out.
    async fn metadata(&mut self, refs: &[SourceRef]) -> Result<Vec<Meta>>;

    /// At most `max_bytes` of a text part's encoded content, decoded. `None`
    /// when the message is gone.
    async fn text(
        &mut self,
        source: &SourceRef,
        part: &PartRef,
        max_bytes: usize,
    ) -> Result<Option<PartText>>;

    /// What the connected source offers.
    fn caps(&self) -> Caps;

    /// Where the special folders are: marked by the provider, or else named
    /// by the owner and present. Never guessed, and never created.
    async fn folders(&mut self, names: &FolderNames) -> Result<Folders>;
}

#[async_trait]
impl<T: MailSource + ?Sized> MailSource for Box<T> {
    async fn changes(
        &mut self,
        cursor: Option<&Cursor>,
        limit: usize,
        window_seconds: u64,
    ) -> Result<Changes> {
        (**self).changes(cursor, limit, window_seconds).await
    }

    async fn metadata(&mut self, refs: &[SourceRef]) -> Result<Vec<Meta>> {
        (**self).metadata(refs).await
    }

    async fn text(
        &mut self,
        source: &SourceRef,
        part: &PartRef,
        max_bytes: usize,
    ) -> Result<Option<PartText>> {
        (**self).text(source, part, max_bytes).await
    }

    fn caps(&self) -> Caps {
        (**self).caps()
    }

    async fn folders(&mut self, names: &FolderNames) -> Result<Folders> {
        (**self).folders(names).await
    }
}

#[cfg(test)]
mod tests;
