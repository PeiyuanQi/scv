//! Unit tests for `src/email/credentials.rs`.

use super::*;
use crate::state::Credentials as _;

fn account(host: &str, username: &str, password: &str) -> Account {
    Account::Imap {
        host: host.into(),
        port: 993,
        username: username.into(),
        password: password.into(),
        address: None,
        smtp: None,
    }
}

#[test]
fn a_new_password_keeps_the_fingerprint_and_another_mailbox_does_not() {
    let base = account("imap.qq.com", "me@qq.com", "code-1");
    let rotated = account("IMAP.QQ.com", "me@qq.com", "code-2");
    assert_eq!(base.fingerprint().unwrap(), rotated.fingerprint().unwrap());
    assert_ne!(
        base.fingerprint().unwrap(),
        account("imap.qq.com", "other@qq.com", "code-1")
            .fingerprint()
            .unwrap()
    );
    assert_ne!(
        base.fingerprint().unwrap(),
        account("imap.163.com", "me@qq.com", "code-1")
            .fingerprint()
            .unwrap()
    );
}

#[test]
fn saved_credentials_round_trip_and_never_print_the_secret() {
    let saved = account("imap.qq.com", "me@qq.com", "s3cret");
    let json = serde_json::to_string(&saved).unwrap();
    assert!(json.contains(r#""provider":"imap""#), "{json}");
    assert_eq!(serde_json::from_str::<Account>(&json).unwrap(), saved);
    let debug = format!("{saved:?}");
    assert!(
        !debug.contains("s3cret") && !debug.contains("me@qq.com"),
        "{debug}"
    );
    assert!(
        serde_json::from_str::<Account>(
            r#"{"provider":"imap","host":"h","port":993,"username":"u","password":"p","extra":1}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<Account>(r#"{"provider":"pop3","host":"h"}"#).is_err());
}

#[test]
fn validation_refuses_what_cannot_be_a_mailbox_login() {
    assert!(
        account("imap.qq.com", "me@qq.com", "code")
            .validate()
            .is_ok()
    );
    for bad in [
        account("", "me", "code"),
        account("imap qq.com", "me", "code"),
        account("-imap.qq.com", "me", "code"),
        account("imap.qq.com/x", "me", "code"),
        account("imap.qq.com", "", "code"),
        account("imap.qq.com", "me\r\nA1 DELETE INBOX", "code"),
        account("imap.qq.com", "me", ""),
        account("imap.qq.com", "me", "code\r\nA1 LOGOUT"),
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    let Account::Imap { host, .. } = account("imap.qq.com", "me", "code") else {
        unreachable!("an IMAP account");
    };
    assert_eq!(host, "imap.qq.com");
}
