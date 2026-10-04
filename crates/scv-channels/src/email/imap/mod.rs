//! The IMAP adapter: a read-only [`MailSource`] over implicit TLS.
//!
//! A connection signs in, sends `ID` when the server offers it (163 and 126
//! refuse to open a mailbox without it), and opens one mailbox with
//! `EXAMINE`, which cannot change it. Messages are named by UID within the
//! mailbox's `UIDVALIDITY`; the cursor is the next UID to look at. Content
//! is only ever read with `BODY.PEEK`, so reading sets no flag.
//!
//! Layers, each with its own module: [`wire`] frames and parses responses,
//! [`guard`] checks every command against the connection's allowlist,
//! [`client`] offers typed read commands only, [`writer`] the executor's
//! write commands, [`fetch`] and [`structure`] interpret what FETCH
//! returns, [`utf7`] encodes mailbox names, and [`tls`] connects.

use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use super::parse;
use super::source::{
    Caps, Changes, Cursor, FolderNames, Folders, MailSource, Meta, PartRef, PartText, ProviderKind,
    Signals, SourceRef,
};
use client::{Client, Item, Search};
use fetch::{Envelope, Fetched};

mod client;
#[cfg(test)]
pub(crate) mod fake;
mod fetch;
mod guard;
mod structure;
pub(crate) mod tls;
mod utf7;
mod wire;
pub(crate) mod writer;

/// Header fields fetched with each message's metadata: the identity, the
/// thread a reply joins, and the bulk signals.
pub(crate) const HEADER_FIELDS: [&str; 7] = [
    "MESSAGE-ID",
    "REFERENCES",
    "LIST-ID",
    "LIST-UNSUBSCRIBE",
    "PRECEDENCE",
    "AUTO-SUBMITTED",
    "RETURN-PATH",
];

/// UIDs per metadata FETCH, keeping each command's response well inside
/// the per-command byte bound.
const FETCH_BATCH: usize = 64;

/// Where and how to read one mailbox.
#[derive(Clone)]
pub(crate) struct ImapConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    /// The password or provider authorization code.
    pub(crate) password: String,
    /// The mailbox to read, in Unicode, such as `INBOX`.
    pub(crate) mailbox: String,
}

/// Written by hand so that neither the password nor the user name (usually
/// an address) reaches a log through `{:?}`.
impl fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("mailbox", &self.mailbox)
            .finish()
    }
}

/// A cursor's value: the mailbox and `UIDVALIDITY` it belongs to, and the
/// first UID not yet listed.
#[derive(Debug, Deserialize)]
struct Position {
    mailbox: String,
    uidvalidity: u32,
    next: u32,
}

/// One mailbox, open read-only, as a [`MailSource`].
pub(crate) struct ImapSource<S = TlsStream<TcpStream>> {
    client: Client<S>,
    mailbox: String,
    /// From the latest `EXAMINE`.
    uidvalidity: u32,
    /// From the latest `EXAMINE`; some servers leave it out.
    uidnext: Option<u32>,
    caps: Caps,
}

impl ImapSource {
    /// Connects over TLS, signs in, and opens the configured mailbox.
    pub(crate) async fn connect(config: &ImapConfig) -> Result<Self> {
        let stream = tls::connect(&config.host, config.port).await?;
        Self::start(stream, config).await
    }
}

