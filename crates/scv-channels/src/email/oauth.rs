//! OAuth for the Gmail API and Microsoft Graph: signing in, and access
//! tokens for the account's grants.
//!
//! Each grant is its own refresh token with its own scopes, requested only
//! when the owner signs in for it: the reader's can only read mail, and the
//! writer's and sender's exist only when asked for (`--write`, `--send`).
//! The code that reads never loads another grant. Gmail signs in with the
//! owner's own installed-app client, PKCE, and a loopback redirect; on a
//! host without a browser, the owner opens the link elsewhere and pastes
//! back the address the browser could not load. Microsoft signs in with the
//! device code flow. Refresh tokens are saved in the account's grants file,
//! apart from its credentials, and a rotated one replaces the old one there.
//! Access tokens live only in memory.
//!
//! No token, code, or secret reaches a log or an error: the providers'
//! error codes are shown, their descriptions are not.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use scv_client::Secret;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::credentials::{Grant, GrantKind, Grants};

/// How long one token request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long sign-in waits for the owner.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// An access token is renewed this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
/// The largest token response read.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Which provider a sign-in is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProvider {
    /// The Gmail API.
    Gmail,
    /// Microsoft Graph (Outlook.com, Microsoft 365).
    Graph,
}

/// The provider's sign-in and token addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoints {
    /// Gmail's authorization page, or Microsoft's device code endpoint.
    pub(crate) authorize: String,
    pub(crate) token: String,
}

impl Endpoints {
    pub(crate) fn for_provider(provider: OAuthProvider, tenant: &str) -> Self {
        match provider {
            OAuthProvider::Gmail => Self {
                authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token: "https://oauth2.googleapis.com/token".into(),
            },
            OAuthProvider::Graph => Self {
                authorize: format!(
                    "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode"
                ),
                token: format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
            },
        }
    }
}

/// The scopes one grant asks for.
pub(crate) fn scopes(provider: OAuthProvider, kind: GrantKind) -> &'static [&'static str] {
    match (provider, kind) {
        (OAuthProvider::Gmail, GrantKind::Reader) => {
            &["https://www.googleapis.com/auth/gmail.readonly"]
        }
        // Gmail has no scope that writes without sending, or sends alone
        // without composing: one writer grant does both.
        (OAuthProvider::Gmail, GrantKind::Writer | GrantKind::Sender) => {
            &["https://www.googleapis.com/auth/gmail.modify"]
        }
        (OAuthProvider::Graph, GrantKind::Reader) => &["offline_access", "User.Read", "Mail.Read"],
        (OAuthProvider::Graph, GrantKind::Writer) => &["offline_access", "Mail.ReadWrite"],
        (OAuthProvider::Graph, GrantKind::Sender) => &["offline_access", "Mail.Send"],
    }
}

/// Scopes that can change a Gmail mailbox or send from it: a reader grant
/// that came back with one is refused.
const GMAIL_WRITE_SCOPES: [&str; 5] = [
    "https://mail.google.com/",
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.compose",
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/gmail.insert",
];

/// How sign-in talks to the person at the terminal.
pub trait Interact: Send + Sync {
    /// Show `text`: where to sign in, and for a device code, what to enter.
    fn show(&self, text: &str);
    /// The address the browser was sent back to, pasted by the person, when
    /// the loopback redirect cannot reach this host; `None` when nothing
    /// will be pasted.
    fn pasted(&self) -> Pasted<'_>;
}

/// The address a person pastes, when they do.
pub type Pasted<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>>;

/// An OAuth sign-in: the provider, the owner's own client, and the grants
/// to ask for beyond the reader.
pub struct Request {
    pub provider: OAuthProvider,
    pub client_id: String,
    /// Gmail installed apps have one; Microsoft public clients do not.
    pub client_secret: Option<Secret>,
    /// Microsoft: `consumers`, `organizations`, `common`, or a tenant ID.
    pub tenant: String,
    /// Also ask for the grant that saves drafts and moves and marks mail.
    pub write: bool,
    /// Also ask for the grant that sends (Gmail's writer grant sends).
    pub send: bool,
    pub interact: Box<dyn Interact>,
}

