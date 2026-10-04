//! What one proposed action would do, written once and bound by a digest.
//!
//! When an action is proposed, everything that decides its effect (the
//! target message, the folder, and for outgoing mail every recipient,
//! header, and the body) goes into its content file,
//! `state/mail/<account>/actions/<id>.json` (mode `0600`). The file is
//! created whole and synced before the ledger records the action, and is
//! never rewritten; it is deleted once the action is over and its tombstone
//! is written. The digest is SHA-256 over a canonical encoding of those
//! fields, never over JSON bytes, so no serializer setting can change it:
//! the approval records it, and execution recomputes it from the file and
//! refuses to act on any difference.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::PathBuf;

use super::settings::SentCopy;
use super::source::SourceRef;

/// The content file's format.
pub(crate) const CONTENT_VERSION: u32 = 1;
/// The largest content file read.
const MAX_CONTENT_BYTES: u64 = 64 * 1024;
/// The most bytes of an outgoing body, and its most lines.
pub(crate) const MAX_BODY_BYTES: usize = 6 * 1024;
pub(crate) const MAX_BODY_LINES: usize = 150;
/// The most characters of an outgoing subject.
pub(crate) const MAX_SUBJECT_CHARS: usize = 200;
/// The most message IDs a `References` header keeps.
pub(crate) const MAX_REFERENCES: usize = 10;

/// What an action does. The set is closed: there is no delete, no flag
/// but read, and no folder management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionKind {
    /// Save an outgoing message in the Drafts folder.
    Draft,
    /// Send an outgoing message.
    Send,
    /// Move a message out of the inbox into the archive.
    Archive,
    /// Mark a message read.
    MarkRead,
    /// Move a message to Trash.
    Trash,
    /// Move a message to Spam (Junk).
    Spam,
}

impl ActionKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Send => "send",
            Self::Archive => "archive",
            Self::MarkRead => "mark_read",
            Self::Trash => "trash",
            Self::Spam => "spam",
        }
    }

    /// Whether it moves a message to another folder.
    pub(crate) fn moves(self) -> bool {
        matches!(self, Self::Archive | Self::Trash | Self::Spam)
    }

    /// The daily limit it counts against.
    pub(crate) fn class(self) -> Class {
        match self {
            Self::Draft => Class::Drafts,
            Self::Send => Class::Sends,
            Self::Archive | Self::Trash | Self::Spam => Class::Moves,
            Self::MarkRead => Class::Flags,
        }
    }
}

/// What an action's daily limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Class {
    Drafts,
    Sends,
    Moves,
    Flags,
}

/// What an outgoing message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Form {
    /// A reply to a reported message.
    Reply,
    /// A reported message forwarded, its text inline.
    Forward,
    /// New mail to addresses the owner typed.
    Compose,
}

impl Form {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Forward => "forward",
            Self::Compose => "compose",
        }
    }
}

/// A folder by what it is for; the provider's own name is resolved once and
/// bound with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FolderRole {
    Drafts,
    Sent,
    Trash,
    Junk,
    Archive,
}

impl FolderRole {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Drafts => "drafts",
            Self::Sent => "sent",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
        }
    }

    /// How the owner reads it.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Drafts => "Drafts",
            Self::Sent => "Sent",
            Self::Trash => "Trash",
            Self::Junk => "Spam",
            Self::Archive => "the archive",
        }
    }
}

/// A resolved folder: its role and the provider's name for it (for IMAP
/// the modified UTF-7 mailbox name on the wire, for Gmail a label, for
/// Graph a well-known folder name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Folder {
    pub(crate) role: FolderRole,
    pub(crate) name: String,
}

/// The message an action acts on, or answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub(crate) reference: SourceRef,
    /// [`super::parse::identity`]: the message at `reference` must still be
    /// this one when the action runs.
    pub(crate) identity: String,
    /// The same message in any folder ([`super::parse::locator`]), so a
    /// check after an interrupted move can find it where it went.
    pub(crate) locator: String,
    /// Its valid `Message-ID`, angle brackets kept.
    pub(crate) message_id: Option<String>,
}

