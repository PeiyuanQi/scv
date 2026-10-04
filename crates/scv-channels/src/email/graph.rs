//! The Microsoft Graph adapter (Outlook.com and Microsoft 365): a
//! [`MailSource`] on the reader grant, and the executor's [`MailEffects`]
//! on the writer and sender grants.
//!
//! Every request asks for immutable IDs, so a message keeps its ID when it
//! moves, and for bodies as text. New mail is what arrived in the watched
//! folder at or after the cursor's received time, the messages already seen
//! at that very time left out; the first run takes the newest received time
//! and reads nothing older. A message's identity is its immutable ID.
//! Metadata comes from a `$select` of the headers the pipeline reads, and
//! attachments are listed by name, type, and size without their content.
//! Writes are one request each: a draft or a send as MIME (Graph files a
//! sent message in Sent itself), a move to a well-known folder, or marking
//! read; a check finds a message by its `internetMessageId`.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::api::{self, Api, Body, EffectParts, Flavor, Method, Mode, Targets};
use super::content::{ActionContent, ActionKind};
use super::effects::MailEffects;
use super::ledger::Approved;
use super::ledger::actions::{Execution, OutcomeCode, Probe};
use super::parse;
use super::source::{
    Address, AttachmentInfo, Caps, Changes, Cursor, FolderNames, Folders, MailSource, Meta,
    PartRef, PartText, ProviderKind, Signals, SourceRef, TransferEncoding,
};

const BASE: &str = "/v1.0/me";
/// The fields a message's metadata reads.
const SELECT: &str = "id,internetMessageId,subject,from,replyTo,toRecipients,ccRecipients,\
     receivedDateTime,internetMessageHeaders,inferenceClassification,hasAttachments";

/// Where reading resumes: the newest received time listed, and the IDs of
/// the messages received at exactly that time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Position {
    received: String,
    #[serde(default)]
    ids: Vec<String>,
}

/// A Graph mailbox's folder, read.
pub(crate) struct GraphSource {
    api: Api,
    /// The well-known folder watched, such as `inbox`.
    folder: String,
}

impl GraphSource {
    pub(crate) fn new(api: Api, mailbox: &str) -> Result<Self> {
        let folder = mailbox.trim().to_ascii_lowercase();
        if folder != "inbox" {
            bail!("mail.mailbox must be INBOX for an Outlook or Microsoft 365 account");
        }
        Ok(Self { api, folder })
    }
}

fn position_cursor(position: &Position) -> Cursor {
    Cursor {
        provider: ProviderKind::Graph,
        value: serde_json::to_string(position).unwrap_or_default(),
    }
}

/// A received time as Graph writes it, such as `2024-10-04T09:00:00Z`,
/// accepted only in that plain shape so it is safe inside a filter.
fn valid_time(time: &str) -> bool {
    !time.is_empty()
        && time.len() <= 40
        && time.bytes().all(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'-' | b':' | b'T' | b'Z' | b'.' | b'+')
        })
}

/// Unix seconds of a Graph time (`YYYY-MM-DDTHH:MM:SS…Z`).
fn unix_of(time: &str) -> u64 {
    let parse =
        |range: std::ops::Range<usize>| time.get(range).and_then(|text| text.parse::<i64>().ok());
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        parse(0..4),
        parse(5..7),
        parse(8..10),
        parse(11..13),
        parse(14..16),
        parse(17..19),
    ) else {
        return 0;
    };
    // Days from the civil date, the inverse of `message::civil`.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).unwrap_or(0)
}

/// A Graph `emailAddress` object as an address.
fn address(value: &Value) -> Option<Address> {
    let email = &value["emailAddress"];
    let address = email["address"].as_str()?;
    Some(Address {
        name: email["name"].as_str().unwrap_or_default().to_owned(),
        address: address.to_owned(),
    })
}

fn addresses(value: &Value) -> Vec<Address> {
    value
        .as_array()
        .map(|list| list.iter().filter_map(address).collect())
        .unwrap_or_default()
}