/// What a sign-in returns: each grant asked for, and an access token of the
/// reader's to learn the mailbox's address with.
pub(crate) struct SignedIn {
    pub(crate) grants: Grants,
    pub(crate) reader_token: Secret,
}

/// The grants a sign-in asks for, reader first.
pub(crate) fn wanted(request: &Request) -> Vec<GrantKind> {
    let mut kinds = vec![GrantKind::Reader];
    if request.write || (request.send && request.provider == OAuthProvider::Gmail) {
        kinds.push(GrantKind::Writer);
    }
    if request.send && request.provider == OAuthProvider::Graph {
        kinds.push(GrantKind::Sender);
    }
    kinds
}

/// Sign in for every grant the request asks for, one consent each, at
/// `endpoints`.
pub(crate) async fn sign_in(request: &Request, endpoints: &Endpoints) -> Result<SignedIn> {
    let http = http_client()?;
    let mut grants = Grants::default();
    let mut reader_token = None;
    for kind in wanted(request) {
        if kind != GrantKind::Reader {
            request.interact.show(&format!(
                "Now sign in once more to let SCV {} (the {} grant).",
                match kind {
                    GrantKind::Writer if request.provider == OAuthProvider::Gmail => {
                        "save drafts, move and mark mail, and send once you approve each"
                    }
                    GrantKind::Writer => "save drafts and move and mark mail once you approve each",
                    _ => "send mail once you approve each",
                },
                kind.name()
            ));
        }
        let tokens = match request.provider {
            OAuthProvider::Gmail => loopback(request, kind, endpoints, &http).await?,
            OAuthProvider::Graph => device_code(request, kind, endpoints, &http).await?,
        };
        check_scopes(request.provider, kind, &tokens.scopes)?;
        let Some(refresh_token) = tokens.refresh_token else {
            bail!(
                "the provider returned no refresh token; remove SCV's access in your account's \
                 security settings and sign in again"
            );
        };
        if kind == GrantKind::Reader {
            reader_token = Some(tokens.access_token);
        }
        grants.set(
            kind,
            Grant {
                refresh_token,
                scopes: tokens.scopes,
            },
        );
    }
    Ok(SignedIn {
        grants,
        reader_token: reader_token.context("no reader grant")?,
    })
}

/// A reader grant must not be able to write: Gmail keeps each grant's
/// scopes apart, so one that came back with more is refused. Microsoft
/// consent is per app, so a Graph token may carry scopes another grant of
/// the same app was given; there the code's own gates hold the line.
fn check_scopes(provider: OAuthProvider, kind: GrantKind, granted: &[String]) -> Result<()> {
    let wanted = scopes(provider, kind);
    let has = |scope: &str| {
        granted
            .iter()
            .any(|granted| granted.eq_ignore_ascii_case(scope))
    };
    let needed = wanted
        .iter()
        .filter(|scope| **scope != "offline_access")
        .all(|scope| has(scope) || has(&format!("https://graph.microsoft.com/{scope}")));
    if !needed {
        bail!(
            "the provider did not grant what the {} grant needs; allow every permission SCV \
             asks for",
            kind.name()
        );
    }
    if provider == OAuthProvider::Gmail
        && kind == GrantKind::Reader
        && GMAIL_WRITE_SCOPES.iter().any(|scope| has(scope))
    {
        bail!(
            "Google returned a reading grant that can also write; remove SCV's access at \
             myaccount.google.com/permissions and sign in again"
        );
    }
    Ok(())
}