/// How the owner is shown the message an action acts on: sanitized, and
/// only ever on an untrusted line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Display {
    pub(crate) handle: String,
    pub(crate) from_address: String,
    pub(crate) from_name: String,
    pub(crate) subject: String,
}

/// Who asked for the action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Origin {
    /// Triage suggested it for the message it read.
    Triage { source: String },
    /// The owner asked for it in a mail chat.
    Owner { route: String, message_id: String },
}

/// An address with its display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Mailbox {
    pub(crate) name: String,
    pub(crate) address: String,
}

/// A message SCV would write: every byte on the wire but `Date` follows
/// from these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Outgoing {
    pub(crate) form: Form,
    pub(crate) from: Mailbox,
    /// Validated plain addresses, `to` and `cc` together at most
    /// `mail.actions.max_recipients`.
    pub(crate) to: Vec<String>,
    pub(crate) cc: Vec<String>,
    /// One line, sanitized, at most [`MAX_SUBJECT_CHARS`].
    pub(crate) subject: String,
    /// Plain text, sanitized, at most [`MAX_BODY_BYTES`] and
    /// [`MAX_BODY_LINES`].
    pub(crate) body: String,
    pub(crate) in_reply_to: Option<String>,
    pub(crate) references: Vec<String>,
    /// `<uuid@own domain>`.
    pub(crate) message_id: String,
    /// How a sent message reaches the Sent folder.
    pub(crate) sent_copy: SentCopy,
    /// SCV's notes for the preview, such as where replies go.
    pub(crate) notes: Vec<String>,
}

/// One action's content, as its file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionContent {
    pub(crate) v: u32,
    /// `a` and 32 lowercase hex digits.
    pub(crate) id: String,
    pub(crate) account: String,
    /// The credential fingerprint when it was proposed.
    pub(crate) fingerprint: String,
    pub(crate) kind: ActionKind,
    /// Actions that are alternatives to each other (saving a reply or
    /// sending it): approving one supersedes the rest.
    pub(crate) group: Option<String>,
    pub(crate) origin: Origin,
    pub(crate) source: Option<Source>,
    pub(crate) display: Option<Display>,
    /// Where it writes or moves to.
    pub(crate) folder: Option<Folder>,
    pub(crate) message: Option<Outgoing>,
    pub(crate) created_at: u64,
    /// No approval counts after this.
    pub(crate) hard_expiry: u64,
    /// [`ActionContent::compute_digest`], lowercase hex.
    pub(crate) digest: String,
}

/// A value in the canonical encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Canon {
    Null,
    Int(u64),
    Str(String),
    List(Vec<Canon>),
    Object(Vec<(&'static str, Canon)>),
}

impl Canon {
    fn text(text: &str) -> Self {
        Self::Str(text.to_owned())
    }

    fn option<T>(value: Option<&T>, encode: impl Fn(&T) -> Self) -> Self {
        value.map_or(Self::Null, encode)
    }

    fn strings(values: &[String]) -> Self {
        Self::List(values.iter().map(|value| Self::text(value)).collect())
    }

    /// `0x00` null, `0x02` u64 big-endian, `0x03` string with
    /// its byte length, `0x04` list with its count, `0x05` object with its
    /// field count, each field its name, a NUL, and its value.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Null => out.push(0),
            Self::Int(value) => {
                out.push(2);
                out.extend(value.to_be_bytes());
            }
            Self::Str(text) => {
                out.push(3);
                out.extend((text.len() as u64).to_be_bytes());
                out.extend(text.as_bytes());
            }
            Self::List(items) => {
                out.push(4);
                out.extend((items.len() as u64).to_be_bytes());
                for item in items {
                    item.encode(out);
                }
            }
            Self::Object(fields) => {
                out.push(5);
                out.extend((fields.len() as u64).to_be_bytes());
                for (name, value) in fields {
                    encode_field(name, value, out);
                }
            }
        }
    }
}

