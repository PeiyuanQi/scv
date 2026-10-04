//! HTTP for the mail APIs (Gmail and Microsoft Graph): bounded requests
//! with a grant's access token, under a request guard.
//!
//! The guard is the API's counterpart of the IMAP and SMTP guards: every
//! request is checked before it is sent against an allowlist of methods and
//! path templates. A reading client may only `GET` the listed paths. A
//! writing client, made for one approved action, may additionally send
//! exactly that action's request: its method, its path with the approved
//! message's ID and nothing else, and for label, move, and flag changes a
//! body of exactly the approved shape. Redirects are never followed,
//! answers are bounded, and neither a token nor the provider's error text
//! reaches a log. A `GET` the provider answers with HTTP 401 drops the
//! cached token and is tried once more. A `POST` or `PATCH` is not: Gmail
//! and Graph do not document a key that makes a repeat the same change, so
//! that 401 is [`Failure::Uncertain`]. The change may have been accepted
//! before the answer, and it is not sent again.

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use super::content::ActionKind;
use super::oauth::{TokenError, TokenSource};

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest answer read.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Where the providers' APIs are; tests point them at local fakes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origins {
    pub(crate) gmail: String,
    pub(crate) graph: String,
}

impl Default for Origins {
    fn default() -> Self {
        Self {
            gmail: "https://gmail.googleapis.com".into(),
            graph: "https://graph.microsoft.com".into(),
        }
    }
}

/// What a client may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Read only.
    Read,
    /// Read, and the request of one approved action.
    Write(Targets),
}

/// The approved action's kind and the message it acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Targets {
    pub(crate) kind: ActionKind,
    /// The provider's ID of the message acted on.
    pub(crate) id: Option<String>,
    /// For a Graph move, the well-known folder it goes to.
    pub(crate) destination: Option<String>,
}

/// A request's method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Patch,
}

/// A request's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Body {
    None,
    Json(Value),
    /// Graph's MIME upload: base64 of the message as `text/plain`.
    Mime(String),
}

/// A request the guard refused; `what` is its method and path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiViolation {
    pub(crate) what: String,
}

impl std::fmt::Display for ApiViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "refused to send a mail API request ({}) this client may not send",
            self.what
        )
    }
}

impl std::error::Error for ApiViolation {}

/// Read-only paths, `{id}` standing for a provider ID and `{folder}` for a
/// well-known folder name.
const READ_PATHS: &[&str] = &[
    "/gmail/v1/users/me/profile",
    "/gmail/v1/users/me/history",
    "/gmail/v1/users/me/messages",
    "/gmail/v1/users/me/messages/{id}",
    "/gmail/v1/users/me/messages/{id}/attachments/{id}",
    "/v1.0/me",
    "/v1.0/me/mailFolders/{folder}",
    "/v1.0/me/mailFolders/{folder}/messages",
    "/v1.0/me/messages/{id}",
    "/v1.0/me/messages/{id}/attachments",
];

const GRAPH_FOLDERS: &[&str] = &[
    "inbox",
    "drafts",
    "sentitems",
    "deleteditems",
    "junkemail",
    "archive",
];

/// Whether `path` matches `template`, and the IDs it holds.
fn matches<'a>(template: &str, path: &'a str) -> Option<Vec<&'a str>> {
    let mut ids = Vec::new();
    let mut template_parts = template.split('/');
    let mut path_parts = path.split('/');
    loop {
        match (template_parts.next(), path_parts.next()) {
            (None, None) => return Some(ids),
            (Some("{id}"), Some(part)) if super::source::SourceRef::valid_api_id(part) => {
                ids.push(part);
            }
            (Some("{folder}"), Some(part)) if GRAPH_FOLDERS.contains(&part) => {
                ids.push(part);
            }
            (Some(expected), Some(part)) if expected == part => {}
            _ => return None,
        }
    }
}

