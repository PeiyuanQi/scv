use super::*;
use crate::slack::tests::Fake;

#[tokio::test]
async fn each_token_goes_only_to_its_methods() {
    let mut fake = Fake::start().await;
    let api = fake.api();
    api.call("apps.connections.open", true, Verb::Post, &[])
        .await
        .unwrap();
    let request = fake.request("/apps.connections.open").await;
    assert_eq!(request.authorization.as_deref(), Some("Bearer xapp-test"));
    api.call("auth.test", false, Verb::Post, &[]).await.unwrap();
    let request = fake.request("/auth.test").await;
    assert_eq!(request.authorization.as_deref(), Some("Bearer xoxb-test"));
}

#[tokio::test]
async fn redirects_are_refused_and_responses_bounded() {
    let fake = Fake::start().await;
    let api = fake.api();
    fake.script_with(
        "/auth.test",
        302,
        "Location: http://127.0.0.1:1/secret\r\n",
        json!({}),
    );
    let error = api
        .call("auth.test", false, Verb::Post, &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("302") && !error.contains("secret"),
        "{error}"
    );
    fake.script(
        "/auth.test",
        200,
        Value::String("x".repeat(MAX_RESPONSE_BYTES + 1)),
    );
    let error = api
        .call("auth.test", false, Verb::Post, &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("response_too_large"), "{error}");
}

#[tokio::test]
async fn a_rate_limit_blocks_the_method_without_another_request() {
    let mut fake = Fake::start().await;
    let api = fake.api();
    fake.script_with(
        "/chat.postMessage",
        429,
        "Retry-After: 60\r\n",
        json!({"ok": false, "error": "ratelimited"}),
    );
    let error = api
        .call("chat.postMessage", false, Verb::Post, &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Retry-After"));
    assert!(!error.is::<Refusal>());
    fake.request("/chat.postMessage").await;
    let error = api
        .call("chat.postMessage", false, Verb::Post, &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cooldown"));
    // Other methods are unaffected.
    api.call("auth.test", false, Verb::Post, &[]).await.unwrap();
    fake.request("/auth.test").await;
}

#[tokio::test]
async fn refusals_carry_only_safe_codes_and_transient_errors_are_not_refusals() {
    let fake = Fake::start().await;
    let api = fake.api();
    for (code, refused, shown) in [
        (
            "missing_scope",
            true,
            "scope; see the setup guide (missing_scope)",
        ),
        ("xoxb-secret\n", true, "(unknown)"),
        ("internal_error", false, "this time"),
    ] {
        fake.script(
            "/auth.test",
            200,
            json!({"ok": false, "error": code, "warning": "xoxb-secret"}),
        );
        let error = api
            .call("auth.test", false, Verb::Post, &[])
            .await
            .unwrap_err();
        assert_eq!(error.is::<Refusal>(), refused, "{code}");
        let error = error.to_string();
        assert!(error.contains(shown), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }
}

#[tokio::test]
async fn identity_is_checked_once_and_must_be_consistent() {
    let mut fake = Fake::start().await;
    let api = fake.api();
    let identity = api.identity().await.unwrap();
    assert_eq!(
        (
            identity.team_id.as_str(),
            identity.app_id.as_str(),
            identity.bot_user_id.as_str()
        ),
        ("T123", "A123", "UBOT")
    );
    assert!(identity.scopes.unwrap().contains("files:write"));
    let lookup = fake.request("/bots.info").await;
    assert_eq!(
        (lookup.method.as_str(), &lookup.body["bot"]),
        ("GET", &json!("B123"))
    );
    api.identity().await.unwrap();
    assert_eq!(fake.made("/auth.test"), 0);

    let fake = Fake::start().await;
    fake.script(
        "/bots.info",
        200,
        json!({"ok": true, "bot": {"app_id": "A123", "user_id": "UOTHER"}}),
    );
    assert!(fake.api().identity().await.is_err());
}

#[test]
fn only_tls_urls_on_slacks_hosts_are_trusted() {
    let api = Api::new("xoxb-test".into(), "xapp-test".into()).unwrap();
    assert!(
        api.trusted("wss://wss-primary.slack.com/link/?ticket=secret", "wss")
            .is_ok()
    );
    assert!(
        api.trusted("https://files.slack.com/files-pri/T1-F1/a.png", "https")
            .is_ok()
    );
    for (url, scheme) in [
        ("ws://wss.slack.com/link", "wss"),
        ("https://wss.slack.com/link", "wss"),
        ("wss://slack.com.evil.test/link", "wss"),
        ("wss://user:secret@wss.slack.com/link", "wss"),
        ("wss://wss.slack.com:444/link", "wss"),
        ("wss://wss.slack.com/link#secret", "wss"),
        ("https://files.slack.com.evil.test/a.png", "https"),
        ("http://files.slack.com/a.png", "https"),
        ("https://127.0.0.1/a.png", "https"),
    ] {
        let error = api.trusted(url, scheme).unwrap_err().to_string();
        assert!(!error.contains("secret"), "{url}: {error}");
    }
}

#[test]
fn posts_escape_markup_and_keep_their_thread() {
    let params = post_params(&Outbound {
        to: "U123",
        reply_to: "slack:C123/thread:1700000000.000001",
        part: 0,
        text: "<!channel> <@U123> &",
        client_id: "stable-id",
    })
    .unwrap();
    let params: HashMap<_, _> = params.into_iter().collect();
    assert_eq!(params["channel"], "C123");
    assert_eq!(params["thread_ts"], "1700000000.000001");
    assert_eq!(params["text"], "&lt;!channel&gt; &lt;@U123&gt; &amp;");
    assert_eq!(params["unfurl_media"], "false");
    assert!(!params.contains_key("client_msg_id"));
}
