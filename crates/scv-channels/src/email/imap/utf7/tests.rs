//! Unit tests for `src/email/imap/utf7.rs`.

use super::*;

#[test]
fn encodes_ascii_as_itself_and_ampersand_as_a_run() {
    assert_eq!(encode("INBOX"), "INBOX");
    assert_eq!(encode("Sent Items"), "Sent Items");
    assert_eq!(encode("A&B"), "A&-B");
    assert_eq!(encode("&"), "&-");
}

#[test]
fn encodes_the_rfc_3501_example_and_chinese_names() {
    assert_eq!(
        encode("~peter/mail/台北/日本語"),
        "~peter/mail/&U,BTFw-/&ZeVnLIqe-"
    );
    assert_eq!(encode("已发送"), "&XfJT0ZAB-");
    assert_eq!(encode("草稿箱"), "&g0l6P3ux-");
}

#[test]
fn round_trips() {
    for name in [
        "INBOX",
        "A&B",
        "&&",
        "&-",
        "Entwürfe",
        "已发送",
        "垃圾邮件",
        "📧 mail",
        "a-b",
        "Sent Items",
        "[Gmail]/All Mail",
        "",
    ] {
        assert_eq!(decode(&encode(name)).unwrap(), name, "{name}");
    }
    assert_eq!(
        decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-").unwrap(),
        "~peter/mail/台北/日本語"
    );
}

#[test]
fn control_characters_are_encoded_never_sent_raw() {
    let name = "INBOX\r\nA2 DELETE INBOX";
    let encoded = encode(name);
    assert!(
        encoded.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
        "{encoded}"
    );
    assert_eq!(decode(&encoded).unwrap(), name);
}

#[test]
fn decode_refuses_malformed_runs() {
    for bad in ["&XfJT", "&*-", "&2D0-", "&XfJT0ZA-"] {
        assert!(decode(bad).is_err(), "{bad}");
    }
}
