//! The executor's IMAP: writing one approved action, and checking what an
//! interrupted one did.
//!
//! A write connection is opened for one [`Approved`] action, in the
//! guard's write mode with exactly that action's targets: its message's
//! mailbox and UID, its folder, and its flags. Before a move or a mark it
//! opens the mailbox, checks that `UIDVALIDITY` and the message's identity
//! are still what was approved, and that the destination still exists; it
//! never creates a folder, never sends `CLOSE` (which would expunge), and
//! expunges only the one UID it moved. A draft or a Sent copy is looked for
//! by its `Message-ID` first, so a retry never leaves a second one.
//! Checks ([`ImapEffects::probe`]) use a read-only connection.

use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use super::client::{Client, Done, Examined, Item, Search, examined, quote};
use super::guard::{AppendFlags, GuardViolation, Mode, Part, StoreFlag, Targets};
use super::wire::Status;
use super::{ImapConfig, names, open_session, utf7};
use crate::email::content::{ActionContent, ActionKind, Source};
use crate::email::effects::{MailEffects, unreachable};
use crate::email::ledger::Approved;
use crate::email::ledger::actions::{Execution, OutcomeCode, Probe, Resume};
use crate::email::smtp::{self, SmtpConfig};
use crate::email::source::SourceRef;

/// Items fetched to tell a message's identity and flags.
const IDENTITY_ITEMS: [Item<'static>; 6] = [
    Item::Uid,
    Item::Flags,
    Item::InternalDate,
    Item::Size,
    Item::Envelope,
    Item::HeaderFields(&["MESSAGE-ID"]),
];

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Client<S> {
    /// `SELECT` of the approved message's mailbox, by its wire name.
    async fn select(&mut self, wire: &str) -> Result<Done> {
        self.run(&format!("SELECT {}", quote(wire)), Vec::new())
            .await
    }

    /// `APPEND` of `message` to the approved folder with `flags`.
    async fn append(&mut self, folder: &str, flags: AppendFlags, message: &[u8]) -> Result<Done> {
        self.run(
            &format!("APPEND {} {} ", quote(folder), flags.wire()),
            vec![Part::Literal(message.to_vec())],
        )
        .await
    }

    /// `UID MOVE` of the approved message to the approved folder.
    async fn uid_move(&mut self, uid: u32, folder: &str) -> Result<Done> {
        self.run(&format!("UID MOVE {uid} {}", quote(folder)), Vec::new())
            .await
    }

    /// `UID COPY`, the first step of a move without `MOVE`.
    async fn uid_copy(&mut self, uid: u32, folder: &str) -> Result<Done> {
        self.run(&format!("UID COPY {uid} {}", quote(folder)), Vec::new())
            .await
    }

    /// `UID STORE uid +FLAGS.SILENT (flag)`.
    async fn uid_store(&mut self, uid: u32, flag: StoreFlag) -> Result<Done> {
        self.run(
            &format!("UID STORE {uid} +FLAGS.SILENT ({})", flag.wire()),
            Vec::new(),
        )
        .await
    }

    /// `UID EXPUNGE` of the one approved message.
    async fn uid_expunge(&mut self, uid: u32) -> Result<Done> {
        self.run(&format!("UID EXPUNGE {uid}"), Vec::new()).await
    }
}

/// Opens connections to one IMAP server.
#[async_trait]
pub(crate) trait Connect: Send + Sync {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send;
    async fn connect(&self) -> Result<Self::Stream>;
}

/// The real server over implicit TLS.
pub(crate) struct TlsConnect {
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[async_trait]
impl Connect for TlsConnect {
    type Stream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

