//! Unit tests for `src/email/oauth.rs`.

use super::super::test_support::{FakeHttp, TOKEN_PATH, token_answer, tokens};
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
const MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";

struct Silent;

impl Interact for Silent {
    fn show(&self, _text: &str) {}

    fn pasted(&self) -> Pasted<'_> {
        Box::pin(std::future::pending())
    }
}

fn request(provider: OAuthProvider, write: bool, send: bool) -> Request {
    Request {
        provider,
        client_id: "client".into(),
        client_secret: None,
        tenant: "consumers".into(),
        write,
        send,
        interact: Box::new(Silent),
    }
}

fn s256(verifier: &str) -> String {
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn posts_to(fake: &FakeHttp, path: &str) -> Vec<super::super::test_support::HttpRequest> {
    fake.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == path)
        .collect()
}

/// What the browser stand-in saw of each consent.
struct Shown {
    texts: Mutex<Vec<String>>,
    challenges: Mutex<Vec<String>>,
    redirects: Mutex<Vec<String>>,
    states: Mutex<Vec<String>>,
}

struct Browser {
    shown: Arc<Shown>,
    paste: bool,
}

impl Interact for Browser {
    fn show(&self, text: &str) {
        self.shown.texts.lock().unwrap().push(text.to_owned());
        let Some(url) = text
            .lines()
            .find(|line| line.starts_with("http://") || line.starts_with("https://"))
        else {
            return;
        };
        let parsed = reqwest::Url::parse(url).expect("authorize url");
        let take = |name: &str| {
            parsed
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        };
        let state = take("state");
        let redirect = take("redirect_uri");
        self.shown
            .challenges
            .lock()
            .unwrap()
            .push(take("code_challenge"));
        self.shown.redirects.lock().unwrap().push(redirect.clone());
        self.shown.states.lock().unwrap().push(state.clone());
        if self.paste {
            return;
        }
        tokio::spawn(async move {
            let target = format!("{redirect}/?code=browser-code&state={state}");
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client");
            client.get(target).send().await.expect("loopback redirect");
        });
    }

    fn pasted(&self) -> Pasted<'_> {
        if !self.paste {
            return Box::pin(std::future::pending());
        }
        let shown = Arc::clone(&self.shown);
        Box::pin(async move {
            let state = shown.states.lock().unwrap().last().cloned().expect("state");
            Some(format!("/redirect?code=pasted-code&state={state}"))
        })
    }
}

fn shown() -> Arc<Shown> {
    Arc::new(Shown {
        texts: Mutex::new(Vec::new()),
        challenges: Mutex::new(Vec::new()),
        redirects: Mutex::new(Vec::new()),
        states: Mutex::new(Vec::new()),
    })
}

#[test]
fn wanted_asks_only_for_the_grants_a_provider_can_use() {
    let cases = [
        (OAuthProvider::Gmail, false, false, vec![GrantKind::Reader]),
        (
            OAuthProvider::Gmail,
            true,
            false,
            vec![GrantKind::Reader, GrantKind::Writer],
        ),
        (
            OAuthProvider::Gmail,
            false,
            true,
            vec![GrantKind::Reader, GrantKind::Writer],
        ),
        (
            OAuthProvider::Gmail,
            true,
            true,
            vec![GrantKind::Reader, GrantKind::Writer],
        ),
        (OAuthProvider::Graph, false, false, vec![GrantKind::Reader]),
        (
            OAuthProvider::Graph,
            true,
            false,
            vec![GrantKind::Reader, GrantKind::Writer],
        ),
        (
            OAuthProvider::Graph,
            false,
            true,
            vec![GrantKind::Reader, GrantKind::Sender],
        ),
        (
            OAuthProvider::Graph,
            true,
            true,
            vec![GrantKind::Reader, GrantKind::Writer, GrantKind::Sender],
        ),
    ];
    for (provider, write, send, kinds) in cases {
        assert_eq!(
            wanted(&request(provider, write, send)),
            kinds,
            "{provider:?} write {write} send {send}"
        );
    }
}

