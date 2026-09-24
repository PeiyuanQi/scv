//! Unit tests for `src/state.rs`.

use super::*;
use scv_channels::state::{Credentials as _, atomic_write};

fn known_account() -> Account {
    Account {
        token: "secret".into(),
        base_url: "https://example.test".into(),
        bot_id: Some("bot".into()),
        user_id: Some("user".into()),
    }
}

#[test]
fn replacement_requires_logout_and_preserves_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
    let original = known_account();
    store.save_account("default", &original).unwrap();
    let mut state = store
        .bind_state("default", |saved| {
            runs_as(saved, &original.token, &original.base_url)
        })
        .unwrap();
    state.pending = vec![scv_channels::new_pending(
        "message",
        "sender",
        "context",
        "private reply",
        1024,
    )];
    store.save_state("default", &state).unwrap();
    let before = std::fs::read(store.state_path("default").unwrap()).unwrap();
    for replacement in [
        Account {
            bot_id: Some("other".into()),
            ..original.clone()
        },
        Account {
            base_url: "https://other.test".into(),
            ..original.clone()
        },
        Account {
            user_id: Some("other".into()),
            ..original.clone()
        },
    ] {
        assert!(
            store
                .save_account("default", &replacement)
                .unwrap_err()
                .to_string()
                .contains("logout first")
        );
        assert!(store.account("default").unwrap().unwrap() == original);
        assert_eq!(
            std::fs::read(store.state_path("default").unwrap()).unwrap(),
            before
        );
    }
    store.remove("default").unwrap();
    let replacement = Account {
        bot_id: Some("other".into()),
        ..original
    };
    store.save_account("default", &replacement).unwrap();
    assert!(store.save_state("default", &state).is_err());
    assert!(store.load_state("default").unwrap().pending.is_empty());
}

#[test]
fn binding_rejects_externally_replaced_credentials_without_changing_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
    let original = known_account();
    store.save_account("default", &original).unwrap();
    let mut state = store
        .bind_state("default", |saved| {
            runs_as(saved, &original.token, &original.base_url)
        })
        .unwrap();
    state.pending = vec![scv_channels::new_pending(
        "message",
        "sender",
        "context",
        "private reply",
        1024,
    )];
    store.save_state("default", &state).unwrap();
    let replacement = Account {
        bot_id: Some("replacement".into()),
        ..original
    };
    atomic_write(
        &store.credentials_path("default").unwrap(),
        &serde_json::to_string(&replacement).unwrap(),
    )
    .unwrap();
    let error = store
        .bind_state("default", |saved| {
            runs_as(saved, &replacement.token, &replacement.base_url)
        })
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "channel state does not match saved credentials"
    );
    assert_eq!(
        serde_json::to_value(store.load_state("default").unwrap()).unwrap(),
        serde_json::to_value(state).unwrap()
    );
}

#[test]
fn known_identity_token_rotation_preserves_pending_while_running() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
    let original = known_account();
    store.save_account("default", &original).unwrap();
    let _running = store.lock("default").unwrap();
    let mut state = store
        .bind_state("default", |saved| {
            runs_as(saved, &original.token, &original.base_url)
        })
        .unwrap();
    state.cursor = "cursor".into();
    state.pending = vec![scv_channels::new_pending(
        "message", "sender", "context", "reply", 1024,
    )];
    store.save_state("default", &state).unwrap();
    let before = serde_json::to_value(&state).unwrap();
    let rotated = Account {
        token: "new secret".into(),
        base_url: "https://EXAMPLE.test:443/".into(),
        ..original.clone()
    };
    store.save_account("default", &rotated).unwrap();
    assert_eq!(
        serde_json::to_value(
            store
                .bind_state("default", |saved| runs_as(
                    saved,
                    &rotated.token,
                    &rotated.base_url
                ))
                .unwrap()
        )
        .unwrap(),
        before
    );
    assert!(
        store
            .bind_state("default", |saved| runs_as(
                saved,
                &original.token,
                &original.base_url
            ))
            .is_err()
    );
    assert!(store.account_snapshot("default").unwrap().0.unwrap() == rotated);
}

#[test]
fn legacy_binding_requires_original_credentials_and_refuses_login_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
    let legacy = Account {
        bot_id: None,
        user_id: None,
        ..known_account()
    };
    atomic_write(
        &store.credentials_path("default").unwrap(),
        &serde_json::to_string(&legacy).unwrap(),
    )
    .unwrap();
    let unbound = BridgeState {
        in_flight: vec![InFlight {
            message_id: "m".into(),
            to_user_id: "sender".into(),
            context_token: "ctx".into(),
            key: String::new(),
        }],
        ..Default::default()
    };
    store.save_state("default", &unbound).unwrap();
    assert!(
        store
            .bind_state("default", |saved| runs_as(
                saved,
                "different",
                &legacy.base_url
            ))
            .is_err()
    );
    assert!(
        store
            .load_state("default")
            .unwrap()
            .credential_fingerprint
            .is_none()
    );
    assert!(
        store
            .save_account("default", &known_account())
            .unwrap_err()
            .to_string()
            .contains("logout first")
    );
    let bound = store
        .bind_state("default", |saved| {
            runs_as(saved, &legacy.token, &legacy.base_url)
        })
        .unwrap();
    assert_eq!(
        bound.credential_fingerprint,
        Some(legacy.fingerprint().unwrap())
    );
    assert_eq!(bound.in_flight[0].message_id, "m");
    assert!(
        store
            .save_account(
                "default",
                &Account {
                    token: "rotated".into(),
                    ..legacy.clone()
                }
            )
            .is_err()
    );
    store.save_account("default", &legacy).unwrap();
}

#[test]
fn replacement_is_rejected_even_without_pending_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_channels::Layout::new(directory.path()), crate::CHANNEL);
    store.save_account("default", &known_account()).unwrap();
    assert!(
        store
            .save_account(
                "default",
                &Account {
                    bot_id: Some("replacement".into()),
                    ..known_account()
                }
            )
            .is_err()
    );
}

#[test]
fn identities_are_backward_compatible() {
    let mut account: Account =
        serde_json::from_str(r#"{"token":"secret","base_url":"https://example.test"}"#).unwrap();
    assert!(account.bot_id.is_none());
    assert!(account.user_id.is_none());
    account.bot_id = Some("bot".into());
    account.user_id = Some("user".into());
    let restored: Account =
        serde_json::from_str(&serde_json::to_string(&account).unwrap()).unwrap();
    assert!(account == restored);
}