/// Tokens from a token endpoint.
#[derive(Debug)]
pub(crate) struct Tokens {
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Option<Secret>,
    pub(crate) expires_in: Duration,
    pub(crate) scopes: Vec<String>,
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

/// POST `form` to `url` and read the JSON answer, whatever its status. An
/// error names the provider's error code only.
async fn post_form(http: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<Value> {
    let mut response = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|_| anyhow!("could not reach the sign-in service"))?;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("the sign-in service's answer broke off"))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            bail!("the sign-in service's answer was too large");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| anyhow!("the sign-in service's answer was unreadable"))
}

/// The tokens in a token endpoint's answer, or its error code.
pub(crate) fn read_tokens(value: &Value) -> Result<Tokens> {
    if let Some(error) = value["error"].as_str() {
        bail!("the sign-in service refused: {}", error_code(error));
    }
    let access_token = value["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .context("the sign-in service returned no access token")?;
    Ok(Tokens {
        access_token: access_token.into(),
        refresh_token: value["refresh_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .map(Secret::from),
        // Access tokens last about an hour; a day bounds any other answer.
        expires_in: Duration::from_secs(value["expires_in"].as_u64().unwrap_or(3600).min(86_400)),
        scopes: value["scope"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    })
}

/// A provider's error code, when it is a plain word; never its text.
fn error_code(code: &str) -> &str {
    if code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        code
    } else {
        "an unrecognized error"
    }
}

/// `len` random URL-safe characters.
fn random_urlsafe(len: usize) -> String {
    let mut out = String::new();
    while out.len() < len {
        out.push_str(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(uuid::Uuid::new_v4().into_bytes()),
        );
    }
    out.truncate(len);
    out
}

/// PKCE: a verifier and its S256 challenge.
pub(crate) fn pkce() -> (String, String) {
    use sha2::Digest as _;
    let verifier = random_urlsafe(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// The Gmail sign-in page for one grant.
pub(crate) fn authorize_url(
    endpoints: &Endpoints,
    client_id: &str,
    redirect: &str,
    scope: &str,
    challenge: &str,
    state: &str,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&endpoints.authorize)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.into())
}

/// The authorization code in the address the browser was sent back to,
/// checked against `state`.
pub(crate) fn code_from_redirect(address: &str, state: &str) -> Result<String> {
    let address = address.trim();
    let url = if address.starts_with('/') {
        reqwest::Url::parse(&format!("http://127.0.0.1{address}"))
    } else {
        reqwest::Url::parse(address)
    }
    .map_err(|_| anyhow!("that is not the address the browser was sent back to"))?;
    let mut code = None;
    let mut returned_state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => returned_state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    // The state first: an answer without it, refusal or not, is not this
    // sign-in's.
    if returned_state.as_deref() != Some(state) {
        bail!("the sign-in answer does not belong to this sign-in; start again");
    }
    if let Some(error) = error {
        bail!("sign-in was refused: {}", error_code(&error));
    }
    code.filter(|code| !code.is_empty())
        .context("the address holds no authorization code")
}

/// Gmail: sign in through the browser for one grant, with the answer
/// coming back to a loopback listener or pasted by the person.
async fn loopback(
    request: &Request,
    kind: GrantKind,
    endpoints: &Endpoints,
    http: &reqwest::Client,
) -> Result<Tokens> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("could not listen for the sign-in answer")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let (verifier, challenge) = pkce();
    let state = random_urlsafe(24);
    let scope = scopes(request.provider, kind).join(" ");
    let url = authorize_url(
        endpoints,
        &request.client_id,
        &redirect,
        &scope,
        &challenge,
        &state,
    )?;
    request.interact.show(&format!(
        "Open this address in a browser and allow access:\n\n{url}\n\nIf the browser is on \
         another machine, it ends on a page that does not load; copy that page's address from \
         the address bar and paste it here."
    ));
    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, async {
        tokio::select! {
            code = accept_redirect(&listener, &state) => code,
            Some(pasted) = request.interact.pasted() => code_from_redirect(&pasted, &state),
        }
    })
    .await
    .map_err(|_| anyhow!("sign-in took longer than ten minutes; start again"))??;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", request.client_id.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    if let Some(secret) = &request.client_secret {
        form.push(("client_secret", secret.expose()));
    }
    read_tokens(&post_form(http, &endpoints.token, &form).await?)
}

