//! The Slack Web API: bounded, redirect-free HTTPS requests to Slack's own
//! hosts. Failures never carry a token, a URL, or text Slack wrote, only
//! Slack's error code when it is a plain identifier.

use super::{Outbound, credentials, inbound};
use crate::retry::Attempt;
use anyhow::{Result, anyhow, bail};
use reqwest::StatusCode;
use scv_client::Secret;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use tokio::{sync::Mutex, time::Instant};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long downloading or uploading one file may take.
const FILE_TIMEOUT: Duration = Duration::from_secs(120);
/// Messages one history page lists.
const PAGE_SIZE: &str = "50";
/// Error codes that mean Slack could not handle the request this time.
const TRANSIENT: [&str; 5] = [
    "ratelimited",
    "internal_error",
    "fatal_error",
    "service_unavailable",
    "request_timeout",
];
/// The bot token scopes SCV uses; login names any the token lacks.
pub(super) const BOT_SCOPES: [&str; 9] = [
    "app_mentions:read",
    "channels:history",
    "chat:write",
    "files:read",
    "files:write",
    "groups:history",
    "im:history",
    "im:write",
    "users:read",
];

/// The installation a bot token belongs to.
#[derive(Clone)]
pub(super) struct Identity {
    pub(super) team_id: String,
    pub(super) app_id: String,
    pub(super) bot_user_id: String,
    /// The token's scopes, as `auth.test` listed them, when it did.
    pub(super) scopes: Option<String>,
}

/// Slack answered `ok: false` with an error the same request cannot get
/// past, such as a missing scope or a channel the bot is not in, or with an
/// HTTP 4xx status (`http_<status>`).
#[derive(Debug)]
pub(super) struct Refusal(String);

impl Refusal {
    /// Slack rejected the token itself: nothing works until the owner signs
    /// in again, so this is no refusal of one request.
    pub(super) fn is_auth(&self) -> bool {
        AUTH_ERRORS.contains(&self.0.as_str())
    }
}

/// Error codes that mean a token no longer works.
const AUTH_ERRORS: [&str; 5] = [
    "invalid_auth",
    "not_authed",
    "token_revoked",
    "token_expired",
    "account_inactive",
];

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let note = match self.0.as_str() {
            "missing_scope" => "the token is missing a scope; see the setup guide",
            code if AUTH_ERRORS.contains(&code) => {
                "Slack rejected the token; sign in again with valid tokens"
            }
            "not_allowed_token_type" => {
                "wrong token type; use xapp- for Socket Mode and xoxb- for the bot"
            }
            "not_in_channel" | "channel_not_found" | "is_archived" => {
                "the bot cannot reach this conversation; invite it and check access"
            }
            _ => "Slack refused the request",
        };
        write!(formatter, "{note} ({})", self.0)
    }
}

impl std::error::Error for Refusal {}

/// A history page, newest first for a conversation and oldest first for a
/// thread, as Slack lists them.
pub(super) struct Page {
    pub(super) messages: Vec<Value>,
    pub(super) next: Option<String>,
}

enum Verb {
    Get,
    Post,
}

pub(super) struct Api {
    client: reqwest::Client,
    bot_token: Secret,
    app_token: Secret,
    /// The bot token's installation, until Slack rejects the token.
    identity: Mutex<Option<Identity>>,
    /// Methods Slack rate limited, until when.
    cooldowns: Mutex<HashMap<&'static str, Instant>>,
    /// The Web API origin, `https://slack.com/api`.
    origin: String,
    /// Production requires TLS on Slack's hosts; local fakes in tests do not.
    tls: bool,
}