/// A message's metadata from its `$select` answer.
pub(crate) fn meta_of(id: &str, message: &Value, attachments: Vec<AttachmentInfo>) -> Meta {
    let headers: Vec<(String, String)> = message["internetMessageHeaders"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|header| {
                    Some((
                        header["name"].as_str()?.to_ascii_lowercase(),
                        header["value"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    };
    let lowered = |name: &str| {
        header(name)
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
    };
    let identity = parse::api_identity("graph", id);
    let from = address(&message["from"]);
    let reply_to = addresses(&message["replyTo"])
        .into_iter()
        .next()
        .filter(|reply_to| Some(reply_to) != from.as_ref());
    Meta {
        source: SourceRef::Graph { id: id.to_owned() },
        identity: identity.clone(),
        received_at: message["receivedDateTime"].as_str().map_or(0, unix_of),
        size: 0,
        from,
        reply_to,
        to: addresses(&message["toRecipients"]),
        cc: addresses(&message["ccRecipients"]),
        subject: message["subject"].as_str().unwrap_or_default().to_owned(),
        message_id: message["internetMessageId"]
            .as_str()
            .and_then(|id| parse::msg_id(id.as_bytes())),
        locator: identity,
        references: header("references")
            .map(|value| parse::msg_ids(value.as_bytes()))
            .unwrap_or_default(),
        date: header("date").map(str::to_owned),
        signals: Signals {
            list_id: header("list-id")
                .map(|value| parse::decode_words(value.as_bytes()))
                .filter(|value| !value.trim().is_empty()),
            list_unsubscribe: header("list-unsubscribe").is_some(),
            precedence: lowered("precedence"),
            auto_submitted: lowered("auto-submitted"),
            null_return_path: header("return-path").is_some_and(|value| {
                value
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .eq("<>".chars())
            }),
        },
        category: message["inferenceClassification"]
            .as_str()
            .filter(|class| *class == "other")
            .map(|_| "OTHER".to_owned()),
        text: Some(PartRef {
            id: "body".into(),
            mime: "text/plain".into(),
            charset: Some("utf-8".into()),
            encoding: TransferEncoding::Binary,
            size: 0,
        }),
        attachments,
    }
    .bounded()
}

#[async_trait]
impl MailSource for GraphSource {
    async fn changes(
        &mut self,
        cursor: Option<&Cursor>,
        limit: usize,
        _window_seconds: u64,
    ) -> Result<Changes> {
        let path = format!("{BASE}/mailFolders/{}/messages", self.folder);
        let position = cursor
            .filter(|cursor| cursor.provider == ProviderKind::Graph)
            .and_then(|cursor| serde_json::from_str::<Position>(&cursor.value).ok())
            .filter(|position| valid_time(&position.received));
        let Some(position) = position else {
            let reply = self
                .api
                .get(
                    &path,
                    &[
                        ("$select", "id,receivedDateTime"),
                        ("$orderby", "receivedDateTime desc"),
                        ("$top", "1"),
                    ],
                )
                .await?;
            if !reply.ok() {
                bail!(
                    "Microsoft Graph refused the folder's messages ({})",
                    reply.status
                );
            }
            let newest = &reply.body["value"][0];
            let received = newest["receivedDateTime"]
                .as_str()
                .filter(|time| valid_time(time))
                .map_or_else(now_time, str::to_owned);
            let ids = newest["id"]
                .as_str()
                .map(|id| vec![id.to_owned()])
                .unwrap_or_default();
            return Ok(Changes::New {
                refs: Vec::new(),
                next: position_cursor(&Position { received, ids }),
            });
        };
        let filter = format!("receivedDateTime ge {}", position.received);
        let top = (limit + position.ids.len()).clamp(1, 1000).to_string();
        let reply = self
            .api
            .get(
                &path,
                &[
                    ("$select", "id,receivedDateTime"),
                    ("$filter", filter.as_str()),
                    ("$orderby", "receivedDateTime asc"),
                    ("$top", top.as_str()),
                ],
            )
            .await?;
        if !reply.ok() {
            bail!(
                "Microsoft Graph refused the folder's messages ({})",
                reply.status
            );
        }
        let mut next = position.clone();
        let mut refs = Vec::new();
        for message in reply.body["value"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let (Some(id), Some(received)) =
                (message["id"].as_str(), message["receivedDateTime"].as_str())
            else {
                continue;
            };
            if !SourceRef::valid_api_id(id)
                || !valid_time(received)
                || position.ids.iter().any(|seen| seen == id)
            {
                continue;
            }
            if refs.len() >= limit {
                break;
            }
            refs.push(SourceRef::Graph { id: id.to_owned() });
            if received == next.received {
                next.ids.push(id.to_owned());
            } else {
                next = Position {
                    received: received.to_owned(),
                    ids: vec![id.to_owned()],
                };
            }
        }
        Ok(Changes::New {
            refs,
            next: position_cursor(&next),
        })
    }

    async fn metadata(&mut self, refs: &[SourceRef]) -> Result<Vec<Meta>> {
        let mut found = Vec::new();
        for reference in refs {
            let SourceRef::Graph { id } = reference else {
                continue;
            };
            let reply = self
                .api
                .get(&format!("{BASE}/messages/{id}"), &[("$select", SELECT)])
                .await?;
            if reply.status == 404 {
                continue;
            }
            if !reply.ok() {
                bail!("Microsoft Graph refused a message ({})", reply.status);
            }
            let attachments = if reply.body["hasAttachments"] == Value::Bool(true) {
                let listed = self
                    .api
                    .get(
                        &format!("{BASE}/messages/{id}/attachments"),
                        &[("$select", "name,contentType,size")],
                    )
                    .await?;
                listed.body["value"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|attachment| AttachmentInfo {
                                name: attachment["name"].as_str().unwrap_or_default().to_owned(),
                                mime: attachment["contentType"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_ascii_lowercase(),
                                size: attachment["size"].as_u64().unwrap_or(0),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            found.push(meta_of(id, &reply.body, attachments));
        }
        Ok(found)
    }

    async fn text(
        &mut self,
        source: &SourceRef,
        _part: &PartRef,
        max_bytes: usize,
    ) -> Result<Option<PartText>> {
        let SourceRef::Graph { id } = source else {
            return Ok(None);
        };
        let reply = self
            .api
            .get(&format!("{BASE}/messages/{id}"), &[("$select", "body")])
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        if !reply.ok() {
            bail!("Microsoft Graph refused a message ({})", reply.status);
        }
        let body = &reply.body["body"];
        let content = body["content"].as_str().unwrap_or_default();
        let html = body["contentType"].as_str() == Some("html");
        let kept = scv_client::text::utf8_prefix(content, max_bytes);
        Ok(Some(PartText {
            text: kept.to_owned(),
            html,
            truncated: kept.len() < content.len(),
        }))
    }

    fn caps(&self) -> Caps {
        Caps {
            find_by_message_id: true,
            sent_autofile: true,
            can_move: true,
            ..Caps::default()
        }
    }

    async fn folders(&mut self, _names: &FolderNames) -> Result<Folders> {
        Ok(graph_folders())
    }
}

/// Graph's well-known folders.
pub(crate) fn graph_folders() -> Folders {
    Folders {
        drafts: Some("drafts".into()),
        sent: Some("sentitems".into()),
        trash: Some("deleteditems".into()),
        junk: Some("junkemail".into()),
        archive: Some("archive".into()),
    }
}

/// Now as Graph writes a time.
fn now_time() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = i64::try_from(now / 86_400).unwrap_or(0);
    let (year, month, day) = super::message::civil(days);
    let seconds = now % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// An OData string literal: single quotes doubled.
fn odata_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The executor's Graph: one approved action on the writer or sender grant.
pub(crate) struct GraphEffects {
    parts: EffectParts,
}

impl GraphEffects {
    pub(crate) fn new(parts: EffectParts) -> Self {
        Self { parts }
    }

    fn reader(&self) -> Result<Api> {
        Api::new(
            Flavor::Graph,
            self.parts.origin.clone(),
            self.parts.reader.clone(),
            Mode::Read,
        )
    }

    fn writer(&self, approved: &Approved) -> Option<Api> {
        let content = approved.content();
        let tokens = if content.kind == ActionKind::Send {
            self.parts.sender.clone()
        } else {
            self.parts.writer.clone()
        }?;
        let id = content
            .source
            .as_ref()
            .and_then(|source| match &source.reference {
                SourceRef::Graph { id } => Some(id.clone()),
                _ => None,
            });
        Api::new(
            Flavor::Graph,
            self.parts.origin.clone(),
            tokens,
            Mode::Write(Targets {
                kind: content.kind,
                id,
                destination: content.folder.as_ref().map(|folder| folder.name.clone()),
            }),
        )
        .ok()
    }

    /// Whether well-known `folder` holds a message with `message_id`.
    async fn find(&self, folder: &str, message_id: &str) -> Result<bool> {
        let filter = format!("internetMessageId eq {}", odata_string(message_id));
        let reply = self
            .reader()?
            .get(
                &format!("{BASE}/mailFolders/{folder}/messages"),
                &[("$filter", filter.as_str()), ("$select", "id")],
            )
            .await?;
        if !reply.ok() {
            bail!("Microsoft Graph refused a search ({})", reply.status);
        }
        Ok(reply.body["value"]
            .as_array()
            .is_some_and(|found| !found.is_empty()))
    }

    /// Message `id`'s folder ID and whether it is read; `None` when gone.
    async fn state(&self, id: &str) -> Result<Option<(String, bool)>> {
        let reply = self
            .reader()?
            .get(
                &format!("{BASE}/messages/{id}"),
                &[("$select", "parentFolderId,isRead")],
            )
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        if !reply.ok() {
            bail!("Microsoft Graph refused a message ({})", reply.status);
        }
        Ok(Some((
            reply.body["parentFolderId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            reply.body["isRead"] == Value::Bool(true),
        )))
    }

    /// The ID of well-known folder `folder`.
    async fn folder_id(&self, folder: &str) -> Result<String> {
        let reply = self
            .reader()?
            .get(
                &format!("{BASE}/mailFolders/{folder}"),
                &[("$select", "id")],
            )
            .await?;
        if !reply.ok() {
            bail!("Microsoft Graph refused a folder ({})", reply.status);
        }
        Ok(reply.body["id"].as_str().unwrap_or_default().to_owned())
    }

    /// Whether `content`'s change shows as done.
    async fn done(&self, content: &ActionContent, id: &str) -> Result<Option<bool>> {
        let Some((parent, read)) = self.state(id).await? else {
            return Ok(None);
        };
        Ok(Some(match (content.kind, &content.folder) {
            (ActionKind::MarkRead, _) => read,
            (_, Some(folder)) => parent == self.folder_id(&folder.name).await?,
            _ => false,
        }))
    }
}

fn not_applied(retry: bool, code: OutcomeCode) -> Execution {
    Execution::NotApplied { retry, code }
}

fn mime(message: &[u8]) -> Body {
    use base64::Engine as _;
    Body::Mime(base64::engine::general_purpose::STANDARD.encode(message))
}

#[async_trait]
impl MailEffects for GraphEffects {
    async fn save_draft(&mut self, approved: &Approved, message: &[u8]) -> Execution {
        let content = approved.content();
        let Some(outgoing) = &content.message else {
            return not_applied(false, OutcomeCode::Internal);
        };
        match self.find("drafts", &outgoing.message_id).await {
            Ok(true) => {
                return Execution::Applied {
                    code: OutcomeCode::AlreadyDone,
                    sent_copy: None,
                };
            }
            Ok(false) => {}
            Err(_) => return super::effects::unreachable(),
        }
        let Some(writer) = self.writer(approved) else {
            return not_applied(false, OutcomeCode::AuthFailed);
        };
        api::classify(
            writer
                .request(
                    Method::Post,
                    &format!("{BASE}/messages"),
                    &[],
                    mime(message),
                )
                .await,
            false,
        )
    }

    async fn change(&mut self, approved: &Approved) -> Execution {
        let content = approved.content();
        let Some(SourceRef::Graph { id }) = content.source.as_ref().map(|source| &source.reference)
        else {
            return not_applied(false, OutcomeCode::Mismatch);
        };
        if !SourceRef::valid_api_id(id) {
            return not_applied(false, OutcomeCode::Internal);
        }
        match self.done(content, id).await {
            Ok(None) => return not_applied(false, OutcomeCode::Gone),
            Ok(Some(true)) => {
                return Execution::Applied {
                    code: OutcomeCode::AlreadyDone,
                    sent_copy: None,
                };
            }
            Ok(Some(false)) => {}
            Err(_) => return super::effects::unreachable(),
        }
        let Some(writer) = self.writer(approved) else {
            return not_applied(false, OutcomeCode::AuthFailed);
        };
        let result = match (content.kind, &content.folder) {
            (ActionKind::MarkRead, _) => {
                writer
                    .request(
                        Method::Patch,
                        &format!("{BASE}/messages/{id}"),
                        &[],
                        Body::Json(json!({ "isRead": true })),
                    )
                    .await
            }
            (_, Some(folder)) => {
                writer
                    .request(
                        Method::Post,
                        &format!("{BASE}/messages/{id}/move"),
                        &[],
                        Body::Json(json!({ "destinationId": folder.name })),
                    )
                    .await
            }
            _ => return not_applied(false, OutcomeCode::Unsupported),
        };
        api::classify(result, false)
    }

    async fn send(&mut self, approved: &Approved, message: &[u8]) -> Execution {
        let Some(sender) = self.writer(approved) else {
            return not_applied(false, OutcomeCode::AuthFailed);
        };
        api::classify(
            sender
                .request(
                    Method::Post,
                    &format!("{BASE}/sendMail"),
                    &[],
                    mime(message),
                )
                .await,
            true,
        )
    }

    async fn copy_sent(&mut self, _approved: &Approved, _message: &[u8]) -> bool {
        // Graph files sent mail by itself.
        true
    }

    async fn probe(&mut self, content: &ActionContent) -> Probe {
        match content.kind {
            ActionKind::Draft | ActionKind::Send => {
                let Some(outgoing) = &content.message else {
                    return Probe::Unknown;
                };
                let (folder, sent) = if content.kind == ActionKind::Send {
                    ("sentitems", true)
                } else {
                    ("drafts", false)
                };
                match self.find(folder, &outgoing.message_id).await {
                    Ok(true) => Probe::Done,
                    Ok(false) if !sent => Probe::NotDone,
                    Ok(false) => Probe::Unknown,
                    Err(_) => Probe::Unreachable,
                }
            }
            _ => {
                let Some(SourceRef::Graph { id }) =
                    content.source.as_ref().map(|source| &source.reference)
                else {
                    return Probe::Unknown;
                };
                match self.done(content, id).await {
                    Ok(None) => Probe::Gone,
                    Ok(Some(true)) => Probe::Done,
                    Ok(Some(false)) => Probe::NotDone,
                    Err(_) => Probe::Unreachable,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
