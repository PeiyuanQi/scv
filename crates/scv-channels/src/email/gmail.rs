//! The Gmail API adapter: a [`MailSource`] on the reader grant, and the
//! executor's [`MailEffects`] on the writer grant.
//!
//! New mail is found with `history.list` from the last history ID (only
//! messages added with the watched label), with no backfill on the first
//! run; a history ID too old to use (`404`) lists the last `catchup_hours`
//! of the label instead. A message is named by its Gmail ID, which is its
//! own for life, so its identity is that ID. Metadata comes from
//! `format=full`, whose parts name attachments without their content; a
//! text part is decoded from its base64url data. Writes are one request
//! each: `drafts.create`, `messages.send` with the message's bytes, and for
//! Trash, Spam, archiving, and marking read `messages.trash` or a label
//! change of exactly the approved kind. Gmail files sent mail by itself,
//! and a check finds a message by its `Message-ID` (`rfc822msgid:`).

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::api::{self, Api, Body, EffectParts, Flavor, Method, Mode, Targets};
use super::content::{ActionContent, ActionKind};
use super::effects::MailEffects;
use super::ledger::Approved;
use super::ledger::actions::{Execution, OutcomeCode, Probe};
use super::parse::{self, TransferEncoding};
use super::source::{
    AttachmentInfo, Caps, Changes, Cursor, FolderNames, Folders, MailSource, Meta, PartRef,
    PartText, ProviderKind, Signals, SourceRef,
};

const BASE: &str = "/gmail/v1/users/me";
/// History pages read in one check.
const MAX_PAGES: usize = 10;
/// A Gmail mailbox's label, read.
pub(crate) struct GmailSource {
    api: Api,
    /// The label watched, such as `INBOX`.
    label: String,
}

impl GmailSource {
    pub(crate) fn new(api: Api, mailbox: &str) -> Result<Self> {
        let label = mailbox.trim();
        if label.is_empty()
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            bail!("mail.mailbox must be a Gmail label ID, such as INBOX, for a Gmail account");
        }
        Ok(Self {
            api,
            label: label.to_owned(),
        })
    }

    async fn history_id(&self) -> Result<String> {
        let reply = self.api.get(&format!("{BASE}/profile"), &[]).await?;
        if !reply.ok() {
            bail!("the Gmail API refused the profile ({})", reply.status);
        }
        id_field(&reply.body, "historyId")
            .ok_or_else(|| anyhow!("the Gmail API's answer had no history ID"))
    }
}

/// A numeric or string ID field as text.
fn id_field(value: &Value, name: &str) -> Option<String> {
    match &value[name] {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn cursor(history: String) -> Cursor {
    Cursor {
        provider: ProviderKind::Gmail,
        value: history,
    }
}

fn reference(id: &str) -> Option<SourceRef> {
    SourceRef::valid_api_id(id).then(|| SourceRef::Gmail { id: id.to_owned() })
}

/// The value of header `name` among a payload's headers.
fn header<'a>(headers: &'a [Value], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| {
            header["name"]
                .as_str()
                .is_some_and(|known| known.eq_ignore_ascii_case(name))
        })
        .and_then(|header| header["value"].as_str())
}

/// The first text part (plain before HTML) that is not an attachment, and
/// every attachment, of a `format=full` payload.
fn walk(
    part: &Value,
    text: &mut Vec<(PartRef, bool)>,
    attachments: &mut Vec<AttachmentInfo>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let mime = part["mimeType"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let filename = part["filename"].as_str().unwrap_or_default();
    let size = part["body"]["size"].as_u64().unwrap_or(0);
    if let Some(parts) = part["parts"].as_array() {
        for child in parts {
            walk(child, text, attachments, depth + 1);
        }
        return;
    }
    if !filename.is_empty()
        || part["body"]["attachmentId"].is_string() && !mime.starts_with("text/")
    {
        attachments.push(AttachmentInfo {
            name: filename.to_owned(),
            mime,
            size,
        });
        return;
    }
    if mime == "text/plain" || mime == "text/html" {
        let headers = part["headers"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let charset = header(headers, "Content-Type").and_then(charset_of);
        text.push((
            PartRef {
                id: part["partId"].as_str().unwrap_or_default().to_owned(),
                mime: mime.clone(),
                charset,
                // Gmail hands over part data decoded from its transfer
                // encoding, as base64url.
                encoding: TransferEncoding::Binary,
                size,
            },
            mime == "text/plain",
        ));
    }
}

/// The `charset` parameter of a `Content-Type` value, lowercased.
fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    })
}