#[test]
fn scopes_stay_inside_one_grants_privilege() {
    assert_eq!(scopes(OAuthProvider::Gmail, GrantKind::Reader), [READONLY]);
    assert_eq!(scopes(OAuthProvider::Gmail, GrantKind::Writer), [MODIFY]);
    assert_eq!(
        scopes(OAuthProvider::Gmail, GrantKind::Sender),
        scopes(OAuthProvider::Gmail, GrantKind::Writer)
    );
    assert_eq!(
        scopes(OAuthProvider::Graph, GrantKind::Reader),
        ["offline_access", "User.Read", "Mail.Read"]
    );
    assert_eq!(
        scopes(OAuthProvider::Graph, GrantKind::Writer),
        ["offline_access", "Mail.ReadWrite"]
    );
    assert_eq!(
        scopes(OAuthProvider::Graph, GrantKind::Sender),
        ["offline_access", "Mail.Send"]
    );
}

#[test]
fn check_scopes_refuses_a_short_grant_and_a_gmail_reader_that_can_write() {
    let ok = |provider, kind, granted: &[&str]| {
        check_scopes(
            provider,
            kind,
            &granted
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|error| panic!("{provider:?} {kind:?} {granted:?}: {error:#}"));
    };
    ok(OAuthProvider::Gmail, GrantKind::Reader, &[READONLY]);
    ok(
        OAuthProvider::Gmail,
        GrantKind::Reader,
        &["HTTPS://WWW.GOOGLEAPIS.COM/AUTH/GMAIL.READONLY"],
    );
    ok(OAuthProvider::Gmail, GrantKind::Writer, &[MODIFY]);
    ok(OAuthProvider::Gmail, GrantKind::Sender, &[MODIFY]);
    ok(
        OAuthProvider::Graph,
        GrantKind::Reader,
        &["User.Read", "Mail.Read"],
    );
    ok(
        OAuthProvider::Graph,
        GrantKind::Reader,
        &[
            "https://graph.microsoft.com/User.Read",
            "https://graph.microsoft.com/Mail.Read",
        ],
    );
    ok(
        OAuthProvider::Graph,
        GrantKind::Reader,
        &[
            "https://graph.microsoft.com/user.read",
            "https://graph.microsoft.com/mail.read",
            "Mail.ReadWrite",
        ],
    );
    ok(OAuthProvider::Graph, GrantKind::Writer, &["Mail.ReadWrite"]);
    ok(OAuthProvider::Graph, GrantKind::Sender, &["Mail.Send"]);

    let missing = check_scopes(OAuthProvider::Graph, GrantKind::Reader, &[]).unwrap_err();
    assert!(missing.to_string().contains("reader"), "{missing:#}");
    let readonly = vec![READONLY.to_owned()];
    let short = check_scopes(OAuthProvider::Gmail, GrantKind::Writer, &readonly).unwrap_err();
    assert!(short.to_string().contains("writer"), "{short:#}");

    for extra in [
        MODIFY,
        "https://mail.google.com/",
        "https://www.googleapis.com/auth/gmail.compose",
        "https://www.googleapis.com/auth/gmail.send",
        "https://www.googleapis.com/auth/gmail.insert",
    ] {
        let error = check_scopes(
            OAuthProvider::Gmail,
            GrantKind::Reader,
            &[READONLY.to_owned(), extra.to_owned()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("write"), "{extra}: {error:#}");
    }
}

#[test]
fn read_tokens_keeps_the_error_code_splits_scopes_and_never_the_secret() {
    let refused = read_tokens(&json!({
        "error": "access_denied",
        "error_description": "rt-SECRET was shown to the user",
        "access_token": "at-SECRET"
    }))
    .unwrap_err();
    let text = format!("{refused:#}");
    assert!(text.contains("access_denied"), "{text}");
    assert!(!text.contains("rt-SECRET"), "{text}");
    assert!(!text.contains("at-SECRET"), "{text}");
    assert!(!text.contains("error_description"), "{text}");

    let messy = read_tokens(&json!({"error": "token rt-SECRET was revoked"})).unwrap_err();
    let text = format!("{messy:#}");
    assert!(text.contains("unrecognized"), "{text}");
    assert!(!text.contains("rt-SECRET"), "{text}");

    for value in [
        json!({"refresh_token": "rt-SECRET"}),
        json!({"access_token": ""}),
        json!({}),
    ] {
        let error = read_tokens(&value).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("access token"), "{text}");
        assert!(!text.contains("rt-SECRET"), "{text}");
    }

    let tokens = read_tokens(&json!({
        "access_token": "at-SECRET",
        "refresh_token": "rt-SECRET",
        "expires_in": 15,
        "scope": "one\ttwo  three"
    }))
    .unwrap();
    assert_eq!(tokens.scopes, ["one", "two", "three"]);
    assert_eq!(tokens.expires_in, Duration::from_secs(15));
    let rendered = format!("{tokens:?}");
    assert!(!rendered.contains("at-SECRET"), "{rendered}");
    assert!(!rendered.contains("rt-SECRET"), "{rendered}");
    assert_eq!(tokens.refresh_token.unwrap().expose(), "rt-SECRET");

    let assumed = read_tokens(&json!({"access_token": "at"})).unwrap();
    assert!(assumed.scopes.is_empty());
    assert!(assumed.refresh_token.is_none());
    assert_eq!(assumed.expires_in, Duration::from_secs(3600));
}

