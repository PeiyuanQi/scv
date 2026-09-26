//! Unit tests for `src/feishu/credentials.rs`.

use super::*;
use crate::state::Credentials as _;

fn account() -> Account {
    Account {
        app_id: "cli_a1b2c3d4e5f60718".into(),
        app_secret: "secret".into(),
        brand: Brand::Feishu,
        owner_open_id: Some("ou_0123abcd".into()),
    }
}

#[test]
fn secret_rotation_keeps_the_binding_but_app_or_owner_changes_do_not() {
    let base = account();
    let rotated = Account {
        app_secret: "other".into(),
        ..account()
    };
    assert_eq!(base.fingerprint().unwrap(), rotated.fingerprint().unwrap());
    for changed in [
        Account {
            app_id: "cli_ffff".into(),
            ..account()
        },
        Account {
            owner_open_id: None,
            ..account()
        },
        Account {
            brand: Brand::Lark,
            ..account()
        },
    ] {
        assert_ne!(base.fingerprint().unwrap(), changed.fingerprint().unwrap());
    }
}

#[test]
fn debug_never_shows_the_secret() {
    assert!(!format!("{:?}", account()).contains("secret\""));
    assert!(format!("{:?}", account()).contains("<redacted>"));
}

#[test]
fn ids_are_checked_before_use() {
    assert!(account().validate().is_ok());
    for app_id in ["", "cli_", "app_123", "cli_12/34", "cli_1 2"] {
        assert!(validate_app_id(app_id).is_err(), "{app_id}");
    }
    for open_id in ["", "ou_", "on_123", "ou_a b", "ou_a\"b"] {
        assert!(validate_open_id(open_id).is_err(), "{open_id}");
    }
    let bad_secret = Account {
        app_secret: "a\nb".into(),
        ..account()
    };
    assert!(bad_secret.validate().is_err());
}

#[test]
fn saved_accounts_are_private_and_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(&scv_client::Layout::new(dir.path()), crate::feishu::CHANNEL);
    store.save_account("default", &account()).unwrap();
    assert_eq!(store.account("default").unwrap(), Some(account()));
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(store.credentials_path("default").unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // Another owner is another identity: it needs a logout first.
    let other = Account {
        owner_open_id: Some("ou_other".into()),
        ..account()
    };
    assert!(store.save_account("default", &other).is_err());
}