/// A message's metadata from its `format=full` answer.
pub(crate) fn meta_of(id: &str, message: &Value) -> Meta {
    let payload = &message["payload"];
    let headers = payload["headers"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let get = |name: &str| header(headers, name).unwrap_or_default();
    let lowered = |name: &str| {
        header(headers, name)
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
    };
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    walk(payload, &mut text, &mut attachments, 0);
    let part = text
        .iter()
        .find(|(_, plain)| *plain)
        .or_else(|| text.first())
        .map(|(part, _)| part.clone());
    let identity = parse::api_identity("gmail", id);
    let labels: Vec<&str> = message["labelIds"]
        .as_array()
        .map(|labels| labels.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Meta {
        source: SourceRef::Gmail { id: id.to_owned() },
        identity: identity.clone(),
        received_at: message["internalDate"]
            .as_str()
            .and_then(|ms| ms.parse::<u64>().ok())
            .map_or(0, |ms| ms / 1000),
        size: message["sizeEstimate"].as_u64().unwrap_or(0),
        from: parse::addresses(get("From")).into_iter().next(),
        reply_to: parse::addresses(get("Reply-To")).into_iter().next(),
        to: parse::addresses(get("To")),
        cc: parse::addresses(get("Cc")),
        subject: parse::decode_words(get("Subject").as_bytes()),
        message_id: parse::msg_id(get("Message-ID").as_bytes()),
        locator: identity,
        references: parse::msg_ids(get("References").as_bytes()),
        date: header(headers, "Date").map(str::to_owned),
        signals: Signals {
            list_id: header(headers, "List-Id")
                .map(|value| parse::decode_words(value.as_bytes()))
                .filter(|value| !value.trim().is_empty()),
            list_unsubscribe: header(headers, "List-Unsubscribe").is_some(),
            precedence: lowered("Precedence"),
            auto_submitted: lowered("Auto-Submitted"),
            null_return_path: header(headers, "Return-Path").is_some_and(|value| {
                value
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .eq("<>".chars())
            }),
        },
        category: labels
            .iter()
            .find(|label| label.starts_with("CATEGORY_"))
            .map(|label| (*label).to_owned()),
        text: part,
        attachments,
    }
    .bounded()
}

#[async_trait]
impl MailSource for GmailSource {
    async fn changes(
        &mut self,
        cursor_now: Option<&Cursor>,
        limit: usize,
        window_seconds: u64,
    ) -> Result<Changes> {
        let start = cursor_now
            .filter(|cursor| cursor.provider == ProviderKind::Gmail)
            .map(|cursor| cursor.value.clone());
        let Some(start) = start else {
            return Ok(Changes::New {
                refs: Vec::new(),
                next: cursor(self.history_id().await?),
            });
        };
        let mut refs: Vec<SourceRef> = Vec::new();
        let mut next = start.clone();
        let mut page: Option<String> = None;
        let max = limit.clamp(1, 500).to_string();
        for _ in 0..MAX_PAGES {
            let mut query = vec![
                ("startHistoryId", start.as_str()),
                ("historyTypes", "messageAdded"),
                ("labelId", self.label.as_str()),
                ("maxResults", max.as_str()),
            ];
            if let Some(token) = &page {
                query.push(("pageToken", token.as_str()));
            }
            let reply = self.api.get(&format!("{BASE}/history"), &query).await?;
            if reply.status == 404 {
                return self.resync(limit, window_seconds).await;
            }
            if !reply.ok() {
                bail!("the Gmail API refused the history ({})", reply.status);
            }
            for record in reply.body["history"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let mut added = Vec::new();
                for item in record["messagesAdded"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let watched = item["message"]["labelIds"]
                        .as_array()
                        .is_some_and(|labels| {
                            labels
                                .iter()
                                .any(|label| label.as_str() == Some(&self.label))
                        });
                    let Some(found) = watched
                        .then(|| item["message"]["id"].as_str())
                        .flatten()
                        .and_then(reference)
                    else {
                        continue;
                    };
                    if !refs.contains(&found) && !added.contains(&found) {
                        added.push(found);
                    }
                }
                if refs.len() + added.len() > limit {
                    if refs.is_empty() {
                        // A history id cannot resume in the middle of one
                        // record, so an oversized record is taken whole.
                        refs.extend(added);
                        if let Some(id) = id_field(record, "id") {
                            next = id;
                        }
                    }
                    return Ok(Changes::New {
                        refs,
                        next: cursor(next),
                    });
                }
                refs.extend(added);
                if let Some(id) = id_field(record, "id") {
                    next = id;
                }
            }
            if let Some(token) = reply.body["nextPageToken"].as_str() {
                page = Some(token.to_owned());
            } else {
                if let Some(latest) = id_field(&reply.body, "historyId") {
                    next = latest;
                }
                break;
            }
        }
        Ok(Changes::New {
            refs,
            next: cursor(next),
        })
    }

    async fn metadata(&mut self, refs: &[SourceRef]) -> Result<Vec<Meta>> {
        let mut found = Vec::new();
        for reference in refs {
            let SourceRef::Gmail { id } = reference else {
                continue;
            };
            let reply = self
                .api
                .get(&format!("{BASE}/messages/{id}"), &[("format", "full")])
                .await?;
            if reply.status == 404 {
                continue;
            }
            if !reply.ok() {
                bail!("the Gmail API refused a message ({})", reply.status);
            }
            found.push(meta_of(id, &reply.body));
        }
        Ok(found)
    }

    async fn text(
        &mut self,
        source: &SourceRef,
        part: &PartRef,
        max_bytes: usize,
    ) -> Result<Option<PartText>> {
        let SourceRef::Gmail { id } = source else {
            return Ok(None);
        };
        let reply = self
            .api
            .get(&format!("{BASE}/messages/{id}"), &[("format", "full")])
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        if !reply.ok() {
            bail!("the Gmail API refused a message ({})", reply.status);
        }
        let Some(found) = find_part(&reply.body["payload"], &part.id, 0) else {
            return Ok(None);
        };
        let data = match (
            found["body"]["data"].as_str(),
            found["body"]["attachmentId"].as_str(),
        ) {
            (Some(data), _) => data.to_owned(),
            (None, Some(attachment)) if SourceRef::valid_api_id(attachment) => {
                let reply = self
                    .api
                    .get(
                        &format!("{BASE}/messages/{id}/attachments/{attachment}"),
                        &[],
                    )
                    .await?;
                reply.body["data"].as_str().unwrap_or_default().to_owned()
            }
            _ => String::new(),
        };
        let bytes = api::from_base64url(&data).unwrap_or_default();
        let kept = &bytes[..bytes.len().min(max_bytes)];
        Ok(Some(PartText {
            text: parse::decode_body(kept, TransferEncoding::Binary, part.charset.as_deref()),
            html: part.is_html(),
            truncated: bytes.len() > max_bytes,
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
        Ok(gmail_folders())
    }
}

impl GmailSource {
    /// After a history ID ran out: the last `window_seconds` of the label,
    /// oldest first, at most `limit`.
    async fn resync(&self, limit: usize, window_seconds: u64) -> Result<Changes> {
        let days = window_seconds.div_ceil(86_400).max(1).to_string();
        let query = format!("newer_than:{days}d");
        let max = limit.clamp(1, 500).to_string();
        let reply = self
            .api
            .get(
                &format!("{BASE}/messages"),
                &[
                    ("labelIds", self.label.as_str()),
                    ("q", query.as_str()),
                    ("maxResults", max.as_str()),
                ],
            )
            .await?;
        if !reply.ok() {
            bail!("the Gmail API refused a message list ({})", reply.status);
        }
        let mut recent: Vec<SourceRef> = reply.body["messages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|message| message["id"].as_str().and_then(reference))
            .collect();
        recent.reverse();
        let estimate = reply.body["resultSizeEstimate"].as_u64().unwrap_or(0) as usize;
        Ok(Changes::Reset {
            beyond: estimate.saturating_sub(recent.len()),
            recent,
            next: cursor(self.history_id().await?),
        })
    }
}

/// Gmail's special folders are labels; archiving takes a message out of the
/// inbox.
pub(crate) fn gmail_folders() -> Folders {
    Folders {
        drafts: Some("DRAFT".into()),
        sent: Some("SENT".into()),
        trash: Some("TRASH".into()),
        junk: Some("SPAM".into()),
        archive: Some("ARCHIVE".into()),
    }
}

/// The part named `id` in a payload.
fn find_part<'a>(part: &'a Value, id: &str, depth: usize) -> Option<&'a Value> {
    if depth > 32 {
        return None;
    }
    if part["partId"].as_str() == Some(id) {
        return Some(part);
    }
    part["parts"]
        .as_array()?
        .iter()
        .find_map(|child| find_part(child, id, depth + 1))
}

/// The executor's Gmail: one approved action on the writer grant.
pub(crate) struct GmailEffects {
    parts: EffectParts,
}

impl GmailEffects {
    pub(crate) fn new(parts: EffectParts) -> Self {
        Self { parts }
    }

    fn reader(&self) -> Result<Api> {
        Api::new(
            Flavor::Gmail,
            self.parts.origin.clone(),
            self.parts.reader.clone(),
            Mode::Read,
        )
    }

    fn writer(&self, approved: &Approved, sending: bool) -> Option<Api> {
        let content = approved.content();
        let tokens = if sending {
            self.parts.sender.clone()
        } else {
            self.parts.writer.clone()
        }?;
        let id = content
            .source
            .as_ref()
            .and_then(|source| match &source.reference {
                SourceRef::Gmail { id } => Some(id.clone()),
                _ => None,
            });
        Api::new(
            Flavor::Gmail,
            self.parts.origin.clone(),
            tokens,
            Mode::Write(Targets {
                kind: content.kind,
                id,
                destination: None,
            }),
        )
        .ok()
    }

    /// Whether a message with `message_id` exists (in Sent, when `sent`).
    async fn find(&self, message_id: &str, sent: bool) -> Result<bool> {
        let message_id = message_id.trim_start_matches('<').trim_end_matches('>');
        let query = if sent {
            format!("rfc822msgid:{message_id} in:sent")
        } else {
            format!("rfc822msgid:{message_id} in:anywhere")
        };
        let reply = self
            .reader()?
            .get(&format!("{BASE}/messages"), &[("q", query.as_str())])
            .await?;
        if !reply.ok() {
            bail!("the Gmail API refused a search ({})", reply.status);
        }
        Ok(reply.body["messages"]
            .as_array()
            .is_some_and(|found| !found.is_empty()))
    }

    /// The labels of message `id`; `None` when it is gone.
    async fn labels(&self, id: &str) -> Result<Option<Vec<String>>> {
        let reply = self
            .reader()?
            .get(&format!("{BASE}/messages/{id}"), &[("format", "minimal")])
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        if !reply.ok() {
            bail!("the Gmail API refused a message ({})", reply.status);
        }
        Ok(Some(
            reply.body["labelIds"]
                .as_array()
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(|label| label.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        ))
    }

    /// The thread of the message a reply answers, to keep it there.
    async fn thread(&self, content: &ActionContent) -> Option<String> {
        let SourceRef::Gmail { id } = &content.source.as_ref()?.reference else {
            return None;
        };
        let reply = self
            .reader()
            .ok()?
            .get(&format!("{BASE}/messages/{id}"), &[("format", "minimal")])
            .await
            .ok()?;
        reply.body["threadId"]
            .as_str()
            .filter(|thread| SourceRef::valid_api_id(thread))
            .map(str::to_owned)
    }
}

/// Whether labels `labels` already show `kind` done.
fn done_by_labels(kind: ActionKind, labels: &[String]) -> bool {
    let has = |label: &str| labels.iter().any(|known| known == label);
    match kind {
        ActionKind::Trash => has("TRASH"),
        ActionKind::Spam => has("SPAM"),
        ActionKind::Archive => !has("INBOX"),
        ActionKind::MarkRead => !has("UNREAD"),
        ActionKind::Draft | ActionKind::Send => false,
    }
}

fn not_applied(retry: bool, code: OutcomeCode) -> Execution {
    Execution::NotApplied { retry, code }
}

#[async_trait]
impl MailEffects for GmailEffects {
    async fn save_draft(&mut self, approved: &Approved, message: &[u8]) -> Execution {
        let content = approved.content();
        let Some(outgoing) = &content.message else {
            return not_applied(false, OutcomeCode::Internal);
        };
        match self.find(&outgoing.message_id, false).await {
            Ok(true) => {
                return Execution::Applied {
                    code: OutcomeCode::AlreadyDone,
                    sent_copy: None,
                };
            }
            Ok(false) => {}
            Err(_) => return super::effects::unreachable(),
        }
        let Some(writer) = self.writer(approved, false) else {
            return not_applied(false, OutcomeCode::AuthFailed);
        };
        let mut draft = json!({ "raw": api::base64url(message) });
        if let Some(thread) = self.thread(content).await {
            draft["threadId"] = Value::String(thread);
        }
        api::classify(
            writer
                .request(
                    Method::Post,
                    &format!("{BASE}/drafts"),
                    &[],
                    Body::Json(json!({ "message": draft })),
                )
                .await,
            false,
        )
    }

    async fn change(&mut self, approved: &Approved) -> Execution {
        let content = approved.content();
        let Some(SourceRef::Gmail { id }) = content.source.as_ref().map(|source| &source.reference)
        else {
            return not_applied(false, OutcomeCode::Mismatch);
        };
        if !SourceRef::valid_api_id(id) {
            return not_applied(false, OutcomeCode::Internal);
        }
        match self.labels(id).await {
            Ok(None) => return not_applied(false, OutcomeCode::Gone),
            Ok(Some(labels)) if done_by_labels(content.kind, &labels) => {
                return Execution::Applied {
                    code: OutcomeCode::AlreadyDone,
                    sent_copy: None,
                };
            }
            Ok(Some(_)) => {}
            Err(_) => return super::effects::unreachable(),
        }
        let Some(writer) = self.writer(approved, false) else {
            return not_applied(false, OutcomeCode::AuthFailed);
        };
        let (path, body) = match content.kind {
            ActionKind::Trash => (format!("{BASE}/messages/{id}/trash"), Body::None),
            ActionKind::Spam => (
                format!("{BASE}/messages/{id}/modify"),
                Body::Json(json!({ "addLabelIds": ["SPAM"], "removeLabelIds": ["INBOX"] })),
            ),
            ActionKind::Archive => (
                format!("{BASE}/messages/{id}/modify"),
                Body::Json(json!({ "removeLabelIds": ["INBOX"] })),
            ),
            ActionKind::MarkRead => (
                format!("{BASE}/messages/{id}/modify"),
                Body::Json(json!({ "removeLabelIds": ["UNREAD"] })),
            ),
            ActionKind::Draft | ActionKind::Send => {
                return not_applied(false, OutcomeCode::Internal);
            }
        };
        api::classify(writer.request(Method::Post, &path, &[], body).await, false)
    }

    async fn send(&mut self, approved: &Approved, message: &[u8]) -> Execution {
        let Some(sender) = self.writer(approved, true) else {
            return not_applied(false, OutcomeCode::AuthFailed);
        };
        let mut body = json!({ "raw": api::base64url(message) });
        if let Some(thread) = self.thread(approved.content()).await {
            body["threadId"] = Value::String(thread);
        }
        api::classify(
            sender
                .request(
                    Method::Post,
                    &format!("{BASE}/messages/send"),
                    &[],
                    Body::Json(body),
                )
                .await,
            true,
        )
    }

    async fn copy_sent(&mut self, _approved: &Approved, _message: &[u8]) -> bool {
        // Gmail files sent mail by itself.
        true
    }

    async fn probe(&mut self, content: &ActionContent) -> Probe {
        match content.kind {
            ActionKind::Draft | ActionKind::Send => {
                let Some(outgoing) = &content.message else {
                    return Probe::Unknown;
                };
                let sent = content.kind == ActionKind::Send;
                match self.find(&outgoing.message_id, sent).await {
                    Ok(true) => Probe::Done,
                    Ok(false) if !sent => Probe::NotDone,
                    Ok(false) => Probe::Unknown,
                    Err(_) => Probe::Unreachable,
                }
            }
            kind => {
                let Some(SourceRef::Gmail { id }) =
                    content.source.as_ref().map(|source| &source.reference)
                else {
                    return Probe::Unknown;
                };
                match self.labels(id).await {
                    Ok(None) => Probe::Gone,
                    Ok(Some(labels)) if done_by_labels(kind, &labels) => Probe::Done,
                    Ok(Some(_)) => Probe::NotDone,
                    Err(_) => Probe::Unreachable,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
