//! An IMAP client over any byte stream.
//!
//! The client's methods here are the commands a read-only mailbox reader
//! needs, typed; there is no method that sends free-form text, and the only
//! methods that change a mailbox are the executor's, in [`super::writer`],
//! on a connection in write mode for one approved action. Every command
//! passes [`guard::check`] whole before its first byte is written, and a
//! refused command sends nothing and ends the connection. A command that fails to write, to read, or to
//! finish within its timeout leaves the protocol state unknown, so it also
//! ends the connection; a server's `NO` or `BAD` does not.
//!
//! Errors never carry the server's free text, which may echo mail content
//! or the credentials the client just sent, and name a response code only
//! from the standard set. A failed sign-in gets SCV's own advice instead of
//! the server's.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::time::timeout;

use super::fetch::{self, Fetched};
use super::guard::{self, Mode, Part};
use super::utf7;
use super::wire::{self, Code, Response, Status, Value};

/// How long one command, from its first byte written to its completion,
/// may take.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// The response codes an error may name: those RFC 3501, RFC 5530, and
/// RFC 9051 define. Any other is the server's own text, which could echo
/// what the client sent, such as its password.
const KNOWN_CODES: [&str; 31] = [
    "ALERT",
    "ALREADYEXISTS",
    "AUTHENTICATIONFAILED",
    "AUTHORIZATIONFAILED",
    "BADCHARSET",
    "CANNOT",
    "CAPABILITY",
    "CLIENTBUG",
    "CLOSED",
    "CONTACTADMIN",
    "CORRUPTION",
    "EXPIRED",
    "EXPUNGEISSUED",
    "HASCHILDREN",
    "INUSE",
    "LIMIT",
    "NONEXISTENT",
    "NOPERM",
    "OVERQUOTA",
    "PARSE",
    "PERMANENTFLAGS",
    "PRIVACYREQUIRED",
    "READ-ONLY",
    "READ-WRITE",
    "SERVERBUG",
    "TRYCREATE",
    "UIDNEXT",
    "UIDNOTSTICKY",
    "UIDVALIDITY",
    "UNAVAILABLE",
    "UNKNOWN-CTE",
];

/// The mailbox state `EXAMINE` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Examined {
    pub(crate) uidvalidity: Option<u32>,
    pub(crate) uidnext: Option<u32>,
    pub(crate) exists: Option<u32>,
}

/// The searches the client runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Search {
    All,
    /// UIDs from this one on (`UID n:*`).
    UidFrom(u32),
    /// Messages received on or after the UTC day holding these Unix
    /// seconds.
    Since(u64),
    /// Messages whose `Message-ID` header holds this valid message ID.
    MessageId(String),
}

/// The FETCH items the client asks for; each reads without setting
/// `\Seen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Item<'a> {
    Uid,
    Flags,
    InternalDate,
    Size,
    Envelope,
    BodyStructure,
    /// `BODY.PEEK[HEADER.FIELDS (...)]`.
    HeaderFields(&'a [&'a str]),
    /// `BODY.PEEK[section]`, optionally `<origin.count>`.
    Peek {
        section: &'a str,
        partial: Option<(u32, u32)>,
    },
}

impl Item<'_> {
    fn name(&self) -> String {
        match self {
            Self::Uid => "UID".to_owned(),
            Self::Flags => "FLAGS".to_owned(),
            Self::InternalDate => "INTERNALDATE".to_owned(),
            Self::Size => "RFC822.SIZE".to_owned(),
            Self::Envelope => "ENVELOPE".to_owned(),
            Self::BodyStructure => "BODYSTRUCTURE".to_owned(),
            Self::HeaderFields(fields) => {
                format!("BODY.PEEK[HEADER.FIELDS ({})]", fields.join(" "))
            }
            Self::Peek { section, partial } => match partial {
                Some((origin, count)) => format!("BODY.PEEK[{section}]<{origin}.{count}>"),
                None => format!("BODY.PEEK[{section}]"),
            },
        }
    }
}

/// A command's completion and the untagged responses before it. The
/// completion's text is not kept: nothing may show it.
#[derive(Debug)]
pub(super) struct Done {
    pub(super) status: Status,
    pub(super) code: Option<Code>,
    pub(super) untagged: Vec<Response>,
}