    async fn connect(&self) -> Result<Self::Stream> {
        super::tls::connect(&self.host, self.port).await
    }
}

/// An IMAP mailbox's effects, with an SMTP server for sending.
pub(crate) struct ImapEffects<C> {
    pub(crate) connect: C,
    pub(crate) config: ImapConfig,
    pub(crate) smtp: Option<SmtpConfig>,
}

/// What a write command did, strictly: a tagged `OK` happened; `NO` did
/// not (and is final when the folder is missing); a command that may have
/// reached the server without an answer is ambiguous.
fn classify(result: Result<Done>) -> Execution {
    match result {
        Ok(done) if done.status == Status::Ok => Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        },
        Ok(done) => {
            let missing = done
                .code
                .as_ref()
                .is_some_and(|code| matches!(code.name.as_str(), "TRYCREATE" | "NONEXISTENT"));
            Execution::NotApplied {
                retry: done.status == Status::No && !missing,
                code: if missing {
                    OutcomeCode::Unsupported
                } else {
                    OutcomeCode::Refused
                },
            }
        }
        Err(error) if error.downcast_ref::<GuardViolation>().is_some() => Execution::NotApplied {
            retry: false,
            code: OutcomeCode::Internal,
        },
        Err(_) => Execution::Ambiguous,
    }
}

fn not_applied(retry: bool, code: OutcomeCode) -> Execution {
    Execution::NotApplied { retry, code }
}

/// The IMAP parts of the message an action acts on.
fn imap_source(source: &Source) -> Option<(&str, u32, u32)> {
    match &source.reference {
        SourceRef::Imap {
            mailbox,
            uidvalidity,
            uid,
        } => Some((mailbox, *uidvalidity, *uid)),
        _ => None,
    }
}

/// What a fetch of one UID found: its identity and flags.
struct Found {
    identity: String,
    flags: Vec<String>,
}

impl Found {
    fn flagged(&self, flag: &str) -> bool {
        self.flags
            .iter()
            .any(|known| known.eq_ignore_ascii_case(flag))
    }
}

async fn fetch_one<S: AsyncRead + AsyncWrite + Unpin + Send>(
    client: &mut Client<S>,
    mailbox: &str,
    uid: u32,
) -> Result<Option<Found>> {
    let fetched = client.uid_fetch(&[uid], &IDENTITY_ITEMS).await?;
    Ok(fetched
        .iter()
        .find(|message| message.uid == Some(uid))
        .map(|message| Found {
            identity: names(mailbox, message).0,
            flags: message.flags.clone().unwrap_or_default(),
        }))
}