/// Check one request against `mode`.
pub(crate) fn check(
    mode: &Mode,
    method: Method,
    path: &str,
    body: &Body,
) -> Result<(), ApiViolation> {
    let refuse = || ApiViolation {
        what: format!("{method:?} {}", path.split('?').next().unwrap_or_default()),
    };
    if path.contains("..") || path.contains('?') || path.contains('#') {
        return Err(refuse());
    }
    if method == Method::Get
        && *body == Body::None
        && READ_PATHS
            .iter()
            .any(|template| matches(template, path).is_some())
    {
        return Ok(());
    }
    let Mode::Write(targets) = mode else {
        return Err(refuse());
    };
    let bound = |template: &str| {
        matches(template, path)
            .is_some_and(|ids| ids.len() == 1 && targets.id.as_deref() == Some(ids[0]))
    };
    let labels = |value: &Value, add: &[&str], remove: &[&str]| {
        let list = |key: &str| -> Option<Vec<&str>> {
            match value.get(key) {
                None => Some(Vec::new()),
                Some(Value::Array(items)) => items.iter().map(Value::as_str).collect(),
                Some(_) => None,
            }
        };
        let same =
            |got: Option<Vec<&str>>, expected: &[&str]| got.is_some_and(|got| got == expected);
        value.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| key == "addLabelIds" || key == "removeLabelIds")
        }) && same(list("addLabelIds"), add)
            && same(list("removeLabelIds"), remove)
    };
    let gmail_message = |value: &Value| {
        value.as_object().is_some_and(|object| {
            object.get("raw").is_some_and(Value::is_string)
                && object.get("threadId").is_none_or(Value::is_string)
                && object.keys().all(|key| key == "raw" || key == "threadId")
        })
    };
    let gmail = path.starts_with("/gmail/");
    let allowed = match (gmail, targets.kind, method, body) {
        (true, ActionKind::Draft, Method::Post, Body::Json(value)) => {
            path == "/gmail/v1/users/me/drafts"
                && value.as_object().is_some_and(|object| object.len() == 1)
                && gmail_message(&value["message"])
        }
        (true, ActionKind::Send, Method::Post, Body::Json(value)) => {
            path == "/gmail/v1/users/me/messages/send" && gmail_message(value)
        }
        (true, ActionKind::Trash, Method::Post, Body::None) => {
            bound("/gmail/v1/users/me/messages/{id}/trash")
        }
        (true, ActionKind::Spam, Method::Post, Body::Json(value)) => {
            bound("/gmail/v1/users/me/messages/{id}/modify") && labels(value, &["SPAM"], &["INBOX"])
        }
        (true, ActionKind::Archive, Method::Post, Body::Json(value)) => {
            bound("/gmail/v1/users/me/messages/{id}/modify") && labels(value, &[], &["INBOX"])
        }
        (true, ActionKind::MarkRead, Method::Post, Body::Json(value)) => {
            bound("/gmail/v1/users/me/messages/{id}/modify") && labels(value, &[], &["UNREAD"])
        }
        (false, ActionKind::Draft, Method::Post, Body::Mime(_)) => path == "/v1.0/me/messages",
        (false, ActionKind::Send, Method::Post, Body::Mime(_)) => path == "/v1.0/me/sendMail",
        (
            false,
            ActionKind::Archive | ActionKind::Trash | ActionKind::Spam,
            Method::Post,
            Body::Json(value),
        ) => {
            bound("/v1.0/me/messages/{id}/move")
                && value.as_object().is_some_and(|object| object.len() == 1)
                && targets
                    .destination
                    .as_deref()
                    .is_some_and(|destination| value["destinationId"].as_str() == Some(destination))
        }
        (false, ActionKind::MarkRead, Method::Patch, Body::Json(value)) => {
            bound("/v1.0/me/messages/{id}")
                && value.as_object().is_some_and(|object| object.len() == 1)
                && value["isRead"] == Value::Bool(true)
        }
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        tracing::error!(request = %refuse().what, "refused a mail API request");
        Err(refuse())
    }
}

/// A provider's answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

impl Reply {
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Why a request failed before an answer came.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The guard refused it: nothing was sent.
    Refused(ApiViolation),
    /// No access token: nothing was sent.
    Token(TokenError),
    /// It never reached the provider: nothing happened there.
    NotSent(anyhow::Error),
    /// It may have reached the provider: its effect is unknown.
    Lost(anyhow::Error),
    /// A `POST` or `PATCH` was answered HTTP 401. The change may have been
    /// accepted before that answer, so it must not be sent again.
    Uncertain(anyhow::Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(violation) => write!(formatter, "{violation}"),
            Self::Token(error) => write!(formatter, "{error}"),
            Self::NotSent(error) | Self::Lost(error) | Self::Uncertain(error) => {
                write!(formatter, "{error:#}")
            }
        }
    }
}

impl std::error::Error for Failure {}

/// Which API a client speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Gmail,
    /// Graph wants immutable IDs and text bodies asked for.
    Graph,
}

/// A client of one API with one grant.
pub(crate) struct Api {
    http: reqwest::Client,
    origin: String,
    tokens: Arc<TokenSource>,
    mode: Mode,
    flavor: Flavor,
}

