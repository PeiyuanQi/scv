//! Unit tests for `src/email/imap/tls.rs`.

use super::*;

#[test]
fn the_client_config_builds_with_the_bundled_roots() {
    let config = client_config().unwrap();
    assert!(config.alpn_protocols.is_empty());
}

#[tokio::test]
async fn an_invalid_host_name_is_refused_before_connecting() {
    assert!(connect("not a host name", 993).await.is_err());
    assert!(connect("", 993).await.is_err());
}
