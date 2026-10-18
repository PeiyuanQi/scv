//! Unit tests for `src/email/imap/tls.rs`.

use super::*;

#[test]
fn the_client_config_builds_with_the_bundled_roots() {
    let config = client_config().unwrap();
    assert_eq!(config.alpn_protocols, [] as [Vec<u8>; 0]);
}

#[tokio::test]
async fn an_invalid_host_name_is_refused_before_connecting() {
    assert!(connect("not a host name", 993).await.is_err());
    assert!(connect("", 993).await.is_err());
}
