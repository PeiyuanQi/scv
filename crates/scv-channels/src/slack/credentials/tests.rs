use super::*;
use crate::state::Credentials as _;

#[test]
fn tokens_are_manual_typed_bounded_and_redacted() {
    let account = crate::slack::tests::account();
    assert!(account.validate().is_ok());
    for (bot, app) in [
        ("", "xapp-test"),
        ("xapp-test", "xoxb-test"),
        ("xoxb-", "xapp-test"),
        ("xoxb-a b", "xapp-test"),
        ("xoxb-test", "xapp-"),
        ("xoxb-test", "xapp-a\nb"),
        ("xoxb-test", "xoxe.xapp-rotating"),
    ] {
        assert!(validate_tokens(bot, app).is_err());
    }
    assert!(validate_tokens(&format!("xoxb-{}", "a".repeat(512)), "xapp-test").is_err());
    let debug = format!("{account:?}");
    assert!(!debug.contains("xoxb-test") && !debug.contains("xapp-test"));
    assert!(validate_owner(Some("display-name")).is_err());
}
#[test]
fn identity_binding_and_private_storage() {
    let account = crate::slack::tests::account();
    let fingerprint = account.fingerprint().unwrap();
    let mut rotated = account.clone();
    rotated.bot_token = "xoxb-rotated".into();
    rotated.app_token = "xapp-rotated".into();
    assert_eq!(fingerprint, rotated.fingerprint().unwrap());
    rotated.team_id = "TOTHER".into();
    assert_ne!(fingerprint, rotated.fingerprint().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_client::Layout::new(dir.path()), "slack");
    store.save_account("default", &account).unwrap();
    assert!(store.save_account("default", &rotated).is_err());
    assert_eq!(store.account("default").unwrap(), Some(account));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(store.credentials_path("default").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[tokio::test]
async fn invalid_onboarding_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_client::Layout::new(dir.path()), "slack");
    assert!(
        login(
            &store,
            "default",
            Login {
                bot_token: "xapp-wrong".into(),
                app_token: "xapp-test".into(),
                owner_user_id: None
            }
        )
        .await
        .is_err()
    );
    assert!(!store.credentials_path("default").unwrap().exists());
}

#[tokio::test]
async fn onboarding_saves_only_after_a_matching_socket_hello() {
    use crate::slack::tests::Fake;
    use serde_json::json;
    for app in ["AOTHER", "A123"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_client::Layout::new(dir.path()), "slack");
        let fake =
            Fake::with_hello(json!({"type": "hello", "connection_info": {"app_id": app}})).await;
        let login = Login {
            bot_token: "xoxb-test".into(),
            app_token: "xapp-test".into(),
            owner_user_id: Some("U123".into()),
        };
        let result = login_with_api(&store, "default", login, &fake.api()).await;
        assert_eq!(result.is_ok(), app == "A123");
        assert_eq!(
            store.credentials_path("default").unwrap().exists(),
            app == "A123"
        );
        if app == "A123" {
            assert_eq!(
                store.account("default").unwrap(),
                Some(crate::slack::tests::account())
            );
        }
    }
}

#[tokio::test]
async fn unavailable_socket_mode_refuses_onboarding_without_saving() {
    use crate::slack::tests::Fake;
    use serde_json::json;
    for error in ["missing_scope", "not_allowed_token_type", "invalid_auth"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&scv_client::Layout::new(dir.path()), "slack");
        let fake = Fake::start().await;
        fake.script(
            "/apps.connections.open",
            200,
            json!({"ok": false, "error": error}),
        );
        let login = Login {
            bot_token: "xoxb-test".into(),
            app_token: "xapp-test".into(),
            owner_user_id: None,
        };
        let result = login_with_api(&store, "default", login, &fake.api()).await;
        let diagnostic = result.unwrap_err().to_string();
        assert!(
            diagnostic.contains("Socket Mode unavailable"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("connections:write"), "{diagnostic}");
        assert!(diagnostic.contains(error), "{diagnostic}");
        assert!(!store.credentials_path("default").unwrap().exists());
    }
}

#[test]
fn missing_scopes_are_named() {
    assert_eq!(
        missing_scopes(&api::BOT_SCOPES.join(", ")),
        Vec::<&str>::new()
    );
    assert_eq!(
        missing_scopes("chat:write,users:read,im:history,app_mentions:read,im:write"),
        [
            "channels:history",
            "files:read",
            "files:write",
            "groups:history"
        ]
    );
}