impl Done {
    /// The completion, or an error naming only the verb, the status, and
    /// the response code.
    pub(super) fn ok(self, verb: &str) -> Result<Self> {
        if self.status == Status::Ok {
            Ok(self)
        } else {
            bail!(
                "the IMAP server refused {verb}: {}{}",
                self.status.name(),
                code_label(self.code.as_ref())
            )
        }
    }
}

/// ` [CODE]` for a response code in [`KNOWN_CODES`], spelled as listed
/// there; nothing for any other.
pub(super) fn code_label(code: Option<&Code>) -> String {
    code.and_then(|code| {
        KNOWN_CODES
            .iter()
            .find(|known| known.eq_ignore_ascii_case(&code.name))
    })
    .map_or_else(String::new, |known| format!(" [{known}]"))
}

/// An IMAP connection, read-only unless made for one approved action.
pub(crate) struct Client<S> {
    stream: BufReader<S>,
    tag: u32,
    /// Set once the protocol state is unknown or the connection closed;
    /// every later command is refused.
    pub(super) broken: bool,
    mode: Mode,
    timeout: Duration,
    /// Uppercased.
    capabilities: Vec<String>,
    preauthenticated: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Client<S> {
    /// Reads the server's greeting: `OK` or `PREAUTH` starts a session,
    /// `BYE` refuses it. `timeout` bounds the greeting and each command.
    pub(crate) async fn start(stream: S, timeout: Duration) -> Result<Self> {
        let mut client = Self {
            stream: BufReader::new(stream),
            tag: 0,
            broken: false,
            mode: Mode::ReadOnly,
            timeout,
            capabilities: Vec::new(),
            preauthenticated: false,
        };
        let mut budget = wire::MAX_RESPONSE;
        let greeting = tokio::time::timeout(timeout, client.read(&mut budget))
            .await
            .map_err(|_| anyhow!("the IMAP server sent no greeting in time"))?
            .context("reading the IMAP greeting failed")?;
        match greeting {
            Response::Status {
                status: status @ (Status::Ok | Status::PreAuth),
                code,
                ..
            } => {
                client.preauthenticated = status == Status::PreAuth;
                if let Some(code) = code.filter(|code| code.name == "CAPABILITY") {
                    client.capabilities = atoms(&code.args);
                }
                Ok(client)
            }
            Response::Status {
                status: Status::Bye,
                code,
                ..
            } => bail!(
                "the IMAP server refused the connection{}",
                code_label(code.as_ref())
            ),
            _ => bail!("the IMAP server sent an unexpected greeting"),
        }
    }

    /// Allow, besides reading, the commands of one approved action on its
    /// targets. Only the executor's writer does this.
    pub(super) fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// The capabilities last announced, uppercased.
    pub(crate) fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    pub(crate) fn has(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|known| known.eq_ignore_ascii_case(capability))
    }

    /// The greeting was `PREAUTH`: the session is already signed in.
    pub(crate) fn preauthenticated(&self) -> bool {
        self.preauthenticated
    }

    /// Whether the connection can take no more commands.
    #[cfg(test)]
    pub(crate) fn is_broken(&self) -> bool {
        self.broken
    }

    /// `CAPABILITY`.
    pub(crate) async fn capability(&mut self) -> Result<()> {
        let done = self.run("CAPABILITY", Vec::new()).await?.ok("CAPABILITY")?;
        self.note_capabilities(&done);
        Ok(())
    }

    /// `ID (key value ...)`, which some servers (163, 126) require before
    /// a mailbox is opened. A server that refuses it is not an error.
    pub(crate) async fn id(&mut self, fields: &[(&str, &str)]) -> Result<()> {
        let fields: Vec<String> = fields
            .iter()
            .flat_map(|(key, value)| [quote(key), quote(value)])
            .collect();
        let done = self
            .run(&format!("ID ({})", fields.join(" ")), Vec::new())
            .await?;
        if done.status != Status::Ok {
            tracing::debug!(status = done.status.name(), "IMAP ID was refused");
        }
        Ok(())
    }