/// Checks a configuration at sign-in: connects, signs in, opens the
/// mailbox, and logs out, reporting what the server offers.
pub(crate) async fn verify(config: &ImapConfig) -> Result<Caps> {
    let source = ImapSource::connect(config).await?;
    let caps = source.caps();
    source.close().await;
    Ok(caps)
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ImapSource<S> {
    /// Starts a session on a connected stream: the greeting, capabilities,
    /// sign-in, `ID` when offered, and `EXAMINE`.
    pub(crate) async fn start(stream: S, config: &ImapConfig) -> Result<Self> {
        let client = open_session(stream, config).await?;
        let caps = Caps {
            push: client.has("IDLE"),
            move_: client.has("MOVE"),
            uidplus: client.has("UIDPLUS"),
            special_use: client.has("SPECIAL-USE"),
            id: client.has("ID"),
            // Base IMAP4rev1 searches any header.
            find_by_message_id: true,
            sent_autofile: false,
            can_move: client.has("MOVE") || client.has("UIDPLUS"),
        };
        let mut source = Self {
            client,
            mailbox: config.mailbox.clone(),
            uidvalidity: 0,
            uidnext: None,
            caps,
        };
        source.examine().await?;
        Ok(source)
    }

    /// Logs out and closes the connection.
    pub(crate) async fn close(mut self) {
        self.client.logout().await;
    }

    /// Opens the mailbox again, for its current `UIDVALIDITY` and
    /// `UIDNEXT`; some servers show new mail only to a fresh selection.
    async fn examine(&mut self) -> Result<()> {
        let examined = self.client.examine(&self.mailbox).await?;
        let Some(uidvalidity) = examined.uidvalidity else {
            bail!("the IMAP server did not report the mailbox's UIDVALIDITY");
        };
        self.uidvalidity = uidvalidity;
        self.uidnext = examined.uidnext;
        Ok(())
    }

    /// [`MailSource::changes`] as of `now`, in Unix seconds.
    async fn changes_at(
        &mut self,
        cursor: Option<&Cursor>,
        limit: usize,
        window_seconds: u64,
        now: u64,
    ) -> Result<Changes> {
        self.examine().await?;
        let Some(cursor) = cursor else {
            let next = self.uid_next(None).await?;
            return Ok(Changes::New {
                refs: Vec::new(),
                next: self.position(next),
            });
        };
        let Some(next) = self.resume(cursor) else {
            let found = self
                .client
                .uid_search(Search::Since(now.saturating_sub(window_seconds)))
                .await?;
            let beyond = found.len().saturating_sub(limit);
            let recent = self.refs(&found[beyond..]);
            let next = self.uid_next(found.last().copied()).await?;
            tracing::debug!(recent = recent.len(), beyond, "IMAP mailbox resynchronized");
            return Ok(Changes::Reset {
                recent,
                beyond,
                next: self.position(next),
            });
        };
        if self.uidnext.is_some_and(|uidnext| uidnext <= next) {
            return Ok(Changes::New {
                refs: Vec::new(),
                next: self.position(next),
            });
        }
        let found = self.client.uid_search(Search::UidFrom(next)).await?;
        let taken = &found[..found.len().min(limit)];
        let next = if taken.len() < found.len() {
            taken.last().map_or(next, |uid| uid.saturating_add(1))
        } else {
            let after = found.last().map(|uid| uid.saturating_add(1));
            [Some(next), self.uidnext, after]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(next)
        };
        Ok(Changes::New {
            refs: self.refs(taken),
            next: self.position(next),
        })
    }

    /// Where a cursor resumes, when it is this mailbox's under the current
    /// `UIDVALIDITY`; `None` calls for a reset.
    fn resume(&self, cursor: &Cursor) -> Option<u32> {
        if cursor.provider != ProviderKind::Imap {
            return None;
        }
        let position: Position = serde_json::from_str(&cursor.value).ok()?;
        (position.mailbox == self.mailbox && position.uidvalidity == self.uidvalidity)
            .then_some(position.next.max(1))
    }

    /// The UID the next message will get: `UIDNEXT`, else one past the
    /// highest UID in use, and never at or below `seen`.
    async fn uid_next(&mut self, seen: Option<u32>) -> Result<u32> {
        let next = if let Some(uidnext) = self.uidnext {
            uidnext
        } else {
            let all = self.client.uid_search(Search::All).await?;
            all.last().map_or(1, |uid| uid.saturating_add(1))
        };
        Ok(seen.map_or(next, |uid| next.max(uid.saturating_add(1))))
    }

    fn position(&self, next: u32) -> Cursor {
        Cursor {
            provider: ProviderKind::Imap,
            value: serde_json::json!({
                "mailbox": self.mailbox,
                "uidvalidity": self.uidvalidity,
                "next": next,
            })
            .to_string(),
        }
    }

    fn refs(&self, uids: &[u32]) -> Vec<SourceRef> {
        uids.iter()
            .map(|&uid| SourceRef::Imap {
                mailbox: self.mailbox.clone(),
                uidvalidity: self.uidvalidity,
                uid,
            })
            .collect()
    }

    /// The UID a reference names in the open mailbox, if it names one.
    fn uid_of(&self, source: &SourceRef) -> Option<u32> {
        let SourceRef::Imap {
            mailbox,
            uidvalidity,
            uid,
        } = source
        else {
            return None;
        };
        (*mailbox == self.mailbox && *uidvalidity == self.uidvalidity && *uid != 0).then_some(*uid)
    }

    fn meta(&self, uid: u32, fetched: &Fetched) -> Meta {
        let header = fetched
            .header_fields()
            .map(parse::header_fields)
            .unwrap_or_default();
        let field = |name: &str| {
            header
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.as_slice())
        };
        let envelope = fetched
            .envelope
            .as_ref()
            .map(Envelope::read)
            .unwrap_or_default();
        let message_id = field("message-id").or(envelope.message_id.as_deref());
        let internal_date = fetched.internal_date.as_deref().unwrap_or_default();
        let size = fetched.size.unwrap_or(0);
        let layout = fetched
            .structure
            .as_ref()
            .map(structure::layout)
            .unwrap_or_default();
        let from = envelope.from.into_iter().next();
        // Servers fill an absent Reply-To with From; only a different one
        // tells the pipeline anything.
        let reply_to = envelope
            .reply_to
            .into_iter()
            .next()
            .filter(|reply_to| Some(reply_to) != from.as_ref());
        let sender = from.as_ref().map_or("", |from| from.address.as_str());
        let identity = parse::identity(
            &self.mailbox,
            message_id,
            internal_date,
            size,
            sender,
            &envelope.subject,
        );
        let locator = parse::locator(message_id, internal_date, size, sender, &envelope.subject);
        let references = field("references").map(parse::msg_ids).unwrap_or_default();
        Meta {
            source: SourceRef::Imap {
                mailbox: self.mailbox.clone(),
                uidvalidity: self.uidvalidity,
                uid,
            },
            identity,
            received_at: fetch::internal_date_seconds(internal_date).unwrap_or(0),
            size,
            from,
            reply_to,
            to: envelope.to,
            cc: envelope.cc,
            subject: envelope.subject,
            message_id: message_id.and_then(parse::msg_id),
            locator,
            references,
            date: envelope.date,
            signals: signals(&header),
            category: None,
            text: layout.text,
            attachments: layout.attachments,
        }
        .bounded()
    }
}

