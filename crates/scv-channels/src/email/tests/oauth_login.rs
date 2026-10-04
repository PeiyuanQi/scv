//! Unit tests for `src/email/mod.rs`.

use super::super::test_support::FakeHttp;
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn s256(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Whether any file under `root` holds `needle`.
fn contains_anywhere(root: &std::path::Path, needle: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
            {
                return true;
            }
        }
    }
    false
}

struct Shown {
    texts: Mutex<Vec<String>>,
    challenges: Mutex<Vec<String>>,
    redirects: Mutex<Vec<String>>,
    states: Mutex<Vec<String>>,
}

struct Browser {
    shown: Arc<Shown>,
}

impl oauth::Interact for Browser {
    fn show(&self, text: &str) {
        self.shown.texts.lock().unwrap().push(text.to_owned());
        let Some(url) = text.lines().find(|line| line.starts_with("http://")) else {
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

    fn pasted(&self) -> oauth::Pasted<'_> {
        Box::pin(std::future::pending())
    }
}

struct Quiet;

impl oauth::Interact for Quiet {
    fn show(&self, text: &str) {
        panic!("sign-in reached the terminal: {text}");
    }

    fn pasted(&self) -> oauth::Pasted<'_> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn gmail_sign_in_saves_the_profile_address_and_keeps_refresh_tokens_in_the_grants() {
    let record = Arc::new(Shown {
        texts: Mutex::new(Vec::new()),
        challenges: Mutex::new(Vec::new()),
        redirects: Mutex::new(Vec::new()),
        states: Mutex::new(Vec::new()),
    });
    let checks = Arc::clone(&record);
    let fake = FakeHttp::start(move |request| {
        if request.path == "/token" {
            let n = checks.challenges.lock().unwrap().len().saturating_sub(1);
            let verifier = request.form("code_verifier").unwrap_or_default();
            let challenge = checks
                .challenges
                .lock()
                .unwrap()
                .get(n)
                .cloned()
                .unwrap_or_default();
            let redirect = checks
                .redirects
                .lock()
                .unwrap()
                .get(n)
                .cloned()
                .unwrap_or_default();
            let reason = if s256(&verifier) != challenge {
                "pkce"
            } else if request.form("redirect_uri").as_deref() != Some(redirect.as_str()) {
                "redirect"
            } else if request.form("client_secret").as_deref() != Some("client-secret") {
                "secret"
            } else if request.form("code").as_deref() != Some("browser-code") {
                "badcode"
            } else {
                ""
            };
            if !reason.is_empty() {
                return (200, json!({"error": reason}).to_string());
            }
            return (
                200,
                json!({
                    "access_token": "at-gmail-access",
                    "refresh_token": "rt-gmail-refresh",
                    "expires_in": 3600,
                    "scope": "https://www.googleapis.com/auth/gmail.readonly"
                })
                .to_string(),
            );
        }
        if request.path == "/gmail/v1/users/me/profile" {
            return (200, json!({"emailAddress": "Owner@Gmail.com"}).to_string());
        }
        (404, json!({"error": "not_found"}).to_string())
    })
    .await;
    let interact = Box::new(Browser {
        shown: Arc::clone(&record),
    });
    let home = tempfile::tempdir().unwrap();
    let layout = scv_client::Layout::new(home.path());
    let request = oauth::Request {
        provider: oauth::OAuthProvider::Gmail,
        client_id: "gmail-client".into(),
        client_secret: Some("client-secret".into()),
        tenant: "consumers".into(),
        write: false,
        send: false,
        interact,
    };
    let endpoints = oauth::Endpoints {
        authorize: format!("{}/authorize", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let origins = api::Origins {
        gmail: fake.origin.clone(),
        graph: fake.origin.clone(),
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        login_oauth(&layout, "work", &request, &endpoints, &origins),
    )
    .await
    .expect("sign-in hung")
    .unwrap();

    assert!(
        record
            .texts
            .lock()
            .unwrap()
            .iter()
            .any(|text| text.contains("gmail-client")),
        "{:?}",
        record.texts.lock().unwrap()
    );
    let profile = fake
        .requests()
        .into_iter()
        .find(|request| request.path == "/gmail/v1/users/me/profile")
        .unwrap();
    assert_eq!(profile.method, "GET");
    assert_eq!(
        profile.header("authorization"),
        Some("Bearer at-gmail-access")
    );
    let credentials = layout.channel_credentials(CHANNEL).join("work.json");
    let grants_path = credentials::Grants::path(&layout, "work").unwrap();
    let text = std::fs::read_to_string(&credentials).unwrap();
    assert!(!text.contains("rt-gmail-refresh"), "{text}");
    assert!(!text.contains("at-gmail-access"), "{text}");
    assert!(!text.contains("browser-code"), "{text}");
    let saved: Account = serde_json::from_str(&text).unwrap();
    match saved {
        Account::Gmail {
            address,
            client_id,
            client_secret,
        } => {
            assert_eq!(address, "Owner@gmail.com");
            assert_eq!(client_id, "gmail-client");
            assert_eq!(client_secret.unwrap().expose(), "client-secret");
        }
        other => panic!("saved {other:?}"),
    }
    let grants = credentials::Grants::load(&grants_path).unwrap();
    assert_eq!(
        grants.reader.as_ref().unwrap().refresh_token.expose(),
        "rt-gmail-refresh"
    );
    assert_eq!(
        grants.reader.as_ref().unwrap().scopes,
        ["https://www.googleapis.com/auth/gmail.readonly"]
    );
    assert!(grants.writer.is_none());
    assert!(grants.sender.is_none());
    assert_eq!(mode_of(&grants_path), 0o600);
}

#[tokio::test]
async fn graph_sign_in_reads_mail_or_the_user_principal_name() {
    let profiles = Arc::new(AtomicUsize::new(0));
    let profile_count = Arc::clone(&profiles);
    let tokens = Arc::new(AtomicUsize::new(0));
    let token_count = Arc::clone(&tokens);
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
        if request.path == "/token" {
            let n = token_count.fetch_add(1, Ordering::SeqCst);
            let (access, refresh) = if n == 0 {
                ("at-ada", "rt-ada-refresh")
            } else {
                ("at-bea", "rt-bea-refresh")
            };
            return (
                200,
                json!({
                    "access_token": access,
                    "refresh_token": refresh,
                    "expires_in": 3600,
                    "scope": "offline_access User.Read Mail.Read"
                })
                .to_string(),
            );
        }
        if request.path == "/v1.0/me" {
            let n = profile_count.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                json!({"mail": "Ada@Outlook.com", "userPrincipalName": "Other@Outlook.com"})
            } else {
                json!({"mail": "not-an-address", "userPrincipalName": "Bea@Outlook.com"})
            };
            return (200, body.to_string());
        }
        (404, json!({"error": "not_found"}).to_string())
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let layout = scv_client::Layout::new(home.path());
    let endpoints = oauth::Endpoints {
        authorize: format!("{}/device", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let origins = api::Origins {
        gmail: fake.origin.clone(),
        graph: fake.origin.clone(),
    };
    for (account, address, refresh, access) in [
        ("ada", "Ada@outlook.com", "rt-ada-refresh", "at-ada"),
        ("bea", "Bea@outlook.com", "rt-bea-refresh", "at-bea"),
    ] {
        let request = oauth::Request {
            provider: oauth::OAuthProvider::Graph,
            client_id: "graph-client".into(),
            client_secret: None,
            tenant: "consumers".into(),
            write: false,
            send: false,
            interact: Box::new(QuietShown),
        };
        tokio::time::timeout(
            Duration::from_secs(8),
            login_oauth(&layout, account, &request, &endpoints, &origins),
        )
        .await
        .expect("graph sign-in hung")
        .unwrap();
        let text = std::fs::read_to_string(
            layout
                .channel_credentials(CHANNEL)
                .join(format!("{account}.json")),
        )
        .unwrap();
        assert!(!text.contains(refresh), "{text}");
        assert!(!text.contains(access), "{text}");
        let saved: Account = serde_json::from_str(&text).unwrap();
        match saved {
            Account::Graph {
                address: saved_address,
                client_id,
                tenant,
            } => {
                assert_eq!(saved_address, address);
                assert_eq!(client_id, "graph-client");
                assert_eq!(tenant, "consumers");
            }
            other => panic!("saved {other:?}"),
        }
        let grants_path = credentials::Grants::path(&layout, account).unwrap();
        let grants = credentials::Grants::load(&grants_path).unwrap();
        assert_eq!(grants.reader.unwrap().refresh_token.expose(), refresh);
        assert_eq!(mode_of(&grants_path), 0o600);
    }
    let me: Vec<_> = fake
        .requests()
        .into_iter()
        .filter(|request| request.path == "/v1.0/me")
        .collect();
    assert_eq!(me.len(), 2);
    assert_eq!(me[0].header("authorization"), Some("Bearer at-ada"));
    assert_eq!(me[1].header("authorization"), Some("Bearer at-bea"));
    assert_eq!(profiles.load(Ordering::SeqCst), 2);
    assert_eq!(tokens.load(Ordering::SeqCst), 2);
    let device = fake
        .requests()
        .into_iter()
        .find(|request| request.path == "/device")
        .unwrap();
    assert_eq!(
        device.form("scope").as_deref(),
        Some("offline_access User.Read Mail.Read")
    );
}

/// Shows the device-code instructions and never pastes.
struct QuietShown;

impl oauth::Interact for QuietShown {
    fn show(&self, text: &str) {
        assert!(text.contains("ABCD-EFGH"), "{text}");
        assert!(!text.contains("device-secret-code"), "{text}");
    }

    fn pasted(&self) -> oauth::Pasted<'_> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_profile_without_an_address_saves_nothing() {
    let checks = Arc::new(Shown {
        texts: Mutex::new(Vec::new()),
        challenges: Mutex::new(Vec::new()),
        redirects: Mutex::new(Vec::new()),
        states: Mutex::new(Vec::new()),
    });
    let record = Arc::clone(&checks);
    let fake = FakeHttp::start(move |request| {
        if request.path == "/token" {
            let n = record.challenges.lock().unwrap().len().saturating_sub(1);
            let verifier = request.form("code_verifier").unwrap_or_default();
            if s256(&verifier)
                != record
                    .challenges
                    .lock()
                    .unwrap()
                    .get(n)
                    .cloned()
                    .unwrap_or_default()
            {
                return (200, json!({"error": "pkce"}).to_string());
            }
            return (
                200,
                json!({
                    "access_token": "at-gmail-access",
                    "refresh_token": "rt-gmail-refresh",
                    "expires_in": 3600,
                    "scope": "https://www.googleapis.com/auth/gmail.readonly"
                })
                .to_string(),
            );
        }
        if request.path == "/gmail/v1/users/me/profile" {
            return (200, json!({"emailAddress": "not-an-address"}).to_string());
        }
        (404, json!({"error": "not_found"}).to_string())
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let layout = scv_client::Layout::new(home.path());
    let request = oauth::Request {
        provider: oauth::OAuthProvider::Gmail,
        client_id: "gmail-client".into(),
        client_secret: Some("client-secret".into()),
        tenant: "consumers".into(),
        write: false,
        send: false,
        interact: Box::new(Browser { shown: checks }),
    };
    let endpoints = oauth::Endpoints {
        authorize: format!("{}/authorize", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let origins = api::Origins {
        gmail: fake.origin.clone(),
        graph: fake.origin.clone(),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        login_oauth(&layout, "broken", &request, &endpoints, &origins),
    )
    .await
    .expect("sign-in hung")
    .unwrap_err();
    assert!(error.to_string().contains("address"), "{error:#}");
    assert!(
        !layout
            .channel_credentials(CHANNEL)
            .join("broken.json")
            .exists()
    );
    assert!(
        !credentials::Grants::path(&layout, "broken")
            .unwrap()
            .exists()
    );
    assert!(!contains_anywhere(home.path(), "rt-gmail-refresh"));
}

#[tokio::test]
async fn an_invalid_client_id_fails_before_any_request() {
    let fake =
        FakeHttp::start(|_| (500, json!({"error": "should_not_be_called"}).to_string())).await;
    let home = tempfile::tempdir().unwrap();
    let layout = scv_client::Layout::new(home.path());
    let request = oauth::Request {
        provider: oauth::OAuthProvider::Gmail,
        client_id: "bad id".into(),
        client_secret: None,
        tenant: "consumers".into(),
        write: false,
        send: false,
        interact: Box::new(Quiet),
    };
    let endpoints = oauth::Endpoints {
        authorize: format!("{}/authorize", fake.origin),
        token: format!("{}/token", fake.origin),
    };
    let origins = api::Origins {
        gmail: fake.origin.clone(),
        graph: fake.origin.clone(),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        login_oauth(&layout, "bad", &request, &endpoints, &origins),
    )
    .await
    .expect("login_oauth hung")
    .unwrap_err();
    assert!(error.to_string().contains("client ID"), "{error:#}");
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    assert!(
        !layout
            .channel_credentials(CHANNEL)
            .join("bad.json")
            .exists()
    );
}
