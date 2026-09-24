//! Feishu Open Platform HTTP calls: the app's tenant token, sending
//! messages, reading chat history, and the long connection's endpoint.
//! Only the brand's own hosts are contacted, redirects are never followed,
//! and response bodies are bounded.

use crate::state::Brand;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A tenant token lives two hours; renew it well before that.
const TOKEN_MARGIN: Duration = Duration::from_secs(10 * 60);
/// Codes that mean the tenant token is missing, invalid, or expired.
const TOKEN_CODES: [i64; 4] = [99991661, 99991663, 99991664, 99991668];
/// Codes that mean "slow down": the same request can succeed later.
const RATE_CODES: [i64; 4] = [99991400, 230020, 11232, 11233];

/// Where one brand's services live, and which hosts its long connection may
/// use.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The Open Platform API origin, such as `https://open.feishu.cn`.
    pub open: String,
    /// The accounts origin that runs app registration.
    pub accounts: String,
    /// The long connection's host must be this domain or below it.
    socket_domain: String,
    /// Production requires TLS; local fakes in tests do not.
    tls: bool,
}

impl Endpoints {
    pub fn for_brand(brand: Brand) -> Self {
        let (open, accounts, domain) = match brand {
            Brand::Feishu => (
                "https://open.feishu.cn",
                "https://accounts.feishu.cn",
                "feishu.cn",
            ),
            Brand::Lark => (
                "https://open.larksuite.com",
                "https://accounts.larksuite.com",
                "larksuite.com",
            ),
        };
        Self {
            open: open.into(),
            accounts: accounts.into(),
            socket_domain: domain.into(),
            tls: true,
        }
    }

    /// Fake services on one local origin, for tests.
    #[cfg(test)]
    pub(crate) fn local(origin: &str) -> Self {
        Self {
            open: origin.into(),
            accounts: origin.into(),
            socket_domain: "127.0.0.1".into(),
            tls: false,
        }
    }

    /// Accept only a long-connection URL on the brand's domain over TLS on
    /// the standard port, without credentials in the authority.
    pub fn check_socket_url(&self, url: &str) -> Result<reqwest::Url> {
        let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("invalid Feishu socket URL"))?;
        let host = parsed.host_str().unwrap_or_default();
        let on_domain =
            host == self.socket_domain || host.ends_with(&format!(".{}", self.socket_domain));
        let transport_ok = if self.tls {
            parsed.scheme() == "wss" && parsed.port_or_known_default() == Some(443)
        } else {
            parsed.scheme() == "ws"
        };
        if !on_domain
            || !transport_ok
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            bail!("Feishu returned an untrusted long-connection host")
        }
        Ok(parsed)
    }
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Read a bounded JSON body whatever the status: Feishu explains errors in
/// the body of 4xx responses. Server diagnostics are never returned.
pub async fn read_json(mut response: reqwest::Response) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("Feishu response exceeds limit")
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("Feishu response exceeds limit")
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| anyhow!("invalid Feishu response"))
}

fn code(value: &Value) -> Option<i64> {
    value.get("code").and_then(Value::as_i64)
}

/// The outcome of one send attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Attempt {
    Delivered,
    /// Feishu refused the message; resending the same request cannot help.
    Refused(String),
    /// A transport, rate, token, or server failure worth retrying.
    Retry(String),
}

/// One page of a chat's history, oldest first.
pub struct HistoryPage {
    pub items: Vec<Value>,
    pub next: Option<String>,
}

/// A failure the Open Platform reported with a code, as opposed to a
/// transport failure.
#[derive(Debug)]
pub struct Refusal(pub i64);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Feishu refused the request (code {})", self.0)
    }
}

impl std::error::Error for Refusal {}

/// One app's authenticated Open Platform client.
pub struct Api {
    client: reqwest::Client,
    endpoints: Endpoints,
    app_id: String,
    app_secret: String,
    /// The tenant token and when to renew it.
    token: Mutex<Option<(String, Instant)>>,
}