/// A signed-in session on a connected stream, read-only: the greeting,
/// capabilities, sign-in, and `ID` when offered.
pub(crate) async fn open_session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    stream: S,
    config: &ImapConfig,
) -> Result<Client<S>> {
    let mut client = Client::start(stream, client::COMMAND_TIMEOUT).await?;
    if client.capabilities().is_empty() {
        client.capability().await?;
    }
    if !client.preauthenticated() {
        client.login(&config.username, &config.password).await?;
    }
    if client.has("ID") {
        client
            .id(&[("name", "SCV"), ("version", env!("CARGO_PKG_VERSION"))])
            .await?;
    }
    Ok(client)
}

/// The identity and locator of the message `fetched` describes in
/// `mailbox` (UTF-8), as [`ImapSource`] computes them for its metadata.
pub(crate) fn names(mailbox: &str, fetched: &Fetched) -> (String, String) {
    let header = fetched
        .header_fields()
        .map(parse::header_fields)
        .unwrap_or_default();
    let envelope = fetched
        .envelope
        .as_ref()
        .map(Envelope::read)
        .unwrap_or_default();
    let message_id = header
        .iter()
        .find(|(name, _)| name == "message-id")
        .map(|(_, value)| value.as_slice())
        .or(envelope.message_id.as_deref());
    let received = fetched.internal_date.as_deref().unwrap_or_default();
    let size = fetched.size.unwrap_or(0);
    let from = envelope
        .from
        .first()
        .map_or("", |from| from.address.as_str());
    (
        parse::identity(mailbox, message_id, received, size, from, &envelope.subject),
        parse::locator(message_id, received, size, from, &envelope.subject),
    )
}

/// The bulk and automation signals in a message's fetched header fields.
fn signals(header: &[(String, Vec<u8>)]) -> Signals {
    let field = |name: &str| {
        header
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_slice())
    };
    let lowered = |name: &str| {
        field(name)
            .map(|value| String::from_utf8_lossy(value).trim().to_lowercase())
            .filter(|value| !value.is_empty())
    };
    Signals {
        list_id: field("list-id")
            .map(parse::decode_words)
            .filter(|list_id| !list_id.trim().is_empty()),
        list_unsubscribe: field("list-unsubscribe").is_some(),
        precedence: lowered("precedence"),
        auto_submitted: lowered("auto-submitted"),
        null_return_path: field("return-path").is_some_and(|value| {
            value
                .iter()
                .filter(|byte| !byte.is_ascii_whitespace())
                .eq(b"<>")
        }),
    }
}