    /// Signs in: `LOGIN`, or `AUTHENTICATE PLAIN` when the server
    /// advertises `LOGINDISABLED`. Capabilities are refreshed afterwards,
    /// since signing in may change them.
    pub(crate) async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        if username.contains('\0') || password.contains('\0') {
            bail!("an IMAP user name or password cannot hold a NUL character");
        }
        let done = if self.has("LOGINDISABLED") {
            if !self.has("AUTH=PLAIN") {
                bail!("the IMAP server offers neither LOGIN nor AUTHENTICATE PLAIN");
            }
            let response = base64::engine::general_purpose::STANDARD
                .encode(format!("\0{username}\0{password}"));
            if self.has("SASL-IR") {
                self.run(&format!("AUTHENTICATE PLAIN {response}"), Vec::new())
                    .await?
            } else {
                self.run("AUTHENTICATE PLAIN", vec![Part::Line(response)])
                    .await?
            }
        } else {
            self.run(
                "LOGIN ",
                vec![
                    astring(username),
                    Part::Text(" ".to_owned()),
                    astring(password),
                ],
            )
            .await?
        };
        if done.status != Status::Ok {
            // The server's text is left out: a hostile server can echo the
            // password it was just sent.
            bail!(
                "IMAP sign-in was refused: {}{}. Check the user name and the password; QQ, \
                 163, and 126 mailboxes take an authorization code from their settings, \
                 with IMAP turned on there, instead of the account password",
                done.status.name(),
                code_label(done.code.as_ref()),
            );
        }
        if !self.note_capabilities(&done) {
            self.capability().await?;
        }
        Ok(())
    }

    /// `EXAMINE`: opens `mailbox` read-only and reports its state.
    pub(crate) async fn examine(&mut self, mailbox: &str) -> Result<Examined> {
        self.examine_wire(&utf7::encode(mailbox)).await
    }

    /// `EXAMINE` of a mailbox by its wire name, as `LIST` gave it.
    pub(crate) async fn examine_wire(&mut self, wire: &str) -> Result<Examined> {
        let command = format!("EXAMINE {}", quote(wire));
        let done = self.run(&command, Vec::new()).await?.ok("EXAMINE")?;
        Ok(examined(&done))
    }

    /// Every mailbox with its attributes: `LIST "" "*" RETURN
    /// (SPECIAL-USE)` when the server marks special folders, `XLIST` when it
    /// has only that, and a plain `LIST` otherwise.
    pub(crate) async fn list_folders(&mut self) -> Result<Vec<super::Listed>> {
        let (command, kind) = if self.has("SPECIAL-USE") {
            ("LIST \"\" \"*\" RETURN (SPECIAL-USE)", "LIST")
        } else if self.has("XLIST") {
            ("XLIST \"\" \"*\"", "XLIST")
        } else {
            ("LIST \"\" \"*\"", "LIST")
        };
        let done = self.run(command, Vec::new()).await?.ok(kind)?;
        let mut listed = Vec::new();
        for response in &done.untagged {
            let Response::Data {
                kind: found,
                values,
            } = response
            else {
                continue;
            };
            if found != kind {
                continue;
            }
            let [Value::List(attributes), _delimiter, name, ..] = values.as_slice() else {
                continue;
            };
            let Some(name) = name.bytes().and_then(|name| std::str::from_utf8(name).ok()) else {
                continue;
            };
            if name.is_empty() || !name.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
                continue;
            }
            listed.push(super::Listed {
                name: name.to_owned(),
                attributes: atoms(attributes),
            });
        }
        tracing::debug!(folders = listed.len(), "IMAP folders listed");
        Ok(listed)
    }
}

