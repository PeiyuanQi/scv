//! Unit tests for `src/feishu/api.rs`.

use super::*;

#[test]
fn socket_urls_must_stay_on_the_brand_domain_over_tls() {
    let feishu = Endpoints::for_brand(Brand::Feishu);
    assert!(
        feishu
            .check_socket_url("wss://msg-frontier.feishu.cn/ws/v2?device_id=1&service_id=2")
            .is_ok()
    );
    for url in [
        "ws://msg-frontier.feishu.cn/ws/v2",
        "wss://msg-frontier.feishu.cn:8443/ws/v2",
        "wss://feishu.cn.attacker.test/ws/v2",
        "wss://attackerfeishu.cn/ws/v2",
        "wss://user:pass@msg-frontier.feishu.cn/ws/v2",
        "wss://msg-frontier.larksuite.com/ws/v2",
        "not a url",
    ] {
        assert!(feishu.check_socket_url(url).is_err(), "{url}");
    }
    let lark = Endpoints::for_brand(Brand::Lark);
    assert!(
        lark.check_socket_url("wss://msg-frontier.larksuite.com/ws/v2")
            .is_ok()
    );
}

#[test]
fn send_outcomes_separate_refusals_from_retries() {
    let ok = |status: u16, body: Value| classify(Ok((status, body)));
    assert_eq!(ok(200, json!({"code": 0})), Attempt::Delivered);
    assert!(matches!(
        ok(400, json!({"code": 230002})),
        Attempt::Refused(_)
    ));
    assert!(matches!(
        ok(400, json!({"code": 99991400})),
        Attempt::Retry(_)
    ));
    assert!(matches!(
        ok(400, json!({"code": 99991663})),
        Attempt::Retry(_)
    ));
    assert!(matches!(ok(503, json!({"code": 1})), Attempt::Retry(_)));
    assert!(matches!(ok(429, json!({})), Attempt::Retry(_)));
    assert!(matches!(ok(404, json!({})), Attempt::Refused(_)));
    assert!(matches!(
        classify(Err(anyhow!("connection reset"))),
        Attempt::Retry(_)
    ));
}

#[test]
fn mention_tags_are_broken_but_other_text_is_kept() {
    assert_eq!(
        neutralize_mentions(r#"hi <at user_id="all">all</at> and <AT id=1>"#),
        "hi <\u{200b}at user_id=\"all\">all</at> and <\u{200b}AT id=1>"
    );
    assert_eq!(
        neutralize_mentions("a < b <b>bold</b> 中文<"),
        "a < b <b>bold</b> 中文<"
    );
    assert_eq!(neutralize_mentions("<a"), "<a");
}

#[test]
fn message_ids_are_path_encoded() {
    assert_eq!(encode_segment("om_x100b"), "om_x100b");
    assert_eq!(encode_segment("a/../b?c"), "a%2F..%2Fb%3Fc");
}