fn encode_field(name: &str, value: &Canon, out: &mut Vec<u8>) {
    out.extend(name.as_bytes());
    out.push(0);
    value.encode(out);
}

/// The domain the digest is computed in.
const DIGEST_DOMAIN: &[u8] = b"scv-mail-action\0v1\0";

fn source_ref(reference: &SourceRef) -> Canon {
    match reference {
        SourceRef::Imap {
            mailbox,
            uidvalidity,
            uid,
        } => Canon::Object(vec![
            ("provider", Canon::text("imap")),
            ("mailbox", Canon::text(mailbox)),
            ("uidvalidity", Canon::Int(u64::from(*uidvalidity))),
            ("uid", Canon::Int(u64::from(*uid))),
        ]),
        SourceRef::Gmail { id } => Canon::Object(vec![
            ("provider", Canon::text("gmail")),
            ("id", Canon::text(id)),
        ]),
        SourceRef::Graph { id } => Canon::Object(vec![
            ("provider", Canon::text("graph")),
            ("id", Canon::text(id)),
        ]),
    }
}

fn sent_copy(copy: SentCopy) -> Canon {
    Canon::text(match copy {
        SentCopy::Provider => "provider",
        SentCopy::Append => "append",
    })
}

impl ActionContent {
    /// Every field but `digest`, in their fixed order.
    pub(crate) fn canonical(&self) -> Vec<(&'static str, Canon)> {
        vec![
            ("v", Canon::Int(u64::from(self.v))),
            ("id", Canon::text(&self.id)),
            ("account", Canon::text(&self.account)),
            ("fingerprint", Canon::text(&self.fingerprint)),
            ("kind", Canon::text(self.kind.name())),
            (
                "group",
                Canon::option(self.group.as_ref(), |g| Canon::text(g)),
            ),
            (
                "origin",
                match &self.origin {
                    Origin::Triage { source } => Canon::Object(vec![
                        ("type", Canon::text("triage")),
                        ("source", Canon::text(source)),
                    ]),
                    Origin::Owner { route, message_id } => Canon::Object(vec![
                        ("type", Canon::text("owner")),
                        ("route", Canon::text(route)),
                        ("message_id", Canon::text(message_id)),
                    ]),
                },
            ),
            (
                "source",
                Canon::option(self.source.as_ref(), |source| {
                    Canon::Object(vec![
                        ("reference", source_ref(&source.reference)),
                        ("identity", Canon::text(&source.identity)),
                        ("locator", Canon::text(&source.locator)),
                        (
                            "message_id",
                            Canon::option(source.message_id.as_ref(), |id| Canon::text(id)),
                        ),
                    ])
                }),
            ),
            (
                "display",
                Canon::option(self.display.as_ref(), |display| {
                    Canon::Object(vec![
                        ("handle", Canon::text(&display.handle)),
                        ("from_address", Canon::text(&display.from_address)),
                        ("from_name", Canon::text(&display.from_name)),
                        ("subject", Canon::text(&display.subject)),
                    ])
                }),
            ),
            (
                "folder",
                Canon::option(self.folder.as_ref(), |folder| {
                    Canon::Object(vec![
                        ("role", Canon::text(folder.role.name())),
                        ("name", Canon::text(&folder.name)),
                    ])
                }),
            ),
            (
                "message",
                Canon::option(self.message.as_ref(), |message| {
                    Canon::Object(vec![
                        ("form", Canon::text(message.form.name())),
                        (
                            "from",
                            Canon::Object(vec![
                                ("name", Canon::text(&message.from.name)),
                                ("address", Canon::text(&message.from.address)),
                            ]),
                        ),
                        ("to", Canon::strings(&message.to)),
                        ("cc", Canon::strings(&message.cc)),
                        ("subject", Canon::text(&message.subject)),
                        ("body", Canon::text(&message.body)),
                        (
                            "in_reply_to",
                            Canon::option(message.in_reply_to.as_ref(), |id| Canon::text(id)),
                        ),
                        ("references", Canon::strings(&message.references)),
                        ("message_id", Canon::text(&message.message_id)),
                        ("sent_copy", sent_copy(message.sent_copy)),
                        ("notes", Canon::strings(&message.notes)),
                    ])
                }),
            ),
            ("created_at", Canon::Int(self.created_at)),
            ("hard_expiry", Canon::Int(self.hard_expiry)),
        ]
    }