/// Answer the browser's one request to the loopback listener and return
/// the code it carried.
async fn accept_redirect(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0; 8192];
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer))
            .await
            .unwrap_or(Ok(0))
            .unwrap_or(0);
        let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
        let Some(path) = head
            .strip_prefix("GET ")
            .and_then(|rest| rest.split_whitespace().next())
        else {
            continue;
        };
        let result = code_from_redirect(path, state);
        let page = if result.is_ok() {
            "SCV is signed in. You can close this tab."
        } else {
            "SCV could not use this answer. Go back to the terminal."
        };
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                )
                .as_bytes(),
            )
            .await;
        // Only this sign-in's answer ends the wait: a browser's request for
        // its icon carries no state, and neither does a request another local
        // process sends to the port.
        if answers_sign_in(path, state) {
            return result;
        }
    }
}

/// Whether loopback request `path` carries `state`, which only this
/// sign-in's answer does.
fn answers_sign_in(path: &str, state: &str) -> bool {
    path.starts_with('/')
        && reqwest::Url::parse(&format!("http://127.0.0.1{path}")).is_ok_and(|url| {
            url.query_pairs()
                .any(|(key, value)| key == "state" && value == state)
        })
}

/// Microsoft: sign in with a device code for one grant.
async fn device_code(
    request: &Request,
    kind: GrantKind,
    endpoints: &Endpoints,
    http: &reqwest::Client,
) -> Result<Tokens> {
    let scope = scopes(request.provider, kind).join(" ");
    let started = post_form(
        http,
        &endpoints.authorize,
        &[("client_id", &request.client_id), ("scope", &scope)],
    )
    .await?;
    if let Some(error) = started["error"].as_str() {
        bail!("Microsoft refused the sign-in: {}", error_code(error));
    }
    let device = started["device_code"]
        .as_str()
        .context("Microsoft returned no device code")?;
    let user_code = started["user_code"].as_str().unwrap_or_default();
    let verification = started["verification_uri"]
        .as_str()
        .unwrap_or("https://microsoft.com/devicelogin");
    if !user_code
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !verification.starts_with("https://")
    {
        bail!("Microsoft's sign-in answer was unexpected");
    }
    request.interact.show(&format!(
        "Open {verification} in a browser, enter the code {user_code}, and allow access."
    ));
    let mut interval = Duration::from_secs(started["interval"].as_u64().unwrap_or(5).clamp(1, 60));
    let deadline = Instant::now()
        + Duration::from_secs(started["expires_in"].as_u64().unwrap_or(900)).min(SIGN_IN_TIMEOUT);
    loop {
        tokio::time::sleep(interval).await;
        if Instant::now() > deadline {
            bail!("the sign-in code ran out; start again");
        }
        let answer = post_form(
            http,
            &endpoints.token,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &request.client_id),
                ("device_code", device),
            ],
        )
        .await?;
        match answer["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            _ => return read_tokens(&answer),
        }
    }
}

/// Access tokens of one grant, renewed from its refresh token as they run
/// out. A rotated refresh token replaces the old one in the grants file.
pub(crate) struct TokenSource {
    http: reqwest::Client,
    provider: OAuthProvider,
    kind: GrantKind,
    endpoints: Endpoints,
    client_id: String,
    client_secret: Option<Secret>,
    grants: PathBuf,
    /// Serializes changes to the grants file with the account's other
    /// credential writes.
    lock: Arc<dyn Fn() -> Result<std::fs::File> + Send + Sync>,
    cached: tokio::sync::Mutex<Option<(Secret, Instant)>>,
}