impl Api {
    pub(super) fn new(bot_token: Secret, app_token: Secret) -> Result<Self> {
        credentials::validate_tokens(&bot_token, &app_token)?;
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(10))
                .https_only(true)
                .build()?,
            bot_token,
            app_token,
            identity: Mutex::new(None),
            cooldowns: Mutex::new(HashMap::new()),
            origin: "https://slack.com/api".into(),
            tls: true,
        })
    }

    /// Fake services on one loopback origin, for tests.
    #[cfg(test)]
    pub(super) fn local(origin: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            origin: origin.into(),
            tls: false,
            ..Self::new("xoxb-test".into(), "xapp-test".into()).unwrap()
        }
    }

    /// Call `method` with the app-level token when `app`, else the bot
    /// token. An `ok: false` answer is a [`Refusal`] unless it is transient.
    async fn call(
        &self,
        method: &'static str,
        app: bool,
        verb: Verb,
        params: &[(&str, &str)],
    ) -> Result<Value> {
        Ok(self.request(method, app, verb, params).await?.0)
    }

    /// As [`Api::call`], with the token's scopes when Slack listed them.
    async fn request(
        &self,
        method: &'static str,
        app: bool,
        verb: Verb,
        params: &[(&str, &str)],
    ) -> Result<(Value, Option<String>)> {
        if self
            .cooldowns
            .lock()
            .await
            .get(method)
            .is_some_and(|until| *until > Instant::now())
        {
            bail!("Slack rate limit cooldown for {method}; retrying after Retry-After")
        }
        let token = if app {
            &self.app_token
        } else {
            &self.bot_token
        };
        let url = format!("{}/{method}", self.origin);
        let request = match verb {
            Verb::Get => self.client.get(url).query(params),
            Verb::Post => self.client.post(url).form(params),
        };
        let response = request
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|_| anyhow!("Slack HTTPS request failed (network or timeout)"))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(60)
                .clamp(1, 3600);
            self.cooldowns.lock().await.insert(
                method,
                Instant::now() + Duration::from_secs(u64::from(seconds)),
            );
            bail!("Slack rate limited {method}; respecting Retry-After")
        }
        if status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT {
            return Err(Refusal(format!("http_{}", status.as_u16())).into());
        }
        if !status.is_success() {
            bail!("Slack {method} failed with status {}", status.as_u16())
        }
        let scopes = response
            .headers()
            .get("x-oauth-scopes")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let value = read_json(response).await?;
        if value["ok"] != true {
            return match value["error"].as_str() {
                Some(code) if !TRANSIENT.contains(&code) => {
                    let refusal = Refusal(safe_code(code));
                    // Checked again before the next receive or send.
                    if refusal.is_auth() && !app {
                        *self.identity.lock().await = None;
                    }
                    Err(refusal.into())
                }
                _ => bail!("Slack could not handle {method} this time"),
            };
        }
        Ok((value, scopes))
    }

    /// The bot token's installation, asked of Slack once per run and again
    /// after Slack rejects the token.
    pub(super) async fn identity(&self) -> Result<Identity> {
        if let Some(identity) = self.identity.lock().await.clone() {
            return Ok(identity);
        }
        let (auth, scopes) = self.request("auth.test", false, Verb::Post, &[]).await?;
        let team_id = id(&auth, "team_id", "T")?;
        let bot_user_id = id(&auth, "user_id", "UW")?;
        let bot_id = id(&auth, "bot_id", "B")?;
        // auth.test has no app ID; bots.info, with users:read, does.
        let bot = self
            .call("bots.info", false, Verb::Get, &[("bot", &bot_id)])
            .await?;
        let app_id = id(&bot["bot"], "app_id", "A")?;
        if id(&bot["bot"], "user_id", "UW")? != bot_user_id {
            bail!("Slack returned inconsistent bot identity")
        }
        let identity = Identity {
            team_id,
            app_id,
            bot_user_id,
            scopes,
        };
        *self.identity.lock().await = Some(identity.clone());
        Ok(identity)
    }

    /// A fresh Socket Mode URL, which `apps.connections.open` gives the
    /// app-level token.
    pub(super) async fn socket_url(&self) -> Result<reqwest::Url> {
        let value = self
            .call("apps.connections.open", true, Verb::Post, &[])
            .await
            .map_err(|error| {
                anyhow!(
                    "Slack Socket Mode unavailable: {error}. Enable Socket Mode, and check the \
                     xapp- token's connections:write scope and network access"
                )
            })?;
        let raw = value["url"]
            .as_str()
            .ok_or_else(|| anyhow!("Slack Socket Mode response omitted the URL"))?;
        self.trusted(raw, "wss")
    }

    /// Post one text part with `chat.postMessage`.
    pub(super) async fn post(&self, message: &Outbound<'_>) -> Attempt<()> {
        let Ok(params) = post_params(message) else {
            return Attempt::Refused("invalid Slack reply handle".into());
        };
        let params: Vec<_> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        attempt(
            self.call("chat.postMessage", false, Verb::Post, &params)
                .await
                .map(drop),
        )
    }

    /// Messages in `channel` after `oldest` (microseconds), newest first.
    pub(super) async fn history(
        &self,
        channel: &str,
        oldest: u64,
        cursor: Option<&str>,
    ) -> Result<Page> {
        let oldest = inbound::ts(oldest);
        let mut params = vec![
            ("channel", channel),
            ("oldest", oldest.as_str()),
            ("inclusive", "false"),
            ("limit", PAGE_SIZE),
        ];
        params.extend(cursor.map(|cursor| ("cursor", cursor)));
        page(
            self.call("conversations.history", false, Verb::Get, &params)
                .await?,
        )
    }

    /// Replies in the thread on `root` after `oldest` (microseconds), oldest
    /// first; Slack may list the root too.
    pub(super) async fn replies(
        &self,
        channel: &str,
        root: &str,
        oldest: u64,
        cursor: Option<&str>,
    ) -> Result<Page> {
        let oldest = inbound::ts(oldest);
        let mut params = vec![
            ("channel", channel),
            ("ts", root),
            ("oldest", oldest.as_str()),
            ("inclusive", "false"),
            ("limit", PAGE_SIZE),
        ];
        params.extend(cursor.map(|cursor| ("cursor", cursor)));
        page(
            self.call("conversations.replies", false, Verb::Get, &params)
                .await?,
        )
    }

    /// The message `ts` in `channel`, such as the root a thread is on.
    pub(super) async fn message(&self, channel: &str, ts: &str) -> Result<Value> {
        let params = [
            ("channel", channel),
            ("ts", ts),
            ("inclusive", "true"),
            ("limit", "1"),
        ];
        let value = self
            .call("conversations.replies", false, Verb::Get, &params)
            .await?;
        value["messages"]
            .as_array()
            .and_then(|messages| messages.iter().find(|message| message["ts"] == ts))
            .cloned()
            .ok_or_else(|| anyhow!("Slack did not return the message"))
    }

    /// The bot's direct conversation with `user`, which file shares need.
    pub(super) async fn open_direct(&self, user: &str) -> Result<String> {
        let value = self
            .call("conversations.open", false, Verb::Post, &[("users", user)])
            .await?;
        id(&value["channel"], "id", "D")
    }

    /// Upload `bytes` as `name` through `files.getUploadURLExternal` and the
    /// URL it returns, giving the file's ID to share.
    pub(super) async fn upload(&self, name: &str, bytes: Vec<u8>) -> Attempt<String> {
        let length = bytes.len().to_string();
        let params = [("filename", name), ("length", length.as_str())];
        let value = match self
            .call("files.getUploadURLExternal", false, Verb::Post, &params)
            .await
        {
            Ok(value) => value,
            Err(error) => return attempt(Err(error)),
        };
        let (Some(url), Ok(file_id)) = (value["upload_url"].as_str(), id(&value, "file_id", "F"))
        else {
            return Attempt::Retry("Slack's upload response was incomplete".into());
        };
        let Ok(url) = self.trusted(url, "https") else {
            return Attempt::Refused("Slack returned an untrusted upload URL".into());
        };
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .timeout(FILE_TIMEOUT)
            .send()
            .await;
        match response.map(|response| response.status()) {
            Ok(status) if status.is_success() => Attempt::Done(file_id),
            Ok(status)
                if status.is_client_error()
                    && status != StatusCode::REQUEST_TIMEOUT
                    && status != StatusCode::TOO_MANY_REQUESTS =>
            {
                Attempt::Refused(format!("Slack refused the upload with status {status}"))
            }
            Ok(status) => Attempt::Retry(format!("Slack upload failed with status {status}")),
            Err(_) => Attempt::Retry("Slack upload failed (network or timeout)".into()),
        }
    }

    /// Share uploaded file `file_id` in `channel`, inside the thread on
    /// `thread` when given.
    pub(super) async fn share(
        &self,
        file_id: &str,
        title: &str,
        channel: &str,
        thread: Option<&str>,
    ) -> Attempt<()> {
        let files = json!([{"id": file_id, "title": title}]).to_string();
        let mut params = vec![("files", files.as_str()), ("channel_id", channel)];
        params.extend(thread.map(|thread| ("thread_ts", thread)));
        attempt(
            self.call("files.completeUploadExternal", false, Verb::Post, &params)
                .await
                .map(drop),
        )
    }

    /// Download a file a message carries, at most `max_bytes`, with the
    /// bot token, which only ever goes to Slack's hosts. `html` says the
    /// file itself is a web page; otherwise an HTML answer is Slack's
    /// sign-in page, which means the token cannot read files.
    pub(super) async fn download(
        &self,
        url: &str,
        max_bytes: u64,
        html: bool,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let url = self.trusted(url, "https")?;
        let mut response = self
            .client
            .get(url)
            .bearer_auth(self.bot_token.expose())
            .timeout(FILE_TIMEOUT)
            .send()
            .await
            .map_err(|_| anyhow!("Slack file download failed (network or timeout)"))?;
        // Slack sends a token that cannot read files to its sign-in page.
        if response.status().is_redirection() {
            bail!(
                "Slack redirected the download to its sign-in page; add the files:read scope \
                 and reinstall the app"
            )
        }
        if !response.status().is_success() {
            bail!(
                "Slack file download failed with status {}",
                response.status().as_u16()
            )
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !html
            && mime
                .as_deref()
                .is_some_and(|mime| mime.starts_with("text/html"))
        {
            bail!(
                "Slack served a sign-in page instead of the file; add the files:read scope and \
                 reinstall the app"
            )
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            bail!("file is larger than the limit")
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("Slack file download was interrupted"))?
        {
            if (body.len() + chunk.len()) as u64 > max_bytes {
                bail!("file is larger than the limit")
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, mime))
    }

    /// `raw` if it is on Slack's hosts over TLS (`secure`, `https` or
    /// `wss`) on the standard port, without credentials in the authority.
    fn trusted(&self, raw: &str, secure: &str) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(raw).map_err(|_| anyhow!("invalid Slack URL"))?;
        let host = url.host_str().unwrap_or_default();
        let placed = if self.tls {
            url.scheme() == secure
                && host.ends_with(".slack.com")
                && url.port_or_known_default() == Some(443)
        } else {
            let plain = if secure == "wss" { "ws" } else { "http" };
            url.scheme() == plain && host == "127.0.0.1"
        };
        if !placed
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!("Slack returned an untrusted URL")
        }
        Ok(url)
    }
}

