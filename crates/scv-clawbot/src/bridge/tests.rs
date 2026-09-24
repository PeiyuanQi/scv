//! Unit tests for `src/bridge.rs`.

use super::*;
#[test]
fn login_requires_ids() {
    assert!(validate_confirmed_login(&serde_json::json!({"bot_token":"t"})).is_err());
}
#[test]
fn origin_allows_returned_regional_host() {
    assert!(
        validate_origin_pair(
            "https://ilinkai.weixin.qq.com",
            "https://region.weixin.qq.com"
        )
        .is_ok()
    );
}
#[test]
fn origin_pins_tls_port() {
    assert!(validate_origin_pair("https://x.test:443", "https://x.test:444").is_err());
}
#[test]
fn origin_rejects_untrusted_host() {
    assert!(validate_origin_pair("https://login.test", "https://attacker.test").is_err());
}
#[test]
fn fake_response_requires_success_ret() {
    assert!(parse_response(br#"{"ret":0,"msgs":[]}"#).is_ok());
    assert!(parse_response(br#"{"ret":-14,"errmsg":"expired"}"#).is_err());
}

#[test]
fn parse_response_rejects_oversized_body() {
    assert!(parse_response(&vec![b' '; crate::MAX_RESPONSE_BYTES + 1]).is_err());
}