impl Api {
    pub fn new(endpoints: Endpoints, app_id: &str, app_secret: &str) -> Result<Self> {
        Ok(Self {
            client: http_client()?,
            endpoints,
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            token: Mutex::new(None),
        })
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// A tenant token, fetched once and renewed before it expires.
    async fn tenant_token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if let Some((token, renew_at)) = cached.as_ref()
            && Instant::now() < *renew_at
        {
            return Ok(token.clone());
        }
        let response = self
            .client
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.endpoints.open
            ))
            .json(&json!({"app_id": self.app_id, "app_secret": self.app_secret}))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let status = response.status();
        let value = read_json(response).await?;
        match (
            code(&value),
            value.get("tenant_access_token").and_then(Value::as_str),
        ) {
            (Some(0), Some(token)) if !token.is_empty() => {
                let lifetime = value
                    .get("expire")
                    .and_then(Value::as_u64)
                    .map_or(Duration::from_secs(7200), Duration::from_secs);
                let renew_in = lifetime
                    .saturating_sub(TOKEN_MARGIN)
                    .max(Duration::from_secs(60));
                *cached = Some((token.to_owned(), Instant::now() + renew_in));
                Ok(token.to_owned())
            }
            (Some(code), _) if code != 0 => Err(Refusal(code).into()),
            _ => bail!(
                "Feishu token request failed with status {}",
                status.as_u16()
            ),
        }
    }

    async fn forget_token(&self) {
        *self.token.lock().await = None;
    }

    /// Check the app ID and secret by fetching a fresh tenant token.
    pub async fn validate(&self) -> Result<()> {
        self.forget_token().await;
        self.tenant_token().await.map(drop)
    }

    /// An authenticated call; `Ok` carries the response body and its code.
    async fn call(
        &self,
        request: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<(u16, Value)> {
        let token = self.tenant_token().await?;
        let response = request(&self.client, &self.endpoints.open)
            .bearer_auth(token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let status = response.status().as_u16();
        match read_json(response).await {
            Ok(value) => {
                if code(&value).is_some_and(|code| TOKEN_CODES.contains(&code)) {
                    self.forget_token().await;
                }
                Ok((status, value))
            }
            Err(error) if status >= 400 => {
                bail!("Feishu request failed with status {status}: {error}")
            }
            Err(error) => Err(error),
        }
    }

    /// The bot's own `open_id`, used to tell whether a group message
    /// mentions it.
    pub async fn bot_open_id(&self) -> Result<String> {
        let (_, value) = self
            .call(|client, open| client.get(format!("{open}/open-apis/bot/v3/info")))
            .await?;
        match code(&value) {
            Some(0) => value
                .pointer("/bot/open_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("Feishu bot info omitted the bot's open_id")),
            Some(code) => Err(Refusal(code).into()),
            None => bail!("invalid Feishu bot info"),
        }
    }

    /// Send one text message: a reply to `reply_to` when it is set, or else
    /// a new message to the user `to`. `uuid` makes resends of the same
    /// part idempotent for an hour.
    pub async fn send_text(&self, to: &str, reply_to: &str, text: &str, uuid: &str) -> Attempt {
        let body = json!({
            "msg_type": "text",
            "content": json!({"text": neutralize_mentions(text)}).to_string(),
            "uuid": uuid,
        });
        let result = if reply_to.is_empty() {
            let mut body = body;
            body["receive_id"] = to.into();
            self.call(move |client, open| {
                client
                    .post(format!("{open}/open-apis/im/v1/messages"))
                    .query(&[("receive_id_type", "open_id")])
                    .json(&body)
            })
            .await
        } else {
            let path = format!(
                "/open-apis/im/v1/messages/{}/reply",
                encode_segment(reply_to)
            );
            self.call(move |client, open| client.post(format!("{open}{path}")).json(&body))
                .await
        };
        classify(result)
    }

    /// One page of a chat's messages created from `start` to `end`, in
    /// Unix seconds, oldest first.
    pub async fn history(
        &self,
        chat_id: &str,
        start: u64,
        end: u64,
        page: Option<&str>,
    ) -> Result<HistoryPage> {
        let mut query = vec![
            ("container_id_type", "chat".to_owned()),
            ("container_id", chat_id.to_owned()),
            ("start_time", start.to_string()),
            ("end_time", end.to_string()),
            ("sort_type", "ByCreateTimeAsc".into()),
            ("page_size", "50".into()),
        ];
        if let Some(page) = page {
            query.push(("page_token", page.to_owned()));
        }
        let (_, value) = self
            .call(|client, open| {
                client
                    .get(format!("{open}/open-apis/im/v1/messages"))
                    .query(&query)
            })
            .await?;
        match code(&value) {
            Some(0) => {}
            Some(code) => return Err(Refusal(code).into()),
            None => bail!("invalid Feishu history response"),
        }
        let data = value.get("data").cloned().unwrap_or_default();
        let items = data
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let next = (data.get("has_more").and_then(Value::as_bool) == Some(true))
            .then(|| data.get("page_token").and_then(Value::as_str))
            .flatten()
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        Ok(HistoryPage { items, next })
    }

    /// The long connection's URL and the server's ping interval.
    pub async fn socket_endpoint(&self) -> Result<(reqwest::Url, Option<Duration>)> {
        let response = self
            .client
            .post(format!("{}/callback/ws/endpoint", self.endpoints.open))
            .header("locale", "zh")
            .json(&json!({"AppID": self.app_id, "AppSecret": self.app_secret}))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let status = response.status().as_u16();
        let value = read_json(response).await?;
        match code(&value) {
            Some(0) => {}
            Some(code) => return Err(Refusal(code).into()),
            None => bail!("Feishu socket endpoint failed with status {status}"),
        }
        let url = value
            .pointer("/data/URL")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Feishu socket endpoint omitted its URL"))?;
        let ping = value
            .pointer("/data/ClientConfig/PingInterval")
            .and_then(Value::as_u64)
            .filter(|seconds| (5..=3600).contains(seconds))
            .map(Duration::from_secs);
        Ok((self.endpoints.check_socket_url(url)?, ping))
    }
}

/// Decide what a send's response means for retrying it.
fn classify(result: Result<(u16, Value)>) -> Attempt {
    let (status, value) = match result {
        Ok(response) => response,
        Err(error) => {
            return match error.downcast_ref::<Refusal>() {
                // A token refusal (such as a wrong secret) may be fixed by a
                // new login; the bridge keeps the reply and retries later.
                Some(refusal) => Attempt::Retry(refusal.to_string()),
                None => Attempt::Retry(format!("{error:#}")),
            };
        }
    };
    match code(&value) {
        Some(0) => Attempt::Delivered,
        Some(code) if TOKEN_CODES.contains(&code) || RATE_CODES.contains(&code) => {
            Attempt::Retry(format!("code {code}"))
        }
        _ if status >= 500 || matches!(status, 408 | 429) => {
            Attempt::Retry(format!("HTTP status {status}"))
        }
        Some(code) => Attempt::Refused(format!("code {code}")),
        None if status >= 400 => Attempt::Refused(format!("HTTP status {status}")),
        None => Attempt::Retry("invalid Feishu response".into()),
    }
}

/// Feishu text messages turn `<at user_id="...">` into mentions, including
/// `@all`. Model output must never notify people, so a zero-width space
/// breaks every such tag.
pub fn neutralize_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('<') {
        out.push_str(&rest[..=index]);
        rest = &rest[index + 1..];
        if rest.len() >= 2 && rest.as_bytes()[..2].eq_ignore_ascii_case(b"at") {
            out.push('\u{200b}');
        }
    }
    out.push_str(rest);
    out
}

/// Percent-encode a path segment such as a message ID.
fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_urls_must_stay_on_the_brand_domain_over_tls() {
        let feishu = Endpoints::for_brand(Brand::Feishu);
        assert!(
            feishu
                .check_socket_url("wss://msg-frontier.feishu.cn/ws/v2?device_id=1&service_id=2")
                .is_ok()
        );
        for url in [
            "ws://msg-frontier.feishu.cn/ws/v2",
            "wss://msg-frontier.feishu.cn:8443/ws/v2",
            "wss://feishu.cn.attacker.test/ws/v2",
            "wss://attackerfeishu.cn/ws/v2",
            "wss://user:pass@msg-frontier.feishu.cn/ws/v2",
            "wss://msg-frontier.larksuite.com/ws/v2",
            "not a url",
        ] {
            assert!(feishu.check_socket_url(url).is_err(), "{url}");
        }
        let lark = Endpoints::for_brand(Brand::Lark);
        assert!(
            lark.check_socket_url("wss://msg-frontier.larksuite.com/ws/v2")
                .is_ok()
        );
    }

    #[test]
    fn send_outcomes_separate_refusals_from_retries() {
        let ok = |status: u16, body: Value| classify(Ok((status, body)));
        assert_eq!(ok(200, json!({"code": 0})), Attempt::Delivered);
        assert!(matches!(
            ok(400, json!({"code": 230002})),
            Attempt::Refused(_)
        ));
        assert!(matches!(
            ok(400, json!({"code": 99991400})),
            Attempt::Retry(_)
        ));
        assert!(matches!(
            ok(400, json!({"code": 99991663})),
            Attempt::Retry(_)
        ));
        assert!(matches!(ok(503, json!({"code": 1})), Attempt::Retry(_)));
        assert!(matches!(ok(429, json!({})), Attempt::Retry(_)));
        assert!(matches!(ok(404, json!({})), Attempt::Refused(_)));
        assert!(matches!(
            classify(Err(anyhow!("connection reset"))),
            Attempt::Retry(_)
        ));
    }

    #[test]
    fn mention_tags_are_broken_but_other_text_is_kept() {
        assert_eq!(
            neutralize_mentions(r#"hi <at user_id="all">all</at> and <AT id=1>"#),
            "hi <\u{200b}at user_id=\"all\">all</at> and <\u{200b}AT id=1>"
        );
        assert_eq!(
            neutralize_mentions("a < b <b>bold</b> 中文<"),
            "a < b <b>bold</b> 中文<"
        );
        assert_eq!(neutralize_mentions("<a"), "<a");
    }

    #[test]
    fn message_ids_are_path_encoded() {
        assert_eq!(encode_segment("om_x100b"), "om_x100b");
        assert_eq!(encode_segment("a/../b?c"), "a%2F..%2Fb%3Fc");
    }
}