impl Api {
    pub(crate) fn new(
        flavor: Flavor,
        origin: String,
        tokens: Arc<TokenSource>,
        mode: Mode,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            origin: origin.trim_end_matches('/').to_owned(),
            tokens,
            mode,
            flavor,
        })
    }

    /// `GET path?query`; a failure is an error.
    pub(crate) async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Reply> {
        self.request(Method::Get, path, query, Body::None)
            .await
            .map_err(|failure| anyhow!(failure))
    }

    /// Send one request. A `GET` the provider answers with HTTP 401 drops the
    /// cached token and is tried once more. A `POST` or `PATCH` is not, even
    /// when the caller could name a key: Gmail and Graph do not document one
    /// that makes the repeat the same change. That 401 is
    /// [`Failure::Uncertain`].
    pub(crate) async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Body,
    ) -> std::result::Result<Reply, Failure> {
        check(&self.mode, method, path, &body).map_err(Failure::Refused)?;
        // A mutation may have been accepted before the 401 came back.
        // Refreshing and sending it again could do it twice.
        let refresh = method == Method::Get;
        let attempts = if refresh { 2 } else { 1 };
        for attempt in 0..attempts {
            let token = self.tokens.token().await.map_err(Failure::Token)?;
            let mut url = reqwest::Url::parse(&format!("{}{path}", self.origin))
                .map_err(|_| Failure::NotSent(anyhow!("a mail API address did not parse")))?;
            if !query.is_empty() {
                url.query_pairs_mut().extend_pairs(query);
            }
            let mut request = match method {
                Method::Get => self.http.get(url),
                Method::Post => self.http.post(url),
                Method::Patch => self.http.patch(url),
            }
            .bearer_auth(token.expose());
            if self.flavor == Flavor::Graph {
                request = request.header(
                    "Prefer",
                    "IdType=\"ImmutableId\", outlook.body-content-type=\"text\"",
                );
            }
            request = match &body {
                Body::None => request,
                Body::Json(value) => request.json(value),
                Body::Mime(base64) => request
                    .header("Content-Type", "text/plain")
                    .body(base64.clone()),
            };
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) if error.is_connect() || error.is_builder() => {
                    return Err(Failure::NotSent(anyhow!("could not reach the mail API")));
                }
                Err(_) => return Err(Failure::Lost(anyhow!("the mail API did not answer"))),
            };
            let status = response.status().as_u16();
            if status == 401 {
                self.tokens.forget().await;
                if refresh && attempt + 1 < attempts {
                    continue;
                }
                if !refresh {
                    return Err(Failure::Uncertain(anyhow!(
                        "the mail API refused the access token; whether the change happened is \
                         unknown"
                    )));
                }
                break;
            }
            let body = read(response).await.map_err(Failure::Lost)?;
            return Ok(Reply { status, body });
        }
        Err(Failure::Token(TokenError::SignIn(anyhow!(
            "the mail API refused SCV's access token"
        ))))
    }
}

/// A bounded answer body as JSON; an empty or non-JSON body is `null`.
async fn read(mut response: reqwest::Response) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("the mail API's answer was too large");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("the mail API's answer broke off"))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            bail!("the mail API's answer was too large");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// What the executor's API effects are made from.
#[derive(Clone)]
pub(crate) struct EffectParts {
    pub(crate) origin: String,
    pub(crate) reader: Arc<TokenSource>,
    pub(crate) writer: Option<Arc<TokenSource>>,
    pub(crate) sender: Option<Arc<TokenSource>>,
}

/// What a write request did, as the executor records it: a 2xx happened; a
/// 404 found the message gone; 429 and, except for a send, 5xx did not
/// happen and may be retried; other 4xx are final; for a send a 5xx, and
/// for anything a request lost on the way, may have happened. A mutation
/// answered HTTP 401 is `Uncertain`: it may have happened, and it is not
/// tried again.
pub(crate) fn classify(
    result: std::result::Result<Reply, Failure>,
    send: bool,
) -> super::ledger::actions::Execution {
    use super::ledger::actions::{Execution, OutcomeCode};
    let not_applied = |retry, code| Execution::NotApplied { retry, code };
    match result {
        Ok(reply) if reply.ok() => Execution::Applied {
            code: OutcomeCode::Applied,
            sent_copy: None,
        },
        Ok(reply) if reply.status == 404 => not_applied(false, OutcomeCode::Gone),
        Ok(reply) if reply.status == 401 || reply.status == 403 => {
            not_applied(false, OutcomeCode::AuthFailed)
        }
        Ok(reply) if reply.status == 429 => not_applied(true, OutcomeCode::Refused),
        Ok(reply) if reply.status >= 500 && send => Execution::Ambiguous,
        Ok(reply) if reply.status >= 500 => not_applied(true, OutcomeCode::Refused),
        Ok(_) => not_applied(false, OutcomeCode::Refused),
        Err(Failure::Refused(_)) => not_applied(false, OutcomeCode::Internal),
        Err(Failure::Token(TokenError::SignIn(_))) => not_applied(false, OutcomeCode::AuthFailed),
        Err(Failure::Token(TokenError::Unavailable(_)) | Failure::NotSent(_)) => {
            not_applied(true, OutcomeCode::Unreachable)
        }
        Err(Failure::Lost(_)) => Execution::Ambiguous,
        Err(Failure::Uncertain(_)) => Execution::Uncertain,
    }
}

/// `bytes` as base64url without padding, as Gmail's `raw` takes it.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Gmail's base64url body data, padded or not, decoded.
pub(crate) fn from_base64url(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let trimmed = text.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()
}

#[cfg(test)]
mod tests;