/// Whether the folder `wire` holds a message that is `source`, found by its
/// `Message-ID` and known by its locator; `None` when that cannot be told.
async fn holds<S: AsyncRead + AsyncWrite + Unpin + Send>(
    client: &mut Client<S>,
    wire: &str,
    source: &Source,
) -> Result<Option<bool>> {
    let Some(message_id) = &source.message_id else {
        return Ok(None);
    };
    let opened = client.examine_wire(wire).await?;
    if opened.uidvalidity.is_none() {
        return Ok(None);
    }
    let uids = client
        .uid_search(Search::MessageId(message_id.clone()))
        .await?;
    if uids.is_empty() {
        return Ok(Some(false));
    }
    let mailbox = utf7::decode(wire).unwrap_or_else(|_| wire.to_owned());
    for fetched in client.uid_fetch(&uids, &IDENTITY_ITEMS).await? {
        if names(&mailbox, &fetched).1 == source.locator {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

/// Whether `folder` holds a message with `message_id`.
async fn has_message_id<S: AsyncRead + AsyncWrite + Unpin + Send>(
    client: &mut Client<S>,
    folder: &str,
    message_id: &str,
) -> Result<bool> {
    client.examine_wire(folder).await?;
    Ok(!client
        .uid_search(Search::MessageId(message_id.to_owned()))
        .await?
        .is_empty())
}

impl<C: Connect> ImapEffects<C> {
    async fn open(&self, mode: Mode) -> Result<Client<C::Stream>> {
        let stream = self.connect.connect().await?;
        let mut client = open_session(stream, &self.config).await?;
        client.set_mode(mode);
        Ok(client)
    }

    /// Append `message` to the action's folder with `flags`, unless a
    /// message with its `Message-ID` is there already.
    async fn append_once(
        &self,
        content: &ActionContent,
        flags: AppendFlags,
        message: &[u8],
    ) -> Execution {
        let (Some(folder), Some(outgoing)) = (&content.folder, &content.message) else {
            return not_applied(false, OutcomeCode::Unsupported);
        };
        let targets = Targets {
            folder: Some(folder.name.clone()),
            append: Some(flags),
            ..Targets::default()
        };
        let mut client = match self.open(Mode::Write(targets)).await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(error = %error, "could not open the mailbox to write");
                return unreachable();
            }
        };
        let execution = match has_message_id(&mut client, &folder.name, &outgoing.message_id).await
        {
            Ok(true) => Execution::Applied {
                code: OutcomeCode::AlreadyDone,
                sent_copy: None,
            },
            Ok(false) => classify(client.append(&folder.name, flags, message).await),
            Err(_) if client.broken => unreachable(),
            // The folder cannot be opened: it is gone.
            Err(_) => not_applied(false, OutcomeCode::Unsupported),
        };
        client.logout().await;
        execution
    }

    async fn change_on(
        &self,
        client: &mut Client<C::Stream>,
        approved: &Approved,
        source: &Source,
    ) -> Execution {
        let content = approved.content();
        let Some((mailbox, uidvalidity, uid)) = imap_source(source) else {
            return not_applied(false, OutcomeCode::Mismatch);
        };
        let wire = utf7::encode(mailbox);
        let selected = match client.select(&wire).await {
            Ok(done) if done.status == Status::Ok => examined(&done),
            Ok(_) => return not_applied(false, OutcomeCode::Gone),
            Err(_) => return unreachable(),
        };
        if selected.uidvalidity != Some(uidvalidity) {
            return not_applied(false, OutcomeCode::Mismatch);
        }
        let found = match fetch_one(client, mailbox, uid).await {
            Ok(Some(found)) => found,
            Ok(None) => return not_applied(false, OutcomeCode::Gone),
            Err(_) => return unreachable(),
        };
        if found.identity != source.identity {
            return not_applied(false, OutcomeCode::Mismatch);
        }
        if content.kind == ActionKind::MarkRead {
            if found.flagged("\\Seen") {
                return Execution::Applied {
                    code: OutcomeCode::AlreadyDone,
                    sent_copy: None,
                };
            }
            return classify(client.uid_store(uid, StoreFlag::Seen).await);
        }
        let Some(folder) = &content.folder else {
            return not_applied(false, OutcomeCode::Unsupported);
        };
        match client.list_folders().await {
            Ok(listed) if listed.iter().any(|known| known.name == folder.name) => {}
            Ok(_) => return not_applied(false, OutcomeCode::Unsupported),
            Err(_) => return unreachable(),
        }
        match approved.resume() {
            Some(Resume::AfterCopy) => {
                match classify(client.uid_store(uid, StoreFlag::Deleted).await) {
                    Execution::Applied { .. } => {}
                    _ => return Execution::Ambiguous,
                }
                return settle(classify(client.uid_expunge(uid).await));
            }
            Some(Resume::Expunge) => return settle(classify(client.uid_expunge(uid).await)),
            None => {}
        }
        if client.has("MOVE") {
            return classify(client.uid_move(uid, &folder.name).await);
        }
        if !client.has("UIDPLUS") {
            return not_applied(false, OutcomeCode::Unsupported);
        }
        match classify(client.uid_copy(uid, &folder.name).await) {
            Execution::Applied { .. } => {}
            other => return other,
        }
        // The copy is in place: whatever fails from here, a check finds the
        // move half done and takes it up.
        match classify(client.uid_store(uid, StoreFlag::Deleted).await) {
            Execution::Applied { .. } => {}
            _ => return Execution::Ambiguous,
        }
        settle(classify(client.uid_expunge(uid).await))
    }
}

/// A later step of a move that did not complete leaves it half done.
fn settle(execution: Execution) -> Execution {
    match execution {
        Execution::Applied { .. } => execution,
        _ => Execution::Ambiguous,
    }
}

#[async_trait]
impl<C: Connect> MailEffects for ImapEffects<C> {
    async fn save_draft(&mut self, approved: &Approved, message: &[u8]) -> Execution {
        self.append_once(approved.content(), AppendFlags::Draft, message)
            .await
    }

    async fn change(&mut self, approved: &Approved) -> Execution {
        let content = approved.content();
        let Some(source) = &content.source else {
            return not_applied(false, OutcomeCode::Mismatch);
        };
        let Some((mailbox, _, uid)) = imap_source(source) else {
            return not_applied(false, OutcomeCode::Mismatch);
        };
        let targets = Targets {
            source: Some(utf7::encode(mailbox)),
            uid: Some(uid),
            folder: content.folder.as_ref().map(|folder| folder.name.clone()),
            store: Some(if content.kind == ActionKind::MarkRead {
                StoreFlag::Seen
            } else {
                StoreFlag::Deleted
            }),
            moves: content.kind.moves(),
            copies: content.kind.moves(),
            append: None,
        };
        let mut client = match self.open(Mode::Write(targets)).await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(error = %error, "could not open the mailbox to write");
                return unreachable();
            }
        };
        let execution = self.change_on(&mut client, approved, source).await;
        client.logout().await;
        execution
    }

    async fn send(&mut self, approved: &Approved, message: &[u8]) -> Execution {
        let content = approved.content();
        let (Some(config), Some(outgoing)) = (&self.smtp, &content.message) else {
            return not_applied(false, OutcomeCode::Unsupported);
        };
        let recipients: Vec<String> = content
            .recipients()
            .into_iter()
            .map(str::to_owned)
            .collect();
        smtp::send(config, &outgoing.from.address, &recipients, message).await
    }

    async fn copy_sent(&mut self, approved: &Approved, message: &[u8]) -> bool {
        matches!(
            self.append_once(approved.content(), AppendFlags::Seen, message)
                .await,
            Execution::Applied { .. }
        )
    }

    async fn probe(&mut self, content: &ActionContent) -> Probe {
        let Ok(mut client) = self.open(Mode::ReadOnly).await else {
            return Probe::Unreachable;
        };
        let probe = probe_on(&mut client, content)
            .await
            .unwrap_or(Probe::Unreachable);
        client.logout().await;
        probe
    }
}