/// The mailbox state an `EXAMINE` or `SELECT` reported.
pub(super) fn examined(done: &Done) -> Examined {
    let mut examined = Examined::default();
    let number = |value: Option<u64>| {
        value
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n != 0)
    };
    for response in &done.untagged {
        match response {
            Response::Status {
                status: Status::Ok,
                code: Some(code),
                ..
            } => match code.name.as_str() {
                "UIDVALIDITY" => examined.uidvalidity = number(code.number()),
                "UIDNEXT" => examined.uidnext = number(code.number()),
                _ => {}
            },
            Response::Message { number, kind, .. } if kind == "EXISTS" => {
                examined.exists = u32::try_from(*number).ok();
            }
            _ => {}
        }
    }
    examined
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Client<S> {
    /// `UID SEARCH`: the matching UIDs, ascending. For `UID n:*`, UIDs
    /// below `n` are dropped: when no message has a UID of `n` or more,
    /// `*` stands for the highest UID in use and the range matches it.
    pub(crate) async fn uid_search(&mut self, search: Search) -> Result<Vec<u32>> {
        let criteria = match &search {
            Search::All => "ALL".to_owned(),
            Search::UidFrom(from) => format!("UID {from}:*"),
            Search::Since(seconds) => format!("SINCE {}", fetch::search_date(*seconds)),
            Search::MessageId(id) => format!("HEADER MESSAGE-ID {}", quote(id)),
        };
        let done = self
            .run(&format!("UID SEARCH {criteria}"), Vec::new())
            .await?
            .ok("UID SEARCH")?;
        let mut uids: Vec<u32> = done
            .untagged
            .iter()
            .filter_map(|response| match response {
                Response::Data { kind, values } if kind == "SEARCH" => Some(values),
                _ => None,
            })
            .flatten()
            .filter_map(|value| match value {
                Value::Number(uid) => u32::try_from(*uid).ok().filter(|&uid| uid != 0),
                _ => None,
            })
            .collect();
        if let Search::UidFrom(from) = search {
            uids.retain(|&uid| uid >= from);
        }
        uids.sort_unstable();
        uids.dedup();
        tracing::debug!(found = uids.len(), "IMAP UID SEARCH");
        Ok(uids)
    }

    /// `UID FETCH`: what each of `uids` returned, `UID` always asked for.
    /// A message the server did not return is absent; data it split over
    /// several responses is merged.
    pub(crate) async fn uid_fetch(
        &mut self,
        uids: &[u32],
        items: &[Item<'_>],
    ) -> Result<Vec<Fetched>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set: Vec<String> = uids.iter().map(u32::to_string).collect();
        let names: Vec<String> = std::iter::once(Item::Uid)
            .chain(items.iter().copied().filter(|item| *item != Item::Uid))
            .map(|item| item.name())
            .collect();
        let command = format!("UID FETCH {} ({})", set.join(","), names.join(" "));
        let done = self.run(&command, Vec::new()).await?.ok("UID FETCH")?;
        let mut fetched: Vec<Fetched> = Vec::new();
        // Sequence numbers let a response without its UID join the one that
        // had it. An EXPUNGE renumbers, so it forgets them.
        let mut by_number: HashMap<u64, usize> = HashMap::new();
        for response in &done.untagged {
            match response {
                Response::Message {
                    number,
                    kind,
                    values,
                } if kind == "FETCH" => {
                    let Some(list) = values.first().and_then(Value::list) else {
                        continue;
                    };
                    let mut part = Fetched::default();
                    part.merge(list);
                    let known = match part.uid {
                        Some(uid) => fetched.iter().position(|seen| seen.uid == Some(uid)),
                        None => by_number.get(number).copied(),
                    };
                    match known {
                        Some(index) => {
                            fetched[index].merge(list);
                            by_number.insert(*number, index);
                        }
                        None if part.uid.is_some() => {
                            by_number.insert(*number, fetched.len());
                            fetched.push(part);
                        }
                        None => {}
                    }
                }
                Response::Message { kind, .. } if kind == "EXPUNGE" => by_number.clear(),
                _ => {}
            }
        }
        fetched.retain(|message| message.uid.is_some_and(|uid| uids.contains(&uid)));
        tracing::debug!(
            asked = uids.len(),
            returned = fetched.len(),
            "IMAP UID FETCH"
        );
        Ok(fetched)
    }

    /// `NOOP`.
    #[cfg(test)]
    pub(crate) async fn noop(&mut self) -> Result<()> {
        self.run("NOOP", Vec::new()).await?.ok("NOOP")?;
        Ok(())
    }

    /// `LOGOUT`, then closes the stream. The connection is finished
    /// whatever the server answers.
    pub(crate) async fn logout(&mut self) {
        if self.broken {
            return;
        }
        if self.run("LOGOUT", Vec::new()).await.is_err() {
            tracing::debug!("IMAP LOGOUT did not complete");
        }
        self.broken = true;
        let _ = timeout(self.timeout, self.stream.get_mut().shutdown()).await;
    }

    /// Records the capabilities a completion carried, in its response code
    /// or an untagged `CAPABILITY`; whether it carried any.
    fn note_capabilities(&mut self, done: &Done) -> bool {
        let mut noted = false;
        if let Some(code) = done.code.as_ref().filter(|code| code.name == "CAPABILITY") {
            self.capabilities = atoms(&code.args);
            noted = true;
        }
        for response in &done.untagged {
            if let Response::Data { kind, values } = response
                && kind == "CAPABILITY"
            {
                self.capabilities = atoms(values);
                noted = true;
            }
        }
        noted
    }

    /// Runs one command: `text` (the verb and its arguments, without the
    /// tag) and then `rest`. The guard sees the whole command first.
    pub(super) async fn run(&mut self, text: &str, rest: Vec<Part>) -> Result<Done> {
        if self.broken {
            bail!("the IMAP connection is closed");
        }
        self.tag += 1;
        let tag = format!("A{:04}", self.tag);
        let mut parts = vec![Part::Text(format!("{tag} {text}"))];
        parts.extend(rest);
        let verb = match guard::check(&self.mode, &parts) {
            Ok(verb) => verb,
            Err(violation) => {
                self.broken = true;
                tracing::error!(verb = %violation.verb, "refused an IMAP command the connection may not send");
                return Err(violation.into());
            }
        };
        tracing::debug!(verb = %verb, "IMAP command");
        match timeout(self.timeout, self.exchange(&tag, &parts)).await {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(error)) => {
                self.broken = true;
                Err(error.context(format!("IMAP {verb} failed")))
            }
            Err(_) => {
                self.broken = true;
                bail!("IMAP {verb} timed out")
            }
        }
    }

    /// Writes a checked command, waiting for the server's `+` before each
    /// literal or SASL line, and reads until its completion.
    async fn exchange(&mut self, tag: &str, parts: &[Part]) -> Result<Done> {
        let mut budget = wire::MAX_RESPONSE;
        let mut untagged = Vec::new();
        let segments = segments(parts);
        let last = segments.len() - 1;
        for (index, segment) in segments.iter().enumerate() {
            let writer = self.stream.get_mut();
            writer.write_all(segment).await?;
            writer.flush().await?;
            if index == last {
                break;
            }
            loop {
                match self.read(&mut budget).await? {
                    Response::Continuation { .. } => break,
                    Response::Tagged {
                        tag: answered,
                        status,
                        code,
                        ..
                    } if answered == tag => {
                        return Ok(Done {
                            status,
                            code,
                            untagged,
                        });
                    }
                    Response::Tagged { .. } => bail!("the IMAP server answered another command"),
                    response => untagged.push(response),
                }
            }
        }
        loop {
            match self.read(&mut budget).await? {
                Response::Tagged {
                    tag: answered,
                    status,
                    code,
                    ..
                } if answered == tag => {
                    return Ok(Done {
                        status,
                        code,
                        untagged,
                    });
                }
                Response::Tagged { .. } => bail!("the IMAP server answered another command"),
                Response::Continuation { .. } => {
                    bail!("the IMAP server asked for more than the command holds")
                }
                response => untagged.push(response),
            }
        }
    }

    /// The next response. An untagged one that does not parse is skipped:
    /// framing does not depend on parsing, and one hostile message must not
    /// stop every fetch.
    async fn read(&mut self, budget: &mut usize) -> Result<Response> {
        loop {
            let raw = wire::read_response(&mut self.stream, budget).await?;
            match wire::parse(&raw) {
                Ok(response) => return Ok(response),
                Err(_) if raw.starts_with(b"* ") => {
                    tracing::warn!(bytes = raw.len(), "skipped a malformed IMAP response");
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// The bytes to write, split where the server must answer `+` first.
fn segments(parts: &[Part]) -> Vec<Vec<u8>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) => current.extend_from_slice(text.as_bytes()),
            Part::Literal(bytes) => {
                current.extend_from_slice(format!("{{{}}}\r\n", bytes.len()).as_bytes());
                segments.push(std::mem::replace(&mut current, bytes.clone()));
            }
            Part::Line(line) => {
                current.extend_from_slice(b"\r\n");
                segments.push(std::mem::replace(&mut current, line.as_bytes().to_vec()));
            }
        }
    }
    current.extend_from_slice(b"\r\n");
    segments.push(current);
    segments
}

/// A string argument: quoted when it is printable ASCII, else a literal.
fn astring(value: &str) -> Part {
    if value.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        Part::Text(quote(value))
    } else {
        Part::Literal(value.as_bytes().to_vec())
    }
}

/// A quoted string, `\` and `"` escaped. Callers pass printable ASCII.
pub(super) fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

fn atoms(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| match value {
            Value::Atom(atom) => Some(atom.to_ascii_uppercase()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