/// A call's result as a send attempt: a refusal is final, anything else
/// worth retrying. A rejected token is retried too, so the reply waits for a
/// new sign-in instead of being held.
pub(super) fn attempt<T>(result: Result<T>) -> Attempt<T> {
    match result {
        Ok(value) => Attempt::Done(value),
        Err(error) => match error.downcast_ref::<Refusal>() {
            Some(refusal) if !refusal.is_auth() => Attempt::Refused(error.to_string()),
            _ => Attempt::Retry(error.to_string()),
        },
    }
}

/// Slack's error code if it is a plain identifier, which cannot carry a
/// token; anything else is `unknown`.
fn safe_code(code: &str) -> String {
    if (1..=64).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        code.into()
    } else {
        "unknown".into()
    }
}

fn id(value: &Value, field: &str, prefixes: &str) -> Result<String> {
    value[field]
        .as_str()
        .filter(|id| inbound::valid_id(id, prefixes))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Slack's response omitted a valid {field}"))
}

fn page(value: Value) -> Result<Page> {
    let messages = value["messages"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("Slack's history response omitted messages"))?;
    let next = value["response_metadata"]["next_cursor"]
        .as_str()
        .filter(|cursor| (1..=1024).contains(&cursor.len()))
        .map(str::to_owned);
    Ok(Page { messages, next })
}

/// `chat.postMessage`'s form for one part. Slack's own markup is escaped
/// and automatic parsing, mentions, and unfurls are off, so model output
/// cannot notify anyone (`<!channel>`, `<@U…>`) or fetch a link preview.
fn post_params(message: &Outbound<'_>) -> Result<Vec<(&'static str, String)>> {
    let (channel, thread) = inbound::target(message.reply_to, message.to)?;
    let text = message
        .text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut params = vec![
        ("channel", channel.to_owned()),
        ("text", text),
        ("parse", "none".to_owned()),
        ("link_names", "false".to_owned()),
        ("unfurl_links", "false".to_owned()),
        ("unfurl_media", "false".to_owned()),
    ];
    params.extend(thread.map(|thread| ("thread_ts", thread.to_owned())));
    Ok(params)
}

async fn read_json(mut response: reqwest::Response) -> Result<Value> {
    // Too large stays too large: refused, so catch-up skips that listing.
    let too_large = || Refusal("response_too_large".into());
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large().into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("Slack's response was interrupted"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(too_large().into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("Slack returned invalid JSON"))
}

#[cfg(test)]
mod tests;
