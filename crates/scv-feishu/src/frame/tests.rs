//! Unit tests for `src/frame.rs`.

use super::*;
use prost::Message as _;

fn part(id: &str, sum: usize, seq: usize, payload: &[u8]) -> Frame {
    Frame {
        method: METHOD_DATA,
        headers: vec![
            Header {
                key: "message_id".into(),
                value: id.into(),
            },
            Header {
                key: "sum".into(),
                value: sum.to_string(),
            },
            Header {
                key: "seq".into(),
                value: seq.to_string(),
            },
        ],
        payload: Some(payload.to_vec()),
        ..Default::default()
    }
}

#[test]
fn frames_round_trip_with_required_fields() {
    let ping = Frame::ping(7);
    let bytes = ping.encode_to_vec();
    // Required fields are present even when zero.
    assert_eq!(&bytes[..4], &[0x08, 0x00, 0x10, 0x00]);
    let decoded = Frame::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded, ping);
    assert_eq!(decoded.header("type"), Some("ping"));
}

#[test]
fn acknowledgement_keeps_the_frame_and_reports_success() {
    let ack = part("m", 1, 0, b"{}").acknowledgement(Duration::from_millis(12));
    assert_eq!(ack.header("message_id"), Some("m"));
    assert_eq!(ack.header("biz_rt"), Some("12"));
    let body: serde_json::Value = serde_json::from_slice(&ack.payload.unwrap()).unwrap();
    assert_eq!(body["code"], 200);
}

#[test]
fn split_events_reassemble_in_any_order() {
    let mut fragments = Fragments::default();
    assert_eq!(fragments.accept(&part("m", 3, 2, b"c")), None);
    assert_eq!(fragments.accept(&part("m", 3, 0, b"a")), None);
    assert_eq!(fragments.accept(&part("m", 3, 1, b"b")).unwrap(), b"abc");
    // A whole event passes straight through.
    assert_eq!(fragments.accept(&part("n", 1, 0, b"x")).unwrap(), b"x");
}

#[test]
fn malformed_or_oversized_parts_are_dropped() {
    let mut fragments = Fragments::default();
    assert_eq!(fragments.accept(&part("m", 2, 2, b"a")), None);
    assert_eq!(fragments.accept(&part("m", MAX_PARTS + 1, 0, b"a")), None);
    assert_eq!(fragments.accept(&part("", 2, 0, b"a")), None);
    let big = vec![b'x'; MAX_EVENT_BYTES];
    assert_eq!(fragments.accept(&part("m", 2, 0, &big)), None);
    assert_eq!(fragments.accept(&part("m", 2, 1, b"y")), None);
    assert!(fragments.partial.is_empty());
}
