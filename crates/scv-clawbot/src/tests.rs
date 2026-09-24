//! Unit tests for `src/lib.rs`.

use super::*;

mod lifecycle;

#[test]
fn validates_origins() {
    assert!(normalize_base_url("https://example.test").is_ok());
    assert!(normalize_base_url("http://example.test").is_err());
    assert!(normalize_base_url("https://user@example.test").is_err());
}
#[test]
fn validates_ret() {
    assert!(check_envelope(&serde_json::json!({"ret":0})).is_ok());
    assert!(check_envelope(&serde_json::json!({"ret":1})).is_err());
}

#[test]
fn accepts_live_send_ack_without_ret() {
    for delivered in [
        &b""[..],
        b"{}",
        br#"{"ret":0}"#,
        br#"{"ret":null}"#,
        b"ok",
        b"[]",
    ] {
        assert!(check_send_ack(delivered).is_ok());
    }
    assert_eq!(
        check_send_ack(br#"{"ret":-2,"errmsg":"prepare failed"}"#).unwrap_err(),
        r#"ret=-2 errcode=- errmsg="prepare failed""#
    );
    assert!(check_send_ack(br#"{"errcode":40001}"#).is_err());
    assert!(check_send_ack(br#"{"ret":"0"}"#).is_err());
    assert_eq!(
        check_send_ack(br#"{"ret":{"detail":"x"},"errcode":7}"#).unwrap_err(),
        r#"ret=non-integer errcode=7 errmsg="""#
    );
}

#[test]
fn accepts_live_getupdates_success_without_ret() {
    assert!(
        validate_updates(&serde_json::json!({
            "msgs": [],
            "sync_buf": "sync",
            "get_updates_buf": "cursor"
        }))
        .is_ok()
    );
}

#[test]
fn rejects_getupdates_error_without_ret() {
    assert!(
        validate_updates(&serde_json::json!({
            "errcode": -14,
            "errmsg": "session timeout"
        }))
        .is_err()
    );
}

fn parsed(items: serde_json::Value) -> Option<Message> {
    match inbound(&serde_json::json!({"message_id":"m", "message_type":1,
        "from_user_id":"u", "context_token":"c", "item_list":items}))?
    {
        Inbound::Text(message) => Some(message),
        Inbound::Ignored { .. } => None,
    }
}

#[test]
fn voice_transcripts_quotes_and_files_come_through() {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
    let voice = parsed(serde_json::json!([{"type":3,"voice_item":{"encode_type":6,
        "text":"call me","media":{"encrypt_query_param":"p","aes_key":key}}}]))
    .unwrap();
    assert_eq!(voice.text, "");
    assert_eq!(voice.media.len(), 1);
    assert_eq!(voice.media[0].transcript.as_deref(), Some("call me"));

    let quote = parsed(
        serde_json::json!([{"type":1,"text_item":{"text":"and this?"},
        "ref_msg":{"title":"Alex","message_item":{"type":2,"image_item":{
            "media":{"encrypt_query_param":"q","aes_key":key}}}}}]),
    )
    .unwrap();
    assert_eq!(quote.text, "[Quoting: Alex | [image]]\nand this?");
    // The quoted image is fetched like the message's own.
    assert_eq!(quote.media.len(), 1);
    assert_eq!(quote.media[0].kind, scv_channels::MediaKind::Image);

    let quoted_text = parsed(serde_json::json!([{"type":1,"text_item":{"text":"yes"},
        "ref_msg":{"message_item":{"type":1,"text_item":{"text":"ready?"}}}}]))
    .unwrap();
    assert_eq!(quoted_text.text, "[Quoting: ready?]\nyes");
    assert!(quoted_text.media.is_empty());

    // Items without text or a file leave nothing to answer.
    assert!(parsed(serde_json::json!([{"type":11,"tool_call_start_item":{}}])).is_none());
    assert!(parsed(serde_json::json!([{"type":2,"image_item":{"media":{}}}])).is_none());
}

#[test]
fn preserves_string_and_unsigned_numeric_message_ids() {
    assert_eq!(
        message_id(&serde_json::json!({"message_id": "string-id"})).as_deref(),
        Some("string-id")
    );
    assert_eq!(
        message_id(&serde_json::json!({"message_id": u64::MAX})).as_deref(),
        Some("18446744073709551615")
    );
    assert_eq!(
        message_id(&serde_json::json!({"msg_id": 42})).as_deref(),
        Some("42")
    );
    assert!(message_id(&serde_json::json!({"message_id": null, "msg_id": 42})).is_none());
    assert!(message_id(&serde_json::json!({"message_id": -1})).is_none());
    assert!(message_id(&serde_json::json!({"message_id": 1.5})).is_none());
    assert!(message_id(&serde_json::from_str(r#"{"message_id":1e3}"#).unwrap()).is_none());
    assert!(
        message_id(&serde_json::from_str(r#"{"message_id":18446744073709551616}"#).unwrap())
            .is_none()
    );
    assert!(
        message_id(&serde_json::json!({
            "message_id": "x".repeat(MAX_MESSAGE_ID_BYTES + 1)
        }))
        .is_none()
    );
}