/// The check itself, on a read-only connection.
async fn probe_on<S: AsyncRead + AsyncWrite + Unpin + Send>(
    client: &mut Client<S>,
    content: &ActionContent,
) -> Result<Probe> {
    match content.kind {
        ActionKind::Draft | ActionKind::Send => {
            let (Some(folder), Some(outgoing)) = (&content.folder, &content.message) else {
                return Ok(Probe::Unknown);
            };
            let found = has_message_id(client, &folder.name, &outgoing.message_id).await?;
            Ok(match (found, content.kind) {
                (true, _) => Probe::Done,
                (false, ActionKind::Draft) => Probe::NotDone,
                // A send not found in Sent may still have gone out.
                (false, _) => Probe::Unknown,
            })
        }
        ActionKind::MarkRead | ActionKind::Archive | ActionKind::Trash | ActionKind::Spam => {
            let Some(source) = &content.source else {
                return Ok(Probe::Unknown);
            };
            let Some((mailbox, uidvalidity, uid)) = imap_source(source) else {
                return Ok(Probe::Unknown);
            };
            let opened: Examined = client.examine_wire(&utf7::encode(mailbox)).await?;
            if opened.uidvalidity != Some(uidvalidity) {
                return Ok(Probe::Unknown);
            }
            let found = fetch_one(client, mailbox, uid).await?;
            if content.kind == ActionKind::MarkRead {
                return Ok(match found {
                    None => Probe::Gone,
                    Some(found) if found.identity != source.identity => Probe::Unknown,
                    Some(found) if found.flagged("\\Seen") => Probe::Done,
                    Some(_) => Probe::NotDone,
                });
            }
            let Some(folder) = &content.folder else {
                return Ok(Probe::Unknown);
            };
            let atomic = client.has("MOVE");
            match found {
                Some(found) if found.identity != source.identity => Ok(Probe::Unknown),
                Some(found) => {
                    let deleted = found.flagged("\\Deleted");
                    let copied = holds(client, &folder.name, source).await?;
                    Ok(match (deleted, copied) {
                        (false, Some(true)) => Probe::Resume(Resume::AfterCopy),
                        (false, Some(false)) => Probe::NotDone,
                        // Without a Message-ID the destination cannot be
                        // searched; only an atomic MOVE leaves no half.
                        (false, None) if atomic => Probe::NotDone,
                        (true, Some(true)) => Probe::Resume(Resume::Expunge),
                        _ => Probe::Unknown,
                    })
                }
                None => Ok(match holds(client, &folder.name, source).await? {
                    Some(true) => Probe::Done,
                    Some(false) => Probe::Gone,
                    None => Probe::Unknown,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests;