    /// SHA-256, as lowercase hex, over the digest domain and the canonical
    /// encoding of every field but `digest`.
    pub(crate) fn compute_digest(&self) -> String {
        use sha2::Digest as _;
        let mut bytes = DIGEST_DOMAIN.to_vec();
        for (name, value) in self.canonical() {
            encode_field(name, &value, &mut bytes);
        }
        sha2::Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Fill in the digest; done once, before the file is written.
    pub(crate) fn seal_digest(mut self) -> Self {
        self.digest = self.compute_digest();
        self
    }

    /// The first eight hex digits of the digest, shown to the owner for
    /// audit.
    pub(crate) fn short_digest(&self) -> &str {
        &self.digest[..self.digest.len().min(8)]
    }

    /// Every address the message goes to.
    pub(crate) fn recipients(&self) -> Vec<&str> {
        self.message
            .iter()
            .flat_map(|message| message.to.iter().chain(&message.cc))
            .map(String::as_str)
            .collect()
    }
}

/// A new action's ID: `a` and 128 random bits in hex.
pub(crate) fn new_id() -> String {
    format!("a{}", uuid::Uuid::new_v4().simple())
}

/// Whether `id` could be an action's ID, so it is safe as a file name.
pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 33
        && id.starts_with('a')
        && id[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The directory of one account's content files.
#[derive(Debug, Clone)]
pub(crate) struct ContentStore {
    directory: PathBuf,
}

/// A content file the janitor may look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) id: String,
    pub(crate) bytes: u64,
    /// Seconds since it was last modified.
    pub(crate) age: u64,
}

impl ContentStore {
    /// `state/mail/<account>/actions`, made private by the caller.
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            bail!("not a mail action ID");
        }
        Ok(self.directory.join(format!("{id}.json")))
    }

    /// Write `content` as a new file and sync it and its directory. An
    /// existing file is never replaced.
    pub(crate) fn write_new(&self, content: &ActionContent) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        if content.digest != content.compute_digest() {
            bail!("a mail action's digest does not match its content");
        }
        let path = self.path(&content.id)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .context("could not create a mail action's content file")?;
        file.write_all(serde_json::to_string(content)?.as_bytes())?;
        file.sync_all()?;
        scv_client::fs::sync_directory(&self.directory)?;
        Ok(())
    }

    /// The content of action `id`; `None` when it has no file.
    pub(crate) fn read(&self, id: &str) -> Result<Option<ActionContent>> {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = self.path(id)?;
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_CONTENT_BYTES {
            bail!("a mail action's content file is not a small regular file");
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let content: ActionContent =
            serde_json::from_str(&text).context("a mail action's content file is unreadable")?;
        if content.id != id {
            bail!("a mail action's content file names another action");
        }
        Ok(Some(content))
    }

    /// Remove action `id`'s file, if it has one.
    pub(crate) fn remove(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.path(id)?) {
            Ok(()) => Ok(scv_client::fs::sync_directory(&self.directory)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Every content file, by ID, with its size and age; other entries are
    /// left out.
    pub(crate) fn list(&self) -> Vec<Listed> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let now = std::time::SystemTime::now();
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let id = name.to_str()?.strip_suffix(".json")?;
                if !valid_id(id) {
                    return None;
                }
                let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
                metadata.is_file().then(|| Listed {
                    id: id.to_owned(),
                    bytes: metadata.len(),
                    age: metadata
                        .modified()
                        .ok()
                        .and_then(|at| now.duration_since(at).ok())
                        .map_or(0, |age| age.as_secs()),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