/// A body section this adapter names: part numbers joined by dots.
fn part_section(id: &str) -> bool {
    !id.is_empty()
        && id.split('.').all(|number| {
            !number.is_empty()
                && number.len() <= 10
                && !number.starts_with('0')
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> MailSource for ImapSource<S> {
    async fn changes(
        &mut self,
        cursor: Option<&Cursor>,
        limit: usize,
        window_seconds: u64,
    ) -> Result<Changes> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        self.changes_at(cursor, limit, window_seconds, now).await
    }

    async fn metadata(&mut self, refs: &[SourceRef]) -> Result<Vec<Meta>> {
        let mut uids: Vec<u32> = refs
            .iter()
            .filter_map(|source| self.uid_of(source))
            .collect();
        uids.sort_unstable();
        uids.dedup();
        let items = [
            Item::Uid,
            Item::InternalDate,
            Item::Size,
            Item::Envelope,
            Item::BodyStructure,
            Item::HeaderFields(&HEADER_FIELDS),
        ];
        let mut found: HashMap<u32, Meta> = HashMap::new();
        for batch in uids.chunks(FETCH_BATCH) {
            for fetched in self.client.uid_fetch(batch, &items).await? {
                if let Some(uid) = fetched.uid {
                    found.insert(uid, self.meta(uid, &fetched));
                }
            }
        }
        Ok(refs
            .iter()
            .filter_map(|source| found.get(&self.uid_of(source)?).cloned())
            .collect())
    }

    async fn text(
        &mut self,
        source: &SourceRef,
        part: &PartRef,
        max_bytes: usize,
    ) -> Result<Option<PartText>> {
        let Some(uid) = self.uid_of(source) else {
            return Ok(None);
        };
        if !part_section(&part.id) {
            bail!("an IMAP text part must be named by its part number");
        }
        // A literal longer than the reader's bound would end the connection.
        let wanted = max_bytes.min(wire::MAX_LITERAL);
        let count = u32::try_from(wanted.max(1)).unwrap_or(u32::MAX);
        let items = [Item::Peek {
            section: &part.id,
            partial: Some((0, count)),
        }];
        let fetched = self.client.uid_fetch(&[uid], &items).await?;
        let Some(fetched) = fetched.iter().find(|message| message.uid == Some(uid)) else {
            return Ok(None);
        };
        let content = fetched.section(&part.id).unwrap_or_default();
        let kept = &content[..content.len().min(wanted)];
        Ok(Some(PartText {
            text: parse::decode_body(kept, part.encoding, part.charset.as_deref()),
            html: part.is_html(),
            truncated: part.size > wanted as u64 || content.len() > wanted,
        }))
    }

    fn caps(&self) -> Caps {
        self.caps.clone()
    }

    async fn folders(&mut self, names: &FolderNames) -> Result<Folders> {
        let listed = self.client.list_folders().await?;
        Ok(resolve_folders(&listed, names))
    }
}

/// A mailbox name from the wire, as Unicode, or as it came when it does
/// not decode.
pub(crate) fn utf7_decode(name: &str) -> String {
    utf7::decode(name).unwrap_or_else(|_| name.to_owned())
}

/// One mailbox as `LIST` or `XLIST` names it: its wire name and its
/// attributes, uppercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) name: String,
    pub(crate) attributes: Vec<String>,
}

/// The special folders among `listed`: marked by `SPECIAL-USE` or `XLIST`
/// attributes, or, where none is marked, named in `names` (UTF-8, encoded
/// here) and present. A folder that cannot be selected is never used, and
/// nothing is guessed.
pub(crate) fn resolve_folders(listed: &[Listed], names: &FolderNames) -> Folders {
    let usable = |folder: &&Listed| {
        !folder
            .attributes
            .iter()
            .any(|attribute| matches!(attribute.as_str(), "\\NOSELECT" | "\\NONEXISTENT"))
    };
    let marked = |marks: &[&str]| {
        listed
            .iter()
            .filter(usable)
            .find(|folder| {
                folder
                    .attributes
                    .iter()
                    .any(|attribute| marks.contains(&attribute.as_str()))
            })
            .map(|folder| folder.name.clone())
    };
    let named = |name: &str| {
        if name.is_empty() {
            return None;
        }
        let wire = utf7::encode(name);
        listed
            .iter()
            .filter(usable)
            .find(|folder| folder.name == wire)
            .map(|folder| folder.name.clone())
    };
    let pick = |marks: &[&str], name: &str| marked(marks).or_else(|| named(name));
    Folders {
        drafts: pick(&["\\DRAFTS"], &names.drafts),
        sent: pick(&["\\SENT"], &names.sent),
        trash: pick(&["\\TRASH"], &names.trash),
        junk: pick(&["\\JUNK", "\\SPAM"], &names.junk),
        archive: pick(&["\\ARCHIVE"], &names.archive),
    }
}

#[cfg(test)]
mod tests;
