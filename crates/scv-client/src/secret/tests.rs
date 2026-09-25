//! Unit tests for src/secret.rs.

use super::*;

#[test]
fn debug_output_never_shows_the_secret() {
    #[derive(Debug)]
    #[allow(dead_code, reason = "only formatted")]
    struct Holder {
        key: Secret,
        keys: Option<Secret>,
    }
    let holder = Holder {
        key: Secret::new("sk-very-secret"),
        keys: Some("another-secret".into()),
    };
    let shown = format!("{holder:?} {holder:#?}");
    assert!(!shown.contains("secret"), "{shown}");
    assert!(shown.contains("<redacted>"));
}

#[test]
fn a_secret_serializes_as_its_plain_string() {
    let secret = Secret::new("abc");
    assert_eq!(serde_json::to_string(&secret).unwrap(), "\"abc\"");
    let back: Secret = serde_json::from_str("\"abc\"").unwrap();
    assert_eq!(back, secret);
    assert_eq!(back.expose(), "abc");
    assert_eq!(back.len(), 3);
}