#[test]
fn error_code_drops_anything_that_is_not_a_plain_word() {
    assert_eq!(error_code("invalid_grant"), "invalid_grant");
    assert_eq!(error_code("ok_Code1"), "ok_Code1");
    assert_eq!(error_code(&"e".repeat(64)), "e".repeat(64));
    assert_eq!(error_code(&"e".repeat(65)), "an unrecognized error");
    assert_eq!(error_code("has space"), "an unrecognized error");
    assert_eq!(error_code("semi;colon"), "an unrecognized error");
    assert_eq!(error_code("the-secret-token"), "an unrecognized error");
    assert_eq!(error_code("quote'"), "an unrecognized error");
}

#[test]
fn pkce_verifier_is_64_url_safe_characters_and_the_challenge_is_its_hash() {
    let (verifier, challenge) = pkce();
    let (again, _) = pkce();
    assert_eq!(verifier.len(), 64, "{verifier}");
    assert!(
        verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{verifier}"
    );
    assert_eq!(challenge, s256(&verifier));
    assert_ne!(verifier, again);
}

#[test]
fn authorize_url_carries_pkce_state_and_offline_consent() {
    let endpoints = Endpoints {
        authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token: "https://oauth2.googleapis.com/token".into(),
    };
    let url = authorize_url(
        &endpoints,
        "client id",
        "http://127.0.0.1:1/cb",
        "a b",
        "chal",
        "state-1",
    )
    .unwrap();
    assert!(
        url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
        "{url}"
    );
    let parsed = reqwest::Url::parse(&url).unwrap();
    let mut pairs = parsed.query_pairs();
    let mut map = std::collections::HashMap::new();
    for (key, value) in pairs.by_ref() {
        assert!(
            map.insert(key.into_owned(), value.into_owned()).is_none(),
            "{url}"
        );
    }
    assert_eq!(map.get("client_id").map(String::as_str), Some("client id"));
    assert_eq!(
        map.get("redirect_uri").map(String::as_str),
        Some("http://127.0.0.1:1/cb")
    );
    assert_eq!(map.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(map.get("scope").map(String::as_str), Some("a b"));
    assert_eq!(map.get("code_challenge").map(String::as_str), Some("chal"));
    assert_eq!(
        map.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(map.get("state").map(String::as_str), Some("state-1"));
    assert_eq!(map.get("access_type").map(String::as_str), Some("offline"));
    assert_eq!(map.get("prompt").map(String::as_str), Some("consent"));
}

#[test]
fn code_from_redirect_accepts_a_url_or_a_path_and_refuses_a_mismatch() {
    assert_eq!(
        code_from_redirect("http://127.0.0.1:9/cb?code=abc&state=s", "s").unwrap(),
        "abc"
    );
    assert_eq!(
        code_from_redirect("https://example.com/cb?state=s&code=abc", "s").unwrap(),
        "abc"
    );
    assert_eq!(
        code_from_redirect("/cb?code=abc&state=s", "s").unwrap(),
        "abc"
    );

    let mismatch = code_from_redirect("/cb?code=rt-SECRET&state=nope", "yes").unwrap_err();
    let text = format!("{mismatch:#}");
    assert!(text.contains("does not belong"), "{text}");
    assert!(!text.contains("rt-SECRET"), "{text}");

    let refused = code_from_redirect(
        "http://127.0.0.1/?error=access_denied&error_description=rt-SECRET&state=s",
        "s",
    )
    .unwrap_err();
    let text = format!("{refused:#}");
    assert!(text.contains("access_denied"), "{text}");
    assert!(!text.contains("rt-SECRET"), "{text}");

    let messy = code_from_redirect("/cb?error=not%20a%20code&state=s", "s").unwrap_err();
    let text = format!("{messy:#}");
    assert!(text.contains("unrecognized"), "{text}");
    assert!(!text.contains("not a code"), "{text}");

    let stranger = code_from_redirect("/cb?error=access_denied&state=nope", "s").unwrap_err();
    let text = format!("{stranger:#}");
    assert!(
        text.contains("does not belong"),
        "a refusal without this sign-in's state is not its answer: {text}"
    );

    assert!(code_from_redirect("/cb?state=s", "s").is_err());
    assert!(code_from_redirect("/cb?code=&state=s", "s").is_err());
    assert!(code_from_redirect("code=abc&state=s", "s").is_err());
}

#[tokio::test]
async fn a_cached_token_is_reused_until_it_is_near_expiry_or_forgotten() {
    assert!(Duration::from_secs(30) < EXPIRY_MARGIN);

    let fake = FakeHttp::start(|request| token_answer(request).unwrap_or((404, "{}".into()))).await;
    let home = tempfile::tempdir().unwrap();
    let source = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let first = source.token().await.unwrap();
    let second = source.token().await.unwrap();
    assert_eq!(first.expose(), "at-reader");
    assert_eq!(second.expose(), first.expose());
    assert!(!format!("{first:?}").contains("at-reader"));
    assert_eq!(posts_to(&fake, TOKEN_PATH).len(), 1);
    source.forget().await;
    assert_eq!(source.token().await.unwrap().expose(), "at-reader");
    assert_eq!(
        posts_to(&fake, TOKEN_PATH).len(),
        2,
        "forget forces a refresh"
    );

    let short = FakeHttp::start(|request| {
        if request.path == TOKEN_PATH {
            (
                200,
                json!({"access_token": "at-short", "expires_in": 30, "token_type": "Bearer"})
                    .to_string(),
            )
        } else {
            (404, "{}".into())
        }
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let source = tokens(
        home.path(),
        &short.origin,
        OAuthProvider::Graph,
        GrantKind::Reader,
    );
    assert_eq!(source.token().await.unwrap().expose(), "at-short");
    assert_eq!(source.token().await.unwrap().expose(), "at-short");
    assert_eq!(
        posts_to(&short, TOKEN_PATH).len(),
        2,
        "a token inside the expiry margin is not reused"
    );
}

#[tokio::test]
async fn a_rotated_refresh_token_is_saved_without_touching_the_other_grants() {
    let fake = FakeHttp::start(|request| {
        if request.method == "POST" && request.path == TOKEN_PATH {
            (
                200,
                json!({
                    "access_token": "at-new",
                    "refresh_token": "rotated-refresh-token",
                    "expires_in": 3600,
                    "scope": "ignored"
                })
                .to_string(),
            )
        } else {
            (404, "{}".into())
        }
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let source = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let path = home.path().join("test.grants");
    let mut grants = Grants::load(&path).unwrap();
    grants.reader.as_mut().unwrap().scopes = vec!["kept-scope".into()];
    grants.save(&path).unwrap();
    assert_eq!(mode_of(&path), 0o600);

    assert_eq!(source.token().await.unwrap().expose(), "at-new");
    let saved = Grants::load(&path).unwrap();
    assert_eq!(
        saved.reader.as_ref().unwrap().refresh_token.expose(),
        "rotated-refresh-token"
    );
    assert_eq!(saved.reader.as_ref().unwrap().scopes, ["kept-scope"]);
    assert_eq!(
        saved.writer.as_ref().unwrap().refresh_token.expose(),
        "rt-writer"
    );
    assert_eq!(
        saved.sender.as_ref().unwrap().refresh_token.expose(),
        "rt-sender"
    );
    assert_eq!(mode_of(&path), 0o600);
    let rendered = format!("{saved:?} {:?}", saved.reader);
    assert!(!rendered.contains("rotated-refresh-token"), "{rendered}");
    assert!(!rendered.contains("rt-writer"), "{rendered}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("rt-reader"), "{text}");
}

#[tokio::test]
async fn invalid_grant_asks_for_sign_in_and_nothing_echoes_the_token() {
    let step = Arc::new(AtomicUsize::new(0));
    let next = Arc::clone(&step);
    let fake = FakeHttp::start(move |request| {
        if request.path != TOKEN_PATH {
            return (404, "{}".into());
        }
        let n = next.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => (
                200,
                json!({"error": "invalid_grant", "error_description": "rt-reader leaked-description"})
                    .to_string(),
            ),
            1 => (
                400,
                json!({"error": "server_error", "error_description": "rt-reader leaked-description"})
                    .to_string(),
            ),
            _ => (200, "not-json rt-reader leaked-description".into()),
        }
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let source = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );

    let revoked = source.token().await.unwrap_err();
    assert!(matches!(revoked, TokenError::SignIn(_)), "{revoked:?}");
    let text = format!("{revoked:?} {revoked}");
    assert!(text.contains("grant"), "{text}");
    assert!(!text.contains("rt-reader"), "{text}");
    assert!(!text.contains("leaked-description"), "{text}");

    let odd = source.token().await.unwrap_err();
    assert!(matches!(odd, TokenError::Unavailable(_)), "{odd:?}");
    let text = format!("{odd:?} {odd}");
    assert!(!text.contains("rt-reader"), "{text}");
    assert!(!text.contains("leaked-description"), "{text}");

    let broken = source.token().await.unwrap_err();
    assert!(matches!(broken, TokenError::Unavailable(_)), "{broken:?}");
    let text = format!("{broken:?} {broken}");
    assert!(!text.contains("rt-reader"), "{text}");
    assert!(!text.contains("leaked-description"), "{text}");
    assert_eq!(step.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn an_unreachable_token_endpoint_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let home = tempfile::tempdir().unwrap();
    let source = tokens(
        home.path(),
        &format!("http://127.0.0.1:{port}"),
        OAuthProvider::Graph,
        GrantKind::Sender,
    );
    let error = tokio::time::timeout(Duration::from_secs(5), source.token())
        .await
        .expect("token request hung")
        .unwrap_err();
    assert!(matches!(error, TokenError::Unavailable(_)), "{error:?}");
    let text = format!("{error:?} {error}");
    assert!(
        text.contains("could not reach") || text.contains("sign-in"),
        "{text}"
    );
    assert!(!text.contains("rt-sender"), "{text}");
}

#[tokio::test]
async fn a_graph_refresh_sends_its_scopes_and_a_secret_and_gmail_sends_neither_scope() {
    let fake = FakeHttp::start(|request| token_answer(request).unwrap_or((404, "{}".into()))).await;
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("test.grants");
    let gmail = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    let lock_path = home.path().join("test.lock");
    let endpoints = Endpoints {
        authorize: format!("{}/oauth/authorize", fake.origin),
        token: format!("{}{TOKEN_PATH}", fake.origin),
    };
    let lock: Arc<dyn Fn() -> Result<std::fs::File> + Send + Sync> =
        Arc::new(move || -> Result<_> {
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?)
        });
    let graph = TokenSource::new(
        OAuthProvider::Graph,
        GrantKind::Reader,
        endpoints.clone(),
        "client".into(),
        None,
        path.clone(),
        Arc::clone(&lock),
    )
    .unwrap();
    let secret = TokenSource::new(
        OAuthProvider::Gmail,
        GrantKind::Reader,
        endpoints,
        "client".into(),
        Some("super-secret".into()),
        path,
        lock,
    )
    .unwrap();

    let token = gmail.token().await.unwrap();
    assert_eq!(token.expose(), "at-reader");
    assert!(!format!("{token:?}").contains("at-reader"));
    graph.token().await.unwrap();
    assert_eq!(secret.token().await.unwrap().expose(), "at-reader");

    let sent = posts_to(&fake, TOKEN_PATH);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[0].form("scope").is_none(), "{:?}", sent[0].body);
    assert!(
        sent[0].form("client_secret").is_none(),
        "{:?}",
        sent[0].body
    );
    assert_eq!(sent[0].form("refresh_token").as_deref(), Some("rt-reader"));
    assert_eq!(
        sent[1].form("scope").as_deref(),
        Some(
            scopes(OAuthProvider::Graph, GrantKind::Reader)
                .join(" ")
                .as_str()
        )
    );
    assert!(
        sent[1].form("client_secret").is_none(),
        "{:?}",
        sent[1].body
    );
    assert!(sent[2].form("scope").is_none(), "{:?}", sent[2].body);
    assert_eq!(
        sent[2].form("client_secret").as_deref(),
        Some("super-secret")
    );
    assert_eq!(sent[2].form("grant_type").as_deref(), Some("refresh_token"));
    let rendered = format!("{:?}", sent[2].json());
    assert!(
        rendered.contains("null") || !rendered.contains("super-secret"),
        "{rendered}"
    );
}

/// Sign in to Gmail against `fake`, checking PKCE, the redirect, and the secret.
async fn gmail_sign_in(write: bool, paste: bool) -> (SignedIn, Arc<Shown>, FakeHttp) {
    let record = shown();
    let challenges = Arc::clone(&record);
    let code = if paste { "pasted-code" } else { "browser-code" };
    let fake = FakeHttp::start(move |request| {
        if request.path != "/token" {
            return (404, json!({"error": "not_found"}).to_string());
        }
        let n = challenges
            .challenges
            .lock()
            .unwrap()
            .len()
            .saturating_sub(1);
        let verifier = request.form("code_verifier").unwrap_or_default();
        let redirect = request.form("redirect_uri").unwrap_or_default();
        let expected_challenge = challenges.challenges.lock().unwrap().get(n).cloned();
        let expected_redirect = challenges.redirects.lock().unwrap().get(n).cloned();
        let reason = if expected_challenge.as_deref() != Some(s256(&verifier).as_str()) {
            "pkce"
        } else if expected_redirect.as_deref() != Some(redirect.as_str()) {
            "redirect"
        } else if request.form("client_secret").as_deref() != Some("client-secret") {
            "secret"
        } else if request.form("code").as_deref() != Some(code) {
            "badcode"
        } else if request.form("client_id").as_deref() != Some("gmail-client") {
            "client"
        } else if request.form("grant_type").as_deref() != Some("authorization_code") {
            "grant"
        } else {
            ""
        };
        if !reason.is_empty() {
            return (200, json!({"error": reason}).to_string());
        }
        let scope = if n == 0 { READONLY } else { MODIFY };
        (
            200,
            json!({
                "access_token": format!("at-{n}"),
                "refresh_token": format!("rt-{n}"),
                "expires_in": 3600,
                "scope": scope
            })
            .to_string(),
        )
    })
    .await;
    let request = Request {
        provider: OAuthProvider::Gmail,
        client_id: "gmail-client".into(),
        client_secret: Some("client-secret".into()),
        tenant: "consumers".into(),
        write,
        send: false,
        interact: Box::new(Browser {
            shown: Arc::clone(&record),
            paste,
        }),
    };
    let endpoints = Endpoints {
        authorize: format!("{}/authorize", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let signed = tokio::time::timeout(Duration::from_secs(5), sign_in(&request, &endpoints))
        .await
        .expect("sign-in hung")
        .unwrap();
    (signed, record, fake)
}

#[tokio::test]
async fn gmail_loopback_sign_in_checks_pkce_and_stores_both_grants() {
    let (signed, record, fake) = gmail_sign_in(true, false).await;
    let urls: Vec<String> = record
        .texts
        .lock()
        .unwrap()
        .iter()
        .filter_map(|text| {
            text.lines()
                .find(|line| line.starts_with("http://"))
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(urls.len(), 2, "{:?}", record.texts.lock().unwrap());
    assert!(
        record
            .texts
            .lock()
            .unwrap()
            .iter()
            .any(|text| text.contains("writer")),
        "{:?}",
        record.texts.lock().unwrap()
    );
    for (url, scope) in urls.iter().zip([READONLY, MODIFY]) {
        let parsed = reqwest::Url::parse(url).unwrap();
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "scope")
                .unwrap()
                .1,
            scope
        );
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "code_challenge_method")
                .unwrap()
                .1,
            "S256"
        );
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "client_id")
                .unwrap()
                .1,
            "gmail-client"
        );
    }
    let token_requests = posts_to(&fake, "/token");
    assert_eq!(token_requests.len(), 2);
    let challenges = record.challenges.lock().unwrap().clone();
    let redirects = record.redirects.lock().unwrap().clone();
    for (request, (challenge, redirect)) in
        token_requests.iter().zip(challenges.iter().zip(&redirects))
    {
        assert_eq!(s256(&request.form("code_verifier").unwrap()), *challenge);
        assert_eq!(
            request.form("redirect_uri").as_deref(),
            Some(redirect.as_str())
        );
        assert_eq!(
            request.form("client_secret").as_deref(),
            Some("client-secret")
        );
        assert_eq!(request.form("code").as_deref(), Some("browser-code"));
    }
    assert_eq!(signed.reader_token.expose(), "at-0");
    assert_eq!(
        signed
            .grants
            .reader
            .as_ref()
            .unwrap()
            .refresh_token
            .expose(),
        "rt-0"
    );
    assert_eq!(signed.grants.reader.as_ref().unwrap().scopes, [READONLY]);
    assert_eq!(
        signed
            .grants
            .writer
            .as_ref()
            .unwrap()
            .refresh_token
            .expose(),
        "rt-1"
    );
    assert_eq!(signed.grants.writer.as_ref().unwrap().scopes, [MODIFY]);
    assert!(signed.grants.sender.is_none());
    let rendered = format!("{:?}", signed.grants);
    assert!(!rendered.contains("rt-0"), "{rendered}");
    assert!(!rendered.contains("client-secret"), "{rendered}");
}

#[tokio::test]
async fn a_pasted_redirect_finishes_gmail_sign_in_without_the_loopback() {
    let (signed, record, fake) = gmail_sign_in(false, true).await;
    assert_eq!(posts_to(&fake, "/token").len(), 1);
    assert_eq!(
        posts_to(&fake, "/token")[0].form("code").as_deref(),
        Some("pasted-code")
    );
    assert_eq!(record.states.lock().unwrap().len(), 1);
    assert_eq!(signed.grants.reader.as_ref().unwrap().scopes, [READONLY]);
    assert!(signed.grants.writer.is_none());
    assert_eq!(signed.reader_token.expose(), "at-0");
}

#[tokio::test]
async fn graph_device_code_waits_through_authorization_pending() {
    let polls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&polls);
    let fake = FakeHttp::start(move |request| {
        if request.path == "/device" {
            return (
                200,
                json!({
                    "device_code": "device-secret-code",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://login.microsoft.com/device",
                    "interval": 1,
                    "expires_in": 120
                })
                .to_string(),
            );
        }
        let n = count.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            (200, json!({"error": "authorization_pending"}).to_string())
        } else {
            (
                200,
                json!({
                    "access_token": "at-graph",
                    "refresh_token": "rt-graph",
                    "expires_in": 3600,
                    "scope": "offline_access User.Read Mail.Read"
                })
                .to_string(),
            )
        }
    })
    .await;
    let record = shown();
    let request = Request {
        provider: OAuthProvider::Graph,
        client_id: "graph-client".into(),
        client_secret: None,
        tenant: "consumers".into(),
        write: false,
        send: false,
        interact: Box::new(Browser {
            shown: Arc::clone(&record),
            paste: false,
        }),
    };
    let endpoints = Endpoints {
        authorize: format!("{}/device", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let signed = tokio::time::timeout(Duration::from_secs(8), sign_in(&request, &endpoints))
        .await
        .expect("device sign-in hung")
        .unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    let device = fake
        .requests()
        .into_iter()
        .find(|request| request.path == "/device")
        .unwrap();
    assert_eq!(device.form("client_id").as_deref(), Some("graph-client"));
    assert_eq!(
        device.form("scope").as_deref(),
        Some(
            scopes(OAuthProvider::Graph, GrantKind::Reader)
                .join(" ")
                .as_str()
        )
    );
    let polls_sent = posts_to(&fake, "/token");
    assert_eq!(polls_sent.len(), 2);
    assert_eq!(
        polls_sent[0].form("grant_type").as_deref(),
        Some("urn:ietf:params:oauth:grant-type:device_code")
    );
    assert_eq!(
        polls_sent[0].form("device_code").as_deref(),
        Some("device-secret-code")
    );
    assert!(polls_sent[0].form("client_secret").is_none());
    let shown = record.texts.lock().unwrap().join("\n");
    assert!(shown.contains("ABCD-EFGH"), "{shown}");
    assert!(
        shown.contains("https://login.microsoft.com/device"),
        "{shown}"
    );
    assert!(!shown.contains("device-secret-code"), "{shown}");
    assert_eq!(signed.reader_token.expose(), "at-graph");
    assert_eq!(
        signed
            .grants
            .reader
            .as_ref()
            .unwrap()
            .refresh_token
            .expose(),
        "rt-graph"
    );
    assert_eq!(
        signed.grants.reader.as_ref().unwrap().scopes,
        ["offline_access", "User.Read", "Mail.Read"]
    );
    assert!(signed.grants.writer.is_none());
    assert!(signed.grants.sender.is_none());
}

#[tokio::test]
async fn a_device_code_with_an_odd_user_code_or_a_plain_http_page_is_refused() {
    let starts = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&starts);
    let fake = FakeHttp::start(move |request| {
        if request.path != "/device" {
            return (200, json!({"error": "polled"}).to_string());
        }
        let n = count.fetch_add(1, Ordering::SeqCst);
        let body = if n == 0 {
            json!({
                "device_code": "device-secret-code",
                "user_code": "BAD CODE",
                "verification_uri": "https://login.microsoft.com/device",
                "interval": 1
            })
        } else {
            json!({
                "device_code": "device-secret-code",
                "user_code": "ABCD",
                "verification_uri": "http://login.microsoft.com/device",
                "interval": 1
            })
        };
        (200, body.to_string())
    })
    .await;
    let endpoints = Endpoints {
        authorize: format!("{}/device", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    for _ in 0..2 {
        let request = Request {
            provider: OAuthProvider::Graph,
            client_id: "graph-client".into(),
            client_secret: None,
            tenant: "consumers".into(),
            write: false,
            send: false,
            interact: Box::new(Silent),
        };
        let error = tokio::time::timeout(Duration::from_secs(3), sign_in(&request, &endpoints))
            .await
            .expect("a refused device code hung")
            .map(|_| ())
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("unexpected"), "{text}");
        assert!(!text.contains("device-secret-code"), "{text}");
    }
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.path != "/token"),
        "{:?}",
        fake.requests()
    );
}

#[tokio::test]
async fn sign_in_refuses_a_token_response_with_no_refresh_token() {
    let fake = FakeHttp::start(|request| {
        if request.path == "/device" {
            return (
                200,
                json!({
                    "device_code": "device-secret-code",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://login.microsoft.com/device",
                    "interval": 1,
                    "expires_in": 120
                })
                .to_string(),
            );
        }
        (
            200,
            json!({
                "access_token": "at-LEAKABLE",
                "expires_in": 3600,
                "scope": "offline_access User.Read Mail.Read"
            })
            .to_string(),
        )
    })
    .await;
    let request = Request {
        provider: OAuthProvider::Graph,
        client_id: "graph-client".into(),
        client_secret: None,
        tenant: "consumers".into(),
        write: false,
        send: false,
        interact: Box::new(Silent),
    };
    let endpoints = Endpoints {
        authorize: format!("{}/device", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let error = tokio::time::timeout(Duration::from_secs(5), sign_in(&request, &endpoints))
        .await
        .expect("sign-in hung")
        .map(|_| ())
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("refresh token"), "{text}");
    assert!(!text.contains("at-LEAKABLE"), "{text}");
    assert!(!text.contains("device-secret-code"), "{text}");
}

#[tokio::test]
async fn the_loopback_waits_past_requests_without_this_sign_ins_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = tokio::spawn(async move {
        for path in [
            "/favicon.ico",
            "/?error=access_denied",
            "/?code=forged&state=wrong",
            "/?code=browser-code&state=s",
        ] {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut page = String::new();
            stream.read_to_string(&mut page).await.unwrap();
        }
    });
    let code = tokio::time::timeout(Duration::from_secs(5), accept_redirect(&listener, "s"))
        .await
        .expect("the loopback answered")
        .unwrap();
    assert_eq!(code, "browser-code");
    requests.await.unwrap();

    let expires = read_tokens(&serde_json::json!({
        "access_token": "at",
        "expires_in": u64::MAX,
    }))
    .unwrap()
    .expires_in;
    assert_eq!(
        expires,
        Duration::from_secs(86_400),
        "a token's lifetime is bounded"
    );
}

#[tokio::test]
async fn a_sign_in_during_a_refresh_keeps_its_grant() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("test.grants");
    let during = path.clone();
    let fake = FakeHttp::start(move |request| {
        if request.method == "POST" && request.path == TOKEN_PATH {
            // The owner signs in again while the refresh is on its way.
            let mut grants = Grants::load(&during).unwrap();
            grants.set(
                GrantKind::Reader,
                crate::email::credentials::Grant {
                    refresh_token: "rt-signed-in-again".to_owned().into(),
                    scopes: Vec::new(),
                },
            );
            grants.save(&during).unwrap();
            (
                200,
                json!({
                    "access_token": "at-new",
                    "refresh_token": "rotated-refresh-token",
                    "expires_in": 3600
                })
                .to_string(),
            )
        } else {
            (404, "{}".into())
        }
    })
    .await;
    let source = tokens(
        home.path(),
        &fake.origin,
        OAuthProvider::Gmail,
        GrantKind::Reader,
    );
    assert_eq!(source.token().await.unwrap().expose(), "at-new");
    let saved = Grants::load(&path).unwrap();
    assert_eq!(
        saved.reader.as_ref().unwrap().refresh_token.expose(),
        "rt-signed-in-again",
        "the rotated token of the old grant does not replace the new sign-in"
    );
}