/// Why no access token could be had.
#[derive(Debug)]
pub(crate) enum TokenError {
    /// The grant is gone, revoked, or expired: the owner must sign in again.
    SignIn(anyhow::Error),
    /// The provider could not be reached or answered oddly; try later.
    Unavailable(anyhow::Error),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignIn(error) => write!(formatter, "{error:#}; sign the mail account in again"),
            Self::Unavailable(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for TokenError {}

impl TokenSource {
    #[allow(
        clippy::too_many_arguments,
        reason = "each is one part of a grant's identity"
    )]
    pub(crate) fn new(
        provider: OAuthProvider,
        kind: GrantKind,
        endpoints: Endpoints,
        client_id: String,
        client_secret: Option<Secret>,
        grants: PathBuf,
        lock: Arc<dyn Fn() -> Result<std::fs::File> + Send + Sync>,
    ) -> Result<Self> {
        Ok(Self {
            http: http_client()?,
            provider,
            kind,
            endpoints,
            client_id,
            client_secret,
            grants,
            lock,
            cached: tokio::sync::Mutex::new(None),
        })
    }

    /// A current access token.
    pub(crate) async fn token(&self) -> std::result::Result<Secret, TokenError> {
        let mut cached = self.cached.lock().await;
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() + EXPIRY_MARGIN < *until
        {
            return Ok(token.clone());
        }
        let tokens = self.refresh().await?;
        let until = Instant::now() + tokens.expires_in;
        *cached = Some((tokens.access_token.clone(), until));
        Ok(tokens.access_token)
    }

    /// Drop the cached token, as after the provider refused it.
    pub(crate) async fn forget(&self) {
        *self.cached.lock().await = None;
    }

    async fn refresh(&self) -> std::result::Result<Tokens, TokenError> {
        let path = self.grants.clone();
        let kind = self.kind;
        let grant = tokio::task::spawn_blocking(move || Grants::load(&path))
            .await
            .map_err(|error| TokenError::Unavailable(anyhow!(error)))?
            .map_err(TokenError::SignIn)?
            .get(kind)
            .cloned()
            .ok_or_else(|| {
                TokenError::SignIn(anyhow!("the mail account has no {} grant", kind.name()))
            })?;
        let scope = scopes(self.provider, self.kind).join(" ");
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", grant.refresh_token.expose()),
            ("client_id", self.client_id.as_str()),
        ];
        if self.provider == OAuthProvider::Graph {
            form.push(("scope", scope.as_str()));
        }
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.expose()));
        }
        let answer = post_form(&self.http, &self.endpoints.token, &form)
            .await
            .map_err(TokenError::Unavailable)?;
        let tokens = match answer["error"].as_str() {
            Some("invalid_grant" | "unauthorized_client" | "invalid_client") => {
                return Err(TokenError::SignIn(anyhow!(
                    "the provider no longer accepts SCV's {} grant",
                    self.kind.name()
                )));
            }
            _ => read_tokens(&answer).map_err(TokenError::Unavailable)?,
        };
        if let Some(rotated) = &tokens.refresh_token
            && rotated != &grant.refresh_token
        {
            let path = self.grants.clone();
            let lock = Arc::clone(&self.lock);
            let refreshed = grant.refresh_token.clone();
            let rotated = Grant {
                refresh_token: rotated.clone(),
                scopes: grant.scopes.clone(),
            };
            tokio::task::spawn_blocking(move || -> Result<()> {
                // The account's lock is held only for short writes: wait
                // them out rather than lose the rotated token.
                let deadline = Instant::now() + Duration::from_secs(2);
                let _lock = loop {
                    match lock() {
                        Err(error)
                            if crate::state::is_busy(&error) && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        result => break result?,
                    }
                };
                let mut grants = Grants::load(&path)?;
                // A sign-in since this refresh began replaced the grant;
                // what it saved stays.
                if grants
                    .get(kind)
                    .is_some_and(|stored| stored.refresh_token == refreshed)
                {
                    grants.set(kind, rotated);
                    grants.save(&path)?;
                }
                Ok(())
            })
            .await
            .map_err(|error| TokenError::Unavailable(anyhow!(error)))?
            .map_err(TokenError::Unavailable)?;
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests;
